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
            "is not in Grok's model list",
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

// ---- POST /api/v1/sessions/:id/context-window -------------------------------------------

/// Points `FL_WIRE_TMUX_BIN` at a fake tmux that reports every pane alive, logs its arguments in
/// `<dir>/tmux.out` and appends what is loaded into a paste buffer to `<dir>/pasted.out`. The
/// variable is restored on drop. Hold `crate::TMUX_ENV_LOCK` while it lives.
struct FakeTmux {
    dir: std::path::PathBuf,
    prev: Option<std::ffi::OsString>,
}

impl FakeTmux {
    fn install(dir: &std::path::Path) -> Self {
        use std::os::unix::fs::PermissionsExt;

        let bin = dir.join("fake-tmux.sh");
        std::fs::write(
            &bin,
            format!(
                "#!/bin/sh\necho \"$*\" >> \"{log}\"\nif [ \"$1\" = \"display-message\" ] || [ \"$1\" = \"list-panes\" ]; then\n  echo \"%12\"\nelif [ \"$1\" = \"load-buffer\" ]; then\n  cat >> \"{pasted}\"\nfi\n",
                log = dir.join("tmux.out").display(),
                pasted = dir.join("pasted.out").display(),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let prev = std::env::var_os("FL_WIRE_TMUX_BIN");
        // SAFETY: callers hold TMUX_ENV_LOCK; the previous value is restored on drop.
        unsafe { std::env::set_var("FL_WIRE_TMUX_BIN", &bin) };
        Self {
            dir: dir.to_path_buf(),
            prev,
        }
    }

    /// Every text pasted into a pane so far, one entry per paste.
    fn pasted(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.join("pasted.out"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn tmux_calls(&self) -> String {
        std::fs::read_to_string(self.dir.join("tmux.out")).unwrap_or_default()
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

fn install_grok_route(tmp: &TempDir, persona: &str, session_id: &str) {
    use fl_core::at::At;
    use fl_wire::identity::{RouteTable, VendorRoute};

    let path = tmp.path().join(".cache/agent-bus/routes.json");
    let mut routes = RouteTable::load(Some(&path));
    routes.upsert(
        persona,
        VendorRoute::Grok {
            session_id: session_id.into(),
            cwd: None,
            tmux_target: Some(format!("grok:{persona}")),
            tmux_pane: Some("%12".into()),
        },
        At::from_unix_millis(1),
    );
    routes.save(Some(&path)).unwrap();
}

fn set_window(state: &HubState, token: &str, target: &str, body: &Value) -> HttpResponse {
    send(
        state,
        "POST",
        &format!("/api/v1/sessions/{target}/context-window"),
        token,
        Some(body),
    )
}

#[test]
fn a_peer_cannot_set_a_window_and_the_body_is_not_read() {
    let _lock = crate::TMUX_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    write_grok_cache(&tmp);
    let state = hub(&tmp);
    let tmux = FakeTmux::install(tmp.path());
    assert_eq!(
        spin_up(
            &state,
            OWNER,
            &spin_up_body("ada", "grok", Some("grok-x"), None)
        )
        .status,
        200
    );
    install_grok_route(&tmp, "ada", "gk-ada");
    let mut frames = state.subscribe();

    // A valid body, a malformed one and no body at all: 403 every time, never a 400.
    let url = "/api/v1/sessions/ada/context-window";
    for body in [
        Some("{\"context_window\":500000}"),
        Some("{ not json"),
        None,
    ] {
        let mut req =
            HttpRequest::new("POST", url).with_header("Authorization", format!("Bearer {PEER}"));
        if let Some(body) = body {
            req.body = body.as_bytes().to_vec();
        }
        let resp = handle_http_request(&state, &req);
        assert_eq!(resp.status, 403, "{body:?}");
    }
    // No token at all is not a Peer: 401.
    let anon = handle_http_request(
        &state,
        &HttpRequest::new("POST", url).with_json_body(&json!({ "context_window": 500_000 })),
    );
    assert_eq!(anon.status, 401);

    // Nothing was stored, sent or announced.
    assert!(record(&state, "grok:ada").get("context_window").is_none());
    assert_eq!(tmux.tmux_calls(), "");
    assert!(tmux.pasted().is_empty());
    assert!(frames.try_recv().is_err());
}

#[test]
fn an_unknown_session_is_a_404_and_a_bad_body_is_a_400() {
    let _lock = crate::TMUX_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    write_grok_cache(&tmp);
    let state = hub(&tmp);
    let tmux = FakeTmux::install(tmp.path());
    let unknown = set_window(
        &state,
        OWNER,
        "nobody",
        &json!({ "context_window": 500_000 }),
    );
    assert_eq!(unknown.status, 404, "{}", error_of(&unknown));

    assert_eq!(
        spin_up(
            &state,
            OWNER,
            &spin_up_body("ada", "grok", Some("grok-x"), None)
        )
        .status,
        200
    );
    install_grok_route(&tmp, "ada", "gk-ada");
    for bad in [
        json!({ "context_window": "500000" }),
        json!({ "context_window": "500k" }),
        json!({ "context_window": 0 }),
        json!({ "context_window": -5 }),
        json!({ "context_window": 2.5 }),
        json!({ "context_window": 500_000.0 }),
        json!({ "context_window": true }),
        json!({ "context_window": [500_000] }),
        json!({ "context_window": 1e30 }),
        json!({}),
        json!({ "window": 500_000 }),
    ] {
        let resp = set_window(&state, OWNER, "ada", &bad);
        assert_eq!(resp.status, 400, "{bad}");
        assert!(error_of(&resp).contains("context_window"), "{bad}");
    }
    let not_object = send(
        &state,
        "POST",
        "/api/v1/sessions/ada/context-window",
        OWNER,
        Some(&json!([1])),
    );
    assert_eq!(not_object.status, 400);
    // Nothing was stored or sent.
    assert!(record(&state, "grok:ada").get("context_window").is_none());
    assert!(tmux.pasted().is_empty());
}

#[test]
fn a_grok_window_is_validated_stored_sent_live_and_cleared_with_null() {
    let _lock = crate::TMUX_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    // The cache lists 256000 and 500000 for grok-x.
    write_grok_cache(&tmp);
    let state = hub(&tmp);
    let tmux = FakeTmux::install(tmp.path());
    assert_eq!(
        spin_up(
            &state,
            OWNER,
            &spin_up_body("ada", "grok", Some("grok-x"), None)
        )
        .status,
        200
    );
    install_grok_route(&tmp, "ada", "gk-ada");
    let mut frames = state.subscribe();

    let ok = set_window(&state, OWNER, "ada", &json!({ "context_window": 500_000 }));
    assert_eq!(ok.status, 200, "{}", error_of(&ok));
    assert_eq!(
        json_of(&ok),
        json!({
            "ok": true,
            "session_id": "gk-ada",
            "vendor": "grok",
            "context_window": 500_000,
            "persisted": true,
            "method": "operator-prompt",
        })
    );
    assert_eq!(tmux.pasted(), vec!["/context-window 500000"]);
    let frame = frames.try_recv().unwrap();
    assert_eq!(
        (frame.channel.as_str(), frame.event.as_str()),
        ("sessions", "session.context_window_changed")
    );
    assert_eq!(frame.data["context_window"], 500_000);
    // The stored value is on the record and in the hosted-sessions list.
    assert_eq!(record(&state, "grok:ada")["context_window"], 500_000);
    // The model's own default size is valid too (a no-op, but accepted).
    let own = set_window(
        &state,
        OWNER,
        "gk-ada",
        &json!({ "context_window": 256_000 }),
    );
    assert_eq!(own.status, 200, "{}", error_of(&own));
    assert_eq!(json_of(&own)["persisted"], true);
    assert_eq!(record(&state, "grok:ada")["context_window"], 256_000);
    assert_eq!(
        tmux.pasted(),
        vec!["/context-window 500000", "/context-window 256000"]
    );

    // A size the model does not offer is a 400 that names the sizes; the record and the CLI are
    // left as they were.
    let refused = set_window(&state, OWNER, "ada", &json!({ "context_window": 400_000 }));
    assert_eq!(refused.status, 400);
    let why = error_of(&refused);
    assert!(
        why.contains("256000, 500000") && why.contains("grok-x"),
        "{why}"
    );
    assert_eq!(record(&state, "grok:ada")["context_window"], 256_000);
    assert_eq!(tmux.pasted().len(), 2);

    // null clears the record and sends nothing.
    let cleared = set_window(&state, OWNER, "ada", &json!({ "context_window": null }));
    assert_eq!(cleared.status, 200, "{}", error_of(&cleared));
    assert_eq!(
        json_of(&cleared),
        json!({
            "ok": true,
            "session_id": "gk-ada",
            "vendor": "grok",
            "context_window": null,
            "persisted": true,
            "method": "next-launch",
        })
    );
    assert!(record(&state, "grok:ada").get("context_window").is_none());
    assert_eq!(tmux.pasted().len(), 2);
}

#[test]
fn the_model_comes_from_the_operators_choice_before_the_record() {
    let _lock = crate::TMUX_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    write_grok_cache(&tmp);
    let state = hub(&tmp);
    let tmux = FakeTmux::install(tmp.path());
    // grok-y offers only 256000; the record says grok-x, which also offers 500000.
    assert_eq!(
        spin_up(
            &state,
            OWNER,
            &spin_up_body("ada", "grok", Some("grok-x"), None)
        )
        .status,
        200
    );
    install_grok_route(&tmp, "ada", "gk-ada");
    state.set_session_model_override(
        "gk-ada",
        crate::session_actions::ModelOverride::new("grok-y".into(), None),
    );
    let refused = set_window(&state, OWNER, "ada", &json!({ "context_window": 500_000 }));
    assert_eq!(refused.status, 400);
    assert!(
        error_of(&refused).contains("grok-y"),
        "{}",
        error_of(&refused)
    );
    assert!(tmux.pasted().is_empty());
}

#[test]
fn without_a_hosted_record_nothing_is_persisted_and_the_live_apply_is_all_there_is() {
    let _lock = crate::TMUX_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    write_grok_cache(&tmp);
    let state = hub(&tmp);
    let tmux = FakeTmux::install(tmp.path());
    install_grok_route(&tmp, "bob", "gk-bob");
    // No record and no known model: the sizes cannot be checked.
    let unknown_model = set_window(&state, OWNER, "bob", &json!({ "context_window": 500_000 }));
    assert_eq!(unknown_model.status, 400);
    assert!(
        error_of(&unknown_model).contains("not available"),
        "{}",
        error_of(&unknown_model)
    );
    assert!(tmux.pasted().is_empty());

    state.set_session_model_override(
        "gk-bob",
        crate::session_actions::ModelOverride::new("grok-x".into(), None),
    );
    let ok = set_window(&state, OWNER, "bob", &json!({ "context_window": 500_000 }));
    assert_eq!(ok.status, 200, "{}", error_of(&ok));
    assert_eq!(
        json_of(&ok),
        json!({
            "ok": true,
            "session_id": "gk-bob",
            "vendor": "grok",
            "context_window": 500_000,
            "persisted": false,
            "method": "operator-prompt",
        })
    );
    assert_eq!(tmux.pasted(), vec!["/context-window 500000"]);
    assert!(
        lifecycle::list_hosted_sessions(&*state.open_store().unwrap())
            .unwrap()
            .is_empty()
    );
    // Clearing a session with no record changes nothing and sends nothing.
    let cleared = set_window(&state, OWNER, "bob", &json!({ "context_window": null }));
    assert_eq!(cleared.status, 200);
    assert_eq!(json_of(&cleared)["persisted"], false);
    assert_eq!(json_of(&cleared)["method"], "next-launch");
    assert_eq!(tmux.pasted().len(), 1);
}

#[test]
fn a_failed_live_send_still_stores_and_reports_next_launch() {
    let _lock = crate::TMUX_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    write_grok_cache(&tmp);
    let state = hub(&tmp);
    assert_eq!(
        spin_up(
            &state,
            OWNER,
            &spin_up_body("ada", "grok", Some("grok-x"), None)
        )
        .status,
        200
    );
    // A tmux that fails every call: the pane is not alive and the headless fallback binary
    // does not exist, so the send fails.
    let bin = tmp.path().join("failing-tmux.sh");
    std::fs::write(&bin, "#!/bin/sh\nexit 1\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let prev = std::env::var_os("FL_WIRE_TMUX_BIN");
    let prev_grok = std::env::var_os("FL_WIRE_GROK_BIN");
    // SAFETY: TMUX_ENV_LOCK is held; both variables are restored below.
    unsafe {
        std::env::set_var("FL_WIRE_TMUX_BIN", &bin);
        std::env::set_var("FL_WIRE_GROK_BIN", tmp.path().join("no-such-grok"));
    }
    install_grok_route(&tmp, "ada", "gk-ada");
    let resp = set_window(&state, OWNER, "ada", &json!({ "context_window": 500_000 }));
    // SAFETY: as above.
    unsafe {
        match prev {
            Some(v) => std::env::set_var("FL_WIRE_TMUX_BIN", v),
            None => std::env::remove_var("FL_WIRE_TMUX_BIN"),
        }
        match prev_grok {
            Some(v) => std::env::set_var("FL_WIRE_GROK_BIN", v),
            None => std::env::remove_var("FL_WIRE_GROK_BIN"),
        }
    }
    assert_eq!(resp.status, 200, "{}", error_of(&resp));
    let body = json_of(&resp);
    assert_eq!(body["method"], "next-launch");
    assert_eq!(body["persisted"], true);
    assert_eq!(record(&state, "grok:ada")["context_window"], 500_000);
}

#[test]
fn another_vendor_is_refused_and_a_record_only_session_stores_for_the_next_launch() {
    let _lock = crate::TMUX_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    write_grok_cache(&tmp);
    let state = hub(&tmp);
    let tmux = FakeTmux::install(tmp.path());
    for (persona, vendor, model, want) in [
        (
            "cy",
            "claude",
            Some("opus[1m]"),
            "claude's context window is not selectable yet",
        ),
        (
            "dee",
            "codex",
            None,
            "codex's context window is not selectable",
        ),
    ] {
        let spun = spin_up(&state, OWNER, &spin_up_body(persona, vendor, model, None));
        assert_eq!(spun.status, 200, "{}", error_of(&spun));
        let resp = set_window(
            &state,
            OWNER,
            persona,
            &json!({ "context_window": 1_000_000 }),
        );
        assert_eq!(resp.status, 400, "{vendor}");
        assert_eq!(error_of(&resp), want);
        let key = format!("{vendor}:{persona}");
        assert!(record(&state, &key).get("context_window").is_none());
    }
    // Clearing is allowed for any vendor and sends nothing.
    let cleared = set_window(&state, OWNER, "cy", &json!({ "context_window": null }));
    assert_eq!(cleared.status, 200, "{}", error_of(&cleared));
    assert_eq!(json_of(&cleared)["vendor"], "claude");
    assert_eq!(json_of(&cleared)["method"], "next-launch");
    assert_eq!(json_of(&cleared)["persisted"], true);

    // A Grok record with no live route: stored, applied at the next launch.
    assert_eq!(
        spin_up(
            &state,
            OWNER,
            &spin_up_body("eve", "grok", Some("grok-x"), None)
        )
        .status,
        200
    );
    let stored = set_window(
        &state,
        OWNER,
        "grok:eve",
        &json!({ "context_window": 500_000 }),
    );
    assert_eq!(stored.status, 200, "{}", error_of(&stored));
    assert_eq!(json_of(&stored)["method"], "next-launch");
    assert_eq!(json_of(&stored)["persisted"], true);
    assert_eq!(record(&state, "grok:eve")["context_window"], 500_000);
    assert!(tmux.pasted().is_empty());
}
