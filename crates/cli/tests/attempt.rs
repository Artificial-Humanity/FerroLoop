use assert_cmd::Command;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;
use std::path::Path;
use std::process::Command as Sys;

fn cli(db: &str) -> Command {
    let mut c = Command::cargo_bin("fl").unwrap();
    // Isolate from the developer's own
    // ~/.config/fl/config.toml, which `fl` reads unconditionally even when
    // --db confines which store it uses. `db`'s own parent directory is a
    // scratch TempDir the caller already holds, so it doubles as an empty
    // config home with no `fl/config.toml` inside it.
    let home = Path::new(db).parent().expect("db has a parent directory");
    c.env("XDG_CONFIG_HOME", home).arg("--db").arg(db);
    c
}

fn git(dir: &Path, args: &[&str]) {
    assert!(
        Sys::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap()
            .status
            .success(),
        "git {args:?} failed"
    );
}

/// A registered project, in a real git working tree.
///
/// ⚠ These tests used to register the bare string `/tmp/x`, which is not a
/// git tree and on most machines does not exist at all. That worked only
/// because `project add` stored whatever it was handed. Keep the repo alive
/// for the test's duration — dropping the TempDir deletes it.
fn project(db: &str) -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q"]);
    git(repo.path(), &["config", "user.email", "t@example.com"]);
    git(repo.path(), &["config", "user.name", "t"]);
    std::fs::write(repo.path().join("a.rs"), "fn a() {}").unwrap();
    git(repo.path(), &["add", "-A"]);
    git(repo.path(), &["commit", "-qm", "first"]);
    cli(db)
        .args(["project", "add", &repo.path().display().to_string()])
        .assert()
        .success();
    repo
}

#[test]
fn an_unknown_adapter_is_refused_and_names_the_ones_that_exist() {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("t.redb").display().to_string();
    let _repo = project(&db);
    cli(&db)
        .args(["record", "add", "--project", "1", "--title", "t"])
        .assert()
        .success();
    cli(&db)
        .args(["attempt", "1", "--adapter", "telepathy"])
        .assert()
        .failure()
        .stderr(contains("telepathy").and(contains("claude")));
}

#[test]
fn a_refused_attempt_is_still_recorded_and_shows_up_in_stats() {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("t.redb").display().to_string();
    let _repo = project(&db);
    cli(&db)
        .args(["record", "add", "--project", "1", "--title", "t"])
        .assert()
        .success();

    // A zero budget is refused by the adapter before anything is spawned.
    cli(&db)
        .args([
            "attempt",
            "1",
            "--adapter",
            "claude",
            "--budget-usd-micros",
            "0",
        ])
        .assert()
        .code(1);

    cli(&db)
        .args(["stats", "--project", "1"])
        .assert()
        .success()
        .stdout(contains("refused").and(contains("attempts: 1")));
}

#[test]
fn stats_over_a_project_with_no_attempts_says_so_rather_than_printing_nothing() {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("t.redb").display().to_string();
    let _repo = project(&db);
    cli(&db)
        .args(["stats", "--project", "1"])
        .assert()
        .success()
        .stdout(contains("attempts: 0"));
}

#[cfg(unix)]
fn write_executable(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, body).unwrap();
    let mut perms = std::fs::metadata(&tmp).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&tmp, perms).unwrap();
    std::fs::rename(&tmp, path).unwrap();
}

#[cfg(unix)]
#[test]
fn all_five_adapters_run_record_telemetry_paths_touched_and_liveness() {
    use fl_core::ids::ProjectId;
    use fl_core::log::PathsTouched;
    use fl_core::store::{Catalog, Ledger};

    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("t.redb").display().to_string();
    let repo = project(&db);
    cli(&db)
        .args([
            "record",
            "add",
            "--project",
            "1",
            "--title",
            "implement telemetry",
        ])
        .assert()
        .success();

    let stub = d.path().join("vendor-stub.sh");
    write_executable(
        &stub,
        "#!/bin/sh\n\
         echo touched >> touched.txt\n\
         printf '%s\\n' '{\"type\":\"result\",\"result\":\"ok\",\"total_cost_usd\":0.005,\"usage\":{\"input_tokens\":100,\"output_tokens\":25}}'\n",
    );
    let stub_str = stub.display().to_string();

    for vendor in ["claude", "codex", "antigravity", "muse", "grok"] {
        cli(&db)
            .env("AGENT_PERSONA", "ferris")
            .args(["attempt", "1", "--adapter", vendor, "--binary", &stub_str])
            .assert()
            .success()
            .stdout(contains("completed").and(contains("\t| ok")));
    }

    cli(&db)
        .args(["stats", "--project", "1"])
        .assert()
        .success()
        .stdout(
            contains("attempts: 5")
                .and(contains("  completed: 5"))
                .and(contains("tokens in: 500"))
                .and(contains("tokens out: 125"))
                .and(contains("cost: 25000 micro-USD"))
                .and(contains("by adapter:"))
                .and(contains("  antigravity: 1 attempts (1 completed), "))
                .and(contains("  claude: 1 attempts (1 completed), "))
                .and(contains("  codex: 1 attempts (1 completed), "))
                .and(contains("  grok: 1 attempts (1 completed), "))
                .and(contains("  muse: 1 attempts (1 completed), ")),
        );

    cli(&db)
        .args(["agent", "liveness", "1", "--json"])
        .assert()
        .success()
        .stdout(
            contains("\"status\": \"exited\"")
                .and(contains("\"outcome\": \"completed\""))
                .and(contains("\"tokens_in\": 100"))
                .and(contains("\"tokens_out\": 25"))
                .and(contains("\"cost_usd_micros\": 5000")),
        );

    cli(&db)
        .args(["agent", "liveness", "ferris"])
        .assert()
        .success()
        .stdout(
            contains("ferris\texited (completed)\tadapter=grok")
                .and(contains("tokens=100/25\tcost=5000 micro-USD")),
        );

    let store = fl_store::open_store(Path::new(&db)).unwrap();
    let projects = store.list_projects().unwrap();
    let attempts = store
        .attempts(&ProjectId(projects[0].id.iri().clone()))
        .unwrap();
    assert_eq!(attempts.len(), 5);
    for a in &attempts {
        assert_eq!(a.tokens_in, 100);
        assert_eq!(a.tokens_out, 25);
        assert_eq!(a.cost_usd_micros, 5000);
        assert_eq!(
            a.paths_touched,
            PathsTouched::Listed(vec!["touched.txt".to_string()])
        );
    }
    drop(repo);
}

#[cfg(unix)]
#[test]
fn stats_reports_partial_cost_floor_and_regret_audit_challenge() {
    use fl_core::at::At;
    use fl_core::log::GateRun;
    use fl_core::model::{
        CommandSpec, GateKind, PopulationDelivery, Regret, Selector, State, Transition,
    };
    use fl_core::store::{Catalog, Ledger};
    use fl_core::verdict::Verdict;

    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("t.redb").display().to_string();
    let _repo = project(&db);
    cli(&db)
        .args(["record", "add", "--project", "1", "--title", "partial cost"])
        .assert()
        .success();

    let paid_stub = d.path().join("paid.sh");
    write_executable(
        &paid_stub,
        "#!/bin/sh\nprintf '%s\\n' '{\"type\":\"result\",\"result\":\"paid\",\"total_cost_usd\":0.01,\"usage\":{\"input_tokens\":200,\"output_tokens\":50}}'\n",
    );
    let free_stub = d.path().join("free.sh");
    write_executable(
        &free_stub,
        "#!/bin/sh\nprintf '%s\\n' '{\"type\":\"result\",\"result\":\"free\",\"usage\":{\"input_tokens\":100,\"output_tokens\":20}}'\n",
    );

    cli(&db)
        .args([
            "attempt",
            "1",
            "--adapter",
            "claude",
            "--binary",
            &paid_stub.display().to_string(),
        ])
        .assert()
        .success();
    cli(&db)
        .args([
            "attempt",
            "1",
            "--adapter",
            "muse",
            "--binary",
            &free_stub.display().to_string(),
        ])
        .assert()
        .success();

    {
        let store = fl_store::open_store(Path::new(&db)).unwrap();
        let proj = store.list_projects().unwrap()[0].id.clone();
        let g_fast = store
            .add_gate(
                &proj,
                "fast-lint",
                GateKind::Command(CommandSpec {
                    program: "true".into(),
                    args: vec![],
                    delivery: PopulationDelivery::Args,
                    timeout_secs: 10,
                    pass_codes: vec![0],
                }),
                Selector::Glob {
                    pattern: "*.rs".into(),
                },
                1,
                "deadbeef",
                "ferris",
            )
            .unwrap();
        let g_med = store
            .add_gate(
                &proj,
                "med-test",
                GateKind::Command(CommandSpec {
                    program: "true".into(),
                    args: vec![],
                    delivery: PopulationDelivery::Args,
                    timeout_secs: 10,
                    pass_codes: vec![0],
                }),
                Selector::Glob {
                    pattern: "*.rs".into(),
                },
                1,
                "deadbeef",
                "ferris",
            )
            .unwrap();
        let g_heavy = store
            .add_gate(
                &proj,
                "heavy-e2e",
                GateKind::Command(CommandSpec {
                    program: "true".into(),
                    args: vec![],
                    delivery: PopulationDelivery::Args,
                    timeout_secs: 10,
                    pass_codes: vec![0],
                }),
                Selector::Glob {
                    pattern: "*.rs".into(),
                },
                1,
                "deadbeef",
                "ferris",
            )
            .unwrap();
        store
            .add_transition(Transition {
                project: proj.clone(),
                name: "start".into(),
                from: State::Todo,
                to: State::Doing,
                regret: Regret::Low,
                gates: vec![g_fast.clone()],
            })
            .unwrap();
        store
            .add_transition(Transition {
                project: proj.clone(),
                name: "review".into(),
                from: State::Doing,
                to: State::Review,
                regret: Regret::Low,
                gates: vec![g_med.clone()],
            })
            .unwrap();
        store
            .add_transition(Transition {
                project: proj,
                name: "ship".into(),
                from: State::Review,
                to: State::Done,
                regret: Regret::Low,
                gates: vec![g_heavy.clone()],
            })
            .unwrap();
        store
            .append_gate_run(GateRun {
                id: None,
                at: Some(At::from_unix_millis(1_000)),
                gate: g_fast,
                record: None,
                commit: "deadbeef".into(),
                verdict: Verdict::from_predicate(true, 1),
                population: 1,
                output_excerpt: None,
                duration_ms: 100,
                cost_usd_micros: 100,
            })
            .unwrap();
        store
            .append_gate_run(GateRun {
                id: None,
                at: Some(At::from_unix_millis(1_500)),
                gate: g_med,
                record: None,
                commit: "deadbeef".into(),
                verdict: Verdict::from_predicate(true, 1),
                population: 1,
                output_excerpt: None,
                duration_ms: 100,
                cost_usd_micros: 300,
            })
            .unwrap();
        store
            .append_gate_run(GateRun {
                id: None,
                at: Some(At::from_unix_millis(2_000)),
                gate: g_heavy,
                record: None,
                commit: "deadbeef".into(),
                verdict: Verdict::from_predicate(true, 1),
                population: 1,
                output_excerpt: None,
                duration_ms: 5_000,
                cost_usd_micros: 6_000,
            })
            .unwrap();
    }

    cli(&db)
        .args(["stats", "--project", "1"])
        .assert()
        .success()
        .stdout(
            contains(
                "note: 1 of 2 attempts reported no cost, so the figure above is a floor and not a total",
            )
            .and(contains(
                "regret audit: transition review -> done (ship) is declared low regret, but its measured mean (6000 micro-USD) is 20x the project median (300 micro-USD)",
            )),
        );

    cli(&db)
        .args(["stats", "--project", "1", "--json"])
        .assert()
        .success()
        .stdout(
            contains("\"zero_cost_attempts\": 1")
                .and(contains("\"tokens_in\": 300"))
                .and(contains("\"tokens_out\": 70"))
                .and(contains("\"cost_usd_micros\": 10000"))
                .and(contains("\"regret_audit\"")),
        );
}

// ---- Vendor-aware attempts: `[vendors.<id>]` and the Vault-backed key ----

mod vendors {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// A loopback server that records each request's path, `Authorization` header and body, and
    /// answers every one with an OpenAI-shaped reply that used 1000 + 200 tokens.
    struct Mock {
        base: String,
        seen: Arc<Mutex<Vec<(String, String, serde_json::Value)>>>,
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl Mock {
        fn start() -> Self {
            let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
            let base = format!("http://{}", server.server_addr().to_ip().unwrap());
            let seen = Arc::new(Mutex::new(Vec::new()));
            let stop = Arc::new(AtomicBool::new(false));
            let (seen2, stop2) = (Arc::clone(&seen), Arc::clone(&stop));
            let thread = std::thread::spawn(move || {
                while !stop2.load(Ordering::Relaxed) {
                    let Ok(Some(mut req)) = server.recv_timeout(Duration::from_millis(50)) else {
                        continue;
                    };
                    let mut text = String::new();
                    std::io::Read::read_to_string(req.as_reader(), &mut text).unwrap();
                    let auth = req
                        .headers()
                        .iter()
                        .find(|h| h.field.equiv("authorization"))
                        .map(|h| h.value.to_string())
                        .unwrap_or_default();
                    seen2.lock().unwrap().push((
                        req.url().to_string(),
                        auth,
                        serde_json::from_str(&text).unwrap_or_default(),
                    ));
                    let body = r#"{"choices":[{"message":{"role":"assistant","content":"hello from the mock"}}],"usage":{"prompt_tokens":1000,"completion_tokens":200}}"#;
                    let response = tiny_http::Response::from_string(body).with_header(
                        tiny_http::Header::from_bytes("content-type", "application/json").unwrap(),
                    );
                    let _ = req.respond(response);
                }
            });
            Self {
                base,
                seen,
                stop,
                thread: Some(thread),
            }
        }

        fn requests(&self) -> Vec<(String, String, serde_json::Value)> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl Drop for Mock {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    /// A scratch workspace: a store with one record, a config home and an empty Vault home.
    struct Ws {
        dir: tempfile::TempDir,
        db: String,
        _repo: tempfile::TempDir,
    }

    impl Ws {
        fn new(config: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let db = dir.path().join("t.redb").display().to_string();
            let repo = project(&db);
            std::fs::create_dir_all(dir.path().join("fl")).unwrap();
            std::fs::write(dir.path().join("fl/config.toml"), config).unwrap();
            let ws = Self {
                dir,
                db,
                _repo: repo,
            };
            ws.fl()
                .args(["record", "add", "--project", "1", "--title", "say hello"])
                .assert()
                .success();
            ws
        }

        /// `fl`, with the Vault confined to this workspace and no ambient key.
        fn fl(&self) -> Command {
            let mut c = cli(&self.db);
            c.env("XDG_DATA_HOME", self.dir.path().join("data"))
                .env_remove("FL_VAULT_PATH")
                .env_remove("FL_VAULT_KEY_PATH")
                .env_remove("FL_VAULT_MASTER_KEY")
                .env_remove("AGENT_PERSONA")
                .env_remove("CFG_ATT_KEY")
                .env_remove("MOONSHOT_API_KEY");
            c
        }

        /// A Vault where `fl` looks for one, holding `name` for `host`.
        fn vault_secret(&self, name: &str, value: &str, host: &str, private: bool) {
            let data = self.dir.path().join("data/fl");
            std::fs::create_dir_all(&data).unwrap();
            let key_path = self.dir.path().join("fl/vault.key");
            let master = fl_gate::load_or_init_master_key(&key_path).unwrap();
            let mut vault = fl_gate::Vault::open(data.join("vault.enc"), master).unwrap();
            vault
                .set(fl_gate::SetSecretArgs {
                    name: name.into(),
                    value: value.into(),
                    allowed_hosts: vec![host.into()],
                    allow_private_network: private,
                    ..fl_gate::SetSecretArgs::default()
                })
                .unwrap();
        }

        fn attempts(&self) -> Vec<fl_core::log::Attempt> {
            use fl_core::ids::ProjectId;
            use fl_core::store::{Catalog, Ledger};
            let store = fl_store::open_store(Path::new(&self.db)).unwrap();
            let projects = store.list_projects().unwrap();
            store
                .attempts(&ProjectId(projects[0].id.iri().clone()))
                .unwrap()
        }
    }

    fn custom_vendor(id: &str, base: &str, key: &str) -> String {
        format!(
            "[vendors.{id}]\n\
             transport = \"http_api\"\n\
             dialect = \"openai_chat_completions\"\n\
             base_url = \"{base}/v1\"\n\
             default_model = \"served-model\"\n\
             {key}\n\
             input_usd_micros_per_mtok = 2000000\n\
             output_usd_micros_per_mtok = 10000000\n"
        )
    }

    #[test]
    fn a_custom_vendor_attempt_completes_with_tokens_and_cost_recorded_under_its_id() {
        let mock = Mock::start();
        let ws = Ws::new(&custom_vendor(
            "att-custom-env",
            &mock.base,
            "env_key = \"CFG_ATT_KEY\"",
        ));
        ws.fl()
            .env("CFG_ATT_KEY", "env-secret")
            .args(["attempt", "1", "--adapter", "att-custom-env"])
            .assert()
            .success()
            .stdout(contains("completed").and(contains("hello from the mock")));
        let seen = mock.requests();
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert_eq!(seen[0].0, "/v1/chat/completions");
        assert_eq!(seen[0].1, "Bearer env-secret");
        assert_eq!(seen[0].2["model"], "served-model");
        let attempts = ws.attempts();
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].adapter, "att-custom-env");
        assert_eq!(attempts[0].tokens_in, 1000);
        assert_eq!(attempts[0].tokens_out, 200);
        // 1000 x $2/Mtok + 200 x $10/Mtok.
        assert_eq!(attempts[0].cost_usd_micros, 4000);
        ws.fl()
            .args(["stats", "--project", "1"])
            .assert()
            .success()
            .stdout(contains("  att-custom-env: 1 attempts (1 completed), "));
    }

    #[test]
    fn a_custom_vendor_attempt_without_its_key_is_refused_and_still_recorded() {
        let mock = Mock::start();
        let ws = Ws::new(&custom_vendor(
            "att-custom-nokey",
            &mock.base,
            "env_key = \"CFG_ATT_KEY\"",
        ));
        ws.fl()
            .args(["attempt", "1", "--adapter", "att-custom-nokey"])
            .assert()
            .code(1)
            .stdout(contains("refused").and(contains("no API key for `att-custom-nokey`")));
        assert!(mock.requests().is_empty(), "nothing was sent");
        let attempts = ws.attempts();
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].adapter, "att-custom-nokey");
    }

    #[test]
    fn a_custom_vendor_without_a_rate_card_is_refused_not_run() {
        let mock = Mock::start();
        let ws = Ws::new(&format!(
            "[vendors.att-custom-free-card]\ntransport = \"http_api\"\n\
             dialect = \"openai_chat_completions\"\nbase_url = \"{}/v1\"\n\
             default_model = \"m\"\nenv_key = \"CFG_ATT_KEY\"\n",
            mock.base
        ));
        ws.fl()
            .env("CFG_ATT_KEY", "env-secret")
            .args(["attempt", "1", "--adapter", "att-custom-free-card"])
            .assert()
            .code(1)
            .stdout(contains("refused").and(contains("no rate card")));
        assert!(mock.requests().is_empty());
    }

    #[test]
    fn a_keyless_local_vendor_runs_with_no_key_at_all() {
        let mock = Mock::start();
        let ws = Ws::new(&custom_vendor("att-custom-local", &mock.base, ""));
        ws.fl()
            .args(["attempt", "1", "--adapter", "ATT-custom-local"])
            .assert()
            .success()
            .stdout(contains("completed"));
        assert_eq!(mock.requests()[0].1, "", "no Authorization header");
        assert_eq!(ws.attempts()[0].adapter, "att-custom-local");
    }

    #[test]
    fn the_key_comes_from_the_vault_when_the_environment_has_none() {
        let mock = Mock::start();
        let ws = Ws::new(&custom_vendor(
            "att-custom-vault",
            &mock.base,
            "vault_secret = \"att_vault_key\"",
        ));
        // Without the secret: refused.
        ws.fl()
            .args(["attempt", "1", "--adapter", "att-custom-vault"])
            .assert()
            .code(1)
            .stdout(contains("refused"));
        // A secret that is not open to this host: refused, and the key is never sent.
        ws.vault_secret(
            "ATT_VAULT_KEY",
            "vault-secret-1",
            "api.elsewhere.example",
            true,
        );
        ws.fl()
            .args(["attempt", "1", "--adapter", "att-custom-vault"])
            .assert()
            .code(1)
            .stdout(contains("refused"));
        assert!(mock.requests().is_empty());
        // Open to the loopback host: read as `fl-exec` and sent.
        ws.vault_secret("ATT_VAULT_KEY", "vault-secret-2", "127.0.0.1", true);
        ws.fl()
            .args(["attempt", "1", "--adapter", "att-custom-vault"])
            .assert()
            .success()
            .stdout(contains("completed"));
        let seen = mock.requests();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].1, "Bearer vault-secret-2");
        let audit =
            std::fs::read_to_string(ws.dir.path().join("data/fl/gate-audit.jsonl")).unwrap();
        assert!(audit.contains("\"caller\":\"fl-exec\""), "{audit}");
        assert!(
            !audit.contains("vault-secret"),
            "the audit log holds no key"
        );
    }

    #[test]
    fn a_built_in_api_vendor_takes_its_base_url_model_and_rate_card_from_config() {
        let mock = Mock::start();
        let ws = Ws::new(&format!(
            "[vendors.kimi-api]\nbase_url = \"{}/v1\"\ndefault_model = \"kimi-override\"\n\
             input_usd_micros_per_m = 1000000\noutput_usd_micros_per_m = 3000000\n",
            mock.base
        ));
        ws.fl()
            .env("MOONSHOT_API_KEY", "moon-key")
            .args(["attempt", "1", "--adapter", "kimi-api"])
            .assert()
            .success()
            .stdout(contains("completed"));
        let seen = mock.requests();
        assert_eq!(seen[0].1, "Bearer moon-key");
        assert_eq!(seen[0].2["model"], "kimi-override");
        let attempts = ws.attempts();
        assert_eq!(attempts[0].adapter, "kimi-api");
        // 1000 x $1/Mtok + 200 x $3/Mtok.
        assert_eq!(attempts[0].cost_usd_micros, 1600);
    }

    #[test]
    fn the_five_cli_names_still_win_and_an_unknown_name_lists_the_registered_ids() {
        let mock = Mock::start();
        let ws = Ws::new(&custom_vendor("att-custom-listed", &mock.base, ""));
        ws.fl()
            .args(["attempt", "1", "--adapter", "telepathy"])
            .assert()
            .failure()
            .stderr(
                contains("telepathy")
                    .and(contains("claude"))
                    .and(contains("kimi-api"))
                    .and(contains("att-custom-listed")),
            );
        // `claude` is still the CLI adapter: it spawns the named binary.
        ws.fl()
            .args([
                "attempt",
                "1",
                "--adapter",
                "claude",
                "--binary",
                "/nonexistent/claude-binary",
            ])
            .assert()
            .code(1);
        assert!(mock.requests().is_empty());
        assert_eq!(ws.attempts()[0].adapter, "claude");
    }

    #[test]
    fn a_bad_vendor_entry_refuses_every_command_naming_the_file_and_key() {
        let ws = Ws::new("");
        std::fs::write(
            ws.dir.path().join("fl/config.toml"),
            "[vendors.att-bad]\ntransport = \"http_api\"\n",
        )
        .unwrap();
        ws.fl()
            .args(["attempt", "1", "--adapter", "claude"])
            .assert()
            .failure()
            .stderr(contains("config.toml").and(contains("[vendors.att-bad]")));
    }
}
