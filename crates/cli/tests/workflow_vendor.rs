//! A workflow step may run any registered vendor, not only the five CLI adapters: the step's
//! `adapter_used` and its attempt are the vendor id, and the five CLIs behave as before.

use fl_core::agent::{AgentRole, RegisterAgentArgs};
use fl_core::at::At;
use fl_core::ids::{ProjectId, WorkflowId};
use fl_core::log::AttemptStatus;
use fl_core::store::{AgentRegistry, Catalog, Ledger, WorkflowStore};
use fl_core::vendor::{ApiDialect, VendorDescriptor, VendorTransport};
use fl_core::workflow::{
    AssigneeRule, END_STEP, EdgeCondition, EdgeDef, JoinPolicy, StepCompletion, StepDef,
    StepOutcome, WorkflowDef,
};
use fl_exec::adapters::AdapterBuild;
use fl_exec::runner::AttemptError;
use fl_exec::workflow::{StepBuildHook, execute_workflow_step_with};
use fl_exec::{
    Git, StepExecutionAction, StepExecutionOptions, WorkflowEnv, WorkflowExecError,
    execute_workflow_step, stamp, start_workflow_run,
};
use fl_store::RedbStore;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A loopback server that records each request's `Authorization` header and answers with an
/// OpenAI-shaped reply that used 1000 + 200 tokens.
struct Mock {
    base: String,
    seen: Arc<Mutex<Vec<String>>>,
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
                seen2.lock().unwrap().push(auth);
                let body = r#"{"choices":[{"message":{"role":"assistant","content":"step done"}}],"usage":{"prompt_tokens":1000,"completion_tokens":200}}"#;
                let _ = req.respond(tiny_http::Response::from_string(body).with_header(
                    tiny_http::Header::from_bytes("content-type", "application/json").unwrap(),
                ));
            }
        });
        Self {
            base,
            seen,
            stop,
            thread: Some(thread),
        }
    }

    fn requests(&self) -> Vec<String> {
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

fn git_repo() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(d.path())
                .status()
                .unwrap()
                .success()
        );
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@example.com"]);
    git(&["config", "user.name", "t"]);
    fs::write(d.path().join("a.txt"), "a\n").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-qm", "init"]);
    d
}

fn vendor(id: &str, base: &str, key: Option<&str>) {
    fl_core::ensure_vendor_registered(VendorDescriptor {
        id: id.into(),
        display_name: id.into(),
        transport: VendorTransport::HttpApi {
            default_base_url: format!("{base}/v1"),
            api_dialect: ApiDialect::OpenAiChatCompletions,
            vault_secret_name: key.map(str::to_string),
            env_key_name: key.map(str::to_string),
        },
        default_model: "m".into(),
        default_context_window: 0,
        default_rate_card: Default::default(),
        mcp_config_rel_path: None,
    })
    .unwrap();
}

struct Fixture {
    _repo: tempfile::TempDir,
    _db: tempfile::TempDir,
    store: RedbStore,
    project: ProjectId,
    head: String,
    root: PathBuf,
}

fn fixture() -> Fixture {
    let repo = git_repo();
    let db = tempfile::tempdir().unwrap();
    let store = RedbStore::open(&db.path().join("fl.redb")).unwrap();
    let project = store
        .add_project(&repo.path().display().to_string())
        .unwrap();
    store
        .register_agent(RegisterAgentArgs {
            display_name: "Felicia".into(),
            email: None,
            project: "ferroloop".into(),
            role: AgentRole::Assistant,
            extra_areas: vec![],
            create_area: true,
            replace: false,
            at: At::from_unix_millis(11),
            by: "owner".into(),
        })
        .unwrap();
    let head = Git::head(repo.path()).unwrap();
    let root = repo.path().to_path_buf();
    Fixture {
        _repo: repo,
        _db: db,
        store,
        project,
        head,
        root,
    }
}

fn one_step_run(fx: &Fixture) -> fl_core::workflow::WorkflowRun {
    let step = StepDef {
        step_id: "only".into(),
        title: "only".into(),
        prompt: "Say hello.".into(),
        assignee: AssigneeRule::Agent {
            name: "felicia".into(),
        },
        reviews_step: None,
        require_different_vendor: false,
        completion: StepCompletion::Ungated,
        join_policy: JoinPolicy::All,
        max_attempts: 1,
        max_visits: 3,
        budget_usd_micros: None,
        max_tokens: None,
        isolate_worktree: false,
    };
    let def = fx
        .store
        .upsert_workflow_def(WorkflowDef {
            id: WorkflowId(stamp::entry_id()),
            project: fx.project.clone(),
            slug: "vendor-flow".into(),
            title: "Vendor flow".into(),
            start_step: "only".into(),
            steps: vec![step],
            edges: vec![EdgeDef {
                from_step: "only".into(),
                to_step: END_STEP.into(),
                on: EdgeCondition::Pass,
            }],
            max_total_steps: 4,
            budget_usd_micros: None,
            max_tokens: None,
            declared_keys: BTreeMap::new(),
            authored_at_commit: fx.head.clone(),
            authored_by: "ferris".into(),
        })
        .unwrap();
    start_workflow_run(
        &fx.store,
        &def,
        "vendor run",
        At::from_unix_millis(20),
        "ferris",
    )
    .unwrap()
}

fn options(adapter: &str) -> StepExecutionOptions {
    StepExecutionOptions {
        adapter_override: Some(adapter.into()),
        by: "felicia".into(),
        ..Default::default()
    }
}

fn priced(key: Option<&'static str>) -> impl Fn(&str) -> AdapterBuild + Send + Sync {
    move |_| AdapterBuild {
        rate_card: Some(fl_core::telemetry::RateCard {
            input_usd_micros_per_mtok: 2_000_000,
            output_usd_micros_per_mtok: 10_000_000,
        }),
        api_key: key.map(|k| -> fl_exec::adapters::ApiKeyResolver {
            Arc::new(move |_| Some(k.to_string()))
        }),
        ..AdapterBuild::default()
    }
}

#[test]
fn a_step_runs_a_registered_http_vendor_and_records_its_id_tokens_and_cost() {
    let mock = Mock::start();
    vendor("wf-vendor-ok", &mock.base, Some("WF_VENDOR_OK_KEY"));
    let fx = fixture();
    let run = one_step_run(&fx);
    let hook = priced(Some("hook-key"));
    let report = execute_workflow_step_with(
        WorkflowEnv::single(&fx.store),
        &run.id,
        &options("WF-Vendor-OK"),
        Some(&hook as &StepBuildHook<'_>),
    )
    .unwrap();
    match &report.action {
        StepExecutionAction::Executed {
            adapter, outcome, ..
        } => {
            assert_eq!(adapter, "wf-vendor-ok");
            assert_eq!(*outcome, StepOutcome::Pass);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        report.run.steps["only"].adapter_used.as_deref(),
        Some("wf-vendor-ok")
    );
    assert_eq!(report.run.total_tokens, 1200);
    assert_eq!(report.run.total_cost_usd_micros, 4000);
    assert_eq!(mock.requests(), vec!["Bearer hook-key".to_string()]);
    let attempts = fx.store.attempts(&fx.project).unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].adapter, "wf-vendor-ok");
    assert_eq!(attempts[0].status, AttemptStatus::Completed);
    assert_eq!((attempts[0].tokens_in, attempts[0].tokens_out), (1000, 200));
    assert_eq!(attempts[0].cost_usd_micros, 4000);
}

#[test]
fn a_step_with_no_key_or_no_rate_card_is_refused_and_the_step_fails() {
    let mock = Mock::start();
    vendor("wf-vendor-nokey", &mock.base, Some("WF_VENDOR_NOKEY_KEY"));
    let fx = fixture();

    // No key.
    let run = one_step_run(&fx);
    let no_key = priced(None);
    let report = execute_workflow_step_with(
        WorkflowEnv::single(&fx.store),
        &run.id,
        &options("wf-vendor-nokey"),
        Some(&no_key as &StepBuildHook<'_>),
    )
    .unwrap();
    assert!(
        matches!(
            report.action,
            StepExecutionAction::Executed {
                outcome: StepOutcome::Fail,
                ..
            }
        ),
        "{:?}",
        report.action
    );
    let attempts = fx.store.attempts(&fx.project).unwrap();
    assert_eq!(attempts[0].status, AttemptStatus::Refused);
    assert_eq!(attempts[0].adapter, "wf-vendor-nokey");

    // A key but no rate card: the plain entry point has no hook, so the environment holds the key
    // (it does not here) and the vendor's own card is zero. The attempt is refused either way.
    let run = one_step_run(&fx);
    let no_card = |_: &str| AdapterBuild {
        api_key: Some(Arc::new(|_| Some("k".to_string()))),
        ..AdapterBuild::default()
    };
    let report = execute_workflow_step_with(
        WorkflowEnv::single(&fx.store),
        &run.id,
        &options("wf-vendor-nokey"),
        Some(&no_card as &StepBuildHook<'_>),
    )
    .unwrap();
    assert!(matches!(
        report.action,
        StepExecutionAction::Executed {
            outcome: StepOutcome::Fail,
            ..
        }
    ));
    assert!(
        fx.store
            .attempts(&fx.project)
            .unwrap()
            .iter()
            .all(|a| a.status == AttemptStatus::Refused)
    );
    assert!(mock.requests().is_empty(), "nothing was sent");
}

#[test]
fn an_unknown_vendor_is_still_unknown_and_the_five_clis_run_as_before() {
    let fx = fixture();
    let run = one_step_run(&fx);
    let err = execute_workflow_step(
        WorkflowEnv::single(&fx.store),
        &run.id,
        &options("wf-no-such-vendor"),
    )
    .unwrap_err();
    assert!(
        matches!(&err, WorkflowExecError::Attempt(AttemptError::UnknownAdapter(n)) if n == "wf-no-such-vendor"),
        "{err:?}"
    );

    // `claude` is the CLI adapter: it spawns the stub and is recorded as `claude`, and a hook
    // (which a CLI never consults) changes nothing.
    let stub = fx.root.join("stub-claude.sh");
    fs::write(
        &stub,
        "#!/bin/sh\nprintf '%s\\n' '{\"type\":\"result\",\"result\":\"ok\",\"usage\":{\"input_tokens\":100,\"output_tokens\":25},\"total_cost_usd\":0.005}'\n",
    )
    .unwrap();
    fs::set_permissions(&stub, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let mut opts = options("Claude");
    opts.binary_override = Some(stub.to_string_lossy().into_owned());
    let hook = priced(Some("unused"));
    let report = execute_workflow_step_with(
        WorkflowEnv::single(&fx.store),
        &run.id,
        &opts,
        Some(&hook as &StepBuildHook<'_>),
    )
    .unwrap();
    assert!(matches!(
        &report.action,
        StepExecutionAction::Executed { adapter, outcome: StepOutcome::Pass, .. } if adapter == "claude"
    ));
    assert_eq!(
        report.run.steps["only"].adapter_used.as_deref(),
        Some("claude")
    );
    assert_eq!(report.run.total_cost_usd_micros, 5000);
}

// ---- `fl flow step --adapter <vendor>` with `[vendors.<id>]` ----

fn git(dir: &Path, args: &[&str]) {
    assert!(
        Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap()
            .success()
    );
}

#[test]
fn fl_flow_step_runs_a_configured_vendor_with_its_rate_card_and_a_vault_key() {
    let mock = Mock::start();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let (cfg, data, repo) = (root.join("config"), root.join("data"), root.join("repo"));
    fs::create_dir_all(cfg.join("fl")).unwrap();
    fs::create_dir_all(data.join("fl")).unwrap();
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "t@example.com"]);
    git(&repo, &["config", "user.name", "t"]);
    fs::write(repo.join("a.txt"), "a\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "init"]);
    let repo = repo.canonicalize().unwrap();
    let store = data.join("fl/fl.redb");
    fs::write(
        cfg.join("fl/config.toml"),
        format!(
            "[state]\nstore = \"{store}\"\n\n\
             [[project]]\nname = \"ferroloop\"\nroot = \"{repo}\"\nstore = \"{store}\"\n\n\
             [vendors.wf-cli-vendor]\ntransport = \"http_api\"\n\
             dialect = \"openai_chat_completions\"\nbase_url = \"{base}/v1\"\n\
             default_model = \"served\"\nvault_secret = \"wf_cli_key\"\n\
             input_usd_micros_per_mtok = 2000000\noutput_usd_micros_per_mtok = 10000000\n",
            store = store.display(),
            repo = repo.display(),
            base = mock.base
        ),
    )
    .unwrap();

    // A Vault holding the key, open to the loopback host.
    let master = fl_gate::load_or_init_master_key(&cfg.join("fl/vault.key")).unwrap();
    let mut vault = fl_gate::Vault::open(data.join("fl/vault.enc"), master).unwrap();
    vault
        .set(fl_gate::SetSecretArgs {
            name: "WF_CLI_KEY".into(),
            value: "vault-key-wf".into(),
            allowed_hosts: vec!["127.0.0.1".into()],
            allow_private_network: true,
            ..fl_gate::SetSecretArgs::default()
        })
        .unwrap();

    let fl = |args: &[&str]| -> Value {
        let out = Command::new(env!("CARGO_BIN_EXE_fl"))
            .current_dir(&repo)
            .env("HOME", &root)
            .env("XDG_RUNTIME_DIR", &root)
            .env("XDG_CACHE_HOME", &root)
            .env("XDG_CONFIG_HOME", &cfg)
            .env("XDG_DATA_HOME", &data)
            .env("FL_WIRE_ROUTES", root.join("routes.json"))
            .env("FL_WIRE_SOCKET", root.join("wire.sock"))
            .env("FL_WIRE_DISABLE_HOST_DISCOVERY", "1")
            .env_remove("FL_DB")
            .env_remove("AGENT_PERSONA")
            .env_remove("FL_VAULT_PATH")
            .env_remove("FL_VAULT_KEY_PATH")
            .env_remove("FL_VAULT_MASTER_KEY")
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "fl {args:?}: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap_or(Value::Null)
    };
    fl(&["project", "add", "."]);
    fl(&[
        "agent",
        "register",
        "Felicia",
        "--project",
        "ferroloop",
        "--role",
        "assistant",
        "--create-area",
        "--by",
        "owner",
    ]);
    let flows = repo.join(".fl/flows");
    fs::create_dir_all(&flows).unwrap();
    fs::write(
        flows.join("v.toml"),
        r#"
slug = "v"
title = "Vendor flow"
start_step = "only"
max_total_steps = 4

[[steps]]
step_id = "only"
title = "Only"
prompt = "Say hello."
assignee = { kind = "agent", name = "felicia" }
completion = { mode = "ungated" }
max_attempts = 1
max_visits = 3

[[edges]]
from_step = "only"
to_step = "END"
on = "pass"
"#,
    )
    .unwrap();
    fl(&[
        "flow",
        "import",
        flows.join("v.toml").to_str().unwrap(),
        "--by",
        "ferris",
        "--json",
    ]);
    fl(&[
        "flow", "start", "v", "--title", "t", "--by", "ferris", "--json",
    ]);
    let done = fl(&[
        "flow",
        "step",
        "1",
        "--adapter",
        "wf-cli-vendor",
        "--by",
        "felicia",
        "--json",
    ]);
    assert_eq!(
        done["steps"]["only"]["adapter_used"],
        json!("wf-cli-vendor"),
        "{done}"
    );
    assert_eq!(done["steps"]["only"]["state"], json!("done"), "{done}");
    assert_eq!(mock.requests(), vec!["Bearer vault-key-wf".to_string()]);
    assert_eq!(done["total_tokens"], json!(1200), "{done}");
    assert_eq!(done["total_cost_usd_micros"], json!(4000), "{done}");
}
