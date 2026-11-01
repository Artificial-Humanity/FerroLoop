use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use fl_hub::HubWsClient;
use serde_json::{Value, json};
use tempfile::TempDir;

struct TestEnv {
    _tmp: TempDir,
    root: PathBuf,
    cfg_dir: PathBuf,
    data_dir: PathBuf,
    work_dir: PathBuf,
    store_path: PathBuf,
    owner_token_file: PathBuf,
    peer_token_file: PathBuf,
}

impl TestEnv {
    fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let cfg_dir = root.join("config");
        let data_dir = root.join("data");
        let work_dir = root.join("work");
        let store_path = data_dir.join("fl").join("fl.redb");
        let owner_token_file = root.join("owner.token");
        let peer_token_file = root.join("peer.token");

        fs::create_dir_all(cfg_dir.join("fl")).unwrap();
        fs::create_dir_all(&data_dir).unwrap();
        fs::create_dir_all(work_dir.join(".agents")).unwrap();
        fs::write(work_dir.join(".agents/REGISTRY.md"), "# placeholder\n").unwrap();
        fs::write(&peer_token_file, "peer-wire-secret-token\n").unwrap();

        Self {
            _tmp: tmp,
            root,
            cfg_dir,
            data_dir,
            work_dir,
            store_path,
            owner_token_file,
            peer_token_file,
        }
    }

    fn write_config(&self) {
        let cfg_toml = format!(
            "[state]\nstore = \"{store}\"\n\n\
             [wire]\ntoken_file = \"{peer_tok}\"\n\n\
             [hub]\nlisten = \"127.0.0.1:8420\"\nowner_token_file = \"{owner_tok}\"\ntrust_tailscale_headers = true\n\n\
             [[project]]\nname = \"ferroloop\"\nroot = \"{work}\"\nstore = \"{store}\"\n",
            store = self.store_path.display(),
            peer_tok = self.peer_token_file.display(),
            owner_tok = self.owner_token_file.display(),
            work = self.work_dir.display(),
        );
        fs::write(self.cfg_dir.join("fl/config.toml"), cfg_toml).unwrap();
    }

    fn fl_in(&self, dir: &Path) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_fl"));
        c.current_dir(dir)
            .env("HOME", &self.root)
            .env("XDG_RUNTIME_DIR", &self.root)
            .env("XDG_CACHE_HOME", &self.root)
            .env("XDG_CONFIG_HOME", &self.cfg_dir)
            .env("XDG_DATA_HOME", &self.data_dir)
            .env("FL_WIRE_DISABLE_HOST_DISCOVERY", "1")
            .env_remove("FL_DB")
            .env_remove("FL_HUB_OWNER_TOKEN")
            .env_remove("FL_WIRE_TOKEN")
            .env_remove("AGENT_PERSONA")
            .env_remove("CLAUDE_CODE_MESSAGING_SOCKET")
            .env_remove("ANTIGRAVITY_CONVERSATION_ID")
            .env_remove("CODEX_THREAD_ID")
            .env_remove("MUSE_SESSION_ID")
            .env_remove("GROK_SESSION_ID")
            .env_remove("GROK_HOOK_EVENT");
        c
    }

    fn fl(&self) -> Command {
        self.fl_in(&self.work_dir)
    }
}

fn run_json(mut cmd: Command) -> Value {
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "command failed ({:?}):\nstdout: {}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

struct RunningHubGuard {
    child: Child,
}

impl Drop for RunningHubGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn fl_arch_init_show_check_and_likec4_export_end_to_end() {
    let env = TestEnv::new();
    env.write_config();

    // 1. Verify `fl arch check --json`, `fl arch show --json`, and `fl arch likec4`
    // pass on the FerroLoop repository's own `.fl/architecture.toml`.
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();

    let repo_check = run_json({
        let mut c = env.fl_in(repo_root);
        c.args(["arch", "check", "--json"]);
        c
    });
    assert_eq!(repo_check["system_id"], "ferroloop");
    assert_eq!(
        repo_check["passed"], true,
        "expected FerroLoop's .fl/architecture.toml to pass conformance check, got violations: {}",
        repo_check["violations"]
    );
    assert_eq!(repo_check["violations"].as_array().unwrap().len(), 0);

    let repo_show = run_json({
        let mut c = env.fl_in(repo_root);
        c.args(["arch", "show", "--json"]);
        c
    });
    assert_eq!(repo_show["system_id"], "ferroloop");
    assert!(repo_show["components"].as_array().unwrap().len() >= 9);

    let repo_c4_out = env
        .fl_in(repo_root)
        .args(["arch", "likec4"])
        .output()
        .unwrap();
    assert!(repo_c4_out.status.success());
    let repo_c4 = String::from_utf8_lossy(&repo_c4_out.stdout);
    assert!(repo_c4.contains("specification {"));
    assert!(repo_c4.contains("ferroloop = system 'FerroLoop'"));
    assert!(repo_c4.contains("fl_hub -> fl_core"));
    assert!(repo_c4.contains("view index of ferroloop"));

    // 2. Create a temporary multi-crate Cargo workspace in `env.work_dir` and test
    // `fl arch init`, `fl arch show`, `fl arch likec4 --output`, and `fl arch check`.
    fs::create_dir_all(env.work_dir.join("crates/alpha/src")).unwrap();
    fs::create_dir_all(env.work_dir.join("crates/beta/src")).unwrap();
    fs::write(
        env.work_dir.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/alpha\", \"crates/beta\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    fs::write(
        env.work_dir.join("crates/alpha/Cargo.toml"),
        "[package]\nname = \"alpha\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::write(
        env.work_dir.join("crates/alpha/src/lib.rs"),
        "pub fn alpha_fn() -> u32 { 1 }\n",
    )
    .unwrap();
    fs::write(
        env.work_dir.join("crates/beta/Cargo.toml"),
        "[package]\nname = \"beta\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nalpha = { path = \"../alpha\" }\n",
    )
    .unwrap();
    fs::write(
        env.work_dir.join("crates/beta/src/lib.rs"),
        "use alpha::alpha_fn;\npub fn beta_fn() -> u32 { alpha_fn() + 1 }\n",
    )
    .unwrap();

    let init_json = run_json({
        let mut c = env.fl();
        c.args(["arch", "init", "--json"]);
        c
    });
    assert!(env.work_dir.join(".fl/architecture.toml").is_file());
    assert_eq!(
        init_json["model"]["components"].as_array().unwrap().len(),
        2
    );
    assert_eq!(init_json["model"]["edges"].as_array().unwrap().len(), 1);

    let check_pass = run_json({
        let mut c = env.fl();
        c.args(["arch", "check", "--json"]);
        c
    });
    assert_eq!(check_pass["passed"], true);

    let c4_file = env.work_dir.join("landscape.c4");
    let c4_write = env
        .fl()
        .args(["arch", "likec4", "--output", c4_file.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(c4_write.status.success());
    let c4_contents = fs::read_to_string(&c4_file).unwrap();
    assert!(c4_contents.contains("beta -> alpha"));

    // 3. Introduce an undeclared dependency (`alpha -> beta`) and verify `fl arch check`
    // exits with code 1 and reports `undeclared_dependency`.
    fs::write(
        env.work_dir.join("crates/alpha/src/lib.rs"),
        "use beta::beta_fn;\npub fn alpha_fn() -> u32 { beta_fn() }\n",
    )
    .unwrap();

    let check_fail_out = env.fl().args(["arch", "check", "--json"]).output().unwrap();
    assert_eq!(
        check_fail_out.status.code(),
        Some(1),
        "expected exit code 1 on architecture violation"
    );
    let check_fail: Value = serde_json::from_slice(&check_fail_out.stdout).unwrap();
    assert_eq!(check_fail["passed"], false);
    let violations = check_fail["violations"].as_array().unwrap();
    assert_eq!(violations.len(), 1);
    assert_eq!(violations[0]["kind"], "undeclared_dependency");
    assert_eq!(violations[0]["from_component"], "alpha");
    assert_eq!(violations[0]["to_component"], "beta");
}

#[test]
fn fl_hub_token_pair_serve_status_and_rest_api_end_to_end() {
    let env = TestEnv::new();

    // 1. Initialize git repository in `work_dir` so `/api/v1/worktrees` has real git state.
    for args in [
        &["init", "-q"][..],
        &["config", "user.email", "ferris@artificialhumanity.io"][..],
        &["config", "user.name", "Ferris"][..],
    ] {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(&env.work_dir)
                .status()
                .unwrap()
                .success()
        );
    }
    fs::write(env.work_dir.join("README.md"), "# FerroLoop Workspace\n").unwrap();
    assert!(
        Command::new("git")
            .args(["add", "README.md"])
            .current_dir(&env.work_dir)
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .args(["commit", "-qm", "initial commit"])
            .current_dir(&env.work_dir)
            .status()
            .unwrap()
            .success()
    );
    // Modify README.md so the worktree has a dirty file for `/api/v1/worktrees`.
    fs::write(
        env.work_dir.join("README.md"),
        "# FerroLoop Workspace\nDirty line\n",
    )
    .unwrap();

    // 2. Generate owner token via `fl hub token --generate --output ... --json`.
    let tok_json = run_json({
        let mut c = env.fl();
        c.args([
            "hub",
            "token",
            "--generate",
            "--output",
            env.owner_token_file.to_str().unwrap(),
            "--json",
        ]);
        c
    });
    let owner_token = tok_json["token"].as_str().unwrap().to_string();
    assert_eq!(owner_token.len(), 64);
    assert_eq!(
        fs::read_to_string(&env.owner_token_file).unwrap().trim(),
        owner_token
    );

    // Write config binding `owner_token_file`, `peer_token_file`, and `store_path`.
    env.write_config();

    // 3. Initialize `.fl/architecture.toml` in `work_dir`.
    let _ = run_json({
        let mut c = env.fl();
        c.args(["arch", "init", "--json"]);
        c
    });

    // 4. Register agents (`Ferris` as lead, `Felicia` as assistant), add project, and add a workflow definition.
    for (name, role) in [("Ferris", "lead"), ("Felicia", "assistant")] {
        let out = env
            .fl()
            .args([
                "agent",
                "register",
                name,
                "--project",
                "ferroloop",
                "--role",
                role,
                "--create-area",
                "--by",
                "owner",
            ])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "register failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let proj_out = env
        .fl()
        .args(["project", "add", env.work_dir.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(proj_out.status.success());

    let flow_file = env.work_dir.join("review-flow.toml");
    fs::write(
        &flow_file,
        r#"slug = "review-flow"
title = "Review Flow"
start_step = "draft"

[[steps]]
step_id = "draft"
title = "Draft Step"
prompt = "Write implementation"
assignee = { kind = "manual" }
completion = { mode = "human_sign_off", approval_tier = "lead" }

[[edges]]
from_step = "draft"
to_step = "END"
on = "pass"
"#,
    )
    .unwrap();
    let flow_add = env
        .fl()
        .args(["flow", "add", "--file", flow_file.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        flow_add.status.success(),
        "flow add failed: {}",
        String::from_utf8_lossy(&flow_add.stderr)
    );

    // 5. Start `fl hub serve --listen 127.0.0.1:0` and read the bound address from stdout.
    let mut serve_cmd = env.fl();
    serve_cmd
        .args(["hub", "serve", "--listen", "127.0.0.1:0"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = serve_cmd.spawn().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut ready_line = String::new();
    reader.read_line(&mut ready_line).unwrap();
    let _hub_guard = RunningHubGuard { child };

    let base_url = ready_line
        .trim()
        .strip_prefix("ferroloop-hub listening on ")
        .expect("expected readiness banner from `fl hub serve`")
        .to_string();
    let addr_str = base_url.strip_prefix("http://").unwrap();

    // 6. Verify `fl hub status --url <base_url> --json`.
    let status_json = run_json({
        let mut c = env.fl();
        c.args(["hub", "status", "--url", &base_url, "--json"]);
        c
    });
    assert_eq!(status_json["reachable"], true);
    assert_eq!(status_json["health"]["status"], "ok");
    assert_eq!(status_json["authenticated"], true);
    assert_eq!(status_json["session"]["authority"]["tier"], "owner");

    // 7. Mint a one-time pairing code with `fl hub pair --url <base_url> --json`
    // while `fl hub serve` is running, then exchange it at `POST /api/v1/auth/pair`.
    let pair_json = run_json({
        let mut c = env.fl();
        c.args(["hub", "pair", "--url", &base_url, "--json"]);
        c
    });
    let pair_code = pair_json["code"].as_str().unwrap();
    assert!(
        pair_json["pair_url"]
            .as_str()
            .unwrap()
            .starts_with(&base_url)
    );

    let http: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(5)))
        .http_status_as_error(false)
        .build()
        .into();

    let pair_resp = http
        .post(&format!("{base_url}/api/v1/auth/pair"))
        .send_json(json!({ "code": pair_code }))
        .unwrap();
    assert_eq!(pair_resp.status().as_u16(), 200);
    let set_cookie = pair_resp
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let cookie_pair = set_cookie.split(';').next().unwrap().to_string();
    let pair_body: Value = pair_resp.into_body().read_json().unwrap();
    assert_eq!(pair_body["authenticated"], true);
    assert_eq!(pair_body["authority"]["tier"], "owner");

    // 8. Exercise `/api/v1/agents`, `/api/v1/worktrees`, `/api/v1/wire/send`,
    // `/api/v1/workflow-runs`, and `/api/v1/architecture`.
    let agents_resp: Value = http
        .get(&format!("{base_url}/api/v1/agents"))
        .header("Cookie", &cookie_pair)
        .call()
        .unwrap()
        .into_body()
        .read_json()
        .unwrap();
    assert_eq!(agents_resp["agents"].as_array().unwrap().len(), 2);

    let worktrees_resp: Value = http
        .get(&format!("{base_url}/api/v1/worktrees"))
        .header("Cookie", &cookie_pair)
        .call()
        .unwrap()
        .into_body()
        .read_json()
        .unwrap();
    let wts = worktrees_resp["worktrees"].as_array().unwrap();
    assert_eq!(wts.len(), 1);
    assert_eq!(wts[0]["is_clean"], false);

    let wire_send_resp = http
        .post(&format!("{base_url}/api/v1/wire/send"))
        .header("Authorization", "Bearer peer-wire-secret-token")
        .header("X-Fl-Agent", "ferris")
        .send_json(json!({
            "from": "ferris",
            "to": ["felicia"],
            "subject": "E2E Hub check",
            "body": "Testing /api/v1/wire/send"
        }))
        .unwrap();
    assert_eq!(wire_send_resp.status().as_u16(), 200);

    let start_run_resp = http
        .post(&format!("{base_url}/api/v1/workflow-runs"))
        .header("Cookie", &cookie_pair)
        .send_json(json!({
            "workflow": "review-flow",
            "title": "Hub E2E Run",
            "by": "ferris"
        }))
        .unwrap();
    assert_eq!(start_run_resp.status().as_u16(), 200);

    let runs_resp: Value = http
        .get(&format!("{base_url}/api/v1/workflow-runs"))
        .header("Cookie", &cookie_pair)
        .call()
        .unwrap()
        .into_body()
        .read_json()
        .unwrap();
    assert_eq!(runs_resp["runs"].as_array().unwrap().len(), 1);

    let arch_resp: Value = http
        .get(&format!("{base_url}/api/v1/architecture"))
        .header("Cookie", &cookie_pair)
        .call()
        .unwrap()
        .into_body()
        .read_json()
        .unwrap();
    assert_eq!(arch_resp["conformance"]["passed"], true);
    assert!(
        arch_resp["likec4_dsl"]
            .as_str()
            .unwrap()
            .contains("specification")
    );

    // 9. Connect WebSocket client to `/ws/v1` with session cookie and verify `system:connected`.
    let mut ws = HubWsClient::connect(
        &format!("ws://{addr_str}/ws/v1"),
        &[("Cookie", &cookie_pair)],
        Duration::from_secs(3),
    )
    .unwrap();
    let connected = ws.recv_frame().unwrap();
    assert_eq!(connected.channel, "system");
    assert_eq!(connected.event, "connected");
    assert_eq!(connected.data["authority"]["tier"], "owner");
    let _ = ws.close();
}

fn unix_mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn config_text(env: &TestEnv) -> String {
    fs::read_to_string(env.cfg_dir.join("fl/config.toml")).unwrap()
}

fn toml_key_count(text: &str, key: &str) -> usize {
    text.lines()
        .filter(|line| line.trim_start().starts_with(key))
        .count()
}

#[test]
fn fl_hub_init_json_writes_config_tokens_and_hub_node() {
    let env = TestEnv::new();
    let init = run_json({
        let mut c = env.fl();
        c.args(["hub", "init", "--json"]);
        c
    });
    assert!(init["service_file"].is_null());
    let node_id = init["node_id"].as_str().unwrap();
    assert!(!node_id.is_empty());
    assert_eq!(init["listen"].as_str(), Some("127.0.0.1:8420"));

    let doc: toml::Value = toml::from_str(&config_text(&env)).unwrap();
    let store = doc["state"]["store"].as_str().unwrap();
    assert!(Path::new(store).is_absolute(), "{store}");
    assert_eq!(store, init["store"].as_str().unwrap());
    assert_eq!(doc["hub"]["listen"].as_str(), Some("127.0.0.1:8420"));

    let owner = env.cfg_dir.join("fl/hub-owner.token");
    let peer = env.cfg_dir.join("fl/hub-peer.token");
    assert_eq!(init["owner_token_file"].as_str(), owner.to_str());
    assert_eq!(init["peer_token_file"].as_str(), peer.to_str());
    assert_eq!(unix_mode(&owner), 0o600);
    assert_eq!(unix_mode(&peer), 0o600);
    let owner_bytes = fs::read(&owner).unwrap();
    assert!(!owner_bytes.iter().all(u8::is_ascii_whitespace));
    assert!(!fs::read(&peer).unwrap().iter().all(u8::is_ascii_whitespace));
    assert!(
        !init
            .to_string()
            .contains(std::str::from_utf8(&owner_bytes).unwrap().trim())
    );

    let again = run_json({
        let mut c = env.fl();
        c.args(["hub", "init", "--json"]);
        c
    });
    assert_eq!(again["node_id"].as_str(), Some(node_id));
    assert_eq!(fs::read(&owner).unwrap(), owner_bytes);
    let text = config_text(&env);
    assert_eq!(toml_key_count(&text, "owner_token_file"), 1);
    assert_eq!(toml_key_count(&text, "token_file"), 1);
    assert_eq!(toml_key_count(&text, "listen"), 1);
    assert_eq!(toml_key_count(&text, "store"), 1);

    let store_db = fl_store::open_redb(Path::new(store)).unwrap();
    let hub = fl_hub::find_hub_node(&store_db).unwrap().expect("hub node");
    assert_eq!(hub.node_id, node_id);
    assert_eq!(hub.role, "hub");
}

#[test]
fn fl_hub_init_service_writes_systemd_unit() {
    let env = TestEnv::new();
    let init = run_json({
        let mut c = env.fl();
        c.args(["hub", "init", "--service", "--json"]);
        c
    });
    let service = env.cfg_dir.join("systemd/user/ferroloop-hub.service");
    assert_eq!(init["service_file"].as_str(), service.to_str());
    let text = fs::read_to_string(&service).unwrap();
    assert!(text.contains("hub serve"), "{text}");
    assert!(text.contains("ExecStart="), "{text}");
    assert!(text.contains(&env.work_dir.display().to_string()), "{text}");
}

#[test]
fn fl_hub_enroll_token_join_and_nodes_round_trip() {
    let env = TestEnv::new();
    let _init = run_json({
        let mut c = env.fl();
        c.args(["hub", "init", "--json"]);
        c
    });

    let mut serve_cmd = env.fl();
    serve_cmd
        .args(["hub", "serve", "--listen", "127.0.0.1:0"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = serve_cmd.spawn().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut ready_line = String::new();
    reader.read_line(&mut ready_line).unwrap();
    let _hub_guard = RunningHubGuard { child };

    let base_url = ready_line
        .trim()
        .strip_prefix("ferroloop-hub listening on ")
        .expect("expected readiness banner from `fl hub serve`")
        .to_string();

    let minted = run_json({
        let mut c = env.fl();
        c.args([
            "hub",
            "enroll-token",
            "--url",
            &base_url,
            "--label",
            "lab",
            "--json",
        ]);
        c
    });
    let code = minted["code"].as_str().unwrap();
    let join_command = minted["join_command"].as_str().unwrap();
    assert!(join_command.contains("fl hub join"), "{join_command}");
    assert!(join_command.contains("--token"), "{join_command}");
    assert!(join_command.contains(code), "{join_command}");
    assert!(join_command.contains("--label"), "{join_command}");

    let joined = run_json({
        let mut c = env.fl();
        c.args([
            "hub", "join", &base_url, "--token", code, "--name", "worker-a", "--json",
        ]);
        c
    });
    assert_eq!(joined["node_id"], "worker-a");
    assert_eq!(joined["hub_url"].as_str(), Some(base_url.as_str()));
    let token_file = PathBuf::from(joined["token_file"].as_str().unwrap());
    assert_eq!(token_file, env.cfg_dir.join("fl/hub-node.token"));
    assert_eq!(unix_mode(&token_file), 0o600);
    let node_token = joined["node_token"].as_str().unwrap();
    assert_eq!(
        fs::read_to_string(&token_file).unwrap(),
        format!("{node_token}\n")
    );
    let node_toml = fs::read_to_string(env.cfg_dir.join("fl/node.toml")).unwrap();
    let node_doc: toml::Value = toml::from_str(&node_toml).unwrap();
    assert_eq!(node_doc["node_id"].as_str(), Some("worker-a"));
    assert_eq!(node_doc["hub_url"].as_str(), Some(base_url.as_str()));
    assert_eq!(node_doc["token_file"].as_str(), token_file.to_str());

    let nodes = run_json({
        let mut c = env.fl();
        c.args(["hub", "nodes", "--url", &base_url, "--json"]);
        c
    });
    assert!(!nodes.to_string().contains("token_hash"));
    let listed = nodes["nodes"].as_array().unwrap();
    assert!(listed.iter().any(|node| node["role"] == "hub"));
    assert!(listed.iter().any(|node| node["node_id"] == "worker-a"));

    let second = env
        .fl()
        .args([
            "hub", "join", &base_url, "--token", code, "--name", "worker-b", "--json",
        ])
        .output()
        .unwrap();
    assert!(!second.status.success());
    let output = format!(
        "{}{}",
        String::from_utf8_lossy(&second.stdout),
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(output.contains("used"), "{output}");
}

#[test]
fn fl_hub_init_preserves_existing_project() {
    let env = TestEnv::new();
    let cfg_path = env.cfg_dir.join("fl/config.toml");
    fs::write(
        &cfg_path,
        format!(
            "[wire]\npeers = [\"ws://127.0.0.1:9\"]\n\n[[project]]\nname = \"keep-me\"\nroot = \"{}\"\nstore = \"{}\"\n",
            env.work_dir.display(),
            env.store_path.display(),
        ),
    )
    .unwrap();

    let _init = run_json({
        let mut c = env.fl();
        c.args(["hub", "init", "--json"]);
        c
    });

    let doc: toml::Value = toml::from_str(&config_text(&env)).unwrap();
    let projects = doc["project"].as_array().unwrap();
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0]["name"].as_str(), Some("keep-me"));
    assert_eq!(doc["wire"]["peers"][0].as_str(), Some("ws://127.0.0.1:9"));
}
