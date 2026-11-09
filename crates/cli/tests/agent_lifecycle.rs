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
    assert!(
        out["prompt"]
            .as_str()
            .unwrap()
            .contains("take up residence in FerroLoop/ as the ui-developer")
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
            "muse:ada\trunning\tmuse:ada\thydration_dispatched=false",
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
             remodeled_from=codex:felicity",
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
