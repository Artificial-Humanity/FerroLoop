//! End-to-end CLI and MCP integration tests for `fl flow` and workflow tools (`workflow spec §4, §5`).

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use fl_core::store::{Catalog, Ledger};

use serde_json::{Value, json};
use tempfile::TempDir;

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn write_stub_adapter(dir: &Path, name: &str, text: &str, exit_code: i32) -> PathBuf {
    let path = dir.join(name);
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' '{{\"type\":\"result\",\"result\":\"{text}\",\"usage\":{{\"input_tokens\":100,\"output_tokens\":25}},\"total_cost_usd\":0.0012}}'\nexit {exit_code}\n"
    );
    fs::write(&path, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).unwrap();
    }
    path
}

struct WorkflowFixture {
    _tmp: TempDir,
    root: PathBuf,
    cfg_dir: PathBuf,
    data_dir: PathBuf,
    repo_dir: PathBuf,
    store_path: PathBuf,
    routes_file: PathBuf,
    sock_file: PathBuf,
}

impl WorkflowFixture {
    fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let cfg_dir = root.join("config");
        let data_dir = root.join("data");
        let repo_dir = root.join("ferroloop");
        let routes_file = root.join("routes.json");
        let sock_file = root.join("wire.sock");
        let store_path = data_dir.join("fl/fl.redb");

        fs::create_dir_all(cfg_dir.join("fl")).unwrap();
        fs::create_dir_all(&data_dir).unwrap();
        fs::create_dir_all(repo_dir.join("src")).unwrap();

        git(&repo_dir, &["init", "-q"]);
        git(&repo_dir, &["config", "user.email", "t@example.com"]);
        git(&repo_dir, &["config", "user.name", "t"]);
        fs::write(
            repo_dir.join("src/lib.rs"),
            "pub fn feature() -> u32 { 42 }\n",
        )
        .unwrap();
        git(&repo_dir, &["add", "-A"]);
        git(&repo_dir, &["commit", "-qm", "initial"]);

        let canon_repo = repo_dir.canonicalize().unwrap();
        let cfg = format!(
            "[state]\nstore = \"{}\"\n\n\
             [[project]]\nname = \"ferroloop\"\nroot = \"{}\"\nstore = \"{}\"\n",
            store_path.display(),
            canon_repo.display(),
            store_path.display(),
        );
        fs::write(cfg_dir.join("fl/config.toml"), cfg).unwrap();

        Self {
            _tmp: tmp,
            root,
            cfg_dir,
            data_dir,
            repo_dir: canon_repo,
            store_path,
            routes_file,
            sock_file,
        }
    }

    fn fl(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_fl"));
        c.current_dir(&self.repo_dir)
            .env("HOME", &self.root)
            .env("XDG_RUNTIME_DIR", &self.root)
            .env("XDG_CACHE_HOME", &self.root)
            .env("XDG_CONFIG_HOME", &self.cfg_dir)
            .env("XDG_DATA_HOME", &self.data_dir)
            .env("FL_WIRE_ROUTES", &self.routes_file)
            .env("FL_WIRE_SOCKET", &self.sock_file)
            .env("FL_WIRE_DISABLE_HOST_DISCOVERY", "1")
            .env_remove("FL_DB")
            .env_remove("AGENT_PERSONA")
            .env_remove("CLAUDE_CODE_MESSAGING_SOCKET")
            .env_remove("ANTIGRAVITY_CONVERSATION_ID")
            .env_remove("CODEX_THREAD_ID")
            .env_remove("MUSE_SESSION_ID")
            .env_remove("GROK_SESSION_ID")
            .env_remove("GROK_HOOK_EVENT");
        c
    }

    fn run_ok(&self, args: &[&str]) -> String {
        let out = self.fl().args(args).output().unwrap();
        assert!(
            out.status.success(),
            "fl {args:?} failed ({:?}):\nstdout: {}\nstderr: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn run_json(&self, args: &[&str]) -> Value {
        let stdout = self.run_ok(args);
        serde_json::from_str(&stdout).unwrap()
    }
}

#[test]
fn workflow_export_import_and_gated_review_loop_end_to_end() {
    let f = WorkflowFixture::new();

    // 1. Register project, roster (`jarvis`, `ferris`, `felicia`), and command gate `check-file`.
    f.run_ok(&["project", "add", "."]);

    for (name, project, role) in [
        ("Jarvis", "ai-lab-amd", "lab_manager"),
        ("Ferris", "ferroloop", "lead"),
        ("Felicia", "ferroloop", "assistant"),
    ] {
        f.run_ok(&[
            "agent",
            "register",
            name,
            "--project",
            project,
            "--role",
            role,
            "--create-area",
            "--by",
            "owner",
        ]);
    }

    f.run_ok(&[
        "gate",
        "add",
        "--project",
        "1",
        "--name",
        "check-file",
        "--glob",
        "src/**/*.rs",
        "--program",
        "true",
        "--authored-by",
        "ferris",
    ]);

    // 2. Write a workflow TOML and import it via `fl flow import`.
    let flow_toml_dir = f.repo_dir.join(".fl/flows");
    fs::create_dir_all(&flow_toml_dir).unwrap();
    let flow_toml_path = flow_toml_dir.join("review-loop.toml");
    let toml_content = r#"
slug = "review-loop"
title = "Gated Implement and Lead Review Loop"
start_step = "implement"
max_total_steps = 16
budget_usd_micros = 5000000
max_tokens = 100000

[declared_keys]
branch = "last_writer"
modules = "append"

[[steps]]
step_id = "implement"
title = "Implement feature"
prompt = "Implement the requested feature in src/lib.rs."
assignee = { kind = "agent", name = "felicia" }
completion = { mode = "gated", gates = ["check-file"], regret = "low" }
max_attempts = 2
max_visits = 3
isolate_worktree = false

[[steps]]
step_id = "review"
title = "Independent lead review"
prompt = "Review the implementation from implement."
reviews_step = "implement"
require_different_vendor = true
completion = { mode = "human_sign_off", approval_tier = "lead" }
max_attempts = 1
max_visits = 3

[[edges]]
from_step = "implement"
to_step = "review"
on = "pass"

[[edges]]
from_step = "review"
to_step = "implement"
on = "fail"

[[edges]]
from_step = "review"
to_step = "END"
on = "pass"
"#;
    fs::write(&flow_toml_path, toml_content).unwrap();

    let imported = f.run_json(&[
        "flow",
        "import",
        flow_toml_path.to_str().unwrap(),
        "--by",
        "ferris",
        "--json",
    ]);
    assert_eq!(imported["handle"], "1");
    assert_eq!(imported["slug"], "review-loop");

    // 3. Verify `fl flow list`, `fl flow show` (by handle `1` and slug `review-loop`), and `fl flow export`.
    let listed = f.run_json(&["flow", "list", "--json"]);
    assert_eq!(listed.as_array().unwrap().len(), 1);
    assert_eq!(listed[0]["slug"], "review-loop");

    let shown_by_handle = f.run_json(&["flow", "show", "1", "--json"]);
    let shown_by_slug = f.run_json(&["flow", "show", "review-loop", "--json"]);
    assert_eq!(shown_by_handle["id"], shown_by_slug["id"]);
    assert_eq!(shown_by_slug["steps"].as_array().unwrap().len(), 2);

    fs::remove_file(&flow_toml_path).unwrap();
    let exported = f.run_json(&["flow", "export", "review-loop", "--json"]);
    assert!(flow_toml_path.exists());
    let exported_toml = fs::read_to_string(&flow_toml_path).unwrap();
    assert!(exported_toml.contains("check-file"), "{exported_toml}");
    assert_eq!(exported["slug"], "review-loop");

    // 4. Start a workflow run (`fl flow start review-loop --title "Build feature" --by ferris`).
    let started = f.run_json(&[
        "flow",
        "start",
        "review-loop",
        "--title",
        "Build feature",
        "--by",
        "ferris",
        "--json",
    ]);
    assert_eq!(started["handle"], "1");
    assert_eq!(started["status"], "running");
    assert_eq!(started["active_steps"], json!(["implement"]));

    // 5. Run `implement` with a fake adapter script; verify gate runs and `review` becomes active in `NeedsHuman`.
    let fake_adapter = write_stub_adapter(&f.root, "fake-claude.sh", "Implemented feature", 0);
    let step1 = f.run_json(&[
        "flow",
        "step",
        "1",
        "--step",
        "implement",
        "--adapter",
        "claude",
        "--binary",
        fake_adapter.to_str().unwrap(),
        "--by",
        "felicia",
        "--json",
    ]);
    assert_eq!(step1["status"], "needs_human");
    assert_eq!(step1["active_steps"], json!(["review"]));
    assert_eq!(step1["steps"]["implement"]["state"], "done");
    assert_eq!(step1["steps"]["implement"]["assigned_agent"], "felicia");
    assert_eq!(step1["steps"]["implement"]["adapter_used"], "claude");
    assert_eq!(step1["steps"]["review"]["state"], "needs_human");
    assert_eq!(step1["steps"]["review"]["visit_count"], 1);

    // 6. Verify `fl flow assign 1 review --agent felicia` is refused with exit code 2 (same agent as `implement`).
    let assign_same_agent = f
        .fl()
        .args([
            "flow", "assign", "1", "review", "--agent", "felicia", "--by", "ferris",
        ])
        .output()
        .unwrap();
    assert_eq!(assign_same_agent.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&assign_same_agent.stderr).contains("review independence"),
        "stderr: {}",
        String::from_utf8_lossy(&assign_same_agent.stderr)
    );

    // Verify `fl flow assign 1 review --agent ferris --adapter claude` is refused with exit code 2 (same vendor).
    let assign_same_vendor = f
        .fl()
        .args([
            "flow",
            "assign",
            "1",
            "review",
            "--agent",
            "ferris",
            "--adapter",
            "claude",
            "--by",
            "ferris",
        ])
        .output()
        .unwrap();
    assert_eq!(assign_same_vendor.status.code(), Some(2));

    // 7. Verify `fl flow approve 1 review --by felicia` is refused with exit code 2 (`InsufficientApprovalAuthority`).
    let approve_felicia = f
        .fl()
        .args(["flow", "approve", "1", "review", "--by", "felicia"])
        .output()
        .unwrap();
    assert_eq!(approve_felicia.status.code(), Some(2));

    // 8. Reject `review` as `ferris` (`fl flow reject 1 review --by ferris --reason "Add missing edge case"`),
    // verifying the run loops back to `implement` (`visit_count == 2`).
    let rejected = f.run_json(&[
        "flow",
        "reject",
        "1",
        "review",
        "--by",
        "ferris",
        "--reason",
        "Add missing edge case",
        "--json",
    ]);
    assert_eq!(rejected["status"], "running");
    assert_eq!(rejected["active_steps"], json!(["implement"]));
    assert_eq!(rejected["steps"]["implement"]["visit_count"], 2);
    assert_eq!(rejected["steps"]["implement"]["state"], "todo");

    // 9. Re-run `implement` (`fl flow step 1 --step implement ...`), then approve `review` as `ferris`.
    let step2 = f.run_json(&[
        "flow",
        "step",
        "1",
        "--step",
        "implement",
        "--adapter",
        "claude",
        "--binary",
        fake_adapter.to_str().unwrap(),
        "--by",
        "felicia",
        "--json",
    ]);
    assert_eq!(step2["status"], "needs_human");
    assert_eq!(step2["active_steps"], json!(["review"]));
    assert_eq!(step2["steps"]["review"]["visit_count"], 2);

    let approved = f.run_json(&[
        "flow", "approve", "1", "review", "--by", "ferris", "--note", "LGTM", "--json",
    ]);
    assert_eq!(approved["status"], "completed");
    assert!(approved["active_steps"].as_array().unwrap().is_empty());
    assert_eq!(approved["steps"]["review"]["state"], "done");

    let status_out = f.run_json(&["flow", "status", "1", "--json"]);
    assert_eq!(status_out["status"], "completed");
    assert_eq!(status_out["total_steps_executed"], 2);
    assert_eq!(status_out["steps"]["implement"]["visit_count"], 2);
    assert_eq!(status_out["steps"]["review"]["visit_count"], 2);
    assert!(!status_out["handoffs"].as_array().unwrap().is_empty());

    // Verify ledger recorded attempts and gate runs in the store.
    let store = fl_store::open_store(&f.store_path).unwrap();
    let proj_id = store.list_projects().unwrap()[0].id.clone();
    let attempts = store.attempts(&proj_id).unwrap();
    assert_eq!(
        attempts.len(),
        2,
        "expected 2 attempts for implement v1 and v2"
    );
}

#[test]
fn workflow_mcp_tools_list_status_handoff_and_declared_keys_enforcement() {
    let f = WorkflowFixture::new();
    f.run_ok(&["project", "add", "."]);

    let flow_path = f.repo_dir.join("mcp-flow.toml");
    let toml_content = r#"
slug = "mcp-flow"
title = "MCP Workflow"
start_step = "plan"

[declared_keys]
branch = "last_writer"
files = "append"

[[steps]]
step_id = "plan"
title = "Plan"
prompt = "Plan the work."

[[steps]]
step_id = "build"
title = "Build"
prompt = "Build the planned work."

[[edges]]
from_step = "plan"
to_step = "build"
on = "pass"

[[edges]]
from_step = "build"
to_step = "END"
on = "pass"
"#;
    fs::write(&flow_path, toml_content).unwrap();

    f.run_ok(&[
        "flow",
        "add",
        "--file",
        flow_path.to_str().unwrap(),
        "--by",
        "ferris",
    ]);
    let started = f.run_json(&[
        "flow",
        "start",
        "mcp-flow",
        "--title",
        "MCP test run",
        "--by",
        "ferris",
        "--json",
    ]);
    let run_id = started["id"].as_str().unwrap().to_string();

    let mut child = f
        .fl()
        .args(["state", "mcp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();

    let mut stdin = child.stdin.take().unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());

    let mut call_rpc = |req: Value| -> Value {
        let line = serde_json::to_string(&req).unwrap();
        writeln!(stdin, "{line}").unwrap();
        stdin.flush().unwrap();
        let mut resp_line = String::new();
        reader.read_line(&mut resp_line).unwrap();
        serde_json::from_str(&resp_line).unwrap()
    };

    // 1. Check `tools/list` includes the 4 workflow MCP tools.
    let tools_resp = call_rpc(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/list",
        "params": {}
    }));
    let tool_names: Vec<String> = tools_resp["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    for expected in [
        "workflow_list",
        "workflow_status",
        "workflow_handoff",
        "workflow_step",
    ] {
        assert!(
            tool_names.contains(&expected.to_string()),
            "missing MCP tool `{expected}` in {tool_names:?}"
        );
    }

    // 2. `workflow_list` returns the workflow definition and active run.
    let list_resp = call_rpc(json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "workflow_list",
            "arguments": {}
        }
    }));
    assert_eq!(list_resp["result"]["isError"], false);
    let list_payload: Value =
        serde_json::from_str(list_resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(list_payload["workflows"].as_array().unwrap().len(), 1);
    assert_eq!(list_payload["runs"].as_array().unwrap().len(), 1);
    assert_eq!(list_payload["runs"][0]["id"], run_id);

    // 3. `workflow_handoff` with an undeclared key returns `isError: true`.
    let bad_handoff = call_rpc(json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "workflow_handoff",
            "arguments": {
                "run_id": "1",
                "step_id": "plan",
                "summary": "Planned changes",
                "facts": {
                    "undeclared_key": "forbidden"
                },
                "by": "felicia"
            }
        }
    }));
    assert_eq!(bad_handoff["result"]["isError"], true);
    assert!(
        bad_handoff["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("undeclared_key")
    );

    // 4. `workflow_handoff` with declared keys (`branch`, `files`) succeeds.
    let ok_handoff = call_rpc(json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "tools/call",
        "params": {
            "name": "workflow_handoff",
            "arguments": {
                "run_id": "1",
                "step_id": "plan",
                "summary": "Planned module layout",
                "shipped": ["docs/plan.md"],
                "next_steps": ["Implement build step"],
                "facts": {
                    "branch": "feat/mcp",
                    "files": ["src/lib.rs"]
                },
                "by": "felicia"
            }
        }
    }));
    assert_eq!(
        ok_handoff["result"]["isError"], false,
        "ok_handoff error: {ok_handoff}"
    );
    let handoff_payload: Value =
        serde_json::from_str(ok_handoff["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(handoff_payload["handoff"]["slug"], "step-plan-v1");
    assert_eq!(handoff_payload["facts"].as_array().unwrap().len(), 2);

    // 5. `workflow_step` advances `plan` -> `build` -> `END` (`Completed`).
    let step_plan = call_rpc(json!({
        "jsonrpc": "2.0",
        "id": 5,
        "method": "tools/call",
        "params": {
            "name": "workflow_step",
            "arguments": {
                "run_id": "1",
                "step_id": "plan",
                "outcome": "pass",
                "by": "felicia"
            }
        }
    }));
    assert_eq!(step_plan["result"]["isError"], false);
    let step_plan_payload: Value =
        serde_json::from_str(step_plan["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(step_plan_payload["run"]["active_steps"], json!(["build"]));

    let step_build = call_rpc(json!({
        "jsonrpc": "2.0",
        "id": 6,
        "method": "tools/call",
        "params": {
            "name": "workflow_step",
            "arguments": {
                "run_id": &run_id,
                "step_id": "build",
                "outcome": "pass",
                "summary": "Built and verified",
                "by": "felicia"
            }
        }
    }));
    assert_eq!(step_build["result"]["isError"], false);
    let step_build_payload: Value =
        serde_json::from_str(step_build["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(step_build_payload["run"]["status"], "completed");

    // 6. `workflow_status` returns the completed run, workflow definition, handoffs, and declared facts.
    let status_resp = call_rpc(json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "tools/call",
        "params": {
            "name": "workflow_status",
            "arguments": {
                "run_id": "1"
            }
        }
    }));
    assert_eq!(status_resp["result"]["isError"], false);
    let status_payload: Value = serde_json::from_str(
        status_resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(status_payload["run"]["status"], "completed");
    assert_eq!(status_payload["workflow"]["slug"], "mcp-flow");
    assert_eq!(status_payload["handoffs"].as_array().unwrap().len(), 2);
    assert_eq!(status_payload["facts"].as_array().unwrap().len(), 2);

    drop(stdin);
    let exit = child.wait().unwrap();
    assert!(exit.success());
}
