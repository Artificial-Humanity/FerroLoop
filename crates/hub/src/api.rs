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

use fl_core::agent::{AgentRecord, AgentRole, AgentStatus, RoleError, RoleRecord};
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
    DeliveryReceipt, DeliveryStatus, MessageKind, MessageUrgency, SendWireArgs, ThreadId,
    WireAddress, WireFilter, validate_sender_name,
};
use fl_core::governor::Principal;
use fl_core::ids::{GateId, Kind, MessageId, WorkflowId, WorkflowRunId};
use fl_core::iri::Iri;
use fl_core::log::{AttemptStatus, GateRun};
use fl_core::state::{
    HandoffEntry, MemoryBasis, MemoryCategory, MemoryEntry, MemoryLifecycle, MergeMode,
    RuntimeEntry, SensitivityPatch, StateConcern, StateEntry, StatePayload, StateScope,
    StateSearchHit, UpsertStateArgs,
};
use fl_core::store::{
    AgentRegistry, Catalog, Handles, Ledger, MessageStore, StateStore, WorkflowStore,
};
use fl_core::telemetry::{
    AttemptHeartbeat, DEFAULT_STALE_AFTER_MS, audit_transition_regret, evaluate_liveness,
    liveness_agent_slug,
};
use fl_core::token_count::count_unit;
use fl_core::workflow::{ApprovalTier, StepCompletion};
use fl_exec::stamp;
use fl_exec::workflow::{
    StepExecutionOptions, WorkflowEnv, abort_workflow_run, approve_workflow_step,
    execute_workflow_step, list_workflow_facts, list_workflow_handoffs, record_step_handoff,
    reject_workflow_step, start_workflow_run,
};
use fl_gate::{
    BridgeTarget, GateGovernor, GovernedOutcome, InboundBridgeEvent, ProxyRequest, SetSecretArgs,
    ToolOutcome, Vault, admission_refusal_text, discover_connectors, evaluate_inbound_bridge_event,
    execute_proxied_request_with_provider_and_contract_phased_with_deadline,
    handle_mcp_jsonrpc_request_with_governor_and_secret_provider, handle_registry_v01_get,
    verify_inbound_bridge_webhook,
};
use fl_store::AnyStore;
use fl_wire::dispatcher::{
    CodexSteerError, MAX_DELIVER, OperatorPromptError, OperatorPromptIntent, OperatorPromptOutcome,
    PushError, SessionResetMode, clear_or_reforge_session, deliver_to_live_grok_pane,
    dispatch_operator_prompt_to_route, push_operator_prompt_to_route,
    push_operator_prompt_with_intent, push_to_route,
};
use fl_wire::identity::{RouteTable, normalize_persona};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::HubStoreGuard;
use crate::artifacts::{self, ArtifactEntry};
use crate::auth::{
    HubAuthority, constant_time_eq, extract_bearer_token, extract_session_cookie,
    format_logout_set_cookie, format_session_set_cookie, sha256_hex,
};
use crate::cloud_adapters;
use crate::discovery::{
    DiscoveredSessionSummary, DiscoveryStoreSnapshot, TranscriptObservation, TranscriptReads,
    context_usage_from_sessions, discover_route_sessions_without_model_info,
    discover_sessions_from_snapshot, list_git_worktrees, lookup_one_route_session, lookup_sessions,
    observe_transcript_from_sessions, read_session_quota_usage_with_reads,
    read_session_transcript_from_snapshot, read_worktree_diff, scan_cargo_architecture_conformance,
    snapshot_discovery_names, snapshot_discovery_store, transcript_observation_from_sessions,
};
use crate::github_pr;
use crate::lifecycle::{
    self, DecommissionMode, LifecycleEnv, LifecycleError, RemodelMode, RemodelRequest,
    SessionTarget, SpinUpRequest,
};
use crate::nodes::{
    DispatchTask, EnrollFail, EnrollWorker, EnsureHubArgs, HeartbeatFail, HeartbeatUpdate,
    NodeRecord, NodeView, RevokeFail, TaskFail, TaskResult, detect_capabilities, dispatch_task,
    enroll_worker, ensure_hub_node, find_hub_node, list_node_tasks, list_visible_nodes,
    local_hostname, mint_enroll_token, node_id_from_hostname, node_state_slug, node_token_matches,
    record_task_result, revoke_node, touch_node,
};
use crate::operators;
use crate::records;
use crate::session_actions::{
    self, METHOD_OPERATOR_PROMPT, METHOD_OVERRIDE_ONLY, METHOD_STORED, ModelOverride, route_vendor,
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
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
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
    let resp = route_http_request(state, req);
    if req.method != "GET" && req.method != "HEAD" {
        // A request that may have written the store: the next discovery or watch read takes a
        // fresh snapshot, so the caller sees its own change.
        state.discovery_snapshots.invalidate();
        state.watch_caches.invalidate();
    }
    resp
}

fn route_http_request(state: &HubState, req: &HttpRequest) -> HttpResponse {
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

    // Operator login (`token`, `password` or `passkey` mode) and the public passkey challenge.
    // Both throttle themselves; neither needs a session.
    if method == "POST" && path == "/api/v1/auth/login" {
        return operators::handle_post_login(state, req);
    }
    if method == "POST" && path == "/api/v1/auth/passkey/challenge" {
        return operators::handle_post_passkey_challenge(state, req);
    }

    // Public worker enrollment. A one-time code is the credential.
    if method == "POST" && path == "/api/v1/nodes/enroll" {
        return handle_post_node_enroll(state, req);
    }

    // Node-scoped federation routes (heartbeat, dispatch, task queue) stay outside the general
    // gate. A node bearer is not an owner token: it opens only its own node's routes, and each
    // handler decides whether the owner alone may call it.
    if let Some(route) = node_scoped_route(method, path) {
        return handle_node_scoped(state, req, route);
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

    if let Some(resp) = crate::warrants::route(state, &authority, req) {
        return resp;
    }
    if let Some(resp) = crate::watch_api::route(state, &authority, req) {
        return resp;
    }
    if let Some(resp) = crate::balance_api::route(state, &authority, req) {
        return resp;
    }
    if let Some(resp) = crate::mcp_api::route(state, &authority, req) {
        return resp;
    }
    if let Some(resp) = crate::vault_consumers_api::route(state, &authority, req) {
        return resp;
    }

    match (method, path) {
        ("GET", "/api/v1/auth/session") => HttpResponse::json(
            200,
            json!({
                "authenticated": true,
                "authority": authority,
            }),
        ),
        ("POST", "/api/v1/auth/logout") => handle_auth_logout(state, req),
        ("GET", "/api/v1/auth/operators") => operators::handle_get_operators(state),
        ("POST", "/api/v1/auth/operators") => {
            operators::handle_post_operator(state, &authority, req)
        }
        ("POST", "/api/v1/auth/totp/enroll") => {
            operators::handle_post_totp_enroll(state, &authority, req)
        }
        ("POST", "/api/v1/auth/totp/verify") => {
            operators::handle_post_totp_verify(state, &authority, req)
        }
        ("POST", "/api/v1/auth/passkey/register") => {
            operators::handle_post_passkey_register(state, &authority, req)
        }
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
        ("GET", "/api/v1/artifacts/versions") => handle_get_artifact_versions(state, req),
        ("GET", "/api/v1/artifacts/version-raw") => handle_get_artifact_version_raw(state, req),
        ("GET", "/api/v1/artifacts/office-preview") => {
            handle_get_artifact_office_preview(state, req)
        }
        ("GET", "/api/v1/artifacts/consolidate") => handle_get_artifacts_consolidate(state),
        ("POST", "/api/v1/artifacts/consolidate") => {
            handle_post_artifacts_consolidate(state, &authority, req)
        }
        ("POST", "/api/v1/artifacts/prune") => handle_post_artifacts_prune(state, &authority, req),
        ("POST", "/api/v1/attachments/prune") => {
            crate::attachments::handle_post_attachments_prune(state, &authority, req)
        }
        ("GET", "/api/v1/records") => records::handle_get_records(state, req),
        ("POST", "/api/v1/records") => records::handle_post_record(state, &authority, req),
        ("GET", "/api/v1/roles") => handle_get_roles(state),
        ("POST", "/api/v1/roles") => handle_post_role(state, &authority, req),
        ("GET", "/api/v1/cloud-sessions") => handle_get_cloud_sessions(state),
        ("GET", "/api/v1/providers/local") => handle_get_local_providers(state),
        ("POST", "/api/v1/providers/local") => handle_post_local_provider(state, &authority, req),
        ("GET", "/api/v1/notifications/subscribe") => handle_get_notifications_subscribe(state),
        ("POST", "/api/v1/notifications/subscribe") => {
            handle_post_notifications_subscribe(state, &authority, req)
        }
        ("POST", "/api/v1/notifications/test") => {
            handle_post_notifications_test(state, &authority, req)
        }
        ("GET", "/api/v1/notifications/events") => handle_get_notifications_events(state),
        ("GET", "/api/v1/settings") => handle_get_settings(state),
        ("GET", "/api/v1/version") => crate::version_api::handle_get(state),
        ("GET", "/api/v1/doctor") => crate::doctor_api::handle_get(state, &authority),
        ("PUT", "/api/v1/settings") => handle_put_settings(state, &authority, req),
        ("POST", "/api/v1/settings/artifacts-dir") => {
            crate::artifact_dirs::handle_post_system_dir(state, &authority, req)
        }
        ("GET", "/api/v1/projects") => crate::projects_api::handle_get_projects(state),
        ("POST", "/api/v1/projects") => {
            crate::projects_api::handle_post_projects(state, &authority, req)
        }
        ("GET", "/api/v1/cloud/adapters") => handle_get_cloud_adapters(state),
        ("PUT", "/api/v1/cloud/adapters") => handle_put_cloud_adapters(state, &authority, req),
        ("GET", "/api/v1/gate/secrets") => handle_get_gate_secrets(state),
        ("POST", "/api/v1/gate/secrets") => handle_post_gate_secret(state, &authority, req),
        ("GET", "/api/v1/gate/connectors") => handle_get_gate_connectors(state),
        ("POST", "/api/v1/gate/call") => handle_post_gate_call(state, &authority, req),
        ("POST", "/api/v1/gate/mcp") => handle_post_gate_mcp(state, &authority, None, req),
        ("GET", "/api/v1/runtimes/matrix") => {
            crate::runtime_matrix::handle_get_matrix(state, &authority)
        }
        ("POST", "/api/v1/runtimes/probe") => {
            crate::runtime_matrix::handle_post_probe(state, &authority, req)
        }
        ("POST", "/api/v1/runtimes/update") => {
            crate::runtime_update::handle_post_update(state, &authority, req)
        }
        ("GET", "/api/v1/nodes") => handle_get_nodes(state),
        ("POST", "/api/v1/nodes/enroll-token") => handle_post_enroll_token(state, &authority, req),
        _ => handle_dynamic_api_routes(state, &authority, req),
    }
}

pub(crate) fn store_open_failure() -> HttpResponse {
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

pub(crate) fn json_object_body(req: &HttpRequest) -> Result<Value, HttpResponse> {
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
        "environment_kind": view.environment_kind,
        "federated_subsystems": view.federated_subsystems,
        "active_tasks": view.active_tasks,
        "load_avg": view.load_avg,
        "reported_status": view.reported_status,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NodeRoute<'a> {
    Heartbeat(&'a str),
    Dispatch(&'a str),
    ListTasks(&'a str),
    TaskResult(&'a str, &'a str),
}

fn node_scoped_route<'a>(method: &str, path: &'a str) -> Option<NodeRoute<'a>> {
    let rest = path.strip_prefix("/api/v1/nodes/")?;
    let segments: Vec<&str> = rest.split('/').collect();
    match (method, segments.as_slice()) {
        ("POST", [id, "heartbeat"]) => Some(NodeRoute::Heartbeat(id)),
        ("POST", [id, "dispatch"]) => Some(NodeRoute::Dispatch(id)),
        ("GET", [id, "tasks"]) => Some(NodeRoute::ListTasks(id)),
        ("POST", [id, "tasks", task_id, "result"]) => Some(NodeRoute::TaskResult(id, task_id)),
        _ => None,
    }
}

fn handle_node_scoped(state: &HubState, req: &HttpRequest, route: NodeRoute<'_>) -> HttpResponse {
    match route {
        NodeRoute::Heartbeat(raw_id) => handle_post_node_heartbeat(state, req, raw_id),
        NodeRoute::Dispatch(raw_id) => handle_post_node_dispatch(state, req, raw_id),
        NodeRoute::ListTasks(raw_id) => handle_get_node_tasks(state, req, raw_id),
        NodeRoute::TaskResult(raw_id, task_id) => {
            handle_post_node_task_result(state, req, raw_id, task_id)
        }
    }
}

/// Same slug as `nodes::node_state_slug`. Heartbeat reads the row after
/// `node_token_matches` returns false for a revoked worker.
fn stored_node_slug(node_id: &str) -> Option<String> {
    node_state_slug(node_id).ok()
}

fn revoked_node_bearer_matches(
    store: &AnyStore,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NodeAccess {
    /// The owner session only.
    Owner,
    /// The owner, or the node's own bearer token.
    OwnerOrNode,
    /// Heartbeat: any authenticated caller (existing behavior), or the node's own bearer even
    /// when revoked, so a revoked worker is told so (403) instead of seeing a generic 401.
    Heartbeat,
}

/// Who `authorize_node_caller` let in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NodeCaller {
    Owner,
    /// The node's own bearer token (revoked too, for a heartbeat).
    Node,
    /// Any other authenticated caller; only `NodeAccess::Heartbeat` admits one.
    Other,
}

fn authorize_node_caller<'a>(
    state: &'a HubState,
    req: &HttpRequest,
    node_id: &str,
    access: NodeAccess,
) -> Result<(HubStoreGuard<'a>, NodeCaller), HttpResponse> {
    let owner_only = access == NodeAccess::Owner;
    let accept_revoked = access == NodeAccess::Heartbeat;
    let authority = authenticate_api_request(state, req).ok();
    let store = match open_store_response(state) {
        Ok(store) => store,
        Err(()) => return Err(store_open_failure()),
    };
    match authority {
        Some(authority) if authority.is_owner() => return Ok((store, NodeCaller::Owner)),
        Some(_) if access == NodeAccess::Heartbeat => return Ok((store, NodeCaller::Other)),
        Some(_) => {
            return Err(HttpResponse::json(
                403,
                json!({ "error": "owner authority or the node token is required" }),
            ));
        }
        None if owner_only => {
            return Err(HttpResponse::json(401, json!({ "error": "unauthorized" })));
        }
        None => {}
    }
    let bearer = req
        .headers
        .get("authorization")
        .and_then(|header| extract_bearer_token(header))
        .unwrap_or_default();
    match node_token_matches(&store, node_id, &bearer) {
        Ok(true) => Ok((store, NodeCaller::Node)),
        Ok(false) if !accept_revoked => {
            Err(HttpResponse::json(401, json!({ "error": "unauthorized" })))
        }
        Ok(false) => match revoked_node_bearer_matches(&store, node_id, &bearer) {
            Ok(true) => Ok((store, NodeCaller::Node)),
            Ok(false) => Err(HttpResponse::json(401, json!({ "error": "unauthorized" }))),
            Err(err) => Err(HttpResponse::json(500, json!({ "error": err }))),
        },
        Err(err) => Err(HttpResponse::json(500, json!({ "error": err }))),
    }
}

fn ensure_primary_hub(state: &HubState, store: &AnyStore, now_ms: u64) -> Result<(), String> {
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
        environment_kind: json_optional_string(body, "environment_kind")?,
        federated_subsystems: json_string_array(body, "federated_subsystems")?,
        now_ms: stamp::now().unix_millis(),
    })
}

fn handle_post_node_heartbeat(state: &HubState, req: &HttpRequest, raw_id: &str) -> HttpResponse {
    let node_id = match decode_node_id(raw_id) {
        Ok(node_id) => node_id,
        Err(resp) => return resp,
    };
    let (store, caller) = match authorize_node_caller(state, req, &node_id, NodeAccess::Heartbeat) {
        Ok(authorized) => authorized,
        Err(resp) => return resp,
    };
    let body = match json_object_body(req) {
        Ok(body) => body,
        Err(resp) => return resp,
    };
    let mut capabilities = match json_optional_string_array(&body, "capabilities") {
        Ok(capabilities) => capabilities,
        Err(error) => return HttpResponse::json(400, json!({ "error": error })),
    };
    let mut labels = match json_optional_string_array(&body, "labels") {
        Ok(labels) => labels,
        Err(error) => return HttpResponse::json(400, json!({ "error": error })),
    };
    let mut fields = match heartbeat_fields(&body) {
        Ok(fields) => fields,
        Err(error) => return HttpResponse::json(400, json!({ "error": error })),
    };
    // Any authenticated caller keeps a node alive (`last_seen`), but only the owner and the
    // node itself may set what the fleet view shows: status, load, capabilities, labels and the
    // rest of the profile. Otherwise a peer could mark another node draining or rewrite what it
    // advertises or federates.
    if caller == NodeCaller::Other {
        fields = HeartbeatFields::default();
        capabilities = None;
        labels = None;
    }
    match touch_node(
        &store,
        HeartbeatUpdate {
            node_id,
            capabilities,
            labels,
            environment_kind: fields.environment_kind,
            federated_subsystems: fields.federated_subsystems,
            active_tasks: fields.active_tasks,
            load_avg: fields.load_avg,
            reported_status: fields.reported_status,
            now_ms: stamp::now().unix_millis(),
        },
    ) {
        Ok(view) => HttpResponse::json(
            200,
            json!({
                "node_id": view.node_id,
                "status": view.status,
                "reported_status": view.reported_status,
                "last_seen_at_ms": view.last_seen_at_ms,
                "active_tasks": view.active_tasks,
                "load_avg": view.load_avg,
                "capabilities": view.capabilities,
                "environment_kind": view.environment_kind,
            }),
        ),
        Err(HeartbeatFail::Invalid(error)) => HttpResponse::json(400, json!({ "error": error })),
        Err(HeartbeatFail::NotFound) => {
            HttpResponse::json(404, json!({ "error": "node not found" }))
        }
        Err(HeartbeatFail::Revoked) => HttpResponse::json(403, json!({ "error": "node revoked" })),
        Err(HeartbeatFail::Store(err)) => HttpResponse::json(500, json!({ "error": err })),
    }
}

#[derive(Default)]
struct HeartbeatFields {
    environment_kind: Option<String>,
    federated_subsystems: Option<Vec<String>>,
    active_tasks: Option<u32>,
    load_avg: Option<f64>,
    reported_status: Option<String>,
}

fn heartbeat_fields(body: &Value) -> Result<HeartbeatFields, String> {
    let active_tasks = match body.get("active_tasks") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| "`active_tasks` must be a non-negative integer".to_string())?,
        ),
    };
    let load_avg = match body.get("load_avg") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_f64()
                .ok_or_else(|| "`load_avg` must be a number".to_string())?,
        ),
    };
    Ok(HeartbeatFields {
        environment_kind: json_optional_string(body, "environment_kind")?,
        federated_subsystems: json_optional_string_array(body, "federated_subsystems")?,
        active_tasks,
        load_avg,
        reported_status: json_optional_string(body, "status")?,
    })
}

fn node_task_failure(fail: TaskFail) -> HttpResponse {
    match fail {
        TaskFail::NodeNotFound => HttpResponse::json(404, json!({ "error": "node not found" })),
        TaskFail::TaskNotFound => HttpResponse::json(404, json!({ "error": "task not found" })),
        TaskFail::NodeRevoked => HttpResponse::json(403, json!({ "error": "node revoked" })),
        TaskFail::NotWorker => HttpResponse::json(
            400,
            json!({ "error": "tasks can only be dispatched to worker nodes" }),
        ),
        TaskFail::Invalid(error) => HttpResponse::json(400, json!({ "error": error })),
        TaskFail::Conflict(error) => HttpResponse::json(409, json!({ "error": error })),
        TaskFail::Store(error) => HttpResponse::json(500, json!({ "error": error })),
    }
}

fn handle_post_node_dispatch(state: &HubState, req: &HttpRequest, raw_id: &str) -> HttpResponse {
    let node_id = match decode_node_id(raw_id) {
        Ok(node_id) => node_id,
        Err(resp) => return resp,
    };
    let store = match authorize_node_caller(state, req, &node_id, NodeAccess::Owner) {
        Ok((store, _)) => store,
        Err(resp) => return resp,
    };
    let body = match json_object_body(req) {
        Ok(body) => body,
        Err(resp) => return resp,
    };
    let parsed = (|| -> Result<DispatchTask, String> {
        let kind = json_optional_string(&body, "kind")?.unwrap_or_else(|| "shell".to_string());
        Ok(DispatchTask {
            kind,
            command: json_string(&body, "command")?,
            project: json_string(&body, "project")?,
            now_ms: stamp::now().unix_millis(),
        })
    })();
    let args = match parsed {
        Ok(args) => args,
        Err(error) => return HttpResponse::json(400, json!({ "error": error })),
    };
    match dispatch_task(&store, &node_id, args) {
        Ok(task) => HttpResponse::json(201, json!({ "task": task })),
        Err(fail) => node_task_failure(fail),
    }
}

fn handle_get_node_tasks(state: &HubState, req: &HttpRequest, raw_id: &str) -> HttpResponse {
    let node_id = match decode_node_id(raw_id) {
        Ok(node_id) => node_id,
        Err(resp) => return resp,
    };
    let store = match authorize_node_caller(state, req, &node_id, NodeAccess::OwnerOrNode) {
        Ok((store, _)) => store,
        Err(resp) => return resp,
    };
    match list_node_tasks(&store, &node_id) {
        Ok(tasks) => HttpResponse::json(200, json!({ "node_id": node_id, "tasks": tasks })),
        Err(fail) => node_task_failure(fail),
    }
}

fn handle_post_node_task_result(
    state: &HubState,
    req: &HttpRequest,
    raw_id: &str,
    raw_task_id: &str,
) -> HttpResponse {
    let node_id = match decode_node_id(raw_id) {
        Ok(node_id) => node_id,
        Err(resp) => return resp,
    };
    let task_id = percent_decode(raw_task_id);
    let store = match authorize_node_caller(state, req, &node_id, NodeAccess::OwnerOrNode) {
        Ok((store, _)) => store,
        Err(resp) => return resp,
    };
    let body = match json_object_body(req) {
        Ok(body) => body,
        Err(resp) => return resp,
    };
    let parsed = (|| -> Result<TaskResult, String> {
        let exit_code = match body.get("exit_code") {
            None | Some(Value::Null) => None,
            Some(value) => Some(
                value
                    .as_i64()
                    .and_then(|n| i32::try_from(n).ok())
                    .ok_or_else(|| "`exit_code` must be an integer".to_string())?,
            ),
        };
        Ok(TaskResult {
            status: json_string(&body, "status")?,
            output: json_string(&body, "output")?,
            exit_code,
            now_ms: stamp::now().unix_millis(),
        })
    })();
    let result = match parsed {
        Ok(result) => result,
        Err(error) => return HttpResponse::json(400, json!({ "error": error })),
    };
    match record_task_result(&store, &node_id, &task_id, result) {
        Ok(task) => HttpResponse::json(200, json!({ "task": task })),
        Err(fail) => node_task_failure(fail),
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

/// The outcome of [`authenticate_headers`].
pub(crate) enum AuthLookup {
    Authorized(HubAuthority),
    Rejected,
    /// A session lookup was needed and the store would not open.
    StoreUnavailable,
}

/// Authenticate a request's headers. Owner, peer and Tailscale credentials and the sessions in
/// memory are decided without the store. Only a cookie or bearer token that is not in memory
/// opens it, behind the global store lock, and a token the store has already rejected is not
/// looked up again for a while (see `auth::SessionMissCache`). A request with no session
/// credential at all never opens the store.
pub(crate) fn authenticate_headers(
    state: &HubState,
    headers: &BTreeMap<String, String>,
    query: &BTreeMap<String, String>,
) -> AuthLookup {
    if let Some(authority) = state.auth.authenticate_request(None, headers, query) {
        return AuthLookup::Authorized(authority);
    }
    if !state.auth.session_store_lookup_needed(headers, query) {
        return AuthLookup::Rejected;
    }
    let Ok(store) = open_store_response(state) else {
        return AuthLookup::StoreUnavailable;
    };
    match state
        .auth
        .authenticate_request(Some(&*store), headers, query)
    {
        Some(authority) => AuthLookup::Authorized(authority),
        None => AuthLookup::Rejected,
    }
}

fn authenticate_api_request(
    state: &HubState,
    req: &HttpRequest,
) -> Result<HubAuthority, HttpResponse> {
    match authenticate_headers(state, &req.headers, &req.query) {
        AuthLookup::Authorized(authority) => Ok(authority),
        // The same body whether the store was read or the rejection was remembered.
        AuthLookup::Rejected => Err(HttpResponse::json(401, json!({ "error": "unauthorized" }))),
        AuthLookup::StoreUnavailable => Err(HttpResponse::json(
            500,
            json!({ "error": "failed to open store: store unavailable" }),
        )),
    }
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

pub(crate) fn load_discovery_snapshot(state: &HubState) -> DiscoveryStoreSnapshot {
    match state.open_store() {
        Ok(store) => snapshot_discovery_store(&store),
        Err(_) => DiscoveryStoreSnapshot::default(),
    }
}

fn handle_get_sessions(state: &HubState) -> HttpResponse {
    drain_ready_session_queues(state);
    // A store error here must not hide the session list.
    if let Ok(store) = state.open_store() {
        let _ = lifecycle::reconcile_dead_hosted_sessions(&*store, &lifecycle_env(state), "hub");
    }
    let Ok(snap) = state.cached_discovery_snapshot() else {
        return store_open_failure();
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
        match crate::attachment_policy::read_static_file(ui_dir, &candidate) {
            Ok(Some(bytes)) => {
                return HttpResponse::bytes(200, mime_type_for_path(&candidate), bytes);
            }
            Ok(None) => {}
            Err(refusal) => return refusal,
        }

        match crate::attachment_policy::read_static_file(ui_dir, &ui_dir.join("index.html")) {
            Ok(Some(bytes)) => return HttpResponse::bytes(200, "text/html; charset=utf-8", bytes),
            Ok(None) => {}
            Err(refusal) => return refusal,
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
    drop(store);
    let Ok(snap) = state.cached_discovery_snapshot() else {
        return store_open_failure();
    };
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
    drop(store);
    let Ok(snap) = state.cached_discovery_snapshot() else {
        return store_open_failure();
    };
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

fn handle_get_prs(store: &AnyStore) -> HttpResponse {
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

fn handle_get_wire_messages(store: &AnyStore, req: &HttpRequest) -> HttpResponse {
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
            let mut receipts_by_message = all_wire_receipts_by_message(store);
            let mut out = Vec::with_capacity(seq_msgs.len());
            for (_seq, msg) in seq_msgs {
                let receipts = receipts_by_message.remove(&msg.id).unwrap_or_default();
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

/// Every delivery receipt in one store read, grouped by message, each group in the order
/// [`MessageStore::list_wire_receipts`] returns it (by `recipient_id`, ties as stored).
///
/// An unfiltered `list_wire_inbox` returns every receipt: no status, `limit`, recipient or
/// thread filter applies, and every backend (mem, redb, ferro local and remote) drops only a
/// receipt whose message is gone, which no listed message can own. It sorts by message id, then
/// `recipient_id`, stably, so each message's receipts keep their per-message order. A failed
/// read lists no receipts, as a failed per-message read did.
fn all_wire_receipts_by_message(store: &AnyStore) -> BTreeMap<MessageId, Vec<DeliveryReceipt>> {
    let mut grouped: BTreeMap<MessageId, Vec<DeliveryReceipt>> = BTreeMap::new();
    for item in store
        .list_wire_inbox(&WireFilter::default())
        .unwrap_or_default()
    {
        grouped
            .entry(item.message.id)
            .or_default()
            .push(item.receipt);
    }
    grouped
}

/// Scan the host into `routes` unless this request already did; `started` records when.
fn scan_routes_once(
    routes: &mut RouteTable,
    home_override: Option<&Path>,
    started: &mut Option<Instant>,
) {
    if started.is_none() {
        *started = Some(Instant::now());
        crate::discovery::discover_routes_into(routes, home_override);
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

    // The saved route table first, as fl-wire's dispatcher does. The host is scanned at most once
    // per send, and the scan saved, only when a recipient is still to deliver (fl-wire's rule:
    // not acked or expired, under `MAX_DELIVER` attempts, a valid persona) and the saved table is
    // older than fl-wire's `ROUTE_REDISCOVERY_INTERVAL`, a recipient has no route there, or a
    // saved route fails the pre-delivery check or never reaches its receiver. Each recipient is
    // delivered at most once: a push that fails stays `Pending` and is never repeated here,
    // since part of it may have landed. The one exception is a push to a saved route that never
    // reached its receiver (`PushError::NotConnected`): nothing was delivered, so it goes to the
    // route a scan finds instead, when that route is different.
    let home = state.config.home_override.as_deref();
    let saved = RouteTable::load(crate::discovery::route_table_path(home).as_deref());
    let mut routes = saved.clone();
    let mut scan_started: Option<Instant> = None;
    let deliverable = |rcpt: &DeliveryReceipt| {
        !matches!(rcpt.status, DeliveryStatus::Acked | DeliveryStatus::Expired)
            && rcpt.attempts < MAX_DELIVER
    };
    let to_deliver: Vec<&DeliveryReceipt> = receipts
        .iter()
        .filter(|rcpt| deliverable(rcpt) && normalize_persona(&rcpt.recipient_name).is_some())
        .collect();
    let missing_route = to_deliver
        .iter()
        .any(|rcpt| routes.get(&rcpt.recipient_name).is_none());
    if !to_deliver.is_empty()
        && (missing_route || !crate::discovery::saved_routes_fresh(home, Instant::now()))
    {
        scan_routes_once(&mut routes, home, &mut scan_started);
    }
    let mut any_updated = false;
    for rcpt in &receipts {
        if !deliverable(rcpt) {
            continue;
        }
        let Some(mut entry) = routes.get(&rcpt.recipient_name).cloned() else {
            continue;
        };
        if scan_started.is_none() && crate::lifecycle::undeliverable(&entry.route).is_some() {
            // Nothing was sent on the saved route: the session may have been replaced since it
            // was saved, so scan once and push on the route the scan found, if any.
            scan_routes_once(&mut routes, home, &mut scan_started);
            if let Some(fresh) = routes.get(&rcpt.recipient_name).cloned() {
                entry = fresh;
            }
        }
        let mut pushed = push_to_route(&entry.route, &msg);
        if scan_started.is_none() && matches!(pushed, Err(PushError::NotConnected(_))) {
            // The saved route's receiver was never reached, so nothing was delivered: scan once
            // and push on the route the scan found, when it is a different one.
            scan_routes_once(&mut routes, home, &mut scan_started);
            if let Some(fresh) = routes
                .get(&rcpt.recipient_name)
                .filter(|fresh| fresh.route != entry.route)
            {
                pushed = push_to_route(&fresh.route, &msg);
            }
        }
        let target_status = match pushed {
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
    if let Some(started) = scan_started {
        crate::discovery::save_route_scan(home, &saved, &routes, started);
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

fn resolve_message_id(store: &AnyStore, raw: &str) -> Result<MessageId, String> {
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
    store: &AnyStore,
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
    store: &AnyStore,
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

fn resolve_workflow_run_id(store: &AnyStore, raw: &str) -> Result<WorkflowRunId, String> {
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
    store: &AnyStore,
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

fn handle_get_state(store: &AnyStore, authority: &HubAuthority, req: &HttpRequest) -> HttpResponse {
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

/// Rank hub search candidates: internal hub entries excluded before ranking,
/// full ranked count returned before `limit` truncation, sensitive hits
/// redacted for non-owners (entry and snippet) after ranking. The ranker
/// follows the store's engine: BM25 on redb, knowledge search on FerroStore.
/// A search is a read request: it never writes to the vector index, so on
/// FerroStore the vector leg covers what an earlier CLI or MCP search indexed.
fn search_state_for_hub(
    store: &AnyStore,
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
    let ranked =
        fl_store::rank_state_entries_read_only(store, &candidates, query, scope, concern, limit);
    let hits: Vec<StateSearchHit> = ranked
        .hits
        .into_iter()
        .map(|hit| redact_search_hit(hit, is_owner))
        .collect();
    (hits, ranked.total_hits)
}

fn handle_get_state_search(
    store: &AnyStore,
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
        store,
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

/// Whether `root` is the directory `home` names (both resolved; a path that cannot be
/// resolved is not).
fn is_same_dir(root: &Path, home: &Path) -> bool {
    matches!(
        (fs::canonicalize(root), fs::canonicalize(home)),
        (Ok(a), Ok(b)) if a == b
    )
}

/// Whether the hub runs in the user's home directory: its `repo_root` is the effective home
/// (`home_override`, else `$HOME`; see [`crate::commands::resolve_home`]).
fn repo_root_is_home(state: &HubState) -> bool {
    crate::commands::resolve_home(state.config.home_override.as_deref())
        .is_some_and(|home| is_same_dir(&state.config.repo_root, &home))
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
    // The early answers below return no symbols but keep the shape of a search
    // body: the counted saving is 0 when this build counts, else null.
    let no_hits_counted = count_unit().map(|_| 0u64);
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
                "tokens_saved_this_query_counted": no_hits_counted,
                "count_unit": count_unit(),
                "token_gain": load_token_gain(&*store),
            }),
        );
    }

    // A hub started in the home directory (a user service with no `WorkingDirectory`) has no
    // repository: the scan would walk the whole home directory on every request.
    if repo_root_is_home(state) {
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
                "tokens_saved_this_query_counted": no_hits_counted,
                "count_unit": count_unit(),
                "token_gain": load_token_gain(&*store),
                "note": "the hub runs in the home directory, which is not searched; name the repository with `[hub] repo_root` or `fl hub serve --repo-root`",
            }),
        );
    }

    let index = scan_code_index(&state.config.repo_root);
    let Ok(store) = open_store_response(state) else {
        return store_open_failure();
    };
    // A request that reads never writes the code vector index: it answers from
    // whatever a CLI or MCP search has already indexed. It searches through a
    // cached read-only handle on the sidecar (opening one costs more than the
    // search); the handle reads what a CLI search syncs afterwards.
    let (ranked, cache_cause) = state
        .code_sidecars
        .search_read_only(&store, &index, kind, &query, limit);
    if let Some(cause) = cache_cause {
        eprintln!("fl: code sidecar cache unavailable: {cause}");
    }
    // The engine's error behind a fallback goes to the operator's stderr, never
    // into the JSON body.
    if let Some(line) = ranked.unavailable_line() {
        eprintln!("{line}");
    }
    let token_gain = match record_token_gain_query(
        &*store,
        ranked.outcome.tokens_saved_this_query,
        ranked.outcome.tokens_saved_this_query_counted,
        ranked.outcome.files_indexed,
        ranked.outcome.symbols_indexed,
        stamp::now(),
        "fl",
    ) {
        Ok(gain) => gain,
        Err(e) => return HttpResponse::json(500, json!({ "error": e.to_string() })),
    };
    let mut body = json!({
        "query": query,
        "kind": kind.as_wire(),
        "symbols": ranked.outcome.symbols,
        "tokens_saved_this_query": ranked.outcome.tokens_saved_this_query,
        "tokens_saved_this_query_counted": ranked.outcome.tokens_saved_this_query_counted,
        "count_unit": ranked.outcome.count_unit(),
        "token_gain": token_gain,
    });
    ranked.annotate_json(&mut body);
    HttpResponse::json(200, body)
}

fn handle_post_state(
    store: &AnyStore,
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

/// `GET /api/v1/stats?summary=1`: totals across every project; on the ferro engine no row is
/// handed to the hub, though the engine still scans the tables. `source` is `columnar` only when every figure came from the engine's columnar scan.
fn handle_get_stats_summary(store: &AnyStore) -> HttpResponse {
    let totals = fl_store::attempt_totals(store, None)
        .and_then(|(a, sa)| fl_store::gate_run_totals(store, None).map(|(g, sg)| (a, sa, g, sg)));
    let (attempts, attempts_source, runs, runs_source) = match totals {
        Ok(t) => t,
        Err(e) => return HttpResponse::json(500, json!({ "error": e.to_string() })),
    };
    let source = if attempts_source == fl_store::TotalsSource::Columnar
        && runs_source == fl_store::TotalsSource::Columnar
    {
        fl_store::TotalsSource::Columnar
    } else {
        fl_store::TotalsSource::Rows
    };
    HttpResponse::json(
        200,
        json!({
            "attempts": attempts.count,
            "wall_clock_ms": attempts.duration_ms,
            "tokens_in": attempts.tokens_in,
            "tokens_out": attempts.tokens_out,
            "cost_usd_micros": attempts.cost_usd_micros,
            "gate_runs": {
                "pass": runs.pass,
                "fail": runs.fail,
                "error": runs.error,
            },
            "source": source.as_str(),
        }),
    )
}

/// `GET /api/v1/stats`: the full report. Only the literal `summary=1` selects the totals-only
/// response of [`handle_get_stats_summary`]; any other value, `summary=0` included, gives this
/// default response.
fn handle_get_stats(store: &AnyStore, req: &HttpRequest) -> HttpResponse {
    if matches!(req.query.get("summary").map(String::as_str), Some("1")) {
        return handle_get_stats_summary(store);
    }
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
    crate::attachments::attachments_dir(state)
}

const MAX_ARTIFACT_PREVIEW_BYTES: u64 = 16 * 1024 * 1024;

pub(crate) fn percent_encode_query_value(input: &str) -> String {
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

pub(crate) fn is_forbidden_artifact_path(path: &Path) -> bool {
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
                // Denied whatever the selected engine: a `.fls` is a store file.
                || seg.ends_with(".fls")
                // The empty lock file an automatic migration leaves beside a store.
                || seg.ends_with(".migrate.lock")
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
        if state.config.home_override.is_none() {
            candidates.push(home.join("Projects/Artificial-Humanity"));
        }
    }

    // Archived artifact copies, so previews of files whose worktree is gone still work, and the
    // artifact directories the settings name.
    candidates.extend(crate::artifact_dirs::serving_roots(state));

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
pub(crate) fn hub_home_dir(state: &HubState) -> Option<PathBuf> {
    state
        .config
        .home_override
        .clone()
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
}

/// Data root of the artifact registry and its managed copies: the system artifacts directory.
fn artifacts_root(state: &HubState) -> PathBuf {
    crate::artifact_dirs::effective_system_dir(state)
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
        versions: Vec::new(),
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

    let root = artifacts_root(state);
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
    let (outcome, registry_entries) = artifacts::with_registry(&root, |registry| {
        for (canon, size) in &live {
            let shared = attachments.as_ref().is_some_and(|a| canon.starts_with(a));
            let tag = if shared {
                None
            } else {
                canonical_session_id.as_deref()
            };
            registry.upsert(artifact_entry(canon, *size, tag));
        }
        let outcome = registry.reconcile(&artifacts::managed_dir(&root), session_ids.as_deref());
        (outcome, registry.entries.len())
    });
    let live_paths: Vec<PathBuf> = live.iter().map(|(p, _)| p.clone()).collect();
    let pruned_count = outcome.pruned;
    record_artifact_versions(state, &root, &live_paths);
    for entry in &outcome.archived {
        if items.len() >= MAX_ARTIFACT_LIST_ITEMS {
            break;
        }
        items.extend(archived_artifact_item(entry));
    }

    // Measured last, after the snapshots this call recorded.
    let storage = artifacts::storage_summary(&artifacts_root(state), registry_entries);
    HttpResponse::json(
        200,
        json!({ "artifacts": items, "pruned_count": pruned_count, "storage": storage }),
    )
}

/// Record the current content of each live artifact as a revision when it changed. Failures
/// (a file that vanished, grew past the cap, or no longer passes vetting) are skipped.
fn record_artifact_versions(state: &HubState, root: &Path, files: &[PathBuf]) {
    let still_vetted = artifact_still_vetted(state);
    for canon in files {
        let _ = artifacts::record_version(root, canon, &still_vetted);
    }
}

/// The vetting `record_version` runs again on the path it opened.
fn artifact_still_vetted(state: &HubState) -> impl Fn(&Path) -> bool {
    let allowed_roots = allowed_artifact_roots(state);
    move |resolved: &Path| {
        vet_artifact_file(&allowed_roots, resolved).is_some_and(|(canon, _)| canon == resolved)
    }
}

/// `GET /api/v1/artifacts/versions?path=` (Track E): the recorded revisions of one artifact,
/// oldest first. A live file of a type the artifact store keeps is registered and its current
/// content recorded first, so history exists even when the artifact was never listed. The
/// registration happens only once the file passes vetting, and the registry and the stored
/// snapshots are capped (see `artifacts::MAX_REGISTRY_ENTRIES`): past the cap the route answers
/// 507 instead of registering.
fn handle_get_artifact_versions(state: &HubState, req: &HttpRequest) -> HttpResponse {
    let raw_path = req.query.get("path").map(String::as_str).unwrap_or("");
    let canonical = match resolve_artifact_candidate_path(state, raw_path) {
        Ok(p) => p,
        Err((status, msg)) => return HttpResponse::json(status, json!({ "error": msg })),
    };
    let meta = match fs::metadata(&canonical) {
        Ok(m) if m.is_file() => m,
        _ => return HttpResponse::json(404, json!({ "error": "artifact file not found" })),
    };
    let root = artifacts_root(state);
    let canon_str = canonical.display().to_string();
    let managed_copy =
        fs::canonicalize(artifacts::managed_dir(&root)).is_ok_and(|dir| canonical.starts_with(dir));
    if !managed_copy {
        let registered = artifacts::with_registry(&root, |reg| {
            reg.entries.iter().any(|e| e.canonical_path == canon_str)
        });
        let still_vetted = artifact_still_vetted(state);
        if registered {
            let _ = artifacts::record_version(&root, &canonical, &still_vetted);
        } else if !artifacts::has_archive_ext(&canonical) {
            return HttpResponse::json(
                415,
                json!({ "error": "artifacts of this type have no version history" }),
            );
        } else if let Err(err) = artifacts::register_and_record_version(
            &root,
            &canonical,
            artifact_entry(&canonical, meta.len(), None),
            &still_vetted,
        ) && err.kind() == std::io::ErrorKind::QuotaExceeded
        {
            return HttpResponse::json(507, json!({ "error": err.to_string() }));
        }
    }
    let versions = artifacts::with_registry(&root, |reg| {
        reg.find_for_path(&canonical)
            .map(|e| e.versions.clone())
            .unwrap_or_default()
    });
    let latest = versions.last().map(|v| v.version);
    let rows: Vec<Value> = versions
        .iter()
        .map(|v| {
            json!({
                "version": v.version,
                "sha256": v.sha256,
                "size_bytes": v.size_bytes,
                "recorded_at_ms": v.recorded_at_ms,
                "is_current": Some(v.version) == latest,
                "has_content": v.snapshot_path.is_some(),
            })
        })
        .collect();
    HttpResponse::json(
        200,
        json!({ "ok": true, "path": canon_str, "versions": rows }),
    )
}

/// `GET /api/v1/artifacts/version-raw?path=&version=` (Track E): the stored bytes of one
/// revision. The path is vetted like every artifact path; the snapshot is read only from the
/// hub's versions directory and within the archive size cap.
fn handle_get_artifact_version_raw(state: &HubState, req: &HttpRequest) -> HttpResponse {
    let raw_path = req.query.get("path").map(String::as_str).unwrap_or("");
    let canonical = match resolve_artifact_candidate_path(state, raw_path) {
        Ok(p) => p,
        Err((status, msg)) => return HttpResponse::json(status, json!({ "error": msg })),
    };
    let version = match req
        .query
        .get("version")
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|v| *v >= 1)
    {
        Some(v) => v,
        None => {
            return HttpResponse::json(
                400,
                json!({ "error": "missing or invalid query parameter `version`" }),
            );
        }
    };
    let root = artifacts_root(state);
    let entry = artifacts::with_registry(&root, |reg| reg.find_for_path(&canonical).cloned());
    let Some(entry) = entry else {
        return HttpResponse::json(404, json!({ "error": "version not found" }));
    };
    match artifacts::read_version_bytes(&root, &entry, version) {
        Ok(bytes) => {
            let mime = match mime_type_for_path(&canonical) {
                // Never serve a recorded revision as active content.
                "text/html; charset=utf-8" | "application/javascript; charset=utf-8" => {
                    "text/plain; charset=utf-8"
                }
                other => other,
            };
            HttpResponse::bytes(200, mime, bytes)
                .with_header("X-Content-Type-Options", "nosniff")
                .with_header(
                    "Content-Security-Policy",
                    "script-src 'none'; object-src 'none'",
                )
        }
        Err(msg) => {
            let status = if msg.contains("exceeds") { 413 } else { 404 };
            HttpResponse::json(status, json!({ "error": msg }))
        }
    }
}

/// `GET /api/v1/artifacts/office-preview?path=` (Track E): a structured preview of a `.docx`,
/// `.xlsx` or `.pptx`, read by the bounded in-process ZIP/XML reader in `artifacts.rs`.
fn handle_get_artifact_office_preview(state: &HubState, req: &HttpRequest) -> HttpResponse {
    let raw_path = req.query.get("path").map(String::as_str).unwrap_or("");
    let canonical = match resolve_artifact_candidate_path(state, raw_path) {
        Ok(p) => p,
        Err((status, msg)) => return HttpResponse::json(status, json!({ "error": msg })),
    };
    let ext = canonical
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !matches!(ext.as_str(), "docx" | "xlsx" | "pptx") {
        return HttpResponse::json(
            400,
            json!({ "error": "office preview supports only .docx, .xlsx and .pptx files" }),
        );
    }
    let meta = match fs::metadata(&canonical) {
        Ok(m) if m.is_file() => m,
        _ => return HttpResponse::json(404, json!({ "error": "artifact file not found" })),
    };
    if meta.len() > MAX_ARTIFACT_PREVIEW_BYTES {
        return HttpResponse::json(
            413,
            json!({
                "error": "artifact exceeds 16 MiB preview limit",
                "size_bytes": meta.len(),
                "max_bytes": MAX_ARTIFACT_PREVIEW_BYTES,
            }),
        );
    }
    let bytes = match fs::read(&canonical) {
        Ok(b) => b,
        Err(e) => return HttpResponse::json(500, json!({ "error": e.to_string() })),
    };
    match artifacts::office_preview(&ext, &bytes) {
        Ok(preview) => HttpResponse::json(
            200,
            json!({
                "ok": true,
                "path": canonical.display().to_string(),
                "name": canonical.file_name().and_then(|n| n.to_str()).unwrap_or("artifact"),
                "ext": ext,
                "size_bytes": meta.len(),
                "preview": preview,
            }),
        ),
        Err(err) => HttpResponse::json(422, json!({ "error": err.to_string() })),
    }
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
    let archived: Vec<PathBuf> = copies.iter().map(|(canon, _)| canon.clone()).collect();
    record_artifact_versions(state, root, &archived);
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
    let root = artifacts_root(state);
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
/// only. Body (all optional): `paths`, `worktree_path`, `session_id`, and the cleanup filters
/// `dry_run`, `max_age_days`, `status` (`archived` | `live` | `all`), `kinds` and
/// `prune_versions_keep`; their semantics are on [`artifacts::CleanupFilter`]. A filter that
/// removes entries or version rows runs after the archive and reconcile steps, in the same
/// registry transaction. `dry_run: true` changes nothing (no deletion, no registry write) and
/// answers with what the same call would remove. A real call archives `paths`, `worktree_path`
/// and `session_id` artifacts first, which changes what the filter removes, so a dry run cannot
/// be combined with any of them: that is a 400. `prune_versions_keep` must be at least 1 (0 is
/// a 400), because an entry with no version row would number its next revision 1 again.
///
/// Answers `ok`, `pruned_count` (entries reconcile dropped), `archived_count` and `active_count`
/// (reconcile's counts, before any filter removal), plus `dry_run`, `removed_entries` (reconcile
/// plus filter), `removed_versions` (rows), `bytes_freed` (regular files removed),
/// `failed_deletes` (files that could not be deleted: their rows are gone anyway, so each one
/// is an orphan file left in `managed/` or `versions/`; 0 in a dry run) and `storage` (see
/// [`artifacts::StorageSummary`], measured after the action).
/// Apply the `canonical_root` rule for consolidate responses.
///
/// Rule (placeholder rule): if `layout.canonical_root` is the
/// `DedicatedArtifactLayoutState::default()` placeholder
/// (`~/.local/share/fl/artifacts/`), it is not a real persisted choice, so
/// it is replaced with `artifact_dirs::effective_system_dir(state)`. A
/// stored state explicitly persisted with any other root keeps that root.
/// This needs no core existence check and is deterministic for tests.
///
/// The rule only applies pre-persist: a real consolidation persists the
/// resolved root via `fl_core::consolidate_artifact_layout`, so after a
/// POST the stored root is the (non-placeholder) resolved root and a later
/// GET returns it unchanged even if the settings (and therefore the
/// effective system dir) have since changed.
fn artifact_layout_for_response(
    state: &HubState,
    mut layout: fl_core::DedicatedArtifactLayoutState,
) -> fl_core::DedicatedArtifactLayoutState {
    if layout.canonical_root == fl_core::DedicatedArtifactLayoutState::default().canonical_root {
        layout.canonical_root = crate::artifact_dirs::effective_system_dir(state)
            .display()
            .to_string();
    }
    layout
}

/// `GET /api/v1/artifacts/consolidate` (Records #77/#186/#207): current
/// dedicated artifact layout. Any authenticated authority may read it.
/// `canonical_root` follows the placeholder rule documented on
/// [`artifact_layout_for_response`].
fn handle_get_artifacts_consolidate(state: &HubState) -> HttpResponse {
    let Ok(store) = open_store_response(state) else {
        return store_open_failure();
    };
    match fl_core::get_artifact_layout(&*store) {
        Ok(layout) => HttpResponse::json(
            200,
            serde_json::to_value(artifact_layout_for_response(state, layout)).unwrap_or(json!({})),
        ),
        Err(e) => HttpResponse::json(500, json!({ "error": e.to_string() })),
    }
}

/// `POST /api/v1/artifacts/consolidate` (Records #77/#186/#207): Owner only.
///
/// Body: `{ dry_run?: bool, retention_policy?: ArtifactRetentionPolicyConfig }`.
/// A missing `dry_run` defaults to `false`; a non-bool `dry_run` is a 400.
/// A present `retention_policy` is deserialized and validated; invalid
/// values are a 400. `dry_run: true` mutates nothing and reports the
/// scattered totals over unconsolidated origins; its response `layout`
/// reflects the supplied `retention_policy` in memory only (a later GET
/// still shows the persisted policy). `dry_run: true` reads strictly, so
/// corrupt stored state is a 500 rather than a false preview.
/// `dry_run: false` resolves `canonical_root` (the effective system dir
/// when the stored root is the `Default` placeholder, else the stored
/// root) and calls `fl_core::consolidate_artifact_layout`, which persists
/// that resolved root.
///
/// NOTE: consolidation marks the stored layout state only (every origin's
/// `consolidated` flag, plus the retention policy). Physical file migration
/// into the canonical root is a follow-up; this handler does not move files.
fn handle_post_artifacts_consolidate(
    state: &HubState,
    authority: &HubAuthority,
    req: &HttpRequest,
) -> HttpResponse {
    if !authority.is_owner() {
        return HttpResponse::json(
            403,
            json!({ "error": "only Owner authority may consolidate artifacts" }),
        );
    }
    let body = match json_object_body(req) {
        Ok(body) => body,
        Err(resp) => return resp,
    };
    let dry_run = match body.get("dry_run") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => {
            return HttpResponse::json(400, json!({ "error": "`dry_run` must be a boolean" }));
        }
    };
    let retention = match body.get("retention_policy") {
        None | Some(Value::Null) => None,
        Some(value) => {
            let parsed: fl_core::ArtifactRetentionPolicyConfig =
                match serde_json::from_value(value.clone()) {
                    Ok(p) => p,
                    Err(e) => {
                        return HttpResponse::json(
                            400,
                            json!({ "error": format!("invalid `retention_policy`: {e}") }),
                        );
                    }
                };
            if let Err(e) = parsed.validate() {
                return HttpResponse::json(400, json!({ "error": e.to_string() }));
            }
            Some(parsed)
        }
    };
    let Ok(store) = open_store_response(state) else {
        return store_open_failure();
    };
    if dry_run {
        let mut layout = match fl_core::get_artifact_layout_strict(&*store) {
            Ok(l) => l,
            Err(e) => return HttpResponse::json(500, json!({ "error": e.to_string() })),
        };
        let (files, bytes) = fl_core::scattered_totals(&layout);
        // In-memory only: preview the supplied retention, never persist it.
        if let Some(r) = retention {
            layout.retention_policy = r;
        }
        return HttpResponse::json(
            200,
            json!({
                "dry_run": true,
                "consolidated_files": files,
                "consolidated_bytes": bytes,
                "layout": artifact_layout_for_response(state, layout),
            }),
        );
    }
    let stored = match fl_core::get_artifact_layout_strict(&*store) {
        Ok(s) => s,
        Err(e) => return HttpResponse::json(500, json!({ "error": e.to_string() })),
    };
    let resolved_root = if stored.canonical_root
        == fl_core::DedicatedArtifactLayoutState::default().canonical_root
    {
        crate::artifact_dirs::effective_system_dir(state)
            .display()
            .to_string()
    } else {
        stored.canonical_root
    };
    match fl_core::consolidate_artifact_layout(&*store, retention, Some(resolved_root)) {
        Ok((layout, files, bytes)) => HttpResponse::json(
            200,
            json!({
                "dry_run": false,
                "consolidated_files": files,
                "consolidated_bytes": bytes,
                "layout": artifact_layout_for_response(state, layout),
            }),
        ),
        Err(e) => HttpResponse::json(500, json!({ "error": e.to_string() })),
    }
}

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
    let (filter, dry_run) = match parse_cleanup_filter(&body) {
        Ok(parsed) => parsed,
        Err(e) => return HttpResponse::json(400, json!({ "error": e })),
    };
    if dry_run && (!paths.is_empty() || !worktree_raw.is_empty() || !session_raw.is_empty()) {
        return HttpResponse::json(
            400,
            json!({ "error": "a dry run cannot be combined with paths, worktree_path or \
                session_id, because archiving them changes what a real run removes" }),
        );
    }
    let root = artifacts_root(state);

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

    if !worktree_missing && !dry_run {
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
    let report = artifacts::run_cleanup(&root, &filter, dry_run);
    let outcome = &report.reconciled;
    let storage = artifacts::storage_summary(&root, report.registry_entries);
    HttpResponse::json(
        200,
        json!({
            "ok": true,
            "pruned_count": outcome.pruned,
            "archived_count": outcome.archived.len(),
            "active_count": outcome.live,
            "dry_run": dry_run,
            "removed_entries": report.totals.removed_entries,
            "removed_versions": report.totals.removed_versions,
            "bytes_freed": report.totals.bytes_freed,
            "failed_deletes": report.totals.failed_deletes,
            "storage": storage,
        }),
    )
}

/// An optional non-negative integer field; `null` is the same as absent.
fn json_optional_u64(body: &Value, key: &str) -> Result<Option<u64>, String> {
    match body.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .map(Some)
            .ok_or_else(|| format!("`{key}` must be a non-negative integer")),
    }
}

/// The cleanup filters of a prune body, and `dry_run`. Any wrong type or unknown value is an
/// error message for a 400.
fn parse_cleanup_filter(body: &Value) -> Result<(artifacts::CleanupFilter, bool), String> {
    let dry_run = match body.get("dry_run") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err("`dry_run` must be a boolean".to_string()),
    };
    let status = match body.get("status") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(match s.as_str() {
            "archived" => artifacts::StatusFilter::Archived,
            "live" => artifacts::StatusFilter::Live,
            "all" => artifacts::StatusFilter::All,
            _ => return Err("`status` must be one of: archived, live, all".to_string()),
        }),
        Some(_) => return Err("`status` must be a string".to_string()),
    };
    let kinds = match body.get("kinds") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => {
            let mut kinds = Vec::with_capacity(items.len());
            for kind in json_string_items("kinds", items)? {
                let kind = kind.to_lowercase();
                if !artifacts::ARTIFACT_KINDS.contains(&kind.as_str()) {
                    return Err(format!(
                        "unknown kind `{kind}`; valid kinds: {}",
                        artifacts::ARTIFACT_KINDS.join(", ")
                    ));
                }
                kinds.push(kind);
            }
            kinds
        }
        Some(_) => return Err("`kinds` must be an array of strings".to_string()),
    };
    let prune_versions_keep = json_optional_u64(body, "prune_versions_keep")?
        .map(|n| match usize::try_from(n) {
            Ok(0) => Err(
                "`prune_versions_keep` must be >= 1 (an entry that kept no version \
                would renumber its next revision 1; remove entries with a status, kinds or \
                max_age_days filter instead)"
                    .to_string(),
            ),
            Ok(n) => Ok(n),
            Err(_) => Err("`prune_versions_keep` must be a non-negative integer".to_string()),
        })
        .transpose()?;
    Ok((
        artifacts::CleanupFilter {
            max_age_days: json_optional_u64(body, "max_age_days")?,
            status,
            kinds,
            prune_versions_keep,
        },
        dry_run,
    ))
}

/// `GET /api/v1/settings` (Record 4): the saved settings, or the defaults.
fn handle_get_settings(state: &HubState) -> HttpResponse {
    let stored =
        hub_home_dir(state).map_or_else(settings::Settings::default, |home| settings::load(&home));
    HttpResponse::json(200, crate::artifact_dirs::settings_view(state, &stored))
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
            let data = crate::artifact_dirs::settings_view(state, &updated);
            state.broadcast("settings", "settings.updated", data.clone());
            HttpResponse::json(200, data)
        }
        Err(settings::UpdateError::Invalid(msg)) => {
            HttpResponse::json(400, json!({ "error": msg }))
        }
        Err(settings::UpdateError::Damaged(why)) => HttpResponse::json(
            409,
            json!({
                "error": format!(
                    "the stored settings file is damaged ({why}); include \
                     attachment_retention_days explicitly to repair it"
                )
            }),
        ),
        Err(settings::UpdateError::Io(e)) => HttpResponse::json(
            500,
            json!({ "error": format!("could not save settings: {e}") }),
        ),
    }
}

fn role_error_response(err: &RoleError) -> HttpResponse {
    let status = match err {
        RoleError::BuiltinRole(_) => 400,
        RoleError::NotFound(_) => 404,
        RoleError::InvalidReviewPolicy(_) | RoleError::InvalidSlug(_) | RoleError::EmptyLabel => {
            400
        }
        RoleError::Store(_) => 500,
    };
    HttpResponse::json(status, json!({ "error": err.to_string() }))
}

/// `GET /api/v1/roles` (Record #74): all roles. Any authenticated caller.
fn handle_get_roles(state: &HubState) -> HttpResponse {
    let Ok(store) = open_store_response(state) else {
        return store_open_failure();
    };
    match fl_core::agent::list_roles(&*store) {
        Ok(roles) => HttpResponse::json(200, serde_json::to_value(&roles).unwrap_or(json!([]))),
        Err(e) => HttpResponse::json(500, json!({ "error": e.to_string() })),
    }
}

/// `POST /api/v1/roles` (Record #74): validate and persist a role. Owner only,
/// like `PUT /api/v1/settings`. 201 for a new slug, 200 for an override.
fn handle_post_role(state: &HubState, authority: &HubAuthority, req: &HttpRequest) -> HttpResponse {
    if !authority.is_owner() {
        return HttpResponse::json(
            403,
            json!({ "error": "only Owner authority may change roles" }),
        );
    }
    let record: RoleRecord = match serde_json::from_slice(&req.body) {
        Ok(record) => record,
        Err(e) => {
            return HttpResponse::json(400, json!({ "error": format!("invalid role: {e}") }));
        }
    };
    let Ok(store) = open_store_response(state) else {
        return store_open_failure();
    };
    let existed = matches!(fl_core::agent::get_role(&*store, &record.slug), Ok(Some(_)));
    match fl_core::agent::upsert_role(&*store, record) {
        Ok(saved) => HttpResponse::json(
            if existed { 200 } else { 201 },
            serde_json::to_value(&saved).unwrap_or(json!({})),
        ),
        Err(e) => role_error_response(&e),
    }
}

/// `DELETE /api/v1/roles/{slug}` (Record #74): delete a custom role. Owner only.
fn handle_delete_role(state: &HubState, authority: &HubAuthority, raw_slug: &str) -> HttpResponse {
    if !authority.is_owner() {
        return HttpResponse::json(
            403,
            json!({ "error": "only Owner authority may change roles" }),
        );
    }
    let slug = percent_decode(raw_slug);
    let Ok(store) = open_store_response(state) else {
        return store_open_failure();
    };
    match fl_core::agent::delete_role(&*store, &slug) {
        Ok(()) => {
            let normalized = fl_core::agent::normalize_role_slug(&slug).unwrap_or(slug);
            HttpResponse::json(200, json!({ "deleted": true, "slug": normalized }))
        }
        Err(e) => role_error_response(&e),
    }
}

/// `GET /api/v1/cloud-sessions` (Record #71): all vendor cloud sessions.
/// Any authenticated caller.
fn handle_get_cloud_sessions(state: &HubState) -> HttpResponse {
    let Ok(store) = open_store_response(state) else {
        return store_open_failure();
    };
    match fl_core::list_cloud_sessions(&*store) {
        Ok(sessions) => {
            HttpResponse::json(200, serde_json::to_value(&sessions).unwrap_or(json!([])))
        }
        Err(e) => HttpResponse::json(500, json!({ "error": e.to_string() })),
    }
}

fn cloud_session_not_found() -> HttpResponse {
    HttpResponse::json(404, json!({ "error": "cloud session not found" }))
}

/// `POST /api/v1/cloud-sessions/{id}/attach` (Record #71): mark mirrored. Owner only.
fn handle_post_cloud_session_attach(
    state: &HubState,
    authority: &HubAuthority,
    raw_id: &str,
) -> HttpResponse {
    if !authority.is_owner() {
        return HttpResponse::json(
            403,
            json!({ "error": "only Owner authority may change cloud sessions" }),
        );
    }
    let session_id = percent_decode(raw_id);
    let Ok(store) = open_store_response(state) else {
        return store_open_failure();
    };
    match fl_core::attach_cloud_session(&*store, &session_id) {
        Ok(Some(entry)) => {
            HttpResponse::json(200, serde_json::to_value(&entry).unwrap_or(json!({})))
        }
        Ok(None) => cloud_session_not_found(),
        Err(e) => HttpResponse::json(500, json!({ "error": e.to_string() })),
    }
}

/// `POST /api/v1/cloud-sessions/{id}/handoff` (Record #71): record a handoff. Owner only.
fn handle_post_cloud_session_handoff(
    state: &HubState,
    authority: &HubAuthority,
    raw_id: &str,
) -> HttpResponse {
    if !authority.is_owner() {
        return HttpResponse::json(
            403,
            json!({ "error": "only Owner authority may change cloud sessions" }),
        );
    }
    let session_id = percent_decode(raw_id);
    let Ok(store) = open_store_response(state) else {
        return store_open_failure();
    };
    let synced_at = stamp::now().as_str().to_string();
    match fl_core::record_cloud_session_handoff(&*store, &session_id, synced_at.clone()) {
        Ok(Some(entry)) => HttpResponse::json(
            200,
            json!({
                "session_id": entry.session_id,
                "repo_branch": entry.repo_branch,
                "handoff_synced": true,
                "synced_at": synced_at,
            }),
        ),
        Ok(None) => cloud_session_not_found(),
        Err(e) => HttpResponse::json(500, json!({ "error": e.to_string() })),
    }
}

#[derive(Debug, Deserialize)]
struct CloudSessionStatusBody {
    status: fl_core::VendorCloudSessionStatus,
}

/// `POST /api/v1/cloud-sessions/{id}/status` (Record #71): set status. Owner only.
fn handle_post_cloud_session_status(
    state: &HubState,
    authority: &HubAuthority,
    raw_id: &str,
    req: &HttpRequest,
) -> HttpResponse {
    if !authority.is_owner() {
        return HttpResponse::json(
            403,
            json!({ "error": "only Owner authority may change cloud sessions" }),
        );
    }
    let body: CloudSessionStatusBody = match serde_json::from_slice(&req.body) {
        Ok(body) => body,
        Err(e) => {
            return HttpResponse::json(
                400,
                json!({ "error": format!("invalid cloud session status: {e}") }),
            );
        }
    };
    let session_id = percent_decode(raw_id);
    let Ok(store) = open_store_response(state) else {
        return store_open_failure();
    };
    match fl_core::set_cloud_session_status(&*store, &session_id, body.status) {
        Ok(Some(entry)) => {
            HttpResponse::json(200, serde_json::to_value(&entry).unwrap_or(json!({})))
        }
        Ok(None) => cloud_session_not_found(),
        Err(e) => HttpResponse::json(500, json!({ "error": e.to_string() })),
    }
}

/// `GET /api/v1/providers/local` (Record #72): all local model providers.
/// Any authenticated caller.
fn handle_get_local_providers(state: &HubState) -> HttpResponse {
    let Ok(store) = open_store_response(state) else {
        return store_open_failure();
    };
    match fl_core::list_local_model_providers(&*store) {
        Ok(presets) => HttpResponse::json(200, serde_json::to_value(&presets).unwrap_or(json!([]))),
        Err(e) => HttpResponse::json(500, json!({ "error": e.to_string() })),
    }
}

/// `POST /api/v1/providers/local` (Record #72): update a provider by kind. Owner only.
fn handle_post_local_provider(
    state: &HubState,
    authority: &HubAuthority,
    req: &HttpRequest,
) -> HttpResponse {
    if !authority.is_owner() {
        return HttpResponse::json(
            403,
            json!({ "error": "only Owner authority may change local providers" }),
        );
    }
    let update: fl_core::LocalProviderUpdate = match serde_json::from_slice(&req.body) {
        Ok(update) => update,
        Err(e) => {
            return HttpResponse::json(
                400,
                json!({ "error": format!("invalid local provider: {e}") }),
            );
        }
    };
    let Ok(store) = open_store_response(state) else {
        return store_open_failure();
    };
    match fl_core::update_local_model_provider_by_kind(&*store, update) {
        Ok(preset) => HttpResponse::json(200, serde_json::to_value(&preset).unwrap_or(json!({}))),
        Err(e @ fl_core::StoreError::ProviderValidation(_)) => {
            HttpResponse::json(400, json!({ "error": e.to_string() }))
        }
        Err(e) => HttpResponse::json(500, json!({ "error": e.to_string() })),
    }
}

/// `GET /api/v1/notifications/subscribe` (Record #67): current operator push
/// state (defaults when unset). Any authenticated caller.
fn handle_get_notifications_subscribe(state: &HubState) -> HttpResponse {
    let Ok(store) = open_store_response(state) else {
        return store_open_failure();
    };
    match fl_core::get_operator_push_state(&*store) {
        Ok(saved) => HttpResponse::json(200, serde_json::to_value(&saved).unwrap_or(json!({}))),
        Err(e) => HttpResponse::json(500, json!({ "error": e.to_string() })),
    }
}

/// `POST /api/v1/notifications/subscribe` (Record #67): validate and persist
/// the operator push state. Owner only, like `POST /api/v1/roles`.
fn handle_post_notifications_subscribe(
    state: &HubState,
    authority: &HubAuthority,
    req: &HttpRequest,
) -> HttpResponse {
    if !authority.is_owner() {
        return HttpResponse::json(
            403,
            json!({ "error": "only Owner authority may change notifications" }),
        );
    }
    let parsed: fl_core::OperatorPushSubscriptionState = match serde_json::from_slice(&req.body) {
        Ok(parsed) => parsed,
        Err(e) => {
            return HttpResponse::json(
                400,
                json!({ "error": format!("invalid notification subscription: {e}") }),
            );
        }
    };
    if let Err(e) = parsed.validate() {
        return HttpResponse::json(400, json!({ "error": e.to_string() }));
    }
    let Ok(store) = open_store_response(state) else {
        return store_open_failure();
    };
    match fl_core::set_operator_push_state(&*store, parsed) {
        Ok(saved) => HttpResponse::json(200, serde_json::to_value(&saved).unwrap_or(json!({}))),
        Err(e) => HttpResponse::json(500, json!({ "error": e.to_string() })),
    }
}

#[derive(Debug, Deserialize)]
struct TestPushBody {
    deep_link: String,
    #[serde(default)]
    project_scope: Option<String>,
}

/// Same-origin path only: starts with a single `/` (not `//`, which a
/// browser treats as protocol-relative) and has no backslash, whitespace,
/// or control characters.
fn is_valid_deep_link(deep_link: &str) -> bool {
    deep_link.starts_with('/')
        && !deep_link.starts_with("//")
        && !deep_link
            .chars()
            .any(|c| c == '\\' || c.is_whitespace() || c.is_control())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TestPushEvaluation {
    dispatched: bool,
    deep_link: String,
    project_scope: Option<String>,
    channels: Vec<String>,
    quiet_hours_active: bool,
    receipt: fl_core::PushDispatchReceipt,
}

/// Pure test-push evaluation: quiet/channel decision and the receipt to
/// record, for an explicit UTC `current_hh_mm` and `at_ms`.
fn evaluate_test_push(
    state: &fl_core::OperatorPushSubscriptionState,
    deep_link: String,
    project_scope: Option<String>,
    current_hh_mm: &str,
    at_ms: u64,
) -> TestPushEvaluation {
    let quiet_hours_active = state.quiet_hours.is_quiet_at(current_hh_mm);
    let channels: Vec<String> = fl_core::active_channels(state)
        .into_iter()
        .map(str::to_string)
        .collect();
    let dispatched = !channels.is_empty() && !quiet_hours_active;
    let receipt = fl_core::PushDispatchReceipt {
        deep_link: deep_link.clone(),
        project_scope: project_scope.clone(),
        channels: channels.clone(),
        quiet_hours_active,
        dispatched,
        at_ms,
    };
    TestPushEvaluation {
        dispatched,
        deep_link,
        project_scope,
        channels,
        quiet_hours_active,
        receipt,
    }
}

/// `POST /api/v1/notifications/test` (Record #67): record a test dispatch.
/// Owner only. The hub has no injected clock here, so the current time comes
/// from `fl_exec::stamp::now()` (an `fl_core::At` UTC timestamp); its
/// `HH:MM` slice (chars 11..16) is the UTC wall-clock time quiet hours are
/// checked against.
fn handle_post_notifications_test(
    state: &HubState,
    authority: &HubAuthority,
    req: &HttpRequest,
) -> HttpResponse {
    if !authority.is_owner() {
        return HttpResponse::json(
            403,
            json!({ "error": "only Owner authority may send test notifications" }),
        );
    }
    let body: TestPushBody = match serde_json::from_slice(&req.body) {
        Ok(body) => body,
        Err(e) => {
            return HttpResponse::json(
                400,
                json!({ "error": format!("invalid test notification: {e}") }),
            );
        }
    };
    if !is_valid_deep_link(&body.deep_link) {
        return HttpResponse::json(
            400,
            json!({ "error": "deep_link must be a same-origin path starting with '/'" }),
        );
    }
    let Ok(store) = open_store_response(state) else {
        return store_open_failure();
    };
    let push_state = match fl_core::get_operator_push_state(&*store) {
        Ok(push_state) => push_state,
        Err(e) => return HttpResponse::json(500, json!({ "error": e.to_string() })),
    };
    let now = stamp::now();
    let evaluation = evaluate_test_push(
        &push_state,
        body.deep_link,
        body.project_scope,
        &now.as_str()[11..16],
        now.unix_millis(),
    );
    if let Err(e) = fl_core::append_push_receipt(&*store, evaluation.receipt.clone()) {
        return HttpResponse::json(500, json!({ "error": e.to_string() }));
    }
    HttpResponse::json(
        200,
        json!({
            "dispatched": evaluation.dispatched,
            "deep_link": evaluation.deep_link,
            "project_scope": evaluation.project_scope,
            "channels": evaluation.channels,
            "quiet_hours_active": evaluation.quiet_hours_active,
        }),
    )
}

/// `GET /api/v1/notifications/events` (Record #67): stored dispatch receipts.
/// Any authenticated caller.
fn handle_get_notifications_events(state: &HubState) -> HttpResponse {
    let Ok(store) = open_store_response(state) else {
        return store_open_failure();
    };
    let receipts = fl_core::list_push_receipts(&*store);
    HttpResponse::json(200, serde_json::to_value(&receipts).unwrap_or(json!([])))
}

/// `GET /api/v1/cloud/adapters` (Records 52 and 53): the saved cloud adapter config, or the
/// defaults. A saved file that cannot be used is a 500, never a silent reset.
fn handle_get_cloud_adapters(state: &HubState) -> HttpResponse {
    let Some(home) = hub_home_dir(state) else {
        return HttpResponse::json(200, json!(cloud_adapters::CloudAdaptersConfig::default()));
    };
    match cloud_adapters::load(&home) {
        Ok(config) => HttpResponse::json(200, json!(config)),
        Err(cloud_adapters::Unreadable) => {
            HttpResponse::json(500, json!({ "error": cloud_adapters::UNREADABLE_MESSAGE }))
        }
    }
}

/// `PUT /api/v1/cloud/adapters`: merge a full or partial body into the saved config and
/// validate the merged result. Owner only, checked before any file access. Broadcasts
/// `cloud_adapters.updated` on the `cloud_adapters` channel.
fn handle_put_cloud_adapters(
    state: &HubState,
    authority: &HubAuthority,
    req: &HttpRequest,
) -> HttpResponse {
    if !authority.is_owner() {
        return HttpResponse::json(
            403,
            json!({ "error": "only Owner authority may change cloud adapter settings" }),
        );
    }
    let body = match json_object_body(req) {
        Ok(body) => body,
        Err(resp) => return resp,
    };
    let patch: cloud_adapters::CloudAdaptersPatch = match serde_json::from_value(body) {
        Ok(patch) => patch,
        Err(e) => {
            return HttpResponse::json(
                400,
                json!({ "error": format!("invalid cloud adapter settings: {e}") }),
            );
        }
    };
    let Some(home) = hub_home_dir(state) else {
        return HttpResponse::json(500, json!({ "error": "home directory unavailable" }));
    };
    match cloud_adapters::update(&home, patch) {
        Ok(updated) => {
            let data = json!(updated);
            state.broadcast("cloud_adapters", "cloud_adapters.updated", data.clone());
            HttpResponse::json(200, data)
        }
        Err(cloud_adapters::UpdateError::Invalid(msg)) => {
            HttpResponse::json(400, json!({ "error": msg }))
        }
        Err(cloud_adapters::UpdateError::Unreadable) => {
            HttpResponse::json(500, json!({ "error": cloud_adapters::UNREADABLE_MESSAGE }))
        }
        Err(cloud_adapters::UpdateError::Io(_)) => HttpResponse::json(
            500,
            json!({ "error": "could not save cloud adapter settings" }),
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
        return crate::attachment_policy::serve_attachment(&attachments_dir(state), &filename);
    }

    // /api/v1/auth/operators/:id
    if method == "DELETE"
        && segments.len() == 5
        && segments[0] == "api"
        && segments[1] == "v1"
        && segments[2] == "auth"
        && segments[3] == "operators"
    {
        return operators::handle_delete_operator(state, authority, segments[4]);
    }

    // /api/v1/agents/:name/recast
    if method == "POST"
        && segments.len() == 5
        && segments[0] == "api"
        && segments[1] == "v1"
        && segments[2] == "agents"
        && segments[4] == "recast"
    {
        return crate::agents::handle_post_agent_recast(state, authority, req, segments[3]);
    }

    // /api/v1/records/:id/move
    if method == "POST"
        && segments.len() == 5
        && segments[0] == "api"
        && segments[1] == "v1"
        && segments[2] == "records"
        && segments[4] == "move"
    {
        return records::handle_post_record_move(state, authority, req, segments[3]);
    }

    // /api/v1/sessions/:id/transcript, /prompt, /clear, /decommission, /remodel, or /queue
    if segments.len() == 5
        && segments[0] == "api"
        && segments[1] == "v1"
        && segments[2] == "sessions"
    {
        let session_id = percent_decode(segments[3]);
        match (method, segments[4]) {
            ("GET", "transcript") => {
                drain_ready_session_queues(state);
                let snap = state.cached_discovery_snapshot().unwrap_or_default();
                let home = state.config.home_override.as_deref();
                // One route lookup and one read and parse per transcript file serve every step.
                let sessions = lookup_sessions(&snap, home);
                let mut files = TranscriptReads::default();
                let (canonical_id, _persona) = resolve_session_target_in(&sessions, &session_id);
                let resolved_session_id = sessions
                    .iter()
                    .find(|session| {
                        session.id == session_id.trim()
                            || session.persona.as_deref() == Some(session_id.trim())
                    })
                    .map(|session| session.id.clone());
                let all_observation = observe_transcript_from_sessions(
                    &session_id,
                    &sessions,
                    &snap,
                    home,
                    &mut files,
                );
                let canonical_observation = if canonical_id != session_id.trim() {
                    transcript_observation_from_sessions(
                        &canonical_id,
                        &sessions,
                        &snap,
                        home,
                        &mut files,
                    )
                } else {
                    match &all_observation {
                        TranscriptObservation::Observed(turns) => Some(turns.clone()),
                        TranscriptObservation::Unavailable => None,
                        TranscriptObservation::NoSource => Some(Vec::new()),
                    }
                };
                let is_busy = state.is_session_busy_from_observation(
                    &canonical_id,
                    canonical_observation.as_deref(),
                );
                let all_turns = match all_observation {
                    TranscriptObservation::Observed(turns) => turns,
                    unavailable => {
                        let reason = match unavailable {
                            TranscriptObservation::Unavailable => "unstable_or_unreadable",
                            TranscriptObservation::NoSource => "source_not_available",
                            TranscriptObservation::Observed(_) => unreachable!(),
                        };
                        return HttpResponse::json(
                            503,
                            json!({
                                "code": "transcript_unavailable",
                                "reason": reason,
                            }),
                        );
                    }
                };
                let total_turns = all_turns.len();
                let limit_opt = match req.query.get("limit").map(|s| s.trim()) {
                    Some("all" | "0") => None,
                    Some(raw) => raw.parse::<usize>().ok().or(Some(250)),
                    None => Some(250),
                };
                let turns = match limit_opt {
                    Some(limit) if total_turns > limit => &all_turns[total_turns - limit..],
                    _ => &all_turns[..],
                };
                let queue = state.session_queue(&canonical_id);
                let context_usage =
                    context_usage_from_sessions(&session_id, &sessions, home, &mut files);
                let quota_usage =
                    read_session_quota_usage_with_reads(&canonical_id, home, &mut files).or_else(
                        || {
                            if canonical_id == session_id {
                                None
                            } else {
                                read_session_quota_usage_with_reads(&session_id, home, &mut files)
                            }
                        },
                    );
                let mut body = json!({
                    "session_id": session_id,
                    "turns": turns,
                    "total_turns": total_turns,
                    "is_busy": is_busy,
                    "in_flight_unconfirmed": state.session_in_flight_unconfirmed(&canonical_id),
                    "queue": queue,
                    "context_usage": context_usage,
                    "quota_usage": quota_usage,
                });
                if let Some(resolved_session_id) = resolved_session_id {
                    body["resolved_session_id"] = json!(resolved_session_id);
                }
                return HttpResponse::json(200, body);
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
                        "in_flight_unconfirmed": state.session_in_flight_unconfirmed(&canonical_id),
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
            ("POST", "context-window") => {
                return handle_post_session_context_window(state, authority, &session_id, req);
            }
            ("POST", "decommission") => {
                return handle_post_session_decommission(state, authority, &session_id, req);
            }
            ("POST", "remodel") => {
                return handle_post_session_remodel(state, authority, &session_id, req);
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
            return handle_post_queued_prompt_send_now(state, authority, &session_id, &prompt_id);
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

    // /api/v1/prs/:number/review or /api/v1/prs/:number/merge, and the project-scoped merge
    // /api/v1/projects/:project/prs/:number/merge and /api/v1/projects/:project/pr/merge.
    // The Hub reads the pull request from GitHub; see `github_pr`.
    if segments.len() == 5 && segments[0] == "api" && segments[1] == "v1" && segments[2] == "prs" {
        let Ok(number) = segments[3].parse::<u64>() else {
            return HttpResponse::json(400, json!({ "error": "invalid PR number" }));
        };
        let Ok(store) = open_store_response(state) else {
            return store_open_failure();
        };
        match (method, segments[4]) {
            ("POST", "review") => {
                return github_pr::handle_post_pr_review(state, &store, authority, number, req);
            }
            ("POST", "merge") => {
                return github_pr::handle_post_pr_merge(
                    state,
                    &store,
                    authority,
                    Some(number),
                    None,
                    req,
                );
            }
            _ => {}
        }
    }
    // /api/v1/projects/:slug
    if segments.len() == 4
        && segments[0] == "api"
        && segments[1] == "v1"
        && segments[2] == "projects"
        && matches!(method, "PATCH" | "DELETE")
    {
        return if method == "PATCH" {
            crate::projects_api::handle_patch_project(state, authority, segments[3], req)
        } else {
            crate::projects_api::handle_delete_project(state, authority, segments[3], req)
        };
    }
    // /api/v1/projects/:id/artifacts-dir. The merge routes below have six or more segments.
    if segments.len() == 5
        && segments[0] == "api"
        && segments[1] == "v1"
        && segments[2] == "projects"
        && segments[4] == "artifacts-dir"
    {
        match method {
            "GET" => return crate::artifact_dirs::handle_get_project_dir(state, segments[3]),
            "POST" => {
                return crate::artifact_dirs::handle_post_project_dir(
                    state,
                    authority,
                    segments[3],
                    req,
                );
            }
            _ => {}
        }
    }
    if method == "POST"
        && segments.len() >= 6
        && segments[0] == "api"
        && segments[1] == "v1"
        && segments[2] == "projects"
    {
        let project = percent_decode(segments[3]);
        let target = match (segments.len(), segments[4], segments[5]) {
            (6, "pr", "merge") => Some(None),
            (7, "prs", number) if segments[6] == "merge" => number.parse::<u64>().ok().map(Some),
            _ => None,
        };
        if let Some(number) = target {
            let Ok(store) = open_store_response(state) else {
                return store_open_failure();
            };
            return github_pr::handle_post_pr_merge(
                state,
                &store,
                authority,
                number,
                Some(&project),
                req,
            );
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

    // /api/v1/cloud-sessions/:id/attach|handoff|status
    if segments.len() == 5
        && segments[0] == "api"
        && segments[1] == "v1"
        && segments[2] == "cloud-sessions"
        && method == "POST"
    {
        match segments[4] {
            "attach" => return handle_post_cloud_session_attach(state, authority, segments[3]),
            "handoff" => return handle_post_cloud_session_handoff(state, authority, segments[3]),
            "status" => {
                return handle_post_cloud_session_status(state, authority, segments[3], req);
            }
            _ => {}
        }
    }

    // /api/v1/roles/:slug
    if segments.len() == 4
        && segments[0] == "api"
        && segments[1] == "v1"
        && segments[2] == "roles"
        && method == "DELETE"
    {
        return handle_delete_role(state, authority, segments[3]);
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
    // Only `id` and `persona` are read, so skip model discovery.
    let sessions =
        discover_route_sessions_without_model_info(snap, state.config.home_override.as_deref());
    resolve_session_target_in(&sessions, raw_session_id)
}

/// The canonical session id and persona for `raw_session_id` among `sessions`.
pub(crate) fn resolve_session_target_in(
    sessions: &[DiscoveredSessionSummary],
    raw_session_id: &str,
) -> (String, Option<String>) {
    let trimmed = raw_session_id.trim();
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
        // A Muse route made from a live tmux window has an empty id until its first transcript;
        // it matches no session id, not even an empty one.
        fl_wire::identity::VendorRoute::Muse { session_id, .. } => {
            !session_id.is_empty()
                && (session_id == trimmed
                    || trimmed.strip_prefix("muse-") == Some(session_id.as_str()))
        }
        // A Grok route made from a live tmux window has an empty id until its first transcript;
        // it matches no session id, not even an empty one.
        fl_wire::identity::VendorRoute::Grok { session_id, .. } => {
            !session_id.is_empty()
                && (session_id == trimmed
                    || trimmed.strip_prefix("grok-") == Some(session_id.as_str()))
        }
        fl_wire::identity::VendorRoute::Webhook { .. } => false,
    }
}

/// The route lookup for a session a request resolved: `canonical_id` is the id discovery gave
/// the session (or the raw id when discovery found none), so a saved route that names another
/// vendor session is stale and the host is scanned again.
fn session_route_query<'a>(
    persona: Option<&'a str>,
    canonical_id: &'a str,
) -> crate::lifecycle::RouteQuery<'a> {
    crate::lifecycle::RouteQuery {
        persona,
        fallback_session_id: canonical_id,
        session_id: Some(canonical_id),
    }
}

fn vendor_route_for_session(
    state: &HubState,
    persona: Option<&str>,
    canonical_id: &str,
) -> Result<fl_wire::identity::VendorRoute, String> {
    crate::lifecycle::find_route(
        state.config.home_override.as_deref(),
        session_route_query(persona, canonical_id),
    )
    .ok_or_else(|| no_route_error(persona, canonical_id))
}

fn no_route_error(persona: Option<&str>, fallback_session_id: &str) -> String {
    format!(
        "no active vendor route for `{}`",
        persona.unwrap_or(fallback_session_id)
    )
}

/// Send on the session's route; a saved route that fails the pre-delivery check, or whose
/// receiver could not be reached at all, is swapped for the one a rescan finds (see
/// [`crate::lifecycle::dispatch_with_rediscovery`]). The outer `Err` is the no-route error.
fn dispatch_to_session_route<T>(
    state: &HubState,
    persona: Option<&str>,
    canonical_id: &str,
    send: impl FnMut(&fl_wire::identity::VendorRoute) -> Result<T, PushError>,
) -> Result<Result<T, String>, String> {
    crate::lifecycle::dispatch_with_rediscovery(
        state.config.home_override.as_deref(),
        session_route_query(persona, canonical_id),
        send,
    )
    .ok_or_else(|| no_route_error(persona, canonical_id))
}

#[cfg(test)]
thread_local! {
    static BEFORE_PROMPT_DISPATCH: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
    static AFTER_PROMPT_DISPATCH: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
    static AFTER_QUEUE_FINISH: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
}

#[cfg(test)]
pub(crate) fn after_next_queue_finish(f: impl FnOnce() + 'static) {
    AFTER_QUEUE_FINISH.with(|hook| *hook.borrow_mut() = Some(Box::new(f)));
}

#[cfg(test)]
thread_local! {
    static QUEUED_TEST_DELIVERY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
#[cfg(test)]
pub(crate) struct StubQueuedDelivery;
#[cfg(test)]
impl Drop for StubQueuedDelivery {
    fn drop(&mut self) {
        QUEUED_TEST_DELIVERY.with(|flag| flag.set(false));
    }
}
#[cfg(test)]
pub(crate) fn stub_queued_delivery_success() -> StubQueuedDelivery {
    QUEUED_TEST_DELIVERY.with(|flag| flag.set(true));
    StubQueuedDelivery
}

#[cfg(test)]
pub(crate) fn before_next_prompt_dispatch(f: impl FnOnce() + 'static) {
    BEFORE_PROMPT_DISPATCH.with(|hook| *hook.borrow_mut() = Some(Box::new(f)));
}

#[cfg(test)]
pub(crate) fn after_next_prompt_dispatch(f: impl FnOnce() + 'static) {
    AFTER_PROMPT_DISPATCH.with(|hook| *hook.borrow_mut() = Some(Box::new(f)));
}

/// Select capability from authenticated authority, never a display label or the queued author.
fn operator_prompt_intent(authority: &HubAuthority, send_now: bool) -> OperatorPromptIntent {
    if authority.is_owner() && send_now {
        OperatorPromptIntent::OwnerSendNow
    } else {
        OperatorPromptIntent::Ordinary
    }
}

fn dispatch_operator_prompt_with_intent_for_session(
    state: &HubState,
    persona: Option<&str>,
    session_id: &str,
    sender_name: &str,
    body: &str,
    intent: OperatorPromptIntent,
    intended_codex_thread_id: Option<&str>,
) -> Result<OperatorPromptOutcome, OperatorPromptError> {
    #[cfg(test)]
    BEFORE_PROMPT_DISPATCH.with(|hook| {
        if let Some(f) = hook.borrow_mut().take() {
            f();
        }
    });
    // Preserve typed steering failures across the ordinary transport's rediscovery boundary.
    // Every steering failure is final: mapping it to Failed prevents a second route attempt.
    let mut steering_error = None;
    let result = crate::lifecycle::dispatch_with_rediscovery_typed(
        state.config.home_override.as_deref(),
        session_route_query(persona, session_id),
        |route| {
            let delivery = if intent == OperatorPromptIntent::OwnerSendNow
                && match intended_codex_thread_id {
                    Some(intended) => !matches!(route,
                        fl_wire::identity::VendorRoute::Codex { thread_id, .. } if thread_id == intended),
                    None => matches!(route, fl_wire::identity::VendorRoute::Codex { .. })
                        && !route_matches_session_id(route, session_id),
                }
            {
                Err(OperatorPromptError::Steer(CodexSteerError::Refused {
                    stage: fl_wire::dispatcher::CodexSteerStage::Route,
                    reason: fl_wire::dispatcher::CodexSteerRefusalReason::InvalidResponse,
                    message: "resolved route does not serve the intended Codex thread".into(),
                }))
            } else {
                push_operator_prompt_with_intent(route, sender_name, body, intent)
            };
            delivery.map_err(|e| match e {
                OperatorPromptError::Push(e) => e,
                OperatorPromptError::Steer(e) => {
                    let message = e.to_string();
                    steering_error = Some(e);
                    PushError::Failed(message)
                }
            })
        },
    )
    .ok_or_else(|| {
        let message = no_route_error(persona, session_id);
        if intent == OperatorPromptIntent::OwnerSendNow && intended_codex_thread_id.is_some() {
            // Route loss cannot prove the pinned thread idle. Preserve explicit intent so
            // a queued Send now stays held instead of later falling back to ordinary input.
            OperatorPromptError::Steer(CodexSteerError::Refused {
                stage: fl_wire::dispatcher::CodexSteerStage::Route,
                reason: fl_wire::dispatcher::CodexSteerRefusalReason::InvalidResponse,
                message,
            })
        } else {
            OperatorPromptError::Push(PushError::NotConnected(message))
        }
    })?;
    #[cfg(test)]
    AFTER_PROMPT_DISPATCH.with(|hook| {
        if let Some(f) = hook.borrow_mut().take() {
            f();
        }
    });
    match steering_error {
        Some(error) => Err(OperatorPromptError::Steer(error)),
        None => result.map_err(OperatorPromptError::Push),
    }
}

fn dispatch_queued_operator_prompt_with_intent_for_session(
    state: &HubState,
    persona: Option<&str>,
    session_id: &str,
    sender_name: &str,
    body: &str,
    intent: OperatorPromptIntent,
    intended_codex_thread_id: Option<&str>,
) -> Result<OperatorPromptOutcome, OperatorPromptError> {
    #[cfg(test)]
    if QUEUED_TEST_DELIVERY.with(|flag| flag.get()) {
        AFTER_PROMPT_DISPATCH.with(|hook| {
            if let Some(f) = hook.borrow_mut().take() {
                f();
            }
        });
        return Ok(OperatorPromptOutcome::TransportAccepted);
    }
    dispatch_operator_prompt_with_intent_for_session(
        state,
        persona,
        session_id,
        sender_name,
        body,
        intent,
        intended_codex_thread_id,
    )
}

/// `delivered` remains transport compatibility only. The explicit stage identifies what
/// accepted input; even an app-server acknowledgment does not prove model consumption.
fn with_operator_delivery(
    mut data: Value,
    result: &Result<OperatorPromptOutcome, OperatorPromptError>,
) -> Value {
    let fields = match result {
        Ok(outcome) => {
            let mut fields = serde_json::to_value(outcome).unwrap();
            fields["delivered"] = json!(true);
            fields["delivery_error"] = Value::Null;
            fields
        }
        Err(error) => {
            let stage = match error {
                OperatorPromptError::Steer(CodexSteerError::Refused { .. }) => "refused",
                OperatorPromptError::Push(PushError::NotConnected(_)) => "not_connected",
                _ => "unconfirmed",
            };
            let mut fields = json!({"delivery_stage":stage, "delivered":false, "delivery_error":error.to_string()});
            if let OperatorPromptError::Steer(error) = error {
                fields["failure_stage"] = json!(format!("{:?}", error.stage()));
                fields["refusal_reason"] = error
                    .refusal_reason()
                    .map(|r| json!(format!("{r:?}")))
                    .unwrap_or(Value::Null);
            }
            fields
        }
    };
    data.as_object_mut()
        .unwrap()
        .extend(fields.as_object().unwrap().clone());
    data
}

fn dispatch_operator_prompt_for_session_typed(
    state: &HubState,
    persona: Option<&str>,
    session_id: &str,
    sender_name: &str,
    body: &str,
) -> Result<OperatorPromptOutcome, PushError> {
    dispatch_queued_operator_prompt_with_intent_for_session(
        state,
        persona,
        session_id,
        sender_name,
        body,
        OperatorPromptIntent::Ordinary,
        None,
    )
    .map_err(|e| match e {
        OperatorPromptError::Push(e) => e,
        OperatorPromptError::Steer(e) => PushError::Failed(e.to_string()),
    })
}

fn dispatch_operator_prompt_for_session(
    state: &HubState,
    persona: Option<&str>,
    fallback_session_id: &str,
    sender_name: &str,
    effective_body: &str,
) -> Result<(), String> {
    let result = dispatch_to_session_route(state, persona, fallback_session_id, |route| {
        push_operator_prompt_to_route(route, sender_name, effective_body)
    })?;
    #[cfg(test)]
    AFTER_PROMPT_DISPATCH.with(|hook| {
        if let Some(f) = hook.borrow_mut().take() {
            f();
        }
    });
    result
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
        // Keep the discovered path across the dispatch: if the file disappears before
        // the post-send observation, it is unknown rather than a new empty transcript.
        let sessions = lookup_sessions(&snap, home);
        let (canonical_id, _) = resolve_session_target_in(&sessions, &session_id);
        let mut files = TranscriptReads::default();
        let turns =
            transcript_observation_from_sessions(&canonical_id, &sessions, &snap, home, &mut files);
        let next = state
            .session_queue(&session_id)
            .first()
            .map(|item| item.id.clone());
        if !state.is_session_busy_from_observation(&canonical_id, turns.as_deref())
            && let Some(prompt_id) = next
            && let Some((item, claim)) =
                state.claim_queued_for_dispatch(&canonical_id, &session_id, &prompt_id, true)
        {
            let delivery_res = dispatch_operator_prompt_for_session_typed(
                state,
                item.persona.as_deref(),
                &canonical_id,
                &item.by,
                &item.effective_body,
            );
            let queue =
                state.finish_queued_prompt(&session_id, &item.id, delivery_res.clone().map(|_| ()));
            #[cfg(test)]
            AFTER_QUEUE_FINISH.with(|hook| {
                if let Some(f) = hook.borrow_mut().take() {
                    f();
                }
            });
            if !matches!(delivery_res, Err(PushError::NotConnected(_))) {
                let after = transcript_observation_from_sessions(
                    &canonical_id,
                    &sessions,
                    &snap,
                    home,
                    &mut files,
                );
                state.mark_session_in_flight_from_observation(&canonical_id, after.as_deref());
                if matches!(delivery_res, Err(PushError::Failed(_))) {
                    state.associate_unconfirmed_queue(&canonical_id, &session_id, &item.id);
                }
            }
            claim.finish();
            let sent_data = with_operator_delivery(
                json!({
                    "session_id": item.session_id,
                    "persona": item.persona,
                    "status": if delivery_res.is_ok() { "sent" } else { "delivery_failed" },
                    "queued": delivery_res.is_err(),
                    "is_busy": true,
                    "prompt_id": item.id,
                    "prompt": item.prompt,
                    "effective_body": item.effective_body,
                    "by": item.by,
                    "queue": queue,
                    "attachments": item.attachments,
                }),
                &delivery_res.map_err(OperatorPromptError::Push),
            );
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
    authority: &HubAuthority,
    raw_session_id: &str,
    prompt_id: &str,
) -> HttpResponse {
    let (canonical_id, target_persona) = resolve_session_target(state, raw_session_id);
    let claimed = state.claim_queued_for_dispatch(&canonical_id, raw_session_id, prompt_id, false);
    let Some((item, claim)) = claimed else {
        let exists = state
            .session_queue(&canonical_id)
            .iter()
            .any(|q| q.id == prompt_id)
            || state
                .session_queue(raw_session_id)
                .iter()
                .any(|q| q.id == prompt_id);
        return HttpResponse::json(
            if exists { 409 } else { 404 },
            json!({ "error": "queued prompt is missing, already sending, or has unconfirmed delivery" }),
        );
    };
    let queue_key = item.session_id.clone();
    let intended_codex_thread_id = item.intended_codex_thread_id.as_deref();
    let dispatch_session_id = if authority.is_owner() && intended_codex_thread_id.is_some() {
        item.session_id.as_str()
    } else {
        canonical_id.as_str()
    };
    let delivery_res = dispatch_queued_operator_prompt_with_intent_for_session(
        state,
        item.persona.as_deref().or(target_persona.as_deref()),
        dispatch_session_id,
        &item.by,
        &item.effective_body,
        operator_prompt_intent(authority, true),
        intended_codex_thread_id,
    );
    let refused = matches!(
        delivery_res,
        Err(OperatorPromptError::Steer(CodexSteerError::Refused { .. }))
    );
    let queue = if refused {
        state.hold_refused_queued_prompt(
            &queue_key,
            prompt_id,
            delivery_res.as_ref().unwrap_err().to_string(),
        )
    } else {
        let legacy = delivery_res.clone().map(|_| ()).map_err(|e| match e {
            OperatorPromptError::Push(e) => e,
            OperatorPromptError::Steer(e) => PushError::Failed(e.to_string()),
        });
        state.finish_queued_prompt(&queue_key, prompt_id, legacy)
    };
    if !refused
        && !matches!(
            delivery_res,
            Err(OperatorPromptError::Push(PushError::NotConnected(_)))
        )
    {
        if matches!(
            delivery_res,
            Err(OperatorPromptError::Steer(
                CodexSteerError::Unconfirmed { .. }
            ))
        ) {
            state.mark_session_in_flight_from_observation(&canonical_id, None);
        } else {
            state.mark_session_in_flight_released(&canonical_id);
        }
        if delivery_res.is_err() {
            state.associate_unconfirmed_queue(&canonical_id, &queue_key, prompt_id);
        }
    }
    claim.finish();

    let data = with_operator_delivery(
        json!({
            "session_id": canonical_id,
            "persona": item.persona.clone().or(target_persona.clone()),
            "status": if delivery_res.is_ok() { "sent" } else { "delivery_failed" },
            "queued": delivery_res.is_err(),
            "is_busy": true,
            "prompt_id": prompt_id,
            "prompt": item.prompt,
            "effective_body": item.effective_body,
            "by": item.by,
            "queue": queue,
            "attachments": item.attachments,
        }),
        &delivery_res,
    );
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
        /// Accepted so existing clients still parse; the hub does not store it (see
        /// `attachment_policy::stored_media_type`).
        #[serde(default, rename = "media_type", alias = "mime_type")]
        _client_media_type: String,
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

    let mut saved_attachments: Vec<SavedPromptAttachment> =
        Vec::with_capacity(payload.attachments.len());
    if !payload.attachments.is_empty() {
        use crate::attachment_policy as policy;

        // Everything is checked and decoded before the first byte is written, so a refused
        // prompt leaves nothing in the attachments directory.
        if let Err(refusal) =
            policy::check_attachment_names(payload.attachments.iter().map(|att| att.name.as_str()))
        {
            return refusal.into_response();
        }
        let mut total_bytes = 0usize;
        let mut decoded_files = Vec::with_capacity(payload.attachments.len());
        for att in &payload.attachments {
            let raw_b64 = match att.data_base64.find(";base64,") {
                Some(pos) if att.data_base64.trim_start().starts_with("data:") => {
                    &att.data_base64[pos + ";base64,".len()..]
                }
                _ => att.data_base64.as_str(),
            }
            .trim();
            if raw_b64.len() > policy::MAX_ATTACHMENT_BASE64_CHARS {
                return policy::AttachmentRefusal::FileTooLarge {
                    name: att.name.clone(),
                }
                .into_response();
            }
            let decoded = match base64::engine::general_purpose::STANDARD.decode(raw_b64) {
                Ok(b) => b,
                Err(e) => {
                    return HttpResponse::json(
                        400,
                        json!({ "error": format!("invalid base64 in attachment `{}`: {e}", att.name) }),
                    );
                }
            };
            if let Err(refusal) =
                policy::check_attachment_size(&att.name, decoded.len(), &mut total_bytes)
            {
                return refusal.into_response();
            }
            decoded_files.push(decoded);
        }

        let dir = attachments_dir(state);
        if let Err(e) = fs::create_dir_all(&dir) {
            return HttpResponse::json(
                500,
                json!({ "error": format!("failed to create attachments dir: {e}") }),
            );
        }
        let now_ms = stamp::now().unix_millis();
        let names: Vec<String> = payload
            .attachments
            .iter()
            .map(|att| policy::sanitize_upload_name(&att.name))
            .collect();
        let files: Vec<(&str, &[u8])> = names
            .iter()
            .zip(&decoded_files)
            .map(|(name, decoded)| (name.as_str(), decoded.as_slice()))
            .collect();
        let stored = match policy::save_attachment_files(&dir, now_ms, &files) {
            Ok(stored) => stored,
            Err(e) => {
                return HttpResponse::json(
                    500,
                    json!({ "error": format!("failed to write attachment: {e}") }),
                );
            }
        };
        for ((name, decoded), file) in names.into_iter().zip(&decoded_files).zip(stored) {
            // The client's `media_type` is not stored: the hub derives it from the file.
            let media_type = policy::stored_media_type(&name, decoded);
            saved_attachments.push(SavedPromptAttachment {
                name,
                media_type,
                path: file.path.display().to_string(),
                url: format!("/api/v1/attachments/{}", file.saved_name),
            });
        }
        // Expire old attachments in the background; the files just saved are younger than any
        // retention window, so the sweep never takes them.
        let _ = crate::attachments::spawn_attachment_sweep(state);
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

    let home = state.config.home_override.as_deref();
    // Most prompts target a live route. Its identity needs agent ordering but not the
    // project's entire attempt history. Preserve the full lookup for unknown targets.
    let names = state
        .open_store()
        .map(|store| snapshot_discovery_names(&store))
        .unwrap_or_default();
    let fast = lookup_one_route_session(&names, home, session_id);
    let (snap, sessions) = if let Some(session) = fast {
        (names, vec![session])
    } else {
        let snap = load_discovery_snapshot(state);
        let sessions = lookup_sessions(&snap, home);
        (snap, sessions)
    };
    let (canonical_session_id, target_persona) = resolve_session_target_in(&sessions, session_id);
    let intended_codex_thread_id = match sessions
        .iter()
        .find(|session| session.id == canonical_session_id)
    {
        Some(session) if session.adapter == "codex" => Some(session.id.clone()),
        Some(_) => None,
        None => session_id.trim().strip_prefix("codex-").map(str::to_string),
    };
    let mut files = TranscriptReads::default();
    // `send_now` dispatches busy or not, so it skips the transcript parse. The check's one side
    // effect, dropping a finished in-flight marker, is overwritten by the mark after dispatch.
    let busy = !payload.send_now && {
        let turns = transcript_observation_from_sessions(
            &canonical_session_id,
            &sessions,
            &snap,
            home,
            &mut files,
        );
        state.is_session_busy_from_observation(&canonical_session_id, turns.as_deref())
    };

    let claim = match state.admit_direct_prompt(
        &canonical_session_id,
        session_id,
        payload.send_now,
        busy,
        crate::DirectPromptInput {
            intended_codex_thread_id: intended_codex_thread_id.clone(),
            persona: target_persona.clone(),
            prompt: prompt_text.clone(),
            effective_body: effective_body.clone(),
            by: sender_name.clone(),
            attachments: saved_attachments.clone(),
        },
    ) {
        crate::DirectPromptDecision::Dispatch(claim) => claim,
        crate::DirectPromptDecision::DispatchBusy => {
            return HttpResponse::json(
                409,
                json!({ "error": "another prompt is being dispatched to this session" }),
            );
        }
        crate::DirectPromptDecision::Queued(item, queue) => {
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
    };

    let delivery_res = dispatch_operator_prompt_with_intent_for_session(
        state,
        target_persona.as_deref(),
        &canonical_session_id,
        &sender_name,
        &effective_body,
        operator_prompt_intent(authority, payload.send_now),
        intended_codex_thread_id.as_deref(),
    );
    match &delivery_res {
        Err(OperatorPromptError::Steer(CodexSteerError::Refused { .. })) => {}
        Ok(OperatorPromptOutcome::AppServerAccepted { .. })
        | Err(OperatorPromptError::Steer(CodexSteerError::Unconfirmed { .. })) => {
            // Steering uses app-server truth; never infer its active turn from a rollout.
            state.mark_session_in_flight_from_observation(&canonical_session_id, None);
        }
        _ => {
            let after = transcript_observation_from_sessions(
                &canonical_session_id,
                &sessions,
                &snap,
                home,
                &mut files,
            );
            state.mark_session_in_flight_from_observation(&canonical_session_id, after.as_deref());
        }
    }
    claim.finish();
    let queue = state.session_queue(&canonical_session_id);

    let data = with_operator_delivery(
        json!({
            "session_id": canonical_session_id,
            "persona": target_persona,
            "status": if matches!(delivery_res, Err(OperatorPromptError::Steer(_))) { "delivery_failed" } else { "sent" },
            "queued": false,
            "is_busy": true,
            "in_flight_unconfirmed": state.session_in_flight_unconfirmed(&canonical_session_id),
            "prompt": prompt_text,
            "effective_body": effective_body,
            "by": sender_name,
            "queue": queue,
            "attachments": saved_attachments,
        }),
        &delivery_res,
    );
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

/// `POST /api/v1/sessions/:id/clear` (mode `clear` or `reforge`): drop the session's queued
/// prompts and model override and reset its live route; `reforge` then sends the synthesized
/// startup prompt. Owner only (403 for a Peer, checked before the body is read and before any
/// side effect), for both modes: it erases another actor's queue and injects a prompt into a
/// live vendor conversation.
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

    if !authority.is_owner() {
        return HttpResponse::json(
            403,
            json!({ "error": "only Owner authority may clear or reforge a session" }),
        );
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

/// Clear or reforge one session: prepare the reforge prompt, then drop queued prompts and model
/// overrides and reset the live vendor route. `mode` is `"clear"` or `"reforge"`. `raw_session_id`
/// is the id the caller used (its queue is dropped too); `canonical_id` and `persona` come from
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
    // Fallible guidance preparation must not erase queues or model choices on refusal.
    let prompt = if mode == "reforge" {
        let lookup = persona.unwrap_or(canonical_id);
        match build_reforge_prompt(state, lookup) {
            Ok(prompt) => Some(prompt),
            Err(error) => return Err((error.status(), error.to_string())),
        }
    } else {
        None
    };

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

    let reset_mode = if mode == "reforge" {
        SessionResetMode::Reforge
    } else {
        SessionResetMode::Clear
    };
    // A reset runs once: after its `/clear` or `/new` lands the session has changed, so a reset
    // that then fails is never run again on another route.
    let reset = crate::lifecycle::dispatch_once(
        state.config.home_override.as_deref(),
        session_route_query(persona, canonical_id),
        |route| clear_or_reforge_session(route, reset_mode, actor, prompt.as_deref()),
    )
    .ok_or_else(|| no_route_error(persona, canonical_id))
    .map_err(|e| (404, e))?;
    // A clear can start a new vendor session (a new id) behind the same persona.
    crate::discovery::invalidate_route_discovery_cache();
    reset.map_err(|e| (502, e))?;

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
/// Owner only (403 for a Peer, checked before anything else): it cancels another actor's
/// work. 404 when the session has no live route; 502 when the
/// interrupt cannot be delivered. See [`session_actions::interrupt_route`] for the per-vendor
/// mechanism and the test-mode no-op.
fn handle_post_session_interrupt(
    state: &HubState,
    authority: &HubAuthority,
    session_id: &str,
    req: &HttpRequest,
) -> HttpResponse {
    if !authority.is_owner() {
        return HttpResponse::json(
            403,
            json!({ "error": "only Owner authority may interrupt a session" }),
        );
    }
    let reason = match optional_reason(req) {
        Ok(reason) => reason,
        Err(resp) => return resp,
    };
    let (canonical_id, persona) = resolve_session_target(state, session_id);
    // Claude and Codex routes carry no tmux target; a hosted session's record names its pane.
    // A Claude session the hub does not host falls back to the live pane its key file names.
    let hosted_pane = hosted_record_for(state, &[session_id, &canonical_id], persona.as_deref())
        .and_then(|record| record.tmux_target);
    let env = lifecycle_env(state);
    let interrupted =
        dispatch_to_session_route(state, persona.as_deref(), &canonical_id, |route| {
            let pane = hosted_pane
                .clone()
                .or_else(|| fl_wire::dispatcher::claude_route_live_pane(route));
            session_actions::interrupt_route(&env, route, pane.as_deref())
                .map(|method| (method, route_vendor(route)))
                .map_err(PushError::Failed)
        });
    let (method, vendor) = match interrupted {
        Ok(Ok(done)) => done,
        Ok(Err(e)) => return HttpResponse::json(502, json!({ "error": e })),
        Err(e) => return HttpResponse::json(404, json!({ "error": e })),
    };
    // Whatever prompt was in flight is no longer running, so the session stops reading as busy.
    // Queued prompts stay queued and drain as usual once the session is idle.
    state.clear_session_in_flight(&canonical_id);
    if session_id != canonical_id {
        state.clear_session_in_flight(session_id);
    }
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
/// Owner only (403 for a Peer, checked before the body is read). The
/// choice is kept in `HubState` and overlaid on `GET /api/v1/sessions` (in memory: gone after
/// a hub restart), written to the hosted-session record when one exists, and sent to a live
/// CLI as `/model <id>`.
///
/// Antigravity is not sent anything. A running Antigravity session keeps its model: FerroDeck
/// shows the choice, and the stored effort-suffixed id (`cli_model` in the response) applies
/// when the session is next launched (restart or reforge). The response keeps `switched: true`
/// and `method: override-only` for compatibility.
fn handle_post_session_model(
    state: &HubState,
    authority: &HubAuthority,
    session_id: &str,
    req: &HttpRequest,
) -> HttpResponse {
    if !authority.is_owner() {
        return HttpResponse::json(
            403,
            json!({ "error": "only Owner authority may change a session's model" }),
        );
    }
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
        ModelOverride::new(model.clone(), thinking_level.clone()),
    );
    // `agy --model` only takes effort-suffixed ids, so an Antigravity record keeps the mapped id
    // (a restart launches with it); every other vendor keeps the model as the operator chose it.
    let cli_model = (vendor == "antigravity")
        .then(|| lifecycle::agy_cli_model_id(&model, thinking_level.as_deref()));
    if hosted.is_some() {
        match open_store_response(state) {
            Ok(store) => {
                // Best effort: the override above already answers `GET /api/v1/sessions`.
                let _ = lifecycle::set_hosted_session_model(
                    &*store,
                    &[trimmed, &canonical_id],
                    cli_model.as_deref().unwrap_or(&model),
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
                &session_actions::model_command(&model),
            ) {
                Ok(()) => METHOD_OPERATOR_PROMPT,
                Err(_) => METHOD_OVERRIDE_ONLY,
            }
        }
        _ => METHOD_OVERRIDE_ONLY,
    };

    let mut data = json!({
        "ok": true,
        "session_id": canonical_id,
        "vendor": vendor,
        "model": model,
        "thinking_level": thinking_level,
        "switched": true,
        "method": method,
    });
    if let Some(cli_model) = cli_model {
        data["cli_model"] = json!(cli_model);
    }
    state.broadcast("sessions", "session.model_changed", data.clone());
    HttpResponse::json(200, data)
}

/// The 409 text when a window reached neither the record nor the pane: `had_record` tells a
/// session with no record to keep it on from a record whose write did not land.
pub(crate) fn context_window_not_applied(had_record: bool) -> &'static str {
    if had_record {
        "the context window was not applied: writing it to the session's hosted record failed and its Grok pane is not live"
    } else {
        "the context window was not applied: the session has no hosted record to keep it on and its Grok pane is not live"
    }
}

/// `POST /api/v1/sessions/:id/context-window` (Local Record #134): choose a session's context
/// window, `{ "context_window": <positive integer> | null }`. Owner only (403 for a Peer,
/// checked before the body is read). `null` clears the choice.
///
/// A number is checked with [`crate::context_window::validate_context_window`] against the
/// session's vendor and model; only Grok is selectable, and a refusal is a 400 that names the
/// sizes on offer. The model is the session's own live model (read from its Grok session file,
/// ignoring a config default), else the operator's latest choice, else the hosted record's;
/// with none the answer is a 400 "name a model". The value is stored on the session's live
/// hosted record of the same vendor when it has one (a session without one stores nothing and
/// the response says `persisted: false`). A Grok route whose tmux pane is live is also pasted
/// `/context-window <tokens>` (`method: operator-prompt`); a pane that is not live is never
/// reached through a headless `grok -p` (a separate process that is not the running TUI), so the
/// window is only stored. `null` sends nothing, because Grok has no reset. Any other route, or a
/// failed send, reports `method: stored`: the window is saved on the record. Applying a stored
/// window at launch is separate work.
///
/// The response reports what was REQUESTED. The effective window is what the vendor reports
/// (`context_usage` on the session), which can differ if the vendor ignored the command.
fn handle_post_session_context_window(
    state: &HubState,
    authority: &HubAuthority,
    session_id: &str,
    req: &HttpRequest,
) -> HttpResponse {
    if !authority.is_owner() {
        return HttpResponse::json(
            403,
            json!({ "error": "only Owner authority may change a session's context window" }),
        );
    }
    let body = match json_object_body(req) {
        Ok(body) => body,
        Err(resp) => return resp,
    };
    let requested: Option<u64> = match body.get("context_window") {
        Some(Value::Null) => None,
        Some(value) => match value.as_u64().filter(|&n| n > 0) {
            Some(tokens) => Some(tokens),
            None => {
                return HttpResponse::json(
                    400,
                    json!({
                        "error": "`context_window` must be a positive integer number of tokens, or null"
                    }),
                );
            }
        },
        None => {
            return HttpResponse::json(
                400,
                json!({
                    "error": "`context_window` is required: a positive integer number of tokens, or null"
                }),
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
    let any_record = hosted_record_for(state, &[trimmed, &canonical_id], persona.as_deref());
    if matched.is_none() && route.is_none() && any_record.is_none() {
        return HttpResponse::json(
            404,
            json!({ "error": format!("unknown session `{trimmed}`") }),
        );
    }
    let vendor = match (&route, matched, &any_record) {
        (Some(route), _, _) => route_vendor(route).to_string(),
        (None, Some(s), _) => s.adapter.clone(),
        (None, None, Some(record)) => record.vendor.clone(),
        (None, None, None) => "unknown".to_string(),
    };
    // Only a live record of the session's own vendor takes (or informs) a window: a persona can
    // hold records of other vendors, and a decommissioned record is history.
    let hosted = any_record.filter(|record| {
        record.vendor == vendor && record.status != lifecycle::STATUS_DECOMMISSIONED
    });

    if let Some(tokens) = requested {
        // The model whose sizes decide, most trustworthy first: what the session itself reports
        // (Grok only, the only vendor whose sizes are checked; a config default does not count),
        // the operator's latest choice, then the hosted record's. The override and the record can
        // be older than an in-CLI `/model`, so they only fill in when the session reports none.
        let override_key = match matched {
            Some(s) => s.id.clone(),
            None => persona.clone().unwrap_or_else(|| canonical_id.clone()),
        };
        let live_model = (vendor == "grok")
            .then(|| {
                crate::discovery::grok_live_model(
                    matched.and_then(|s| s.transcript_path.as_deref()),
                    state.config.home_override.as_deref(),
                )
            })
            .flatten();
        let model = live_model
            .or_else(|| state.session_model_override(&override_key).map(|o| o.model))
            .or_else(|| hosted.as_ref().and_then(|r| r.model.clone()));
        if let Err(why) = crate::context_window::validate_context_window(
            &vendor,
            model.as_deref(),
            tokens,
            state.config.home_override.as_deref(),
        ) {
            return HttpResponse::json(400, json!({ "error": why }));
        }
    }

    let mut persisted = false;
    if let Some(record) = &hosted {
        match open_store_response(state) {
            Ok(store) => {
                // Best effort, like the model: a failed write reports `persisted: false`. The
                // record found above is written by its own id, so a request that named the
                // session by its vendor id (not its persona) still reaches it.
                persisted = matches!(
                    lifecycle::set_hosted_session_context_window(
                        &*store,
                        &[record.session_id.as_str()],
                        requested,
                        authority.actor_name(),
                    ),
                    Ok(Some(_))
                );
            }
            Err(()) => return store_open_failure(),
        }
    }

    // Only a live pane takes the command: the dispatcher's own Grok path would fall back to a
    // headless `grok -p` for a dead pane, which is not the running TUI.
    let live = match (&route, requested) {
        (Some(route @ fl_wire::identity::VendorRoute::Grok { .. }), Some(tokens)) => {
            deliver_to_live_grok_pane(route, &session_actions::context_window_command(tokens))
                .is_ok()
        }
        _ => false,
    };
    let method = if live {
        METHOD_OPERATOR_PROMPT
    } else {
        METHOD_STORED
    };
    if requested.is_some() && !live && !persisted {
        return HttpResponse::json(
            409,
            json!({ "error": context_window_not_applied(hosted.is_some()) }),
        );
    }

    let data = json!({
        "ok": true,
        "session_id": canonical_id,
        "vendor": vendor,
        "context_window": requested,
        "persisted": persisted,
        "method": method,
    });
    state.broadcast("sessions", "session.context_window_changed", data.clone());
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
/// and workflows. The workspace is the hub's `repo_root`; skills also come from the workspace
/// parent and the user's home (`home_override`, else `HOME`).
fn handle_get_commands(state: &HubState) -> HttpResponse {
    let home = crate::commands::resolve_home(state.config.home_override.as_deref());
    let commands =
        crate::commands::list_commands_with_home(&state.config.repo_root, home.as_deref());
    HttpResponse::json(200, json!({ "commands": commands }))
}

fn lifecycle_error_response(err: &LifecycleError) -> HttpResponse {
    HttpResponse::json(err.status(), json!({ "error": err.to_string() }))
}

pub(crate) fn lifecycle_env(state: &HubState) -> LifecycleEnv<'_> {
    let mut env = LifecycleEnv::new(state.config.home_override.as_deref());
    env.workspace_root = state.config.repo_root.parent();
    env
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
    // Choosing a window is an Owner decision, like the hub-wide defaults.
    if payload.context_window.is_some() && !authority.is_owner() {
        return HttpResponse::json(
            403,
            json!({ "error": "only Owner authority may choose a session's context window" }),
        );
    }
    let Ok(store) = open_store_response(state) else {
        return store_open_failure();
    };
    // A virtual scope has no directory of its own: its agents start in the workspace root.
    let mut payload =
        payload.with_default_cwd(&*store, &resolve_workspace_root(&state.config.repo_root));
    if payload.context_window.is_none() {
        payload.context_window = default_context_window_for(state, &payload);
    }
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

/// The hub-wide context window default for the vendor of `req`, when it suits the requested
/// model; `None` when there is no default or it is stale (never an error).
fn default_context_window_for(state: &HubState, req: &SpinUpRequest) -> Option<u64> {
    let home = hub_home_dir(state)?;
    let defaults = settings::load(&home).context_window_defaults;
    let vendor = req.vendor.trim().to_ascii_lowercase();
    let model = req
        .model
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty());
    crate::context_window::applicable_default(&defaults, &vendor, model, Some(&home))
}

/// `POST /api/v1/sessions/:id/decommission` (`prepare` or `teardown`, see
/// [`lifecycle::prepare_decommission`] and [`lifecycle::teardown_session`]). Owner only: 403 for
/// a Peer, checked before the body is read and before any discovery, store access or side effect.
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

    if !authority.is_owner() {
        return HttpResponse::json(
            403,
            json!({ "error": "only Owner authority may decommission a session" }),
        );
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

/// `POST /api/v1/sessions/:id/remodel` (Local Record 59): move a session to another vendor,
/// keeping its persona, project, role and worktree (see [`lifecycle::remodel_session`]).
/// `prepare` asks the session to hand off (queued when no live route takes it); `execute`
/// replaces it. The store guard is held from target resolution to the end of the handler, past
/// every queue, model-override and broadcast write, so no other lifecycle request of this hub
/// runs in between and two remodels of one persona never interleave their writes.
///
/// A failed `execute` answers 502 and broadcasts `session.remodel_failed` with its outcome
/// (`restored` or `down`). When the old session was restored, the operator's model override
/// (and thinking level) the teardown cleared is put back as it was (see [`run_remodel`]).
///
/// Owner only: 403 for a Peer, checked before the body is read and before any discovery, store
/// access or side effect.
fn handle_post_session_remodel(
    state: &HubState,
    authority: &HubAuthority,
    session_id: &str,
    req: &HttpRequest,
) -> HttpResponse {
    if !authority.is_owner() {
        return HttpResponse::json(
            403,
            json!({ "error": "only Owner authority may remodel a session" }),
        );
    }
    let payload: RemodelRequest = match req.json() {
        Ok(payload) => payload,
        Err(e) => return HttpResponse::json(400, json!({ "error": e })),
    };
    // Resolve before taking the store guard: discovery opens the store itself.
    let discovered = resolve_session_target(state, session_id);
    let by = authority.actor_name();
    let Ok(store) = open_store_response(state) else {
        return store_open_failure();
    };
    let target = match SessionTarget::resolve(&*store, session_id, Some(discovered)) {
        Ok(target) => target,
        Err(e) => return lifecycle_error_response(&e),
    };
    let resp = match run_remodel(state, &store, &target, &payload, by, caller_of(authority)) {
        RemodelRun::Done { data, .. } => HttpResponse::json(200, data),
        RemodelRun::Failed { data, .. } => HttpResponse::json(502, data),
        RemodelRun::Refused(e) => lifecycle_error_response(&e),
    };
    // Released only now: every write [`run_remodel`] made belongs to this remodel.
    drop(store);
    resp
}

/// How [`run_remodel`] ended.
pub(crate) enum RemodelRun {
    /// The remodel ran; `data` is its response, broadcast as `session.remodeled`.
    Done {
        out: Box<lifecycle::RemodelResponse>,
        data: Value,
    },
    /// It failed after touching the session (502); `data` was broadcast as
    /// `session.remodel_failed`. `restored_session_id` is `None` when the persona is down.
    Failed {
        data: Value,
        message: String,
        restored_session_id: Option<String>,
    },
    /// Refused before touching anything.
    Refused(LifecycleError),
}

/// Run one remodel of `target` with every hub-side effect the remodel endpoint has: the
/// teardown's queue and model-override clearing (and putting the overrides back when a failed
/// remodel restored the old session), the route-scan drop, the new model override, the
/// handshake queueing for `prepare`, and the `session.remodeled` / `session.remodel_failed`
/// broadcast. `store` is the caller's [`HubStoreGuard`]: hold it from target resolution until
/// after this returns (see [`lifecycle::remodel_session`] on concurrency).
pub(crate) fn run_remodel(
    state: &HubState,
    store: &AnyStore,
    target: &SessionTarget,
    payload: &RemodelRequest,
    by: &str,
    caller: lifecycle::Caller,
) -> RemodelRun {
    let env = lifecycle_env(state);
    // The overrides the teardown clears, as they were, so a restore can put them back.
    let mut cleared_overrides: Vec<(String, ModelOverride)> = Vec::new();
    let result = lifecycle::remodel_session(store, &env, target, payload, by, caller, |key| {
        for k in [key.to_string(), key.to_ascii_lowercase()] {
            if let Some(o) = state.session_model_override(&k)
                && !cleared_overrides.iter().any(|(seen, _)| *seen == k)
            {
                cleared_overrides.push((k, o));
            }
        }
        state.clear_session_prompt_queue(key);
        state.clear_session_model_override(key);
    });
    // Teardown and spin-up dropped the route scan; the store snapshot is dropped for every
    // write request. Drop the scan here too, so a failed remodel's restore shows at once.
    crate::discovery::invalidate_route_discovery_cache();
    let out = match result {
        Ok(out) => out,
        Err(LifecycleError::Partial {
            message,
            restored_session_id,
        }) => {
            if restored_session_id.is_some() {
                for (key, o) in cleared_overrides {
                    state.set_session_model_override(&key, o);
                }
            }
            let data = json!({
                "session_id": target.session_id,
                "persona": target.persona,
                "vendor": payload.vendor.trim().to_ascii_lowercase(),
                "outcome": if restored_session_id.is_some() { "restored" } else { "down" },
                "restored_session_id": restored_session_id,
                "error": message,
            });
            state.broadcast("sessions", "session.remodel_failed", data.clone());
            return RemodelRun::Failed {
                data,
                message,
                restored_session_id,
            };
        }
        Err(e) => return RemodelRun::Refused(e),
    };
    match out.mode {
        RemodelMode::Prepare => {
            if let (false, Some(prompt)) = (out.handshake_dispatched, out.prompt.as_ref()) {
                // No live route took the handshake: leave it on the queue for the next idle turn.
                let (_, queue) = state.enqueue_session_prompt(
                    &out.session_id,
                    Some(out.persona.clone()),
                    prompt.clone(),
                    prompt.clone(),
                    by.to_string(),
                    Vec::new(),
                );
                state.broadcast(
                    "session",
                    "queue_updated",
                    json!({
                        "session_id": out.session_id,
                        "persona": out.persona,
                        "is_busy": true,
                        "queue": queue,
                    }),
                );
            }
        }
        RemodelMode::Execute => {
            // Nothing queued for, or chosen for, the old run applies to the new one: the old
            // vendor's model override must never mask the new vendor's model.
            for key in [&out.previous_session_id, &out.session_id, &out.persona] {
                state.clear_session_prompt_queue(key);
                state.clear_session_model_override(key);
            }
            if let Some(model) = &out.model {
                // Keyed by the persona (and the hosted id): discovery does not know the new
                // vendor session's id yet, and `apply_model_overrides` matches the persona.
                for key in [out.persona.to_ascii_lowercase(), out.session_id.clone()] {
                    state.set_session_model_override(&key, ModelOverride::new(model.clone(), None));
                }
            }
        }
    }
    let data = serde_json::to_value(&out).unwrap_or_default();
    state.broadcast("sessions", "session.remodeled", data.clone());
    RemodelRun::Done {
        out: Box::new(out),
        data,
    }
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

fn build_reforge_prompt(state: &HubState, persona: &str) -> Result<String, LifecycleError> {
    let store = state
        .open_store()
        .map_err(|e| LifecycleError::Store(e.to_string()))?;
    let mut agents: Vec<AgentRecord> = store
        .list_agents(None, true)?
        .into_iter()
        .filter(|agent| {
            agent.name_key.eq_ignore_ascii_case(persona.trim())
                || agent.display_name.eq_ignore_ascii_case(persona.trim())
        })
        .collect();
    agents.sort_by_key(|agent| agent.status != AgentStatus::Active);
    let agent = agents.into_iter().next();
    if let Some(agent) = &agent {
        let required = lifecycle::recast_guidance_required(&*store, agent)?;
        let catalog_custom = match &agent.role {
            AgentRole::Custom(slug) => lifecycle::has_catalog_guidance(&*store, slug)?,
            _ => false,
        };
        if required || catalog_custom {
            return lifecycle::registered_role_prompt(
                &*store,
                &agent.display_name,
                &agent.project,
                &agent.role,
            );
        }
    }
    drop(store);
    if let Some(prompt) = reforge_prompt_from_hydrate(state, persona)
        && !prompt.trim().is_empty()
    {
        return Ok(prompt);
    }
    let agent = agent.ok_or_else(|| {
        LifecycleError::NotFound(format!("no agent record for reforge of `{persona}`"))
    })?;
    Ok(canonical_reforge_prompt(
        &agent.display_name,
        &agent.project,
        &reforge_role_label(&agent.role),
    ))
}

pub(crate) fn reforge_role_label(role: &AgentRole) -> String {
    match role {
        AgentRole::Lead => "lead".to_string(),
        AgentRole::Assistant => "assistant".to_string(),
        AgentRole::LabManager => "lab_manager".to_string(),
        AgentRole::UiDeveloper => "ui-developer".to_string(),
        AgentRole::WebDeveloper => "web-developer".to_string(),
        AgentRole::MobileDeveloper => "mobile-developer".to_string(),
        AgentRole::SystemDeveloper => "system-developer".to_string(),
        AgentRole::DataEngineer => "data-engineer".to_string(),
        AgentRole::Tester => "tester".to_string(),
        AgentRole::Strategist => "strategist".to_string(),
        AgentRole::Journalist => "journalist".to_string(),
        // A custom slug that spells a reserved built-in keeps its `custom:` prefix, so
        // `agent_role_from_label` does not read the label back as the built-in role.
        AgentRole::Custom(slug) => match slug.as_str() {
            "lead" | "assistant" | "lab_manager" | "lab-manager" => format!("custom:{slug}"),
            _ => slug.clone(),
        },
    }
}

/// The FerroState scope token (`project:<slug>`) that names `project`, for the `--scope` flag of
/// `fl state`. The slug comes from [`derive_import_slug`], the same slugger the checkpoint scope
/// uses. A project name that cannot be slugged falls back to `workspace`, which always parses.
pub(crate) fn project_state_scope(project: &str) -> String {
    derive_import_slug(project)
        .map(|slug| format!("project:{slug}"))
        .filter(|token| StateScope::from_wire(token).is_ok())
        .unwrap_or_else(|| "workspace".to_string())
}

pub(crate) fn canonical_reforge_prompt(name: &str, project: &str, role: &str) -> String {
    let name_lower = name.to_ascii_lowercase();
    // The handoff key is the lowercase persona. It relies on the upstream persona validators
    // (`is_valid_persona` / `validate_agent_identity`), which always yield a valid state slug.
    // Known limitation: a persona named `current` or `checkpoint-<x>` would collide with the shared
    // handoff key or the checkpoint entries; those names are not reserved here.
    let scope = project_state_scope(project);
    let render_scopes = if scope == "workspace" {
        "--scope workspace".to_string()
    } else {
        format!("--scope workspace --scope {scope}")
    };
    format!(
        "Take the name of {name} and take up residence in {project}/ as the {role}.\n\
         \n\
         This session has just been reset (`/clear`) for clean-slate context hygiene. Perform your root startup hydration now:\n\
         1. Read `.agents/AGENTS.md` and `.agents/REGISTRY.md`.\n\
         2. Read your role file(s): `.agents/roles/{role}.md`.\n\
         3. Read your project instruction files: `{project}/AGENTS.md`, `{project}/WORKFLOW.md`, `{project}/PERSONA.md`.\n\
         4. Read your working state from FerroState, the primary state source: run `fl state render {render_scopes}` for the startup summary, `fl state handoff get --scope {scope} --key {name_lower}` for your own latest handoff (if it reports none, `fl state handoff get --scope {scope}` reads the shared `current` handoff), and `fl state search \"<query>\"` to look up anything specific.\n\
         5. Optional legacy reference: if `Notes/{project}/STATE.md` exists, skim it; FerroState wins wherever the two disagree. Do not require it.\n\
         6. Re-register your active session route so NATS and FerroDeck track this fresh session:\n\
            ```bash\n\
            AGENT_PERSONA={name_lower} agent-msg register {name} --project {project} --role {role} --replace\n\
            ```\n\
         7. Check and acknowledge any pending NATS messages:\n\
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
    // The handoff key is the lowercase persona. It relies on the upstream persona validators
    // (`is_valid_persona` / `validate_agent_identity`), which always yield a valid state slug.
    // Known limitation: a persona named `current` or `checkpoint-<x>` would collide with the shared
    // handoff key or the checkpoint entries; those names are not reserved here.
    let scope = project_state_scope(project);
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
         1. Save unsaved progress, findings and your handoff in FerroState, the primary state source: `fl state handoff set --scope {scope} --key {name_lower} --summary \"<one line, at most 240 characters>\" --by {name_lower}` for the handoff (`--key {name_lower}` keeps it apart from other agents' handoffs in the project; the summary must be ONE line of at most 240 characters; put the detail in repeatable `--shipped`, `--in-flight`, `--blocker`, `--next` flags or in `--notes`), then `fl state checkpoint --scope {scope} --by {name_lower}` for the git checkpoint. `Notes/{project}/STATE.md` is an optional legacy note; update it only if it exists.\n\
         2. Commit and push your work, or clean up your temp worktrees and branches (`git worktree remove`, `git branch -d`). Leave nothing only on this session's disk.\n\
         3. Send your final handoff to {recipient} once:\n\
            ```bash\n\
            fl wire send {to} \"{name} decommission handoff\" \"<what is done, open, and where it lives>\" --from {name_lower}\n\
            ```\n\
         4. Acknowledge the remaining inbox messages:\n\
            ```bash\n\
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

/// Variables `agent-hydrate` (a `uv run` script) reads beyond the baseline: where `uv` keeps its
/// cache, its interpreters and its project environment. Paths, not secrets.
const HYDRATE_ENV_PASS: [&str; 4] = [
    "UV_CACHE_DIR",
    "UV_PYTHON_INSTALL_DIR",
    "UV_PYTHON",
    "UV_PROJECT_ENVIRONMENT",
];

/// The environment of the hydrate helper: the baseline (`PATH`, `HOME`, `XDG_*`, `AGENT_PERSONA`)
/// and [`HYDRATE_ENV_PASS`], with no vendor credential, no `FL_*` and no GitHub token.
fn hydrate_env() -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    let pass: Vec<String> = HYDRATE_ENV_PASS.iter().map(|n| n.to_string()).collect();
    fl_exec::child_env::helper_env(&pass, &[])
}

pub(crate) fn read_command_stdout(bin: &Path, args: &[&str], timeout: Duration) -> Option<String> {
    let mut child = Command::new(bin)
        .args(args)
        .env_clear()
        .envs(hydrate_env())
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

fn handle_post_workflow_step(
    state: &HubState,
    store: &AnyStore,
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
        env_pass: Vec::new(),
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
    store: &AnyStore,
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
    store: &AnyStore,
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
    store: &AnyStore,
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
    #[serde(deny_unknown_fields)]
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
        ..Default::default()
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

    // Raw proxy: no connector tool is named, so no response contract
    // applies — Uncontracted by definition (Record #96). It is still a
    // governed dispatch: admit first, reserving the worst-case
    // `LEGACY_MAX_RESPONSE_BYTES` (settlement is capped at the reserve),
    // settle on execution.
    // Project binding: `project` is not a ProxyRequest field, so the raw
    // surface binds the endpoint host as its project scope label.
    let governor = match hub_gate_governor(state) {
        Ok(g) => g,
        Err(()) => return hub_admission_unavailable(),
    };
    let (host, path) = match fl_gate::parse_url_scheme_host_port(&proxy_req.url) {
        Ok((_, h, _)) => {
            let after = proxy_req
                .url
                .split_once("://")
                .map(|(_, rest)| rest)
                .unwrap_or("");
            let p = after
                .find('/')
                .map(|i| {
                    after[i..]
                        .split(['?', '#'])
                        .next()
                        .unwrap_or("/")
                        .to_string()
                })
                .unwrap_or_else(|| "/".to_string());
            (h, p)
        }
        Err(_) => (String::new(), proxy_req.url.clone()),
    };
    // Canonicalise against discovered connector aliases so a raw call to
    // a connector endpoint shares that endpoint's budget scopes.
    let connectors = discover_connectors(
        Some(&state.config.repo_root),
        state.config.home_override.as_deref(),
    )
    .unwrap_or_default();
    let aliases = fl_gate::connector_endpoint_aliases(&connectors);
    let endpoint =
        fl_core::governor::resolve_canonical_endpoint(&aliases, None, None, &host, &path);
    let reserved = fl_gate::LEGACY_MAX_RESPONSE_BYTES;
    let mut adm_req = fl_core::governor::AdmissionRequest::new(
        hub_principal(authority),
        &host,
        &endpoint,
        reserved,
    );
    adm_req.caller_agent = proxy_req.caller_agent.clone().unwrap_or_default();
    let vault_ref = &vault;
    let proxy_ref = &proxy_req;
    let dispatch_now = hub_now_ms();
    match governor.governed_dispatch(&adm_req, dispatch_now, |permit| {
        // Shared helper re-checks the deadline with a fresh now after prepare.
        match execute_proxied_request_with_provider_and_contract_phased_with_deadline(
            vault_ref,
            proxy_ref,
            None,
            None,
            Some(permit.deadline_mono_ms),
            &hub_now_ms,
        ) {
            Ok(resp) => {
                let actual = resp.body.len() as u64;
                fl_gate::DispatchResult::Executed(resp, actual)
            }
            Err(fl_gate::DispatchPhaseError::BeforeSend(e)) => {
                fl_gate::DispatchResult::RefusedBeforeDispatch(e)
            }
            Err(fl_gate::DispatchPhaseError::AfterSend(e)) => {
                fl_gate::DispatchResult::FailedAfterSend(e)
            }
        }
    }) {
        Ok((resp, _permit)) => {
            HttpResponse::json(200, json!({ "response": resp, "outcome": "uncontracted" }))
        }
        Err(fl_gate::GovernedOrGateError::Refused(
            fl_core::governor::AdmissionOutcome::AdmissionUnavailable,
        )) => hub_admission_unavailable(),
        Err(fl_gate::GovernedOrGateError::Refused(
            fl_core::governor::AdmissionOutcome::Deferred {
                reason,
                retry_after_ms,
            },
        )) => HttpResponse::json(
            429,
            json!({ "error": "deferred", "reason": reason, "retry_after_ms": retry_after_ms }),
        ),
        Err(fl_gate::GovernedOrGateError::Refused(
            fl_core::governor::AdmissionOutcome::Denied { reason },
        )) => HttpResponse::json(403, json!({ "error": "denied", "reason": reason })),
        Err(fl_gate::GovernedOrGateError::Refused(_)) => hub_admission_unavailable(),
        Err(fl_gate::GovernedOrGateError::Gate(e)) => {
            HttpResponse::json(403, json!({ "error": e.to_string() }))
        }
    }
}

/// Build a GateGovernor over the hub's ONE shared persistent governor
/// store (lazily built once on `HubState`, then reused by every governed
/// hub path). Failure is an error, never a legacy ungoverned fallback:
/// governed surfaces answer 503 `admission_unavailable`. Uses
/// `GovernorPolicy::default()` (finite `DEFAULT_BUDGET_UNITS` budget)
/// rather than a locally inflated cap.
fn hub_gate_governor(state: &HubState) -> Result<std::sync::Arc<GateGovernor>, ()> {
    let persistent = state.gate_governor_store.get_or_try_init(|| {
        let store = fl_store::open_store(&state.config.store_path).map_err(|_| ())?;
        Ok(std::sync::Arc::new(
            fl_store::PersistentGovernorStore::with_policy(
                store,
                fl_core::governor::GovernorPolicy::default(),
            ),
        ))
    })?;
    // Owner policy channel: FL_GATE_APPROVED_POLICY_DIGESTS (empty = no failover).
    Ok(std::sync::Arc::new(
        GateGovernor::new(persistent)
            .with_approved_policy_digests(fl_gate::approved_policy_digests_from_env()),
    ))
}

fn hub_admission_unavailable() -> HttpResponse {
    HttpResponse::json(503, json!({ "error": fl_gate::ADMISSION_UNAVAILABLE }))
}

/// Principal for a hub authority: Owner authority is `Principal::Owner`;
/// a peer acts under a bundle named for its actor.
fn hub_principal(authority: &HubAuthority) -> Principal {
    if authority.is_owner() {
        Principal::Owner
    } else {
        Principal::Bundle(authority.actor_name().to_string())
    }
}

fn hub_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
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

    let governor = match hub_gate_governor(state) {
        Ok(g) => g,
        Err(()) => return hub_admission_unavailable(),
    };
    // Project binding: the request's `project` field when present, else ""
    // (the MCP layer then binds the matched connector id).
    let request_project = rpc_req
        .get("project")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let principal = hub_principal(authority);
    match handle_mcp_jsonrpc_request_with_governor_and_secret_provider(
        &rpc_req,
        &vault,
        &connectors,
        connector_filter,
        Some(authority.actor_name().to_string()),
        Some(&governor),
        &principal,
        &request_project,
        None,
    ) {
        Some(reply) => HttpResponse::json(200, reply),
        None => HttpResponse::json(202, json!({ "accepted": true })),
    }
}

/// Bridge connector dispatch through the governor. Governor construction
/// failure is a typed `admission_unavailable` refusal, never a legacy
/// direct invoke. Project binding is the connector id.
fn bridge_governed_execute(
    state: &HubState,
    authority: &HubAuthority,
    conn: &fl_gate::ConnectorManifest,
    vault: &Vault,
    tool: &str,
    args: &Value,
) -> Result<(fl_gate::ProxyResponse, ToolOutcome), fl_gate::GateError> {
    let gov = hub_gate_governor(state).map_err(|()| fl_gate::GateError::PolicyDenied {
        secret: "gate-governor".into(),
        reason: fl_gate::ADMISSION_UNAVAILABLE.to_string(),
    })?;
    let now = hub_now_ms();
    let project = conn.id.clone();
    match gov.governed_invoke(
        conn,
        vault,
        tool,
        args,
        Some("bridge".to_string()),
        hub_principal(authority),
        &project,
        now,
        // No real quota observation exists on this path.
        None,
    ) {
        Ok(GovernedOutcome::Completed {
            response, outcome, ..
        }) => Ok((response, outcome)),
        Ok(GovernedOutcome::Refused(outcome)) => Err(fl_gate::GateError::PolicyDenied {
            secret: "gate-governor".into(),
            reason: admission_refusal_text(&outcome),
        }),
        Err(e) => Err(e),
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
            match bridge_governed_execute(state, authority, conn, &vault, &resolved_tool, &args) {
                Ok((
                    _resp,
                    ToolOutcome::ContractFailedAfterExecution {
                        body_sha256,
                        body_bytes,
                        violation,
                    },
                )) => HttpResponse::json(
                    502,
                    json!({ "error": "executed_result_withheld", "outcome": "executed_result_withheld", "body_sha256": body_sha256, "body_bytes": body_bytes, "violation": violation, "plan": plan }),
                ),
                Ok((
                    resp,
                    ToolOutcome::ContractOk {
                        value: projected,
                        token_count,
                        count_unit,
                    },
                )) => {
                    let mut body = json!({
                        "status": "executed",
                        "outcome": "contract_ok",
                        "plan": plan,
                        "upstream_status": resp.status,
                        "result": projected,
                    });
                    if let Some(tc) = token_count {
                        body["token_count"] = json!(tc);
                        body["count_unit"] = json!(count_unit);
                    }
                    HttpResponse::json(200, body)
                }
                Ok((resp, ToolOutcome::Uncontracted(projected))) => HttpResponse::json(
                    200,
                    json!({
                        "status": "executed",
                        "outcome": "uncontracted",
                        "plan": plan,
                        "upstream_status": resp.status,
                        "result": projected,
                    }),
                ),
                Err(e) if e.to_string().contains(fl_gate::ADMISSION_UNAVAILABLE) => {
                    hub_admission_unavailable()
                }
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

    #[test]
    fn store_files_are_forbidden_artifact_paths_whatever_the_engine() {
        assert!(is_forbidden_artifact_path(Path::new("/w/data/foo.fls")));
        assert!(is_forbidden_artifact_path(Path::new("/w/data/foo.redb")));
        assert!(is_forbidden_artifact_path(Path::new(
            "/w/data/foo.fls.migrate.lock"
        )));
        assert!(!is_forbidden_artifact_path(Path::new("/w/data/notes.md")));
    }

    #[test]
    fn a_root_that_is_the_home_directory_is_recognised_through_a_link_and_a_dot() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let repo = tmp.path().join("repo");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&repo).unwrap();
        let link = tmp.path().join("home-link");
        std::os::unix::fs::symlink(&home, &link).unwrap();
        assert!(is_same_dir(&home, &home));
        assert!(is_same_dir(&link, &home));
        assert!(is_same_dir(&home.join("."), &home));
        assert!(!is_same_dir(&repo, &home));
        // a repository inside the home directory is a repository, not the home directory
        fs::create_dir_all(home.join("proj")).unwrap();
        assert!(!is_same_dir(&home.join("proj"), &home));
        assert!(!is_same_dir(&tmp.path().join("missing"), &home));
    }

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

    /// A throwaway store on the given engine, engine passed explicitly.
    fn test_store(kind: fl_store::BackendKind) -> (tempfile::TempDir, AnyStore) {
        let dir = tempfile::TempDir::new().unwrap();
        let store = fl_store::open_store_with(
            &dir.path().join("hub.store"),
            &fl_store::StoreOptions {
                backend: Some(kind),
                ..fl_store::StoreOptions::default()
            },
        )
        .unwrap();
        (dir, store)
    }

    fn roles_test_state() -> (tempfile::TempDir, HubState) {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = crate::HubServerConfig {
            store_path: tmp.path().join("store.redb"),
            repo_root: tmp.path().join("repo"),
            owner_token: Some("owner-test-tok".to_string()),
            peer_token: Some("peer-test-tok".to_string()),
            home_override: Some(tmp.path().join("home")),
            ..crate::HubServerConfig::default()
        };
        fs::create_dir_all(&cfg.repo_root).unwrap();
        (tmp, HubState::new(cfg))
    }

    fn roles_call(
        state: &HubState,
        method: &str,
        target: &str,
        token: &str,
        body: Option<Vec<u8>>,
    ) -> (u16, Value) {
        let mut req = HttpRequest::new(method, target);
        req.headers
            .insert("authorization".to_string(), format!("Bearer {token}"));
        if let Some(body) = body {
            req.body = body;
        }
        let resp = handle_http_request(state, &req);
        let value = serde_json::from_slice(&resp.body).unwrap_or(json!(null));
        (resp.status, value)
    }

    fn role_body(slug: &str, label: &str, builtin: bool) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "slug": slug, "label": label, "description": "d", "builtin": builtin,
            "review_policy": "lead-review", "prompt_template": "p"
        }))
        .unwrap()
    }

    #[test]
    fn gate_governor_unavailable_is_503_not_legacy_success() {
        let (tmp, mut state) = roles_test_state();
        // A directory where the store file should be: opening it fails.
        let bad = tmp.path().join("store-as-dir");
        fs::create_dir_all(&bad).unwrap();
        state.config.store_path = bad;
        let body = serde_json::to_vec(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "get_thing", "arguments": {}}
        }))
        .unwrap();
        let (status, value) = roles_call(
            &state,
            "POST",
            "/api/v1/gate/mcp",
            "owner-test-tok",
            Some(body),
        );
        assert_eq!(status, 503, "{value}");
        assert_eq!(value["error"], "admission_unavailable");
        // Raw gate call is governed too: same 503, not a legacy proxy attempt.
        let body = serde_json::to_vec(&json!({
            "method": "GET", "url": "https://example.com/x", "headers": {}, "body": null
        }))
        .unwrap();
        let (status, value) = roles_call(
            &state,
            "POST",
            "/api/v1/gate/call",
            "owner-test-tok",
            Some(body),
        );
        assert_eq!(status, 503, "{value}");
        assert_eq!(value["error"], "admission_unavailable");
    }

    #[test]
    fn roles_endpoints_list_create_override_and_delete() {
        let (_tmp, state) = roles_test_state();
        let owner = "owner-test-tok";
        let (status, list) = roles_call(&state, "GET", "/api/v1/roles", owner, None);
        assert_eq!(status, 200);
        let list = list.as_array().unwrap();
        assert_eq!(list.len(), 12);
        assert_eq!(list.iter().filter(|r| r["builtin"] == true).count(), 11);

        // Custom create -> 201, builtin=false; then listed.
        let (status, saved) = roles_call(
            &state,
            "POST",
            "/api/v1/roles",
            owner,
            Some(role_body("reviewer", "Reviewer", false)),
        );
        assert_eq!(status, 201, "{saved}");
        assert_eq!(saved["slug"], "reviewer");
        assert_eq!(saved["builtin"], false);
        let (_, list) = roles_call(&state, "GET", "/api/v1/roles", owner, None);
        assert_eq!(list.as_array().unwrap().len(), 13);
        assert!(
            list.as_array()
                .unwrap()
                .iter()
                .any(|r| r["slug"] == "reviewer" && r["builtin"] == false)
        );

        // Spoofed builtin:true on a custom slug is corrected to false.
        let (status, saved) = roles_call(
            &state,
            "POST",
            "/api/v1/roles",
            owner,
            Some(role_body("spoof", "Spoof", true)),
        );
        assert_eq!(status, 201, "{saved}");
        assert_eq!(saved["builtin"], false);

        // Builtin override keeps builtin=true (and existed -> 200).
        let (status, saved) = roles_call(
            &state,
            "POST",
            "/api/v1/roles",
            owner,
            Some(role_body("lead", "Boss", false)),
        );
        assert_eq!(status, 200, "{saved}");
        assert_eq!(saved["builtin"], true);
        assert_eq!(saved["label"], "Boss");

        // lab_manager normalizes to the builtin lab-manager.
        let (status, saved) = roles_call(
            &state,
            "POST",
            "/api/v1/roles",
            owner,
            Some(role_body("lab_manager", "Lab Boss", false)),
        );
        assert_eq!(status, 200, "{saved}");
        assert_eq!(saved["slug"], "lab-manager");
        assert_eq!(saved["builtin"], true);

        // Deletes: builtin 400 (also via lab_manager alias), unknown 404, custom 200 then absent.
        let (status, err) = roles_call(&state, "DELETE", "/api/v1/roles/lead", owner, None);
        assert_eq!(status, 400, "{err}");
        assert!(err["error"].as_str().unwrap().contains("built-in"));
        let (status, _) = roles_call(&state, "DELETE", "/api/v1/roles/lab_manager", owner, None);
        assert_eq!(status, 400);
        let (status, err) = roles_call(&state, "DELETE", "/api/v1/roles/nope", owner, None);
        assert_eq!(status, 404, "{err}");
        assert!(err.get("error").is_some());
        let (status, _) = roles_call(&state, "DELETE", "/api/v1/roles/reviewer", owner, None);
        assert_eq!(status, 200);
        let (_, list) = roles_call(&state, "GET", "/api/v1/roles", owner, None);
        assert!(
            !list
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["slug"] == "reviewer")
        );
    }

    #[test]
    fn roles_endpoints_reject_invalid_and_require_owner_for_writes() {
        let (_tmp, state) = roles_test_state();
        let owner = "owner-test-tok";
        let (status, err) = roles_call(
            &state,
            "POST",
            "/api/v1/roles",
            owner,
            Some(b"{not json".to_vec()),
        );
        assert_eq!(status, 400, "{err}");
        assert!(err.get("error").is_some());
        let bad_policy = serde_json::to_vec(&json!({
            "slug": "x", "label": "X", "description": "", "builtin": false,
            "review_policy": "bogus", "prompt_template": ""
        }))
        .unwrap();
        let (status, _) = roles_call(&state, "POST", "/api/v1/roles", owner, Some(bad_policy));
        assert_eq!(status, 400);
        let (status, _) = roles_call(
            &state,
            "POST",
            "/api/v1/roles",
            owner,
            Some(role_body("x", "  ", false)),
        );
        assert_eq!(status, 400);
        // Peer may read but not write.
        let (status, _) = roles_call(&state, "GET", "/api/v1/roles", "peer-test-tok", None);
        assert_eq!(status, 200);
        let (status, _) = roles_call(
            &state,
            "POST",
            "/api/v1/roles",
            "peer-test-tok",
            Some(role_body("y", "Y", false)),
        );
        assert_eq!(status, 403);
        let (status, _) = roles_call(&state, "DELETE", "/api/v1/roles/y", "peer-test-tok", None);
        assert_eq!(status, 403);
    }

    #[test]
    fn cloud_sessions_list_defaults() {
        let (_tmp, state) = roles_test_state();
        let (status, list) = roles_call(
            &state,
            "GET",
            "/api/v1/cloud-sessions",
            "owner-test-tok",
            None,
        );
        assert_eq!(status, 200, "{list}");
        let list = list.as_array().unwrap();
        assert_eq!(list.len(), 4);
        assert_eq!(list[0]["session_id"], "cs-claude-web-901");
    }

    #[test]
    fn cloud_session_attach_flips_mirrored_and_persists() {
        let (_tmp, state) = roles_test_state();
        let owner = "owner-test-tok";
        let (status, entry) = roles_call(
            &state,
            "POST",
            "/api/v1/cloud-sessions/cs-codex-cloud-412/attach",
            owner,
            None,
        );
        assert_eq!(status, 200, "{entry}");
        assert_eq!(entry["session_id"], "cs-codex-cloud-412");
        assert_eq!(entry["mirrored"], true);
        let (status, list) = roles_call(&state, "GET", "/api/v1/cloud-sessions", owner, None);
        assert_eq!(status, 200);
        let found = list
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["session_id"] == "cs-codex-cloud-412")
            .unwrap();
        assert_eq!(found["mirrored"], true);
    }

    #[test]
    fn cloud_session_attach_unknown_404_and_peer_403() {
        let (_tmp, state) = roles_test_state();
        let (status, err) = roles_call(
            &state,
            "POST",
            "/api/v1/cloud-sessions/nope/attach",
            "owner-test-tok",
            None,
        );
        assert_eq!(status, 404, "{err}");
        assert!(err.get("error").is_some());
        let (status, err) = roles_call(
            &state,
            "POST",
            "/api/v1/cloud-sessions/cs-codex-cloud-412/attach",
            "peer-test-tok",
            None,
        );
        assert_eq!(status, 403, "{err}");
        assert!(err.get("error").is_some());
    }

    #[test]
    fn cloud_session_handoff_returns_branch_and_synced() {
        let (_tmp, state) = roles_test_state();
        let (status, resp) = roles_call(
            &state,
            "POST",
            "/api/v1/cloud-sessions/cs-claude-web-901/handoff",
            "owner-test-tok",
            None,
        );
        assert_eq!(status, 200, "{resp}");
        assert_eq!(resp["session_id"], "cs-claude-web-901");
        assert_eq!(
            resp["repo_branch"],
            "Artificial-Humanity/FerroLoop@feat/cloud-handoff"
        );
        assert_eq!(resp["handoff_synced"], true);
        assert!(!resp["synced_at"].as_str().unwrap().is_empty());
        let (status, err) = roles_call(
            &state,
            "POST",
            "/api/v1/cloud-sessions/nope/handoff",
            "owner-test-tok",
            None,
        );
        assert_eq!(status, 404, "{err}");
        assert!(err.get("error").is_some());
    }

    #[test]
    fn cloud_session_status_persists_and_bad_status_400() {
        let (_tmp, state) = roles_test_state();
        let owner = "owner-test-tok";
        let body = serde_json::to_vec(&json!({ "status": "paused" })).unwrap();
        let (status, entry) = roles_call(
            &state,
            "POST",
            "/api/v1/cloud-sessions/cs-claude-web-901/status",
            owner,
            Some(body),
        );
        assert_eq!(status, 200, "{entry}");
        assert_eq!(entry["status"], "paused");
        let (_, list) = roles_call(&state, "GET", "/api/v1/cloud-sessions", owner, None);
        let found = list
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["session_id"] == "cs-claude-web-901")
            .unwrap();
        assert_eq!(found["status"], "paused");
        let bad = serde_json::to_vec(&json!({ "status": "bogus" })).unwrap();
        let (status, err) = roles_call(
            &state,
            "POST",
            "/api/v1/cloud-sessions/cs-claude-web-901/status",
            owner,
            Some(bad),
        );
        assert_eq!(status, 400, "{err}");
        assert!(err.get("error").is_some());
        let body = serde_json::to_vec(&json!({ "status": "paused" })).unwrap();
        let (status, _) = roles_call(
            &state,
            "POST",
            "/api/v1/cloud-sessions/nope/status",
            owner,
            Some(body),
        );
        assert_eq!(status, 404);
    }

    #[test]
    fn local_providers_list_defaults() {
        let (_tmp, state) = roles_test_state();
        let (status, list) = roles_call(
            &state,
            "GET",
            "/api/v1/providers/local",
            "owner-test-tok",
            None,
        );
        assert_eq!(status, 200, "{list}");
        assert_eq!(list.as_array().unwrap().len(), 3);
    }

    #[test]
    fn local_providers_update_ollama_keeps_id_and_persists() {
        let (_tmp, state) = roles_test_state();
        let owner = "owner-test-tok";
        let body = serde_json::to_vec(&json!({
            "kind": "ollama",
            "endpoint_url": "http://127.0.0.1:9999/v1",
            "vault_credential_ref": "keyless (loopback)",
            "context_window_tokens": 131072,
            "max_output_tokens": 16384
        }))
        .unwrap();
        let (status, preset) =
            roles_call(&state, "POST", "/api/v1/providers/local", owner, Some(body));
        assert_eq!(status, 200, "{preset}");
        assert_eq!(preset["id"], "ollama-local");
        assert_eq!(preset["endpoint_url"], "http://127.0.0.1:9999/v1");
        let (_, list) = roles_call(&state, "GET", "/api/v1/providers/local", owner, None);
        let found = list
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == "ollama-local")
            .unwrap();
        assert_eq!(found["endpoint_url"], "http://127.0.0.1:9999/v1");
    }

    #[test]
    fn local_providers_raw_key_400_and_peer_403() {
        let (_tmp, state) = roles_test_state();
        let body = serde_json::to_vec(&json!({
            "kind": "ollama",
            "endpoint_url": "http://127.0.0.1:9999/v1",
            "vault_credential_ref": "sk-rawkey123",
            "context_window_tokens": 131072,
            "max_output_tokens": 16384
        }))
        .unwrap();
        let (status, err) = roles_call(
            &state,
            "POST",
            "/api/v1/providers/local",
            "owner-test-tok",
            Some(body.clone()),
        );
        assert_eq!(status, 400, "{err}");
        assert!(err.get("error").is_some());
        let (status, _) = roles_call(
            &state,
            "POST",
            "/api/v1/providers/local",
            "peer-test-tok",
            Some(body),
        );
        assert_eq!(status, 403);
    }

    #[test]
    fn notifications_subscribe_defaults_persist_and_owner_gate() {
        let (_tmp, state) = roles_test_state();
        let owner = "owner-test-tok";
        let (status, got) = roles_call(
            &state,
            "GET",
            "/api/v1/notifications/subscribe",
            owner,
            None,
        );
        assert_eq!(status, 200, "{got}");
        assert_eq!(got["project_scope"], "all");
        assert_eq!(got["web_push_permission"], "granted");
        assert_eq!(got["quiet_hours"]["enabled"], true);
        assert_eq!(got["quiet_hours"]["start_time"], "22:00");
        // Any authenticated caller may read.
        let (status, _) = roles_call(
            &state,
            "GET",
            "/api/v1/notifications/subscribe",
            "peer-test-tok",
            None,
        );
        assert_eq!(status, 200);

        // Persist with quiet disabled, then GET reflects it.
        let mut body = got.clone();
        body["quiet_hours"]["enabled"] = json!(false);
        body["project_scope"] = json!("ferroloop");
        let (status, saved) = roles_call(
            &state,
            "POST",
            "/api/v1/notifications/subscribe",
            owner,
            Some(serde_json::to_vec(&body).unwrap()),
        );
        assert_eq!(status, 200, "{saved}");
        assert_eq!(saved["project_scope"], "ferroloop");
        let (status, got) = roles_call(
            &state,
            "GET",
            "/api/v1/notifications/subscribe",
            owner,
            None,
        );
        assert_eq!(status, 200);
        assert_eq!(got["project_scope"], "ferroloop");
        assert_eq!(got["quiet_hours"]["enabled"], false);

        // Non-owner write -> 403 and no change; invalid quiet time -> 400.
        let (status, err) = roles_call(
            &state,
            "POST",
            "/api/v1/notifications/subscribe",
            "peer-test-tok",
            Some(serde_json::to_vec(&body).unwrap()),
        );
        assert_eq!(status, 403, "{err}");
        assert!(
            err["error"]
                .as_str()
                .unwrap()
                .contains("only Owner authority")
        );
        let mut bad = body.clone();
        bad["quiet_hours"]["enabled"] = json!(true);
        bad["quiet_hours"]["start_time"] = json!("25:00");
        let (status, err) = roles_call(
            &state,
            "POST",
            "/api/v1/notifications/subscribe",
            owner,
            Some(serde_json::to_vec(&bad).unwrap()),
        );
        assert_eq!(status, 400, "{err}");
        assert!(err.get("error").is_some());
    }

    #[test]
    fn notifications_test_dispatch_validates_and_records_receipt() {
        let (_tmp, state) = roles_test_state();
        let owner = "owner-test-tok";
        // Deterministic: disable quiet hours first.
        let (_, got) = roles_call(
            &state,
            "GET",
            "/api/v1/notifications/subscribe",
            owner,
            None,
        );
        let mut body = got;
        body["quiet_hours"]["enabled"] = json!(false);
        let (status, _) = roles_call(
            &state,
            "POST",
            "/api/v1/notifications/subscribe",
            owner,
            Some(serde_json::to_vec(&body).unwrap()),
        );
        assert_eq!(status, 200);

        let test_body = serde_json::to_vec(&json!({
            "deep_link": "/sessions/abc", "project_scope": "ferroloop"
        }))
        .unwrap();
        let (status, resp) = roles_call(
            &state,
            "POST",
            "/api/v1/notifications/test",
            owner,
            Some(test_body),
        );
        assert_eq!(status, 200, "{resp}");
        assert_eq!(resp["dispatched"], true);
        assert_eq!(resp["deep_link"], "/sessions/abc");
        assert_eq!(resp["project_scope"], "ferroloop");
        assert_eq!(resp["channels"], json!(["vapid", "apns", "fcm"]));
        assert_eq!(resp["quiet_hours_active"], false);

        for bad_link in [
            "https://evil",
            "//evil",
            "/a\nb",
            "relative",
            "/a\\b",
            "/a b",
            "/a\tb",
            "/a\u{1}b",
        ] {
            let bad = serde_json::to_vec(&json!({ "deep_link": bad_link })).unwrap();
            let (status, err) = roles_call(
                &state,
                "POST",
                "/api/v1/notifications/test",
                owner,
                Some(bad),
            );
            assert_eq!(status, 400, "{bad_link}: {err}");
            assert!(err.get("error").is_some());
        }
        let (status, _) = roles_call(
            &state,
            "POST",
            "/api/v1/notifications/test",
            "peer-test-tok",
            Some(serde_json::to_vec(&json!({ "deep_link": "/x" })).unwrap()),
        );
        assert_eq!(status, 403);

        let (status, events) =
            roles_call(&state, "GET", "/api/v1/notifications/events", owner, None);
        assert_eq!(status, 200, "{events}");
        let events = events.as_array().unwrap();
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0]["deep_link"], "/sessions/abc");
        assert_eq!(events[0]["dispatched"], true);
        assert_eq!(events[0]["channels"], json!(["vapid", "apns", "fcm"]));
        assert!(events[0]["at_ms"].as_u64().unwrap() > 0);
    }

    #[test]
    fn notifications_test_push_quiet_window_records_undispatched_receipt() {
        let (_tmp, state) = roles_test_state();
        // Default state: quiet 22:00-07:00 UTC, enabled.
        let push_state = fl_core::OperatorPushSubscriptionState::default();
        let evaluation = evaluate_test_push(
            &push_state,
            "/sessions/abc".to_string(),
            Some("ferroloop".to_string()),
            "23:00",
            1_700_000_000_000,
        );
        assert!(evaluation.quiet_hours_active);
        assert!(!evaluation.dispatched);
        assert!(evaluation.receipt.quiet_hours_active);
        assert!(!evaluation.receipt.dispatched);
        assert_eq!(evaluation.receipt.at_ms, 1_700_000_000_000);
        let store = state.open_store().unwrap();
        fl_core::append_push_receipt(&*store, evaluation.receipt).unwrap();
        drop(store);
        let (status, events) = roles_call(
            &state,
            "GET",
            "/api/v1/notifications/events",
            "owner-test-tok",
            None,
        );
        assert_eq!(status, 200, "{events}");
        let events = events.as_array().unwrap();
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0]["quiet_hours_active"], true);
        assert_eq!(events[0]["dispatched"], false);
    }

    fn put_memory(
        store: &AnyStore,
        slug: &str,
        summary: &str,
        body: &str,
        sensitive: bool,
        at: u64,
    ) {
        store
            .upsert_state(UpsertStateArgs {
                scope: StateScope::Workspace,
                slug: slug.to_string(),
                sensitivity: if sensitive {
                    SensitivityPatch::SetSensitive
                } else {
                    SensitivityPatch::Inherit
                },
                payload: StatePayload::Memory(MemoryEntry {
                    category: MemoryCategory::Note,
                    basis: MemoryBasis::Inference,
                    lifecycle: MemoryLifecycle::Active,
                    superseded_by: None,
                    pinned: false,
                    summary: summary.to_string(),
                    body: body.to_string(),
                }),
                at: At::from_unix_millis(at),
                by: "test".to_string(),
            })
            .unwrap();
    }

    #[test]
    fn hub_search_on_ferro_is_read_only_and_keeps_redaction() {
        let (_dir, store) = test_store(fl_store::BackendKind::Ferro);
        put_memory(
            &store,
            "build-cache",
            "checkpoint the build cache",
            "Nightly job writes a checkpoint of the build cache to disk.",
            false,
            1_001,
        );
        put_memory(
            &store,
            "secret-cache",
            "checkpoint the build cache",
            "Nightly job writes a checkpoint, secretbodytoken, to disk.",
            true,
            1_002,
        );
        put_memory(
            &store,
            "hub-session-abc123",
            "checkpoint the internal session",
            "Internal checkpoint state of the hub session.",
            false,
            1_003,
        );
        put_memory(
            &store,
            "lunch-notes",
            "meeting notes about lunch",
            "The team agreed on sandwiches for the offsite.",
            false,
            1_004,
        );
        let entries = store.list_state(None, None).unwrap();
        assert!(entries.iter().any(|e| e.slug == "hub-session-abc123"));

        // A hub search never creates the vector index: a stem query finds
        // nothing before a CLI-style search has indexed the entries.
        let (hits, _) =
            search_state_for_hub(&store, &entries, "checkpointing", None, None, 10, true);
        assert!(hits.is_empty(), "read-only search indexed: {hits:?}");

        // A CLI or MCP style search (it may write) indexes the entries.
        let cli = fl_store::rank_state_entries(&store, &entries, "checkpoint", None, None, 10);
        assert!(!cli.hits.is_empty());

        // A stem query and a lone typo, which BM25 cannot match.
        for q in ["checkpointing", "chekpoint"] {
            let (hits, total) = search_state_for_hub(&store, &entries, q, None, None, 10, false);
            let slugs: Vec<&str> = hits.iter().map(|h| h.entry.slug.as_str()).collect();
            assert!(slugs.contains(&"build-cache"), "`{q}` found {slugs:?}");
            assert!(
                !slugs.contains(&"hub-session-abc123"),
                "internal hub entry leaked for `{q}`: {slugs:?}"
            );
            assert!(!slugs.contains(&"lunch-notes"), "`{q}` found {slugs:?}");
            // Sensitive entries are never indexed, so a vector-only query
            // cannot reach them, for anyone.
            assert!(!slugs.contains(&"secret-cache"), "`{q}` found {slugs:?}");
            assert_eq!(total, hits.len(), "`{q}`");
        }

        // A lexical query reaches the sensitive entry: redacted for a
        // non-owner (entry and snippet), real for the owner.
        let (hits, _) = search_state_for_hub(&store, &entries, "checkpoint", None, None, 10, false);
        let slugs: Vec<&str> = hits.iter().map(|h| h.entry.slug.as_str()).collect();
        let secret = hits
            .iter()
            .find(|h| h.entry.slug == "secret-cache")
            .unwrap_or_else(|| panic!("sensitive hit missing: {slugs:?}"));
        assert_eq!(secret.snippet, "[REDACTED]");
        let serialized = serde_json::to_string(secret).unwrap();
        assert!(!serialized.contains("secretbodytoken"), "{serialized}");
        assert!(!slugs.contains(&"hub-session-abc123"), "{slugs:?}");

        let (owner, _) = search_state_for_hub(&store, &entries, "checkpoint", None, None, 10, true);
        let secret = owner
            .iter()
            .find(|h| h.entry.slug == "secret-cache")
            .unwrap();
        assert_ne!(secret.snippet, "[REDACTED]");
        assert!(owner.iter().all(|h| h.entry.slug != "hub-session-abc123"));
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
        let (_dir, store) = test_store(fl_store::BackendKind::Redb);

        // Non-owner: sensitive entry and snippet redacted, internal excluded.
        let (hits, total) = search_state_for_hub(&store, &entries, "zephyr", None, None, 10, false);
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
            search_state_for_hub(&store, &entries, "zephyr", None, None, 10, true);
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
        let (none, total_zero) =
            search_state_for_hub(&store, &entries, "zephyr", None, None, 0, true);
        assert!(none.is_empty());
        assert_eq!(total_zero, 2);

        // Truncation keeps total before the cap.
        let (one, total_one) =
            search_state_for_hub(&store, &entries, "zephyr", None, None, 1, true);
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
        let (_dir, store) = test_store(fl_store::BackendKind::Redb);

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
        let (hits, total) = search_state_for_hub(&store, &entries, "zephyr", None, None, 10, false);
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
        let (owner_hits, _) =
            search_state_for_hub(&store, &entries, "zephyr", None, None, 10, true);
        let serialized_owner = serde_json::to_string(&owner_hits[0]).unwrap();
        for token in SECRET_TOKENS {
            assert!(serialized_owner.contains(token), "{serialized_owner}");
        }
    }

    /// A stored (uncompressed) ZIP holding one entry, enough for the office reader.
    fn tiny_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, data) in entries {
            let off = out.len() as u32;
            out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
            out.extend_from_slice(&[20, 0, 0, 0, 0, 0]);
            out.extend_from_slice(&[0; 8]);
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(data);
            central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            central.extend_from_slice(&[20, 0, 20, 0, 0, 0, 0, 0]);
            central.extend_from_slice(&[0; 8]);
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&[0; 12]);
            central.extend_from_slice(&off.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let cd_off = out.len() as u32;
        out.extend_from_slice(&central);
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(central.len() as u32).to_le_bytes());
        out.extend_from_slice(&cd_off.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    #[test]
    fn artifact_versions_and_office_preview_endpoints_serve_vetted_paths_only() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo_root = tmp.path().join("repo");
        let home_dir = tmp.path().join("home");
        fs::create_dir_all(&repo_root).unwrap();
        fs::create_dir_all(&home_dir).unwrap();
        let cfg = crate::HubServerConfig {
            store_path: tmp.path().join("store.redb"),
            repo_root: repo_root.clone(),
            owner_token: Some("owner-test-tok".to_string()),
            home_override: Some(home_dir),
            ..crate::HubServerConfig::default()
        };
        let state = HubState::new(cfg);
        let get = |target: &str| -> HttpResponse {
            let mut req = HttpRequest::new("GET", target);
            req.headers.insert(
                "authorization".to_string(),
                "Bearer owner-test-tok".to_string(),
            );
            handle_http_request(&state, &req)
        };
        let json_of = |r: &HttpResponse| -> Value { serde_json::from_slice(&r.body).unwrap() };
        let enc = |p: &Path| percent_encode_query_value(&p.display().to_string());

        // Office preview.
        let docx = repo_root.join("brief.docx");
        let body = br#"<w:document><w:body><w:p><w:pPr><w:pStyle w:val="Heading2"/></w:pPr><w:r><w:t>Hi</w:t></w:r></w:p></w:body></w:document>"#;
        fs::write(&docx, tiny_zip(&[("word/document.xml", body)])).unwrap();
        let r = get(&format!(
            "/api/v1/artifacts/office-preview?path={}",
            enc(&docx)
        ));
        assert_eq!(r.status, 200);
        let v = json_of(&r);
        assert_eq!(v["ok"], true);
        assert_eq!(v["ext"], "docx");
        assert_eq!(
            v["preview"]["blocks"][0],
            json!({"type":"heading","level":2,"text":"Hi"})
        );
        // Not an office extension, a corrupt file, a missing file, a forbidden path.
        let md = repo_root.join("note.md");
        fs::write(&md, "# n").unwrap();
        let r = get(&format!(
            "/api/v1/artifacts/office-preview?path={}",
            enc(&md)
        ));
        assert_eq!(r.status, 400);
        let bad = repo_root.join("bad.xlsx");
        fs::write(&bad, "this is not a zip file at all").unwrap();
        let r = get(&format!(
            "/api/v1/artifacts/office-preview?path={}",
            enc(&bad)
        ));
        assert_eq!(r.status, 422);
        let r = get(&format!(
            "/api/v1/artifacts/office-preview?path={}",
            enc(&repo_root.join("nope.docx"))
        ));
        assert_eq!(r.status, 404);
        let env = repo_root.join(".env");
        fs::write(&env, "T=1").unwrap();
        let r = get(&format!(
            "/api/v1/artifacts/office-preview?path={}",
            enc(&env)
        ));
        assert_eq!(r.status, 403);
        assert_eq!(get("/api/v1/artifacts/office-preview").status, 400);

        // Version history: first request registers and records v1.
        let r = get(&format!("/api/v1/artifacts/versions?path={}", enc(&md)));
        assert_eq!(r.status, 200);
        let v = json_of(&r);
        assert_eq!(v["ok"], true);
        assert_eq!(v["versions"].as_array().unwrap().len(), 1);
        assert_eq!(v["versions"][0]["version"], 1);
        assert_eq!(v["versions"][0]["is_current"], true);
        assert_eq!(v["versions"][0]["size_bytes"], 3);
        assert_eq!(v["versions"][0]["sha256"].as_str().unwrap().len(), 64);
        // Unchanged: still one. Changed: two, and only the newest is current.
        let r = get(&format!("/api/v1/artifacts/versions?path={}", enc(&md)));
        assert_eq!(json_of(&r)["versions"].as_array().unwrap().len(), 1);
        fs::write(&md, "# new text").unwrap();
        let r = get(&format!("/api/v1/artifacts/versions?path={}", enc(&md)));
        let v = json_of(&r);
        let rows = v["versions"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            (rows[0]["is_current"].clone(), rows[1]["is_current"].clone()),
            (json!(false), json!(true))
        );
        assert_ne!(rows[0]["sha256"], rows[1]["sha256"]);

        // Raw content of each revision.
        let raw = |n: &str| {
            get(&format!(
                "/api/v1/artifacts/version-raw?path={}&version={n}",
                enc(&md)
            ))
        };
        let r = raw("1");
        assert_eq!(r.status, 200);
        assert_eq!(r.body, b"# n");
        assert_eq!(raw("2").body, b"# new text");
        assert_eq!(raw("3").status, 404);
        assert_eq!(raw("0").status, 400);
        assert_eq!(raw("abc").status, 400);
        assert_eq!(
            get(&format!("/api/v1/artifacts/version-raw?path={}", enc(&md))).status,
            400
        );
        // Path vetting applies to both endpoints.
        for ep in ["versions", "version-raw"] {
            let r = get(&format!(
                "/api/v1/artifacts/{ep}?path={}&version=1",
                enc(&env)
            ));
            assert_eq!(r.status, 403, "{ep}");
            let r = get(&format!(
                "/api/v1/artifacts/{ep}?path=%2Fetc%2Fpasswd&version=1"
            ));
            assert_eq!(r.status, 403, "{ep}");
        }
        // A listed-but-never-recorded path has no revision to serve.
        let other = repo_root.join("other.md");
        fs::write(&other, "o").unwrap();
        assert_eq!(
            get(&format!(
                "/api/v1/artifacts/version-raw?path={}&version=1",
                enc(&other)
            ))
            .status,
            404
        );
        // Both need authentication.
        let unauth = HttpRequest::new(
            "GET",
            format!("/api/v1/artifacts/versions?path={}", enc(&md)),
        );
        assert_ne!(handle_http_request(&state, &unauth).status, 200);
    }

    #[test]
    fn the_versions_route_registers_only_kept_types_and_stops_at_the_registry_cap() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo_root = tmp.path().join("repo");
        let home_dir = tmp.path().join("home");
        fs::create_dir_all(&repo_root).unwrap();
        fs::create_dir_all(&home_dir).unwrap();
        let cfg = crate::HubServerConfig {
            store_path: tmp.path().join("store.redb"),
            repo_root: repo_root.clone(),
            owner_token: Some("owner-test-tok".to_string()),
            home_override: Some(home_dir),
            ..crate::HubServerConfig::default()
        };
        let state = HubState::new(cfg);
        let get = |p: &Path| -> HttpResponse {
            let target = format!(
                "/api/v1/artifacts/versions?path={}",
                percent_encode_query_value(&p.display().to_string())
            );
            let mut req = HttpRequest::new("GET", target);
            req.headers.insert(
                "authorization".to_string(),
                "Bearer owner-test-tok".to_string(),
            );
            handle_http_request(&state, &req)
        };
        let root = artifacts_root(&state);
        let entries = || artifacts::with_registry(&root, |reg| reg.entries.len());

        // A type the store does not keep is refused and leaves no registry entry.
        let code = repo_root.join("main.rs");
        fs::write(&code, "fn main() {}").unwrap();
        assert_eq!(get(&code).status, 415);
        assert_eq!(entries(), 0);

        // A kept type registers.
        let md = repo_root.join("note.md");
        fs::write(&md, "# n").unwrap();
        assert_eq!(get(&md).status, 200);
        assert_eq!(entries(), 1);

        // With the registry full, a new file is refused and nothing is added; a registered one
        // still answers.
        artifacts::with_registry(&root, |reg| {
            for i in 0..artifacts::MAX_REGISTRY_ENTRIES {
                let mut e = artifact_entry(Path::new(&format!("/gone/{i}.md")), 1, None);
                e.status = artifacts::STATUS_ARCHIVED.to_string();
                reg.entries.push(e);
            }
        });
        let full = entries();
        let other = repo_root.join("other.md");
        fs::write(&other, "o").unwrap();
        assert_eq!(get(&other).status, 507);
        assert_eq!(entries(), full);
        assert_eq!(get(&md).status, 200);
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

#[cfg(test)]
mod council_guidance_tests {
    use super::*;
    use crate::HubServerConfig;
    use fl_core::agent::RegisterAgentArgs;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    #[test]
    fn catalog_chair_explicit_hydration_reads_edits_and_refuses_deleted_guidance() {
        let tmp = TempDir::new().unwrap();
        let state = HubState::new(HubServerConfig {
            store_path: tmp.path().join("store.redb"),
            repo_root: tmp.path().into(),
            home_override: Some(tmp.path().into()),
            ..HubServerConfig::default()
        });
        {
            let store = state.open_store().unwrap();
            store
                .register_agent(RegisterAgentArgs {
                    display_name: "Ada".into(),
                    email: None,
                    project: "ferroloop".into(),
                    role: AgentRole::Custom("council-chair".into()),
                    extra_areas: vec![],
                    create_area: true,
                    replace: false,
                    at: stamp::now(),
                    by: "owner".into(),
                })
                .unwrap();
            let mut chair = fl_core::agent::get_role(&*store, "council-chair")
                .unwrap()
                .unwrap();
            chair.prompt_template = "CURRENT EDITED CATALOG ADVICE".into();
            fl_core::agent::upsert_role(&*store, chair).unwrap();
        }
        let prompt = build_reforge_prompt(&state, "ada").unwrap();
        assert!(prompt.contains("CURRENT EDITED CATALOG ADVICE"));
        assert!(!prompt.contains("roles/council-chair.md"));
        fl_core::agent::delete_role(&*state.open_store().unwrap(), "council-chair").unwrap();
        let error = build_reforge_prompt(&state, "ada").unwrap_err();
        assert_eq!(error.status(), 400);
        assert!(error.to_string().contains("missing or deleted"));
    }

    #[test]
    fn refused_reforge_preflight_preserves_alias_queues_and_model_overrides() {
        use fl_wire::identity::{RouteTable, VendorRoute};
        for corruption in ["deleted", "corrupt"] {
            let tmp = TempDir::new().unwrap();
            let state = HubState::new(HubServerConfig {
                store_path: tmp.path().join("store.redb"),
                repo_root: tmp.path().into(),
                home_override: Some(tmp.path().into()),
                ..HubServerConfig::default()
            });
            let store = state.open_store().unwrap();
            store
                .register_agent(RegisterAgentArgs {
                    display_name: "Ada".into(),
                    email: None,
                    project: "ferroloop".into(),
                    role: AgentRole::Custom("council-chair".into()),
                    extra_areas: vec![],
                    create_area: true,
                    replace: false,
                    at: stamp::now(),
                    by: "owner".into(),
                })
                .unwrap();
            if corruption == "deleted" {
                fl_core::agent::delete_role(&*store, "council-chair").unwrap();
            } else {
                store
                    .upsert_state(UpsertStateArgs {
                        scope: StateScope::Workspace,
                        slug: "role-council-chair".into(),
                        sensitivity: SensitivityPatch::Inherit,
                        payload: StatePayload::Runtime(RuntimeEntry {
                            value: json!({"invalid": "guidance"}),
                            merge_mode: fl_core::state::MergeMode::LastWriter,
                        }),
                        at: stamp::now(),
                        by: "owner".into(),
                    })
                    .unwrap();
            }
            drop(store);
            let socket = tmp.path().join("reset.sock");
            let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
            listener.set_nonblocking(true).unwrap();
            let mut routes = RouteTable::default();
            routes.upsert(
                "ada",
                VendorRoute::Webhook {
                    socket_path: socket,
                },
                stamp::now(),
            );
            routes
                .save(Some(&tmp.path().join(".cache/agent-bus/routes.json")))
                .unwrap();
            let keys = ["webhook-ada", "raw-alias", "ada"];
            for key in keys {
                // Seed the queues directly: this fixture must not run discovery or route probes.
                state.prompt_queues.lock().unwrap().enqueue(
                    key,
                    crate::DirectPromptInput {
                        intended_codex_thread_id: None,
                        persona: Some("ada".into()),
                        prompt: format!("prompt-{key}"),
                        effective_body: format!("body-{key}"),
                        by: "owner".into(),
                        attachments: vec![],
                    },
                );
                state.set_session_model_override(
                    key,
                    session_actions::ModelOverride::new(
                        format!("model-{key}"),
                        Some(format!("thinking-{key}")),
                    ),
                );
            }
            let before: Vec<_> = keys
                .iter()
                .map(|key| (state.session_queue(key), state.session_model_override(key)))
                .collect();
            let error = reset_session(&state, "owner", keys[1], keys[0], Some(keys[2]), "reforge")
                .unwrap_err();
            assert_eq!(error.0, 400, "{corruption}: {error:?}");
            assert!(error.1.contains("missing or deleted"), "{error:?}");
            for (key, expected) in keys.iter().zip(before) {
                assert_eq!(
                    (state.session_queue(key), state.session_model_override(key)),
                    expected,
                    "{corruption}: {key}"
                );
            }
            assert_eq!(
                listener.accept().unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock,
                "preflight refusal must dispatch no reset"
            );
        }
    }

    #[test]
    fn recast_explicit_reforge_uses_current_registration_despite_opaque_hydration() {
        let tmp = TempDir::new().unwrap();
        let state = HubState::new(HubServerConfig {
            store_path: tmp.path().join("store.redb"),
            repo_root: tmp.path().into(),
            home_override: Some(tmp.path().into()),
            owner_token: Some("owner-secret".into()),
            ..HubServerConfig::default()
        });
        state
            .open_store()
            .unwrap()
            .register_agent(RegisterAgentArgs {
                display_name: "Ada".into(),
                email: None,
                project: "old-project".into(),
                role: AgentRole::Assistant,
                extra_areas: vec![],
                create_area: true,
                replace: false,
                at: stamp::now(),
                by: "owner".into(),
            })
            .unwrap();
        let script = tmp.path().join("hydrate-fixture");
        std::fs::write(
            &script,
            "#!/bin/sh\nprintf '%s\\n' '{\"Ada\":\"STALE OPAQUE ASSISTANT\"}'\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        with_hydrate_lock(|| {
            let previous = std::env::var_os("FL_HUB_AGENT_HYDRATE_BIN");
            unsafe {
                std::env::set_var("FL_HUB_AGENT_HYDRATE_BIN", &script);
            }
            for role in ["custom:council-chair", "assistant"] {
                let response = crate::handle_http_request(
                    &state,
                    &HttpRequest::new("POST", "/api/v1/agents/ada/recast")
                        .with_header("Authorization", "Bearer owner-secret")
                        .with_json_body(
                            &json!({"project":"new-project", "role":role, "create_area":true}),
                        ),
                );
                assert_eq!(response.status, 200);
                let prompt = build_reforge_prompt(&state, "ada").unwrap();
                assert!(prompt.contains("new-project/"), "{prompt}");
                assert!(!prompt.contains("STALE OPAQUE"), "{prompt}");
                assert!(
                    prompt.contains(&format!("--role {role} --replace")),
                    "{prompt}"
                );
                if role == "custom:council-chair" {
                    assert!(prompt.contains("preferred contact"), "{prompt}");
                    assert!(!prompt.contains("roles/council-chair.md"), "{prompt}");
                }
            }
            unsafe {
                match previous {
                    Some(value) => std::env::set_var("FL_HUB_AGENT_HYDRATE_BIN", value),
                    None => std::env::remove_var("FL_HUB_AGENT_HYDRATE_BIN"),
                }
            }
        });
    }
}
