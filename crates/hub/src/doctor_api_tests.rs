//! `GET /api/v1/doctor`: Owner only, the six `G7.*` checks in order, the contract FerroDeck
//! reads, and nothing written.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use fl_core::pr_policy::ReviewerApps;
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::{HttpRequest, HubServerConfig, HubState, handle_http_request};

const OWNER: &str = "owner-tok-secret";
const PEER: &str = "peer-tok-secret";
const IDS: [&str; 6] = [
    "G7.release-identity",
    "G7.receipt-integrity",
    "G7.path-ownership",
    "G7.store-health",
    "G7.vault-perms",
    "G7.code-ui-agree",
];

/// The environment variables the doctor consults. A developer's real vault, install root or
/// store selection must never reach a test.
const DOCTOR_ENV: [&str; 8] = [
    "FL_INSTALL_ROOT",
    "FL_VAULT_PATH",
    "FL_VAULT_KEY_PATH",
    "FL_VAULT_MASTER_KEY",
    "XDG_DATA_HOME",
    "XDG_CONFIG_HOME",
    "FL_STORE_BACKEND",
    "FL_DB",
];

/// Holds the lock that serialises these tests and removes [`DOCTOR_ENV`] until dropped, then
/// puts the values back.
struct CleanEnv {
    saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

fn clean_env() -> CleanEnv {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let saved = DOCTOR_ENV
        .iter()
        .map(|k| (*k, std::env::var_os(k)))
        .collect();
    for k in DOCTOR_ENV {
        // SAFETY: every test that reads these variables through the doctor holds LOCK.
        unsafe { std::env::remove_var(k) };
    }
    CleanEnv { saved, _lock: lock }
}

impl Drop for CleanEnv {
    fn drop(&mut self) {
        for (k, v) in self.saved.drain(..) {
            // SAFETY: still holding LOCK.
            unsafe {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
    }
}

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

fn request(state: &HubState, method: &str, token: Option<&str>) -> (u16, Value) {
    let mut req = HttpRequest::new(method, "/api/v1/doctor");
    if let Some(t) = token {
        req = req.with_header("Authorization", format!("Bearer {t}"));
    }
    let resp = handle_http_request(state, &req);
    (resp.status, resp.json_value().unwrap_or(Value::Null))
}

fn get(state: &HubState, token: Option<&str>) -> (u16, Value) {
    request(state, "GET", token)
}

fn check_status<'a>(v: &'a Value, id: &str) -> &'a str {
    v["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == id)
        .unwrap()["status"]
        .as_str()
        .unwrap()
}

fn detail<'a>(v: &'a Value, id: &str) -> &'a str {
    v["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == id)
        .unwrap()["detail"]
        .as_str()
        .unwrap()
}

fn snapshot(dir: &Path) -> BTreeMap<String, String> {
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
        for e in fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            let meta = fs::symlink_metadata(&p).unwrap();
            let content = if meta.is_file() {
                format!("{:?}", fs::read(&p).unwrap())
            } else {
                String::new()
            };
            out.insert(
                p.strip_prefix(base).unwrap().display().to_string(),
                format!(
                    "{:o} {} {:?} {content}",
                    meta.permissions().mode(),
                    meta.len(),
                    meta.modified().unwrap()
                ),
            );
            if meta.is_dir() {
                walk(base, &p, out);
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

#[test]
fn only_the_owner_may_read_the_doctor() {
    let _env = clean_env();
    let tmp = TempDir::new().unwrap();
    let state = hub(&tmp, true);
    assert_eq!(get(&state, Some(OWNER)).0, 200);
    let (status, body) = get(&state, Some(PEER));
    assert_eq!(status, 403);
    assert_eq!(body["error"], "owner authority required");
    assert!(
        body.get("checks").is_none(),
        "a peer learned something: {body}"
    );
    assert_eq!(get(&state, None).0, 401);
    assert_eq!(get(&state, Some("wrong-token")).0, 401);
    // Only GET is served.
    for method in ["POST", "PUT", "DELETE"] {
        let (status, _) = request(&state, method, Some(OWNER));
        assert_ne!(status, 200, "{method}");
    }
}

#[test]
fn the_body_is_the_six_ids_in_order_in_the_shape_ferrodeck_reads() {
    let _env = clean_env();
    let tmp = TempDir::new().unwrap();
    let (status, v) = get(&hub(&tmp, false), Some(OWNER));
    assert_eq!(status, 200, "{v}");
    let ids: Vec<&str> = v["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, IDS);
    let allowed = ["pass", "warn", "fail", "not-installed", "unsupported"];
    for c in v["checks"].as_array().unwrap() {
        let mut keys: Vec<&str> = c.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["detail", "id", "status", "title"]);
        assert!(allowed.contains(&c["status"].as_str().unwrap()), "{c}");
    }
    assert_eq!(v["schema_version"], 1);
    assert!(allowed.contains(&v["overall"].as_str().unwrap()));
    assert_ne!(v["overall"], "pass", "an empty hub is not healthy");
    for key in ["pass", "warn", "fail", "not_installed", "unsupported"] {
        assert!(v["summary"][key].is_u64(), "summary.{key}: {v}");
    }
    // Nothing is installed here: unmanaged, not healthy.
    assert_eq!(check_status(&v, "G7.receipt-integrity"), "not-installed");
    assert_eq!(
        detail(&v, "G7.receipt-integrity"),
        "unmanaged (no install receipt)"
    );
    assert_eq!(check_status(&v, "G7.store-health"), "not-installed");
    assert_eq!(check_status(&v, "G7.vault-perms"), "not-installed");
    assert_eq!(check_status(&v, "G7.code-ui-agree"), "not-installed");
}

#[test]
fn it_writes_nothing_and_is_deterministic() {
    let _env = clean_env();
    let tmp = TempDir::new().unwrap();
    let state = hub(&tmp, true);
    // A store, a vault and a UI bundle for it to look at.
    drop(fl_store::FerroStore::open(tmp.path().join("store.fls")).unwrap());
    let vault = tmp.path().join(".local/share/fl/vault.enc");
    fs::create_dir_all(vault.parent().unwrap()).unwrap();
    fs::write(&vault, "SENTINEL-VAULT").unwrap();
    fs::set_permissions(&vault, fs::Permissions::from_mode(0o600)).unwrap();
    let before = snapshot(tmp.path());
    let (_, a) = get(&state, Some(OWNER));
    let (_, b) = get(&state, Some(OWNER));
    assert_eq!(snapshot(tmp.path()), before, "the doctor wrote something");
    assert_eq!(a, b);
    let text = a.to_string();
    for needle in ["SENTINEL", OWNER, PEER] {
        assert!(!text.contains(needle), "{needle} leaked: {text}");
    }
}

#[test]
fn the_store_the_vault_and_the_ui_are_judged_from_the_hubs_own_context() {
    let _env = clean_env();
    let tmp = TempDir::new().unwrap();
    let state = hub(&tmp, true);
    // A healthy FerroStore at the configured path (`store.redb` is `store.fls` under ferro).
    drop(fl_store::FerroStore::open(tmp.path().join("store.fls")).unwrap());
    // A vault anyone may read.
    let vault = tmp.path().join(".local/share/fl/vault.enc");
    fs::create_dir_all(vault.parent().unwrap()).unwrap();
    fs::write(&vault, "x").unwrap();
    fs::set_permissions(&vault, fs::Permissions::from_mode(0o644)).unwrap();
    // A UI bundle from another commit.
    fs::write(
        tmp.path().join("ui/build-info.json"),
        json!({"commit": "fedcba9876543210fedcba9876543210fedcba98"}).to_string(),
    )
    .unwrap();
    let (_, v) = get(&state, Some(OWNER));
    assert_eq!(
        check_status(&v, "G7.store-health"),
        "pass",
        "{}",
        detail(&v, "G7.store-health")
    );
    assert_eq!(check_status(&v, "G7.vault-perms"), "fail");
    assert!(detail(&v, "G7.vault-perms").contains("0644"));
    let hub_known = crate::version_api::build_info().commit_known();
    assert_eq!(
        check_status(&v, "G7.code-ui-agree"),
        if hub_known { "fail" } else { "unsupported" }
    );
    assert_eq!(v["overall"], "fail");
}

#[test]
fn a_store_held_by_a_writer_is_a_warning_and_the_request_does_not_wait_long() {
    let _env = clean_env();
    let tmp = TempDir::new().unwrap();
    let mut state = hub(&tmp, false);
    state.store_backend = Some(fl_store::BackendKind::Redb);
    let _writer = fl_store::RedbStore::open(&tmp.path().join("store.redb")).unwrap();
    let t = std::time::Instant::now();
    let (status_code, v) = get(&state, Some(OWNER));
    assert_eq!(status_code, 200);
    assert!(
        t.elapsed() < std::time::Duration::from_secs(3),
        "{:?}",
        t.elapsed()
    );
    assert_eq!(
        check_status(&v, "G7.store-health"),
        "warn",
        "{}",
        detail(&v, "G7.store-health")
    );
    assert!(detail(&v, "G7.store-health").contains("held by a writer"));
    // The hub's own store lock was not taken: another request could have run.
    assert!(state.store_lock.try_lock().is_ok());
}

#[test]
fn a_corrupt_receipt_in_the_hubs_home_fails() {
    let _env = clean_env();
    let tmp = TempDir::new().unwrap();
    let state = hub(&tmp, false);
    let prefix = tmp.path().join(".local/share/fl");
    fs::create_dir_all(&prefix).unwrap();
    fs::write(prefix.join("receipt.json"), "{").unwrap();
    let (_, v) = get(&state, Some(OWNER));
    assert_eq!(check_status(&v, "G7.receipt-integrity"), "fail");
    assert_eq!(check_status(&v, "G7.path-ownership"), "unsupported");
    assert_eq!(v["overall"], "fail");
}

#[test]
fn a_store_the_hub_itself_holds_open_is_verified_at_once() {
    let _env = clean_env();
    let tmp = TempDir::new().unwrap();
    let state = hub(&tmp, false);
    // The hub's real case: its own guard holds the store (and `store_lock`) while the doctor
    // runs. The check takes neither, so it neither waits nor deadlocks.
    let guard = state.open_store().unwrap();
    let before = snapshot(tmp.path());
    let t = std::time::Instant::now();
    let (code, v) = get(&state, Some(OWNER));
    assert_eq!(code, 200, "{v}");
    assert!(
        t.elapsed() < std::time::Duration::from_secs(2),
        "{:?}",
        t.elapsed()
    );
    assert_eq!(
        check_status(&v, "G7.store-health"),
        "pass",
        "{}",
        detail(&v, "G7.store-health")
    );
    assert_eq!(
        snapshot(tmp.path()),
        before,
        "the doctor changed the store directory"
    );
    drop(guard);
}

#[test]
fn everything_is_resolved_under_the_test_home() {
    let _env = clean_env();
    let tmp = TempDir::new().unwrap();
    let state = hub(&tmp, true);
    drop(fl_store::FerroStore::open(tmp.path().join("store.fls")).unwrap());
    for rel in [".local/share/fl/vault.enc", ".config/fl/vault.key"] {
        let p = tmp.path().join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, "x").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(p.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
    }
    let (_, v) = get(&state, Some(OWNER));
    let home = tmp.path().to_str().unwrap();
    assert!(
        detail(&v, "G7.vault-perms").contains("owner matches"),
        "{v}"
    );
    let mut seen = 0;
    for c in v["checks"].as_array().unwrap() {
        for word in c["detail"]
            .as_str()
            .unwrap()
            .split(|ch: char| ch.is_whitespace() || ch == ';' || ch == ',' || ch == ':')
        {
            if word.starts_with('/') {
                seen += 1;
                assert!(
                    word.starts_with(home),
                    "{} names {word}, outside {home}",
                    c["id"]
                );
            }
        }
    }
    assert!(seen >= 1, "the store path must appear: {v}");
    // No path the real environment could have supplied.
    let text = v.to_string();
    if let Some(real) = std::env::var_os("HOME").and_then(|h| h.into_string().ok())
        && !home.starts_with(&real)
    {
        assert!(!text.contains(&real), "{real} leaked into {text}");
    }
}
