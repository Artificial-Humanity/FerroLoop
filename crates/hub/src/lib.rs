//! `fl-hub`: FerroLoop Hub HTTP/1.1 & WebSocket daemon, two-tier authentication
//! (`HubAuthority::Owner` vs. `HubAuthority::Peer`), host session/worktree/architecture
//! discovery, static `FerroDeck` serving, and `/api/v1/*` REST API (spec §2–§4).

pub mod api;
pub mod auth;
pub mod discovery;
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
        }
    }

    pub fn session_queue(&self, session_key: &str) -> Vec<QueuedOperatorPrompt> {
        let Ok(guard) = self.prompt_queues.lock() else {
            return Vec::new();
        };
        guard.queues.get(session_key).cloned().unwrap_or_default()
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
            let completed = (turns.len() > in_flight.baseline_turn_count
                || in_flight.baseline_busy)
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
    pub fn is_session_busy_released(&self, session_id: &str) -> bool {
        let snap = match self.open_store() {
            Ok(store) => discovery::snapshot_discovery_store(&store),
            Err(_) => return false,
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
    pub fn mark_session_in_flight_released(&self, session_key: &str) {
        let snap = match self.open_store() {
            Ok(store) => discovery::snapshot_discovery_store(&store),
            Err(_) => return,
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
        for auth in [&owner, &peer] {
            let resp = handle_http_request(
                &state,
                &HttpRequest::new("POST", "/api/v1/state/checkpoint")
                    .with_header("Authorization", auth)
                    .with_json_body(&json!({ "slug": "hub-pairing-forged" })),
            );
            assert_eq!(resp.status, 403);
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
}
