//! Black-box tests for `fl agent spin-up`, `decommission` and `remodel`, each in a private home,
//! store and route table, with real `tmux` spawning switched off.

use assert_cmd::Command;
use fl_core::at::At;
use fl_wire::identity::{RouteTable, VendorRoute};
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;
use serde_json::Value;
use std::path::{Path, PathBuf};

struct Env {
    root: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        Self {
            root: tempfile::tempdir().unwrap(),
        }
    }

    fn path(&self) -> &Path {
        self.root.path()
    }

    fn store(&self) -> PathBuf {
        self.path().join("fl.redb")
    }

    /// `XDG_RUNTIME_DIR` is where `fl` keeps its wire route table.
    fn routes(&self) -> PathBuf {
        self.path().join("fl-wire-routes.json")
    }

    fn fl(&self) -> Command {
        let mut c = Command::cargo_bin("fl").unwrap();
        c.current_dir(self.path())
            .env("HOME", self.path())
            .env("XDG_RUNTIME_DIR", self.path())
            .env("XDG_CONFIG_HOME", self.path().join("config"))
            .env("XDG_DATA_HOME", self.path().join("data"))
            .env("FL_WIRE_DISABLE_HOST_DISCOVERY", "1")
            .env("FL_TEST_SPINUP_NO_SPAWN", "1")
            .env_remove("FL_DB")
            .env_remove("AGENT_PERSONA")
            .env_remove("AGENT_NAME")
            .env_remove("AGENT_HYDRATION_GENERATION")
            .env_remove("FL_HOSTED_ID")
            .env_remove("CLAUDE_CODE_MESSAGING_SOCKET")
            .env_remove("CLAUDE_SESSION_ID")
            .env_remove("CODEX_THREAD_ID")
            .env_remove("MUSE_SESSION_ID")
            .env_remove("GROK_SESSION_ID")
            .env_remove("ANTIGRAVITY_CONVERSATION_ID")
            .env_remove("ANTIGRAVITY_AGENT")
            .arg("--db")
            .arg(self.store());
        c
    }

    fn spin_up(&self, persona: &str, role: &str, vendor: &str) -> Command {
        let mut c = self.fl();
        c.args(["agent", "spin-up", persona, "--project", "FerroLoop"])
            .args([
                "--role", role, "--vendor", vendor, "--by", "owner", "--json",
            ]);
        c
    }

    fn json_of(mut cmd: Command) -> Value {
        let out = cmd.assert().success().get_output().stdout.clone();
        serde_json::from_slice(&out).unwrap()
    }

    fn roster(&self, include_retired: bool) -> Vec<Value> {
        let mut c = self.fl();
        c.args(["agent", "roster", "--json"]);
        if include_retired {
            c.arg("--include-retired");
        }
        Self::json_of(c).as_array().unwrap().clone()
    }

    /// Every `hosted-session-*` runtime entry in the store, by `fl state list`.
    fn hosted_records(&self) -> Vec<Value> {
        let mut c = self.fl();
        c.args(["state", "list", "--concern", "runtime", "--json"]);
        Self::json_of(c)
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["slug"].as_str().unwrap().starts_with("hosted-session-"))
            .map(|e| e["payload"]["runtime"]["value"].clone())
            .collect()
    }

    fn write_webhook_route(&self, persona: &str) {
        let mut table = RouteTable::default();
        table.upsert(
            persona,
            VendorRoute::Webhook {
                socket_path: self.path().join("absent.sock"),
            },
            At::from_unix_millis(1),
        );
        table.save(Some(&self.routes())).unwrap();
    }
}

#[test]
fn untrusted_codex_spin_up_exits_nonzero_before_registering_or_contacting_a_host() {
    let env = Env::new();
    let config_home = env.path().join("explicit-codex");
    std::fs::create_dir(&config_home).unwrap();
    for config in [None, Some("token = \"PRIVATE_TOKEN\n")] {
        if let Some(config) = config {
            std::fs::write(config_home.join("config.toml"), config).unwrap();
        }
        let mut command = env.spin_up("ada", "assistant", "codex");
        command
            .env_remove("FL_TEST_SPINUP_NO_SPAWN")
            .env("CODEX_HOME", &config_home)
            .env("FL_HOST_SOCK", env.path().join("absent-host.sock"))
            .assert()
            .failure()
            .stderr(contains("Codex trust check"))
            .stderr(contains("trust_level = \"trusted\""))
            .stderr(contains("PRIVATE_TOKEN").not());
        assert!(env.roster(true).is_empty());
        assert!(env.hosted_records().is_empty());
    }
}

#[test]
fn spin_up_registers_the_agent_and_persists_a_hosted_record() {
    let env = Env::new();
    let out = Env::json_of(env.spin_up("felicity", "ui-developer", "claude"));
    assert_eq!(out["session_id"], "claude:felicity");
    assert_eq!(out["persona"], "felicity");
    assert_eq!(out["project"], "FerroLoop");
    assert_eq!(out["role"], "ui-developer");
    assert_eq!(out["vendor"], "claude");
    assert_eq!(out["node_id"], "ai-lab-0");
    assert_eq!(out["hosted"], true);
    assert_eq!(out["status"], "running");
    assert_eq!(out["tmux_target"], "Felicity:claude");
    assert_eq!(out["hydration_dispatched"], false);
    assert_eq!(out["hydration_status"], "failed");
    assert!(
        out["prompt"]
            .as_str()
            .unwrap()
            .contains("take up residence in the unresolved project context as the ui-developer")
    );

    let roster = env.roster(false);
    assert_eq!(roster.len(), 1);
    assert_eq!(roster[0]["name_key"], "felicity");
    assert_eq!(roster[0]["status"], "active");

    let records = env.hosted_records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["session_id"], "claude:felicity");
    assert_eq!(records[0]["restart_policy"], "on-failure");
    assert_eq!(records[0]["restart_count"], 0);
    assert_eq!(records[0]["runtime_spawned"], false);
    assert!(records[0]["decommission_prepared_at_ms"].is_null());

    env.fl()
        .args(["agent", "spin-up", "ada", "--project", "FerroLoop"])
        .args(["--role", "assistant", "--vendor", "muse", "--by", "owner"])
        .args(["--hosted", "false", "--node-id", "ai-lab-1"])
        .assert()
        .success()
        .stdout(contains(
            "muse:ada\trunning\tmuse:ada\thydration_dispatched=false\thydration_status=failed",
        ));
    let records = env.hosted_records();
    let ada = records.iter().find(|r| r["persona"] == "ada").unwrap();
    assert_eq!(ada["hosted"], false);
    assert_eq!(ada["node_id"], "ai-lab-1");
    assert_eq!(ada["runtime_spawned"], false);
}

#[test]
fn spin_up_accepts_catalog_chair_and_persists_its_custom_identity() {
    for role in ["council-chair", "custom:council-chair"] {
        let env = Env::new();
        let out = Env::json_of(env.spin_up("ada", role, "claude"));
        assert_eq!(out["role"], role);

        let roster = env.roster(false);
        assert_eq!(roster.len(), 1);
        assert_eq!(roster[0]["role"], "custom:council-chair");

        let records = env.hosted_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["role"], role);
        assert_eq!(records[0]["runtime_spawned"], false);
    }
}

#[test]
fn spin_up_rejects_each_invalid_field_without_touching_the_store() {
    let env = Env::new();
    env.spin_up("Felicity", "assistant", "claude")
        .assert()
        .code(1)
        .stderr(contains("persona must be a lowercase slug"));
    env.spin_up("fel icity", "assistant", "claude")
        .assert()
        .code(1);
    env.spin_up("felicity", "owner", "claude")
        .assert()
        .code(1)
        .stderr(contains(
            "role `owner` is not an executable role or available catalog role",
        ));
    env.spin_up("felicity", "assistant", "gemini")
        .assert()
        .code(1)
        .stderr(contains(
            "vendor must be one of: anthropic-api, antigravity, claude, codex, gemini-api, grok, \
kimi-api, lemonade, muse, ollama, openai-api, openrouter-api, xai-api",
        ));
    env.fl()
        .args(["agent", "spin-up", "felicity", "--project", " "])
        .args(["--role", "lead", "--vendor", "claude", "--by", "owner"])
        .assert()
        .code(1)
        .stderr(contains("project must not be empty"));

    assert!(env.roster(true).is_empty());
    assert!(env.hosted_records().is_empty());
}

#[test]
fn spin_up_of_a_second_lead_conflicts_and_changes_nothing() {
    let env = Env::new();
    env.spin_up("ferris", "lead", "claude").assert().success();
    env.spin_up("felix", "lead", "claude")
        .assert()
        .code(2)
        .stderr(contains("already has an active Lead"));
    assert_eq!(env.roster(true).len(), 1);
    assert_eq!(env.hosted_records().len(), 1);
}

#[test]
fn spin_up_of_an_active_agent_in_another_role_conflicts_and_changes_nothing() {
    let env = Env::new();
    env.spin_up("felicity", "assistant", "claude")
        .assert()
        .success();
    env.spin_up("felicity", "ui-developer", "claude")
        .assert()
        .code(2)
        .stderr(contains("already active"));
    assert_eq!(env.roster(true).len(), 1);
    assert_eq!(env.roster(true)[0]["role"], "assistant");
    assert_eq!(env.hosted_records().len(), 1);
}

#[test]
fn decommissioned_sessions_are_not_prepared_again() {
    let env = Env::new();
    env.spin_up("felicity", "assistant", "claude")
        .assert()
        .success();
    let decommission = |mode: &str| {
        let mut c = env.fl();
        c.args([
            "agent",
            "decommission",
            "claude:felicity",
            "--mode",
            mode,
            "--by",
            "owner",
        ]);
        c
    };
    decommission("teardown").assert().success();
    decommission("prepare")
        .assert()
        .code(2)
        .stderr(contains("is decommissioned"));
    assert_eq!(env.hosted_records()[0]["status"], "decommissioned");
    decommission("teardown").assert().success();
}

#[test]
fn decommission_rejects_a_bad_mode_and_an_unknown_session() {
    let env = Env::new();
    env.spin_up("felicity", "assistant", "claude")
        .assert()
        .success();
    env.fl()
        .args([
            "agent",
            "decommission",
            "claude:felicity",
            "--mode",
            "wipe",
            "--by",
            "owner",
        ])
        .assert()
        .code(1)
        .stderr(contains("mode must be \"prepare\" or \"teardown\""));
    env.fl()
        .args([
            "agent",
            "decommission",
            "ghost",
            "--mode",
            "prepare",
            "--by",
            "owner",
        ])
        .assert()
        .code(1)
        .stderr(contains("no hosted session or agent record for `ghost`"));
    assert_eq!(env.hosted_records()[0]["status"], "running");
    assert_eq!(env.roster(false)[0]["status"], "active");
}

#[test]
fn decommission_prepare_then_teardown_retires_the_agent_and_removes_the_route() {
    let env = Env::new();
    env.spin_up("felicity", "assistant", "claude")
        .assert()
        .success();
    env.write_webhook_route("felicity");

    let mut prepare = env.fl();
    prepare.args([
        "agent",
        "decommission",
        "claude:felicity",
        "--mode",
        "prepare",
    ]);
    prepare.args(["--by", "owner", "--json"]);
    let out = Env::json_of(prepare);
    assert_eq!(out["session_id"], "claude:felicity");
    assert_eq!(out["persona"], "felicity");
    assert_eq!(out["mode"], "prepare");
    assert_eq!(out["status"], "decommissioning");
    // The route's socket is absent, so the handshake was not delivered.
    assert_eq!(out["handshake_dispatched"], false);
    assert_eq!(out["unregistered"], false);
    assert_eq!(out["runtime_stopped"], false);
    assert!(
        out["prompt"]
            .as_str()
            .unwrap()
            .contains("fl wire read --ack")
    );
    assert_eq!(env.hosted_records()[0]["status"], "decommissioning");
    assert!(
        env.hosted_records()[0]["decommission_prepared_at_ms"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert_eq!(env.roster(false)[0]["status"], "active");
    assert!(
        RouteTable::load(Some(&env.routes()))
            .get("felicity")
            .is_some()
    );

    // The text form prints the undelivered prompt so the operator can send it.
    env.fl()
        .args([
            "agent",
            "decommission",
            "felicity",
            "--mode",
            "prepare",
            "--by",
            "owner",
        ])
        .assert()
        .success()
        .stdout(
            contains("claude:felicity\tdecommissioning\thandshake_dispatched=false")
                .and(contains("no live route took the handshake")),
        );

    let mut teardown = env.fl();
    teardown.args([
        "agent",
        "decommission",
        "claude:felicity",
        "--mode",
        "teardown",
    ]);
    teardown.args(["--by", "owner", "--json"]);
    let out = Env::json_of(teardown);
    assert_eq!(out["mode"], "teardown");
    assert_eq!(out["status"], "decommissioned");
    assert_eq!(out["handshake_dispatched"], false);
    assert_eq!(out["unregistered"], true);
    // `FL_TEST_SPINUP_NO_SPAWN`: no runtime was started, so none is reported stopped.
    assert_eq!(out["runtime_stopped"], false);
    assert!(out["prompt"].is_null());

    assert_eq!(env.hosted_records()[0]["status"], "decommissioned");
    assert!(env.roster(false).is_empty());
    let retired = env.roster(true);
    assert_eq!(retired[0]["name_key"], "felicity");
    assert_eq!(retired[0]["status"], "retired");
    assert!(
        RouteTable::load(Some(&env.routes()))
            .get("felicity")
            .is_none()
    );
}

impl Env {
    fn remodel(&self, session: &str, extra: &[&str]) -> Command {
        let mut c = self.fl();
        c.args(["agent", "remodel", session]).args(extra);
        c
    }
}

#[test]
fn remodel_execute_moves_the_persona_to_another_vendor_in_json_and_text() {
    let env = Env::new();
    let worktree = env.path().join("worktree");
    std::fs::create_dir_all(&worktree).unwrap();
    let worktree = worktree.display().to_string();
    env.spin_up("felicity", "assistant", "claude")
        .args(["--model", "claude-opus-5-5", "--cwd", &worktree])
        .assert()
        .success();

    let mut c = env.remodel("claude:felicity", &["--vendor", "codex", "--json"]);
    c.args(["--model", "gpt-6-sol", "--by", "owner"]);
    let out = Env::json_of(c);
    assert_eq!(out["mode"], "execute");
    assert_eq!(out["session_id"], "codex:felicity");
    assert_eq!(out["previous_session_id"], "claude:felicity");
    assert_eq!(out["persona"], "felicity");
    assert_eq!(out["project"], "FerroLoop");
    assert_eq!(out["role"], "assistant");
    assert_eq!(out["from_vendor"], "claude");
    assert_eq!(out["vendor"], "codex");
    assert_eq!(out["model"], "gpt-6-sol");
    assert_eq!(out["cwd"], worktree.as_str());
    assert_eq!(out["status"], "running");
    assert_eq!(out["tmux_target"], "Felicity:codex");
    assert_eq!(out["hydration_dispatched"], false);
    assert_eq!(out["hydration_status"], "failed");
    assert_eq!(out["handoff"], "remodel-felicity");

    let roster = env.roster(false);
    assert_eq!(roster.len(), 1);
    assert_eq!(roster[0]["name_key"], "felicity");
    assert_eq!(roster[0]["role"], "assistant");
    let records = env.hosted_records();
    let by_id = |id: &str| records.iter().find(|r| r["session_id"] == id).unwrap();
    assert_eq!(by_id("claude:felicity")["status"], "decommissioned");
    assert_eq!(by_id("codex:felicity")["status"], "running");
    assert_eq!(by_id("codex:felicity")["cwd"], worktree.as_str());

    // The text form, like spin-up's, names the old session too. `--by` defaults to `owner`.
    env.remodel("felicity", &["--vendor", "grok", "--node", "ai-lab-0"])
        .assert()
        .success()
        .stdout(contains(
            "grok:felicity\trunning\tFelicity:grok\thydration_dispatched=false\t\
             remodeled_from=codex:felicity\thydration_status=failed",
        ));

    // The handoff entry is in the store for the next session to read.
    let mut c = env.fl();
    c.args(["state", "list", "--concern", "handoff", "--json"]);
    let handoffs = Env::json_of(c);
    let entry = handoffs
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["slug"] == "remodel-felicity")
        .unwrap();
    assert_eq!(
        entry["payload"]["handoff"]["summary"],
        "Remodel of felicity: codex -> grok (remodeled)"
    );
}

#[test]
fn remodel_prepare_prints_the_undelivered_handshake_and_stops_nothing() {
    let env = Env::new();
    env.spin_up("felicity", "assistant", "claude")
        .assert()
        .success();
    env.write_webhook_route("felicity");

    let out = Env::json_of(env.remodel(
        "claude:felicity",
        &["--vendor", "codex", "--mode", "prepare", "--json"],
    ));
    assert_eq!(out["mode"], "prepare");
    assert_eq!(out["status"], "decommissioning");
    assert_eq!(out["session_id"], "claude:felicity");
    assert_eq!(out["handshake_dispatched"], false);
    assert!(out["prompt"].as_str().unwrap().contains("fl state handoff"));

    env.remodel("felicity", &["--vendor", "codex", "--mode", "prepare"])
        .assert()
        .success()
        .stdout(
            contains("claude:felicity\tdecommissioning\thandshake_dispatched=false")
                .and(contains("no live route took the handshake")),
        );
    assert_eq!(env.hosted_records()[0]["status"], "decommissioning");
    assert_eq!(env.roster(false)[0]["status"], "active");
}

#[test]
fn remodel_rejects_bad_fields_and_unknown_sessions_without_touching_the_store() {
    let env = Env::new();
    env.spin_up("felicity", "assistant", "claude")
        .assert()
        .success();
    env.remodel("claude:felicity", &["--vendor", "gemini"])
        .assert()
        .code(1)
        .stderr(contains(
            "vendor must be one of: anthropic-api, antigravity, claude, codex, gemini-api, grok, \
kimi-api, lemonade, muse, ollama, openai-api, openrouter-api, xai-api",
        ));
    env.remodel("claude:felicity", &["--vendor", "codex", "--mode", "wipe"])
        .assert()
        .code(1)
        .stderr(contains("mode must be \"prepare\" or \"execute\""));
    env.remodel(
        "claude:felicity",
        &["--vendor", "codex", "--cwd", "/no/such/dir"],
    )
    .assert()
    .code(1)
    .stderr(contains("cwd `/no/such/dir` is not a directory"));
    env.remodel("ghost", &["--vendor", "codex"])
        .assert()
        .code(1)
        .stderr(contains("no hosted session or agent record for `ghost`"));
    let records = env.hosted_records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["status"], "running");
    assert_eq!(env.roster(false)[0]["status"], "active");
}

#[cfg(unix)]
#[test]
fn hydration_status_codex_queue_and_ambiguous_exit_never_steer_or_resend() {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;
    for exit_code in [0, 1] {
        let env = Env::new();
        let bin = env.path().join("codex-fixture");
        let log = env.path().join("queue-inputs");
        std::fs::write(&bin, format!("#!/bin/sh\nprintf '%s\\n' \"$@\" >> \"$(dirname \"$0\")/queue-inputs\"\nexit {exit_code}\n")).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = env.path().join("control.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut routes = RouteTable::default();
        routes.upsert(
            "ada",
            VendorRoute::Codex {
                thread_id: "thread-ada".into(),
                cwd: Some(env.path().into()),
            },
            At::from_unix_millis(1),
        );
        routes.save(Some(&env.routes())).unwrap();
        let mut command = env.spin_up("ada", "assistant", "codex");
        command
            .env("FL_WIRE_CODEX_BIN", &bin)
            .env("CODEX_HOME", env.path().join("codex-home"))
            .env("FL_WIRE_CODEX_CONTROL_SOCK", &socket)
            .env("FL_HOST_SOCK", env.path().join("no-host.sock"))
            .env("FL_WIRE_TMUX_BIN", "/bin/false");
        let out = Env::json_of(command);
        assert_eq!(out["hydration_dispatched"], exit_code == 0);
        assert_eq!(out["hydration_status"], "unconfirmed");
        let input = std::fs::read_to_string(log).unwrap();
        assert_eq!(input.lines().filter(|line| *line == "queue").count(), 1);
        assert!(input.starts_with("queue\n--thread\nthread-ada\n--message\n"));
        assert!(input.contains(out["prompt"].as_str().unwrap()));
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "hydration must preserve Ordinary intent even with the owner sender label"
        );
        assert_eq!(env.hosted_records()[0]["hydration_status"], "unconfirmed");
    }
}

#[test]
fn hydration_status_legacy_record_decodes_as_unconfirmed_when_saved_again() {
    let env = Env::new();
    Env::json_of(env.spin_up("ada", "assistant", "claude"));
    let mut legacy = env.hosted_records().remove(0);
    legacy.as_object_mut().unwrap().remove("hydration_status");
    legacy["hydration_dispatched"] = true.into();
    env.fl()
        .args([
            "state",
            "runtime",
            "set",
            "--scope",
            "workspace",
            "--key",
            "hosted-session-claude-ada",
            "--value",
            &legacy.to_string(),
            "--merge",
            "last_writer",
            "--by",
            "owner",
        ])
        .assert()
        .success();
    env.fl()
        .args([
            "agent",
            "decommission",
            "claude:ada",
            "--mode",
            "prepare",
            "--by",
            "owner",
        ])
        .assert()
        .success();
    let record = env.hosted_records().remove(0);
    assert_eq!(record["hydration_dispatched"], true);
    assert_eq!(record["hydration_status"], "unconfirmed");
}

fn one_json_document(output: &[u8]) -> Value {
    let mut documents = serde_json::Deserializer::from_slice(output).into_iter::<Value>();
    let value = documents.next().expect("one JSON document").unwrap();
    assert!(
        documents.next().is_none(),
        "stdout contains another document or trailing text"
    );
    value
}

#[test]
fn hydration_guidance_warnings_keep_json_stdout_and_human_diagnostics() {
    let e = Env::new();
    let output = e
        .spin_up("ada", "assistant", "claude")
        .assert()
        .success()
        .get_output()
        .clone();
    let json = one_json_document(&output.stdout);
    let warnings = json["warnings"].as_array().unwrap();
    assert!(!warnings.is_empty());
    let json_stderr = output.stderr;
    let output = e
        .fl()
        .args([
            "agent",
            "spin-up",
            "bea",
            "--project",
            "FerroLoop",
            "--role",
            "assistant",
            "--vendor",
            "claude",
            "--by",
            "owner",
        ])
        .assert()
        .success()
        .get_output()
        .clone();
    let stderr = String::from_utf8(output.stderr).unwrap();
    let expected: Vec<_> = warnings
        .iter()
        .map(|w| format!("warning: {}", w.as_str().unwrap()))
        .collect();
    assert_eq!(stderr.lines().collect::<Vec<_>>(), expected);
    assert!(
        json_stderr.is_empty(),
        "JSON warnings belong in the response"
    );
    assert_eq!(String::from_utf8(output.stdout).unwrap().lines().count(), 1);
}

#[test]
fn hydration_remodel_warnings_are_printed_once_or_returned_as_json() {
    let e = Env::new();
    Env::json_of(e.spin_up("ada", "assistant", "claude"));
    let output = e
        .remodel("ada", &["--vendor", "claude", "--by", "owner", "--json"])
        .assert()
        .success()
        .get_output()
        .clone();
    let json = one_json_document(&output.stdout);
    let warnings = json["warnings"].as_array().unwrap();
    assert!(!warnings.is_empty());
    assert!(output.stderr.is_empty());
    let output = e
        .remodel("ada", &["--vendor", "claude", "--by", "owner"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stderr = String::from_utf8(output.stderr).unwrap();
    let expected: Vec<_> = warnings
        .iter()
        .map(|w| format!("warning: {}", w.as_str().unwrap()))
        .collect();
    assert_eq!(stderr.lines().collect::<Vec<_>>(), expected);
    assert_eq!(String::from_utf8(output.stdout).unwrap().lines().count(), 1);
}

#[test]
fn hosted_session_register_replace_with_hydration_generation_confirms_without_duplicate_prompt_and_fences_stale_generation()
 {
    let e = Env::new();
    let sock_path = e.path().join("ada-webhook.sock");
    let listener = std::os::unix::net::UnixListener::bind(&sock_path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut table = RouteTable::default();
    table.upsert(
        "ada",
        VendorRoute::Webhook {
            socket_path: sock_path,
        },
        At::from_unix_millis(1),
    );
    table.save(Some(&e.routes())).unwrap();

    // 1. Spin up `ada` with a live webhook route: prompt is delivered once, status is `unconfirmed`.
    let spun = Env::json_of(e.spin_up("ada", "assistant", "claude"));
    assert_eq!(spun["hydration_dispatched"], true);
    assert_eq!(spun["hydration_status"], "unconfirmed");
    let g1 = spun["hydration_generation"]
        .as_str()
        .expect("hydration_generation in spin-up JSON")
        .to_string();
    let prompt = spun["prompt"].as_str().unwrap();
    assert!(
        prompt.contains(&format!(
            "AGENT_PERSONA=ada AGENT_HYDRATION_GENERATION={g1} fl wire register Ada --project FerroLoop --role assistant --replace"
        )),
        "spin-up prompt must embed AGENT_HYDRATION_GENERATION={g1}: {prompt}"
    );
    assert!(
        listener.accept().is_ok(),
        "initial spin-up should deliver hydration prompt once"
    );
    assert!(
        listener.accept().is_err(),
        "no second prompt should be queued"
    );

    // 2. Run `fl wire register Ada --project FerroLoop --role assistant --replace` with AGENT_HYDRATION_GENERATION=g1.
    let mut reg = e.fl();
    reg.env("AGENT_PERSONA", "ada")
        .env("AGENT_HYDRATION_GENERATION", &g1)
        .args([
            "wire",
            "register",
            "Ada",
            "--project",
            "FerroLoop",
            "--role",
            "assistant",
            "--replace",
        ])
        .assert()
        .success();
    assert!(
        listener.accept().is_err(),
        "wire register --replace must not trigger a duplicate prompt send"
    );

    // Verify `hydration-ack-<g1>` receipt exists and `list_hosted_sessions` projects `confirmed`.
    let store = fl_store::open_store(&e.store()).unwrap();
    let session = fl_hub::lifecycle::find_hosted_session(&store, &["claude:ada"])
        .unwrap()
        .unwrap();
    assert_eq!(
        session.hydration_status,
        fl_hub::lifecycle::HydrationStatus::Confirmed
    );
    drop(store);

    // 3. Remodel `ada` to mint a newer generation `g2` (`g1 < g2`) and verify stale `g1` is refused.
    let remodeled =
        Env::json_of(e.remodel("ada", &["--vendor", "claude", "--by", "owner", "--json"]));
    let g2 = remodeled["hydration_generation"]
        .as_str()
        .expect("hydration_generation in remodel JSON")
        .to_string();
    assert!(g1 < g2, "expected {g1} < {g2}");

    let mut stale_reg = e.fl();
    stale_reg
        .env("AGENT_PERSONA", "ada")
        .env("AGENT_HYDRATION_GENERATION", &g1)
        .args([
            "wire",
            "register",
            "Ada",
            "--project",
            "FerroLoop",
            "--role",
            "assistant",
            "--replace",
        ])
        .assert()
        .failure()
        .stderr(contains(format!(
            "registration refused for `ada`: session launch generation `{g1}` was superseded by a newer launch; this process should exit"
        )));
}

#[test]
fn sonora_named_binding_spin_up_keeps_umbrella_cwd_without_warnings() {
    let e = Env::new();
    let workspace = e.path().join("workspace");
    let umbrella = workspace.join("Sonora");
    let root = umbrella.join("github");
    for dir in [
        root.join(".git"),
        workspace.join("audio-suite"),
        workspace.join("Notes"),
        workspace.join(".agents/roles"),
        e.path().join("config/fl"),
    ] {
        std::fs::create_dir_all(dir).unwrap();
    }
    for file in ["AGENTS.md", "WORKFLOW.md", "PERSONA.md"] {
        std::fs::write(root.join(file), "project guidance").unwrap();
    }
    std::fs::write(workspace.join(".agents/AGENTS.md"), "workspace").unwrap();
    std::fs::write(workspace.join(".agents/roles/assistant.md"), "role").unwrap();
    std::fs::write(
        e.path().join("config/fl/config.toml"),
        format!(
            "[[project]]\nname = 'Audio-Suite'\nroot = {:?}\nstore = {:?}\n",
            root.to_str().unwrap(),
            e.store().to_str().unwrap()
        ),
    )
    .unwrap();
    let output = e
        .fl()
        .current_dir(&umbrella)
        .args([
            "agent",
            "spin-up",
            "ada",
            "--project",
            "audio-suite",
            "--role",
            "assistant",
            "--vendor",
            "claude",
            "--by",
            "owner",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .clone();
    let response = one_json_document(&output.stdout);
    assert_eq!(response["warnings"], serde_json::json!([]), "{response}");
    assert_eq!(e.hosted_records()[0]["cwd"], umbrella.to_str().unwrap());
    assert!(
        response["prompt"]
            .as_str()
            .unwrap()
            .contains(root.join("AGENTS.md").to_str().unwrap())
    );
    assert!(output.stderr.is_empty());
}

/// Exercise the real registration command from the vendor's tool-process tree. Codex uses
/// a separate app-server process; the other tool shells descend from their hosted CLI.
#[test]
#[cfg(target_os = "linux")]
fn hydration_register_live_process_shapes_confirm_and_write_routes_for_each_vendor() {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::fs::symlink;
    use std::os::unix::process::CommandExt;
    use std::process::{Command as Process, Stdio};

    for (vendor, binary, env_key) in [
        ("claude", "claude", "CLAUDE_CODE_MESSAGING_SOCKET"),
        ("codex", "codex", "CODEX_THREAD_ID"),
        ("muse", "muse", "MUSE_SESSION_ID"),
        ("antigravity", "agy", "ANTIGRAVITY_CONVERSATION_ID"),
        ("grok", "grok", "GROK_SESSION_ID"),
    ] {
        let e = Env::new();
        let mut spin = e.spin_up("ada", "assistant", vendor);
        spin.env("CODEX_HOME", e.path().join("codex-home"));
        let spun = Env::json_of(spin);
        let generation = spun["hydration_generation"].as_str().unwrap();
        let vendor_bin = e.path().join(binary);
        symlink("/bin/sh", &vendor_bin).unwrap();
        let mut tui = (vendor == "codex").then(|| {
            Process::new(&vendor_bin)
                .args(["-c", "sleep 60"])
                .process_group(0)
                .spawn()
                .unwrap()
        });
        let script = if vendor == "claude" {
            "echo $$; read go; export CLAUDE_CODE_MESSAGING_SOCKET=$1/$$.sock; shift; \"$@\""
        } else {
            "echo $$; read go; shift; \"$@\""
        };
        let mut cmd = Process::new(&vendor_bin);
        cmd.arg("-c")
            .arg(script)
            .arg("tool")
            .arg(e.path())
            .arg(env!("CARGO_BIN_EXE_fl"))
            .args([
                "--db",
                e.store().to_str().unwrap(),
                "wire",
                "register",
                "Ada",
                "--project",
                "FerroLoop",
                "--role",
                "assistant",
                "--replace",
            ])
            .current_dir(e.path())
            .env("HOME", e.path())
            .env("XDG_RUNTIME_DIR", e.path())
            .env("XDG_CONFIG_HOME", e.path().join("config"))
            .env("XDG_DATA_HOME", e.path().join("data"))
            .env("FL_WIRE_DISABLE_HOST_DISCOVERY", "1")
            .env("AGENT_PERSONA", "ada")
            .env("AGENT_HYDRATION_GENERATION", generation)
            .env_remove("FL_HOSTED_ID")
            .env_remove("CLAUDE_CODE_MESSAGING_SOCKET")
            .env_remove("CLAUDE_SESSION_ID")
            .env_remove("CODEX_THREAD_ID")
            .env_remove("MUSE_SESSION_ID")
            .env_remove("GROK_SESSION_ID")
            .env_remove("ANTIGRAVITY_CONVERSATION_ID")
            .env_remove("ANTIGRAVITY_AGENT")
            .env_remove("AGENT_NAME")
            .env_remove("FL_DB")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        if vendor != "claude" {
            cmd.env(env_key, "live-session-id");
        }
        let mut tool = cmd.spawn().unwrap();
        let mut stdout = BufReader::new(tool.stdout.take().unwrap());
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        let app_pid: u32 = line.trim().parse().unwrap();
        let hosted_pid = tui.as_ref().map_or(app_pid, |p| p.id());
        {
            let store = fl_store::open_store(&e.store()).unwrap();
            let id = format!("{vendor}:ada");
            let mut record = fl_hub::lifecycle::find_hosted_session(&store, &[&id])
                .unwrap()
                .unwrap();
            record.pid = Some(hosted_pid);
            record.pid_start_time = fl_wire::identity::proc_start_time_ticks(hosted_pid);
            record.runtime_leader_pid = Some(hosted_pid);
            fl_hub::lifecycle::persist_hosted_session(&store, &record, "test").unwrap();
        }
        writeln!(tool.stdin.take().unwrap(), "go").unwrap();
        let status = tool.wait().unwrap();
        if let Some(ref mut tui) = tui {
            let _ = tui.kill();
            let _ = tui.wait();
        }
        assert!(status.success(), "{vendor}: registration failed: {status}");
        let store = fl_store::open_store(&e.store()).unwrap();
        let record = fl_hub::lifecycle::find_hosted_session(&store, &[&format!("{vendor}:ada")])
            .unwrap()
            .unwrap();
        assert_eq!(
            record.hydration_status,
            fl_hub::lifecycle::HydrationStatus::Confirmed,
            "{vendor}"
        );
        let table = RouteTable::load(Some(&e.routes()));
        assert_eq!(table.get("ada").unwrap().route.canonical_vendor(), vendor);
    }
}

#[test]
fn area_set_preserves_nested_competition_path() {
    let env = Env::new();
    env.fl()
        .args([
            "agent",
            "area",
            "set",
            "competition",
            "--path",
            "Kaggle/Competition/",
            "--purpose",
            "test",
            "--by",
            "owner",
        ])
        .assert()
        .success();
    let mut cmd = env.fl();
    cmd.args(["agent", "area", "list", "--json"]);
    let areas = Env::json_of(cmd);
    let area = areas
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["area_key"] == "competition")
        .unwrap();
    assert_eq!(area["path"], "Kaggle/Competition/");
}
