//! The context window of a spun-up session and the hub-wide defaults (Local Record #134):
//! settings GET and PUT, who may choose, validation on spin-up, and default resolution.

use fl_core::pr_policy::ReviewerApps;
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::lifecycle;
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

fn send(
    state: &HubState,
    method: &str,
    path: &str,
    token: &str,
    body: Option<&Value>,
) -> HttpResponse {
    let mut req =
        HttpRequest::new(method, path).with_header("Authorization", format!("Bearer {token}"));
    if let Some(body) = body {
        req = req.with_json_body(body);
    }
    handle_http_request(state, &req)
}

fn json_of(resp: &HttpResponse) -> Value {
    serde_json::from_slice(&resp.body).unwrap_or(Value::Null)
}

fn error_of(resp: &HttpResponse) -> String {
    json_of(resp)["error"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

fn write_grok_cache(tmp: &TempDir) {
    let dir = tmp.path().join(".grok");
    std::fs::create_dir_all(&dir).unwrap();
    let cache = json!({"models": {
        "grok-x": {"info": {"context_window": 256_000, "context_windows": [256_000, 500_000]}},
        "grok-y": {"info": {"context_window": 256_000, "context_windows": [256_000]}},
    }});
    std::fs::write(dir.join("models_cache.json"), cache.to_string()).unwrap();
}

fn spin_up_body(persona: &str, vendor: &str, model: Option<&str>, window: Option<Value>) -> Value {
    let mut body = json!({ "persona": persona, "project": "FerroLoop", "role": "assistant", "vendor": vendor });
    if let Some(model) = model {
        body["model"] = json!(model);
    }
    if let Some(window) = window {
        body["context_window"] = window;
    }
    body
}

fn spin_up(state: &HubState, token: &str, body: &Value) -> HttpResponse {
    send(state, "POST", "/api/v1/sessions/spin-up", token, Some(body))
}

fn put_defaults(state: &HubState, token: &str, defaults: &Value) -> HttpResponse {
    send(
        state,
        "PUT",
        "/api/v1/settings",
        token,
        Some(&json!({ "context_window_defaults": defaults })),
    )
}

fn record(state: &HubState, session_id: &str) -> Value {
    let resp = send(state, "GET", "/api/v1/hosted-sessions", OWNER, None);
    assert_eq!(resp.status, 200);
    json_of(&resp)["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["session_id"] == session_id)
        .cloned()
        .unwrap()
}

#[test]
fn the_defaults_round_trip_through_settings_and_are_absent_when_empty() {
    let tmp = TempDir::new().unwrap();
    let state = hub(&tmp);
    let got = json_of(&send(&state, "GET", "/api/v1/settings", OWNER, None));
    assert!(got.get("context_window_defaults").is_none(), "{got}");

    let put = put_defaults(&state, OWNER, &json!({ "grok": 500_000 }));
    assert_eq!(put.status, 200, "{}", error_of(&put));
    assert_eq!(
        json_of(&put)["context_window_defaults"],
        json!({ "grok": 500_000 })
    );
    let got = json_of(&send(&state, "GET", "/api/v1/settings", OWNER, None));
    assert_eq!(got["context_window_defaults"], json!({ "grok": 500_000 }));
    // The file keeps it, and another patch leaves it alone.
    let other = send(
        &state,
        "PUT",
        "/api/v1/settings",
        OWNER,
        Some(&json!({ "timezone": "Asia/Tokyo" })),
    );
    assert_eq!(other.status, 200);
    assert_eq!(
        json_of(&other)["context_window_defaults"],
        json!({ "grok": 500_000 })
    );

    // null removes a vendor's default, and an empty map leaves the key out again.
    let cleared = put_defaults(&state, OWNER, &json!({ "grok": null }));
    assert_eq!(cleared.status, 200);
    assert!(json_of(&cleared).get("context_window_defaults").is_none());
}

#[test]
fn the_defaults_refuse_unselectable_vendors_and_bad_values() {
    let tmp = TempDir::new().unwrap();
    let state = hub(&tmp);
    for vendor in ["claude", "codex", "muse", "antigravity", "nope"] {
        let resp = put_defaults(&state, OWNER, &json!({ vendor: 200_000 }));
        assert_eq!(resp.status, 400, "{vendor}");
        assert!(
            error_of(&resp).contains("not selectable"),
            "{}",
            error_of(&resp)
        );
    }
    let claude = error_of(&put_defaults(
        &state,
        OWNER,
        &json!({ "claude": 1_000_000 }),
    ));
    assert!(
        claude.contains("claude's context window is not selectable yet"),
        "{claude}"
    );
    for bad in [
        json!({ "grok": 0 }),
        json!({ "grok": -1 }),
        json!({ "grok": "500k" }),
        json!({ "grok": 1.5 }),
    ] {
        assert_eq!(put_defaults(&state, OWNER, &bad).status, 400, "{bad}");
    }
    // A refused patch writes nothing.
    let got = json_of(&send(&state, "GET", "/api/v1/settings", OWNER, None));
    assert!(got.get("context_window_defaults").is_none());
}

#[test]
fn only_the_owner_changes_the_defaults_or_chooses_a_window() {
    let tmp = TempDir::new().unwrap();
    write_grok_cache(&tmp);
    let state = hub(&tmp);
    assert_eq!(
        put_defaults(&state, PEER, &json!({ "grok": 500_000 })).status,
        403
    );
    let got = json_of(&send(&state, "GET", "/api/v1/settings", PEER, None));
    assert!(got.get("context_window_defaults").is_none());

    let body = spin_up_body("ada", "grok", Some("grok-x"), Some(json!(500_000)));
    assert_eq!(spin_up(&state, PEER, &body).status, 403);
    assert!(
        lifecycle::list_hosted_sessions(&*state.open_store().unwrap())
            .unwrap()
            .is_empty()
    );
    // A Peer may still spin up without choosing one.
    let plain = spin_up_body("ada", "grok", Some("grok-x"), None);
    assert_eq!(spin_up(&state, PEER, &plain).status, 200);
}

#[test]
fn a_chosen_window_is_validated_recorded_and_shown() {
    let tmp = TempDir::new().unwrap();
    write_grok_cache(&tmp);
    let state = hub(&tmp);
    let resp = spin_up(
        &state,
        OWNER,
        &spin_up_body("ada", "grok", Some("grok-x"), Some(json!(500_000))),
    );
    assert_eq!(resp.status, 200, "{}", error_of(&resp));
    assert_eq!(json_of(&resp)["context_window"], 500_000);
    assert_eq!(record(&state, "grok:ada")["context_window"], 500_000);

    // Without one, neither the response nor the record carries the key.
    let none = spin_up(
        &state,
        OWNER,
        &spin_up_body("bob", "grok", Some("grok-x"), None),
    );
    assert_eq!(none.status, 200);
    assert!(json_of(&none).get("context_window").is_none());
    assert!(record(&state, "grok:bob").get("context_window").is_none());
}

#[test]
fn a_refused_window_is_a_400_that_names_why_and_registers_nothing() {
    let tmp = TempDir::new().unwrap();
    write_grok_cache(&tmp);
    let state = hub(&tmp);
    let cases = [
        (
            spin_up_body("ada", "grok", Some("grok-x"), Some(json!(400_000))),
            "256000, 500000",
        ),
        (
            spin_up_body("ada", "grok", Some("grok-unlisted"), Some(json!(256_000))),
            "not available",
        ),
        (
            spin_up_body("ada", "grok", None, Some(json!(256_000))),
            "name a model",
        ),
        (
            spin_up_body("ada", "claude", Some("opus[1m]"), Some(json!(1_000_000))),
            "not selectable yet",
        ),
        (
            spin_up_body("ada", "codex", None, Some(json!(272_000))),
            "codex's context window is not selectable",
        ),
        (
            spin_up_body("ada", "grok", Some("grok-x"), Some(json!(0))),
            "positive",
        ),
        (
            spin_up_body("ada", "grok", Some("grok-x"), Some(json!("500k"))),
            "positive integer",
        ),
        (
            spin_up_body("ada", "grok", Some("grok-x"), Some(json!(-5))),
            "positive integer",
        ),
        (
            spin_up_body("ada", "grok", Some("grok-x"), Some(json!(2.5))),
            "positive integer",
        ),
    ];
    for (body, want) in cases {
        let resp = spin_up(&state, OWNER, &body);
        assert_eq!(resp.status, 400, "{body}");
        assert!(
            error_of(&resp).contains(want),
            "{body}: {}",
            error_of(&resp)
        );
    }
    assert!(
        lifecycle::list_hosted_sessions(&*state.open_store().unwrap())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_default_applies_only_when_it_suits_the_model_and_never_fails_a_spin_up() {
    let tmp = TempDir::new().unwrap();
    write_grok_cache(&tmp);
    let state = hub(&tmp);
    assert_eq!(
        put_defaults(&state, OWNER, &json!({ "grok": 500_000 })).status,
        200
    );

    // grok-x offers 500k: the default is used.
    let used = spin_up(
        &state,
        PEER,
        &spin_up_body("ada", "grok", Some("grok-x"), None),
    );
    assert_eq!(used.status, 200, "{}", error_of(&used));
    assert_eq!(json_of(&used)["context_window"], 500_000);
    assert_eq!(record(&state, "grok:ada")["context_window"], 500_000);

    // An explicit choice wins over the default.
    let explicit = spin_up(
        &state,
        OWNER,
        &spin_up_body("bob", "grok", Some("grok-x"), Some(json!(256_000))),
    );
    assert_eq!(json_of(&explicit)["context_window"], 256_000);

    // grok-y does not offer it, an unlisted model and no model have no list: all ignored.
    for (persona, model) in [
        ("cy", Some("grok-y")),
        ("dee", Some("grok-unlisted")),
        ("eve", None),
    ] {
        let resp = spin_up(&state, OWNER, &spin_up_body(persona, "grok", model, None));
        assert_eq!(resp.status, 200, "{persona}: {}", error_of(&resp));
        assert!(json_of(&resp).get("context_window").is_none(), "{persona}");
    }

    // Another vendor has no default.
    let codex = spin_up(&state, OWNER, &spin_up_body("fay", "codex", None, None));
    assert_eq!(codex.status, 200);
    assert!(json_of(&codex).get("context_window").is_none());

    // A cache that disappears later does not fail the spin-up either.
    std::fs::remove_file(tmp.path().join(".grok/models_cache.json")).unwrap();
    let gone = spin_up(
        &state,
        OWNER,
        &spin_up_body("gus", "grok", Some("grok-x"), None),
    );
    assert_eq!(gone.status, 200);
    assert!(json_of(&gone).get("context_window").is_none());
}
