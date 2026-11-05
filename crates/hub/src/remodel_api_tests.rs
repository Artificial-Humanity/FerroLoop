//! `POST /api/v1/sessions/:id/remodel` (Local Record 59): status codes, the response shape, the
//! model override and queue it leaves, its broadcast, and two remodels racing.

use fl_core::agent::{AgentRole, AgentStatus, RegisterAgentArgs};
use fl_core::pr_policy::ReviewerApps;
use fl_core::store::AgentRegistry;
use fl_exec::stamp;
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::lifecycle::{self, HostedSessionRecord};
use crate::session_actions::ModelOverride;
use crate::{HttpRequest, HttpResponse, HubServerConfig, HubState, handle_http_request};

const OWNER: &str = "owner-tok-secret";
const PEER: &str = "peer-tok-secret";

fn hub(tmp: &TempDir) -> HubState {
    HubState::new(HubServerConfig {
        listen: "127.0.0.1:0".into(),
        store_path: tmp.path().join("store.redb"),
        repo_root: tmp.path().to_path_buf(),
        ui_dir: None,
        owner_login: "lmcfarlin".into(),
        reviewer_apps: ReviewerApps::default(),
        owner_token: Some(OWNER.into()),
        peer_token: Some(PEER.into()),
        trust_tailscale_headers: false,
        enable_terminal: false,
        session_ttl_secs: 3600,
        home_override: Some(tmp.path().to_path_buf()),
        github: crate::github_pr::HubGithub::default(),
    })
}

fn post(state: &HubState, path: &str, token: &str, body: &Value) -> HttpResponse {
    handle_http_request(
        state,
        &HttpRequest::new("POST", path)
            .with_header("Authorization", format!("Bearer {token}"))
            .with_json_body(body),
    )
}

fn remodel(state: &HubState, id: &str, token: &str, body: &Value) -> HttpResponse {
    post(
        state,
        &format!("/api/v1/sessions/{id}/remodel"),
        token,
        body,
    )
}

fn spin_up(state: &HubState, persona: &str, role: &str, vendor: &str) {
    let resp = post(
        state,
        "/api/v1/sessions/spin-up",
        OWNER,
        &json!({ "persona": persona, "project": "FerroLoop", "role": role, "vendor": vendor }),
    );
    assert_eq!(resp.status, 200, "{}", body_text(&resp));
}

fn body_text(resp: &HttpResponse) -> String {
    String::from_utf8_lossy(&resp.body).into_owned()
}

fn records(state: &HubState) -> Vec<HostedSessionRecord> {
    lifecycle::list_hosted_sessions(&*state.open_store().unwrap()).unwrap()
}

fn status_of(state: &HubState, persona: &str) -> AgentStatus {
    state
        .open_store()
        .unwrap()
        .agent_by_name(persona)
        .unwrap()
        .unwrap()
        .status
}

fn override_model(state: &HubState, key: &str) -> Option<String> {
    state.session_model_override(key).map(|o| o.model)
}

#[test]
fn execute_returns_the_new_session_sets_the_model_override_and_broadcasts() {
    let tmp = TempDir::new().unwrap();
    let state = hub(&tmp);
    spin_up(&state, "felicity", "assistant", "claude");
    // The operator chose a model for the old run, and a prompt waits for it.
    for key in ["felicity", "claude:felicity"] {
        state.set_session_model_override(
            key,
            ModelOverride {
                model: "claude-opus-5-5".into(),
                thinking_level: None,
            },
        );
        state.enqueue_session_prompt(
            key,
            Some("felicity".into()),
            "stale".into(),
            "stale".into(),
            "owner".into(),
            Vec::new(),
        );
    }
    let mut frames = state.subscribe();
    let invalidations =
        || crate::discovery::cost_counters::ROUTE_CACHE_INVALIDATIONS.with(std::cell::Cell::get);
    let before = invalidations();
    let resp = remodel(
        &state,
        "claude%3Afelicity",
        OWNER,
        &json!({ "vendor": "codex", "model": "gpt-6-sol" }),
    );
    assert_eq!(resp.status, 200, "{}", body_text(&resp));
    // The old runtime stopped and a new one may have started: the route scan was dropped.
    assert!(invalidations() > before);
    let out = resp.json_value().unwrap();
    for (key, want) in [
        ("mode", json!("execute")),
        ("session_id", json!("codex:felicity")),
        ("previous_session_id", json!("claude:felicity")),
        ("persona", json!("felicity")),
        ("project", json!("FerroLoop")),
        ("role", json!("assistant")),
        ("from_vendor", json!("claude")),
        ("vendor", json!("codex")),
        ("model", json!("gpt-6-sol")),
        ("node_id", json!("ai-lab-0")),
        ("hosted", json!(true)),
        // `FerroLoop` is no area with a directory here, so its agent started in the workspace
        // root (this hub's repo root), and the remodel keeps it.
        ("cwd", json!(tmp.path())),
        ("status", json!("running")),
        ("handshake_dispatched", json!(false)),
        ("hydration_dispatched", json!(false)),
        // Runtime calls are off in tests: nothing was started, so nothing is reported stopped.
        ("runtime_stopped", json!(false)),
        ("tmux_target", json!("Felicity:codex")),
        ("handoff", json!("remodel-felicity")),
        ("warnings", json!([])),
    ] {
        assert_eq!(out[key], want, "{key}");
    }
    assert!(
        out["prompt"]
            .as_str()
            .unwrap()
            .contains("take up residence in FerroLoop/")
    );

    // The old run's override and queue are gone; the new model shows under the persona.
    assert_eq!(override_model(&state, "claude:felicity"), None);
    assert_eq!(
        override_model(&state, "felicity").as_deref(),
        Some("gpt-6-sol")
    );
    assert!(state.session_queue("claude:felicity").is_empty());
    assert!(state.session_queue("felicity").is_empty());

    let frame = frames.try_recv().unwrap();
    assert_eq!(
        (frame.channel.as_str(), frame.event.as_str()),
        ("sessions", "session.remodeled")
    );
    assert_eq!(frame.data, out);
    assert!(frames.try_recv().is_err());

    let recs = records(&state);
    assert_eq!(recs.len(), 2);
    assert_eq!(recs[0].session_id, "claude:felicity");
    assert_eq!(recs[0].status, "decommissioned");
    assert_eq!(recs[1].session_id, "codex:felicity");
    assert_eq!(recs[1].status, "running");
    assert_eq!(recs[1].model.as_deref(), Some("gpt-6-sol"));
    assert_eq!(status_of(&state, "felicity"), AgentStatus::Active);

    // A remodel with no model takes the new vendor's default: no override is left at all.
    let resp = remodel(&state, "felicity", OWNER, &json!({ "vendor": "grok" }));
    assert_eq!(resp.status, 200, "{}", body_text(&resp));
    assert_eq!(resp.json_value().unwrap()["model"], Value::Null);
    for key in ["felicity", "codex:felicity", "grok:felicity"] {
        assert_eq!(override_model(&state, key), None, "{key}");
    }
}

#[test]
fn errors_map_like_the_sibling_lifecycle_endpoints_and_change_nothing() {
    let tmp = TempDir::new().unwrap();
    let state = hub(&tmp);
    state
        .open_store()
        .unwrap()
        .register_agent(RegisterAgentArgs {
            display_name: "Jarvis".into(),
            email: None,
            project: "ferroloop".into(),
            role: AgentRole::LabManager,
            extra_areas: Vec::new(),
            create_area: true,
            replace: false,
            at: stamp::now(),
            by: "owner".into(),
        })
        .unwrap();
    spin_up(&state, "felicity", "assistant", "claude");
    let before = records(&state);
    let mut frames = state.subscribe();

    let unauth = handle_http_request(
        &state,
        &HttpRequest::new("POST", "/api/v1/sessions/claude%3Afelicity/remodel")
            .with_json_body(&json!({ "vendor": "codex" })),
    );
    assert_eq!(unauth.status, 401);
    for (id, token, body, want) in [
        // A Peer may not remodel any session, hosted or not.
        ("claude%3Ajarvis", PEER, json!({ "vendor": "codex" }), 403),
        ("ghost", OWNER, json!({ "vendor": "codex" }), 404),
        (
            "claude%3Afelicity",
            OWNER,
            json!({ "vendor": "gemini" }),
            400,
        ),
        ("claude%3Afelicity", OWNER, json!({}), 400),
        (
            "claude%3Afelicity",
            OWNER,
            json!({ "vendor": "codex", "model": "a\nb" }),
            400,
        ),
        (
            "claude%3Afelicity",
            OWNER,
            json!({ "vendor": "codex", "mode": "teardown" }),
            400,
        ),
        (
            "claude%3Afelicity",
            OWNER,
            json!({ "vendor": "codex", "persona": "jarvis" }),
            400,
        ),
        (
            "claude%3Afelicity",
            OWNER,
            json!({ "vendor": "codex", "cwd": "/no/such/remodel/dir" }),
            400,
        ),
    ] {
        let resp = remodel(&state, id, token, &body);
        assert_eq!(resp.status, want, "{id} {body}: {}", body_text(&resp));
        assert!(resp.json_value().unwrap()["error"].is_string());
    }
    let not_json = handle_http_request(
        &state,
        &HttpRequest::new("POST", "/api/v1/sessions/claude%3Afelicity/remodel")
            .with_header("Authorization", format!("Bearer {OWNER}"))
            .with_body(b"{nope".to_vec()),
    );
    assert_eq!(not_json.status, 400);

    assert_eq!(records(&state), before);
    assert_eq!(status_of(&state, "jarvis"), AgentStatus::Active);
    assert_eq!(status_of(&state, "felicity"), AgentStatus::Active);
    assert!(frames.try_recv().is_err());
}

#[test]
fn a_peer_cannot_remodel_a_hosted_session_and_nothing_changes() {
    let tmp = TempDir::new().unwrap();
    let state = hub(&tmp);
    spin_up(&state, "felicity", "assistant", "claude");
    state.set_session_model_override(
        "claude:felicity",
        ModelOverride {
            model: "claude-opus-5-5".into(),
            thinking_level: None,
        },
    );
    state.enqueue_session_prompt(
        "claude:felicity",
        Some("felicity".into()),
        "queued".into(),
        "queued".into(),
        "owner".into(),
        Vec::new(),
    );
    let before = records(&state);
    let invalidations =
        || crate::discovery::cost_counters::ROUTE_CACHE_INVALIDATIONS.with(std::cell::Cell::get);
    let invalidated = invalidations();
    let mut frames = state.subscribe();

    // Both phases, and an unparsable body: the gate comes before the body is read.
    for body in [
        json!({ "vendor": "codex" }),
        json!({ "vendor": "codex", "mode": "prepare" }),
        json!({}),
    ] {
        let resp = remodel(&state, "claude%3Afelicity", PEER, &body);
        assert_eq!(resp.status, 403, "{body}: {}", body_text(&resp));
        assert!(body_text(&resp).contains("Owner"), "{body}");
    }
    let raw = handle_http_request(
        &state,
        &HttpRequest::new("POST", "/api/v1/sessions/claude%3Afelicity/remodel")
            .with_header("Authorization", format!("Bearer {PEER}"))
            .with_body(b"{nope".to_vec()),
    );
    assert_eq!(raw.status, 403);

    assert_eq!(records(&state), before);
    assert_eq!(status_of(&state, "felicity"), AgentStatus::Active);
    assert_eq!(state.session_queue("claude:felicity").len(), 1);
    assert_eq!(
        override_model(&state, "claude:felicity").as_deref(),
        Some("claude-opus-5-5")
    );
    assert_eq!(invalidations(), invalidated, "no runtime was touched");
    assert!(
        frames.try_recv().is_err(),
        "a refused call broadcasts nothing"
    );

    // The Owner still can.
    let resp = remodel(&state, "felicity", OWNER, &json!({ "vendor": "codex" }));
    assert_eq!(resp.status, 200, "{}", body_text(&resp));
}

#[test]
fn prepare_queues_an_undelivered_handshake_and_stops_nothing() {
    let tmp = TempDir::new().unwrap();
    let state = hub(&tmp);
    spin_up(&state, "felicity", "assistant", "claude");
    let resp = remodel(
        &state,
        "felicity",
        OWNER,
        &json!({ "vendor": "codex", "mode": "prepare" }),
    );
    assert_eq!(resp.status, 200, "{}", body_text(&resp));
    let out = resp.json_value().unwrap();
    assert_eq!(out["mode"], "prepare");
    assert_eq!(out["status"], "decommissioning");
    assert_eq!(out["session_id"], "claude:felicity");
    assert_eq!(out["handshake_dispatched"], false);
    let queue = state.session_queue("claude:felicity");
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0].prompt, out["prompt"].as_str().unwrap());
    assert_eq!(records(&state)[0].status, "decommissioning");
    assert_eq!(status_of(&state, "felicity"), AgentStatus::Active);

    let resp = remodel(&state, "felicity", OWNER, &json!({ "vendor": "codex" }));
    assert_eq!(resp.status, 200, "{}", body_text(&resp));
    assert!(state.session_queue("claude:felicity").is_empty());
    assert_eq!(resp.json_value().unwrap()["session_id"], "codex:felicity");
}

#[test]
fn a_lead_remodel_through_the_hub_keeps_its_seat() {
    let tmp = TempDir::new().unwrap();
    let state = hub(&tmp);
    spin_up(&state, "ferris", "lead", "claude");
    let resp = remodel(&state, "ferris", OWNER, &json!({ "vendor": "muse" }));
    assert_eq!(resp.status, 200, "{}", body_text(&resp));
    let lead = state
        .open_store()
        .unwrap()
        .agent_by_name("ferris")
        .unwrap()
        .unwrap();
    assert_eq!(
        (lead.status, lead.role),
        (AgentStatus::Active, AgentRole::Lead)
    );
}

#[test]
fn concurrent_remodels_of_one_persona_leave_exactly_one_live_session() {
    let tmp = TempDir::new().unwrap();
    let state = hub(&tmp);
    spin_up(&state, "felicity", "assistant", "claude");
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let racers: Vec<_> = ["codex", "grok", "muse"]
        .into_iter()
        .map(|vendor| {
            let state = state.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                let body = json!({ "vendor": vendor, "model": format!("m-{vendor}") });
                remodel(&state, "felicity", OWNER, &body).status
            })
        })
        .collect();
    let statuses: Vec<u16> = racers.into_iter().map(|r| r.join().unwrap()).collect();
    // The store guard serializes them: each remodels whatever the one before left running.
    assert_eq!(statuses, [200, 200, 200]);
    let live: Vec<HostedSessionRecord> = records(&state)
        .into_iter()
        .filter(|r| r.status != "decommissioned")
        .collect();
    assert_eq!(live.len(), 1, "{live:?}");
    assert_eq!(status_of(&state, "felicity"), AgentStatus::Active);
    // The last remodel's writes are the last ones: its model shows under the persona, and no
    // earlier remodel's late write replaced it.
    let last = format!("m-{}", live[0].vendor);
    assert_eq!(live[0].model.as_deref(), Some(last.as_str()));
    for key in ["felicity", live[0].session_id.as_str()] {
        assert_eq!(
            override_model(&state, key).as_deref(),
            Some(last.as_str()),
            "{key}"
        );
    }
}

#[test]
fn a_failed_execute_broadcasts_its_outcome_and_a_restore_puts_the_override_back() {
    let tmp = TempDir::new().unwrap();
    let state = hub(&tmp);
    spin_up(&state, "felicity", "assistant", "claude");
    let chosen = ModelOverride {
        model: "claude-opus-5-5".into(),
        thinking_level: Some("high".into()),
    };
    for key in ["felicity", "claude:felicity"] {
        state.set_session_model_override(key, chosen.clone());
    }
    lifecycle::FAIL_SPIN_UP_VENDORS.with_borrow_mut(|v| v.push("codex".into()));
    let mut frames = state.subscribe();

    let resp = remodel(
        &state,
        "claude%3Afelicity",
        OWNER,
        &json!({ "vendor": "codex" }),
    );
    assert_eq!(resp.status, 502, "{}", body_text(&resp));
    let out = resp.json_value().unwrap();
    assert_eq!(out["outcome"], "restored");
    assert_eq!(out["restored_session_id"], "claude:felicity");
    assert_eq!(out["persona"], "felicity");
    assert_eq!(out["vendor"], "codex");
    assert!(
        out["error"]
            .as_str()
            .unwrap()
            .contains("codex binary is missing")
    );
    let frame = frames.try_recv().unwrap();
    assert_eq!(
        (frame.channel.as_str(), frame.event.as_str()),
        ("sessions", "session.remodel_failed")
    );
    assert_eq!(frame.data, out);
    assert!(frames.try_recv().is_err());
    // The restored session runs the model and thinking level the operator chose before.
    for key in ["felicity", "claude:felicity"] {
        assert_eq!(
            state.session_model_override(key),
            Some(chosen.clone()),
            "{key}"
        );
    }

    // The restore fails too: the persona is down, and nothing is put back.
    lifecycle::FAIL_SPIN_UP_VENDORS.with_borrow_mut(|v| v.push("claude".into()));
    let resp = remodel(&state, "felicity", OWNER, &json!({ "vendor": "codex" }));
    assert_eq!(resp.status, 502, "{}", body_text(&resp));
    let out = resp.json_value().unwrap();
    assert_eq!(out["outcome"], "down");
    assert!(out["restored_session_id"].is_null());
    let frame = frames.try_recv().unwrap();
    assert_eq!(frame.event, "session.remodel_failed");
    assert_eq!(frame.data["outcome"], "down");
    for key in ["felicity", "claude:felicity"] {
        assert_eq!(state.session_model_override(key), None, "{key}");
    }
    lifecycle::FAIL_SPIN_UP_VENDORS.with_borrow_mut(Vec::clear);
}
