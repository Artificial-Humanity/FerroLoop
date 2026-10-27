//! `fl-hub`: FerroLoop Hub HTTP/1.1 & WebSocket daemon, two-tier authentication
//! (`HubAuthority::Owner` vs. `HubAuthority::Peer`), host session/worktree/architecture
//! discovery, static `FerroDeck` serving, and `/api/v1/*` REST API (spec §2–§4).

pub mod api;
pub mod auth;
pub mod discovery;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fl_core::pr_policy::DEFAULT_OWNER_LOGIN;
use fl_core::store::StoreError;
use fl_store::RedbStore;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::broadcast;

pub use api::{HttpRequest, HttpResponse, handle_http_request, parse_http_request, percent_decode};
pub use auth::{
    AuthManager, DEFAULT_PAIRING_TTL_SECS, DEFAULT_SESSION_TTL_SECS, HubAuthority, PairingInfo,
    SESSION_COOKIE_NAME, constant_time_eq, extract_bearer_token, extract_session_cookie,
    format_logout_set_cookie, format_session_set_cookie, generate_hex_token,
    mint_pairing_code_in_store, pairing_state_slug, resolve_owner_token, resolve_peer_token,
    session_state_slug, sha256_hex,
};
pub use discovery::{
    DiscoveredSessionSummary, TranscriptToolCall, TranscriptTurn, WorkspaceArchScan, WorktreeDiff,
    WorktreeFileStatus, WorktreeSummary, discover_sessions, discover_sessions_with_home,
    list_git_worktrees, parse_jsonl_transcript, read_session_transcript,
    read_session_transcript_with_home, read_worktree_diff, scan_cargo_architecture_conformance,
    scan_workspace_for_arch,
};

/// Default listen address for `fl hub serve` (`127.0.0.1:8420`, spec §2.1).
pub const DEFAULT_HUB_LISTEN: &str = "127.0.0.1:8420";

/// Multiplexed WebSocket frame envelope on `/ws/v1` and `/api/v1/stream` (spec §4.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HubWsFrame {
    pub channel: String,
    pub event: String,
    #[serde(default)]
    pub data: Value,
}

/// Configuration for [`HubState`] and the `fl hub serve` daemon (spec §2.1).
#[derive(Debug, Clone)]
pub struct HubServerConfig {
    pub listen: String,
    pub store_path: PathBuf,
    pub repo_root: PathBuf,
    pub ui_dir: Option<PathBuf>,
    pub owner_login: String,
    pub owner_token: Option<String>,
    pub peer_token: Option<String>,
    pub trust_tailscale_headers: bool,
    pub enable_terminal: bool,
    pub session_ttl_secs: u64,
    pub home_override: Option<PathBuf>,
}

impl Default for HubServerConfig {
    fn default() -> Self {
        Self {
            listen: DEFAULT_HUB_LISTEN.to_string(),
            store_path: PathBuf::from(".fl/store.redb"),
            repo_root: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            ui_dir: None,
            owner_login: DEFAULT_OWNER_LOGIN.to_string(),
            owner_token: None,
            peer_token: None,
            trust_tailscale_headers: false,
            enable_terminal: true,
            session_ttl_secs: DEFAULT_SESSION_TTL_SECS,
            home_override: None,
        }
    }
}

/// Shared Hub server state holding configuration, [`AuthManager`], on-demand `RedbStore`
/// access (so CLI commands like `fl hub pair` can concurrently access the `.redb` file),
/// and the live [`HubWsFrame`] broadcast channel.
#[derive(Debug, Clone)]
pub struct HubState {
    pub config: HubServerConfig,
    pub auth: AuthManager,
    pub ws_tx: broadcast::Sender<HubWsFrame>,
    store_lock: Arc<Mutex<()>>,
}

impl HubState {
    pub fn new(config: HubServerConfig) -> Self {
        let auth = AuthManager::new(
            config.owner_login.clone(),
            config.owner_token.clone(),
            config.peer_token.clone(),
            config.trust_tailscale_headers,
            config.session_ttl_secs,
        );
        let (ws_tx, _rx) = broadcast::channel(256);
        Self {
            config,
            auth,
            ws_tx,
            store_lock: Arc::new(Mutex::new(())),
        }
    }

    /// Open the workspace `RedbStore` for the duration of a single operation, retrying
    /// briefly if an external CLI process holds a transient lock on the `.redb` file.
    pub fn open_store(&self) -> Result<RedbStore, StoreError> {
        let _guard = self.store_lock.lock().ok();
        if let Some(parent) = self.config.store_path.parent()
            && !parent.as_os_str().is_empty()
        {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut last_err = None;
        for attempt in 0..40 {
            match RedbStore::open(&self.config.store_path) {
                Ok(store) => return Ok(store),
                Err(e) => {
                    last_err = Some(e);
                    if attempt < 39 {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                }
            }
        }
        Err(last_err.expect("at least one attempt ran"))
    }

    /// Broadcast a [`HubWsFrame`] to all connected `/ws/v1` clients.
    pub fn broadcast(&self, channel: &str, event: &str, data: Value) {
        let _ = self.ws_tx.send(HubWsFrame {
            channel: channel.to_string(),
            event: event.to_string(),
            data,
        });
    }

    /// Subscribe to live [`HubWsFrame`] events.
    pub fn subscribe(&self) -> broadcast::Receiver<HubWsFrame> {
        self.ws_tx.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::model::State;
    use fl_core::store::{Catalog, WorkflowStore};
    use fl_core::workflow::{
        ApprovalTier, AssigneeRule, END_STEP, EdgeCondition, EdgeDef, JoinPolicy, StepCompletion,
        StepDef, WorkflowDef, WorkflowRun,
    };
    use fl_exec::stamp;
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::fs;
    use tempfile::TempDir;

    fn test_hub_state(tmp: &TempDir) -> HubState {
        let ui_dir = tmp.path().join("ui_dist");
        fs::create_dir_all(&ui_dir).unwrap();
        fs::write(
            ui_dir.join("index.html"),
            "<!doctype html><html><body>FerroDeck SPA Root</body></html>",
        )
        .unwrap();
        fs::write(ui_dir.join("app.js"), "console.log('ferrodeck');").unwrap();

        HubState::new(HubServerConfig {
            listen: "127.0.0.1:0".into(),
            store_path: tmp.path().join("store.redb"),
            repo_root: tmp.path().to_path_buf(),
            ui_dir: Some(ui_dir),
            owner_login: "lmcfarlin".into(),
            owner_token: Some("owner-tok-secret".into()),
            peer_token: Some("peer-tok-secret".into()),
            trust_tailscale_headers: true,
            enable_terminal: true,
            session_ttl_secs: 3600,
            home_override: Some(tmp.path().to_path_buf()),
        })
    }

    #[test]
    fn rest_auth_pairing_session_and_authority_tiers_enforced() {
        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);

        // 1. Health is unauthenticated.
        let health_resp = handle_http_request(&state, &HttpRequest::new("GET", "/api/v1/health"));
        assert_eq!(health_resp.status, 200);
        assert_eq!(health_resp.json_value().unwrap()["status"], "ok");

        // 2. Unauthenticated /api/v1/agents is rejected with 401.
        let unauth = handle_http_request(&state, &HttpRequest::new("GET", "/api/v1/agents"));
        assert_eq!(unauth.status, 401);

        // 3. Mint a pairing code in the store (simulating `fl hub pair` in a separate CLI process)
        // while `HubState` is alive!
        let pair_info = {
            let store = state.open_store().unwrap();
            mint_pairing_code_in_store(&store, "http://127.0.0.1:8420", 600).unwrap()
        };

        // 4. Exchange pairing code at POST /api/v1/auth/pair.
        let pair_req = HttpRequest::new("POST", "/api/v1/auth/pair")
            .with_json_body(&json!({ "code": pair_info.code }));
        let pair_resp = handle_http_request(&state, &pair_req);
        assert_eq!(pair_resp.status, 200);
        let pair_json = pair_resp.json_value().unwrap();
        assert_eq!(pair_json["authenticated"], true);
        assert_eq!(pair_json["authority"]["tier"], "owner");
        let set_cookie = pair_resp
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("set-cookie"))
            .map(|(_, v)| v.clone())
            .unwrap();
        assert!(set_cookie.contains("fl_hub_session="));
        assert!(set_cookie.contains("HttpOnly"));
        assert!(set_cookie.contains("SameSite=Strict"));

        let cookie_pair = set_cookie.split(';').next().unwrap().to_string();

        // 5. Authenticate GET /api/v1/auth/session with the session cookie.
        let sess_req =
            HttpRequest::new("GET", "/api/v1/auth/session").with_header("Cookie", &cookie_pair);
        let sess_resp = handle_http_request(&state, &sess_req);
        assert_eq!(sess_resp.status, 200);
        assert_eq!(
            sess_resp.json_value().unwrap()["authority"]["tier"],
            "owner"
        );

        // 6. Peer token can write non-sensitive state and send wire messages,
        // but is refused (403) on sensitive state writes and Owner-tier workflow sign-off!
        let peer_sensitive_write = HttpRequest::new("POST", "/api/v1/state")
            .with_header("Authorization", "Bearer peer-tok-secret")
            .with_header("X-Fl-Agent", "ferris")
            .with_json_body(&json!({
                "scope": "workspace",
                "slug": "api-secret",
                "sensitive": true,
                "value": { "token": "top-secret-value" }
            }));
        assert_eq!(
            handle_http_request(&state, &peer_sensitive_write).status,
            403
        );

        // Peer cannot forge internal `hub-pairing-*` or `hub-session-*` state entries via POST /api/v1/state:
        for forged_slug in [
            "hub-pairing-forgedcode",
            "HUB-PAIRING-forgedcode",
            "hub-session-forgedhash",
            "  hub-session-forgedhash  ",
        ] {
            let forged_write = HttpRequest::new("POST", "/api/v1/state")
                .with_header("Authorization", "Bearer peer-tok-secret")
                .with_header("X-Fl-Agent", "ferris")
                .with_json_body(&json!({
                    "scope": "workspace",
                    "slug": forged_slug,
                    "value": {
                        "kind": "hub/pairing",
                        "key": "hub/pairing/forgedcode",
                        "code": "forgedcode",
                        "expires_at_ms": u64::MAX,
                        "used": false
                    }
                }));
            assert_eq!(
                handle_http_request(&state, &forged_write).status,
                403,
                "expected 403 for reserved slug {forged_slug:?}"
            );
        }

        // Owner can write sensitive state:
        let owner_sensitive_write = HttpRequest::new("POST", "/api/v1/state")
            .with_header("Cookie", &cookie_pair)
            .with_json_body(&json!({
                "scope": "workspace",
                "slug": "api-secret",
                "sensitive": true,
                "value": { "token": "top-secret-value" }
            }));
        assert_eq!(
            handle_http_request(&state, &owner_sensitive_write).status,
            200
        );

        // Peer reading GET /api/v1/state sees redacted payload for sensitive entry:
        let peer_state_read = HttpRequest::new("GET", "/api/v1/state")
            .with_header("Authorization", "Bearer peer-tok-secret");
        let peer_state_resp = handle_http_request(&state, &peer_state_read);
        assert_eq!(peer_state_resp.status, 200);
        let peer_state_str = String::from_utf8_lossy(&peer_state_resp.body);
        assert!(!peer_state_str.contains("top-secret-value"));
        assert!(peer_state_str.contains("[REDACTED]"));

        // Seed an Owner-tier NeedsHuman workflow run and verify Peer gets 403 while Owner succeeds (200).
        let run_id = {
            let store = state.open_store().unwrap();
            let proj = store
                .add_project(&tmp.path().display().to_string())
                .unwrap();
            let def = WorkflowDef {
                id: fl_core::ids::WorkflowId(stamp::entry_id()),
                project: proj,
                slug: "deploy-flow".into(),
                title: "Deploy Flow".into(),
                start_step: "gate-owner".into(),
                steps: vec![StepDef {
                    step_id: "gate-owner".into(),
                    title: "Owner Gate".into(),
                    prompt: "Await owner".into(),
                    assignee: AssigneeRule::Manual,
                    reviews_step: None,
                    require_different_vendor: false,
                    completion: StepCompletion::HumanSignOff {
                        approval_tier: ApprovalTier::Owner,
                    },
                    join_policy: JoinPolicy::All,
                    max_attempts: 3,
                    max_visits: 3,
                    budget_usd_micros: None,
                    max_tokens: None,
                    isolate_worktree: false,
                }],
                edges: vec![EdgeDef {
                    from_step: "gate-owner".into(),
                    to_step: END_STEP.into(),
                    on: EdgeCondition::Pass,
                }],
                max_total_steps: 10,
                budget_usd_micros: None,
                max_tokens: None,
                declared_keys: BTreeMap::new(),
                authored_at_commit: "0000000".into(),
                authored_by: "ferris".into(),
            };
            store.upsert_workflow_def(def.clone()).unwrap();
            let mut run = WorkflowRun::new(
                fl_core::ids::WorkflowRunId(stamp::entry_id()),
                &def,
                "Run 1",
                stamp::now(),
                "ferris",
            )
            .unwrap();
            run.status = fl_core::workflow::WorkflowRunStatus::NeedsHuman;
            run.steps.get_mut("gate-owner").unwrap().state = State::NeedsHuman;
            let saved = store.create_workflow_run(run).unwrap();
            saved.id
        };

        let approve_path = format!("/api/v1/workflow-runs/{run_id}/approve");
        let peer_approve = HttpRequest::new("POST", &approve_path)
            .with_header("Authorization", "Bearer peer-tok-secret")
            .with_header("X-Fl-Agent", "ferris")
            .with_json_body(&json!({ "step_id": "gate-owner" }));
        assert_eq!(handle_http_request(&state, &peer_approve).status, 403);

        let owner_approve = HttpRequest::new("POST", &approve_path)
            .with_header("Cookie", &cookie_pair)
            .with_json_body(&json!({ "step_id": "gate-owner", "note": "Ship it" }));
        let owner_approve_resp = handle_http_request(&state, &owner_approve);
        assert_eq!(owner_approve_resp.status, 200);
        assert_eq!(
            owner_approve_resp.json_value().unwrap()["run"]["status"],
            "completed"
        );
    }

    #[test]
    fn static_ferrodeck_serving_spa_fallback_and_traversal_rejection() {
        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);

        // Exact static asset
        let js_resp = handle_http_request(&state, &HttpRequest::new("GET", "/app.js"));
        assert_eq!(js_resp.status, 200);
        assert!(js_resp.content_type.starts_with("application/javascript"));
        assert_eq!(js_resp.body, b"console.log('ferrodeck');");

        // SPA fallback to index.html
        let spa_resp = handle_http_request(&state, &HttpRequest::new("GET", "/workflows/runs"));
        assert_eq!(spa_resp.status, 200);
        assert!(spa_resp.content_type.starts_with("text/html"));
        assert!(String::from_utf8_lossy(&spa_resp.body).contains("FerroDeck SPA Root"));

        // Path traversal rejected with 400 Bad Request
        for bad_target in [
            "/../secret",
            "/..%2fsecret",
            "/%2e%2e/etc/passwd",
            "/a\\..\\b",
        ] {
            let bad_resp = handle_http_request(&state, &HttpRequest::new("GET", bad_target));
            assert_eq!(bad_resp.status, 400, "expected 400 for {bad_target}");
        }
    }

    #[test]
    fn rest_agents_wire_arch_and_sessions_endpoints_round_trip() {
        use fl_core::agent::{AgentRole, RegisterAgentArgs};
        use fl_core::store::AgentRegistry;

        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);

        // Register two agents in the store and verify GET /api/v1/agents
        {
            let store = state.open_store().unwrap();
            for (name, role) in [
                ("Ferris", AgentRole::Lead),
                ("Felicia", AgentRole::Assistant),
            ] {
                store
                    .register_agent(RegisterAgentArgs {
                        display_name: name.into(),
                        email: None,
                        project: "ferroloop".into(),
                        role,
                        extra_areas: vec![],
                        create_area: true,
                        replace: true,
                        at: stamp::now(),
                        by: "owner".into(),
                    })
                    .unwrap();
            }
        }

        let agents_resp = handle_http_request(
            &state,
            &HttpRequest::new("GET", "/api/v1/agents")
                .with_header("Authorization", "Bearer owner-tok-secret"),
        );
        assert_eq!(agents_resp.status, 200);
        assert_eq!(
            agents_resp.json_value().unwrap()["agents"]
                .as_array()
                .unwrap()
                .len(),
            2
        );

        // Send a wire message from Ferris to Felicia and verify /api/v1/wire/messages + /api/v1/wire/ack
        let send_req = HttpRequest::new("POST", "/api/v1/wire/send")
            .with_header("Authorization", "Bearer peer-tok-secret")
            .with_header("X-Fl-Agent", "ferris")
            .with_json_body(&json!({
                "from": "ferris",
                "to": ["felicia"],
                "subject": "Task 2 ready",
                "body": "Please review crates/hub"
            }));
        let send_resp = handle_http_request(&state, &send_req);
        assert_eq!(send_resp.status, 200);
        let msg_id = send_resp.json_value().unwrap()["message"]["id"]
            .as_str()
            .unwrap()
            .to_string();

        let mut inbox_req = HttpRequest::new("GET", "/api/v1/wire/messages")
            .with_header("Authorization", "Bearer peer-tok-secret");
        inbox_req.query.insert("to".into(), "felicia".into());
        inbox_req.query.insert("unread".into(), "true".into());
        let inbox_resp = handle_http_request(&state, &inbox_req);
        assert_eq!(inbox_resp.status, 200);
        assert_eq!(
            inbox_resp.json_value().unwrap()["messages"]
                .as_array()
                .unwrap()
                .len(),
            1
        );

        let ack_req = HttpRequest::new("POST", "/api/v1/wire/ack")
            .with_header("Authorization", "Bearer peer-tok-secret")
            .with_header("X-Fl-Agent", "felicia")
            .with_json_body(&json!({ "id": msg_id, "recipient": "felicia" }));
        assert_eq!(handle_http_request(&state, &ack_req).status, 200);

        // Write `.fl/architecture.toml` and verify GET /api/v1/architecture
        let fl_dir = tmp.path().join(".fl");
        fs::create_dir_all(&fl_dir).unwrap();
        fs::write(
            fl_dir.join("architecture.toml"),
            "system_id = \"ferroloop\"\ntitle = \"FerroLoop\"\n\n[[components]]\nid = \"fl_core\"\ntitle = \"fl-core\"\nkind = \"crate\"\npackage = \"fl-core\"\npaths = [\"crates/core/**\"]\n",
        )
        .unwrap();

        let arch_resp = handle_http_request(
            &state,
            &HttpRequest::new("GET", "/api/v1/architecture")
                .with_header("Authorization", "Bearer owner-tok-secret"),
        );
        assert_eq!(arch_resp.status, 200);
        assert!(
            arch_resp.json_value().unwrap()["likec4_dsl"]
                .as_str()
                .unwrap()
                .contains("specification")
        );

        // GET /api/v1/architecture?path=... rejects path traversal and absolute paths with 400 Bad Request
        for bad_arch_path in [
            "../Cargo.toml",
            "/etc/passwd",
            "./.fl/architecture.toml",
            "a\\..\\b",
        ] {
            let mut bad_arch_req = HttpRequest::new("GET", "/api/v1/architecture")
                .with_header("Authorization", "Bearer owner-tok-secret");
            bad_arch_req
                .query
                .insert("path".into(), bad_arch_path.into());
            let bad_arch_resp = handle_http_request(&state, &bad_arch_req);
            assert_eq!(
                bad_arch_resp.status, 400,
                "expected 400 for architecture path {bad_arch_path}"
            );
        }
    }
}
