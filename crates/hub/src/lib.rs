//! `fl-hub`: FerroLoop Hub HTTP/1.1 & WebSocket daemon, two-tier authentication
//! (`HubAuthority::Owner` vs. `HubAuthority::Peer`), host session/worktree/architecture
//! discovery, static `FerroDeck` serving, and `/api/v1/*` REST API (spec §2–§4).

pub mod api;
mod artifacts;
pub mod auth;
pub mod commands;
mod direct_quota;
pub mod discovery;
pub mod lifecycle;
pub mod nodes;
pub mod session_actions;
mod settings;
pub mod terminal;
pub mod ws;

use std::collections::BTreeMap;
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fl_core::pr_policy::DEFAULT_OWNER_LOGIN;
use fl_core::store::StoreError;
use fl_exec::stamp;
use fl_store::RedbStore;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::broadcast;

pub use api::{
    HttpRequest, HttpResponse, drain_ready_session_queues, handle_http_request, parse_http_request,
    percent_decode,
};
pub use auth::{
    AuthManager, DEFAULT_PAIRING_TTL_SECS, DEFAULT_SESSION_TTL_SECS, HubAuthority, PairingInfo,
    SESSION_COOKIE_NAME, constant_time_eq, extract_bearer_token, extract_session_cookie,
    format_logout_set_cookie, format_session_set_cookie, generate_hex_token,
    mint_pairing_code_in_store, pairing_state_slug, resolve_owner_token, resolve_peer_token,
    session_state_slug, sha256_hex,
};
pub use discovery::{
    DiscoveredSessionSummary, QuotaWindowUsage, SessionContextUsage, SessionQuotaUsage,
    TranscriptToolCall, TranscriptTurn, WorkspaceArchScan, WorktreeDiff, WorktreeFileStatus,
    WorktreeSummary, discover_sessions, discover_sessions_with_home, is_transcript_busy,
    list_git_worktrees, parse_jsonl_transcript, read_session_context_usage_with_home,
    read_session_quota_usage_with_home, read_session_transcript, read_session_transcript_with_home,
    read_worktree_diff, scan_cargo_architecture_conformance, scan_workspace_for_arch,
};
pub use nodes::{
    EnrollFail, EnrollTokenInfo, EnrollWorker, EnrolledWorker, EnsureHubArgs, HeartbeatFail,
    HeartbeatUpdate, NodeRecord, NodeView, RevokeFail, canonical_node_id, detect_capabilities,
    enroll_worker, ensure_hub_node, find_hub_node, list_visible_nodes, local_hostname,
    mint_enroll_token, node_id_from_hostname, node_status, node_token_matches, revoke_node,
    touch_node,
};
pub use terminal::{TerminalManager, TerminalSession};
pub use ws::{HubServer, HubWsClient, compute_accept_key, try_parse_ws_frame};

/// Default listen address for `fl hub serve` (`127.0.0.1:8420`, spec §2.1).
pub const DEFAULT_HUB_LISTEN: &str = "127.0.0.1:8420";

/// Saved file/image attachment metadata associated with an operator prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedPromptAttachment {
    pub name: String,
    pub media_type: String,
    pub path: String,
    pub url: String,
}

/// A queued operator chat prompt waiting for a busy session to become idle (or for "Send Now").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedOperatorPrompt {
    pub id: String,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persona: Option<String>,
    pub prompt: String,
    pub effective_body: String,
    pub by: String,
    #[serde(default)]
    pub attachments: Vec<SavedPromptAttachment>,
    pub created_at: String,
}

#[derive(Debug, Clone)]
struct InFlightPrompt {
    baseline_turn_count: usize,
    baseline_busy: bool,
    dispatched_at_ms: i64,
}

#[derive(Debug, Default)]
struct PromptQueueState {
    queues: BTreeMap<String, Vec<QueuedOperatorPrompt>>,
    in_flight: BTreeMap<String, InFlightPrompt>,
    next_seq: u64,
}

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
    prompt_queues: Arc<Mutex<PromptQueueState>>,
    model_overrides: Arc<Mutex<BTreeMap<String, session_actions::ModelOverride>>>,
}

/// RAII guard holding a [`RedbStore`] together with the `store_lock` mutex for
/// the store's whole lifetime, so concurrent in-process opens are serialized.
pub struct HubStoreGuard<'a> {
    store: Option<RedbStore>,
    _guard: Option<std::sync::MutexGuard<'a, ()>>,
}

impl Deref for HubStoreGuard<'_> {
    type Target = RedbStore;

    fn deref(&self) -> &RedbStore {
        self.store
            .as_ref()
            .expect("store present while guard is alive")
    }
}

impl Drop for HubStoreGuard<'_> {
    fn drop(&mut self) {
        // Release the redb file lock before the mutex guard unlocks.
        self.store.take();
    }
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
            prompt_queues: Arc::new(Mutex::new(PromptQueueState::default())),
            model_overrides: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// Remember the model an operator chose. `key` is the discovered session id when discovery
    /// lists the session, else the lowercased persona: [`session_actions::apply_model_overrides`]
    /// matches a summary on its id first and its persona second, so either key applies. The map
    /// is in memory only: it is lost when the hub restarts, after which discovery reports what
    /// the vendor session itself says, and an override is dropped by
    /// [`HubState::clear_session_model_override`] when its session is cleared, reforged,
    /// spun up again or torn down. `GET /api/v1/sessions` overlays it on every summary.
    pub fn set_session_model_override(
        &self,
        session_id: &str,
        model_override: session_actions::ModelOverride,
    ) {
        let mut guard = self
            .model_overrides
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        guard.insert(session_id.to_string(), model_override);
    }

    /// Drop the override recorded under `key`, or under its lowercased form (the persona key).
    /// A session that is cleared, reforged, spun up or torn down no longer runs the model the
    /// operator chose for its previous run, so the override must not mask what it reports now.
    pub fn clear_session_model_override(&self, key: &str) {
        let mut guard = self
            .model_overrides
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        guard.remove(key);
        guard.remove(&key.to_ascii_lowercase());
    }

    pub fn session_model_override(
        &self,
        session_id: &str,
    ) -> Option<session_actions::ModelOverride> {
        let guard = self
            .model_overrides
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        guard.get(session_id).cloned()
    }

    /// Session summaries with the in-memory model overrides applied.
    pub fn with_model_overrides(
        &self,
        mut sessions: Vec<DiscoveredSessionSummary>,
    ) -> Vec<DiscoveredSessionSummary> {
        let guard = self
            .model_overrides
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        session_actions::apply_model_overrides(&mut sessions, &guard);
        sessions
    }

    /// Forget the in-flight marker for `session_key`, so the session no longer reads as busy
    /// because of a prompt the operator just interrupted. Queued prompts stay queued.
    pub fn clear_session_in_flight(&self, session_key: &str) {
        let mut guard = self.prompt_queues.lock().unwrap_or_else(|e| e.into_inner());
        guard.in_flight.remove(session_key);
    }

    pub fn session_queue(&self, session_key: &str) -> Vec<QueuedOperatorPrompt> {
        let Ok(guard) = self.prompt_queues.lock() else {
            return Vec::new();
        };
        guard.queues.get(session_key).cloned().unwrap_or_default()
    }

    /// Account quota for `session_id`, including the 60-second direct vendor probe cache.
    pub fn session_quota_usage(&self, session_id: &str) -> Option<SessionQuotaUsage> {
        discovery::read_session_quota_usage_with_home(
            session_id,
            self.config.home_override.as_deref(),
        )
    }

    pub fn queued_session_keys(&self) -> Vec<String> {
        let Ok(guard) = self.prompt_queues.lock() else {
            return Vec::new();
        };
        guard
            .queues
            .iter()
            .filter_map(|(k, v)| (!v.is_empty()).then_some(k.clone()))
            .collect()
    }

    pub fn enqueue_session_prompt(
        &self,
        session_key: &str,
        persona: Option<String>,
        prompt: String,
        effective_body: String,
        by: String,
        attachments: Vec<SavedPromptAttachment>,
    ) -> (QueuedOperatorPrompt, Vec<QueuedOperatorPrompt>) {
        let now = stamp::now();
        let now_ms = now.unix_millis();
        let mut guard = self.prompt_queues.lock().unwrap_or_else(|e| e.into_inner());
        guard.next_seq += 1;
        let seq = guard.next_seq;
        let item = QueuedOperatorPrompt {
            id: format!("qp-{now_ms}-{seq}"),
            session_id: session_key.to_string(),
            persona,
            prompt,
            effective_body,
            by,
            attachments,
            created_at: now.to_string(),
        };
        let queue = guard.queues.entry(session_key.to_string()).or_default();
        queue.push(item.clone());
        let snapshot = queue.clone();
        (item, snapshot)
    }

    pub fn pop_next_queued_prompt(
        &self,
        session_key: &str,
    ) -> Option<(QueuedOperatorPrompt, Vec<QueuedOperatorPrompt>)> {
        let mut guard = self.prompt_queues.lock().unwrap_or_else(|e| e.into_inner());
        let queue = guard.queues.get_mut(session_key)?;
        if queue.is_empty() {
            return None;
        }
        let item = queue.remove(0);
        let snapshot = queue.clone();
        Some((item, snapshot))
    }

    /// Drop queued operator prompts and the in-flight marker for `session_key`.
    /// A clear or reforge must not drain prompts that were waiting on the old context.
    pub fn clear_session_prompt_queue(&self, session_key: &str) {
        let mut guard = self.prompt_queues.lock().unwrap_or_else(|e| e.into_inner());
        guard.queues.remove(session_key);
        guard.in_flight.remove(session_key);
    }

    pub fn remove_queued_prompt(
        &self,
        session_key: &str,
        prompt_id: &str,
    ) -> Option<(QueuedOperatorPrompt, Vec<QueuedOperatorPrompt>)> {
        let mut guard = self.prompt_queues.lock().unwrap_or_else(|e| e.into_inner());
        let queue = guard.queues.get_mut(session_key)?;
        let pos = queue.iter().position(|item| item.id == prompt_id)?;
        let item = queue.remove(pos);
        let snapshot = queue.clone();
        Some((item, snapshot))
    }

    pub fn update_queued_prompt(
        &self,
        session_key: &str,
        prompt_id: &str,
        new_prompt: String,
    ) -> Option<(QueuedOperatorPrompt, Vec<QueuedOperatorPrompt>)> {
        let mut guard = self.prompt_queues.lock().unwrap_or_else(|e| e.into_inner());
        let queue = guard.queues.get_mut(session_key)?;
        let item = queue.iter_mut().find(|item| item.id == prompt_id)?;
        let mut effective_body = new_prompt.clone();
        for saved in &item.attachments {
            effective_body.push_str(&format!(
                "\n\n[Attached file: {} ({})]",
                saved.path, saved.url
            ));
        }
        item.prompt = new_prompt;
        item.effective_body = effective_body;
        let updated = item.clone();
        let snapshot = queue.clone();
        Some((updated, snapshot))
    }

    pub fn is_session_busy(&self, store: &RedbStore, session_id: &str) -> bool {
        let turns = read_session_transcript_with_home(
            session_id,
            store,
            self.config.home_override.as_deref(),
        );
        self.is_session_busy_from_turns(session_id, &turns)
    }

    /// Busy check from turns already read. Does not open the store or the transcript file.
    pub fn is_session_busy_from_turns(&self, session_id: &str, turns: &[TranscriptTurn]) -> bool {
        let transcript_busy = is_transcript_busy(turns);
        let now_ms = stamp::now().unix_millis() as i64;
        let mut guard = self.prompt_queues.lock().unwrap_or_else(|e| e.into_inner());
        let in_flight_active = if let Some(in_flight) = guard.in_flight.get(session_id) {
            let expired = now_ms - in_flight.dispatched_at_ms > 300_000;
            // An empty slice is not a transcript observation. Do not clear in-flight
            // just because the caller has no turns yet.
            let completed = !turns.is_empty()
                && (turns.len() > in_flight.baseline_turn_count || in_flight.baseline_busy)
                && !transcript_busy;
            if expired || completed {
                guard.in_flight.remove(session_id);
                false
            } else {
                true
            }
        } else {
            false
        };
        in_flight_active || transcript_busy
    }

    /// Open the store, snapshot attempt rows, drop the guard, then read the transcript.
    /// A failed open still reads vendor files against an empty attempt snapshot.
    pub fn is_session_busy_released(&self, session_id: &str) -> bool {
        let snap = match self.open_store() {
            Ok(store) => discovery::snapshot_discovery_store(&store),
            Err(_) => discovery::DiscoveryStoreSnapshot::default(),
        };
        let turns = discovery::read_session_transcript_from_snapshot(
            session_id,
            &snap,
            self.config.home_override.as_deref(),
        );
        self.is_session_busy_from_turns(session_id, &turns)
    }

    pub fn mark_session_in_flight(&self, store: &RedbStore, session_key: &str) {
        let turns = read_session_transcript_with_home(
            session_key,
            store,
            self.config.home_override.as_deref(),
        );
        self.mark_session_in_flight_from_turns(session_key, &turns);
    }

    /// Record an in-flight prompt from turns already read. Does not touch the store.
    pub fn mark_session_in_flight_from_turns(&self, session_key: &str, turns: &[TranscriptTurn]) {
        let baseline_busy = is_transcript_busy(turns);
        let now_ms = stamp::now().unix_millis() as i64;
        let mut guard = self.prompt_queues.lock().unwrap_or_else(|e| e.into_inner());
        guard.in_flight.insert(
            session_key.to_string(),
            InFlightPrompt {
                baseline_turn_count: turns.len(),
                baseline_busy,
                dispatched_at_ms: now_ms,
            },
        );
    }

    /// Open the store, snapshot, drop the guard, then read the transcript and mark in-flight.
    /// A failed open still reads vendor files and records in-flight.
    pub fn mark_session_in_flight_released(&self, session_key: &str) {
        let snap = match self.open_store() {
            Ok(store) => discovery::snapshot_discovery_store(&store),
            Err(_) => discovery::DiscoveryStoreSnapshot::default(),
        };
        let turns = discovery::read_session_transcript_from_snapshot(
            session_key,
            &snap,
            self.config.home_override.as_deref(),
        );
        self.mark_session_in_flight_from_turns(session_key, &turns);
    }

    /// Open the workspace `RedbStore` for the duration of a single operation, retrying
    /// briefly if an external CLI process holds a transient lock on the `.redb` file.
    /// The returned [`HubStoreGuard`] holds `store_lock` for the store's whole
    /// lifetime, serializing concurrent in-process opens.
    pub fn open_store(&self) -> Result<HubStoreGuard<'_>, StoreError> {
        let guard = Some(self.store_lock.lock().unwrap_or_else(|e| e.into_inner()));
        if let Some(parent) = self.config.store_path.parent()
            && !parent.as_os_str().is_empty()
        {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut last_err = None;
        for attempt in 0..200 {
            match RedbStore::open(&self.config.store_path) {
                Ok(store) => {
                    return Ok(HubStoreGuard {
                        store: Some(store),
                        _guard: guard,
                    });
                }
                Err(e) => {
                    last_err = Some(e);
                    if attempt < 199 {
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
    use serde_json::{Value, json};
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

        // Peer cannot forge internal `hub-pairing-*`, `hub-session-*`, or `hub-node-*` entries.
        for forged_slug in [
            "hub-pairing-forgedcode",
            "HUB-PAIRING-forgedcode",
            "hub-session-forgedhash",
            "  hub-session-forgedhash  ",
            "hub-node-forgedid",
            "HUB-NODE-forgedid",
            "  hub-node-forgedid  ",
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
    fn state_search_route_ranks_redacts_and_validates() {
        use fl_core::state::{
            MemoryBasis, MemoryCategory, MemoryEntry, MemoryLifecycle, SensitivityPatch,
            StatePayload, StateScope, UpsertStateArgs,
        };
        use fl_core::store::StateStore;

        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);
        {
            let store = state.open_store().unwrap();
            for (slug, body, sensitive) in [
                ("weak-note", "zephyr appears once", false),
                ("secret-note", "zephyr secretbodytoken zephyr zephyr", true),
            ] {
                store
                    .upsert_state(UpsertStateArgs {
                        scope: StateScope::Workspace,
                        slug: slug.into(),
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
                            summary: format!("{slug} summary"),
                            body: body.into(),
                        }),
                        at: stamp::now(),
                        by: "test".into(),
                    })
                    .unwrap();
            }
        }

        // Peer: ranked shape, sensitive snippet/entry redacted.
        let peer_resp = handle_http_request(
            &state,
            &HttpRequest::new("GET", "/api/v1/state/search?q=zephyr")
                .with_header("Authorization", "Bearer peer-tok-secret"),
        );
        assert_eq!(peer_resp.status, 200);
        let peer_json = peer_resp.json_value().unwrap();
        assert_eq!(peer_json["query"], "zephyr");
        assert_eq!(peer_json["total_hits"], 2);
        assert_eq!(peer_json["hits"].as_array().unwrap().len(), 2);
        let peer_str = String::from_utf8_lossy(&peer_resp.body);
        assert!(!peer_str.contains("secretbodytoken"), "{peer_str}");
        assert!(peer_str.contains("[REDACTED]"), "{peer_str}");

        // Owner sees the full sensitive hit.
        let owner_resp = handle_http_request(
            &state,
            &HttpRequest::new("GET", "/api/v1/state/search?q=zephyr")
                .with_header("Authorization", "Bearer owner-tok-secret"),
        );
        assert_eq!(owner_resp.status, 200);
        assert!(
            String::from_utf8_lossy(&owner_resp.body).contains("secretbodytoken"),
            "owner must see full sensitive hit"
        );

        // limit=0 keeps total_hits; missing q is an empty 200, not a 500.
        let zero_resp = handle_http_request(
            &state,
            &HttpRequest::new("GET", "/api/v1/state/search?q=zephyr&limit=0")
                .with_header("Authorization", "Bearer peer-tok-secret"),
        );
        assert_eq!(zero_resp.status, 200);
        let zero_json = zero_resp.json_value().unwrap();
        assert_eq!(zero_json["total_hits"], 2);
        assert_eq!(zero_json["hits"].as_array().unwrap().len(), 0);

        let blank_resp = handle_http_request(
            &state,
            &HttpRequest::new("GET", "/api/v1/state/search")
                .with_header("Authorization", "Bearer peer-tok-secret"),
        );
        assert_eq!(blank_resp.status, 200);
        assert_eq!(blank_resp.json_value().unwrap()["total_hits"], 0);

        // Bad scope/concern/limit are 400s.
        for target in [
            "/api/v1/state/search?q=zephyr&scope=bogus%3A1",
            "/api/v1/state/search?q=zephyr&concern=bogus",
            "/api/v1/state/search?q=zephyr&limit=abc",
            "/api/v1/state/search?q=zephyr&limit=-1",
        ] {
            let resp = handle_http_request(
                &state,
                &HttpRequest::new("GET", target)
                    .with_header("Authorization", "Bearer peer-tok-secret"),
            );
            assert_eq!(resp.status, 400, "{target}");
            assert!(resp.json_value().unwrap()["error"].is_string(), "{target}");
        }
    }

    #[test]
    fn state_import_workspace_is_owner_only_with_exact_three_field_shape() {
        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);
        let root = tmp.path();
        let mem = root.join(".agents/memory/keep-this.md");
        fs::create_dir_all(mem.parent().unwrap()).unwrap();
        fs::write(
            &mem,
            "---\ndescription: Keep this rule\nmetadata:\n  type: feedback\n---\nBody text.\n",
        )
        .unwrap();
        let state_md = root.join("Notes/Demo.io/STATE.md");
        fs::create_dir_all(state_md.parent().unwrap()).unwrap();
        fs::write(
            &state_md,
            "# Demo State\n\n## Shipped\n\n- Did a thing\n\n## Next steps\n\n- Do the next thing\n",
        )
        .unwrap();

        // Peer is refused before anything is written.
        let peer_resp = handle_http_request(
            &state,
            &HttpRequest::new("POST", "/api/v1/state/import-workspace")
                .with_header("Authorization", ["Bearer ", "peer", "-tok-secret"].concat())
                .with_json_body(&json!({ "workspace_root": root.to_string_lossy() })),
        );
        assert_eq!(peer_resp.status, 403);

        // Owner import returns exactly the three documented fields.
        let owner_resp = handle_http_request(
            &state,
            &HttpRequest::new("POST", "/api/v1/state/import-workspace")
                .with_header(
                    "Authorization",
                    ["Bearer ", "owner", "-tok-secret"].concat(),
                )
                .with_json_body(
                    &json!({ "workspace_root": root.to_string_lossy(), "by": "tester" }),
                ),
        );
        assert_eq!(
            owner_resp.status,
            200,
            "{}",
            String::from_utf8_lossy(&owner_resp.body)
        );
        let body = owner_resp.json_value().unwrap();
        let obj = body.as_object().unwrap();
        assert_eq!(obj.len(), 3, "{body}");
        assert_eq!(obj["imported_memories"], 1);
        assert_eq!(obj["imported_handoffs"], 1);
        assert_eq!(obj["scopes"], json!(["project:demo-io", "workspace"]));
        assert!(!obj.contains_key("skipped"));

        // Wrong-typed fields and non-object bodies are 400s (owner).
        for bad in [
            json!({ "workspace_root": 42 }),
            json!({ "by": true }),
            json!(["nope"]),
        ] {
            let resp = handle_http_request(
                &state,
                &HttpRequest::new("POST", "/api/v1/state/import-workspace")
                    .with_header(
                        "Authorization",
                        ["Bearer ", "owner", "-tok-secret"].concat(),
                    )
                    .with_json_body(&bad),
            );
            assert_eq!(resp.status, 400, "{bad}");
        }
    }

    #[test]
    fn state_checkpoint_route_reports_git_state_and_enforces_protections() {
        use fl_core::state::{
            HandoffEntry, SensitivityPatch, StatePayload, StateScope, UpsertStateArgs,
        };
        use fl_core::store::StateStore;

        fn hub_git(repo: &std::path::Path, args: &[&str]) {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }

        let tmp = TempDir::new().unwrap();
        // Isolated repo subdir: hub store/ui files live outside it, so the
        // dirty count is exact and the derived scope is deterministic.
        let repo = tmp.path().join("hubproj");
        fs::create_dir_all(&repo).unwrap();
        let repo = repo.as_path();
        hub_git(repo, &["init", "-b", "main"]);
        hub_git(repo, &["config", "user.name", "Checkpoint Tester"]);
        hub_git(repo, &["config", "user.email", "tester@example.invalid"]);
        fs::write(repo.join("tracked.txt"), "initial\n").unwrap();
        hub_git(repo, &["add", "tracked.txt"]);
        hub_git(repo, &["commit", "-m", "hub checkpoint subject"]);
        fs::write(repo.join("tracked.txt"), "modified\n").unwrap();

        let mut state = test_hub_state(&tmp);
        state.config.repo_root = repo.to_path_buf();
        let owner = ["Bearer ", "owner", "-tok-secret"].concat();
        let peer = ["Bearer ", "peer", "-tok-secret"].concat();

        // Success: exactly the shared report shape.
        let resp = handle_http_request(
            &state,
            &HttpRequest::new("POST", "/api/v1/state/checkpoint")
                .with_header("Authorization", &owner)
                .with_json_body(&json!({ "scope": "workspace", "by": "tester" })),
        );
        assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
        let body = resp.json_value().unwrap();
        let obj = body.as_object().unwrap();
        assert_eq!(obj.len(), 4, "{body}");
        assert_eq!(body["branch"], "main");
        assert_eq!(body["head_sha"].as_str().unwrap().len(), 40);
        assert_eq!(body["dirty_files"], 1, "{body}");
        assert_eq!(body["entry"]["slug"], "checkpoint-tester");
        assert_eq!(body["entry"]["scope"], "workspace");
        assert_eq!(body["entry"]["concern"], "handoff");

        // Empty body works and defaults `by` to the caller's actor name.
        let empty_resp = handle_http_request(
            &state,
            &HttpRequest::new("POST", "/api/v1/state/checkpoint")
                .with_header("Authorization", &owner),
        );
        assert_eq!(empty_resp.status, 200);
        let empty_body = empty_resp.json_value().unwrap();
        assert_eq!(empty_body["entry"]["by"], "lmcfarlin");
        // Default scope derives from the probed repo top-level name.
        assert_eq!(
            empty_body["entry"]["scope"], "project:hubproj",
            "{empty_body}"
        );
        assert_eq!(empty_body["dirty_files"], 1, "{empty_body}");

        // Wrong-typed fields and non-object bodies are 400s.
        for bad in [
            json!({ "slug": 42 }),
            json!({ "scope": "bogus:1" }),
            json!(["x"]),
        ] {
            let resp = handle_http_request(
                &state,
                &HttpRequest::new("POST", "/api/v1/state/checkpoint")
                    .with_header("Authorization", &owner)
                    .with_json_body(&bad),
            );
            assert_eq!(resp.status, 400, "{bad}");
        }

        // Reserved hub slugs are 403 for owner and peer alike.
        for slug in [
            "hub-pairing-forged",
            "hub-session-forged",
            "hub-node-forged",
        ] {
            for auth in [&owner, &peer] {
                let resp = handle_http_request(
                    &state,
                    &HttpRequest::new("POST", "/api/v1/state/checkpoint")
                        .with_header("Authorization", auth)
                        .with_json_body(&json!({ "slug": slug })),
                );
                assert_eq!(resp.status, 403, "{slug}");
            }
        }

        // A peer cannot overwrite an existing sensitive checkpoint.
        {
            let store = state.open_store().unwrap();
            store
                .upsert_state(UpsertStateArgs {
                    scope: StateScope::Workspace,
                    slug: "checkpoint-sensitive".into(),
                    sensitivity: SensitivityPatch::SetSensitive,
                    payload: StatePayload::Handoff(HandoffEntry {
                        summary: "secret checkpoint".into(),
                        shipped: Vec::new(),
                        in_flight: Vec::new(),
                        blockers: Vec::new(),
                        next_steps: Vec::new(),
                        notes: None,
                    }),
                    at: stamp::now(),
                    by: "owner".into(),
                })
                .unwrap();
        }
        let peer_resp = handle_http_request(
            &state,
            &HttpRequest::new("POST", "/api/v1/state/checkpoint")
                .with_header("Authorization", &peer)
                .with_header("X-Fl-Agent", "ferris")
                .with_json_body(&json!({ "scope": "workspace", "slug": "checkpoint-sensitive" })),
        );
        assert_eq!(peer_resp.status, 403);
        // The owner still can.
        let owner_resp = handle_http_request(
            &state,
            &HttpRequest::new("POST", "/api/v1/state/checkpoint")
                .with_header("Authorization", &owner)
                .with_json_body(&json!({ "scope": "workspace", "slug": "checkpoint-sensitive" })),
        );
        assert_eq!(owner_resp.status, 200);
    }

    #[test]
    fn state_checkpoint_route_non_repo_is_500() {
        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);
        let owner = ["Bearer ", "owner", "-tok-secret"].concat();
        // Invalid explicit scope/slug validate before the git probe: 400
        // (reserved slug 403) even though `repo_root` is not a repository.
        for (bad, expected) in [
            (json!({ "scope": "bogus:1" }), 400),
            (json!({ "slug": "1bad slug!" }), 400),
            (json!({ "slug": "hub-pairing-forged" }), 403),
            (json!({ "slug": "hub-session-forged" }), 403),
            (json!({ "slug": "hub-node-forged" }), 403),
        ] {
            let resp = handle_http_request(
                &state,
                &HttpRequest::new("POST", "/api/v1/state/checkpoint")
                    .with_header("Authorization", &owner)
                    .with_json_body(&bad),
            );
            assert_eq!(resp.status, expected, "{bad}");
        }
        let resp = handle_http_request(
            &state,
            &HttpRequest::new("POST", "/api/v1/state/checkpoint")
                .with_header("Authorization", &owner)
                .with_json_body(&json!({})),
        );
        assert_eq!(resp.status, 500);
        assert!(resp.json_value().unwrap()["error"].is_string());
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

    fn send_http_over_tcp(addr: std::net::SocketAddr, req: &HttpRequest) -> HttpResponse {
        use std::io::{Read, Write};
        use std::net::TcpStream;

        let mut tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(3)).unwrap();
        tcp.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        tcp.set_write_timeout(Some(Duration::from_secs(3))).unwrap();

        let mut raw = format!(
            "{} {} HTTP/1.1\r\nHost: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
            req.method,
            req.raw_target,
            addr,
            req.body.len()
        );
        for (k, v) in &req.headers {
            raw.push_str(&format!("{k}: {v}\r\n"));
        }
        raw.push_str("\r\n");

        tcp.write_all(raw.as_bytes()).unwrap();
        if !req.body.is_empty() {
            tcp.write_all(&req.body).unwrap();
        }
        tcp.flush().unwrap();

        let mut resp_bytes = Vec::new();
        tcp.read_to_end(&mut resp_bytes).unwrap();
        let header_end = resp_bytes
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("HTTP response header terminator");
        let head_str = String::from_utf8_lossy(&resp_bytes[..header_end]);
        let mut lines = head_str.lines();
        let status_line = lines.next().unwrap();
        let status: u16 = status_line
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        let mut headers = Vec::new();
        let mut content_type = String::new();
        for line in lines {
            if let Some((k, v)) = line.split_once(':') {
                let k_trim = k.trim().to_string();
                let v_trim = v.trim().to_string();
                if k_trim.eq_ignore_ascii_case("content-type") {
                    content_type = v_trim.clone();
                }
                headers.push((k_trim, v_trim));
            }
        }
        let body = resp_bytes[header_end + 4..].to_vec();
        HttpResponse {
            status,
            status_text: "OK",
            content_type,
            headers,
            body,
        }
    }

    #[test]
    fn pairing_cookie_tailscale_and_peer_authority_boundaries() {
        rest_auth_pairing_session_and_authority_tiers_enforced();

        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);

        // Trusted Tailscale-User-Login grants Owner authority when trust_tailscale_headers == true:
        let ts_req = HttpRequest::new("GET", "/api/v1/auth/session")
            .with_header("Tailscale-User-Login", "lmcfarlin@tailnet.ts.net");
        let ts_resp = handle_http_request(&state, &ts_req);
        assert_eq!(ts_resp.status, 200);
        let ts_json = ts_resp.json_value().unwrap();
        assert_eq!(ts_json["authority"]["tier"], "owner");
        assert_eq!(ts_json["authority"]["login"], "lmcfarlin@tailnet.ts.net");

        // When trust_tailscale_headers == false, Tailscale-User-Login alone is rejected with 401:
        let mut untrusted_cfg = state.config.clone();
        untrusted_cfg.trust_tailscale_headers = false;
        let untrusted_state = HubState::new(untrusted_cfg);
        assert_eq!(handle_http_request(&untrusted_state, &ts_req).status, 401);
    }

    #[test]
    fn static_ferrodeck_serving_spa_fallback_and_traversal_refusal() {
        static_ferrodeck_serving_spa_fallback_and_traversal_rejection();

        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);

        let rooms_resp = handle_http_request(&state, &HttpRequest::new("GET", "/rooms/main"));
        assert_eq!(rooms_resp.status, 200);
        assert!(String::from_utf8_lossy(&rooms_resp.body).contains("FerroDeck SPA Root"));

        let trav_resp = handle_http_request(&state, &HttpRequest::new("GET", "/..%2fsecret"));
        assert_eq!(trav_resp.status, 400);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn websocket_broadcast_and_owner_only_terminal_session() {
        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);
        let server = HubServer::bind(state.config.clone()).await.unwrap();
        let addr = server.local_addr();
        let shutdown = server.shutdown_handle();
        let server_task = tokio::spawn(server.serve());

        let timeout = Duration::from_secs(3);

        // 1. Unauthenticated WebSocket connection on /ws/v1 is rejected with 401 Unauthorized.
        let unauth_err = HubWsClient::connect(&format!("ws://{addr}/ws/v1"), &[], timeout)
            .err()
            .expect("unauthenticated ws upgrade should fail");
        assert!(
            unauth_err.to_string().contains("401"),
            "expected 401 on unauthenticated ws upgrade, got: {unauth_err}"
        );

        // 2. Exchange a pairing code over TCP HTTP and connect Owner WebSocket using `Cookie: fl_hub_session=...`.
        let pair_info = {
            let store = state.open_store().unwrap();
            mint_pairing_code_in_store(&store, &format!("http://{addr}"), 600).unwrap()
        };
        let pair_http = HttpRequest::new("POST", "/api/v1/auth/pair")
            .with_json_body(&json!({ "code": pair_info.code }));
        let pair_resp = send_http_over_tcp(addr, &pair_http);
        assert_eq!(pair_resp.status, 200);
        let cookie_hdr = pair_resp
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("set-cookie"))
            .unwrap()
            .1
            .split(';')
            .next()
            .unwrap()
            .to_string();

        let mut owner_ws = HubWsClient::connect(
            &format!("ws://{addr}/ws/v1"),
            &[("Cookie", &cookie_hdr)],
            timeout,
        )
        .unwrap();
        let owner_connected = owner_ws.recv_frame().unwrap();
        assert_eq!(owner_connected.channel, "system");
        assert_eq!(owner_connected.event, "connected");
        assert_eq!(owner_connected.data["authority"]["tier"], "owner");

        // 3. Connect Peer WebSocket on `/api/v1/stream` using Peer token.
        let mut peer_ws = HubWsClient::connect(
            &format!("ws://{addr}/api/v1/stream?agent=ferris"),
            &[("Authorization", "Bearer peer-tok-secret")],
            timeout,
        )
        .unwrap();
        let peer_connected = peer_ws.recv_frame().unwrap();
        assert_eq!(peer_connected.channel, "system");
        assert_eq!(peer_connected.event, "connected");
        assert_eq!(peer_connected.data["authority"]["tier"], "peer");
        assert_eq!(peer_connected.data["authority"]["agent"], "ferris");

        // 4. Verify `system:ping` -> `system:pong`.
        owner_ws
            .send_frame(&HubWsFrame {
                channel: "system".into(),
                event: "ping".into(),
                data: json!({ "seq": 42 }),
            })
            .unwrap();
        let pong = owner_ws.recv_frame().unwrap();
        assert_eq!(pong.channel, "system");
        assert_eq!(pong.event, "pong");
        assert_eq!(pong.data["seq"], 42);

        // 5. Verify `POST /api/v1/wire/send` over TCP broadcasts `wire:message_sent` to both WebSocket clients.
        let wire_req = HttpRequest::new("POST", "/api/v1/wire/send")
            .with_header("Authorization", "Bearer peer-tok-secret")
            .with_header("X-Fl-Agent", "ferris")
            .with_json_body(&json!({
                "from": "ferris",
                "to": ["felicia"],
                "subject": "WS broadcast check",
                "body": "Hello over /ws/v1"
            }));
        let wire_resp = send_http_over_tcp(addr, &wire_req);
        assert_eq!(wire_resp.status, 200);

        let owner_bcast = owner_ws.recv_frame().unwrap();
        assert_eq!(owner_bcast.channel, "wire");
        assert_eq!(owner_bcast.event, "message_sent");
        assert_eq!(owner_bcast.data["message"]["subject"], "WS broadcast check");

        let peer_bcast = peer_ws.recv_frame().unwrap();
        assert_eq!(peer_bcast.channel, "wire");
        assert_eq!(peer_bcast.event, "message_sent");

        // 6. Verify `Peer` receives 403 error frame on `terminal:open`.
        peer_ws
            .send_frame(&HubWsFrame {
                channel: "terminal".into(),
                event: "open".into(),
                data: json!({ "session_id": "peer-term" }),
            })
            .unwrap();
        let peer_term_err = peer_ws.recv_frame().unwrap();
        assert_eq!(peer_term_err.channel, "terminal");
        assert_eq!(peer_term_err.event, "error");
        assert_eq!(peer_term_err.data["status"], 403);
        assert!(
            peer_term_err.data["message"]
                .as_str()
                .unwrap()
                .contains("owner authority required")
        );

        // 7. Verify `Owner` can open a terminal, resize it, send `echo hello_ferrodeck\n`,
        // receive `hello_ferrodeck` in `terminal:output`, and close it.
        owner_ws
            .send_frame(&HubWsFrame {
                channel: "terminal".into(),
                event: "open".into(),
                data: json!({
                    "session_id": "term-1",
                    "cols": 80,
                    "rows": 24,
                }),
            })
            .unwrap();
        let opened = owner_ws.recv_frame().unwrap();
        assert_eq!(opened.channel, "terminal");
        assert_eq!(opened.event, "opened");
        assert_eq!(opened.data["session_id"], "term-1");
        assert!(opened.data["pid"].as_u64().unwrap() > 0);

        owner_ws
            .send_frame(&HubWsFrame {
                channel: "terminal".into(),
                event: "resize".into(),
                data: json!({
                    "session_id": "term-1",
                    "cols": 132,
                    "rows": 42,
                }),
            })
            .unwrap();
        let resized = owner_ws.recv_frame().unwrap();
        assert_eq!(resized.channel, "terminal");
        assert_eq!(resized.event, "resized");
        assert_eq!(resized.data["cols"], 132);
        assert_eq!(resized.data["rows"], 42);

        owner_ws
            .send_frame(&HubWsFrame {
                channel: "terminal".into(),
                event: "input".into(),
                data: json!({
                    "session_id": "term-1",
                    "data": "echo hello_ferrodeck\n",
                }),
            })
            .unwrap();

        let mut collected_output = String::new();
        for _ in 0..16 {
            let frame = owner_ws.recv_frame().unwrap();
            assert_eq!(frame.channel, "terminal");
            if frame.event == "output" {
                collected_output.push_str(frame.data["data"].as_str().unwrap_or_default());
                if collected_output.contains("hello_ferrodeck") {
                    break;
                }
            }
        }
        assert!(
            collected_output.contains("hello_ferrodeck"),
            "expected terminal output to contain hello_ferrodeck, got: {collected_output:?}"
        );

        owner_ws
            .send_frame(&HubWsFrame {
                channel: "terminal".into(),
                event: "close".into(),
                data: json!({ "session_id": "term-1" }),
            })
            .unwrap();
        let mut saw_exited = false;
        for _ in 0..8 {
            let frame = owner_ws.recv_frame().unwrap();
            if frame.channel == "terminal" && frame.event == "exited" {
                assert_eq!(frame.data["session_id"], "term-1");
                saw_exited = true;
                break;
            }
        }
        assert!(saw_exited, "expected terminal:exited after terminal:close");

        // 8. Verify that disconnecting a WebSocket with an active terminal kills the spawned child process.
        owner_ws
            .send_frame(&HubWsFrame {
                channel: "terminal".into(),
                event: "open".into(),
                data: json!({
                    "session_id": "term-disconnect",
                    "command": ["sleep", "60"],
                }),
            })
            .unwrap();
        let sleep_opened = owner_ws.recv_frame().unwrap();
        assert_eq!(sleep_opened.channel, "terminal");
        assert_eq!(sleep_opened.event, "opened");
        let sleep_pid = sleep_opened.data["pid"].as_u64().unwrap();
        let _ = owner_ws.close();
        drop(owner_ws);

        // Wait briefly for server disconnect cleanup to reap the sleep process.
        let mut reaped = false;
        for _ in 0..40 {
            let status_path = format!("/proc/{sleep_pid}/status");
            match fs::read_to_string(&status_path) {
                Err(_) => {
                    reaped = true;
                    break;
                }
                Ok(content) if content.contains("State:\tZ") => {
                    reaped = true;
                    break;
                }
                Ok(_) => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
        assert!(
            reaped,
            "expected terminal child process {sleep_pid} to be killed on WebSocket disconnect"
        );

        let _ = peer_ws.close();
        let _ = shutdown.send(true);
        let _ = server_task.await;

        // 9. Verify `enable_terminal == false` rejects terminal frames even for Owner with 403.
        let mut no_term_cfg = state.config.clone();
        no_term_cfg.enable_terminal = false;
        let no_term_srv = HubServer::bind(no_term_cfg).await.unwrap();
        let no_term_addr = no_term_srv.local_addr();
        let no_term_shutdown = no_term_srv.shutdown_handle();
        let no_term_task = tokio::spawn(no_term_srv.serve());

        let mut owner_no_term_ws = HubWsClient::connect(
            &format!("ws://{no_term_addr}/ws/v1"),
            &[("Authorization", "Bearer owner-tok-secret")],
            timeout,
        )
        .unwrap();
        let _ = owner_no_term_ws.recv_frame().unwrap(); // system:connected
        owner_no_term_ws
            .send_frame(&HubWsFrame {
                channel: "terminal".into(),
                event: "open".into(),
                data: json!({ "session_id": "disabled-term" }),
            })
            .unwrap();
        let disabled_err = owner_no_term_ws.recv_frame().unwrap();
        assert_eq!(disabled_err.channel, "terminal");
        assert_eq!(disabled_err.event, "error");
        assert_eq!(disabled_err.data["status"], 403);
        let _ = owner_no_term_ws.close();
        let _ = no_term_shutdown.send(true);
        let _ = no_term_task.await;
    }

    #[test]
    fn session_prompt_saves_base64_attachments_and_serves_them_over_http() {
        use base64::Engine;

        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);

        let png_bytes: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 1, 2, 3, 4];
        let b64 = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(png_bytes)
        );

        let prompt_req = HttpRequest::new("POST", "/api/v1/sessions/ferris/prompt")
            .with_header("Authorization", "Bearer owner-tok-secret")
            .with_json_body(&json!({
                "prompt": "Check this screenshot",
                "attachments": [
                    {
                        "name": "ipad screenshot.png",
                        "media_type": "image/png",
                        "data_base64": b64
                    }
                ]
            }));
        let prompt_resp = handle_http_request(&state, &prompt_req);
        assert_eq!(prompt_resp.status, 200);
        let body_json = prompt_resp.json_value().unwrap();

        let attachments = body_json["attachments"].as_array().unwrap();
        assert_eq!(attachments.len(), 1);
        let att_url = attachments[0]["url"].as_str().unwrap();
        assert!(att_url.starts_with("/api/v1/attachments/"));
        assert!(att_url.ends_with("-ipad_screenshot.png"));

        let effective_body = body_json["effective_body"].as_str().unwrap();
        assert!(effective_body.contains("Check this screenshot"));
        assert!(effective_body.contains(&format!("({att_url})]")));
        assert_eq!(body_json["status"], "sent");
        assert_eq!(body_json["queued"], false);

        // Verify that operator prompts do NOT create FerroWire messages in GET /api/v1/wire/messages
        let wire_resp = handle_http_request(
            &state,
            &HttpRequest::new("GET", "/api/v1/wire/messages")
                .with_header("Authorization", "Bearer owner-tok-secret"),
        );
        assert_eq!(wire_resp.status, 200);
        assert!(
            wire_resp.json_value().unwrap()["messages"]
                .as_array()
                .unwrap()
                .is_empty(),
            "operator prompt should not pollute FerroWire mail inbox"
        );

        let get_att_req = HttpRequest::new("GET", att_url)
            .with_header("Authorization", "Bearer owner-tok-secret");
        let get_att_resp = handle_http_request(&state, &get_att_req);
        assert_eq!(get_att_resp.status, 200);
        assert_eq!(get_att_resp.content_type, "image/png");
        assert_eq!(get_att_resp.body, png_bytes);
    }

    #[test]
    fn operator_prompt_queue_send_now_cancel_and_auto_drain_lifecycle() {
        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);

        // Create a mock Antigravity session transcript for `ferris` (`conv-ferris-1`)
        let conv_id = "conv-ferris-1";
        let logs_dir = tmp
            .path()
            .join(".gemini")
            .join("antigravity-cli")
            .join("brain")
            .join(conv_id)
            .join(".system_generated")
            .join("logs");
        fs::create_dir_all(&logs_dir).unwrap();
        let transcript_path = logs_dir.join("transcript.jsonl");

        // Start with a busy transcript (ends with a USER_INPUT turn)
        let turn0 = json!({
            "step_index": 0,
            "type": "USER_INPUT",
            "content": "You will take the name of Ferris. Run cargo test.",
            "created_at": "2026-10-27T16:00:00Z"
        })
        .to_string();
        fs::write(&transcript_path, format!("{turn0}\n")).unwrap();

        // 1. Sending a prompt with send_now=false when session is busy queues it
        let q1_resp = handle_http_request(
            &state,
            &HttpRequest::new("POST", "/api/v1/sessions/ferris/prompt")
                .with_header("Authorization", "Bearer owner-tok-secret")
                .with_json_body(&json!({
                    "prompt": "First queued prompt",
                    "send_now": false
                })),
        );
        assert_eq!(q1_resp.status, 200);
        let q1_json = q1_resp.json_value().unwrap();
        assert_eq!(q1_json["status"], "queued");
        assert_eq!(q1_json["queued"], true);
        assert_eq!(q1_json["is_busy"], true);
        let qp1_id = q1_json["queued_prompt"]["id"].as_str().unwrap().to_string();
        assert_eq!(q1_json["queue"].as_array().unwrap().len(), 1);

        // 2. Enqueue two more prompts (qp2 and qp3)
        let q2_resp = handle_http_request(
            &state,
            &HttpRequest::new("POST", "/api/v1/sessions/ferris/prompt")
                .with_header("Authorization", "Bearer owner-tok-secret")
                .with_json_body(&json!({
                    "prompt": "Second queued prompt"
                })),
        );
        let qp2_id = q2_resp.json_value().unwrap()["queued_prompt"]["id"]
            .as_str()
            .unwrap()
            .to_string();

        let q3_resp = handle_http_request(
            &state,
            &HttpRequest::new("POST", "/api/v1/sessions/ferris/prompt")
                .with_header("Authorization", "Bearer owner-tok-secret")
                .with_json_body(&json!({
                    "prompt": "Third queued prompt"
                })),
        );
        let qp3_id = q3_resp.json_value().unwrap()["queued_prompt"]["id"]
            .as_str()
            .unwrap()
            .to_string();

        // Verify GET /api/v1/sessions/ferris/queue returns all 3 items
        let get_q_resp = handle_http_request(
            &state,
            &HttpRequest::new("GET", "/api/v1/sessions/ferris/queue")
                .with_header("Authorization", "Bearer owner-tok-secret"),
        );
        assert_eq!(get_q_resp.status, 200);
        assert_eq!(
            get_q_resp.json_value().unwrap()["queue"]
                .as_array()
                .unwrap()
                .len(),
            3
        );

        // 3. Cancel qp2 via DELETE /api/v1/sessions/ferris/queue/:prompt_id
        let del_resp = handle_http_request(
            &state,
            &HttpRequest::new("DELETE", format!("/api/v1/sessions/ferris/queue/{qp2_id}"))
                .with_header("Authorization", "Bearer owner-tok-secret"),
        );
        assert_eq!(del_resp.status, 200);
        let del_json = del_resp.json_value().unwrap();
        assert_eq!(del_json["status"], "cancelled");
        assert_eq!(del_json["queue"].as_array().unwrap().len(), 2);

        // 3b. Edit qp3 in-place via PATCH /api/v1/sessions/ferris/queue/:prompt_id
        let patch_resp = handle_http_request(
            &state,
            &HttpRequest::new("PATCH", format!("/api/v1/sessions/ferris/queue/{qp3_id}"))
                .with_header("Authorization", "Bearer owner-tok-secret")
                .with_json_body(&json!({
                    "prompt": "Third queued prompt (edited)"
                })),
        );
        assert_eq!(patch_resp.status, 200);
        let patch_json = patch_resp.json_value().unwrap();
        assert_eq!(patch_json["status"], "updated");
        assert_eq!(
            patch_json["queued_prompt"]["prompt"],
            "Third queued prompt (edited)"
        );
        assert_eq!(patch_json["queue"].as_array().unwrap().len(), 2);

        // 4. Send qp1 now via POST /api/v1/sessions/ferris/queue/:prompt_id/send-now
        let send_now_resp = handle_http_request(
            &state,
            &HttpRequest::new(
                "POST",
                format!("/api/v1/sessions/ferris/queue/{qp1_id}/send-now"),
            )
            .with_header("Authorization", "Bearer owner-tok-secret"),
        );
        assert_eq!(send_now_resp.status, 200);
        let sn_json = send_now_resp.json_value().unwrap();
        assert_eq!(sn_json["status"], "sent");
        assert_eq!(sn_json["prompt_id"], qp1_id);
        assert_eq!(sn_json["queue"].as_array().unwrap().len(), 1);
        assert_eq!(sn_json["queue"][0]["id"], qp3_id);

        // 5. Direct POST with send_now=true bypasses the queue even while busy
        let bypass_resp = handle_http_request(
            &state,
            &HttpRequest::new("POST", "/api/v1/sessions/ferris/prompt")
                .with_header("Authorization", "Bearer owner-tok-secret")
                .with_json_body(&json!({
                    "prompt": "Urgent immediate override",
                    "send_now": true
                })),
        );
        assert_eq!(bypass_resp.status, 200);
        let bypass_json = bypass_resp.json_value().unwrap();
        assert_eq!(bypass_json["status"], "sent");
        assert_eq!(bypass_json["queued"], false);
        assert_eq!(bypass_json["queue"].as_array().unwrap().len(), 1);

        // 6. Update transcript on disk to end with an assistant turn (no tool calls)
        // and verify GET /api/v1/sessions/ferris/transcript auto-drains qp3!
        let turn1 = json!({
            "step_index": 1,
            "type": "PLANNER_RESPONSE",
            "content": "Completed initial task.",
            "created_at": "2026-10-27T16:01:00Z"
        })
        .to_string();
        fs::write(&transcript_path, format!("{turn0}\n{turn1}\n")).unwrap();

        let tr_resp = handle_http_request(
            &state,
            &HttpRequest::new("GET", "/api/v1/sessions/ferris/transcript")
                .with_header("Authorization", "Bearer owner-tok-secret"),
        );
        assert_eq!(tr_resp.status, 200);
        let tr_json = tr_resp.json_value().unwrap();
        assert!(
            tr_json["queue"].as_array().unwrap().is_empty(),
            "expected qp3 to be automatically drained when session finished its turn"
        );
        assert_eq!(
            tr_json["is_busy"], true,
            "session should now be marked in_flight after draining qp3"
        );
        assert_eq!(tr_json["total_turns"], 2);

        // 7. Verify ?limit=<N> and ?limit=all tail slicing on GET /api/v1/sessions/ferris/transcript
        let tr_limit_1 = handle_http_request(
            &state,
            &HttpRequest::new("GET", "/api/v1/sessions/ferris/transcript?limit=1")
                .with_header("Authorization", "Bearer owner-tok-secret"),
        )
        .json_value()
        .unwrap();
        assert_eq!(tr_limit_1["total_turns"], 2);
        assert_eq!(tr_limit_1["turns"].as_array().unwrap().len(), 1);
        assert_eq!(tr_limit_1["turns"][0]["content"], "Completed initial task.");

        let tr_limit_all = handle_http_request(
            &state,
            &HttpRequest::new("GET", "/api/v1/sessions/ferris/transcript?limit=all")
                .with_header("Authorization", "Bearer owner-tok-secret"),
        )
        .json_value()
        .unwrap();
        assert_eq!(tr_limit_all["total_turns"], 2);
        assert_eq!(tr_limit_all["turns"].as_array().unwrap().len(), 2);

        // 8. Verify FerroGate `/api/v1/gate/*` endpoints (secrets ACL, connectors, registry v0.1, and caller_agent spoofing guard)
        let peer_set_resp = handle_http_request(
            &state,
            &HttpRequest::new("POST", "/api/v1/gate/secrets")
                .with_header("Authorization", "Bearer peer-tok-secret")
                .with_json_body(&json!({
                    "name": "DISCORD_BOT_TOKEN",
                    "value": "secret-val",
                    "allowed_hosts": ["discord.com"]
                })),
        );
        assert_eq!(
            peer_set_resp.status, 403,
            "Peer authority must not be allowed to write vault secrets"
        );

        let owner_set_resp = handle_http_request(
            &state,
            &HttpRequest::new("POST", "/api/v1/gate/secrets")
                .with_header("Authorization", "Bearer owner-tok-secret")
                .with_json_body(&json!({
                    "name": "DISCORD_BOT_TOKEN",
                    "value": "secret-discord-value-12345",
                    "allowed_hosts": ["discord.com"],
                    "allowed_methods": ["POST"],
                    "allowed_agents": ["ferris"]
                })),
        );
        assert_eq!(owner_set_resp.status, 200);

        let list_secrets_resp = handle_http_request(
            &state,
            &HttpRequest::new("GET", "/api/v1/gate/secrets")
                .with_header("Authorization", "Bearer peer-tok-secret"),
        );
        assert_eq!(list_secrets_resp.status, 200);
        let secrets_body = String::from_utf8_lossy(&list_secrets_resp.body);
        assert!(secrets_body.contains("DISCORD_BOT_TOKEN"));
        assert!(!secrets_body.contains("secret-discord-value-12345"));

        // Peer attempts to spoof `caller_agent: "ferris"` on `/api/v1/gate/call` -> rejected because Peer actor is `"peer"`
        let spoof_call_resp = handle_http_request(
            &state,
            &HttpRequest::new("POST", "/api/v1/gate/call")
                .with_header("Authorization", "Bearer peer-tok-secret")
                .with_json_body(&json!({
                    "method": "POST",
                    "url": "https://discord.com/api/v10/channels/1/messages",
                    "headers": {
                        "Authorization": "Bot {{FERROGATE:DISCORD_BOT_TOKEN}}"
                    },
                    "caller_agent": "ferris"
                })),
        );
        assert_eq!(
            spoof_call_resp.status, 403,
            "Peer must not be able to spoof caller_agent=ferris to bypass allowed_agents"
        );

        let del_secret_resp = handle_http_request(
            &state,
            &HttpRequest::new("DELETE", "/api/v1/gate/secrets/DISCORD_BOT_TOKEN")
                .with_header("Authorization", "Bearer owner-tok-secret"),
        );
        assert_eq!(del_secret_resp.status, 200);
    }

    #[test]
    fn store_open_failure_keeps_vendor_busy_and_sessions_returns_500() {
        let tmp = TempDir::new().unwrap();
        let mut state = test_hub_state(&tmp);
        let conv_id = "conv-store-open-fail";
        let logs_dir = tmp
            .path()
            .join(".gemini")
            .join("antigravity-cli")
            .join("brain")
            .join(conv_id)
            .join(".system_generated")
            .join("logs");
        fs::create_dir_all(&logs_dir).unwrap();
        let turn = json!({
            "step_index": 0,
            "type": "USER_INPUT",
            "content": "still working",
            "created_at": "2026-10-29T16:00:00Z"
        })
        .to_string();
        fs::write(logs_dir.join("transcript.jsonl"), format!("{turn}\n")).unwrap();

        let store_dir = tmp.path().join("store-is-a-directory");
        fs::create_dir_all(&store_dir).unwrap();
        state.config.store_path = store_dir;

        assert!(
            state.is_session_busy_released(conv_id),
            "a failed store open must not treat a busy vendor transcript as idle"
        );
        state.mark_session_in_flight_released(conv_id);
        assert!(
            state.is_session_busy_from_turns(conv_id, &[]),
            "in-flight must stay set when the follow-up turn slice is empty"
        );

        let resp = handle_http_request(
            &state,
            &HttpRequest::new("GET", "/api/v1/sessions")
                .with_header("Authorization", "Bearer owner-tok-secret"),
        );
        assert_eq!(resp.status, 500);
        let body = String::from_utf8_lossy(&resp.body);
        assert!(
            body.contains("failed to open store"),
            "error body was {body}"
        );
    }

    #[test]
    fn worktrees_request_does_not_take_store_lock() {
        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);
        let _guard = state.open_store().expect("hold store lock");
        let state2 = state.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let started = std::time::Instant::now();
            let resp = handle_http_request(
                &state2,
                &HttpRequest::new("GET", "/api/v1/worktrees")
                    .with_header("Authorization", "Bearer owner-tok-secret"),
            );
            let _ = tx.send((started.elapsed(), resp.status));
        });
        let (elapsed, status) = rx
            .recv_timeout(Duration::from_millis(500))
            .expect("GET /api/v1/worktrees blocked on HubStoreGuard");
        assert!(
            elapsed < Duration::from_millis(500),
            "worktrees handler waited {elapsed:?}"
        );
        assert!(status == 200 || status == 500, "status {status}");
        drop(_guard);
        handle.join().expect("worktrees thread");
    }

    #[test]
    fn empty_prompt_queue_does_not_open_the_store() {
        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);
        let _guard = state.open_store().expect("hold store lock");
        let state2 = state.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let started = std::time::Instant::now();
            drain_ready_session_queues(&state2);
            let _ = tx.send(started.elapsed());
        });
        let elapsed = rx
            .recv_timeout(Duration::from_millis(500))
            .expect("drain with an empty queue blocked on HubStoreGuard");
        assert!(
            elapsed < Duration::from_millis(500),
            "empty drain waited {elapsed:?}"
        );
        drop(_guard);
        handle.join().expect("drain thread");
    }

    #[test]
    fn open_store_serializes_concurrent_holders() {
        use std::sync::Barrier;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let tmp = TempDir::new().unwrap();
        let state = Arc::new(test_hub_state(&tmp));
        let current = Arc::new(AtomicUsize::new(0));
        let max_concurrent = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(4));

        // White-box: the guard must actually hold store_lock for its lifetime.
        // This fails if open_store drops the lock early, unlike holder counts alone.
        let guard = state.open_store().expect("open_store for lock probe");
        assert!(
            state.store_lock.try_lock().is_err(),
            "store_lock must be held while HubStoreGuard is alive"
        );
        drop(guard);
        assert!(
            state.store_lock.try_lock().is_ok(),
            "store_lock must be released after HubStoreGuard is dropped"
        );

        let handles: Vec<_> = (0..4)
            .map(|_| {
                let state = Arc::clone(&state);
                let current = Arc::clone(&current);
                let max_concurrent = Arc::clone(&max_concurrent);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    let store = state
                        .open_store()
                        .expect("serialized open_store must not collide");
                    let holders = current.fetch_add(1, Ordering::SeqCst) + 1;
                    max_concurrent.fetch_max(holders, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(50));
                    let _ = store.list_workflow_defs(None);
                    current.fetch_sub(1, Ordering::SeqCst);
                    drop(store);
                })
            })
            .collect();

        for handle in handles {
            handle.join().expect("open_store thread panicked");
        }
        assert_eq!(
            max_concurrent.load(Ordering::SeqCst),
            1,
            "HubStoreGuard must serialize RedbStore holders"
        );
    }

    #[test]
    fn code_search_route_records_token_gain_and_stats_exposes_it() {
        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        let mut code = String::from("pub fn hub_widget() {}\n");
        for i in 0..40 {
            code.push_str(&format!("// filler line {i} padding the file out\n"));
        }
        fs::write(tmp.path().join("src/widget.rs"), code).unwrap();

        let hit = handle_http_request(
            &state,
            &HttpRequest::new("GET", "/api/v1/state/code-search?q=hub_widget&kind=def")
                .with_header("Authorization", "Bearer owner-tok-secret"),
        );
        assert_eq!(hit.status, 200);
        let hit_json = hit.json_value().unwrap();
        assert_eq!(hit_json["query"], "hub_widget", "{hit_json}");
        assert_eq!(hit_json["kind"], "def", "{hit_json}");
        assert_eq!(hit_json["symbols"][0]["name"], "hub_widget", "{hit_json}");
        assert!(
            hit_json["tokens_saved_this_query"].as_u64().unwrap() > 0,
            "{hit_json}"
        );
        assert_eq!(hit_json["token_gain"]["queries_count"], 1, "{hit_json}");

        let stats = handle_http_request(
            &state,
            &HttpRequest::new("GET", "/api/v1/stats")
                .with_header("Authorization", "Bearer owner-tok-secret"),
        );
        assert_eq!(stats.status, 200);
        let stats_json = stats.json_value().unwrap();
        assert_eq!(stats_json["token_gain"]["queries_count"], 1, "{stats_json}");
        assert!(
            stats_json["token_gain"]["files_indexed"].as_u64().unwrap() >= 1,
            "{stats_json}"
        );

        let bad_kind = handle_http_request(
            &state,
            &HttpRequest::new("GET", "/api/v1/state/code-search?q=hub_widget&kind=bogus")
                .with_header("Authorization", "Bearer owner-tok-secret"),
        );
        assert_eq!(bad_kind.status, 400);

        let blank = handle_http_request(
            &state,
            &HttpRequest::new("GET", "/api/v1/state/code-search?kind=def")
                .with_header("Authorization", "Bearer owner-tok-secret"),
        );
        assert_eq!(blank.status, 200);
        let blank_json = blank.json_value().unwrap();
        assert_eq!(blank_json["symbols"].as_array().unwrap().len(), 0);
        assert_eq!(blank_json["tokens_saved_this_query"], 0);
        assert_eq!(blank_json["token_gain"]["queries_count"], 1, "{blank_json}");
    }

    fn install_webhook_route(home: &std::path::Path, persona: &str, socket: &std::path::Path) {
        use fl_core::at::At;
        use fl_wire::identity::{RouteTable, VendorRoute};

        let mut routes = RouteTable::default();
        routes.upsert(
            persona,
            VendorRoute::Webhook {
                socket_path: socket.to_path_buf(),
            },
            At::from_unix_millis(1),
        );
        let path = home.join(".cache").join("agent-bus").join("routes.json");
        routes.save(Some(&path)).unwrap();
    }

    fn collect_webhook_lines(
        socket: std::path::PathBuf,
        count: usize,
    ) -> std::thread::JoinHandle<Vec<String>> {
        use std::io::{BufRead, BufReader};
        use std::os::unix::net::UnixListener;

        let listener = UnixListener::bind(&socket).unwrap();
        std::thread::spawn(move || {
            let mut lines = Vec::new();
            while lines.len() < count {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 {
                    continue;
                }
                lines.push(line);
            }
            lines
        })
    }

    #[test]
    fn session_clear_drops_queue_and_sends_clear() {
        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);
        state.enqueue_session_prompt(
            "felicia",
            Some("felicia".into()),
            "stale queued prompt".into(),
            "stale queued prompt".into(),
            "owner".into(),
            Vec::new(),
        );
        state.mark_session_in_flight_from_turns("felicia", &[]);

        let unauth = handle_http_request(
            &state,
            &HttpRequest::new("POST", "/api/v1/sessions/felicia/clear")
                .with_json_body(&json!({ "mode": "clear" })),
        );
        assert_eq!(unauth.status, 401);
        assert_eq!(state.session_queue("felicia").len(), 1);

        let bad_mode = handle_http_request(
            &state,
            &HttpRequest::new("POST", "/api/v1/sessions/felicia/clear")
                .with_header("Authorization", "Bearer owner-tok-secret")
                .with_json_body(&json!({ "mode": "wipe" })),
        );
        assert_eq!(bad_mode.status, 400);
        assert_eq!(state.session_queue("felicia").len(), 1);

        let missing_route = handle_http_request(
            &state,
            &HttpRequest::new("POST", "/api/v1/sessions/felicia/clear")
                .with_header("Authorization", "Bearer peer-tok-secret")
                .with_json_body(&json!({ "mode": "clear" })),
        );
        assert_eq!(missing_route.status, 404);
        assert!(state.session_queue("felicia").is_empty());
        assert!(!state.is_session_busy_from_turns("felicia", &[]));

        state.enqueue_session_prompt(
            "felicia",
            Some("felicia".into()),
            "second stale prompt".into(),
            "second stale prompt".into(),
            "owner".into(),
            Vec::new(),
        );
        let socket = tmp.path().join("felicia.sock");
        install_webhook_route(tmp.path(), "felicia", &socket);
        let lines = collect_webhook_lines(socket, 2);
        let mut frames = state.subscribe();

        let cleared = handle_http_request(
            &state,
            &HttpRequest::new("POST", "/api/v1/sessions/felicia/clear")
                .with_header("Authorization", "Bearer owner-tok-secret")
                .with_json_body(&json!({ "mode": "clear" })),
        );
        assert_eq!(
            cleared.status,
            200,
            "{}",
            String::from_utf8_lossy(&cleared.body)
        );
        let body = cleared.json_value().unwrap();
        assert_eq!(body["ok"], true);
        assert_eq!(body["session_id"], "webhook-felicia");
        assert_eq!(body["persona"], "felicia");
        assert_eq!(body["mode"], "clear");
        assert!(body["prompt"].is_null());
        assert!(state.session_queue("felicia").is_empty());

        let frame = frames.try_recv().unwrap();
        assert_eq!(frame.channel, "sessions");
        assert_eq!(frame.event, "session.cleared");
        assert_eq!(frame.data["mode"], "clear");

        let alias = handle_http_request(
            &state,
            &HttpRequest::new("POST", "/api/v1/sessions/felicia/clear")
                .with_header("Authorization", "Bearer peer-tok-secret")
                .with_json_body(&json!({ "mode": "clear_only" })),
        );
        assert_eq!(alias.status, 200);
        assert_eq!(alias.json_value().unwrap()["mode"], "clear");

        let delivered = lines.join().unwrap();
        for line in &delivered {
            let value: Value = serde_json::from_str(line).unwrap();
            assert_eq!(value["kind"], "operator_prompt");
            assert_eq!(value["body"], "/clear");
        }
    }

    fn authed(method: &str, path: &str, token: &str, body: &Value) -> HttpRequest {
        HttpRequest::new(method, path)
            .with_header("Authorization", format!("Bearer {token}"))
            .with_json_body(body)
    }

    fn spin_up_body(persona: &str) -> Value {
        json!({
            "persona": persona,
            "project": "FerroLoop",
            "role": "assistant",
            "vendor": "claude",
        })
    }

    fn spin_up(state: &HubState, body: &Value) -> HttpResponse {
        handle_http_request(
            state,
            &authed("POST", "/api/v1/sessions/spin-up", "owner-tok-secret", body),
        )
    }

    #[test]
    fn spin_up_validates_registers_agent_and_persists_hosted_record() {
        use fl_core::agent::{AgentRole, AgentStatus};
        use fl_core::store::AgentRegistry;

        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);
        let body = spin_up_body("felicity");

        let unauth = handle_http_request(
            &state,
            &HttpRequest::new("POST", "/api/v1/sessions/spin-up").with_json_body(&body),
        );
        assert_eq!(unauth.status, 401);

        let mut bad_cases: Vec<(&str, Value)> = Vec::new();
        for (field, value) in [
            ("persona", json!("")),
            ("persona", json!("Felicity")),
            ("persona", json!("fel icity")),
            ("project", json!("  ")),
            ("role", json!("owner")),
            ("role", json!("")),
            ("vendor", json!("gemini")),
            ("vendor", json!("")),
            ("node_id", json!("bad node")),
        ] {
            let mut bad = body.clone();
            bad[field] = value;
            bad_cases.push((field, bad));
        }
        for (field, bad) in bad_cases {
            let resp = spin_up(&state, &bad);
            assert_eq!(resp.status, 400, "{field}: {bad}");
            assert!(
                resp.json_value().unwrap()["error"]
                    .as_str()
                    .unwrap()
                    .starts_with(field),
                "{field}"
            );
        }
        let not_json = handle_http_request(
            &state,
            &HttpRequest::new("POST", "/api/v1/sessions/spin-up")
                .with_header("Authorization", "Bearer owner-tok-secret")
                .with_body(b"{nope".to_vec()),
        );
        assert_eq!(not_json.status, 400);
        // Nothing from the rejected requests reached the registry or the record list.
        assert!(
            state
                .open_store()
                .unwrap()
                .list_agents(None, true)
                .unwrap()
                .is_empty()
        );

        let ok = spin_up(&state, &body);
        assert_eq!(ok.status, 200, "{}", String::from_utf8_lossy(&ok.body));
        let out = ok.json_value().unwrap();
        assert_eq!(out["session_id"], "claude:felicity");
        assert_eq!(out["persona"], "felicity");
        assert_eq!(out["project"], "FerroLoop");
        assert_eq!(out["role"], "assistant");
        assert_eq!(out["vendor"], "claude");
        assert!(out["model"].is_null());
        assert_eq!(out["node_id"], "ai-lab-0");
        assert_eq!(out["hosted"], true);
        assert_eq!(out["status"], "running");
        assert_eq!(out["tmux_target"], "Felicity:claude");
        // No live route exists, so nothing took the hydration prompt.
        assert_eq!(out["hydration_dispatched"], false);
        assert!(out["prompt"].as_str().unwrap().starts_with(
            "Take the name of Felicity and take up residence in FerroLoop/ as the assistant."
        ));

        {
            let store = state.open_store().unwrap();
            let agent = store.agent_by_name("felicity").unwrap().unwrap();
            assert_eq!(agent.status, AgentStatus::Active);
            assert_eq!(agent.role, AgentRole::Assistant);
            assert_eq!(agent.project, "ferroloop");
            assert!(store.get_area("ferroloop").unwrap().is_some());
        }

        // A Peer may spin up too (same gate as the session clear endpoint); the override fields stick.
        let peer = handle_http_request(
            &state,
            &authed(
                "POST",
                "/api/v1/sessions/spin-up",
                "peer-tok-secret",
                &json!({
                    "persona": "ada-2", "project": "FerroLoop", "role": "ui-developer",
                    "vendor": "muse", "model": "muse-spark", "node_id": "ai-lab-1",
                    "hosted": false, "cwd": tmp.path().display().to_string(),
                }),
            ),
        );
        assert_eq!(peer.status, 200, "{}", String::from_utf8_lossy(&peer.body));
        let peer_out = peer.json_value().unwrap();
        assert_eq!(peer_out["session_id"], "muse:ada-2");
        assert_eq!(peer_out["model"], "muse-spark");
        assert_eq!(peer_out["node_id"], "ai-lab-1");
        assert_eq!(peer_out["hosted"], false);

        let listed = handle_http_request(
            &state,
            &authed(
                "GET",
                "/api/v1/hosted-sessions",
                "peer-tok-secret",
                &json!({}),
            ),
        )
        .json_value()
        .unwrap();
        let rec = &listed["sessions"][1];
        assert_eq!(rec["session_id"], "muse:ada-2");
        assert_eq!(rec["role"], "ui-developer");
        assert_eq!(rec["status"], "running");
        assert_eq!(rec["restart_policy"], "on-failure");
        assert_eq!(rec["restart_count"], 0);
        assert_eq!(rec["tmux_target"], "Ada-2:muse");
        assert_eq!(rec["hydration_dispatched"], false);
        assert!(rec["decommission_prepared_at_ms"].is_null());
        assert!(rec["created_at_ms"].as_u64().unwrap() > 0);
    }

    #[test]
    fn decommission_prepare_then_teardown_cleans_every_trace() {
        use fl_core::agent::AgentStatus;
        use fl_core::store::AgentRegistry;
        use fl_wire::identity::RouteTable;

        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);
        assert_eq!(spin_up(&state, &spin_up_body("felicity")).status, 200);
        assert_eq!(spin_up(&state, &spin_up_body("ada")).status, 200);
        let decommission = |token: Option<&str>, id: &str, body: Value| {
            let req = HttpRequest::new("POST", format!("/api/v1/sessions/{id}/decommission"))
                .with_json_body(&body);
            let req = match token {
                Some(t) => req.with_header("Authorization", format!("Bearer {t}")),
                None => req,
            };
            handle_http_request(&state, &req)
        };

        assert_eq!(
            decommission(None, "claude:felicity", json!({ "mode": "prepare" })).status,
            401
        );
        for bad in [json!({ "mode": "wipe" }), json!({ "mode": "" }), json!({})] {
            let resp = decommission(Some("owner-tok-secret"), "claude:felicity", bad);
            assert_eq!(resp.status, 400);
        }
        // Rejected requests changed nothing.
        let store_status = |name: &str| {
            state
                .open_store()
                .unwrap()
                .agent_by_name(name)
                .unwrap()
                .unwrap()
                .status
        };
        assert_eq!(store_status("felicity"), AgentStatus::Active);

        // Prepare with a live route: the handshake is delivered to it, not queued.
        let socket = tmp.path().join("felicity.sock");
        install_webhook_route(tmp.path(), "felicity", &socket);
        let lines = collect_webhook_lines(socket, 1);
        let prepared = decommission(
            Some("owner-tok-secret"),
            "claude%3Afelicity",
            json!({ "mode": "prepare" }),
        );
        assert_eq!(
            prepared.status,
            200,
            "{}",
            String::from_utf8_lossy(&prepared.body)
        );
        let out = prepared.json_value().unwrap();
        assert_eq!(out["session_id"], "claude:felicity");
        assert_eq!(out["persona"], "felicity");
        assert_eq!(out["mode"], "prepare");
        assert_eq!(out["status"], "decommissioning");
        assert_eq!(out["handshake_dispatched"], true);
        assert_eq!(out["unregistered"], false);
        assert_eq!(out["runtime_stopped"], false);
        let prompt = out["prompt"].as_str().unwrap();
        assert!(
            prompt.contains("Notes/FerroLoop/STATE.md") && prompt.contains("agent-msg read --ack")
        );
        let delivered: Value = serde_json::from_str(&lines.join().unwrap()[0]).unwrap();
        assert_eq!(delivered["kind"], "operator_prompt");
        assert_eq!(delivered["body"], prompt);
        assert!(state.session_queue("claude:felicity").is_empty());
        assert_eq!(store_status("felicity"), AgentStatus::Active);

        let record = |id: &str| {
            let listed = handle_http_request(
                &state,
                &authed(
                    "GET",
                    "/api/v1/hosted-sessions",
                    "owner-tok-secret",
                    &json!({}),
                ),
            )
            .json_value()
            .unwrap();
            listed["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["session_id"] == id)
                .cloned()
                .unwrap()
        };
        let rec = record("claude:felicity");
        assert_eq!(rec["status"], "decommissioning");
        assert!(rec["decommission_prepared_at_ms"].as_u64().unwrap() > 0);

        // Prepare without a route falls back to the session prompt queue.
        let queued = decommission(
            Some("peer-tok-secret"),
            "claude:ada",
            json!({ "mode": "prepare" }),
        )
        .json_value()
        .unwrap();
        assert_eq!(queued["handshake_dispatched"], false);
        assert_eq!(state.session_queue("claude:ada").len(), 1);
        assert_eq!(
            state.session_queue("claude:ada")[0].prompt,
            queued["prompt"].as_str().unwrap()
        );

        // Teardown: queue cleared, route gone, agent retired, record decommissioned.
        state.enqueue_session_prompt(
            "claude:felicity",
            Some("felicity".into()),
            "stale".into(),
            "stale".into(),
            "owner".into(),
            Vec::new(),
        );
        state.enqueue_session_prompt(
            "felicity",
            Some("felicity".into()),
            "stale too".into(),
            "stale too".into(),
            "owner".into(),
            Vec::new(),
        );
        let torn = decommission(
            Some("owner-tok-secret"),
            "claude:felicity",
            json!({ "mode": "teardown" }),
        );
        assert_eq!(torn.status, 200, "{}", String::from_utf8_lossy(&torn.body));
        let out = torn.json_value().unwrap();
        assert_eq!(out["session_id"], "claude:felicity");
        assert_eq!(out["mode"], "teardown");
        assert_eq!(out["status"], "decommissioned");
        assert_eq!(out["handshake_dispatched"], false);
        assert_eq!(out["unregistered"], true);
        assert_eq!(out["runtime_stopped"], true);
        assert!(out["prompt"].is_null());

        assert!(state.session_queue("claude:felicity").is_empty());
        assert!(state.session_queue("felicity").is_empty());
        let routes_path = tmp.path().join(".cache/agent-bus/routes.json");
        assert!(
            RouteTable::load(Some(&routes_path))
                .get("felicity")
                .is_none()
        );
        assert_eq!(store_status("felicity"), AgentStatus::Retired);
        let rec = record("claude:felicity");
        assert_eq!(rec["status"], "decommissioned");
        assert!(
            rec["updated_at_ms"].as_u64().unwrap()
                >= rec["decommission_prepared_at_ms"].as_u64().unwrap()
        );
        // The other session is untouched.
        assert_eq!(store_status("ada"), AgentStatus::Active);
        assert_eq!(record("claude:ada")["status"], "decommissioning");
        assert_eq!(state.session_queue("claude:ada").len(), 1);
    }

    #[test]
    fn decommission_of_an_unknown_session_prepare_404_teardown_ok() {
        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);
        let call = |mode: &str| {
            handle_http_request(
                &state,
                &authed(
                    "POST",
                    "/api/v1/sessions/ghost/decommission",
                    "owner-tok-secret",
                    &json!({ "mode": mode }),
                ),
            )
        };
        assert_eq!(call("prepare").status, 404);
        let torn = call("teardown");
        assert_eq!(torn.status, 200);
        assert_eq!(torn.json_value().unwrap()["session_id"], "ghost");
    }

    fn register_resident(
        state: &HubState,
        persona: &str,
        role: fl_core::agent::AgentRole,
        project: &str,
    ) {
        use fl_core::agent::RegisterAgentArgs;
        use fl_core::store::AgentRegistry;
        state
            .open_store()
            .unwrap()
            .register_agent(RegisterAgentArgs {
                display_name: persona[..1].to_ascii_uppercase() + &persona[1..],
                email: Some(format!("{persona}@example.org")),
                project: project.into(),
                role,
                extra_areas: vec!["notes".into()],
                create_area: true,
                replace: false,
                at: stamp::now(),
                by: "owner".into(),
            })
            .unwrap();
    }

    fn decommission_as(state: &HubState, token: &str, id: &str, mode: &str) -> HttpResponse {
        handle_http_request(
            state,
            &authed(
                "POST",
                &format!("/api/v1/sessions/{id}/decommission"),
                token,
                &json!({ "mode": mode }),
            ),
        )
    }

    #[test]
    fn spin_up_never_re_homes_a_resident_but_keeps_its_email_on_a_repeat() {
        use fl_core::agent::{AgentRole, AgentStatus};
        use fl_core::store::AgentRegistry;

        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);
        register_resident(&state, "jarvis", AgentRole::LabManager, "ferroloop");
        register_resident(&state, "ada", AgentRole::Assistant, "ferroloop");
        let agent = |name: &str| {
            state
                .open_store()
                .unwrap()
                .agent_by_name(name)
                .unwrap()
                .unwrap()
        };
        let jarvis = agent("jarvis");

        // Nobody can spin up a resident's persona in another role or project: the Owner gets the
        // 409 (a Peer is refused earlier, with 403; see the peer_spin_up_* test).
        for (role, project) in [("assistant", "FerroLoop"), ("lab-manager", "Elsewhere")] {
            let resp = handle_http_request(
                &state,
                &authed(
                    "POST",
                    "/api/v1/sessions/spin-up",
                    "owner-tok-secret",
                    &json!({
                        "persona": "jarvis", "project": project, "role": role, "vendor": "claude",
                    }),
                ),
            );
            assert_eq!(resp.status, 409, "{role} {project}");
        }
        assert_eq!(agent("jarvis"), jarvis);
        assert_eq!(agent("jarvis").status, AgentStatus::Active);
        let listed = handle_http_request(
            &state,
            &authed(
                "GET",
                "/api/v1/hosted-sessions",
                "owner-tok-secret",
                &json!({}),
            ),
        )
        .json_value()
        .unwrap();
        assert!(listed["sessions"].as_array().unwrap().is_empty());

        // The same project and role is an idempotent re-spin that keeps email and extra areas.
        assert_eq!(spin_up(&state, &spin_up_body("ada")).status, 200);
        let ada = agent("ada");
        assert_eq!(ada.email, "ada@example.org");
        assert_eq!(ada.extra_areas, ["notes"]);
    }

    #[test]
    fn peer_spin_up_of_an_unmanaged_agent_is_403_and_cannot_unlock_teardown() {
        use fl_core::agent::{AgentRole, AgentStatus};
        use fl_core::store::AgentRegistry;

        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);
        register_resident(&state, "jarvis", AgentRole::LabManager, "ferroloop");
        let jarvis = state
            .open_store()
            .unwrap()
            .agent_by_name("jarvis")
            .unwrap()
            .unwrap();
        let hosted_count = || {
            handle_http_request(
                &state,
                &authed(
                    "GET",
                    "/api/v1/hosted-sessions",
                    "owner-tok-secret",
                    &json!({}),
                ),
            )
            .json_value()
            .unwrap()["sessions"]
                .as_array()
                .unwrap()
                .len()
        };
        let body = json!({
            "persona": "jarvis", "project": "ferroloop", "role": "lab-manager", "vendor": "claude",
        });
        let spin_up_as = |token: &str| {
            handle_http_request(
                &state,
                &authed("POST", "/api/v1/sessions/spin-up", token, &body),
            )
        };

        let resp = spin_up_as("peer-tok-secret");
        assert_eq!(resp.status, 403, "{}", String::from_utf8_lossy(&resp.body));
        assert_eq!(hosted_count(), 0);

        // The refused spin-up did not make it hosted, so the Peer still cannot retire it.
        for mode in ["prepare", "teardown"] {
            let resp = decommission_as(&state, "peer-tok-secret", "claude%3Ajarvis", mode);
            assert_eq!(
                resp.status,
                403,
                "{mode}: {}",
                String::from_utf8_lossy(&resp.body)
            );
        }
        assert_eq!(hosted_count(), 0);
        let after = state
            .open_store()
            .unwrap()
            .agent_by_name("jarvis")
            .unwrap()
            .unwrap();
        assert_eq!(after, jarvis);
        assert_eq!(after.status, AgentStatus::Active);

        // The Owner may host it; email and extra areas survive.
        let resp = spin_up_as("owner-tok-secret");
        assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
        assert_eq!(hosted_count(), 1);
        let hosted = state
            .open_store()
            .unwrap()
            .agent_by_name("jarvis")
            .unwrap()
            .unwrap();
        assert_eq!(hosted.email, "jarvis@example.org");
        assert_eq!(hosted.extra_areas, ["notes"]);
    }

    #[test]
    fn peer_cannot_reach_a_reregistered_agent_through_its_stale_decommissioned_record() {
        use fl_core::agent::{AgentRole, AgentStatus, RegisterAgentArgs};
        use fl_core::store::AgentRegistry;

        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);
        assert_eq!(spin_up(&state, &spin_up_body("ada")).status, 200);
        let torn = decommission_as(&state, "owner-tok-secret", "claude%3Aada", "teardown");
        assert_eq!(torn.status, 200, "{}", String::from_utf8_lossy(&torn.body));
        // The resident restarts and registers itself again, outside the lifecycle.
        state
            .open_store()
            .unwrap()
            .register_agent(RegisterAgentArgs {
                display_name: "Ada".into(),
                email: Some("ada@example.org".into()),
                project: "FerroLoop".into(),
                role: AgentRole::Assistant,
                extra_areas: Vec::new(),
                create_area: true,
                replace: true,
                at: stamp::now(),
                by: "ada".into(),
            })
            .unwrap();
        let agent = || {
            state
                .open_store()
                .unwrap()
                .agent_by_name("ada")
                .unwrap()
                .unwrap()
        };
        let hosted = || {
            handle_http_request(
                &state,
                &authed(
                    "GET",
                    "/api/v1/hosted-sessions",
                    "owner-tok-secret",
                    &json!({}),
                ),
            )
            .json_value()
            .unwrap()["sessions"]
                .clone()
        };
        let agent_before = agent();
        let hosted_before = hosted();
        assert_eq!(agent_before.status, AgentStatus::Active);
        assert_eq!(hosted_before[0]["status"], "decommissioned");

        let resp = handle_http_request(
            &state,
            &authed(
                "POST",
                "/api/v1/sessions/spin-up",
                "peer-tok-secret",
                &spin_up_body("ada"),
            ),
        );
        assert_eq!(resp.status, 403, "{}", String::from_utf8_lossy(&resp.body));
        for mode in ["prepare", "teardown"] {
            let resp = decommission_as(&state, "peer-tok-secret", "claude%3Aada", mode);
            assert_eq!(
                resp.status,
                403,
                "{mode}: {}",
                String::from_utf8_lossy(&resp.body)
            );
        }
        assert_eq!(agent(), agent_before);
        assert_eq!(hosted(), hosted_before);

        // The Owner may still spin it up again.
        let resp = spin_up(&state, &spin_up_body("ada"));
        assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
        assert_eq!(hosted()[0]["status"], "running");
    }

    #[test]
    fn peer_teardown_needs_a_hosted_record_and_malformed_ids_are_400_without_side_effects() {
        use fl_core::agent::{AgentRole, AgentStatus};
        use fl_core::store::AgentRegistry;

        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);
        register_resident(&state, "jarvis", AgentRole::LabManager, "ferroloop");
        state.enqueue_session_prompt(
            "jarvis",
            Some("jarvis".into()),
            "queued".into(),
            "queued".into(),
            "owner".into(),
            Vec::new(),
        );
        let socket = tmp.path().join("jarvis.sock");
        install_webhook_route(tmp.path(), "jarvis", &socket);
        let routes_path = tmp.path().join(".cache/agent-bus/routes.json");
        let jarvis_is_untouched = || {
            assert_eq!(state.session_queue("jarvis").len(), 1);
            assert!(
                fl_wire::identity::RouteTable::load(Some(&routes_path))
                    .get("jarvis")
                    .is_some()
            );
            let store = state.open_store().unwrap();
            assert_eq!(
                store.agent_by_name("jarvis").unwrap().unwrap().status,
                AgentStatus::Active
            );
            drop(store);
            let listed = handle_http_request(
                &state,
                &authed(
                    "GET",
                    "/api/v1/hosted-sessions",
                    "owner-tok-secret",
                    &json!({}),
                ),
            )
            .json_value()
            .unwrap();
            assert!(listed["sessions"].as_array().unwrap().is_empty());
        };

        // A Peer cannot adopt-and-retire a registered agent through an unmanaged id.
        let resp = decommission_as(&state, "peer-tok-secret", "claude%3Ajarvis", "teardown");
        assert_eq!(resp.status, 403, "{}", String::from_utf8_lossy(&resp.body));
        jarvis_is_untouched();

        // Malformed persona ids are 400 for both phases, before any side effect.
        state.enqueue_session_prompt(
            "claude:Foo Bar",
            None,
            "queued".into(),
            "queued".into(),
            "owner".into(),
            Vec::new(),
        );
        for mode in ["prepare", "teardown"] {
            let resp = decommission_as(&state, "owner-tok-secret", "claude%3AFoo%20Bar", mode);
            assert_eq!(resp.status, 400, "{mode}");
        }
        assert_eq!(state.session_queue("claude:Foo Bar").len(), 1);
        jarvis_is_untouched();

        // A Peer may tear down a hosted session; the Owner may adopt an unmanaged one.
        assert_eq!(spin_up(&state, &spin_up_body("ada")).status, 200);
        assert_eq!(
            decommission_as(&state, "peer-tok-secret", "claude:ada", "teardown").status,
            200
        );
        assert_eq!(
            decommission_as(&state, "owner-tok-secret", "claude:jarvis", "teardown").status,
            200
        );
        let store = state.open_store().unwrap();
        assert_eq!(
            store.agent_by_name("jarvis").unwrap().unwrap().status,
            AgentStatus::Retired
        );
    }

    #[test]
    fn peer_prepare_of_an_unmanaged_session_is_403_and_cannot_unlock_teardown() {
        use fl_core::agent::{AgentRole, AgentStatus};
        use fl_core::store::AgentRegistry;

        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);
        register_resident(&state, "jarvis", AgentRole::LabManager, "ferroloop");
        let hosted_count = || {
            handle_http_request(
                &state,
                &authed(
                    "GET",
                    "/api/v1/hosted-sessions",
                    "owner-tok-secret",
                    &json!({}),
                ),
            )
            .json_value()
            .unwrap()["sessions"]
                .as_array()
                .unwrap()
                .len()
        };
        let jarvis_status = || {
            state
                .open_store()
                .unwrap()
                .agent_by_name("jarvis")
                .unwrap()
                .unwrap()
                .status
        };

        for mode in ["prepare", "teardown"] {
            let resp = decommission_as(&state, "peer-tok-secret", "claude%3Ajarvis", mode);
            assert_eq!(
                resp.status,
                403,
                "{mode}: {}",
                String::from_utf8_lossy(&resp.body)
            );
            assert_eq!(hosted_count(), 0, "{mode}");
            assert_eq!(jarvis_status(), AgentStatus::Active, "{mode}");
        }
        assert!(state.session_queue("claude:jarvis").is_empty());

        // The Owner may prepare (adopting the session); the Peer still cannot retire it.
        let resp = decommission_as(&state, "owner-tok-secret", "claude%3Ajarvis", "prepare");
        assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
        assert_eq!(hosted_count(), 1);
        assert_eq!(jarvis_status(), AgentStatus::Active);
    }

    #[test]
    fn a_decommissioned_session_cannot_be_prepared_or_restarted() {
        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);
        assert_eq!(spin_up(&state, &spin_up_body("ada")).status, 200);
        assert_eq!(
            decommission_as(&state, "owner-tok-secret", "claude:ada", "teardown").status,
            200
        );
        assert_eq!(
            decommission_as(&state, "owner-tok-secret", "claude:ada", "prepare").status,
            409
        );
        let restart = handle_http_request(
            &state,
            &authed(
                "POST",
                "/api/v1/hosted-sessions/claude:ada/restart",
                "owner-tok-secret",
                &json!({}),
            ),
        );
        assert_eq!(restart.status, 409);
        assert!(state.session_queue("claude:ada").is_empty());
        let listed = handle_http_request(
            &state,
            &authed(
                "GET",
                "/api/v1/hosted-sessions",
                "owner-tok-secret",
                &json!({}),
            ),
        )
        .json_value()
        .unwrap();
        assert_eq!(listed["sessions"][0]["status"], "decommissioned");
        assert_eq!(listed["sessions"][0]["restart_count"], 0);
        // Teardown stays repeatable.
        assert_eq!(
            decommission_as(&state, "owner-tok-secret", "claude:ada", "teardown").status,
            200
        );
    }

    #[test]
    fn hosted_sessions_list_sorted_and_restart_increments_count() {
        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);
        for (persona, vendor) in [("zed", "codex"), ("ada", "muse"), ("bob", "claude")] {
            let mut body = spin_up_body(persona);
            body["vendor"] = json!(vendor);
            assert_eq!(spin_up(&state, &body).status, 200);
        }
        let get = |token: Option<&str>| {
            let req = HttpRequest::new("GET", "/api/v1/hosted-sessions");
            let req = match token {
                Some(t) => req.with_header("Authorization", format!("Bearer {t}")),
                None => req,
            };
            handle_http_request(&state, &req)
        };
        assert_eq!(get(None).status, 401);
        for token in ["owner-tok-secret", "peer-tok-secret"] {
            let listed = get(Some(token)).json_value().unwrap();
            let ids: Vec<&str> = listed["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["session_id"].as_str().unwrap())
                .collect();
            assert_eq!(ids, ["claude:bob", "codex:zed", "muse:ada"]);
        }

        let restart = |token: Option<&str>, id: &str| {
            let req = HttpRequest::new("POST", format!("/api/v1/hosted-sessions/{id}/restart"));
            let req = match token {
                Some(t) => req.with_header("Authorization", format!("Bearer {t}")),
                None => req,
            };
            handle_http_request(&state, &req)
        };
        assert_eq!(restart(None, "claude:bob").status, 401);
        assert_eq!(restart(Some("peer-tok-secret"), "claude:bob").status, 403);
        assert_eq!(
            restart(Some("owner-tok-secret"), "claude:nobody").status,
            404
        );

        let first = restart(Some("owner-tok-secret"), "claude%3Abob");
        assert_eq!(
            first.status,
            200,
            "{}",
            String::from_utf8_lossy(&first.body)
        );
        let out = first.json_value().unwrap();
        assert_eq!(out["session_id"], "claude:bob");
        assert_eq!(out["restart_count"], 1);
        assert_eq!(out["status"], "running");
        assert!(
            out["prompt"]
                .as_str()
                .unwrap()
                .contains("take up residence in FerroLoop/")
        );
        let second = restart(Some("owner-tok-secret"), "claude:bob")
            .json_value()
            .unwrap();
        assert_eq!(second["restart_count"], 2);

        let listed = get(Some("owner-tok-secret")).json_value().unwrap();
        assert_eq!(listed["sessions"][0]["restart_count"], 2);
        assert_eq!(listed["sessions"][1]["restart_count"], 0);
    }

    #[test]
    fn session_reforge_sends_canonical_prompt() {
        use fl_core::agent::{AgentRole, RegisterAgentArgs};
        use fl_core::store::AgentRegistry;

        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);
        {
            let store = state.open_store().unwrap();
            store
                .register_agent(RegisterAgentArgs {
                    display_name: "Felicia".into(),
                    email: None,
                    project: "ferroloop".into(),
                    role: AgentRole::Assistant,
                    extra_areas: vec![],
                    create_area: true,
                    replace: true,
                    at: stamp::now(),
                    by: "owner".into(),
                })
                .unwrap();
            store
                .register_agent(RegisterAgentArgs {
                    display_name: "Scribe".into(),
                    email: None,
                    project: "ferroloop".into(),
                    role: AgentRole::Custom("scribe".into()),
                    extra_areas: vec![],
                    create_area: false,
                    replace: true,
                    at: stamp::now(),
                    by: "owner".into(),
                })
                .unwrap();
        }
        assert_eq!(
            api::reforge_role_label(&AgentRole::Custom("scribe".into())),
            "scribe"
        );
        let expected = api::canonical_reforge_prompt("Felicia", "ferroloop", "assistant");
        assert!(expected.contains("AGENT_PERSONA=felicia agent-msg register Felicia --project ferroloop --role assistant --replace"));
        assert!(expected.contains("Co-authored-by: Felicia <felicia@artificialhumanity.io>"));
        assert!(
            api::canonical_reforge_prompt("Scribe", "ferroloop", "scribe")
                .contains(".agents/roles/scribe.md")
        );

        state.enqueue_session_prompt(
            "felicia",
            Some("felicia".into()),
            "do not send after reforge".into(),
            "do not send after reforge".into(),
            "owner".into(),
            Vec::new(),
        );
        let socket = tmp.path().join("reforge.sock");
        install_webhook_route(tmp.path(), "felicia", &socket);
        let lines = collect_webhook_lines(socket, 2);
        let mut frames = state.subscribe();

        let resp = api::with_hydrate_lock(|| {
            handle_http_request(
                &state,
                &HttpRequest::new("POST", "/api/v1/sessions/felicia/clear")
                    .with_header("Authorization", "Bearer owner-tok-secret")
                    .with_json_body(&json!({ "mode": "reforge" })),
            )
        });
        assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
        let body = resp.json_value().unwrap();
        assert_eq!(body["ok"], true);
        assert_eq!(body["mode"], "reforge");
        assert_eq!(body["persona"], "felicia");
        assert_eq!(body["prompt"], expected);
        assert!(state.session_queue("felicia").is_empty());

        let frame = frames.try_recv().unwrap();
        assert_eq!(frame.event, "session.cleared");
        assert_eq!(frame.channel, "sessions");
        assert_eq!(frame.data["prompt"], expected);

        let delivered = lines.join().unwrap();
        let first: Value = serde_json::from_str(&delivered[0]).unwrap();
        let second: Value = serde_json::from_str(&delivered[1]).unwrap();
        assert_eq!(first["body"], "/clear");
        assert_eq!(second["body"], expected);

        let alias = handle_http_request(
            &state,
            &HttpRequest::new("POST", "/api/v1/sessions/nobody/clear")
                .with_header("Authorization", "Bearer owner-tok-secret")
                .with_json_body(&json!({ "mode": "rehydrate" })),
        );
        assert_eq!(alias.status, 404);
    }

    #[test]
    fn session_reforge_uses_agent_hydrate_bin_when_set() {
        use fl_core::agent::{AgentRole, RegisterAgentArgs};
        use fl_core::store::AgentRegistry;
        use std::os::unix::fs::PermissionsExt;

        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);
        {
            let store = state.open_store().unwrap();
            store
                .register_agent(RegisterAgentArgs {
                    display_name: "Felicia".into(),
                    email: None,
                    project: "ferroloop".into(),
                    role: AgentRole::Assistant,
                    extra_areas: vec![],
                    create_area: true,
                    replace: true,
                    at: stamp::now(),
                    by: "owner".into(),
                })
                .unwrap();
        }
        let script = tmp.path().join("agent-hydrate");
        fs::write(
            &script,
            "#!/bin/sh\nprintf '%s\\n' '{\"Felicia\":\"HYDRATE FROM BIN\"}'\n",
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let socket = tmp.path().join("hydrate.sock");
        install_webhook_route(tmp.path(), "felicia", &socket);
        let lines = collect_webhook_lines(socket, 2);

        let resp = api::with_hydrate_lock(|| {
            // SAFETY: the hydrate lock serializes writers of FL_HUB_AGENT_HYDRATE_BIN.
            let previous = std::env::var_os("FL_HUB_AGENT_HYDRATE_BIN");
            unsafe {
                std::env::set_var("FL_HUB_AGENT_HYDRATE_BIN", &script);
            }
            let resp = handle_http_request(
                &state,
                &HttpRequest::new("POST", "/api/v1/sessions/felicia/clear")
                    .with_header("Authorization", "Bearer owner-tok-secret")
                    .with_json_body(&json!({ "mode": "rehydrate" })),
            );
            unsafe {
                match previous {
                    Some(value) => std::env::set_var("FL_HUB_AGENT_HYDRATE_BIN", value),
                    None => std::env::remove_var("FL_HUB_AGENT_HYDRATE_BIN"),
                }
            }
            resp
        });
        assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
        let body = resp.json_value().unwrap();
        assert_eq!(body["mode"], "reforge");
        assert_eq!(body["prompt"], "HYDRATE FROM BIN");
        let delivered = lines.join().unwrap();
        let second: Value = serde_json::from_str(&delivered[1]).unwrap();
        assert_eq!(second["body"], "HYDRATE FROM BIN");
    }

    #[test]
    fn session_reforge_ignores_hydrate_prompt_for_other_persona() {
        use fl_core::agent::{AgentRole, RegisterAgentArgs};
        use fl_core::store::AgentRegistry;
        use std::os::unix::fs::PermissionsExt;

        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);
        {
            let store = state.open_store().unwrap();
            store
                .register_agent(RegisterAgentArgs {
                    display_name: "Felicia".into(),
                    email: None,
                    project: "ferroloop".into(),
                    role: AgentRole::Assistant,
                    extra_areas: vec![],
                    create_area: true,
                    replace: true,
                    at: stamp::now(),
                    by: "owner".into(),
                })
                .unwrap();
        }
        let expected = api::canonical_reforge_prompt("Felicia", "ferroloop", "assistant");
        let script = tmp.path().join("agent-hydrate");
        fs::write(
            &script,
            "#!/bin/sh\nprintf '%s\\n' '{\"Other\":\"WRONG\"}'\n",
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let socket = tmp.path().join("hydrate-other.sock");
        install_webhook_route(tmp.path(), "felicia", &socket);
        let lines = collect_webhook_lines(socket, 2);

        let resp = api::with_hydrate_lock(|| {
            // SAFETY: the hydrate lock serializes writers of FL_HUB_AGENT_HYDRATE_BIN.
            let previous = std::env::var_os("FL_HUB_AGENT_HYDRATE_BIN");
            unsafe {
                std::env::set_var("FL_HUB_AGENT_HYDRATE_BIN", &script);
            }
            let resp = handle_http_request(
                &state,
                &HttpRequest::new("POST", "/api/v1/sessions/felicia/clear")
                    .with_header("Authorization", "Bearer owner-tok-secret")
                    .with_json_body(&json!({ "mode": "reforge" })),
            );
            unsafe {
                match previous {
                    Some(value) => std::env::set_var("FL_HUB_AGENT_HYDRATE_BIN", value),
                    None => std::env::remove_var("FL_HUB_AGENT_HYDRATE_BIN"),
                }
            }
            resp
        });
        assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
        let body = resp.json_value().unwrap();
        assert_eq!(body["prompt"], expected);
        assert!(!body["prompt"].as_str().unwrap().contains("WRONG"));
        let delivered = lines.join().unwrap();
        let second: Value = serde_json::from_str(&delivered[1]).unwrap();
        assert_eq!(second["body"], expected);
    }

    #[test]
    fn session_reforge_hydrate_returns_canonical_when_stdout_stays_open() {
        use fl_core::agent::{AgentRole, RegisterAgentArgs};
        use fl_core::store::AgentRegistry;
        use std::os::unix::fs::PermissionsExt;

        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);
        {
            let store = state.open_store().unwrap();
            store
                .register_agent(RegisterAgentArgs {
                    display_name: "Felicia".into(),
                    email: None,
                    project: "ferroloop".into(),
                    role: AgentRole::Assistant,
                    extra_areas: vec![],
                    create_area: true,
                    replace: true,
                    at: stamp::now(),
                    by: "owner".into(),
                })
                .unwrap();
        }
        let expected = api::canonical_reforge_prompt("Felicia", "ferroloop", "assistant");
        let script = tmp.path().join("agent-hydrate");
        fs::write(&script, "#!/bin/sh\nsleep 5 &\nexit 0\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let socket = tmp.path().join("hydrate-hang.sock");
        install_webhook_route(tmp.path(), "felicia", &socket);
        let lines = collect_webhook_lines(socket, 2);

        let started = std::time::Instant::now();
        let resp = api::with_hydrate_lock(|| {
            // SAFETY: the hydrate lock serializes writers of FL_HUB_AGENT_HYDRATE_BIN.
            let previous = std::env::var_os("FL_HUB_AGENT_HYDRATE_BIN");
            unsafe {
                std::env::set_var("FL_HUB_AGENT_HYDRATE_BIN", &script);
            }
            let resp = handle_http_request(
                &state,
                &HttpRequest::new("POST", "/api/v1/sessions/felicia/clear")
                    .with_header("Authorization", "Bearer owner-tok-secret")
                    .with_json_body(&json!({ "mode": "reforge" })),
            );
            unsafe {
                match previous {
                    Some(value) => std::env::set_var("FL_HUB_AGENT_HYDRATE_BIN", value),
                    None => std::env::remove_var("FL_HUB_AGENT_HYDRATE_BIN"),
                }
            }
            resp
        });
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_millis(4500),
            "reforge waited {elapsed:?} for a descendant holding hydrate stdout"
        );
        assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
        let body = resp.json_value().unwrap();
        assert_eq!(body["prompt"], expected);
        let delivered = lines.join().unwrap();
        let second: Value = serde_json::from_str(&delivered[1]).unwrap();
        assert_eq!(second["body"], expected);
    }

    fn http_call(
        state: &HubState,
        method: &str,
        path: &str,
        bearer: Option<&str>,
        body: Option<&Value>,
    ) -> HttpResponse {
        let mut req = HttpRequest::new(method, path);
        if let Some(token) = bearer {
            req = req.with_header("Authorization", format!("Bearer {token}"));
        }
        if let Some(body) = body {
            req = req.with_json_body(body);
        }
        handle_http_request(state, &req)
    }

    fn response_text(resp: &HttpResponse) -> String {
        String::from_utf8_lossy(&resp.body).into_owned()
    }

    #[test]
    fn http_nodes_enrollment_heartbeat_and_revoke() {
        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);
        let owner = Some("owner-tok-secret");
        let peer = Some("peer-tok-secret");

        let unauth = http_call(&state, "GET", "/api/v1/nodes", None, None);
        assert_eq!(unauth.status, 401, "{}", response_text(&unauth));

        let owner_list = http_call(&state, "GET", "/api/v1/nodes", owner, None);
        assert_eq!(owner_list.status, 200, "{}", response_text(&owner_list));
        let owner_raw = response_text(&owner_list);
        assert!(!owner_raw.contains("token_hash"), "{owner_raw}");
        let owner_json = owner_list.json_value().unwrap();
        let hub = owner_json["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|node| node["role"].as_str() == Some("hub"))
            .expect("hub node");
        assert_eq!(hub["status"], "online");
        assert_eq!(
            hub["listen_addr"].as_str(),
            Some(state.config.listen.as_str())
        );
        assert_eq!(hub["hostname"], local_hostname());
        assert_eq!(hub["os"], std::env::consts::OS);
        assert_eq!(hub["arch"], std::env::consts::ARCH);
        assert!(hub["capabilities"].is_array());
        let hub_id = hub["node_id"].as_str().unwrap().to_string();
        assert_eq!(owner_json["nodes"].as_array().unwrap().len(), 1);

        let hub_code_resp = http_call(&state, "POST", "/api/v1/nodes/enroll-token", owner, None);
        assert_eq!(
            hub_code_resp.status,
            200,
            "{}",
            response_text(&hub_code_resp)
        );
        let hub_code = hub_code_resp.json_value().unwrap()["code"]
            .as_str()
            .unwrap()
            .to_string();
        let over_hub = http_call(
            &state,
            "POST",
            "/api/v1/nodes/enroll",
            None,
            Some(&json!({
                "code": hub_code,
                "node_id": hub_id,
                "display_name": "Not the hub",
                "hostname": "worker-host",
                "os": "linux",
                "arch": "x86_64"
            })),
        );
        assert_eq!(over_hub.status, 400, "{}", response_text(&over_hub));
        let spare_after_hub = http_call(
            &state,
            "POST",
            "/api/v1/nodes/enroll",
            None,
            Some(&json!({
                "code": hub_code,
                "node_id": "worker-after-hub",
                "display_name": "After Hub",
                "hostname": "worker-host",
                "os": "linux",
                "arch": "x86_64"
            })),
        );
        assert_eq!(
            spare_after_hub.status,
            200,
            "{}",
            response_text(&spare_after_hub)
        );

        let peer_list = http_call(&state, "GET", "/api/v1/nodes", peer, None);
        assert_eq!(peer_list.status, 200, "{}", response_text(&peer_list));

        let peer_mint = http_call(
            &state,
            "POST",
            "/api/v1/nodes/enroll-token",
            peer,
            Some(&json!({ "label": "mac", "ttl_secs": 600 })),
        );
        assert_eq!(peer_mint.status, 403, "{}", response_text(&peer_mint));
        assert!(response_text(&peer_mint).contains("owner authority required"));

        let labeled = http_call(
            &state,
            "POST",
            "/api/v1/nodes/enroll-token",
            owner,
            Some(&json!({ "label": "mac", "ttl_secs": 600 })),
        );
        assert_eq!(labeled.status, 200, "{}", response_text(&labeled));
        let labeled_json = labeled.json_value().unwrap();
        let labeled_code = labeled_json["code"].as_str().unwrap().to_string();
        let join_command = labeled_json["join_command"].as_str().unwrap();
        assert!(join_command.contains("fl hub join"), "{join_command}");
        assert!(join_command.contains("--token"), "{join_command}");
        assert!(join_command.contains(&labeled_code), "{join_command}");
        assert!(join_command.contains("--label"), "{join_command}");

        let blank = http_call(&state, "POST", "/api/v1/nodes/enroll-token", owner, None);
        assert_eq!(blank.status, 200, "{}", response_text(&blank));
        let blank_json = blank.json_value().unwrap();
        assert_eq!(blank_json["ttl_secs"], 600);
        let blank_code = blank_json["code"].as_str().unwrap().to_string();

        let worker_body = json!({
            "code": blank_code,
            "node_id": "worker-a",
            "display_name": "Worker A",
            "hostname": "worker-host",
            "os": "linux",
            "arch": "x86_64",
            "listen_addr": null,
            "capabilities": ["git", "tmux"],
            "labels": ["bench"]
        });
        let enrolled = http_call(
            &state,
            "POST",
            "/api/v1/nodes/enroll",
            None,
            Some(&worker_body),
        );
        assert_eq!(enrolled.status, 200, "{}", response_text(&enrolled));
        let enrolled_json = enrolled.json_value().unwrap();
        assert_eq!(enrolled_json["node_id"], "worker-a");
        assert_eq!(
            enrolled_json["hub_url"],
            format!("http://{}", state.config.listen)
        );
        let node_token = enrolled_json["node_token"].as_str().unwrap().to_string();
        assert!(!node_token.is_empty());

        let again = http_call(
            &state,
            "POST",
            "/api/v1/nodes/enroll",
            None,
            Some(&worker_body),
        );
        assert_eq!(again.status, 409, "{}", response_text(&again));
        assert!(response_text(&again).contains("already used"));

        let listed = http_call(&state, "GET", "/api/v1/nodes", owner, None);
        assert_eq!(listed.status, 200, "{}", response_text(&listed));
        assert!(!response_text(&listed).contains("token_hash"));
        let listed_nodes = listed.json_value().unwrap()["nodes"]
            .as_array()
            .unwrap()
            .clone();
        assert!(listed_nodes.iter().any(|node| node["node_id"] == hub_id));
        let worker = listed_nodes
            .iter()
            .find(|node| node["node_id"] == "worker-a")
            .expect("enrolled worker");
        assert_eq!(worker["capabilities"], json!(["git", "tmux"]));

        let node_get = http_call(
            &state,
            "GET",
            "/api/v1/nodes",
            Some(node_token.as_str()),
            None,
        );
        assert_eq!(node_get.status, 401, "{}", response_text(&node_get));

        let beat = http_call(
            &state,
            "POST",
            "/api/v1/nodes/worker-a/heartbeat",
            Some(node_token.as_str()),
            Some(&json!({ "capabilities": ["muse"] })),
        );
        assert_eq!(beat.status, 200, "{}", response_text(&beat));
        let beat_json = beat.json_value().unwrap();
        assert_eq!(beat_json["node_id"], "worker-a");
        assert_eq!(beat_json["status"], "online");
        assert!(beat_json["last_seen_at_ms"].as_u64().unwrap() > 0);

        let owner_beat = http_call(
            &state,
            "POST",
            "/api/v1/nodes/worker-a/heartbeat",
            owner,
            None,
        );
        assert_eq!(owner_beat.status, 200, "{}", response_text(&owner_beat));
        let peer_beat = http_call(
            &state,
            "POST",
            "/api/v1/nodes/worker-a/heartbeat",
            peer,
            None,
        );
        assert_eq!(peer_beat.status, 200, "{}", response_text(&peer_beat));

        let missing_beat = http_call(
            &state,
            "POST",
            "/api/v1/nodes/missing-node/heartbeat",
            owner,
            None,
        );
        assert_eq!(missing_beat.status, 404, "{}", response_text(&missing_beat));

        let after_beat = http_call(&state, "GET", "/api/v1/nodes", owner, None);
        let after_nodes = after_beat.json_value().unwrap()["nodes"]
            .as_array()
            .unwrap()
            .clone();
        let worker = after_nodes
            .iter()
            .find(|node| node["node_id"] == "worker-a")
            .expect("worker after heartbeat");
        assert_eq!(worker["capabilities"], json!(["muse"]));

        let wrong = http_call(
            &state,
            "POST",
            "/api/v1/nodes/worker-a/heartbeat",
            Some("not-the-token"),
            None,
        );
        assert_eq!(wrong.status, 401, "{}", response_text(&wrong));

        let deny_hub = http_call(
            &state,
            "DELETE",
            &format!("/api/v1/nodes/{hub_id}"),
            owner,
            None,
        );
        assert_eq!(deny_hub.status, 400, "{}", response_text(&deny_hub));
        assert!(response_text(&deny_hub).contains("hub"));
        let still = http_call(&state, "GET", "/api/v1/nodes", owner, None);
        let still_nodes = still.json_value().unwrap()["nodes"]
            .as_array()
            .unwrap()
            .clone();
        assert!(still_nodes.iter().any(|node| node["node_id"] == hub_id));

        let removed = http_call(&state, "DELETE", "/api/v1/nodes/worker-a", owner, None);
        assert_eq!(removed.status, 200, "{}", response_text(&removed));
        let removed_json = removed.json_value().unwrap();
        assert_eq!(removed_json["revoked"], true);
        assert_eq!(removed_json["node_id"], "worker-a");
        let hidden = http_call(&state, "GET", "/api/v1/nodes", owner, None);
        let hidden_nodes = hidden.json_value().unwrap()["nodes"]
            .as_array()
            .unwrap()
            .clone();
        assert!(
            hidden_nodes
                .iter()
                .all(|node| node["node_id"] != "worker-a")
        );
        assert!(hidden_nodes.iter().any(|node| node["node_id"] == hub_id));

        let revoked_beat = http_call(
            &state,
            "POST",
            "/api/v1/nodes/worker-a/heartbeat",
            Some(node_token.as_str()),
            None,
        );
        assert_eq!(revoked_beat.status, 403, "{}", response_text(&revoked_beat));
        assert!(response_text(&revoked_beat).contains("revoked"));

        let second_body = json!({
            "code": labeled_code,
            "node_id": "worker-b",
            "display_name": "Worker B",
            "hostname": "other-host",
            "os": "linux",
            "arch": "aarch64",
            "capabilities": ["git"],
            "labels": ["edge"]
        });
        let second = http_call(
            &state,
            "POST",
            "/api/v1/nodes/enroll",
            None,
            Some(&second_body),
        );
        assert_eq!(second.status, 200, "{}", response_text(&second));
        let peer_delete = http_call(&state, "DELETE", "/api/v1/nodes/worker-b", peer, None);
        assert_eq!(peer_delete.status, 403, "{}", response_text(&peer_delete));
        assert!(response_text(&peer_delete).contains("owner authority required"));
        let kept = http_call(&state, "GET", "/api/v1/nodes", owner, None);
        let kept_nodes = kept.json_value().unwrap()["nodes"]
            .as_array()
            .unwrap()
            .clone();
        assert!(kept_nodes.iter().any(|node| node["node_id"] == "worker-b"));

        let expired_code = {
            let store = state.open_store().unwrap();
            mint_enroll_token(&store, None, 60, "http://127.0.0.1:9", 0)
                .unwrap()
                .code
        };
        let expired_body = json!({
            "code": expired_code,
            "node_id": "worker-expired",
            "display_name": "Expired",
            "hostname": "old-host",
            "os": "linux",
            "arch": "x86_64"
        });
        let expired = http_call(
            &state,
            "POST",
            "/api/v1/nodes/enroll",
            None,
            Some(&expired_body),
        );
        assert_eq!(expired.status, 401, "{}", response_text(&expired));
        assert!(response_text(&expired).contains("expired"));

        let wrong_code = http_call(
            &state,
            "POST",
            "/api/v1/nodes/enroll",
            None,
            Some(&json!({
                "code": "not-a-real-code",
                "node_id": "worker-bad-code",
                "display_name": "Bad Code",
                "hostname": "worker-host",
                "os": "linux",
                "arch": "x86_64"
            })),
        );
        assert_eq!(wrong_code.status, 401, "{}", response_text(&wrong_code));
        assert!(response_text(&wrong_code).contains("invalid"));
    }

    #[test]
    fn http_enroll_local_hostname_before_get_rejects_and_keeps_code() {
        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);
        let owner = Some("owner-tok-secret");
        let minted = http_call(&state, "POST", "/api/v1/nodes/enroll-token", owner, None);
        assert_eq!(minted.status, 200, "{}", response_text(&minted));
        let code = minted.json_value().unwrap()["code"]
            .as_str()
            .unwrap()
            .to_string();
        let host_id = local_hostname();
        let rejected = http_call(
            &state,
            "POST",
            "/api/v1/nodes/enroll",
            None,
            Some(&json!({
                "code": code,
                "node_id": host_id,
                "display_name": "Taken Host",
                "hostname": "worker-host",
                "os": "linux",
                "arch": "x86_64"
            })),
        );
        assert_eq!(rejected.status, 400, "{}", response_text(&rejected));

        let spare_id = if host_id.eq_ignore_ascii_case("worker-after-host") {
            "worker-after-host-b"
        } else {
            "worker-after-host"
        };
        let enrolled = http_call(
            &state,
            "POST",
            "/api/v1/nodes/enroll",
            None,
            Some(&json!({
                "code": code,
                "node_id": spare_id,
                "display_name": "Spare Worker",
                "hostname": "worker-host",
                "os": "linux",
                "arch": "x86_64"
            })),
        );
        assert_eq!(enrolled.status, 200, "{}", response_text(&enrolled));

        let listed = http_call(&state, "GET", "/api/v1/nodes", owner, None);
        assert_eq!(listed.status, 200, "{}", response_text(&listed));
        let nodes = listed.json_value().unwrap()["nodes"]
            .as_array()
            .unwrap()
            .clone();
        assert!(nodes.iter().any(|node| node["role"] == "hub"));
    }

    // ---- Track 1 session control: interrupt, model, bulk-action, commands -----------------

    /// Add `route` for `persona` to the route table under `home` (keeps earlier entries).
    fn install_route(home: &std::path::Path, persona: &str, route: fl_wire::identity::VendorRoute) {
        use fl_core::at::At;
        use fl_wire::identity::RouteTable;

        let path = home.join(".cache").join("agent-bus").join("routes.json");
        let mut routes = RouteTable::load(Some(&path));
        routes.upsert(persona, route, At::from_unix_millis(1));
        routes.save(Some(&path)).unwrap();
    }

    fn muse_route(session_id: &str, tmux_target: Option<&str>) -> fl_wire::identity::VendorRoute {
        fl_wire::identity::VendorRoute::Muse {
            session_id: session_id.into(),
            cwd: None,
            tmux_target: tmux_target.map(str::to_string),
            tmux_pane: None,
        }
    }

    fn webhook_route(socket: &std::path::Path) -> fl_wire::identity::VendorRoute {
        fl_wire::identity::VendorRoute::Webhook {
            socket_path: socket.to_path_buf(),
        }
    }

    fn call(state: &HubState, method: &str, path: &str, token: &str, body: &Value) -> HttpResponse {
        handle_http_request(state, &authed(method, path, token, body))
    }

    const OWNER: &str = "owner-tok-secret";
    const PEER: &str = "peer-tok-secret";

    fn error_text(resp: &HttpResponse) -> String {
        resp.json_value().unwrap()["error"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    #[test]
    fn interrupt_endpoint_gates_resolves_and_reports_the_method() {
        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);
        install_route(tmp.path(), "ada", muse_route("ms-ada", Some("muse:ada")));
        let url = "/api/v1/sessions/ada/interrupt";

        let unauth = handle_http_request(
            &state,
            &HttpRequest::new("POST", url).with_json_body(&json!({})),
        );
        assert_eq!(unauth.status, 401);

        let unknown = call(
            &state,
            "POST",
            "/api/v1/sessions/nobody/interrupt",
            OWNER,
            &json!({}),
        );
        assert_eq!(unknown.status, 404, "{}", error_text(&unknown));

        // Mark the session busy, so the interrupt has a flag to clear.
        state.mark_session_in_flight_from_turns("ms-ada", &[]);
        assert!(state.is_session_busy_from_turns("ms-ada", &[]));
        let mut frames = state.subscribe();

        let ok = call(&state, "POST", url, OWNER, &json!({ "reason": "stuck" }));
        assert_eq!(ok.status, 200, "{}", error_text(&ok));
        assert_eq!(
            ok.json_value().unwrap(),
            json!({
                "session_id": "ms-ada",
                "interrupted": true,
                "vendor": "muse",
                "method": "noop-test",
            })
        );
        assert!(!state.is_session_busy_from_turns("ms-ada", &[]));
        let frame = frames.try_recv().unwrap();
        assert_eq!(
            (frame.channel.as_str(), frame.event.as_str()),
            ("sessions", "session.interrupted")
        );
        assert_eq!(frame.data["reason"], "stuck");
        assert_eq!(frame.data["persona"], "ada");

        // `{}`, an empty body, a null reason, the canonical id and a Peer token all work.
        for (token, target, req_body) in [
            (PEER, "ada", Some(json!({}))),
            (OWNER, "ms-ada", None),
            (OWNER, "ada", Some(json!({ "reason": null }))),
        ] {
            let mut req = HttpRequest::new("POST", format!("/api/v1/sessions/{target}/interrupt"))
                .with_header("Authorization", format!("Bearer {token}"));
            if let Some(b) = &req_body {
                req = req.with_json_body(b);
            }
            let resp = handle_http_request(&state, &req);
            assert_eq!(resp.status, 200, "{target}: {}", error_text(&resp));
            assert_eq!(resp.json_value().unwrap()["session_id"], "ms-ada");
        }

        let bad_reason = call(&state, "POST", url, OWNER, &json!({ "reason": 7 }));
        assert_eq!(bad_reason.status, 400);
        let bad_body = handle_http_request(
            &state,
            &HttpRequest::new("POST", url)
                .with_header("Authorization", "Bearer owner-tok-secret")
                .with_body(b"{nope".to_vec()),
        );
        assert_eq!(bad_body.status, 400);
    }

    #[test]
    fn interrupt_endpoint_is_502_when_the_route_cannot_be_interrupted() {
        use fl_wire::identity::VendorRoute;

        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);
        // A webhook route, a tmux vendor with no target, and a Claude route with no hosted pane.
        install_route(
            tmp.path(),
            "hook",
            webhook_route(&tmp.path().join("hook.sock")),
        );
        install_route(tmp.path(), "blind", muse_route("ms-blind", None));
        install_route(
            tmp.path(),
            "grace",
            VendorRoute::Claude {
                pid: 0,
                session_id: "cs-grace".into(),
                socket_path: tmp.path().join("grace.sock"),
                key_path: Some(tmp.path().join("grace.key")),
            },
        );
        for persona in ["hook", "blind", "grace"] {
            let resp = call(
                &state,
                "POST",
                &format!("/api/v1/sessions/{persona}/interrupt"),
                OWNER,
                &json!({}),
            );
            assert_eq!(resp.status, 502, "{persona}: {}", error_text(&resp));
            assert!(!error_text(&resp).is_empty(), "{persona}");
        }
    }

    #[test]
    fn interrupt_endpoint_uses_the_key_file_pane_of_a_claude_session_the_hub_does_not_host() {
        let _lock = TMUX_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);
        let tmux = FakeTmux::install(tmp.path());
        let _listener = install_claude_tmux_route(tmp.path(), "felicity");
        let post = |persona: &str| {
            call(
                &state,
                "POST",
                &format!("/api/v1/sessions/{persona}/interrupt"),
                OWNER,
                &json!({}),
            )
        };

        // No hosted record, but the key file names a live pane: the interrupt has a target.
        let resp = post("felicity");
        assert_eq!(resp.status, 200, "{}", error_text(&resp));
        let data = resp.json_value().unwrap();
        assert_eq!(data["vendor"], "claude");
        assert_eq!(data["method"], "noop-test");
        assert!(
            tmux.log().contains("display-message -p -t %12"),
            "{}",
            tmux.log()
        );

        // A key file whose pane is a session name (not a `%n` pane id) or that is missing is
        // still a 502.
        fs::write(
            tmp.path().join("felicity.key"),
            json!({ "peerToken": "t", "tmux": "Felicity:@12" }).to_string(),
        )
        .unwrap();
        assert_eq!(post("felicity").status, 502);
        fs::remove_file(tmp.path().join("felicity.key")).unwrap();
        assert_eq!(post("felicity").status, 502);
    }

    #[test]
    fn interrupt_endpoint_finds_a_claude_pane_in_the_hosted_record_and_handles_antigravity() {
        use fl_wire::identity::VendorRoute;

        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);
        assert_eq!(spin_up(&state, &spin_up_body("felicity")).status, 200);
        install_route(
            tmp.path(),
            "felicity",
            VendorRoute::Claude {
                pid: 0,
                session_id: "cs-felicity".into(),
                socket_path: tmp.path().join("felicity.sock"),
                key_path: Some(tmp.path().join("felicity.key")),
            },
        );
        install_route(
            tmp.path(),
            "agy-1",
            VendorRoute::Agy {
                conversation_id: "conv-1".into(),
                ls_pid: None,
                ls_address: None,
                csrf_token: None,
                agentapi_exe: None,
            },
        );
        let claude = call(
            &state,
            "POST",
            "/api/v1/sessions/felicity/interrupt",
            OWNER,
            &json!({}),
        );
        assert_eq!(claude.status, 200, "{}", error_text(&claude));
        let claude = claude.json_value().unwrap();
        assert_eq!(claude["vendor"], "claude");
        assert_eq!(claude["method"], "noop-test");
        assert_eq!(claude["session_id"], "cs-felicity");

        let agy = call(
            &state,
            "POST",
            "/api/v1/sessions/agy-1/interrupt",
            OWNER,
            &json!({}),
        );
        assert_eq!(agy.status, 200, "{}", error_text(&agy));
        let agy = agy.json_value().unwrap();
        assert_eq!(agy["vendor"], "antigravity");
        assert_eq!(agy["method"], "noop-test");
        assert_eq!(agy["session_id"], "conv-1");
    }

    #[test]
    fn model_endpoint_validates_overrides_the_summary_and_dispatches_the_command() {
        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);
        let socket = tmp.path().join("felicia.sock");
        install_route(tmp.path(), "felicia", webhook_route(&socket));
        let lines = collect_webhook_lines(socket, 2);
        let url = "/api/v1/sessions/felicia/model";

        let unauth = handle_http_request(
            &state,
            &HttpRequest::new("POST", url).with_json_body(&json!({ "model": "m" })),
        );
        assert_eq!(unauth.status, 401);

        let too_long = "m".repeat(129);
        for bad in [
            json!({}),
            json!({ "model": "" }),
            json!({ "model": "   " }),
            json!({ "model": "two\nlines" }),
            json!({ "model": "tab\there" }),
            json!({ "model": "esc\u{1b}[0m" }),
            json!({ "model": too_long }),
            json!({ "model": 5 }),
            json!({ "model": "ok", "thinking_level": "extreme" }),
            json!({ "model": "ok", "thinking_level": "" }),
            json!({ "model": "ok", "thinking_level": 3 }),
        ] {
            let resp = call(&state, "POST", url, OWNER, &bad);
            assert_eq!(resp.status, 400, "{bad}");
        }
        let unknown = call(
            &state,
            "POST",
            "/api/v1/sessions/nobody/model",
            OWNER,
            &json!({ "model": "gpt-6-sol" }),
        );
        assert_eq!(unknown.status, 404);
        // Nothing from the rejected requests was recorded.
        assert!(state.session_model_override("webhook-felicia").is_none());
        assert!(state.session_model_override("nobody").is_none());

        let mut frames = state.subscribe();
        let ok = call(
            &state,
            "POST",
            url,
            OWNER,
            &json!({ "model": "  claude-opus-5-5 ", "thinking_level": "HIGH" }),
        );
        assert_eq!(ok.status, 200, "{}", error_text(&ok));
        assert_eq!(
            ok.json_value().unwrap(),
            json!({
                "ok": true,
                "session_id": "webhook-felicia",
                "vendor": "webhook",
                "model": "claude-opus-5-5",
                "thinking_level": "high",
                "switched": true,
                "method": "operator-prompt",
            })
        );
        let frame = frames.try_recv().unwrap();
        assert_eq!(
            (frame.channel.as_str(), frame.event.as_str()),
            ("sessions", "session.model_changed")
        );

        // The override shows in the session list.
        let listed = call(&state, "GET", "/api/v1/sessions", OWNER, &json!({}));
        let sessions = listed.json_value().unwrap()["sessions"]
            .as_array()
            .unwrap()
            .clone();
        let felicia = sessions
            .iter()
            .find(|s| s["id"] == "webhook-felicia")
            .unwrap();
        assert_eq!(felicia["model"], "claude-opus-5-5");
        assert_eq!(felicia["thinking_level"], "high");

        // A Peer may switch too; no thinking level is `null`, and keeps the earlier level.
        let peer = call(&state, "POST", url, PEER, &json!({ "model": "gpt-6-sol" }));
        assert_eq!(peer.status, 200);
        assert!(peer.json_value().unwrap()["thinking_level"].is_null());
        let listed = call(&state, "GET", "/api/v1/sessions", PEER, &json!({}));
        let sessions = listed.json_value().unwrap()["sessions"]
            .as_array()
            .unwrap()
            .clone();
        let felicia = sessions
            .iter()
            .find(|s| s["id"] == "webhook-felicia")
            .unwrap();
        assert_eq!(felicia["model"], "gpt-6-sol");

        // The CLI got `/model <id>` once per accepted call (the trimmed id, not the raw one).
        let delivered = lines.join().unwrap();
        let bodies: Vec<String> = delivered
            .iter()
            .map(|l| {
                serde_json::from_str::<Value>(l).unwrap()["body"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(bodies, vec!["/model claude-opus-5-5", "/model gpt-6-sol"]);
    }

    fn set_model(state: &HubState, target: &str, model: &str) {
        let resp = call(
            state,
            "POST",
            &format!("/api/v1/sessions/{target}/model"),
            OWNER,
            &json!({ "model": model }),
        );
        assert_eq!(resp.status, 200, "{target}: {}", error_text(&resp));
    }

    fn listed_model(state: &HubState, id: &str) -> Value {
        let listed = call(state, "GET", "/api/v1/sessions", OWNER, &json!({}));
        let sessions = listed.json_value().unwrap()["sessions"]
            .as_array()
            .unwrap()
            .clone();
        sessions.iter().find(|s| s["id"] == id).unwrap()["model"].clone()
    }

    #[test]
    fn model_override_applies_when_the_session_matches_by_persona_only() {
        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);
        install_route(
            tmp.path(),
            "felicia",
            webhook_route(&tmp.path().join("felicia.sock")),
        );

        // `Felicia` is not the discovered persona, so the request matches no summary, but it
        // does find the route. The override is keyed by the lowercased persona and still shows.
        set_model(&state, "Felicia", "model-a");
        assert_eq!(
            state.session_model_override("felicia").unwrap().model,
            "model-a"
        );
        assert!(state.session_model_override("Felicia").is_none());
        assert_eq!(listed_model(&state, "webhook-felicia"), "model-a");

        // A hosted session with no route yet: the override waits under its persona, and shows
        // as soon as discovery lists the session.
        assert_eq!(spin_up(&state, &spin_up_body("ada")).status, 200);
        set_model(&state, "claude:ada", "model-b");
        assert_eq!(
            state.session_model_override("ada").unwrap().model,
            "model-b"
        );
        install_route(
            tmp.path(),
            "ada",
            webhook_route(&tmp.path().join("ada.sock")),
        );
        assert_eq!(listed_model(&state, "webhook-ada"), "model-b");
        // A session id match wins over a persona key.
        state.set_session_model_override(
            "webhook-ada",
            session_actions::ModelOverride {
                model: "model-c".into(),
                thinking_level: None,
            },
        );
        assert_eq!(listed_model(&state, "webhook-ada"), "model-c");
    }

    #[test]
    fn model_override_is_dropped_on_reset_spin_up_and_teardown() {
        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);

        // Clear: the override of the discovered session id goes.
        let socket = tmp.path().join("felicia.sock");
        install_route(tmp.path(), "felicia", webhook_route(&socket));
        // The webhook takes `/model model-a`, then `/clear`.
        let lines = collect_webhook_lines(socket, 2);
        set_model(&state, "felicia", "model-a");
        assert!(state.session_model_override("webhook-felicia").is_some());
        let cleared = call(
            &state,
            "POST",
            "/api/v1/sessions/felicia/clear",
            OWNER,
            &json!({ "mode": "clear" }),
        );
        assert_eq!(cleared.status, 200, "{}", error_text(&cleared));
        lines.join().unwrap();
        assert!(state.session_model_override("webhook-felicia").is_none());
        assert_eq!(listed_model(&state, "webhook-felicia"), Value::Null);

        // Spin-up again of a persona with an override waiting.
        assert_eq!(spin_up(&state, &spin_up_body("ada")).status, 200);
        set_model(&state, "claude:ada", "model-b");
        assert!(state.session_model_override("ada").is_some());
        assert_eq!(spin_up(&state, &spin_up_body("ada")).status, 200);
        assert!(state.session_model_override("ada").is_none());

        // Teardown.
        set_model(&state, "claude:ada", "model-c");
        assert!(state.session_model_override("ada").is_some());
        let torn = call(
            &state,
            "POST",
            "/api/v1/sessions/claude:ada/decommission",
            OWNER,
            &json!({ "mode": "teardown" }),
        );
        assert_eq!(torn.status, 200, "{}", error_text(&torn));
        assert!(state.session_model_override("ada").is_none());
        assert!(state.session_model_override("claude:ada").is_none());
    }

    #[test]
    fn model_endpoint_is_override_only_without_a_deliverable_route_and_updates_the_hosted_record() {
        use fl_wire::identity::VendorRoute;

        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);

        // Antigravity is never sent a command; a dead webhook socket is a failed dispatch.
        install_route(
            tmp.path(),
            "agy-1",
            VendorRoute::Agy {
                conversation_id: "conv-1".into(),
                ls_pid: None,
                ls_address: None,
                csrf_token: None,
                agentapi_exe: None,
            },
        );
        install_route(
            tmp.path(),
            "dead",
            webhook_route(&tmp.path().join("dead.sock")),
        );
        for (target, session, vendor) in [
            ("agy-1", "conv-1", "antigravity"),
            ("dead", "webhook-dead", "webhook"),
        ] {
            let resp = call(
                &state,
                "POST",
                &format!("/api/v1/sessions/{target}/model"),
                OWNER,
                &json!({ "model": "gemini-4-argon" }),
            );
            assert_eq!(resp.status, 200, "{target}: {}", error_text(&resp));
            let body = resp.json_value().unwrap();
            assert_eq!(body["method"], "override-only", "{target}");
            assert_eq!(body["session_id"], session);
            assert_eq!(body["vendor"], vendor);
            assert_eq!(body["switched"], true);
            assert_eq!(
                state.session_model_override(session).unwrap().model,
                "gemini-4-argon"
            );
        }

        // A hosted session with no live route: the record takes the model.
        assert_eq!(spin_up(&state, &spin_up_body("ada")).status, 200);
        let before = call(&state, "GET", "/api/v1/hosted-sessions", OWNER, &json!({}));
        assert!(before.json_value().unwrap()["sessions"][0]["model"].is_null());
        let resp = call(
            &state,
            "POST",
            "/api/v1/sessions/claude:ada/model",
            OWNER,
            &json!({ "model": "claude-sonnet-5-5" }),
        );
        assert_eq!(resp.status, 200, "{}", error_text(&resp));
        let body = resp.json_value().unwrap();
        assert_eq!(body["vendor"], "claude");
        assert_eq!(body["method"], "override-only");
        assert_eq!(body["session_id"], "claude:ada");
        let after = call(&state, "GET", "/api/v1/hosted-sessions", OWNER, &json!({}));
        let hosted = after.json_value().unwrap()["sessions"][0].clone();
        assert_eq!(hosted["session_id"], "claude:ada");
        assert_eq!(hosted["model"], "claude-sonnet-5-5");
    }

    #[test]
    fn bulk_action_is_owner_only_validated_and_reports_each_session() {
        use fl_core::agent::{AgentRole, RegisterAgentArgs};
        use fl_core::store::AgentRegistry;

        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);
        let url = "/api/v1/sessions/bulk-action";

        let unauth = handle_http_request(
            &state,
            &HttpRequest::new("POST", url).with_json_body(&json!({ "action": "clear" })),
        );
        assert_eq!(unauth.status, 401);
        let peer = call(&state, "POST", url, PEER, &json!({ "action": "clear" }));
        assert_eq!(peer.status, 403, "{}", error_text(&peer));
        // Peer is refused before the body is read.
        assert_eq!(
            call(&state, "POST", url, PEER, &json!({ "action": "nope" })).status,
            403
        );
        for bad in [
            json!({ "action": "wipe" }),
            json!({ "action": "" }),
            json!({}),
            json!({ "action": 4 }),
        ] {
            let resp = call(&state, "POST", url, OWNER, &bad);
            assert_eq!(resp.status, 400, "{bad}");
            // Reached the bulk handler, not a session-id route.
            assert!(
                !error_text(&resp).contains("session"),
                "{}",
                error_text(&resp)
            );
        }
        let empty = call(&state, "POST", url, OWNER, &json!({ "action": "compact" }));
        assert_eq!(
            empty.json_value().unwrap(),
            json!({ "action": "compact", "total": 0, "succeeded": 0, "results": [] })
        );

        // felicia has a listener; ada's socket does not exist, so every action fails for it.
        let socket = tmp.path().join("felicia.sock");
        install_route(tmp.path(), "felicia", webhook_route(&socket));
        install_route(
            tmp.path(),
            "ada",
            webhook_route(&tmp.path().join("ada.sock")),
        );
        {
            let store = state.open_store().unwrap();
            store
                .register_agent(RegisterAgentArgs {
                    display_name: "Felicia".into(),
                    email: None,
                    project: "ferroloop".into(),
                    role: AgentRole::Assistant,
                    extra_areas: vec![],
                    create_area: true,
                    replace: true,
                    at: stamp::now(),
                    by: "owner".into(),
                })
                .unwrap();
        }
        state.enqueue_session_prompt(
            "webhook-felicia",
            Some("felicia".into()),
            "stale".into(),
            "stale".into(),
            "owner".into(),
            Vec::new(),
        );
        let lines = collect_webhook_lines(socket, 4);

        let check = |action: &str, ada_error_has: &str| {
            let resp = call(&state, "POST", url, OWNER, &json!({ "action": action }));
            assert_eq!(resp.status, 200, "{action}: {}", error_text(&resp));
            let body = resp.json_value().unwrap();
            assert_eq!(body["action"], action);
            assert_eq!(body["total"], 2, "{body}");
            assert_eq!(body["succeeded"], 1, "{body}");
            let results = body["results"].as_array().unwrap();
            assert_eq!(results.len(), 2);
            let ok = results
                .iter()
                .find(|r| r["session_id"] == "webhook-felicia")
                .unwrap();
            assert_eq!(*ok, json!({ "session_id": "webhook-felicia", "ok": true }));
            let failed = results
                .iter()
                .find(|r| r["session_id"] == "webhook-ada")
                .unwrap();
            assert_eq!(failed["ok"], false);
            assert!(
                failed["error"].as_str().unwrap().contains(ada_error_has),
                "{action}: {failed}"
            );
        };
        check("clear", "webhook socket");
        // The clear dropped felicia's queue like the single-session endpoint does.
        assert!(state.session_queue("webhook-felicia").is_empty());
        check("compact", "webhook socket");
        // Reforge needs an agent record: felicia has one, ada does not. (The hydrate lock
        // keeps another test's FL_HUB_AGENT_HYDRATE_BIN out of this call.)
        api::with_hydrate_lock(|| check("reforge", "no agent record"));

        let delivered = lines.join().unwrap();
        let bodies: Vec<String> = delivered
            .iter()
            .map(|l| {
                serde_json::from_str::<Value>(l).unwrap()["body"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(bodies[0], "/clear");
        assert_eq!(bodies[1], "/compact");
        assert_eq!(bodies[2], "/clear");
        assert_eq!(
            bodies[3],
            api::canonical_reforge_prompt("Felicia", "ferroloop", "assistant")
        );
    }

    #[test]
    fn commands_endpoint_lists_builtins_and_workspace_skills_and_flows() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);
        let root = tmp.path();
        let skills = root.join(".agents/skills");
        fs::create_dir_all(skills.join("alpha")).unwrap();
        fs::write(
            skills.join("alpha/SKILL.md"),
            "---\nname: alpha\ndescription: Alpha does a thing\n---\n# Alpha\n",
        )
        .unwrap();
        fs::create_dir_all(skills.join("broken")).unwrap();
        fs::write(skills.join("broken/SKILL.md"), "no frontmatter here").unwrap();
        fs::create_dir_all(skills.join("unfenced")).unwrap();
        fs::write(
            skills.join("unfenced/SKILL.md"),
            "intro\nname: unfenced\ndescription: x\n---\n",
        )
        .unwrap();
        // A skill whose directory is a symlink to outside the workspace.
        let outside = TempDir::new().unwrap();
        fs::create_dir_all(outside.path().join("evil")).unwrap();
        fs::write(
            outside.path().join("evil/SKILL.md"),
            "---\nname: evil\ndescription: escaped\n---\n",
        )
        .unwrap();
        symlink(outside.path().join("evil"), skills.join("evil")).unwrap();
        fs::create_dir_all(root.join(".fl/flows")).unwrap();
        fs::write(
            root.join(".fl/flows/ship.toml"),
            "title = \"Ship a change\"\nstart_step = \"a\"\n",
        )
        .unwrap();
        fs::write(outside.path().join("leak.toml"), "title = \"leak\"\n").unwrap();
        symlink(
            outside.path().join("leak.toml"),
            root.join(".fl/flows/leak.toml"),
        )
        .unwrap();

        let unauth = handle_http_request(&state, &HttpRequest::new("GET", "/api/v1/commands"));
        assert_eq!(unauth.status, 401);

        for token in [OWNER, PEER] {
            let resp = call(&state, "GET", "/api/v1/commands", token, &json!({}));
            assert_eq!(resp.status, 200, "{}", error_text(&resp));
            let commands = resp.json_value().unwrap()["commands"]
                .as_array()
                .unwrap()
                .clone();
            let find = |kind: &str, name: &str| {
                commands
                    .iter()
                    .find(|c| c["kind"] == kind && c["name"] == name)
                    .cloned()
            };
            for name in ["compact", "clear", "model", "goal", "plan", "boost"] {
                assert!(find("command", name).is_some(), "{name}");
            }
            for name in ["ultrareview", "code-review", "subagent-dev", "ste"] {
                assert!(find("skill", name).is_some(), "{name}");
            }
            assert_eq!(
                find("skill", "alpha").unwrap(),
                json!({ "name": "alpha", "kind": "skill", "description": "Alpha does a thing" })
            );
            assert_eq!(
                find("workflow", "ship").unwrap(),
                json!({ "name": "ship", "kind": "workflow", "description": "Ship a change" })
            );
            assert!(find("skill", "broken").is_none());
            assert!(find("skill", "unfenced").is_none());
            assert!(find("skill", "evil").is_none());
            assert!(find("workflow", "leak").is_none());
            for c in &commands {
                assert_eq!(c.as_object().unwrap().len(), 3, "{c}");
                assert!(c["description"].is_string());
            }
            let keys: Vec<(String, String)> = commands
                .iter()
                .map(|c| {
                    (
                        c["kind"].as_str().unwrap().to_string(),
                        c["name"].as_str().unwrap().to_string(),
                    )
                })
                .collect();
            let mut sorted = keys.clone();
            sorted.sort();
            assert_eq!(keys, sorted);
        }
    }

    // ---- Operator prompts reach a terminal as direct input for the owner only -----------

    static TMUX_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Points `FL_WIRE_TMUX_BIN` at a fake tmux that records its arguments in `<dir>/tmux.out`
    /// and reports every pane alive, then restores the variable on drop. Hold `TMUX_ENV_LOCK`.
    struct FakeTmux {
        out: std::path::PathBuf,
        prev: Option<std::ffi::OsString>,
    }

    impl FakeTmux {
        fn install(dir: &std::path::Path) -> Self {
            use std::os::unix::fs::PermissionsExt;

            let out = dir.join("tmux.out");
            let bin = dir.join("fake-tmux.sh");
            fs::write(
                &bin,
                format!(
                    "#!/bin/sh\necho \"$*\" >> \"{}\"\nif [ \"$1\" = \"display-message\" ]; then\n  echo \"%12\"\nelif [ \"$1\" = \"load-buffer\" ]; then\n  cat > /dev/null\nfi\n",
                    out.display()
                ),
            )
            .unwrap();
            fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
            let prev = std::env::var_os("FL_WIRE_TMUX_BIN");
            // SAFETY: callers hold TMUX_ENV_LOCK; the previous value is restored on drop.
            unsafe { std::env::set_var("FL_WIRE_TMUX_BIN", &bin) };
            Self { out, prev }
        }

        fn log(&self) -> String {
            fs::read_to_string(&self.out).unwrap_or_default()
        }

        fn forget(&self) {
            let _ = fs::remove_file(&self.out);
        }
    }

    impl Drop for FakeTmux {
        fn drop(&mut self) {
            // SAFETY: see `install`.
            unsafe {
                match self.prev.take() {
                    Some(v) => std::env::set_var("FL_WIRE_TMUX_BIN", v),
                    None => std::env::remove_var("FL_WIRE_TMUX_BIN"),
                }
            }
        }
    }

    /// A Claude route for `persona` whose key file names a tmux pane. The mock socket is
    /// returned so a test can see whether the peer-session path was used.
    fn install_claude_tmux_route(
        home: &std::path::Path,
        persona: &str,
    ) -> std::os::unix::net::UnixListener {
        use std::os::unix::net::UnixListener;

        let sock = home.join(format!("{persona}.sock"));
        let key = home.join(format!("{persona}.key"));
        fs::write(
            &key,
            json!({ "peerToken": "tok-hub", "tmux": "Felicity:@12.%12" }).to_string(),
        )
        .unwrap();
        let listener = UnixListener::bind(&sock).unwrap();
        listener.set_nonblocking(true).unwrap();
        install_route(
            home,
            persona,
            fl_wire::identity::VendorRoute::Claude {
                pid: 0,
                session_id: format!("cs-{persona}"),
                socket_path: sock,
                key_path: Some(key),
            },
        );
        listener
    }

    /// The `from` of the peer-session user frame the socket received, if it received one.
    fn socket_from(listener: &std::os::unix::net::UnixListener) -> Option<String> {
        use std::io::{BufRead, BufReader};

        let (stream, _) = listener.accept().ok()?;
        stream.set_nonblocking(false).unwrap();
        let lines: Vec<String> = BufReader::new(stream)
            .lines()
            .take(2)
            .map(|l| l.unwrap())
            .collect();
        let user: Value = serde_json::from_str(&lines[1]).unwrap();
        Some(user["from"].as_str().unwrap().to_string())
    }

    fn post_prompt(
        state: &HubState,
        persona: &str,
        token: &str,
        agent: Option<&str>,
        body: &Value,
    ) -> Value {
        let mut req = authed(
            "POST",
            &format!("/api/v1/sessions/{persona}/prompt"),
            token,
            body,
        );
        if let Some(agent) = agent {
            req = req.with_header("X-Fl-Agent", agent);
        }
        let resp = handle_http_request(state, &req);
        assert_eq!(resp.status, 200, "{}", error_text(&resp));
        resp.json_value().unwrap()
    }

    #[test]
    fn owner_prompt_reaches_a_claude_tmux_pane_but_a_peer_never_does() {
        let _lock = TMUX_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);
        let tmux = FakeTmux::install(tmp.path());
        let listener = install_claude_tmux_route(tmp.path(), "felicity");

        // The Owner token: pasted into the pane, nothing on the peer socket.
        let sent = post_prompt(
            &state,
            "felicity",
            OWNER,
            None,
            &json!({ "prompt": "hello from the owner", "send_now": true }),
        );
        assert_eq!(sent["delivered"], true, "{sent}");
        assert_eq!(sent["by"], "lmcfarlin");
        let log = tmux.log();
        assert!(log.contains("paste-buffer -d -b"), "{log}");
        assert!(log.contains("send-keys -t %12 Enter"), "{log}");
        assert_eq!(socket_from(&listener), None, "owner prompt used the socket");

        // A Peer token that claims to be the owner, however it spells the claim, takes the
        // peer-session socket and never the pane.
        let claims: [(Option<&str>, Value); 6] = [
            (
                None,
                json!({ "send_now": true, "prompt": "p1", "by": "owner" }),
            ),
            (
                None,
                json!({ "send_now": true, "prompt": "p2", "by": "OWNER" }),
            ),
            (
                None,
                json!({ "send_now": true, "prompt": "p3", "by": " lmcfarlin " }),
            ),
            (
                None,
                json!({ "send_now": true, "prompt": "p4", "by": "not a slug!" }),
            ),
            (Some("owner"), json!({ "send_now": true, "prompt": "p5" })),
            (
                Some("lmcfarlin"),
                json!({ "send_now": true, "prompt": "p6", "by": "owner" }),
            ),
        ];
        for (agent, body) in claims {
            tmux.forget();
            let sent = post_prompt(&state, "felicity", PEER, agent, &body);
            assert_eq!(sent["delivered"], true, "{body}: {sent}");
            let by = sent["by"].as_str().unwrap().to_string();
            assert!(
                !fl_wire::dispatcher::is_owner_sender(&by),
                "{body}: sender `{by}` is an owner name"
            );
            assert!(
                !tmux.log().contains("paste-buffer"),
                "{body}: a peer prompt was pasted into the pane: {}",
                tmux.log()
            );
            assert_eq!(
                socket_from(&listener).as_deref(),
                Some(by.as_str()),
                "{body}"
            );
        }

        // A Peer with an honest name keeps its own label.
        tmux.forget();
        let sent = post_prompt(
            &state,
            "felicity",
            PEER,
            Some("ferris"),
            &json!({ "send_now": true, "prompt": "p7" }),
        );
        assert_eq!(sent["by"], "ferris");
        assert!(!tmux.log().contains("paste-buffer"));
        assert_eq!(socket_from(&listener).as_deref(), Some("ferris"));
    }

    #[test]
    fn peer_authority_never_resolves_to_an_owner_name() {
        use crate::auth::HubAuthority;

        for agent in ["owner", "OWNER", "lmcfarlin", " Owner "] {
            let peer = HubAuthority::Peer {
                agent: Some(agent.to_string()),
            };
            assert_eq!(peer.actor_name(), "peer", "{agent:?}");
        }
        let named = HubAuthority::Peer {
            agent: Some("ferris".into()),
        };
        assert_eq!(named.actor_name(), "ferris");
        assert_eq!(HubAuthority::Peer { agent: None }.actor_name(), "peer");
        let owner = HubAuthority::Owner {
            login: "lmcfarlin".into(),
        };
        assert_eq!(owner.actor_name(), "lmcfarlin");
    }

    #[test]
    fn peer_spin_up_hydration_is_never_pasted_into_a_pane_even_as_owner_named_agent() {
        let _lock = TMUX_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);
        let tmux = FakeTmux::install(tmp.path());
        let listener = install_claude_tmux_route(tmp.path(), "felicity");

        let mut req = authed(
            "POST",
            "/api/v1/sessions/spin-up",
            PEER,
            &spin_up_body("felicity"),
        );
        req = req.with_header("X-Fl-Agent", "owner");
        let resp = handle_http_request(&state, &req);
        assert_eq!(resp.status, 200, "{}", error_text(&resp));
        assert_eq!(resp.json_value().unwrap()["hydration_dispatched"], true);
        assert!(
            !tmux.log().contains("paste-buffer"),
            "peer hydration was pasted: {}",
            tmux.log()
        );
        assert_eq!(socket_from(&listener).as_deref(), Some("peer"));
    }

    /// A bridge `bridge-hook` that admits any sender id and routes to `wire:felicity`.
    fn install_wire_bridge(home: &std::path::Path) {
        let dir = home.join(".config").join("fl").join("gate");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("bridge-hook.toml"),
            r#"
id = "bridge-hook"
title = "Bridge hook"
base_url = "http://127.0.0.1:1"
allowed_hosts = ["127.0.0.1"]

[[bridges]]
id = "bridge-hook"
kind = "webhook"
allowed_user_ids = ["*"]
default_target = "wire:felicity"
"#,
        )
        .unwrap();
    }

    #[test]
    fn bridge_webhook_label_is_never_an_owner_name_so_it_is_never_pasted_into_a_pane() {
        let _lock = TMUX_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);
        let tmux = FakeTmux::install(tmp.path());
        let listener = install_claude_tmux_route(tmp.path(), "felicity");
        install_wire_bridge(tmp.path());

        // The webhook body is third-party text. Whatever name it claims, the prompt takes the
        // peer-session socket under a non-owner label and never the pane.
        let claims: [(&str, Option<&str>, &str); 6] = [
            ("u1", Some("owner"), "bridge"),
            ("u1", Some("OWNER"), "bridge"),
            ("u1", Some("lmcfarlin"), "bridge"),
            ("u1", Some("not a slug!"), "bridge"),
            ("owner", None, "bridge"),
            ("u1", Some("alice"), "alice"),
        ];
        for (sender_id, sender_name, expect) in claims {
            tmux.forget();
            let body = json!({
                "bridge_id": "ignored",
                "sender_id": sender_id,
                "sender_name": sender_name,
                "channel_id": "chan",
                "content": "do the thing",
            });
            let resp = call(
                &state,
                "POST",
                "/api/v1/gate/bridges/bridge-hook/webhook",
                OWNER,
                &body,
            );
            assert_eq!(resp.status, 200, "{body}: {}", error_text(&resp));
            assert_eq!(resp.json_value().unwrap()["delivered"], true, "{body}");
            assert!(
                !tmux.log().contains("paste-buffer"),
                "{body}: a webhook prompt was pasted into the pane: {}",
                tmux.log()
            );
            assert_eq!(socket_from(&listener).as_deref(), Some(expect), "{body}");
        }
    }

    #[test]
    fn wire_send_never_lets_a_peer_send_as_the_owner() {
        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);
        let send = |token: &str, agent: Option<&str>, body: Value| -> String {
            let mut req = authed("POST", "/api/v1/wire/send", token, &body);
            if let Some(agent) = agent {
                req = req.with_header("X-Fl-Agent", agent);
            }
            let resp = handle_http_request(&state, &req);
            assert_eq!(resp.status, 200, "{body}: {}", error_text(&resp));
            resp.json_value().unwrap()["message"]["sender_name"]
                .as_str()
                .unwrap()
                .to_string()
        };
        let msg = |extra: Value| {
            let mut body = json!({ "to": ["felicity"], "subject": "s", "body": "b" });
            body.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            body
        };

        // A Peer claiming an owner name, or sending an unusable one, never becomes the owner.
        for claim in ["owner", "OWNER", " lmcfarlin ", "not a slug!"] {
            for key in ["sender_name", "from"] {
                let got = send(PEER, None, msg(json!({ key: claim })));
                assert_eq!(got, "peer", "{key}={claim:?}");
                let got = send(PEER, Some("ferris"), msg(json!({ key: claim })));
                assert_eq!(got, "ferris", "{key}={claim:?} as ferris");
            }
        }
        assert_eq!(send(PEER, Some("owner"), msg(json!({}))), "peer");
        // An honest Peer name is kept.
        assert_eq!(send(PEER, None, msg(json!({ "from": "ferris" }))), "ferris");
        // The Owner keeps its own label, and an unusable one falls back to `owner`.
        assert_eq!(send(OWNER, None, msg(json!({}))), "lmcfarlin");
        assert_eq!(
            send(OWNER, None, msg(json!({ "sender_name": "owner" }))),
            "owner"
        );
        assert_eq!(
            send(OWNER, None, msg(json!({ "sender_name": "not a slug!" }))),
            "owner"
        );
    }

    #[test]
    fn spin_up_refuses_the_owner_names_as_a_persona() {
        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);
        for persona in ["owner", "lmcfarlin"] {
            for token in [OWNER, PEER] {
                let resp = call(
                    &state,
                    "POST",
                    "/api/v1/sessions/spin-up",
                    token,
                    &spin_up_body(persona),
                );
                assert_eq!(resp.status, 400, "{persona}: {}", error_text(&resp));
                assert!(
                    error_text(&resp).contains("persona"),
                    "{}",
                    error_text(&resp)
                );
            }
        }
    }

    // --- Record 29: artifact registry, archived copies, pruning -------------------------------

    fn artifacts_root_of(tmp: &TempDir) -> std::path::PathBuf {
        crate::artifacts::data_root(tmp.path())
    }

    fn artifacts_root_of_home(home: &std::path::Path) -> std::path::PathBuf {
        crate::artifacts::data_root(home)
    }

    fn managed_files(tmp: &TempDir) -> Vec<String> {
        let dir = crate::artifacts::managed_dir(&artifacts_root_of(tmp));
        let mut names: Vec<String> = fs::read_dir(dir)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    fn list_artifacts(state: &HubState) -> Value {
        let resp = call(state, "GET", "/api/v1/artifacts/list", OWNER, &json!({}));
        assert_eq!(resp.status, 200, "{}", error_text(&resp));
        resp.json_value().unwrap()
    }

    fn prune(state: &HubState, token: &str, body: &Value) -> HttpResponse {
        call(state, "POST", "/api/v1/artifacts/prune", token, body)
    }

    #[test]
    fn artifact_list_reports_live_then_archived_then_drops_what_is_gone() {
        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);
        let att = tmp.path().join("attachments");
        fs::create_dir_all(&att).unwrap();
        let file = att.join("plan.md");
        fs::write(&file, "# plan").unwrap();
        let file_str = fs::canonicalize(&file).unwrap().display().to_string();

        let live = list_artifacts(&state);
        assert_eq!(live["pruned_count"], 0);
        assert_eq!(live["artifacts"].as_array().unwrap().len(), 1);
        assert_eq!(live["artifacts"][0]["status"], "live");
        assert_eq!(live["artifacts"][0]["path"], file_str.as_str());
        let reg = crate::artifacts::Registry::load(&artifacts_root_of(&tmp));
        assert_eq!(reg.entries.len(), 1);
        assert_eq!(reg.entries[0].canonical_path, file_str);
        assert!(reg.entries[0].managed_path.is_none());

        // Archive it, then lose the original: the managed copy is served in its place.
        let resp = prune(&state, OWNER, &json!({ "paths": [file_str] }));
        assert_eq!(resp.status, 200, "{}", error_text(&resp));
        assert_eq!(managed_files(&tmp).len(), 1);
        fs::remove_file(&file).unwrap();
        let archived = list_artifacts(&state);
        assert_eq!(archived["pruned_count"], 0);
        let item = &archived["artifacts"][0];
        assert_eq!(item["status"], "archived");
        assert_eq!(item["original_path"], file_str.as_str());
        let managed_dir = crate::artifacts::managed_dir(&artifacts_root_of(&tmp));
        let served = item["path"].as_str().unwrap();
        assert!(
            std::path::Path::new(served).starts_with(fs::canonicalize(&managed_dir).unwrap()),
            "{served}"
        );
        assert_eq!(item["name"], "plan.md");
        assert_eq!(item["kind"], "markdown");

        // The archived file previews even though its worktree-side original is gone.
        let preview = call(
            &state,
            "GET",
            &format!("/api/v1/artifacts/preview?path={served}"),
            OWNER,
            &json!({}),
        );
        assert_eq!(preview.status, 200, "{}", error_text(&preview));
        assert_eq!(preview.json_value().unwrap()["content"], "# plan");

        // Original and copy both gone: dropped from the response and the registry.
        for name in managed_files(&tmp) {
            fs::remove_file(managed_dir.join(name)).unwrap();
        }
        let gone = list_artifacts(&state);
        assert_eq!(gone["artifacts"], json!([]));
        assert_eq!(gone["pruned_count"], 1);
        assert!(
            crate::artifacts::Registry::load(&artifacts_root_of(&tmp))
                .entries
                .is_empty()
        );
        assert_eq!(list_artifacts(&state)["pruned_count"], 0);
    }

    #[test]
    fn archived_artifacts_preview_when_home_is_apart_from_the_repo() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        let home = tmp.path().join("home");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(home.join("attachments")).unwrap();
        let state = HubState::new(HubServerConfig {
            store_path: home.join("store.redb"),
            repo_root: repo.clone(),
            owner_token: Some(OWNER.to_string()),
            home_override: Some(home.clone()),
            ..HubServerConfig::default()
        });
        let file = home.join("attachments/keep.md");
        fs::write(&file, "# kept").unwrap();
        let named = file.display().to_string();
        assert_eq!(
            prune(&state, OWNER, &json!({ "paths": [named] })).status,
            200
        );
        fs::remove_file(&file).unwrap();
        let listed = list_artifacts(&state);
        let item = &listed["artifacts"][0];
        assert_eq!(item["status"], "archived", "{listed}");
        let served = item["path"].as_str().unwrap().to_string();
        assert!(served.contains("/fl/artifacts/managed/"), "{served}");
        let preview = call(
            &state,
            "GET",
            &format!("/api/v1/artifacts/preview?path={served}"),
            OWNER,
            &json!({}),
        );
        assert_eq!(preview.status, 200, "{}", error_text(&preview));
        assert_eq!(preview.json_value().unwrap()["content"], "# kept");
        // Anything else under the managed root's parent stays out of reach.
        let registry = artifacts_root_of_home(&home).join("registry.json");
        let enc = registry.display().to_string();
        let denied = call(
            &state,
            "GET",
            &format!("/api/v1/artifacts/preview?path={enc}"),
            OWNER,
            &json!({}),
        );
        assert_eq!(denied.status, 403);
    }

    #[test]
    fn artifact_registry_ignores_a_managed_path_outside_the_managed_dir() {
        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);
        let secret = tmp.path().join("notes.md");
        fs::write(&secret, "private").unwrap();
        let gone = tmp.path().join("gone.md");
        let canon = fs::canonicalize(&secret).unwrap();
        crate::artifacts::with_registry(&artifacts_root_of(&tmp), |reg| {
            reg.entries.push(crate::artifacts::ArtifactEntry {
                path: gone.display().to_string(),
                canonical_path: gone.display().to_string(),
                managed_path: Some(canon.display().to_string()),
                name: "gone.md".into(),
                ext: "md".into(),
                kind: "markdown".into(),
                size_bytes: 7,
                session_id: None,
                status: "live".into(),
                updated_at_ms: 1,
            });
        });
        let listed = list_artifacts(&state);
        assert_eq!(listed["artifacts"], json!([]));
        assert_eq!(listed["pruned_count"], 1);
    }

    #[test]
    fn artifact_prune_is_owner_only_and_validates_its_body() {
        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);
        let url = "/api/v1/artifacts/prune";
        let unauth = handle_http_request(
            &state,
            &HttpRequest::new("POST", url).with_json_body(&json!({})),
        );
        assert_eq!(unauth.status, 401);
        let peer = prune(&state, PEER, &json!({}));
        assert_eq!(peer.status, 403, "{}", error_text(&peer));

        let owner_req = |body: &[u8]| {
            HttpRequest::new("POST", url)
                .with_header("Authorization", format!("Bearer {OWNER}"))
                .with_body(body.to_vec())
        };
        let malformed = handle_http_request(&state, &owner_req(b"{ nope"));
        assert_eq!(malformed.status, 400);
        let not_object = handle_http_request(&state, &owner_req(b"[1]"));
        assert_eq!(not_object.status, 400);
        for bad in [
            json!({ "paths": "a.md" }),
            json!({ "paths": [3] }),
            json!({ "worktree_path": 4 }),
            json!({ "session_id": [] }),
        ] {
            assert_eq!(prune(&state, OWNER, &bad).status, 400, "{bad}");
        }
        let empty = handle_http_request(&state, &owner_req(b""));
        assert_eq!(empty.status, 200);
        assert_eq!(
            empty.json_value().unwrap(),
            json!({ "ok": true, "pruned_count": 0, "archived_count": 0, "active_count": 0 })
        );
        assert!(managed_files(&tmp).is_empty());
    }

    #[test]
    fn artifact_prune_copies_only_small_user_facing_files_inside_the_worktree() {
        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);
        let wt = tmp.path().join("wt");
        fs::create_dir_all(wt.join("docs")).unwrap();
        fs::write(wt.join("docs/notes.md"), "# notes").unwrap();
        fs::write(wt.join("diagram.mmd"), "graph TD;A-->B").unwrap();
        fs::write(wt.join("big.png"), vec![0u8; 5 * 1024 * 1024 + 1]).unwrap();
        fs::write(wt.join("main.rs"), "fn main() {}").unwrap();
        // Same file name elsewhere in the repo: named, but outside the worktree.
        fs::write(tmp.path().join("notes.md"), "# other notes").unwrap();
        let named: Vec<String> = ["docs/notes.md", "diagram.mmd", "big.png", "main.rs"]
            .iter()
            .map(|p| wt.join(p).display().to_string())
            .chain([tmp.path().join("notes.md").display().to_string()])
            .collect();

        let resp = prune(
            &state,
            OWNER,
            &json!({ "worktree_path": wt.display().to_string(), "paths": named }),
        );
        assert_eq!(resp.status, 200, "{}", error_text(&resp));
        let out = resp.json_value().unwrap();
        assert_eq!(out["ok"], true);
        assert_eq!(out["pruned_count"], 0);
        assert_eq!(out["active_count"], 2, "{out}");
        let managed = managed_files(&tmp);
        assert_eq!(managed.len(), 2, "{managed:?}");
        assert!(managed.iter().any(|n| n.ends_with("-notes.md")));
        assert!(managed.iter().any(|n| n.ends_with("-diagram.mmd")));
        assert!(managed.iter().all(|n| !n.contains('/')));
        let reg = crate::artifacts::Registry::load(&artifacts_root_of(&tmp));
        assert_eq!(reg.entries.len(), 2);
        assert!(reg.entries.iter().all(|e| e.managed_path.is_some()));
        assert!(reg.entries.iter().all(|e| e.status == "live"));

        // Pruning again re-copies in place: no duplicates.
        let again = prune(
            &state,
            OWNER,
            &json!({ "worktree_path": wt.display().to_string(), "paths": [wt.join("diagram.mmd").display().to_string()] }),
        );
        assert_eq!(again.status, 200);
        assert_eq!(managed_files(&tmp), managed);
    }

    #[test]
    fn artifact_prune_refuses_worktrees_and_paths_outside_the_allowed_roots() {
        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);
        let outside = TempDir::new().unwrap();
        fs::write(outside.path().join("leak.md"), "# leak").unwrap();

        let resp = prune(
            &state,
            OWNER,
            &json!({ "worktree_path": outside.path().display().to_string() }),
        );
        assert_eq!(resp.status, 403, "{}", error_text(&resp));
        let resp = prune(
            &state,
            OWNER,
            &json!({ "paths": [outside.path().join("leak.md").display().to_string()] }),
        );
        assert_eq!(resp.status, 403, "{}", error_text(&resp));
        // A missing path outside the roots is refused the same way: no existence oracle.
        let resp = prune(
            &state,
            OWNER,
            &json!({ "worktree_path": outside.path().join("nope").display().to_string() }),
        );
        assert_eq!(resp.status, 403);
        // Forbidden names inside the roots.
        fs::create_dir_all(tmp.path().join("repo/.git")).unwrap();
        for forbidden in ["repo/.git", "repo/../repo"] {
            let resp = prune(
                &state,
                OWNER,
                &json!({ "worktree_path": tmp.path().join(forbidden).display().to_string() }),
            );
            assert_eq!(resp.status, 403, "{forbidden}");
        }
        let env_file = tmp.path().join(".env");
        fs::write(&env_file, "TOKEN=1").unwrap();
        let resp = prune(
            &state,
            OWNER,
            &json!({ "paths": [env_file.display().to_string()] }),
        );
        assert_eq!(resp.status, 403);
        // A symlink in the worktree that leads outside the roots is refused, not followed.
        let wt = tmp.path().join("wt");
        fs::create_dir_all(&wt).unwrap();
        std::os::unix::fs::symlink(outside.path().join("leak.md"), wt.join("link.md")).unwrap();
        let resp = prune(
            &state,
            OWNER,
            &json!({ "worktree_path": wt.display().to_string(),
                     "paths": [wt.join("link.md").display().to_string()] }),
        );
        assert_eq!(resp.status, 403);
        assert!(managed_files(&tmp).is_empty());
    }

    #[test]
    fn artifact_prune_skips_tampered_registry_entries() {
        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);
        let wt = tmp.path().join("wt");
        let other = tmp.path().join("other");
        fs::create_dir_all(&wt).unwrap();
        fs::create_dir_all(other.join(".git")).unwrap();
        fs::write(wt.join("ok.md"), "# ok").unwrap();
        fs::write(other.join("elsewhere.md"), "# elsewhere").unwrap();
        fs::write(other.join(".git/hidden.md"), "# hidden").unwrap();
        let outside = TempDir::new().unwrap();
        fs::write(outside.path().join("leak.md"), "# leak").unwrap();
        std::os::unix::fs::symlink(outside.path().join("leak.md"), wt.join("link.md")).unwrap();
        let entry = |p: &std::path::Path| crate::artifacts::ArtifactEntry {
            path: p.display().to_string(),
            canonical_path: p.display().to_string(),
            managed_path: None,
            name: "x.md".into(),
            ext: "md".into(),
            kind: "markdown".into(),
            size_bytes: 1,
            session_id: None,
            status: "live".into(),
            updated_at_ms: 1,
        };
        let canon_wt = fs::canonicalize(&wt).unwrap();
        crate::artifacts::with_registry(&artifacts_root_of(&tmp), |reg| {
            for p in [
                canon_wt.join("ok.md"),
                fs::canonicalize(&other).unwrap().join("elsewhere.md"),
                fs::canonicalize(&other).unwrap().join(".git/hidden.md"),
                canon_wt.join("link.md"),
            ] {
                reg.entries.push(entry(&p));
            }
        });
        let resp = prune(
            &state,
            OWNER,
            &json!({ "worktree_path": wt.display().to_string() }),
        );
        assert_eq!(resp.status, 200, "{}", error_text(&resp));
        let managed = managed_files(&tmp);
        assert_eq!(managed.len(), 1, "{managed:?}");
        assert!(managed[0].ends_with("-ok.md"));
    }

    #[test]
    fn a_later_session_id_cannot_retag_a_listed_artifact_and_attachments_stay_untagged() {
        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);
        let brain = tmp.path().join(".gemini/antigravity-cli/brain");
        fs::create_dir_all(brain.join("sess-a")).unwrap();
        fs::create_dir_all(brain.join("sess-b")).unwrap();
        fs::write(brain.join("sess-a/plan.md"), "# plan").unwrap();
        // sess-b can reach the same file (a link inside the allowed brain root).
        std::os::unix::fs::symlink("../sess-a/plan.md", brain.join("sess-b/plan.md")).unwrap();
        fs::create_dir_all(tmp.path().join("attachments")).unwrap();
        fs::write(tmp.path().join("attachments/shared.md"), "# shared").unwrap();

        let list_as = |sid: &str| {
            let resp = call(
                &state,
                "GET",
                &format!("/api/v1/artifacts/list?session_id={sid}"),
                PEER,
                &json!({}),
            );
            assert_eq!(resp.status, 200, "{}", error_text(&resp));
        };
        let session_of = |name: &str| {
            crate::artifacts::Registry::load(&artifacts_root_of(&tmp))
                .entries
                .into_iter()
                .find(|e| e.name == name)
                .unwrap_or_else(|| panic!("no entry for {name}"))
                .session_id
        };
        list_as("sess-a");
        let first = session_of("plan.md");
        assert!(
            first.as_deref().is_some_and(|s| s.contains("sess-a")),
            "{first:?}"
        );
        assert_eq!(
            session_of("shared.md"),
            None,
            "attachments belong to no session"
        );
        list_as("sess-b");
        assert_eq!(session_of("plan.md"), first, "first owner is kept");
        assert_eq!(session_of("shared.md"), None);
    }

    #[test]
    fn prune_with_a_missing_worktree_archives_nothing_from_paths() {
        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);
        let elsewhere = tmp.path().join("elsewhere.md");
        fs::write(&elsewhere, "# elsewhere").unwrap();
        let named = elsewhere.display().to_string();
        let gone = tmp.path().join("wt-gone").display().to_string();

        let resp = prune(
            &state,
            OWNER,
            &json!({ "worktree_path": gone, "paths": [named] }),
        );
        assert_eq!(resp.status, 200, "{}", error_text(&resp));
        let out = resp.json_value().unwrap();
        assert_eq!(out["archived_count"], 0, "{out}");
        assert!(managed_files(&tmp).is_empty());
        assert!(
            crate::artifacts::Registry::load(&artifacts_root_of(&tmp))
                .entries
                .is_empty()
        );

        // With no worktree named, the same path is archived.
        assert_eq!(
            prune(&state, OWNER, &json!({ "paths": [named] })).status,
            200
        );
        assert_eq!(managed_files(&tmp).len(), 1);
    }

    #[test]
    fn prune_skips_a_current_copy_and_caps_the_files_per_call() {
        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);
        let wt = tmp.path().join("wt");
        fs::create_dir_all(&wt).unwrap();
        fs::write(wt.join("keep.md"), "# keep").unwrap();
        let wt_arg = wt.display().to_string();
        let body =
            json!({ "worktree_path": wt_arg, "paths": [wt.join("keep.md").display().to_string()] });
        assert_eq!(prune(&state, OWNER, &body).status, 200);
        let dir = crate::artifacts::managed_dir(&artifacts_root_of(&tmp));
        let copy = dir.join(&managed_files(&tmp)[0]);
        let first = fs::metadata(&copy).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert_eq!(prune(&state, OWNER, &body).status, 200);
        assert_eq!(
            fs::metadata(&copy).unwrap().modified().unwrap(),
            first,
            "not copied again"
        );
        // The registry still records the copy.
        let reg = crate::artifacts::Registry::load(&artifacts_root_of(&tmp));
        assert!(reg.entries[0].managed_path.is_some());

        let many = tmp.path().join("many");
        fs::create_dir_all(&many).unwrap();
        let paths: Vec<String> = (0..205)
            .map(|i| {
                let f = many.join(format!("n{i}.md"));
                fs::write(&f, format!("# {i}")).unwrap();
                f.display().to_string()
            })
            .collect();
        let resp = prune(
            &state,
            OWNER,
            &json!({ "worktree_path": many.display().to_string(), "paths": paths }),
        );
        assert_eq!(resp.status, 200, "{}", error_text(&resp));
        // 1 from the first call plus at most 200 more.
        assert_eq!(
            managed_files(&tmp).len(),
            201,
            "{}",
            managed_files(&tmp).len()
        );
    }

    fn teardown_body(body: &mut Value, cwd: &std::path::Path) {
        body["cwd"] = json!(cwd.display().to_string());
    }

    fn seed_artifact(tmp: &TempDir, file: &std::path::Path, session: &str) {
        let canon = fs::canonicalize(file).unwrap();
        crate::artifacts::with_registry(&artifacts_root_of(tmp), |reg| {
            reg.entries.push(crate::artifacts::ArtifactEntry {
                path: canon.display().to_string(),
                canonical_path: canon.display().to_string(),
                managed_path: None,
                name: "report.md".into(),
                ext: "md".into(),
                kind: "markdown".into(),
                size_bytes: 8,
                session_id: Some(session.into()),
                status: "live".into(),
                updated_at_ms: 1,
            });
        });
    }

    #[test]
    fn teardown_archives_the_sessions_artifacts_and_survives_an_archive_failure() {
        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);
        let wt = tmp.path().join("wt-felicity");
        fs::create_dir_all(&wt).unwrap();
        fs::write(wt.join("report.md"), "# report").unwrap();
        let mut body = spin_up_body("felicity");
        teardown_body(&mut body, &wt);
        assert_eq!(spin_up(&state, &body).status, 200);
        seed_artifact(&tmp, &wt.join("report.md"), "claude:felicity");

        // Prepare never archives.
        let prepared = decommission_as(&state, OWNER, "claude%3Afelicity", "prepare");
        assert_eq!(prepared.status, 200, "{}", error_text(&prepared));
        assert!(managed_files(&tmp).is_empty());

        let torn = decommission_as(&state, OWNER, "claude%3Afelicity", "teardown");
        assert_eq!(torn.status, 200, "{}", error_text(&torn));
        assert_eq!(torn.json_value().unwrap()["status"], "decommissioned");
        let managed = managed_files(&tmp);
        assert_eq!(managed.len(), 1, "{managed:?}");
        let reg = crate::artifacts::Registry::load(&artifacts_root_of(&tmp));
        assert!(reg.entries[0].managed_path.is_some());

        // The worktree goes; its report is still listed, from the archive.
        fs::remove_dir_all(&wt).unwrap();
        let listed = list_artifacts(&state);
        assert_eq!(listed["artifacts"][0]["status"], "archived");
        assert_eq!(listed["artifacts"][0]["name"], "report.md");

        // An archive that cannot be written never blocks a teardown or changes its status.
        let wt2 = tmp.path().join("wt-ada");
        fs::create_dir_all(&wt2).unwrap();
        fs::write(wt2.join("report.md"), "# report").unwrap();
        let mut body = spin_up_body("ada");
        teardown_body(&mut body, &wt2);
        assert_eq!(spin_up(&state, &body).status, 200);
        seed_artifact(&tmp, &wt2.join("report.md"), "claude:ada");
        let root = artifacts_root_of(&tmp);
        fs::remove_dir_all(&root).unwrap();
        fs::create_dir_all(root.parent().unwrap()).unwrap();
        fs::write(&root, "not a directory").unwrap();
        let torn = decommission_as(&state, OWNER, "claude%3Aada", "teardown");
        assert_eq!(torn.status, 200, "{}", error_text(&torn));
        assert_eq!(torn.json_value().unwrap()["status"], "decommissioned");
    }

    #[test]
    fn a_refused_peer_teardown_archives_nothing() {
        use fl_core::agent::{AgentRole, RegisterAgentArgs};
        use fl_core::store::AgentRegistry;

        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);
        let wt = tmp.path().join("wt-ada");
        fs::create_dir_all(&wt).unwrap();
        fs::write(wt.join("report.md"), "# report").unwrap();
        let mut body = spin_up_body("ada");
        teardown_body(&mut body, &wt);
        assert_eq!(spin_up(&state, &body).status, 200);
        assert_eq!(
            decommission_as(&state, OWNER, "claude%3Aada", "teardown").status,
            200
        );
        // The persona registers again outside the lifecycle: a Peer may not retire it, but the
        // stale record still names the worktree.
        state
            .open_store()
            .unwrap()
            .register_agent(RegisterAgentArgs {
                display_name: "Ada".into(),
                email: Some("ada@example.org".into()),
                project: "FerroLoop".into(),
                role: AgentRole::Assistant,
                extra_areas: Vec::new(),
                create_area: true,
                replace: true,
                at: stamp::now(),
                by: "ada".into(),
            })
            .unwrap();
        let root = artifacts_root_of(&tmp);
        let _ = fs::remove_dir_all(&root);
        seed_artifact(&tmp, &wt.join("report.md"), "claude:ada");

        let resp = decommission_as(&state, PEER, "claude%3Aada", "teardown");
        assert_eq!(resp.status, 403, "{}", error_text(&resp));
        assert!(managed_files(&tmp).is_empty());
        let reg = crate::artifacts::Registry::load(&root);
        assert!(reg.entries[0].managed_path.is_none());

        // The Owner is let through, and then it does archive.
        let resp = decommission_as(&state, OWNER, "claude%3Aada", "teardown");
        assert_eq!(resp.status, 200, "{}", error_text(&resp));
        assert_eq!(managed_files(&tmp).len(), 1);
    }

    // --- Record 4: unified settings ----------------------------------------------------------

    fn default_settings_json() -> Value {
        json!({
            "theme_preference": "dark",
            "subsystems": {
                "ferrogate_enabled": true,
                "ferrostate_enabled": true,
                "ferrostep_enabled": true,
            },
            "notifications": {
                "notify_on_pr_review": true,
                "notify_on_session_error": true,
                "notify_on_wire_urgent": true,
                "sound_enabled": false,
            },
            "hub_connection": { "auto_reconnect_ws": true, "poll_interval_secs": 10 },
        })
    }

    #[test]
    fn settings_get_returns_defaults_for_any_authenticated_caller() {
        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);
        let unauth = handle_http_request(&state, &HttpRequest::new("GET", "/api/v1/settings"));
        assert_eq!(unauth.status, 401);
        for token in [OWNER, PEER] {
            let resp = call(&state, "GET", "/api/v1/settings", token, &json!({}));
            assert_eq!(resp.status, 200, "{}", error_text(&resp));
            assert_eq!(resp.json_value().unwrap(), default_settings_json());
        }
        // Reading never writes the file.
        assert!(
            !tmp.path()
                .join(".config/fl/ferrodeck-settings.json")
                .exists()
        );
    }

    #[test]
    fn settings_put_is_owner_only_merges_partials_and_persists() {
        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);
        let url = "/api/v1/settings";
        let peer = call(
            &state,
            "PUT",
            url,
            PEER,
            &json!({ "theme_preference": "light" }),
        );
        assert_eq!(peer.status, 403, "{}", error_text(&peer));
        assert_eq!(
            call(&state, "GET", url, PEER, &json!({}))
                .json_value()
                .unwrap(),
            default_settings_json()
        );

        let resp = call(
            &state,
            "PUT",
            url,
            OWNER,
            &json!({
                "theme_preference": "system",
                "notifications": { "sound_enabled": true },
                "hub_connection": { "poll_interval_secs": 30 },
            }),
        );
        assert_eq!(resp.status, 200, "{}", error_text(&resp));
        let mut expected = default_settings_json();
        expected["theme_preference"] = json!("system");
        expected["notifications"]["sound_enabled"] = json!(true);
        expected["hub_connection"]["poll_interval_secs"] = json!(30);
        assert_eq!(resp.json_value().unwrap(), expected);

        // A second partial keeps the first one's changes.
        let resp = call(
            &state,
            "PUT",
            url,
            OWNER,
            &json!({ "subsystems": { "ferrostep_enabled": false } }),
        );
        assert_eq!(resp.status, 200);
        expected["subsystems"]["ferrostep_enabled"] = json!(false);
        assert_eq!(resp.json_value().unwrap(), expected);

        // Persisted: a fresh hub over the same home reads them back.
        let reloaded = test_hub_state(&tmp);
        let resp = call(&reloaded, "GET", url, PEER, &json!({}));
        assert_eq!(resp.json_value().unwrap(), expected);
        let on_disk: Value = serde_json::from_slice(
            &fs::read(tmp.path().join(".config/fl/ferrodeck-settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(on_disk, expected);
    }

    #[test]
    fn settings_put_rejects_bad_values_without_changing_anything() {
        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);
        let url = "/api/v1/settings";
        for bad in [
            json!({ "hub_connection": { "poll_interval_secs": 1 } }),
            json!({ "hub_connection": { "poll_interval_secs": 301 } }),
            json!({ "hub_connection": { "poll_interval_secs": -5 } }),
            json!({ "hub_connection": { "poll_interval_secs": "10" } }),
            json!({ "theme_preference": "purple" }),
            json!({ "theme_preference": 3 }),
            json!({ "colour": "red" }),
            json!({ "subsystems": { "ferrobogus_enabled": true } }),
            json!({ "notifications": { "sound_enabled": "loud" } }),
            // Valid fields beside an invalid one are not applied.
            json!({ "theme_preference": "light", "hub_connection": { "poll_interval_secs": 1 } }),
        ] {
            let resp = call(&state, "PUT", url, OWNER, &bad);
            assert_eq!(resp.status, 400, "{bad}: {}", error_text(&resp));
        }
        let ok = call(
            &state,
            "PUT",
            url,
            OWNER,
            &json!({ "hub_connection": { "poll_interval_secs": 2 } }),
        );
        assert_eq!(ok.status, 200);
        let ok = call(
            &state,
            "PUT",
            url,
            OWNER,
            &json!({ "hub_connection": { "poll_interval_secs": 300 } }),
        );
        assert_eq!(ok.status, 200);
        let bad_json = handle_http_request(
            &state,
            &HttpRequest::new("PUT", url)
                .with_header("Authorization", format!("Bearer {OWNER}"))
                .with_body(b"{ nope".to_vec()),
        );
        assert_eq!(bad_json.status, 400);
        // Nothing but the two valid interval changes ever reached the file.
        let mut expected = default_settings_json();
        expected["hub_connection"]["poll_interval_secs"] = json!(300);
        assert_eq!(
            call(&state, "GET", url, OWNER, &json!({}))
                .json_value()
                .unwrap(),
            expected
        );
    }

    #[test]
    fn settings_load_survives_a_damaged_file() {
        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);
        let path = tmp.path().join(".config/fl/ferrodeck-settings.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"{"theme_preference":"light","hub_connection":{"poll_interval_secs":"x"}}"#,
        )
        .unwrap();
        let resp = call(&state, "GET", "/api/v1/settings", OWNER, &json!({}));
        assert_eq!(resp.status, 200);
        let mut expected = default_settings_json();
        expected["theme_preference"] = json!("light");
        assert_eq!(resp.json_value().unwrap(), expected);
        fs::write(&path, "\u{0}garbage").unwrap();
        let resp = call(&state, "GET", "/api/v1/settings", OWNER, &json!({}));
        assert_eq!(resp.json_value().unwrap(), default_settings_json());
    }

    #[test]
    fn settings_put_broadcasts_settings_updated_and_a_refused_one_does_not() {
        let tmp = TempDir::new().unwrap();
        let state = test_hub_state(&tmp);
        let mut rx = state.subscribe();
        assert_eq!(
            call(
                &state,
                "PUT",
                "/api/v1/settings",
                PEER,
                &json!({ "theme_preference": "light" })
            )
            .status,
            403
        );
        assert_eq!(
            call(
                &state,
                "PUT",
                "/api/v1/settings",
                OWNER,
                &json!({ "theme_preference": "bad" })
            )
            .status,
            400
        );
        assert!(rx.try_recv().is_err(), "a refused PUT must not broadcast");

        let resp = call(
            &state,
            "PUT",
            "/api/v1/settings",
            OWNER,
            &json!({ "theme_preference": "light" }),
        );
        assert_eq!(resp.status, 200);
        let frame = rx.try_recv().expect("settings.updated frame");
        assert_eq!(frame.channel, "settings");
        assert_eq!(frame.event, "settings.updated");
        assert_eq!(frame.data, resp.json_value().unwrap());
        assert_eq!(frame.data["theme_preference"], "light");
    }
}
