//! Hub endpoints for dedicated artifact layout consolidation (#77/#186/#207).

use std::fs;
use std::path::PathBuf;

use fl_core::at::At;
use fl_core::pr_policy::ReviewerApps;
use fl_core::state::{
    MergeMode, RuntimeEntry, SensitivityPatch, StateConcern, StatePayload, StateScope,
    UpsertStateArgs,
};
use fl_core::store::StateStore;
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::artifact_dirs::effective_system_dir;
use crate::settings;
use crate::{HttpRequest, HttpResponse, HubServerConfig, HubState, handle_http_request};

const OWNER: &str = "owner-tok-secret";
const PEER: &str = "peer-tok-secret";
const PATH: &str = "/api/v1/artifacts/consolidate";

struct Env {
    _tmp: TempDir,
    base: PathBuf,
    state: HubState,
}

impl Env {
    fn data(&self) -> PathBuf {
        self.base.join("data")
    }
}

fn env() -> Env {
    let tmp = TempDir::new().unwrap();
    let base = fs::canonicalize(tmp.path()).unwrap();
    for dir in ["home", "data", "repo"] {
        fs::create_dir_all(base.join(dir)).unwrap();
    }
    let mut state = HubState::new(HubServerConfig {
        listen: "127.0.0.1:0".into(),
        store_path: base.join("data/store.redb"),
        repo_root: base.join("repo"),
        ui_dir: None,
        owner_login: "lmcfarlin".into(),
        reviewer_apps: ReviewerApps::default(),
        owner_token: Some(OWNER.into()),
        peer_token: Some(PEER.into()),
        trust_tailscale_headers: false,
        enable_terminal: false,
        session_ttl_secs: 3600,
        home_override: Some(base.join("home")),
        github: crate::github_pr::HubGithub::default(),
    });
    state.store_backend = Some(fl_store::BackendKind::Ferro);
    Env {
        _tmp: tmp,
        base,
        state,
    }
}

fn send(env: &Env, method: &str, token: &str, body: Option<&Value>) -> HttpResponse {
    let mut req =
        HttpRequest::new(method, PATH).with_header("Authorization", format!("Bearer {token}"));
    if let Some(body) = body {
        req = req.with_json_body(body);
    }
    handle_http_request(&env.state, &req)
}

fn json_of(resp: &HttpResponse) -> Value {
    resp.json_value().unwrap()
}

fn seed_corrupt(env: &Env) -> Value {
    let corrupt = json!({"bogus": 1});
    env.state
        .open_store()
        .unwrap()
        .upsert_state(UpsertStateArgs {
            scope: StateScope::Workspace,
            slug: fl_core::DEDICATED_ARTIFACT_LAYOUT_SLUG.into(),
            sensitivity: SensitivityPatch::Inherit,
            payload: StatePayload::Runtime(RuntimeEntry {
                value: corrupt.clone(),
                merge_mode: MergeMode::LastWriter,
            }),
            at: At::from_unix_millis(1),
            by: "test".into(),
        })
        .unwrap();
    corrupt
}

fn stored_value(env: &Env) -> Option<Value> {
    let store = env.state.open_store().unwrap();
    let entry = store
        .get_state_by_key(
            &StateScope::Workspace,
            StateConcern::Runtime,
            fl_core::DEDICATED_ARTIFACT_LAYOUT_SLUG,
        )
        .unwrap()?;
    let StatePayload::Runtime(rt) = &entry.payload else {
        return None;
    };
    Some(rt.value.clone())
}

fn origin<'a>(layout: &'a Value, vendor: &str) -> &'a Value {
    layout["vendor_origins"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["vendor"] == vendor)
        .unwrap()
}

#[test]
fn consolidate_get_defaults() {
    let env = env();
    for token in [OWNER, PEER] {
        let resp = send(&env, "GET", token, None);
        assert_eq!(resp.status, 200);
        let layout = json_of(&resp);
        // Placeholder rule: default root replaced by hub effective system dir.
        assert_eq!(
            layout["canonical_root"],
            json!(effective_system_dir(&env.state).display().to_string())
        );
        assert_eq!(
            layout["canonical_root"],
            json!(env.data().join("artifacts").display().to_string())
        );
        assert_eq!(origin(&layout, "codex")["consolidated"], json!(true));
        assert_eq!(origin(&layout, "antigravity")["consolidated"], json!(false));
        assert_eq!(origin(&layout, "claude")["consolidated"], json!(false));
        assert_eq!(layout["retention_policy"]["keep_latest_versions"], json!(5));
    }
}

#[test]
fn consolidate_post_dry_run_does_not_persist() {
    let env = env();
    let resp = send(&env, "POST", OWNER, Some(&json!({ "dry_run": true })));
    assert_eq!(resp.status, 200);
    let body = json_of(&resp);
    assert_eq!(body["dry_run"], json!(true));
    assert_eq!(body["consolidated_files"], json!(29));
    assert_eq!(body["consolidated_bytes"], json!(4194304 + 1572864));
    assert_eq!(body["consolidated_bytes"], json!(5767168));
    // GET after still unconsolidated.
    let layout = json_of(&send(&env, "GET", OWNER, None));
    assert_eq!(origin(&layout, "antigravity")["consolidated"], json!(false));
    assert_eq!(origin(&layout, "claude")["consolidated"], json!(false));
}

#[test]
fn consolidate_post_real_marks_all_and_second_is_zero() {
    let env = env();
    let body = json_of(&send(
        &env,
        "POST",
        OWNER,
        Some(&json!({ "dry_run": false })),
    ));
    assert_eq!(body["dry_run"], json!(false));
    assert_eq!(body["consolidated_files"], json!(29));
    assert_eq!(body["consolidated_bytes"], json!(5767168));
    assert!(
        body["layout"]["vendor_origins"]
            .as_array()
            .unwrap()
            .iter()
            .all(|o| o["consolidated"] == json!(true))
    );
    let layout = json_of(&send(&env, "GET", OWNER, None));
    assert!(
        layout["vendor_origins"]
            .as_array()
            .unwrap()
            .iter()
            .all(|o| o["consolidated"] == json!(true))
    );
    let second = json_of(&send(
        &env,
        "POST",
        OWNER,
        Some(&json!({ "dry_run": false })),
    ));
    assert_eq!(second["consolidated_files"], json!(0));
    assert_eq!(second["consolidated_bytes"], json!(0));
}

#[test]
fn consolidate_post_retention_update_persists() {
    let env = env();
    let retention = json!({
        "keep_latest_versions": 3,
        "auto_archive_days": 7,
        "pin_ratified_specs": false
    });
    let resp = send(
        &env,
        "POST",
        OWNER,
        Some(&json!({ "dry_run": false, "retention_policy": retention })),
    );
    assert_eq!(resp.status, 200);
    assert_eq!(json_of(&resp)["layout"]["retention_policy"], retention);
    let layout = json_of(&send(&env, "GET", OWNER, None));
    assert_eq!(layout["retention_policy"], retention);
}

#[test]
fn consolidate_post_retention_keep_zero_is_400() {
    let env = env();
    let resp = send(
        &env,
        "POST",
        OWNER,
        Some(&json!({
            "dry_run": false,
            "retention_policy": {
                "keep_latest_versions": 0,
                "auto_archive_days": 7,
                "pin_ratified_specs": false
            }
        })),
    );
    assert_eq!(resp.status, 400);
    // Non-bool dry_run is also a 400.
    let resp = send(&env, "POST", OWNER, Some(&json!({ "dry_run": "yes" })));
    assert_eq!(resp.status, 400);
}

#[test]
fn consolidate_post_peer_is_403() {
    let env = env();
    let resp = send(&env, "POST", PEER, Some(&json!({ "dry_run": false })));
    assert_eq!(resp.status, 403);
}

#[test]
fn consolidate_get_after_post_root_stable_when_settings_change() {
    let env = env();
    let posted = json_of(&send(
        &env,
        "POST",
        OWNER,
        Some(&json!({ "dry_run": false })),
    ));
    let root = posted["layout"]["canonical_root"].clone();
    assert_eq!(
        root,
        json!(env.data().join("artifacts").display().to_string())
    );
    assert_eq!(
        json_of(&send(&env, "GET", OWNER, None))["canonical_root"],
        root
    );
    // Settings later point elsewhere; the persisted root must not move.
    let moved = env.base.join("targets/moved");
    settings::update_artifact_dirs(&env.base.join("home"), |s| {
        s.artifacts_dir = Some(moved.display().to_string());
    })
    .unwrap();
    assert_eq!(effective_system_dir(&env.state), moved);
    assert_eq!(
        json_of(&send(&env, "GET", OWNER, None))["canonical_root"],
        root
    );
}

#[test]
fn consolidate_corrupt_state_500s_and_is_untouched() {
    let env = env();
    let corrupt = seed_corrupt(&env);
    for body in [json!({ "dry_run": true }), json!({ "dry_run": false })] {
        let resp = send(&env, "POST", OWNER, Some(&body));
        assert_eq!(resp.status, 500, "{body}");
    }
    assert_eq!(stored_value(&env), Some(corrupt));
}

#[test]
fn consolidate_dry_run_retention_preview_is_not_persisted() {
    let env = env();
    let retention = json!({
        "keep_latest_versions": 9,
        "auto_archive_days": 90,
        "pin_ratified_specs": false
    });
    let resp = send(
        &env,
        "POST",
        OWNER,
        Some(&json!({ "dry_run": true, "retention_policy": retention })),
    );
    assert_eq!(resp.status, 200);
    assert_eq!(json_of(&resp)["layout"]["retention_policy"], retention);
    let layout = json_of(&send(&env, "GET", OWNER, None));
    assert_eq!(layout["retention_policy"]["keep_latest_versions"], json!(5));
    assert_eq!(stored_value(&env), None);
}
