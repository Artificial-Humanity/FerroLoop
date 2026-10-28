//! Versioned `/api/v1/*` JSON REST API and static `FerroDeck` asset / SPA serving (spec §2.3, §3).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use base64::Engine;

use fl_core::agent::AgentStatus;
use fl_core::arch::{
    ArchitectureModel, DEFAULT_ARCH_MANIFEST_PATH, export_likec4_dsl, validate_architecture_model,
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
use fl_wire::dispatcher::{dispatch_message_for_store, dispatch_operator_prompt_to_route};
use fl_wire::identity::RouteTable;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::auth::{
    HubAuthority, extract_bearer_token, extract_session_cookie, format_logout_set_cookie,
    format_session_set_cookie,
};
use crate::discovery::{
    discover_sessions_with_home, list_git_worktrees, read_session_transcript_with_home,
    read_worktree_diff, scan_cargo_architecture_conformance,
};
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

    // 4. Authenticate `/api/v1/*` request.
    let store = match state.open_store() {
        Ok(s) => s,
        Err(e) => {
            return HttpResponse::json(
                500,
                json!({ "error": format!("failed to open store: {e}") }),
            );
        }
    };

    let Some(authority) = state
        .auth
        .authenticate_request(Some(&store), &req.headers, &req.query)
    else {
        return HttpResponse::json(401, json!({ "error": "unauthorized" }));
    };

    match (method, path) {
        ("GET", "/api/v1/auth/session") => HttpResponse::json(
            200,
            json!({
                "authenticated": true,
                "authority": authority,
            }),
        ),
        ("POST", "/api/v1/auth/logout") => {
            if let Some(tok) = req
                .headers
                .get("cookie")
                .and_then(|c| extract_session_cookie(c))
                .or_else(|| {
                    req.headers
                        .get("authorization")
                        .and_then(|a| extract_bearer_token(a))
                })
            {
                state.auth.revoke_session_token(Some(&store), &tok);
            }
            HttpResponse::json(200, json!({ "authenticated": false }))
                .with_header("Set-Cookie", format_logout_set_cookie())
        }
        ("GET", "/api/v1/agents") => handle_get_agents(state, &store, req),
        ("GET", "/api/v1/agents/liveness") => handle_get_agents_liveness(state, &store, req),
        ("GET", "/api/v1/sessions") => {
            drain_ready_session_queues(state, &store);
            let sessions =
                discover_sessions_with_home(&store, state.config.home_override.as_deref());
            HttpResponse::json(200, json!({ "sessions": sessions }))
        }
        ("GET", "/api/v1/worktrees") => match list_git_worktrees(&state.config.repo_root) {
            Ok(worktrees) => HttpResponse::json(200, json!({ "worktrees": worktrees })),
            Err(e) => HttpResponse::json(500, json!({ "error": e })),
        },
        ("GET", "/api/v1/prs") => handle_get_prs(&store),
        ("GET", "/api/v1/wire/messages") => handle_get_wire_messages(&store, req),
        ("POST", "/api/v1/wire/send") => handle_post_wire_send(state, &store, &authority, req),
        ("POST", "/api/v1/wire/ack") => handle_post_wire_ack(state, &store, &authority, req),
        ("GET", "/api/v1/workflows") => match store.list_workflow_defs(None) {
            Ok(workflows) => HttpResponse::json(200, json!({ "workflows": workflows })),
            Err(e) => HttpResponse::json(500, json!({ "error": e.to_string() })),
        },
        ("GET", "/api/v1/workflow-runs") => match store.list_workflow_runs(None, None) {
            Ok(runs) => HttpResponse::json(200, json!({ "runs": runs })),
            Err(e) => HttpResponse::json(500, json!({ "error": e.to_string() })),
        },
        ("POST", "/api/v1/workflow-runs") => {
            handle_post_workflow_run_start(state, &store, &authority, req)
        }
        ("GET", "/api/v1/state") => handle_get_state(&store, &authority, req),
        ("GET", "/api/v1/state/search") => handle_get_state_search(&store, &authority, req),
        ("POST", "/api/v1/state") => handle_post_state(&store, &authority, req),
        ("POST", "/api/v1/state/checkpoint") => {
            handle_post_state_checkpoint(state, &store, &authority, req)
        }
        ("POST", "/api/v1/state/import-workspace") => {
            handle_post_state_import_workspace(state, &store, &authority, req)
        }
        ("GET", "/api/v1/stats") => handle_get_stats(&store, req),
        ("GET", "/api/v1/architecture") => handle_get_architecture(state, req),
        ("GET", "/api/v1/gate/secrets") => handle_get_gate_secrets(state),
        ("POST", "/api/v1/gate/secrets") => handle_post_gate_secret(state, &authority, req),
        ("GET", "/api/v1/gate/connectors") => handle_get_gate_connectors(state),
        ("POST", "/api/v1/gate/call") => handle_post_gate_call(state, &authority, req),
        ("POST", "/api/v1/gate/mcp") => handle_post_gate_mcp(state, &authority, None, req),
        _ => handle_dynamic_api_routes(state, &store, &authority, req),
    }
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
        store.as_ref(),
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

fn handle_get_agents(state: &HubState, store: &RedbStore, req: &HttpRequest) -> HttpResponse {
    let project_filter = req.query.get("project").map(String::as_str);
    let include_retired = req
        .query
        .get("include_retired")
        .is_some_and(|v| v == "true" || v == "1");
    let agents = match store.list_agents(project_filter, include_retired) {
        Ok(a) => a,
        Err(e) => return HttpResponse::json(500, json!({ "error": e.to_string() })),
    };
    let areas = match store.list_areas() {
        Ok(a) => a,
        Err(e) => return HttpResponse::json(500, json!({ "error": e.to_string() })),
    };
    let routes = discover_sessions_with_home(store, state.config.home_override.as_deref());
    HttpResponse::json(
        200,
        json!({
            "agents": agents,
            "areas": areas,
            "routes": routes,
        }),
    )
}

fn handle_get_agents_liveness(
    state: &HubState,
    store: &RedbStore,
    req: &HttpRequest,
) -> HttpResponse {
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

    let agents = store.list_agents(None, false).unwrap_or_default();
    let sessions = discover_sessions_with_home(store, state.config.home_override.as_deref());

    let mut items = Vec::with_capacity(agents.len());
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
    store: &RedbStore,
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

    let raw_sender = payload
        .sender_name
        .as_deref()
        .or(payload.from.as_deref())
        .unwrap_or_else(|| authority.actor_name());
    let sender_name = validate_sender_name(raw_sender).unwrap_or_else(|_| "owner".to_string());
    let sender_id = store
        .agent_by_name(&sender_name)
        .ok()
        .flatten()
        .map(|a| a.id);
    let now = stamp::now();

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

    let route_override = state
        .config
        .home_override
        .as_ref()
        .map(|h| h.join(".cache").join("agent-bus").join("routes.json"));
    let mut routes = RouteTable::load(route_override.as_deref());
    let updated = dispatch_message_for_store(
        store,
        &mut routes,
        state.config.home_override.as_deref(),
        &msg,
        now,
    );
    if !updated.is_empty()
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
        .filter(|e| {
            !(e.scope == StateScope::Workspace
                && (e.slug.starts_with("hub-pairing-") || e.slug.starts_with("hub-session-")))
        })
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

fn is_internal_hub_entry(entry: &StateEntry) -> bool {
    entry.scope == StateScope::Workspace
        && (entry.slug.starts_with("hub-pairing-") || entry.slug.starts_with("hub-session-"))
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
    if normalized_slug.starts_with("hub-pairing-") || normalized_slug.starts_with("hub-session-") {
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
    store: &RedbStore,
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
        if normalized.starts_with("hub-pairing-") || normalized.starts_with("hub-session-") {
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
        store,
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
    store: &RedbStore,
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
    let report = apply_workspace_import(store, scan.upserts, scan.skipped);
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

fn handle_dynamic_api_routes(
    state: &HubState,
    store: &RedbStore,
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

    // /api/v1/sessions/:id/transcript, /api/v1/sessions/:id/prompt, or /api/v1/sessions/:id/queue
    if segments.len() == 5
        && segments[0] == "api"
        && segments[1] == "v1"
        && segments[2] == "sessions"
    {
        let session_id = percent_decode(segments[3]);
        match (method, segments[4]) {
            ("GET", "transcript") => {
                drain_ready_session_queues(state, store);
                let (canonical_id, _persona) = resolve_session_target(state, store, &session_id);
                let all_turns = read_session_transcript_with_home(
                    &session_id,
                    store,
                    state.config.home_override.as_deref(),
                );
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
                let is_busy = state.is_session_busy(store, &canonical_id);
                let queue = state.session_queue(&canonical_id);
                return HttpResponse::json(
                    200,
                    json!({
                        "session_id": session_id,
                        "turns": turns,
                        "total_turns": total_turns,
                        "is_busy": is_busy,
                        "queue": queue,
                    }),
                );
            }
            ("GET", "queue") => {
                drain_ready_session_queues(state, store);
                let (canonical_id, _persona) = resolve_session_target(state, store, &session_id);
                let is_busy = state.is_session_busy(store, &canonical_id);
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
                return handle_post_session_prompt(state, store, authority, &session_id, req);
            }
            _ => {}
        }
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
            return handle_post_queued_prompt_send_now(state, store, &session_id, &prompt_id);
        }
        if segments.len() == 6 && method == "DELETE" {
            return handle_delete_queued_prompt(state, store, &session_id, &prompt_id);
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
        match (method, segments[4]) {
            ("POST", "review") => {
                return handle_post_pr_review(store, authority, number, req);
            }
            ("POST", "merge") => {
                return handle_post_pr_merge(store, authority, number, req);
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
        let raw_run_id = percent_decode(segments[3]);
        let run_id = match resolve_workflow_run_id(store, &raw_run_id) {
            Ok(id) => id,
            Err(e) => return HttpResponse::json(404, json!({ "error": e })),
        };

        if segments.len() == 4 && method == "GET" {
            return match store.get_workflow_run(&run_id) {
                Ok(Some(run)) => {
                    let handoffs = list_workflow_handoffs(store, &run_id).unwrap_or_default();
                    let facts = list_workflow_facts(store, &run_id).unwrap_or_default();
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
                    return handle_post_workflow_step(state, store, authority, &run_id, req);
                }
                "handoff" => {
                    return handle_post_workflow_handoff(state, store, authority, &run_id, req);
                }
                "approve" => {
                    return handle_post_workflow_sign_off(
                        state, store, authority, &run_id, true, req,
                    );
                }
                "reject" => {
                    return handle_post_workflow_sign_off(
                        state, store, authority, &run_id, false, req,
                    );
                }
                "abort" => {
                    return handle_post_workflow_abort(state, store, authority, &run_id, req);
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
            return handle_post_gate_bridge_webhook(state, store, authority, &bridge_id, req);
        }
    }

    HttpResponse::json(404, json!({ "error": "not found" }))
}

fn resolve_session_target(
    state: &HubState,
    store: &RedbStore,
    raw_session_id: &str,
) -> (String, Option<String>) {
    let trimmed = raw_session_id.trim();
    let sessions = discover_sessions_with_home(store, state.config.home_override.as_deref());
    if let Some(matched) = sessions
        .iter()
        .find(|s| s.id == trimmed || s.persona.as_deref() == Some(trimmed))
    {
        return (matched.id.clone(), matched.persona.clone());
    }
    (trimmed.to_string(), Some(trimmed.to_ascii_lowercase()))
}

fn dispatch_operator_prompt_for_session(
    state: &HubState,
    persona: Option<&str>,
    fallback_session_id: &str,
    sender_name: &str,
    effective_body: &str,
) -> Result<(), String> {
    let route_override = state
        .config
        .home_override
        .as_ref()
        .map(|h| h.join(".cache").join("agent-bus").join("routes.json"));
    let mut routes = RouteTable::load(route_override.as_deref());
    routes.discover_routes(state.config.home_override.as_deref(), stamp::now());
    let lookup_key = persona.unwrap_or(fallback_session_id);
    if let Some(entry) = routes.get(lookup_key) {
        return dispatch_operator_prompt_to_route(&entry.route, sender_name, effective_body);
    }
    Err(format!("no active vendor route for `{lookup_key}`"))
}

/// Automatically drain and dispatch the next queued operator prompt for any session whose
/// transcript/in-flight state has transitioned from busy to idle.
pub fn drain_ready_session_queues(state: &HubState, store: &RedbStore) {
    for session_id in state.queued_session_keys() {
        if !state.is_session_busy(store, &session_id)
            && let Some((item, queue)) = state.pop_next_queued_prompt(&session_id)
        {
            let delivery_res = dispatch_operator_prompt_for_session(
                state,
                item.persona.as_deref(),
                &item.session_id,
                &item.by,
                &item.effective_body,
            );
            state.mark_session_in_flight(store, &session_id);
            let sent_data = json!({
                "session_id": item.session_id,
                "persona": item.persona,
                "status": "sent",
                "queued": false,
                "is_busy": true,
                "delivered": delivery_res.is_ok(),
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
    store: &RedbStore,
    raw_session_id: &str,
    prompt_id: &str,
) -> HttpResponse {
    let (canonical_id, target_persona) = resolve_session_target(state, store, raw_session_id);
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
    state.mark_session_in_flight(store, &canonical_id);

    let data = json!({
        "session_id": canonical_id,
        "persona": item.persona.clone().or(target_persona.clone()),
        "status": "sent",
        "queued": false,
        "is_busy": true,
        "delivered": delivery_res.is_ok(),
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

fn handle_delete_queued_prompt(
    state: &HubState,
    store: &RedbStore,
    raw_session_id: &str,
    prompt_id: &str,
) -> HttpResponse {
    let (canonical_id, target_persona) = resolve_session_target(state, store, raw_session_id);
    let removed = state
        .remove_queued_prompt(&canonical_id, prompt_id)
        .or_else(|| state.remove_queued_prompt(raw_session_id, prompt_id));
    let Some((item, queue)) = removed else {
        return HttpResponse::json(404, json!({ "error": "queued prompt not found" }));
    };

    let is_busy = state.is_session_busy(store, &canonical_id);
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
    store: &RedbStore,
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

    let sender_raw = payload
        .by
        .as_deref()
        .unwrap_or_else(|| authority.actor_name());
    let sender_name = validate_sender_name(sender_raw).unwrap_or_else(|_| "owner".to_string());

    drain_ready_session_queues(state, store);
    let (canonical_session_id, target_persona) = resolve_session_target(state, store, session_id);
    let busy = state.is_session_busy(store, &canonical_session_id);
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
    state.mark_session_in_flight(store, &canonical_session_id);
    let queue = state.session_queue(&canonical_session_id);

    let data = json!({
        "session_id": canonical_session_id,
        "persona": target_persona,
        "status": "sent",
        "queued": false,
        "is_busy": true,
        "delivered": delivery_res.is_ok(),
        "prompt": prompt_text,
        "effective_body": effective_body,
        "by": sender_name,
        "queue": queue,
        "attachments": saved_attachments,
    });
    state.broadcast("session", "prompt_sent", data.clone());
    HttpResponse::json(200, data)
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
    store: &RedbStore,
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
            let (canonical_id, target_persona) = resolve_session_target(state, store, persona);
            let sender_label = event.sender_name.as_deref().unwrap_or(&event.sender_id);
            let delivery_res = dispatch_operator_prompt_for_session(
                state,
                target_persona.as_deref(),
                &canonical_id,
                sender_label,
                &plan.prompt_body,
            );
            state.mark_session_in_flight(store, &canonical_id);
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
}
