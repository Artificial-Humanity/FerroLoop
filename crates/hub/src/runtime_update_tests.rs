//! `POST /api/v1/runtimes/update`: the command table, the runner, the gate and the handler.
//! Every command here is a script the test wrote under a temporary home; no real CLI runs.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fl_core::pr_policy::ReviewerApps;
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::auth::HubAuthority;
use crate::runtime_matrix::{CLI_RUNTIMES, run_version};
use crate::runtime_update::{
    CommandRun, HostExec, MAX_UPDATE_OUTPUT, UPDATE_COMMANDS, UpdateExec, UpdateGate,
    post_update_with, run_command, update_command, update_runtime,
};
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

fn owner() -> HubAuthority {
    HubAuthority::Owner {
        login: "lmcfarlin".into(),
    }
}

fn post(path: &str, token: Option<&str>, body: Option<&Value>) -> HttpRequest {
    let mut req = HttpRequest::new("POST", path);
    if let Some(token) = token {
        req = req.with_header("Authorization", format!("Bearer {token}"));
    }
    match body {
        Some(body) => req.with_json_body(body),
        None => req,
    }
}

fn call(state: &HubState, token: Option<&str>, body: Option<&Value>) -> HttpResponse {
    handle_http_request(state, &post("/api/v1/runtimes/update", token, body))
}

fn json_of(resp: &HttpResponse) -> Value {
    resp.json_value().unwrap()
}

/// `<home>/.local/bin/<name>`, an executable `/bin/sh` script.
fn script(home: &Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let dir = home.join(".local").join("bin");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// A CLI that reports the version in `<home>/<name>.ver`, and whose `update` runs `on_update`
/// (which may write that file). Every call appends its argv to `<home>/<name>.args`.
fn cli(home: &Path, name: &str, version: &str, on_update: &str) -> PathBuf {
    std::fs::write(home.join(format!("{name}.ver")), version).unwrap();
    let h = home.display();
    script(
        home,
        name,
        &format!(
            r#"echo "$@" >> "{h}/{name}.args"
case "$1" in
  --version) cat "{h}/{name}.ver" ;;
  update) {on_update} ;;
  *) exit 64 ;;
esac"#
        ),
    )
}

fn args_of(home: &Path, name: &str) -> Vec<String> {
    std::fs::read_to_string(home.join(format!("{name}.args")))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

/// Real runner and version probe, with a short deadline and a retry on the one spawn error a
/// freshly written script can give: `execve` of a file some other thread's forked child still
/// holds open for writing fails with ETXTBSY.
struct FixtureExec {
    timeout: Duration,
}

impl FixtureExec {
    fn new() -> Self {
        Self {
            timeout: Duration::from_secs(20),
        }
    }
}

fn busy(err: &str) -> bool {
    err.contains("Text file busy")
}

impl UpdateExec for FixtureExec {
    fn run(&self, program: &Path, argv: &[&str]) -> CommandRun {
        for _ in 0..200 {
            match run_command(program, argv, self.timeout) {
                CommandRun::SpawnFailed(e) if busy(&e) => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                other => return other,
            }
        }
        CommandRun::SpawnFailed("Text file busy".into())
    }

    fn version(&self, program: &Path) -> Result<String, String> {
        for _ in 0..200 {
            match run_version(program, Duration::from_secs(5)) {
                Err(e) if busy(&e) => std::thread::sleep(Duration::from_millis(10)),
                other => return other,
            }
        }
        Err("Text file busy".into())
    }
}

fn update_one(home: &Path, runtime: &str, exec: &dyn UpdateExec) -> Value {
    serde_json::to_value(update_runtime(runtime, "", Some(home), exec).0).unwrap()
}

#[test]
fn the_command_table_is_fixed_and_covers_only_clis_with_an_update_subcommand() {
    let names: Vec<_> = UPDATE_COMMANDS.iter().map(|s| s.runtime).collect();
    assert_eq!(names, ["claude", "codex", "grok", "agy"]);
    assert!(UPDATE_COMMANDS.iter().all(|s| s.argv == ["update"]));
    assert!(names.iter().all(|n| CLI_RUNTIMES.contains(n)));
    assert_eq!(update_command("claude").as_deref(), Some("claude update"));
    assert_eq!(update_command("agy").as_deref(), Some("agy update"));
    // `muse` updates itself in the background and has no subcommand: it is not guessed.
    assert_eq!(update_command("muse"), None);
    assert_eq!(update_command("rm"), None);
}

#[test]
fn an_update_that_changes_the_version_is_updated_and_runs_only_the_fixed_argv() {
    let tmp = TempDir::new().unwrap();
    let h = tmp.path().display();
    cli(
        tmp.path(),
        "claude",
        "1.0.0",
        &format!("echo '2.0.0' > {h}/claude.ver; echo 'installed 2.0.0'"),
    );
    let result = update_one(tmp.path(), "claude", &FixtureExec::new());
    assert_eq!(result["runtime"], "claude");
    assert_eq!(result["ok"], true);
    assert_eq!(result["status"], "updated");
    assert_eq!(result["version_before"], "1.0.0");
    assert_eq!(result["version_after"], "2.0.0");
    assert_eq!(result["output_tail"], "installed 2.0.0");
    // Probe, update, probe: and nothing else was passed to the CLI.
    assert_eq!(
        args_of(tmp.path(), "claude"),
        ["--version", "update", "--version"]
    );
}

#[test]
fn an_update_that_leaves_the_version_is_unchanged_and_ok() {
    let tmp = TempDir::new().unwrap();
    cli(tmp.path(), "codex", "0.1.0", "echo 'already up to date'");
    let result = update_one(tmp.path(), "codex", &FixtureExec::new());
    assert_eq!(result["ok"], true);
    assert_eq!(result["status"], "unchanged");
    assert_eq!(result["version_before"], "0.1.0");
    assert_eq!(result["version_after"], "0.1.0");
    assert_eq!(result["output_tail"], "already up to date");
}

#[test]
fn a_failing_update_is_failed_with_its_output_and_does_not_claim_a_new_version() {
    let tmp = TempDir::new().unwrap();
    let h = tmp.path().display();
    // It even changes the version file before it fails: a failure is still a failure.
    cli(
        tmp.path(),
        "grok",
        "1.0.0",
        &format!("echo 9.9.9 > {h}/grok.ver; echo 'no network' >&2; exit 3"),
    );
    let result = update_one(tmp.path(), "grok", &FixtureExec::new());
    assert_eq!(result["ok"], false);
    assert_eq!(result["status"], "failed");
    assert_eq!(result["output_tail"], "no network");

    cli(tmp.path(), "agy", "1.0.0", "exit 7");
    let silent = update_one(tmp.path(), "agy", &FixtureExec::new());
    assert_eq!(silent["status"], "failed");
    assert_eq!(silent["output_tail"], "update exited with status 7");
}

#[test]
fn an_update_that_outlives_its_deadline_is_killed_with_its_children_and_is_a_timeout() {
    let tmp = TempDir::new().unwrap();
    let h = tmp.path().display();
    cli(
        tmp.path(),
        "claude",
        "1.0.0",
        &format!("echo started; sleep 60 & echo $! > {h}/child.pid; wait"),
    );
    let exec = FixtureExec {
        timeout: Duration::from_millis(600),
    };
    let began = Instant::now();
    let result = update_one(tmp.path(), "claude", &exec);
    assert!(
        began.elapsed() < Duration::from_secs(20),
        "the deadline held"
    );
    assert_eq!(result["status"], "timeout");
    assert_eq!(result["ok"], false);
    assert_eq!(result["output_tail"], "started");
    let pid: i32 = std::fs::read_to_string(tmp.path().join("child.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // The grandchild died with the group (a zombie awaiting its reaper has no cmdline).
    let gone = (0..100).any(|_| {
        std::thread::sleep(Duration::from_millis(20));
        std::fs::read(format!("/proc/{pid}/cmdline")).map_or(true, |c| c.is_empty())
    });
    assert!(gone, "the update's child {pid} outlived the timeout");
}

#[test]
fn output_keeps_only_the_end_without_escapes_and_a_chatty_updater_is_not_blocked() {
    let tmp = TempDir::new().unwrap();
    // 3000 lines is far more than a pipe holds: the updater would block if nobody read it all.
    cli(
        tmp.path(),
        "claude",
        "1.0.0",
        r#"printf '\033[31mred\033[0m\r'; i=0; while [ $i -lt 3000 ]; do echo "line $i padding padding padding padding padding padding"; i=$((i+1)); done; echo THE-END"#,
    );
    let result = update_one(tmp.path(), "claude", &FixtureExec::new());
    assert_eq!(result["status"], "unchanged");
    let tail = result["output_tail"].as_str().unwrap();
    assert!(tail.len() <= MAX_UPDATE_OUTPUT, "{} bytes", tail.len());
    assert!(
        tail.len() > MAX_UPDATE_OUTPUT / 2,
        "the tail is kept, not dropped"
    );
    assert!(tail.ends_with("THE-END"));
    assert!(!tail.contains("line 0 "), "the start was cut");
    assert!(!tail.contains('\x1b') && !tail.contains('\r'));

    // Short output keeps its start, and escapes are removed.
    let tmp = TempDir::new().unwrap();
    cli(
        tmp.path(),
        "codex",
        "1",
        r#"printf '\033[1;32mdone\033[0m\n'; echo warn >&2"#,
    );
    let result = update_one(tmp.path(), "codex", &FixtureExec::new());
    assert_eq!(result["output_tail"], "done\nwarn");
}

#[test]
fn a_multibyte_tail_is_cut_on_a_character_boundary() {
    let tmp = TempDir::new().unwrap();
    // 3000 three-byte characters: a byte cut lands inside one unless it is moved.
    cli(
        tmp.path(),
        "claude",
        "1",
        r#"i=0; while [ $i -lt 3000 ]; do printf '€'; i=$((i+1)); done"#,
    );
    let result = update_one(tmp.path(), "claude", &FixtureExec::new());
    let tail = result["output_tail"].as_str().unwrap();
    assert!(!tail.is_empty() && tail.len() <= MAX_UPDATE_OUTPUT);
    assert!(tail.chars().all(|c| c == '€'), "no replacement characters");
}

#[test]
fn a_runtime_with_no_update_command_is_unsupported_and_is_never_run() {
    let tmp = TempDir::new().unwrap();
    let h = tmp.path().display();
    script(tmp.path(), "muse", &format!("touch {h}/muse.ran"));
    let result = update_one(tmp.path(), "muse", &FixtureExec::new());
    assert_eq!(result["status"], "unsupported");
    assert_eq!(result["ok"], false);
    assert!(!tmp.path().join("muse.ran").exists());
}

#[test]
fn a_runtime_that_is_not_installed_is_not_installed() {
    let tmp = TempDir::new().unwrap();
    let result = update_one(tmp.path(), "grok", &FixtureExec::new());
    assert_eq!(result["status"], "not_installed");
    assert_eq!(result["ok"], false);
    assert_eq!(result["version_before"], Value::Null);
    assert!(result["output_tail"].as_str().unwrap().contains("grok"));
}

#[test]
fn a_command_that_cannot_be_spawned_is_reported_not_panicked() {
    let run = run_command(
        Path::new("/nonexistent/updater"),
        &["update"],
        Duration::from_secs(1),
    );
    assert!(matches!(run, CommandRun::SpawnFailed(e) if e.starts_with("spawn failed:")));
}

#[test]
fn the_update_gate_admits_one_holder_and_reopens_when_it_is_dropped() {
    let gate = UpdateGate::default();
    let first = gate.try_begin().expect("first");
    assert!(gate.try_begin().is_none());
    drop(first);
    assert!(gate.try_begin().is_some());
    // A panic while holding it releases it too.
    let gate = std::sync::Arc::new(UpdateGate::default());
    let g = gate.clone();
    let _ = std::thread::spawn(move || {
        let _held = g.try_begin().unwrap();
        panic!("boom");
    })
    .join();
    assert!(gate.try_begin().is_some());
}

// ---------------------------------------------------------------- the handler

#[test]
fn only_the_owner_may_update_and_the_body_is_checked_before_anything_runs() {
    let tmp = TempDir::new().unwrap();
    let h = tmp.path().display();
    script(tmp.path(), "claude", &format!("touch {h}/ran"));
    let state = hub(&tmp);
    let ok = json!({ "runtime": "claude" });

    assert_eq!(call(&state, None, Some(&ok)).status, 401);
    let peer = call(&state, Some(PEER), Some(&ok));
    assert_eq!(peer.status, 403, "{}", String::from_utf8_lossy(&peer.body));
    // A peer is refused before its body is even read.
    assert_eq!(
        call(&state, Some(PEER), Some(&json!({ "runtime": "bogus" }))).status,
        403
    );

    for bad in [
        json!({}),
        json!({ "runtime": "bogus" }),
        json!({ "runtime": "" }),
        json!({ "runtime": 5 }),
        json!({ "runtime": null }),
        json!({ "runtime": "claude --version" }),
        json!({ "runtime": "../claude" }),
        json!({ "runtime": ["claude"] }),
        json!({ "runtime": "claude", "args": ["--dangerous"] }),
        json!({ "runtime": "claude", "argv": "x" }),
        json!(["claude"]),
        json!("claude"),
    ] {
        let resp = call(&state, Some(OWNER), Some(&bad));
        assert_eq!(
            resp.status,
            400,
            "{bad}: {}",
            String::from_utf8_lossy(&resp.body)
        );
        assert!(json_of(&resp)["error"].is_string(), "{bad}");
    }
    // No body, and a body that is not JSON.
    assert_eq!(call(&state, Some(OWNER), None).status, 400);
    let mut raw = post("/api/v1/runtimes/update", Some(OWNER), None);
    raw.body = b"{not json".to_vec();
    assert_eq!(handle_http_request(&state, &raw).status, 400);

    assert!(
        !tmp.path().join("ran").exists(),
        "a refused request ran the CLI"
    );
    // The gate is not left held by a refusal.
    assert!(state.update_gate.try_begin().is_some());
}

#[test]
fn the_update_route_answers_unsupported_and_not_installed_without_a_fake_runner() {
    let tmp = TempDir::new().unwrap();
    let h = tmp.path().display();
    script(tmp.path(), "muse", &format!("touch {h}/muse.ran"));
    let state = hub(&tmp);

    let muse = call(&state, Some(OWNER), Some(&json!({ "runtime": "muse" })));
    assert_eq!(muse.status, 200);
    let muse = json_of(&muse);
    assert!(muse.is_object(), "one runtime answers with one result");
    assert_eq!(muse["status"], "unsupported");
    assert!(!tmp.path().join("muse.ran").exists());

    // The hub is sandboxed: the host's real claude is never found.
    let claude = json_of(&call(
        &state,
        Some(OWNER),
        Some(&json!({ "runtime": " Claude " })),
    ));
    assert_eq!(claude["runtime"], "claude");
    assert_eq!(claude["status"], "not_installed");
}

#[test]
fn a_successful_update_through_the_handler_drops_the_cached_probe() {
    let tmp = TempDir::new().unwrap();
    let h = tmp.path().display();
    cli(
        tmp.path(),
        "claude",
        "1.0.0",
        &format!("echo 2.0.0 > {h}/claude.ver"),
    );
    let state = hub(&tmp);

    let claude_version = |state: &HubState| -> Option<String> {
        // The probe can meet ETXTBSY on a fresh script: ask again, not from the cache.
        for _ in 0..100 {
            state.probe_gate.invalidate();
            let req = HttpRequest::new("GET", "/api/v1/runtimes/matrix")
                .with_header("Authorization", format!("Bearer {OWNER}"));
            let matrix = json_of(&handle_http_request(state, &req));
            let row = matrix["cli_runtimes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|c| c["name"] == "claude")
                .unwrap()
                .clone();
            if let Some(v) = row["version"].as_str() {
                return Some(v.to_string());
            }
        }
        None
    };
    assert_eq!(claude_version(&state).as_deref(), Some("1.0.0"));
    // A probe within two seconds is served from the cache: only the update can change it now.
    let req = HttpRequest::new("GET", "/api/v1/runtimes/matrix")
        .with_header("Authorization", format!("Bearer {OWNER}"));
    std::fs::write(tmp.path().join("claude.ver"), "1.0.0").unwrap();

    let resp = post_update_with(
        &state,
        &owner(),
        &post(
            "/api/v1/runtimes/update",
            None,
            Some(&json!({ "runtime": "claude" })),
        ),
        &FixtureExec::new(),
    );
    assert_eq!(resp.status, 200);
    let result = json_of(&resp);
    assert_eq!(result["status"], "updated");
    assert_eq!(result["version_before"], "1.0.0");
    assert_eq!(result["version_after"], "2.0.0");

    let matrix = json_of(&handle_http_request(&state, &req));
    let row = matrix["cli_runtimes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "claude")
        .unwrap()
        .clone();
    assert_eq!(
        row["version"], "2.0.0",
        "the next matrix read shows the new version"
    );
}

#[test]
fn all_runs_every_runtime_in_order_and_answers_with_an_array() {
    let tmp = TempDir::new().unwrap();
    let h = tmp.path().display();
    cli(
        tmp.path(),
        "claude",
        "1.0.0",
        &format!("echo 2.0.0 > {h}/claude.ver"),
    );
    cli(tmp.path(), "codex", "0.1.0", "exit 1");
    script(tmp.path(), "muse", &format!("touch {h}/muse.ran"));
    let state = hub(&tmp);

    let resp = post_update_with(
        &state,
        &owner(),
        &post(
            "/api/v1/runtimes/update",
            None,
            Some(&json!({ "runtime": "all" })),
        ),
        &FixtureExec::new(),
    );
    assert_eq!(resp.status, 200);
    let results = json_of(&resp);
    let results = results.as_array().expect("an array");
    let seen: Vec<_> = results
        .iter()
        .map(|r| {
            (
                r["runtime"].as_str().unwrap(),
                r["status"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        seen,
        [
            ("claude", "updated"),
            ("codex", "failed"),
            ("grok", "not_installed"),
            ("agy", "not_installed"),
            ("muse", "unsupported"),
        ]
    );
    assert!(!tmp.path().join("muse.ran").exists());
}

#[test]
fn a_second_update_while_one_runs_is_a_conflict_and_the_gate_reopens_after() {
    let tmp = TempDir::new().unwrap();
    let h = tmp.path().display();
    cli(
        tmp.path(),
        "claude",
        "1.0.0",
        &format!("touch {h}/started; while [ ! -e {h}/release ]; do sleep 0.05; done"),
    );
    cli(tmp.path(), "codex", "1.0.0", "true");
    let state = hub(&tmp);
    let body = json!({ "runtime": "claude" });

    std::thread::scope(|scope| {
        let first = scope.spawn(|| {
            post_update_with(
                &state,
                &owner(),
                &post("/api/v1/runtimes/update", None, Some(&body)),
                &FixtureExec::new(),
            )
        });
        let began = Instant::now();
        while !tmp.path().join("started").exists() {
            assert!(
                began.elapsed() < Duration::from_secs(20),
                "the first update never started"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        // Same runtime, another runtime and `all`: each is refused while the first runs.
        for runtime in ["claude", "codex", "all"] {
            let resp = call(&state, Some(OWNER), Some(&json!({ "runtime": runtime })));
            assert_eq!(
                resp.status,
                409,
                "{runtime}: {}",
                String::from_utf8_lossy(&resp.body)
            );
            assert!(
                json_of(&resp)["error"]
                    .as_str()
                    .unwrap()
                    .contains("already running")
            );
        }
        // The refused requests ran nothing.
        assert_eq!(args_of(tmp.path(), "codex"), Vec::<String>::new());
        std::fs::write(tmp.path().join("release"), "").unwrap();
        let first = first.join().unwrap();
        assert_eq!(first.status, 200);
        assert_eq!(json_of(&first)["status"], "unchanged");
    });

    let again = post_update_with(
        &state,
        &owner(),
        &post(
            "/api/v1/runtimes/update",
            None,
            Some(&json!({ "runtime": "codex" })),
        ),
        &FixtureExec::new(),
    );
    assert_eq!(again.status, 200);
}

#[test]
fn the_real_executor_runs_the_resolved_binary_with_its_own_timeout() {
    // `HostExec` is what the route uses: it must agree with `run_command` and `run_version`.
    let tmp = TempDir::new().unwrap();
    let path = cli(tmp.path(), "codex", "4.5.6", "echo hi");
    let exec = HostExec;
    let version = (0..100).find_map(|_| {
        exec.version(&path).ok().or_else(|| {
            std::thread::sleep(Duration::from_millis(10));
            None
        })
    });
    assert_eq!(version.as_deref(), Some("4.5.6"));
}
