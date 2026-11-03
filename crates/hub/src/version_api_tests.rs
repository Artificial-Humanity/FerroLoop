//! `GET /api/v1/version`: the hub's build, the UI bundle's, and whether they agree.

use std::fs;

use fl_core::build_info::BuildInfo;
use fl_core::pr_policy::ReviewerApps;
use serde_json::{Value, json};
use tempfile::TempDir;

use super::{MAX_UI_BUILD_INFO_BYTES, UI_BUILD_INFO_FILE, body, parse_ui_build_info};
use crate::{HttpRequest, HubServerConfig, HubState, handle_http_request};

const OWNER: &str = "owner-tok-secret";
const PEER: &str = "peer-tok-secret";
const C: &str = "0123456789abcdef0123456789abcdef01234567";

fn hub(tmp: &TempDir, with_ui: bool) -> HubState {
    let ui = tmp.path().join("ui");
    fs::create_dir_all(&ui).unwrap();
    HubState::new(HubServerConfig {
        listen: "127.0.0.1:0".into(),
        store_path: tmp.path().join("store.redb"),
        repo_root: tmp.path().to_path_buf(),
        ui_dir: with_ui.then_some(ui),
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

fn get(state: &HubState, token: Option<&str>) -> (u16, Value) {
    let mut req = HttpRequest::new("GET", "/api/v1/version");
    if let Some(t) = token {
        req = req.with_header("Authorization", format!("Bearer {t}"));
    }
    let resp = handle_http_request(state, &req);
    (resp.status, resp.json_value().unwrap_or(Value::Null))
}

fn write_ui(tmp: &TempDir, text: &str) {
    fs::create_dir_all(tmp.path().join("ui")).unwrap();
    fs::write(tmp.path().join("ui").join(UI_BUILD_INFO_FILE), text).unwrap();
}

fn known_hub(dirty: &str) -> BuildInfo {
    BuildInfo::from_stamp("0.1.0", C, dirty, "42", None)
}

#[test]
fn the_route_reports_the_hub_build_and_no_ui_without_a_file() {
    let tmp = TempDir::new().unwrap();
    for with_ui in [false, true] {
        let (status, v) = get(&hub(&tmp, with_ui), Some(OWNER));
        assert_eq!(status, 200, "{v}");
        for key in [
            "name",
            "version",
            "commit",
            "commit_short",
            "dirty",
            "build_time_unix",
            "source",
            "release_id",
            "ui",
            "code_ui_agree",
        ] {
            assert!(v.get(key).is_some(), "missing {key}: {v}");
        }
        assert_eq!(v["name"], "fl-hub");
        assert_eq!(v["source"], "source-build");
        assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(v["ui"], Value::Null);
        assert_eq!(v["code_ui_agree"], Value::Null);
    }
}

#[test]
fn a_peer_may_read_and_an_unauthenticated_caller_may_not() {
    let tmp = TempDir::new().unwrap();
    let state = hub(&tmp, true);
    assert_eq!(get(&state, Some(PEER)).0, 200);
    assert_eq!(get(&state, Some(OWNER)).0, 200);
    assert_eq!(get(&state, None).0, 401);
    assert_eq!(get(&state, Some("wrong-token")).0, 401);
}

#[test]
fn the_answer_leaks_no_path_and_no_secret() {
    let tmp = TempDir::new().unwrap();
    write_ui(
        &tmp,
        &json!({"commit": C, "version": "1", "path": "/etc/shadow", "token": "s3cret"}).to_string(),
    );
    let (_, v) = get(&hub(&tmp, true), Some(PEER));
    let text = v.to_string();
    for needle in [
        tmp.path().to_str().unwrap(),
        OWNER,
        PEER,
        "/etc/shadow",
        "s3cret",
    ] {
        assert!(!text.contains(needle), "{needle} leaked in {text}");
    }
}

#[test]
fn a_ui_build_info_file_is_reported_through_the_route() {
    let tmp = TempDir::new().unwrap();
    write_ui(
        &tmp,
        &json!({"commit": "0000000abcdef", "build_time_unix": 7, "version": "0.1.0"}).to_string(),
    );
    let (status, v) = get(&hub(&tmp, true), Some(OWNER));
    assert_eq!(status, 200);
    assert_eq!(
        v["ui"],
        json!({"commit": "0000000abcdef", "build_time_unix": 7, "version": "0.1.0"})
    );
    // The hub is a source build of some commit that is not 0000000...: disagreement, or
    // `null` when this tree carries no commit at all.
    let hub_known = v["commit"] != "unknown";
    assert_eq!(
        v["code_ui_agree"],
        if hub_known { json!(false) } else { Value::Null }
    );
}

#[test]
fn agreement_is_decided_on_the_commit() {
    let tmp = TempDir::new().unwrap();
    let ui = tmp.path().join("ui");
    fs::create_dir_all(&ui).unwrap();
    let hub = known_hub("true");
    let check = |text: &str| {
        fs::write(ui.join(UI_BUILD_INFO_FILE), text).unwrap();
        body(&hub, Some(&ui))
    };
    // Same commit, full or abbreviated: they agree, a dirty hub or not.
    let v = check(&json!({"commit": C}).to_string());
    assert_eq!(v["code_ui_agree"], true);
    assert_eq!(v["dirty"], true);
    assert_eq!(
        check(&json!({"commit": &C[..7]}).to_string())["code_ui_agree"],
        true
    );
    // A different commit, and one that only shares a SHORT prefix, do not.
    let other = format!("f{}", &C[1..]);
    assert_eq!(
        check(&json!({"commit": other}).to_string())["code_ui_agree"],
        false
    );
    assert_eq!(
        check(&json!({"commit": "0123456fffff"}).to_string())["code_ui_agree"],
        false
    );
    // An unknown hub commit decides nothing.
    let unknown = BuildInfo::from_stamp("0.1.0", "unknown", "unknown", "1", None);
    fs::write(
        ui.join(UI_BUILD_INFO_FILE),
        json!({"commit": C}).to_string(),
    )
    .unwrap();
    let v = body(&unknown, Some(&ui));
    assert_eq!(v["code_ui_agree"], Value::Null);
    assert_eq!(v["ui"]["commit"], C);
}

#[test]
fn a_bad_ui_file_is_a_null_ui_not_an_error() {
    let tmp = TempDir::new().unwrap();
    let state = hub(&tmp, true);
    let bad = [
        "".to_string(),
        "not json".to_string(),
        "[1,2]".to_string(),
        "{}".to_string(),
        json!({"commit": 5}).to_string(),
        json!({"commit": "abc"}).to_string(),
        json!({"commit": "XYZXYZXYZ"}).to_string(),
        json!({"commit": "../../etc/passwd"}).to_string(),
        // Valid JSON, but too big.
        format!(
            "{{\"commit\":\"{C}\",\"pad\":\"{}\"}}",
            "x".repeat(MAX_UI_BUILD_INFO_BYTES as usize)
        ),
    ];
    for text in &bad {
        write_ui(&tmp, text);
        let (status, v) = get(&state, Some(OWNER));
        assert_eq!(status, 200, "{text:.40}");
        assert_eq!(v["ui"], Value::Null, "{text:.40}");
        assert_eq!(v["code_ui_agree"], Value::Null);
    }
    // A bad optional field is dropped, the file kept.
    let v = parse_ui_build_info(
        &json!({"commit": C, "build_time_unix": -1, "version": "x".repeat(200)}).to_string(),
    )
    .unwrap();
    assert_eq!(v["build_time_unix"], Value::Null);
    assert_eq!(v["version"], Value::Null);
}

#[test]
fn a_file_just_under_the_limit_is_read_and_a_directory_or_symlink_is_not() {
    let tmp = TempDir::new().unwrap();
    let state = hub(&tmp, true);
    let ui = tmp.path().join("ui");
    let head = format!("{{\"commit\":\"{C}\",\"pad\":\"");
    let tail = "\"}";
    let pad = MAX_UI_BUILD_INFO_BYTES as usize - head.len() - tail.len();
    write_ui(&tmp, &format!("{head}{}{tail}", "x".repeat(pad)));
    assert_eq!(get(&state, Some(OWNER)).1["ui"]["commit"], C);

    // A directory named build-info.json.
    fs::remove_file(ui.join(UI_BUILD_INFO_FILE)).unwrap();
    fs::create_dir(ui.join(UI_BUILD_INFO_FILE)).unwrap();
    assert_eq!(get(&state, Some(OWNER)).1["ui"], Value::Null);
    fs::remove_dir(ui.join(UI_BUILD_INFO_FILE)).unwrap();

    // A symlink to a valid file elsewhere is not followed.
    let elsewhere = tmp.path().join("elsewhere.json");
    fs::write(&elsewhere, json!({"commit": C}).to_string()).unwrap();
    std::os::unix::fs::symlink(&elsewhere, ui.join(UI_BUILD_INFO_FILE)).unwrap();
    assert_eq!(get(&state, Some(OWNER)).1["ui"], Value::Null);
}
