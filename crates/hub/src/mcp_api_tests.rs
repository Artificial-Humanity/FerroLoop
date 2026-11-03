//! `GET /api/v1/mcp/catalog` and `POST /api/v1/mcp/sync`: Owner only, one row per known repo, a
//! sync that writes the project's vendor files and nothing else, no secret or value in an answer.
//! Every test works in temporary directories: the ownership records are redirected into one, and
//! a planted home directory must come out byte for byte as it went in.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use fl_core::pr_policy::ReviewerApps;
use serde_json::{Value, json};
use tempfile::TempDir;

use super::{project_relative, repo_name};
use crate::{HttpRequest, HubServerConfig, HubState, handle_http_request};

const OWNER: &str = "owner-tok-secret";
const PEER: &str = "peer-tok-secret";

/// Planted in the catalog as an `env` value and as a header value: it must reach the vendor
/// files, since fl writes literals, and no answer of the hub.
const CANARY_ENV: &str = "CANARY-ENV-VALUE-7f3a91";
const CANARY_HEADER: &str = "CANARY-HEADER-VALUE-c40e2d";
const CANARY_COMMAND: &str = "canary-command-bin-55d1";
const CANARY_ARG: &str = "--canary-arg-9b8e";

const CATALOG: &str = r#"
[server.alpha]
transport = "stdio"
command = "canary-command-bin-55d1"
args = ["--canary-arg-9b8e"]
env.ALPHA_MODE = "CANARY-ENV-VALUE-7f3a91"
env.ALPHA_TOKEN = { secret = true }

[server.beta]
transport = "http"
url = "https://beta.example.com/mcp"
vendors = ["claude"]
headers.X-Canary = "CANARY-HEADER-VALUE-c40e2d"
headers.Authorization = { secret = true, env = "BETA_TOKEN", scheme = "Bearer" }

[server.gamma]
enabled = false
transport = "stdio"
command = "gamma-bin"
"#;

/// The five vendor files `fl mcp sync` writes, relative to the project.
const VENDOR_FILES: [&str; 5] = [
    ".mcp.json",
    ".codex/config.toml",
    ".agents/mcp_config.json",
    ".muse/settings.json",
    ".grok/config.toml",
];

const GITIGNORE: &str = "/.mcp.json\n/.codex/config.toml\n/.agents/mcp_config.json\n/.muse/settings.json\n/.grok/config.toml\n";

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A git repository at `dir` that ignores the vendor files, with `catalog` as its catalog when
/// given.
fn repo(dir: &Path, catalog: Option<&str>) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q"]);
    fs::write(dir.join(".gitignore"), GITIGNORE).unwrap();
    if let Some(text) = catalog {
        fs::create_dir_all(dir.join(".fl")).unwrap();
        fs::write(dir.join(".fl/mcp.toml"), text).unwrap();
    }
    dir.to_path_buf()
}

struct Fixture {
    tmp: TempDir,
    /// The hub's `repo_root`.
    repo: PathBuf,
    /// A planted home directory (the hub's `home_override`), with vendor user-level files.
    home: PathBuf,
    state: HubState,
}

fn fixture_with(catalog: Option<&str>) -> Fixture {
    let tmp = TempDir::new().unwrap();
    // Canonical, so a comparison with what the hub reports is not about `/tmp` links.
    let base = fs::canonicalize(tmp.path()).unwrap();
    let repo = repo(&base.join("work"), catalog);
    let home = base.join("home");
    for (rel, text) in [
        (".claude.json", "{\"mcpServers\":{}}"),
        (
            ".codex/config.toml",
            "[mcp_servers.keep]\ncommand = \"x\"\n",
        ),
        (".gemini/antigravity-cli/mcp_config.json", "{}"),
        (".config/muse/settings.json", "{}"),
        (".grok/config.toml", "# user\n"),
    ] {
        let p = home.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }
    let state = hub(&base, &repo, &home);
    Fixture {
        tmp,
        repo,
        home,
        state,
    }
}

fn fixture() -> Fixture {
    fixture_with(Some(CATALOG))
}

fn hub(base: &Path, repo: &Path, home: &Path) -> HubState {
    let state = HubState::new(HubServerConfig {
        listen: "127.0.0.1:0".into(),
        store_path: base.join("store.redb"),
        repo_root: repo.to_path_buf(),
        ui_dir: None,
        owner_login: "lmcfarlin".into(),
        reviewer_apps: ReviewerApps::default(),
        owner_token: Some(OWNER.into()),
        peer_token: Some(PEER.into()),
        trust_tailscale_headers: false,
        enable_terminal: false,
        session_ttl_secs: 3600,
        home_override: Some(home.to_path_buf()),
        github: crate::github_pr::HubGithub::default(),
    });
    state.mcp.set_records_dir(base.join("state/fl/mcp"));
    state
}

fn send(
    state: &HubState,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: &str,
) -> (u16, Value) {
    let mut req = HttpRequest::new(method, path).with_body(body.as_bytes().to_vec());
    if let Some(t) = token {
        req = req.with_header("Authorization", format!("Bearer {t}"));
    }
    let resp = handle_http_request(state, &req);
    (resp.status, resp.json_value().unwrap_or(Value::Null))
}

fn get(state: &HubState) -> Value {
    let (status, body) = send(state, "GET", "/api/v1/mcp/catalog", Some(OWNER), "");
    assert_eq!(status, 200, "{body}");
    body
}

/// The digest of the first row, or 64 zeros when the row has none (it is not `synced` or `drift`).
fn digest_of(state: &HubState) -> String {
    let body = get(state);
    only_row(&body)["catalog_digest"]
        .as_str()
        .map_or_else(|| "0".repeat(64), str::to_string)
}

/// What FerroDeck does: look at the row, then sync it with the digest it saw.
fn sync(state: &HubState) -> (u16, Value) {
    let digest = digest_of(state);
    let repo = repo_name(&state.config.repo_root);
    post(
        state,
        &json!({ "repo": repo, "expect_digest": digest }).to_string(),
    )
}

fn post(state: &HubState, body: &str) -> (u16, Value) {
    send(state, "POST", "/api/v1/mcp/sync", Some(OWNER), body)
}

fn only_row(body: &Value) -> &Value {
    let rows = body["repos"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{body}");
    &rows[0]
}

/// Every file under `dir` with its bytes (a link as its target text); directories by name.
fn snapshot(dir: &Path) -> BTreeMap<String, String> {
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for e in entries {
            let p = e.unwrap().path();
            let meta = fs::symlink_metadata(&p).unwrap();
            let key = p.strip_prefix(base).unwrap().display().to_string();
            if meta.is_dir() {
                out.insert(key, "dir".into());
                walk(base, &p, out);
            } else if meta.is_symlink() {
                out.insert(key, format!("link {:?}", fs::read_link(&p).unwrap()));
            } else {
                out.insert(key, format!("{:?}", fs::read(&p).unwrap()));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

fn vendor_state(row: &Value, vendor: &str) -> String {
    row["vendors"][vendor].as_str().unwrap().to_string()
}

// --- tiers -------------------------------------------------------------------------------------

#[test]
fn both_endpoints_are_owner_only() {
    let f = fixture();
    for (method, path) in [("GET", "/api/v1/mcp/catalog"), ("POST", "/api/v1/mcp/sync")] {
        assert_eq!(send(&f.state, method, path, None, "").0, 401, "{path}");
        assert_eq!(
            send(&f.state, method, path, Some("wrong"), "").0,
            401,
            "{path}"
        );
        let (status, body) = send(&f.state, method, path, Some(PEER), "");
        assert_eq!(status, 403, "{path}");
        assert!(body["error"].as_str().unwrap().contains("owner"), "{body}");
        // The Owner is let in: the GET answers, and the POST reads a body and wants a digest.
        let want = if method == "GET" { 200 } else { 400 };
        assert_eq!(
            send(&f.state, method, path, Some(OWNER), "").0,
            want,
            "{path}"
        );
    }
}

#[test]
fn a_peer_sync_writes_nothing() {
    let f = fixture();
    let before = snapshot(f.tmp.path());
    let (status, _) = send(&f.state, "POST", "/api/v1/mcp/sync", Some(PEER), "{}");
    assert_eq!(status, 403);
    assert_eq!(snapshot(f.tmp.path()), before);
}

#[test]
fn the_wrong_method_is_a_405_for_the_owner() {
    let f = fixture();
    let (status, _) = send(&f.state, "POST", "/api/v1/mcp/catalog", Some(OWNER), "");
    assert_eq!(status, 405);
    let (status, _) = send(&f.state, "GET", "/api/v1/mcp/sync", Some(OWNER), "");
    assert_eq!(status, 405);
}

// --- the catalog rows --------------------------------------------------------------------------

#[test]
fn a_fresh_clone_is_drift_and_the_row_has_the_ui_shape() {
    let f = fixture();
    let body = get(&f.state);
    let row = only_row(&body);
    assert_eq!(row["repo"], "work");
    assert_eq!(row["status"], "drift");
    assert_eq!(
        row["catalog_path"],
        f.repo.join(".fl/mcp.toml").display().to_string()
    );
    assert_eq!(row["root"], f.repo.display().to_string());
    // The three metrics nothing measures are null, not 0 and not invented.
    for key in [
        "deduplicated_daemons",
        "active_vendor_sessions",
        "spawned_mcp_processes",
    ] {
        assert!(row.get(key).is_some_and(Value::is_null), "{key}: {row}");
    }
    // Names only, after the team defaults: `gamma` is off.
    assert_eq!(row["enabled_servers"], json!(["alpha", "beta"]));
    for vendor in ["claude", "codex", "antigravity", "muse", "grok"] {
        assert_eq!(vendor_state(row, vendor), "drift", "{vendor}");
    }
    assert!(row.get("error").is_none(), "{row}");
}

#[test]
fn a_get_writes_nothing() {
    let f = fixture();
    let before = snapshot(f.tmp.path());
    get(&f.state);
    get(&f.state);
    assert_eq!(snapshot(f.tmp.path()), before);
}

#[test]
fn a_repo_without_a_catalog_is_missing() {
    let f = fixture_with(None);
    let body = get(&f.state);
    let row = only_row(&body);
    assert_eq!(row["status"], "missing");
    assert!(row["enabled_servers"].is_null());
    assert!(row["vendors"].is_null());
    assert!(row["deduplicated_daemons"].is_null());
    let (status, body) = sync(&f.state);
    assert_eq!(status, 200);
    assert_eq!(body["ok"], true);
    let result = &body["repos"][0];
    assert_eq!(result["status_before"], "missing");
    assert_eq!(result["status_after"], "missing");
    assert_eq!(result["written"], json!([]));
    assert!(result["note"].as_str().unwrap().contains("no MCP catalog"));
    assert!(!f.repo.join(".mcp.json").exists());
}

#[test]
fn a_directory_with_no_git_and_no_catalog_is_missing() {
    let tmp = TempDir::new().unwrap();
    let base = fs::canonicalize(tmp.path()).unwrap();
    let bare = base.join("bare");
    fs::create_dir_all(&bare).unwrap();
    let state = hub(&base, &bare, &base.join("home"));
    // Under a temporary directory there is no `.git` above `bare`.
    assert_eq!(only_row(&get(&state))["status"], "missing");
    let absent = hub(&base, &base.join("absent"), &base.join("home"));
    let body = get(&absent);
    let row = only_row(&body);
    assert_eq!(row["status"], "missing");
    assert_eq!(row["repo"], "absent");
}

#[test]
fn a_malformed_catalog_is_an_error_row_that_quotes_nothing() {
    let f = fixture_with(Some(
        "[server.alpha]\ntransport = \"stdio\"\ncommand = \"x\"\nenv.A = \"CANARY-ENV-VALUE-7f3a91\"\nbogus = 1\n",
    ));
    let body = get(&f.state);
    let row = only_row(&body);
    assert_eq!(row["status"], "error");
    let error = row["error"].as_str().unwrap();
    assert!(error.contains("not a valid MCP catalog"), "{error}");
    assert!(!body.to_string().contains(CANARY_ENV), "{body}");
    assert!(row["vendors"].is_null());
    assert!(row["enabled_servers"].is_null());
    // A sync of it is refused, with the same text, and writes nothing.
    let (status, sync) = sync(&f.state);
    assert_eq!(status, 200);
    assert_eq!(sync["ok"], false);
    assert_eq!(sync["repos"][0]["status_before"], "error");
    assert!(
        sync["repos"][0]["error"]
            .as_str()
            .unwrap()
            .contains("not a valid MCP")
    );
    assert!(!f.repo.join(".mcp.json").exists());
    assert!(!sync.to_string().contains(CANARY_ENV));
}

#[test]
fn a_vendor_file_git_would_commit_is_an_error_row() {
    let f = fixture();
    fs::write(f.repo.join(".gitignore"), "/.codex/config.toml\n").unwrap();
    let body = get(&f.state);
    let row = only_row(&body);
    assert_eq!(row["status"], "error");
    assert!(
        row["error"]
            .as_str()
            .unwrap()
            .contains("not ignored by git"),
        "{row}"
    );
    let (_, sync) = sync(&f.state);
    assert_eq!(sync["ok"], false);
    assert!(!f.repo.join(".mcp.json").exists());
}

#[test]
fn a_tracked_vendor_file_is_an_error_row() {
    let f = fixture();
    fs::write(f.repo.join(".mcp.json"), "{\"mcpServers\":{}}").unwrap();
    git(&f.repo, &["add", "-f", ".mcp.json"]);
    let body = get(&f.state);
    let row = only_row(&body);
    assert_eq!(row["status"], "error");
    assert!(
        row["error"].as_str().unwrap().contains("tracked by git"),
        "{row}"
    );
}

#[test]
fn the_catalog_may_sit_above_the_hubs_repo_root() {
    let f = fixture();
    let nested = f.repo.join("crates/inner");
    fs::create_dir_all(&nested).unwrap();
    let state = hub(&fs::canonicalize(f.tmp.path()).unwrap(), &nested, &f.home);
    let body = get(&state);
    let row = only_row(&body);
    assert_eq!(row["repo"], "inner");
    assert_eq!(row["status"], "drift");
    assert_eq!(
        row["catalog_path"],
        f.repo.join(".fl/mcp.toml").display().to_string()
    );
    // A sync writes in the project directory, not in the nested one.
    let (status, sync) = sync(&state);
    assert_eq!(status, 200);
    assert_eq!(sync["ok"], true, "{sync}");
    assert!(f.repo.join(".mcp.json").is_file());
    assert!(!nested.join(".mcp.json").exists());
}

#[test]
fn the_machine_switches_decide_what_is_enabled_and_written() {
    let f = fixture();
    f.state.set_mcp_switches(|_project| {
        Ok(fl_mcp::sync::Switches {
            enable: vec!["gamma".into()],
            disable: vec!["alpha".into()],
        })
    });
    let body = get(&f.state);
    assert_eq!(only_row(&body)["enabled_servers"], json!(["beta", "gamma"]));
    let (_, sync) = sync(&f.state);
    assert_eq!(sync["ok"], true, "{sync}");
    let claude = fs::read_to_string(f.repo.join(".mcp.json")).unwrap();
    assert!(claude.contains("gamma") && claude.contains("beta") && !claude.contains("alpha"));
    // A resolver that fails is an error row, not a guess.
    f.state
        .set_mcp_switches(|_| Err("two entries disagree".into()));
    let body = get(&f.state);
    let row = only_row(&body);
    assert_eq!(row["status"], "error");
    assert_eq!(row["error"], "two entries disagree");
}

// --- sync --------------------------------------------------------------------------------------

#[test]
fn a_sync_writes_the_vendor_files_then_a_second_sync_changes_nothing() {
    let f = fixture();
    let (status, body) = sync(&f.state);
    assert_eq!(status, 200);
    assert_eq!(body["ok"], true, "{body}");
    let result = &body["repos"][0];
    assert_eq!(result["repo"], "work");
    assert_eq!(result["status_before"], "drift");
    assert_eq!(result["status_after"], "synced");
    let mut written: Vec<&str> = result["written"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    written.sort_unstable();
    let mut want = VENDOR_FILES.to_vec();
    want.sort_unstable();
    assert_eq!(written, want);
    for rel in VENDOR_FILES {
        assert!(f.repo.join(rel).is_file(), "{rel}");
    }
    assert_eq!(only_row(&get(&f.state))["status"], "synced");

    let after_first = snapshot(f.tmp.path());
    let (status, body) = sync(&f.state);
    assert_eq!(status, 200);
    assert_eq!(body["ok"], true);
    let result = &body["repos"][0];
    assert_eq!(result["status_before"], "synced");
    assert_eq!(result["status_after"], "synced");
    assert_eq!(result["written"], json!([]));
    assert_eq!(snapshot(f.tmp.path()), after_first, "a second sync wrote");
}

#[test]
fn a_sync_writes_only_the_project_files_and_leaves_the_home_directory_alone() {
    let f = fixture();
    let home_before = snapshot(&f.home);
    let tmp_before = snapshot(f.tmp.path());
    let (status, body) = sync(&f.state);
    assert_eq!(status, 200);
    assert_eq!(body["ok"], true, "{body}");
    assert_eq!(snapshot(&f.home), home_before, "the home directory changed");

    // Everything new is a vendor file in the project, or the ownership records in the state
    // directory the test gave the hub.
    let after = snapshot(f.tmp.path());
    let new: Vec<&String> = after
        .keys()
        .filter(|k| tmp_before.get(*k) != after.get(*k))
        .collect();
    let repo_rel = "work/";
    for key in new {
        let in_state = key == "state" || key.starts_with("state/");
        let in_repo_vendor = key.strip_prefix(repo_rel).is_some_and(|rel| {
            VENDOR_FILES.contains(&rel) || [".codex", ".agents", ".muse", ".grok"].contains(&rel)
        });
        assert!(in_state || in_repo_vendor, "unexpected write: {key}");
    }
    // No temporary file is left beside a vendor file.
    for key in after.keys() {
        assert!(!key.contains(".fl-mcp-"), "left behind: {key}");
    }
}

#[test]
fn a_vendor_file_is_never_a_user_level_file() {
    // The hub plans every vendor under the project and confines each target to it.
    let project = Path::new("/work/repo");
    assert_eq!(
        project_relative(project, Path::new("/work/repo/.mcp.json")),
        Some(PathBuf::from(".mcp.json"))
    );
    assert_eq!(
        project_relative(project, Path::new("/work/repo/.codex/config.toml")),
        Some(PathBuf::from(".codex/config.toml"))
    );
    for outside in [
        "/home/user/.claude.json",
        "/home/user/.codex/config.toml",
        "/work/repo/../home/.claude.json",
        "/work/repo/.mcp/../../.claude.json",
        "/work/repository/.mcp.json",
        "/work/repo",
        "relative/.mcp.json",
    ] {
        assert_eq!(
            project_relative(project, Path::new(outside)),
            None,
            "{outside}"
        );
    }
}

#[test]
fn one_vendor_can_drift_while_the_others_are_synced() {
    let f = fixture();
    sync(&f.state);
    fs::remove_file(f.repo.join(".mcp.json")).unwrap();
    let body = get(&f.state);
    let row = only_row(&body);
    assert_eq!(row["status"], "drift");
    assert_eq!(vendor_state(row, "claude"), "drift");
    for vendor in ["codex", "antigravity", "muse", "grok"] {
        assert_eq!(vendor_state(row, vendor), "synced", "{vendor}");
    }
    // The sync writes that one file.
    let (_, sync) = sync(&f.state);
    assert_eq!(sync["repos"][0]["written"], json!([".mcp.json"]));
    assert_eq!(only_row(&get(&f.state))["status"], "synced");
}

#[test]
fn a_hand_edited_entry_is_refused_and_nothing_is_written() {
    let f = fixture();
    sync(&f.state);
    let path = f.repo.join(".mcp.json");
    let mut json: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    json["mcpServers"]["alpha"]["command"] = json!("hand-edited");
    fs::write(&path, serde_json::to_string_pretty(&json).unwrap()).unwrap();
    // Something else drifts too, so a sync would otherwise write.
    fs::remove_file(f.repo.join(".grok/config.toml")).unwrap();
    let before = snapshot(&f.repo);

    let body = get(&f.state);
    let row = only_row(&body);
    assert_eq!(row["status"], "error");
    assert_eq!(vendor_state(row, "claude"), "refused");
    let refusals = row["refusals"].as_array().unwrap();
    assert_eq!(refusals.len(), 1, "{row}");
    assert!(refusals[0].as_str().unwrap().contains("alpha"));
    assert!(
        !body.to_string().contains("hand-edited"),
        "a value leaked: {body}"
    );

    let (status, sync) = sync(&f.state);
    assert_eq!(status, 200);
    assert_eq!(sync["ok"], false);
    let result = &sync["repos"][0];
    assert_eq!(result["status_before"], "error");
    assert_eq!(result["status_after"], "error");
    assert_eq!(result["written"], json!([]));
    assert!(result["error"].as_str().is_some());
    assert_eq!(snapshot(&f.repo), before, "a refused sync wrote");
}

#[test]
fn no_secret_value_or_launch_detail_is_in_any_answer() {
    let f = fixture();
    let mut seen = vec![get(&f.state).to_string()];
    seen.push(sync(&f.state).1.to_string());
    seen.push(get(&f.state).to_string());
    seen.push(sync(&f.state).1.to_string());
    let (_, unknown) = post(&f.state, "{\"repo\":\"nope\"}");
    seen.push(unknown.to_string());
    // The literals really are in play: fl writes them into the claude file.
    let claude = fs::read_to_string(f.repo.join(".mcp.json")).unwrap();
    assert!(claude.contains(CANARY_ENV) && claude.contains(CANARY_HEADER));
    for text in seen {
        for canary in [
            CANARY_ENV,
            CANARY_HEADER,
            CANARY_COMMAND,
            CANARY_ARG,
            "ALPHA_TOKEN",
            "BETA_TOKEN",
        ] {
            assert!(!text.contains(canary), "`{canary}` is in {text}");
        }
    }
}

// --- which repo --------------------------------------------------------------------------------

#[test]
fn a_known_repo_is_named_by_its_name_or_its_path() {
    let f = fixture();
    let body = json!({ "repo": "work", "expect_digest": digest_of(&f.state) });
    let (status, body) = post(&f.state, &body.to_string());
    assert_eq!((status, &body["ok"]), (200, &json!(true)), "{body}");
    assert_eq!(body["repos"][0]["status_after"], "synced");
    let by_path =
        json!({ "repo": f.repo.display().to_string(), "expect_digest": digest_of(&f.state) });
    let (status, body) = post(&f.state, &by_path.to_string());
    assert_eq!((status, &body["ok"]), (200, &json!(true)), "{body}");
    assert_eq!(body["repos"].as_array().unwrap().len(), 1);
}

#[test]
fn an_unknown_repo_is_refused_and_nothing_is_written() {
    let f = fixture();
    // Another repository on the same machine, with a catalog of its own and every gate open.
    let other = repo(
        &fs::canonicalize(f.tmp.path()).unwrap().join("other"),
        Some(CATALOG),
    );
    let before = snapshot(f.tmp.path());
    let cases: [(String, u16); 8] = [
        ("nope".into(), 404),
        ("other".into(), 404),
        (other.display().to_string(), 404),
        (format!("{}/", other.display()), 404),
        (format!("{}/../other", f.repo.display()), 404),
        ("/etc".into(), 404),
        ("../other".into(), 400),
        ("./work".into(), 400),
    ];
    for (given, want) in cases {
        let body = json!({ "repo": given, "expect_digest": "0".repeat(64) });
        let (status, body) = post(&f.state, &body.to_string());
        assert_eq!(status, want, "{given}: {body}");
        assert!(body["error"].is_string());
        assert!(body.get("repos").is_none());
        assert!(!body.to_string().contains(&given), "echoed {given}");
    }
    assert_eq!(snapshot(f.tmp.path()), before, "a refused request wrote");
    assert!(!other.join(".mcp.json").exists());
}

#[test]
fn a_bad_body_is_a_400() {
    let f = fixture();
    let zero = "0".repeat(64);
    for body in [
        "[]".to_string(),
        "not json".into(),
        format!("{{\"repo\":3,\"expect_digest\":\"{zero}\"}}"),
        format!("{{\"repo\":\"\",\"expect_digest\":\"{zero}\"}}"),
        format!("{{\"repo\":[\"work\"],\"expect_digest\":\"{zero}\"}}"),
        format!("{{\"repo\":\"work\",\"expect_digest\":\"{zero}\",\"force\":true}}"),
        format!("{{\"expect_digest\":\"{zero}\"}}"),
        format!("{{\"repo\":null,\"expect_digest\":\"{zero}\"}}"),
        "{\"repo\":\"work\",\"expect_digest\":\"short\"}".into(),
        "{\"repo\":\"work\",\"expect_digest\":7}".into(),
    ] {
        let (status, answer) = post(&f.state, &body);
        assert_eq!(status, 400, "{body}: {answer}");
    }
    assert!(!f.repo.join(".mcp.json").exists());
}

#[test]
fn a_sync_without_a_digest_is_refused_and_writes_nothing() {
    let f = fixture();
    let before = snapshot(f.tmp.path());
    // No digest, whether it names a repo, names none, or is empty: "sync everything" is gone.
    for body in [
        "",
        "{}",
        "{\"repo\":\"work\"}",
        "{\"repo\":\"work\",\"expect_digest\":null}",
    ] {
        let (status, answer) = post(&f.state, body);
        assert_eq!(status, 400, "{body}");
        let error = answer["error"].as_str().unwrap();
        assert!(error.contains("expect_digest"), "{error}");
    }
    assert_eq!(snapshot(f.tmp.path()), before);
}

#[test]
fn the_digest_follows_the_catalog_the_switches_and_the_plan() {
    let f = fixture();
    let d1 = digest_of(&f.state);
    assert_eq!(d1.len(), 64);
    assert!(d1.bytes().all(|b| b.is_ascii_hexdigit()));
    assert_eq!(digest_of(&f.state), d1, "the digest is stable");
    // A different command in the catalog is a different digest, and putting it back restores it.
    let catalog = f.repo.join(".fl/mcp.toml");
    fs::write(&catalog, CATALOG.replace(CANARY_COMMAND, "another-bin")).unwrap();
    let d2 = digest_of(&f.state);
    assert_ne!(d2, d1);
    fs::write(&catalog, CATALOG).unwrap();
    assert_eq!(digest_of(&f.state), d1);
    // So are the machine's switches...
    f.state.set_mcp_switches(|_| {
        Ok(fl_mcp::sync::Switches {
            enable: vec![],
            disable: vec!["beta".into()],
        })
    });
    let d3 = digest_of(&f.state);
    assert_ne!(d3, d1);
    // ...and the state of the vendor files (a synced repo is another digest from a drifting one).
    f.state
        .set_mcp_switches(|_| Ok(fl_mcp::sync::Switches::default()));
    assert_eq!(digest_of(&f.state), d1);
    sync(&f.state);
    let d4 = digest_of(&f.state);
    assert_ne!(d4, d1);
    // A digest is a hash: none of the catalog's text is in it.
    for text in [d1, d2, d3, d4] {
        for canary in [CANARY_ENV, CANARY_HEADER, CANARY_COMMAND, CANARY_ARG] {
            assert!(!text.contains(canary));
        }
    }
}

#[test]
fn a_digest_of_a_catalog_that_has_changed_since_is_refused_and_nothing_is_written() {
    let f = fixture();
    let looked_at = digest_of(&f.state);
    // A `git pull` changes the command of a server between the look and the click.
    fs::write(
        f.repo.join(".fl/mcp.toml"),
        CATALOG.replace(CANARY_COMMAND, "pulled-bin"),
    )
    .unwrap();
    let before = snapshot(f.tmp.path());
    let body = json!({ "repo": "work", "expect_digest": looked_at });
    let (status, answer) = post(&f.state, &body.to_string());
    assert_eq!(status, 200);
    assert_eq!(answer["ok"], false);
    let result = &answer["repos"][0];
    assert!(
        result["error"]
            .as_str()
            .unwrap()
            .contains("catalog changed since you looked")
    );
    assert_eq!(result["written"], json!([]));
    assert_eq!(snapshot(f.tmp.path()), before, "a stale sync wrote");
    assert!(!f.repo.join(".mcp.json").exists());
    // With the digest of what it now is, the sync goes through, and writes what it now says.
    let (_, answer) = sync(&f.state);
    assert_eq!(answer["ok"], true, "{answer}");
    assert!(
        fs::read_to_string(f.repo.join(".mcp.json"))
            .unwrap()
            .contains("pulled-bin")
    );
}

#[test]
fn the_row_says_what_a_click_would_do_by_name_only() {
    let f = fixture();
    let body = get(&f.state);
    let row = only_row(&body);
    let changes = row["changes"].as_array().unwrap();
    assert!(changes.contains(&json!({ "vendor": "claude", "server": "alpha", "action": "add" })));
    assert!(changes.contains(&json!({ "vendor": "claude", "server": "beta", "action": "add" })));
    assert!(
        !changes.iter().any(|c| c["server"] == "gamma"),
        "gamma is off"
    );
    for c in changes {
        let keys: Vec<&String> = c.as_object().unwrap().keys().collect();
        assert_eq!(keys.len(), 3, "{c}");
    }
    sync(&f.state);
    assert_eq!(only_row(&get(&f.state))["changes"], json!([]));
    // An edit to a server's launch is an update, and a server dropped is a remove.
    fs::write(
        f.repo.join(".fl/mcp.toml"),
        CATALOG
            .replace(CANARY_COMMAND, "other-bin")
            .replace("[server.gamma]", "[server.gamma-gone]")
            .replace("enabled = false", "enabled = false\nvendors = [\"claude\"]"),
    )
    .unwrap();
    let body = get(&f.state);
    let changes = only_row(&body)["changes"].as_array().unwrap().clone();
    assert!(
        changes.contains(&json!({ "vendor": "claude", "server": "alpha", "action": "update" }))
    );
    assert!(!body.to_string().contains("other-bin"));
}

#[test]
fn a_busy_hub_answers_busy_instead_of_waiting() {
    let f = fixture();
    f.state.mcp.set_wait(std::time::Duration::from_millis(50));
    let held = f.state.mcp.gate().expect("the gate is free");
    let (status, body) = send(&f.state, "GET", "/api/v1/mcp/catalog", Some(OWNER), "");
    assert_eq!(status, 503, "{body}");
    assert!(body["error"].as_str().unwrap().contains("busy"));
    let post_body = json!({ "repo": "work", "expect_digest": "0".repeat(64) });
    let (status, body) = post(&f.state, &post_body.to_string());
    assert_eq!(status, 200);
    assert_eq!(body["ok"], false);
    assert!(body["error"].as_str().unwrap().contains("busy"));
    drop(held);
    assert_eq!(get(&f.state)["repos"].as_array().unwrap().len(), 1);
}

#[cfg(unix)]
#[test]
fn a_repo_root_reached_through_a_link_is_synced_where_it_really_is() {
    let f = fixture();
    let base = fs::canonicalize(f.tmp.path()).unwrap();
    let link = base.join("alias");
    std::os::unix::fs::symlink(&f.repo, &link).unwrap();
    let state = hub(&base, &link, &f.home);
    let body = get(&state);
    let row = only_row(&body);
    // The repo is named by the real directory, and so is the catalog.
    assert_eq!(row["repo"], "work");
    assert_eq!(row["status"], "drift");
    assert_eq!(row["root"], f.repo.display().to_string());
    assert_eq!(
        row["catalog_path"],
        f.repo.join(".fl/mcp.toml").display().to_string()
    );
    // It answers to the link's path and to the real one.
    for given in [link.display().to_string(), f.repo.display().to_string()] {
        let body = json!({ "repo": given, "expect_digest": digest_of(&state) });
        let (status, body) = post(&state, &body.to_string());
        assert_eq!(status, 200, "{given}: {body}");
    }
    assert!(f.repo.join(".mcp.json").is_file());
    assert_eq!(repo_name(&link), "work");
}

#[cfg(unix)]
#[test]
fn a_vendor_file_that_is_a_link_is_refused_not_written_through() {
    let f = fixture();
    let outside = fs::canonicalize(f.tmp.path()).unwrap().join("outside.json");
    fs::write(&outside, "{}").unwrap();
    std::os::unix::fs::symlink(&outside, f.repo.join(".mcp.json")).unwrap();
    let body = get(&f.state);
    assert_eq!(only_row(&body)["status"], "error", "{body}");
    let (_, sync) = sync(&f.state);
    assert_eq!(sync["ok"], false);
    assert_eq!(fs::read_to_string(&outside).unwrap(), "{}");
}

// --- concurrency -------------------------------------------------------------------------------

#[test]
fn concurrent_syncs_take_turns() {
    let f = fixture();
    // Six threads each ask git a few questions; on a loaded machine the last in line may wait
    // longer than the default before it answers `busy`.
    f.state.mcp.set_wait(std::time::Duration::from_secs(120));
    let results: Vec<(u16, Value)> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..6)
            .map(|_| {
                scope.spawn(|| {
                    // A digest another thread's sync made stale is looked at again.
                    loop {
                        let answer = sync(&f.state);
                        if !answer.1.to_string().contains("catalog changed") {
                            break answer;
                        }
                    }
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    for (status, body) in &results {
        assert_eq!(*status, 200);
        assert_eq!(body["ok"], true, "{body}");
    }
    // Exactly one of them wrote; the rest found it synced.
    let writers = results
        .iter()
        .filter(|(_, b)| !b["repos"][0]["written"].as_array().unwrap().is_empty())
        .count();
    assert_eq!(writers, 1, "{results:?}");
    assert_eq!(only_row(&get(&f.state))["status"], "synced");
}

// --- gates that keep the hub inside the repository -------------------------------------------

/// A repository whose config makes git run a script on every index read, the script leaving a
/// sentinel file; returns the sentinel's path.
fn fsmonitor_trap(repo: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let sentinel = repo.join("..").join("fsmonitor-ran");
    let script = repo.join("..").join("fsmonitor.sh");
    fs::write(
        &script,
        format!("#!/bin/sh\ntouch '{}'\nexit 0\n", sentinel.display()),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    git(repo, &["add", ".gitignore"]);
    let config = repo.join(".git/config");
    let mut text = fs::read_to_string(&config).unwrap();
    text.push_str(&format!("[core]\n\tfsmonitor = {}\n", script.display()));
    fs::write(&config, text).unwrap();
    sentinel
}

#[test]
fn a_repositorys_fsmonitor_never_runs_when_the_hub_asks_git() {
    let f = fixture();
    let sentinel = fsmonitor_trap(&f.repo);
    // The control: plain git, as the CLI runs it, does run the program.
    let _ = fl_exec::git::Git::is_tracked(&f.repo, ".mcp.json");
    let _ = fl_exec::git::Git::is_ignored(&f.repo, ".mcp.json");
    assert!(
        sentinel.exists(),
        "the trap does not fire for plain git, so this test proves nothing"
    );
    fs::remove_file(&sentinel).unwrap();
    // The hub asks git for both questions on a drifting repo, and gets answers.
    let body = get(&f.state);
    assert_eq!(only_row(&body)["status"], "drift", "{body}");
    sync(&f.state);
    assert!(
        !sentinel.exists(),
        "git ran the repository's fsmonitor for the hub"
    );
}

#[test]
fn a_catalog_in_the_home_directory_or_above_it_is_never_synced() {
    let f = fixture_with(None);
    let base = fs::canonicalize(f.tmp.path()).unwrap();
    // A dotfiles repository: the hub's root is the home directory, with a catalog and a `.git`.
    git(&f.home, &["init", "-q"]);
    fs::write(f.home.join(".gitignore"), GITIGNORE).unwrap();
    fs::create_dir_all(f.home.join(".fl")).unwrap();
    fs::write(f.home.join(".fl/mcp.toml"), CATALOG).unwrap();
    // ... a repository below it that has no catalog of its own, so the one above is found ...
    let below = f.home.join("code/proj");
    fs::create_dir_all(&below).unwrap();
    let home_before = snapshot(&f.home);
    for root in [f.home.clone(), below] {
        let state = hub(&base, &root, &f.home);
        let body = get(&state);
        let row = only_row(&body);
        assert_eq!(row["status"], "error", "{root:?}: {row}");
        assert!(
            row["error"].as_str().unwrap().contains("home directory"),
            "{row}"
        );
        assert!(row["catalog_digest"].is_null());
        let sync = post(
            &state,
            &json!({ "repo": repo_name(&root), "expect_digest": "0".repeat(64) }).to_string(),
        );
        assert_eq!(sync.1["ok"], false);
        assert_eq!(
            snapshot(&f.home),
            home_before,
            "{root:?}: the home directory changed"
        );
    }
}

#[test]
fn a_catalog_above_the_repositorys_git_root_is_not_this_repositorys() {
    let f = fixture_with(None);
    let base = fs::canonicalize(f.tmp.path()).unwrap();
    // `big` holds a catalog and no `.git`; the hub's repository is a git root below it.
    let big = base.join("big");
    fs::create_dir_all(big.join(".fl")).unwrap();
    fs::write(big.join(".fl/mcp.toml"), CATALOG).unwrap();
    let proj = repo(&big.join("proj"), None);
    let before = snapshot(&big);
    let state = hub(&base, &proj, &f.home);
    let body = get(&state);
    let row = only_row(&body);
    assert_eq!(row["status"], "error", "{row}");
    assert!(
        row["error"]
            .as_str()
            .unwrap()
            .contains("not in this repository"),
        "{row}"
    );
    post(
        &state,
        &json!({ "repo": "proj", "expect_digest": "0".repeat(64) }).to_string(),
    );
    assert_eq!(snapshot(&big), before);
}

#[cfg(unix)]
#[test]
fn a_linked_dot_codex_into_the_home_directory_is_refused_and_the_home_is_untouched() {
    let f = fixture();
    std::os::unix::fs::symlink(f.home.join(".codex"), f.repo.join(".codex")).unwrap();
    let home_before = snapshot(&f.home);
    let body = get(&f.state);
    let row = only_row(&body);
    assert_eq!(row["status"], "error", "{row}");
    let (status, answer) = sync(&f.state);
    assert_eq!(status, 200);
    assert_eq!(answer["ok"], false, "{answer}");
    assert_eq!(snapshot(&f.home), home_before, "the home directory changed");
    // Nothing else was written either: a refusal writes nothing.
    assert!(!f.repo.join(".mcp.json").exists());
}

#[test]
fn a_home_the_hub_cannot_determine_closes_the_gate() {
    // Unit: no home at all, as with `$HOME` unset and no override, and a home that does not exist.
    let tmp = TempDir::new().unwrap();
    let root = fs::canonicalize(tmp.path()).unwrap();
    for home in [None, Some(root.join("no-such-home").as_path())] {
        let problem = super::project_problem(&root, &root, home).expect("refused");
        assert!(
            problem.contains("cannot determine the home directory"),
            "{problem}"
        );
    }
    assert!(super::project_problem(&root, &root, Some(&root)).is_some());
    let elsewhere = TempDir::new().unwrap();
    let other = fs::canonicalize(elsewhere.path()).unwrap();
    assert_eq!(super::project_problem(&root, &root, Some(&other)), None);
}

#[test]
fn a_hub_whose_home_does_not_exist_syncs_nothing() {
    let f = fixture();
    let base = fs::canonicalize(f.tmp.path()).unwrap();
    let state = hub(&base, &f.repo, &base.join("no-such-home"));
    let body = get(&state);
    let row = only_row(&body);
    assert_eq!(row["status"], "error", "{row}");
    assert!(
        row["error"]
            .as_str()
            .unwrap()
            .contains("cannot determine the home directory")
    );
    assert!(row["catalog_digest"].is_null());
    let before = snapshot(&f.repo);
    let (status, answer) = sync(&state);
    assert_eq!(status, 200);
    assert_eq!(answer["ok"], false, "{answer}");
    assert_eq!(snapshot(&f.repo), before);
}
