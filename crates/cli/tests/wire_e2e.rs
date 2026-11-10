use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde_json::{Value, json};
use tempfile::TempDir;

struct TestEnv {
    _tmp: TempDir,
    root: PathBuf,
    cfg_dir: PathBuf,
    data_dir: PathBuf,
    work_dir: PathBuf,
    routes_file: PathBuf,
    sock_file: PathBuf,
}

impl TestEnv {
    fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let cfg_dir = root.join("config");
        let data_dir = root.join("data");
        let work_dir = root.join("work");
        let routes_file = root.join("routes.json");
        let sock_file = root.join("wire.sock");

        std::fs::create_dir_all(cfg_dir.join("fl")).unwrap();
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::create_dir_all(work_dir.join(".agents")).unwrap();

        let store_path = data_dir.join("fl/fl.redb");
        std::fs::write(
            cfg_dir.join("fl/config.toml"),
            format!(
                "[state]\nstore = \"{}\"\n\n[wire]\nsocket = \"{}\"\nretention_days = 30\nsync_wire = true\n",
                store_path.display(),
                sock_file.display()
            ),
        )
        .unwrap();

        Self {
            _tmp: tmp,
            root,
            cfg_dir,
            data_dir,
            work_dir,
            routes_file,
            sock_file,
        }
    }

    fn cmd_for(&self, bin_path: &std::path::Path) -> Command {
        let mut c = Command::new(bin_path);
        c.current_dir(&self.work_dir)
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
            .env_remove("AGENT_NAME")
            .env_remove("FL_HOSTED_ID")
            .env_remove("AGENT_HYDRATION_GENERATION")
            .env_remove("CLAUDE_SESSION_ID")
            .env_remove("ANTIGRAVITY_LS_ADDRESS")
            .env_remove("ANTIGRAVITY_CSRF_TOKEN")
            .env_remove("ANTIGRAVITY_AGENTAPI_EXE")
            .env_remove("ANTIGRAVITY_AGENT")
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env_remove("CLAUDE_CODE_MESSAGING_SOCKET")
            .env_remove("ANTIGRAVITY_CONVERSATION_ID")
            .env_remove("CODEX_THREAD_ID")
            .env_remove("MUSE_SESSION_ID")
            .env_remove("GROK_SESSION_ID")
            .env_remove("GROK_HOOK_EVENT");
        c
    }

    fn fl(&self) -> Command {
        self.cmd_for(std::path::Path::new(env!("CARGO_BIN_EXE_fl")))
    }

    fn symlink_bin(&self, name: &str) -> PathBuf {
        let link = self.root.join(name);
        if !link.exists() {
            #[cfg(unix)]
            std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_fl"), &link).unwrap();
        }
        link
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

#[test]
fn fl_wire_cli_send_broadcast_ask_reply_read_ack_wait_status_and_prune() {
    let env = TestEnv::new();

    // Register three agents: ferris (ferroloop lead), kayla (kaggle-gemma4 lead), kacey (kaggle-gemma4 journalist, registered via the legacy `scribe` spelling)
    let agent_msg_bin = env.symlink_bin("agent-msg");
    for (name, proj, role) in [
        ("Ferris", "ferroloop", "lead"),
        ("Kayla", "kaggle-gemma4", "lead"),
        ("Kacey", "kaggle-gemma4", "scribe"),
    ] {
        let out = env
            .cmd_for(&agent_msg_bin)
            .args(["register", name, "--project", proj, "--role", role])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "register {name} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    // Verify agent-msg roster reflects registered agents without creating .agents/REGISTRY.md
    let roster_out = env
        .cmd_for(&agent_msg_bin)
        .args(["roster"])
        .output()
        .unwrap();
    assert!(roster_out.status.success());
    let roster_txt = String::from_utf8_lossy(&roster_out.stdout);
    assert!(roster_txt.contains("Ferris") && roster_txt.contains("journalist"));
    assert!(!roster_txt.contains("scribe"), "{roster_txt}");
    assert!(!env.work_dir.join(".agents/REGISTRY.md").exists());

    // Verify collision returns exit code 2 without --replace
    let dup = env
        .cmd_for(&agent_msg_bin)
        .args([
            "register",
            "Ferris",
            "--project",
            "ferroloop",
            "--role",
            "lead",
        ])
        .output()
        .unwrap();
    assert_eq!(dup.status.code(), Some(2));

    // Direct send from ferris to kayla
    let mut send_cmd = env.fl();
    send_cmd.args([
        "wire",
        "send",
        "kayla",
        "Spec review",
        "Please check section 3.",
        "--from",
        "ferris",
        "--json",
    ]);
    let sent = run_json(send_cmd);
    let msg_id = sent["message"]["id"].as_str().unwrap().to_string();
    assert_eq!(sent["receipts"].as_array().unwrap().len(), 1);
    assert_eq!(sent["receipts"][0]["recipient_name"], "kayla");

    // Area+role selector send: @area:kaggle-gemma4/journalist -> only kacey
    let mut role_cmd = env.fl();
    role_cmd.args([
        "wire",
        "send",
        "@area:kaggle-gemma4/journalist",
        "Log update",
        "Update experiment table.",
        "--from",
        "ferris",
        "--json",
    ]);
    let role_sent = run_json(role_cmd);
    assert_eq!(role_sent["receipts"].as_array().unwrap().len(), 1);
    assert_eq!(role_sent["receipts"][0]["recipient_name"], "kacey");

    // Broadcast from ferris -> reaches kayla and kacey, not ferris
    let mut bcast_cmd = env.fl();
    bcast_cmd.args([
        "wire",
        "broadcast",
        "Maintenance window",
        "Rebooting in 10m.",
        "--from",
        "ferris",
        "--json",
    ]);
    let bcast = run_json(bcast_cmd);
    assert_eq!(bcast["receipts"].as_array().unwrap().len(), 2);

    // Read kayla's inbox without --ack, then ack explicitly
    let mut read_cmd = env.fl();
    read_cmd.args(["wire", "read", "--for", "kayla", "--json"]);
    let inbox = run_json(read_cmd);
    assert_eq!(inbox.as_array().unwrap().len(), 2);

    let mut ack_cmd = env.fl();
    ack_cmd.args(["wire", "ack", &msg_id, "--for", "kayla", "--json"]);
    let acked = run_json(ack_cmd);
    assert_eq!(acked[0]["status"], "acked");

    // Ask + Reply correlation
    let mut ask_cmd = env.fl();
    ask_cmd.args([
        "wire",
        "ask",
        "kayla",
        "GPU free?",
        "Need 1 GPU for 5m.",
        "--from",
        "ferris",
        "--json",
    ]);
    let ask_sent = run_json(ask_cmd);
    let ask_id = ask_sent["id"]
        .as_str()
        .or_else(|| ask_sent["message"]["id"].as_str())
        .unwrap()
        .to_string();

    let mut reply_cmd = env.fl();
    reply_cmd.args([
        "wire",
        "reply",
        &ask_id,
        "Yes, GPU 0 is idle.",
        "--from",
        "kayla",
        "--json",
    ]);
    let reply_sent = run_json(reply_cmd);
    assert_eq!(reply_sent["message"]["correlation_id"], ask_id);
    assert_eq!(reply_sent["message"]["subject"], "Re: GPU free?");

    // Non-blocking wait prints dispatcher status and exits 0 when inbox is empty
    let mut drain_cmd = env.fl();
    drain_cmd.args(["wire", "read", "--ack", "--for", "ferris", "--json"]);
    let _ = run_json(drain_cmd);

    let wait_out = env
        .fl()
        .args(["wire", "wait", "--for", "ferris"])
        .output()
        .unwrap();
    assert!(wait_out.status.success());
    assert!(
        String::from_utf8_lossy(&wait_out.stdout)
            .contains("Central FerroWire dispatcher is active for 'ferris'")
    );

    // Status and prune
    let mut status_cmd = env.fl();
    status_cmd.args(["wire", "status", "--json"]);
    let st = run_json(status_cmd);
    assert_eq!(st["daemon"], "inactive");

    let mut prune_cmd = env.fl();
    prune_cmd.args(["wire", "prune", "--older-than-days", "30", "--json"]);
    let pruned = run_json(prune_cmd);
    assert_eq!(pruned["pruned"], 0);

    // Unregister via agent-msg
    let unreg = env
        .cmd_for(&agent_msg_bin)
        .args(["unregister", "kacey"])
        .output()
        .unwrap();
    assert!(unreg.status.success());
}

#[test]
fn claude_and_codex_hook_delivers_and_acks_unread_messages() {
    let env = TestEnv::new();
    let hook_bin = env.symlink_bin("agent-bus-claude-hook");

    // Send a message to jarvis
    let mut send_cmd = env.fl();
    send_cmd.args([
        "wire",
        "send",
        "jarvis",
        "Hook alert",
        "Check systemd unit.",
        "--from",
        "ferris",
    ]);
    assert!(send_cmd.status().unwrap().success());

    // Invoke Stop hook with AGENT_PERSONA=jarvis
    let mut child = env
        .cmd_for(&hook_bin)
        .env("AGENT_PERSONA", "jarvis")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(
            serde_json::to_vec(&json!({
                "hook_event_name": "Stop",
                "session_id": "sess-123",
                "cwd": env.work_dir,
            }))
            .unwrap()
            .as_slice(),
        )
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let resp: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(resp["decision"], "block");
    assert!(resp["reason"].as_str().unwrap().contains("Hook alert"));

    // Second Stop hook call sees empty inbox and prints {}
    let mut child2 = env
        .cmd_for(&hook_bin)
        .env("AGENT_PERSONA", "jarvis")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child2
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"{\"hook_event_name\":\"Stop\"}")
        .unwrap();
    let out2 = child2.wait_with_output().unwrap();
    let resp2: Value = serde_json::from_slice(&out2.stdout).unwrap();
    assert_eq!(resp2, json!({}));

    // Send another message and test SessionStart with a .codex transcript path
    let mut send_cmd2 = env.fl();
    send_cmd2.args([
        "wire",
        "send",
        "jarvis",
        "Codex startup",
        "Welcome back.",
        "--from",
        "ferris",
    ]);
    assert!(send_cmd2.status().unwrap().success());

    let codex_transcript = env.root.join(".codex/sessions/t-99.jsonl");
    std::fs::create_dir_all(codex_transcript.parent().unwrap()).unwrap();
    std::fs::write(&codex_transcript, "take the name of JARVIS\n").unwrap();

    let mut child3 = env
        .cmd_for(&hook_bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child3
        .stdin
        .as_mut()
        .unwrap()
        .write_all(
            serde_json::to_vec(&json!({
                "hook_event_name": "SessionStart",
                "thread_id": "thread-codex-99",
                "transcript_path": codex_transcript,
                "cwd": env.work_dir,
            }))
            .unwrap()
            .as_slice(),
        )
        .unwrap();
    let out3 = child3.wait_with_output().unwrap();
    let resp3: Value = serde_json::from_slice(&out3.stdout).unwrap();
    assert_eq!(resp3["hookSpecificOutput"]["hookEventName"], "SessionStart");
    assert!(
        resp3["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .contains("Codex startup")
    );

    // Send a message to mira (Meta Muse) and test `fl wire hook muse` with `"event": "SessionStart"`
    let mut send_muse = env.fl();
    send_muse.args([
        "wire",
        "send",
        "mira",
        "Muse hook check",
        "Muse session active.",
        "--from",
        "ferris",
    ]);
    assert!(send_muse.status().unwrap().success());

    let mut muse_child = env
        .fl()
        .args(["wire", "hook", "muse"])
        .env("AGENT_PERSONA", "mira")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    muse_child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(
            serde_json::to_vec(&json!({
                "event": "SessionStart",
                "session_id": "muse-sess-77",
                "cwd": env.work_dir,
            }))
            .unwrap()
            .as_slice(),
        )
        .unwrap();
    let muse_out = muse_child.wait_with_output().unwrap();
    assert!(muse_out.status.success());
    let muse_resp: Value = serde_json::from_slice(&muse_out.stdout).unwrap();
    assert_eq!(
        muse_resp["hookSpecificOutput"]["hookEventName"],
        "SessionStart"
    );
    assert!(
        muse_resp["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .contains("Muse hook check")
    );

    // Send a message to gideon (xAI Grok) and test `fl wire hook grok` with `"hookEventName": "Stop"` and `"sessionId"`
    let mut send_grok = env.fl();
    send_grok.args([
        "wire",
        "send",
        "gideon",
        "Grok hook check",
        "Grok stop hook active.",
        "--from",
        "ferris",
    ]);
    assert!(send_grok.status().unwrap().success());

    let mut grok_child = env
        .fl()
        .args(["wire", "hook", "grok"])
        .env("AGENT_PERSONA", "gideon")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    grok_child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(
            serde_json::to_vec(&json!({
                "hookEventName": "Stop",
                "sessionId": "grok-sess-88",
                "cwd": env.work_dir,
            }))
            .unwrap()
            .as_slice(),
        )
        .unwrap();
    let grok_out = grok_child.wait_with_output().unwrap();
    assert!(grok_out.status.success());
    let grok_resp: Value = serde_json::from_slice(&grok_out.stdout).unwrap();
    assert_eq!(grok_resp["decision"], "block");
    assert!(
        grok_resp["reason"]
            .as_str()
            .unwrap()
            .contains("Grok hook check")
    );
}

#[test]
fn mcp_server_exposes_and_executes_all_ferrowire_tools() {
    let env = TestEnv::new();
    let mcp_bin = env.symlink_bin("agent-bus-mcp");

    let mut child = env
        .cmd_for(&mcp_bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());

    let mut call = |req: Value| -> Value {
        let line = format!("{}\n", serde_json::to_string(&req).unwrap());
        stdin.write_all(line.as_bytes()).unwrap();
        stdin.flush().unwrap();
        let mut resp_line = String::new();
        reader.read_line(&mut resp_line).unwrap();
        serde_json::from_str(&resp_line).unwrap()
    };

    // 1. tools/list
    let list_resp = call(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/list",
        "params": {}
    }));
    let tools = list_resp["result"]["tools"].as_array().unwrap();
    for expected in [
        "send_message",
        "broadcast_message",
        "ask_message",
        "reply_message",
        "get_messages",
        "ack_message",
    ] {
        assert!(
            tools.iter().any(|t| t["name"] == expected),
            "missing MCP tool {expected}"
        );
    }

    // 2. send_message
    let send_resp = call(json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "send_message",
            "arguments": {
                "to": "sonya",
                "subject": "Vocoder checkpoint",
                "body": "Step 50k ready.",
                "from_agent": "ferris"
            }
        }
    }));
    assert_eq!(send_resp["result"]["isError"], false);

    // 3. get_messages for sonya
    let get_resp = call(json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "get_messages",
            "arguments": {
                "agent": "sonya",
                "unread_only": true,
                "ack": false
            }
        }
    }));
    let text = get_resp["result"]["content"][0]["text"].as_str().unwrap();
    let msgs: Vec<Value> = serde_json::from_str(text).unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0]["recipient"], "sonya");
    assert_eq!(msgs[0]["to"], "sonya");
    let mid = msgs[0]["id"].as_str().unwrap().to_string();

    // 4. ack_message
    let ack_resp = call(json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "tools/call",
        "params": {
            "name": "ack_message",
            "arguments": {
                "message_id": mid,
                "agent": "sonya"
            }
        }
    }));
    assert_eq!(ack_resp["result"]["isError"], false);

    drop(stdin);
    let _ = child.wait();
}

#[test]
fn agent_msg_register_and_unregister_update_the_workspace_registry_from_a_subdirectory() {
    let env = TestEnv::new();
    let agent_msg_bin = env.symlink_bin("agent-msg");
    let sub = env.work_dir.join("ProjectA").join("src");
    std::fs::create_dir_all(&sub).unwrap();
    let registry = env.work_dir.join(".agents/REGISTRY.md");

    for (name, proj, role) in [("Ferris", "ferroloop", "lead"), ("Kayla", "kaggle", "lead")] {
        let out = env
            .cmd_for(&agent_msg_bin)
            .current_dir(&sub)
            .args(["register", name, "--project", proj, "--role", role])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let roster_out = env
        .cmd_for(&agent_msg_bin)
        .current_dir(&sub)
        .args(["roster"])
        .output()
        .unwrap();
    assert!(roster_out.status.success());
    let roster = String::from_utf8_lossy(&roster_out.stdout);
    assert!(
        roster.contains("Ferris") && roster.contains("Kayla"),
        "{roster}"
    );
    assert!(
        !registry.exists(),
        ".agents/REGISTRY.md must not be created in the workspace root"
    );
    assert!(
        !sub.join(".agents").exists(),
        "no registry may be created in the subdirectory"
    );

    let out = env
        .cmd_for(&agent_msg_bin)
        .current_dir(&sub)
        .args(["unregister", "kayla"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let roster_out = env
        .cmd_for(&agent_msg_bin)
        .current_dir(&sub)
        .args(["roster"])
        .output()
        .unwrap();
    assert!(roster_out.status.success());
    let roster = String::from_utf8_lossy(&roster_out.stdout);
    assert!(roster.contains("Ferris"), "{roster}");
    assert!(
        !roster.contains("Kayla"),
        "a retired agent must leave the active roster: {roster}"
    );
    assert!(
        !registry.exists(),
        ".agents/REGISTRY.md must not be created after unregister"
    );
}

#[test]
fn agent_msg_read_defaults_to_unread_and_all_includes_acknowledged() {
    let env = TestEnv::new();
    let agent_msg_bin = env.symlink_bin("agent-msg");
    for (name, proj) in [("Ferris", "ferroloop"), ("Kayla", "kaggle")] {
        assert!(
            env.cmd_for(&agent_msg_bin)
                .args(["register", name, "--project", proj, "--role", "lead"])
                .status()
                .unwrap()
                .success()
        );
    }
    assert!(
        env.fl()
            .args([
                "wire",
                "send",
                "kayla",
                "Unread default",
                "body",
                "--from",
                "ferris"
            ])
            .status()
            .unwrap()
            .success()
    );
    let read = |args: &[&str]| {
        let out = env
            .cmd_for(&agent_msg_bin)
            .env("AGENT_PERSONA", "kayla")
            .arg("read")
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    assert!(read(&["--ack"]).contains("Unread default"));
    assert!(
        !read(&[]).contains("Unread default"),
        "an acknowledged message is not unread"
    );
    assert!(read(&["--all"]).contains("Unread default"));
    assert!(
        !read(&["--all", "--unread-only"]).contains("Unread default"),
        "--unread-only wins over --all"
    );
}

#[test]
fn agent_msg_hydrate_runs_agent_hydrate_and_forwards_its_exit_status() {
    use std::os::unix::fs::PermissionsExt;
    let env = TestEnv::new();
    let agent_msg_bin = env.symlink_bin("agent-msg");
    let bindir = env.root.join("fake-bin");
    std::fs::create_dir_all(&bindir).unwrap();
    let args_out = env.root.join("hydrate.args");
    let script = bindir.join("agent-hydrate");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\necho \"$*\" > \"{}\"\nexit 7\n",
            args_out.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        bindir.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let out = env
        .cmd_for(&agent_msg_bin)
        .env("PATH", path)
        .args(["hydrate", "Kayla", "--replace"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(7),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(&args_out).unwrap().trim(),
        "Kayla --replace"
    );
}

#[test]
fn agent_msg_register_replace_keeps_an_existing_lab_manager_and_never_raises_anyone() {
    let env = TestEnv::new();
    let agent_msg_bin = env.symlink_bin("agent-msg");
    let register = |name: &str, project: &str, role: &str, extra: Option<&str>, replace: bool| {
        let mut cmd = env.cmd_for(&agent_msg_bin);
        cmd.args(["register", name, "--project", project, "--role", role]);
        if let Some(extra) = extra {
            cmd.args(["--extra-areas", extra]);
        }
        if replace {
            cmd.arg("--replace");
        }
        let out = cmd.output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    let roster = || -> Value {
        let out = env
            .cmd_for(&agent_msg_bin)
            .args(["roster", "--json"])
            .output()
            .unwrap();
        serde_json::from_slice(&out.stdout).unwrap()
    };
    let find = |roster: &Value, key: &str| -> Value {
        roster
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["name_key"] == key)
            .unwrap_or_else(|| panic!("{key} missing from {roster}"))
            .clone()
    };

    register("Manager", "lab", "lab_manager", Some("agents,notes"), false);
    register("Worker", "other", "assistant", None, false);
    // A reforge-style re-registration as lead.
    register("Manager", "lab", "lead", None, true);
    register("Worker", "other", "lead", None, true);

    let r = roster();
    let manager = find(&r, "manager");
    assert_eq!(manager["role"], "lab_manager", "{manager}");
    assert_eq!(
        manager["extra_areas"],
        serde_json::json!(["agents", "notes"])
    );
    // Only an existing lab manager is preserved: asking for lead raises nobody, and the name
    // is irrelevant.
    assert_eq!(find(&r, "worker")["role"], "lead");

    // A retired lab manager holds no role: unregister, then a lead --replace comes back as lead.
    let out = env
        .cmd_for(&agent_msg_bin)
        .args(["unregister", "manager"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    register("Manager", "lab", "lead", None, true);
    let r = roster();
    let manager = find(&r, "manager");
    assert_eq!(manager["role"], "lead", "{manager}");
    assert_eq!(manager["extra_areas"], serde_json::json!([]), "{manager}");
}

// Scripted peers exercise actual CLI Unix framing with fixture-only paths.
fn scripted_status(env: &TestEnv, diagnostics: Option<Value>, json_output: bool) -> String {
    let listener = std::os::unix::net::UnixListener::bind(&env.sock_file).unwrap();
    let mut response = json!({
        "status": "status", "node_id": "fixture-node", "host": "fixture-host",
        "socket_path": env.sock_file, "listen_addr": null,
        "connected_peers": ["fixture-peer"], "routes": [],
    });
    if let Some(snapshot) = diagnostics {
        response["dispatch_diagnostics"] = snapshot;
    }
    let peer = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut line = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&line).unwrap(),
            json!({"op":"status","include_codex_queues":true})
        );
        writeln!(stream, "{response}").unwrap();
    });
    let mut command = env.fl();
    command.args(["wire", "status"]);
    if json_output {
        command.arg("--json");
    }
    let output = command.output().unwrap();
    peer.join().unwrap();
    std::fs::remove_file(&env.sock_file).unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn diagnostic_fixture(held: bool) -> Value {
    json!({
        "schema_version": 1, "instance_nonce": null, "instance_started_at": "2026-11-06T00:00:00.000Z",
        "scope": "daemon_lifetime", "global_capacity_exhausted": false,
        "diagnostics_partial": false, "omitted_evidence_total": 0,
        "omitted_evidence_total_saturated": false,
        "records": if held { vec![json!({
            "recipient": "fixture-held", "recipient_digest": null, "recipient_invalid": false,
            "trigger_message_id": null, "trigger_message_id_digest": null,
            "trigger_message_id_omitted": false, "hold_observed_at": "2026-11-06T00:00:00.000Z",
            "phase": "retained_fence", "first_reason": "receipt_write_error",
            "decisive_reason": "unresolved_receipt_status",
            "failure_stages": ["receipt_write_error", "unresolved_receipt_status"],
            "omitted_failure_stages": 0, "omitted_failure_stages_saturated": false,
            "route_observations": [], "omitted_route_observations": 0,
            "omitted_route_observations_saturated": false, "route_dispatch_calls": 1,
            "route_dispatch_calls_saturated": false, "registration_generation": null,
        })] } else { vec![] },
    })
}

#[test]
fn wire_status_diagnostics_scripted_supported_empty_and_held() {
    let env = TestEnv::new();
    for held in [false, true] {
        let fixture = diagnostic_fixture(held);
        let output = scripted_status(&env, Some(fixture.clone()), true);
        let status: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(status["dispatch_diagnostics"], fixture);
        assert_eq!(status["connected_peers"], json!(["fixture-peer"]));
        let output = scripted_status(&env, Some(fixture), false);
        assert!(output.contains(&format!("dispatch_diagnostics=available\tholds={}\tglobal_capacity_exhausted=false\tomitted_evidence=0", usize::from(held))), "{output}");
        if held {
            assert!(output.contains("unresolved_receipt_status=1"), "{output}");
        }
    }
}

#[test]
fn wire_status_diagnostics_old_and_offline_are_unavailable() {
    let env = TestEnv::new();
    let output = scripted_status(&env, None, true);
    let value: Value = serde_json::from_str(&output).unwrap();
    assert!(value.get("dispatch_diagnostics").is_none());
    let output = scripted_status(&env, None, false);
    assert!(
        output.contains("dispatch_diagnostics=unavailable"),
        "{output}"
    );
    assert!(!output.contains("holds="));
    let mut cmd = env.fl();
    cmd.args(["wire", "status", "--json"]);
    let value = run_json(cmd);
    assert_eq!(value["daemon"], "inactive");
    assert_eq!(value.get("dispatch_diagnostics"), Some(&Value::Null));
    let output = env.fl().args(["wire", "status"]).output().unwrap();
    assert!(output.status.success());
    let output = String::from_utf8(output.stdout).unwrap();
    assert!(
        output.contains("dispatch_diagnostics=unavailable"),
        "{output}"
    );
    assert!(!output.contains("holds="));
}

#[test]
fn wire_status_diagnostics_active_daemon_is_supported_empty() {
    let env = TestEnv::new();
    let _daemon = fl_wire::daemon::spawn_daemon(
        std::sync::Arc::new(fl_store::RedbStore::open(&env.root.join("active.redb")).unwrap()),
        fl_wire::daemon::DaemonConfig {
            node_id: "fixture-node".into(),
            host: "fixture-host".into(),
            socket_path: env.sock_file.clone(),
            routes_path: Some(env.routes_file.clone()),
            home_override: Some(env.root.clone()),
            listen_addr: None,
            peers: vec![],
            psk: vec![],
        },
    )
    .unwrap();
    let mut command = env.fl();
    command.args(["wire", "status", "--json"]);
    let value = run_json(command);
    assert_eq!(value["node_id"], "fixture-node");
    let snapshot = &value["dispatch_diagnostics"];
    assert_eq!(snapshot["schema_version"], 1);
    assert_eq!(snapshot["records"], json!([]));
    assert_eq!(snapshot["global_capacity_exhausted"], false);
    if let Some(nonce) = snapshot["instance_nonce"].as_str() {
        assert_eq!(nonce.len(), 32);
        assert!(
            nonce
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        );
    } else {
        assert!(snapshot["instance_nonce"].is_null());
    }
    let output = env.fl().args(["wire", "status"]).output().unwrap();
    assert!(output.status.success());
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("dispatch_diagnostics=available\tholds=0")
    );
}

#[test]
fn wire_status_diagnostics_omission_and_saturation_remain_visible() {
    let env = TestEnv::new();
    let mut fixture = diagnostic_fixture(false);
    fixture["global_capacity_exhausted"] = json!(true);
    fixture["diagnostics_partial"] = json!(true);
    fixture["omitted_evidence_total"] = json!(u64::MAX);
    fixture["omitted_evidence_total_saturated"] = json!(true);
    let value: Value =
        serde_json::from_str(&scripted_status(&env, Some(fixture.clone()), true)).unwrap();
    assert_eq!(value["dispatch_diagnostics"], fixture);
    let output = scripted_status(&env, Some(fixture), false);
    for field in [
        "dispatch_diagnostics=available",
        "holds=0",
        "global_capacity_exhausted=true",
        "omitted_evidence=18446744073709551615",
        "diagnostics_partial=true",
        "omitted_evidence_saturated=true",
    ] {
        assert!(output.contains(field), "missing {field}: {output}");
    }
}

#[test]
fn wire_status_reports_pending_recipients_depth_and_age() {
    let env = TestEnv::new();
    let listener = std::os::unix::net::UnixListener::bind(&env.sock_file).unwrap();
    let response = json!({
        "status": "status",
        "node_id": "fixture-node",
        "host": "fixture-host",
        "socket_path": env.sock_file,
        "listen_addr": null,
        "connected_peers": [],
        "routes": [],
        "dispatch_diagnostics": diagnostic_fixture(false),
        "pending_recipients": [{
            "recipient": "fiona",
            "pending_count": 2,
            "oldest_pending_at": "2026-11-09T00:00:00.000Z",
            "oldest_pending_age_ms": 45000
        }]
    });
    let resp_json = response.clone();
    let peer = std::thread::spawn(move || {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            writeln!(stream, "{resp_json}").unwrap();
        }
    });

    let mut json_cmd = env.fl();
    json_cmd.args(["wire", "status", "--json"]);
    let st = run_json(json_cmd);
    assert_eq!(st["pending_recipients"][0]["recipient"], "fiona");
    assert_eq!(st["pending_recipients"][0]["pending_count"], 2);
    assert_eq!(st["pending_recipients"][0]["oldest_pending_age_ms"], 45000);

    let text_out = env.fl().args(["wire", "status"]).output().unwrap();
    peer.join().unwrap();
    std::fs::remove_file(&env.sock_file).unwrap();
    assert!(text_out.status.success());
    let text = String::from_utf8(text_out.stdout).unwrap();
    assert!(
        text.contains("pending_recipients=fiona:2(oldest_age=45000ms)"),
        "{text}"
    );
}

#[test]
fn wire_status_renders_codex_queues_known_bounded_estimated_age_and_unknown_never_zero() {
    let env = TestEnv::new();
    let listener = std::os::unix::net::UnixListener::bind(&env.sock_file).unwrap();
    let response = json!({
        "status": "status",
        "node_id": "fixture-node",
        "host": "fixture-host",
        "socket_path": env.sock_file,
        "listen_addr": null,
        "connected_peers": [],
        "routes": [],
        "dispatch_diagnostics": diagnostic_fixture(false),
        "codex_queues": [
            {
                "persona": "felix",
                "project": "ferroloop",
                "thread_id": "thread-felix",
                "queue": {
                    "state": "known",
                    "depth": 100,
                    "has_more": true,
                    "estimated_oldest_age_secs": 42,
                    "verified_codex_version": "0.167.0"
                }
            },
            {
                "persona": "kacey",
                "project": "ferroloop",
                "thread_id": "thread-kacey",
                "queue": {
                    "state": "known",
                    "depth": 0,
                    "has_more": false,
                    "verified_codex_version": "0.167.0"
                }
            },
            {
                "persona": "ghost",
                "queue": {
                    "state": "unknown",
                    "reason": "unverified Codex app-server version 0.168.0 (verified 0.167.0)",
                    "verified_codex_version": "0.167.0",
                    "detected_codex_version": "0.168.0"
                }
            },
            {
                "persona": "future",
                "queue": {
                    "state": "future_v2",
                    "extra": 42
                }
            }
        ]
    });
    let resp_json = response.clone();
    let peer = std::thread::spawn(move || {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            let req: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(req["op"], "status");
            assert_eq!(req["include_codex_queues"], true);
            writeln!(stream, "{resp_json}").unwrap();
        }
    });

    let mut json_cmd = env.fl();
    json_cmd.args(["wire", "status", "--json"]);
    let st = run_json(json_cmd);
    assert_eq!(st["codex_queues"][0]["persona"], "felix");
    assert_eq!(st["codex_queues"][0]["queue"]["state"], "known");
    assert_eq!(st["codex_queues"][0]["queue"]["depth"], 100);
    assert_eq!(st["codex_queues"][0]["queue"]["has_more"], true);
    assert_eq!(
        st["codex_queues"][0]["queue"]["estimated_oldest_age_secs"],
        42
    );
    assert_eq!(st["codex_queues"][1]["queue"]["depth"], 0);
    assert_eq!(st["codex_queues"][2]["queue"]["state"], "unknown");
    assert!(st["codex_queues"][2]["queue"].get("depth").is_none());
    assert_eq!(
        st["codex_queues"][2]["queue"]["detected_codex_version"],
        "0.168.0"
    );
    assert_eq!(st["codex_queues"][3]["persona"], "future");
    assert_eq!(st["codex_queues"][3]["queue"]["state"], "unsupported");
    assert!(st["codex_queues"][3]["queue"].get("depth").is_none());
    assert!(st["codex_queues"][3]["queue"].get("extra").is_none());

    let text_out = env.fl().args(["wire", "status"]).output().unwrap();
    peer.join().unwrap();
    std::fs::remove_file(&env.sock_file).unwrap();
    assert!(text_out.status.success());
    let text = String::from_utf8(text_out.stdout).unwrap();
    assert!(
        text.contains(
            "codex_queues=felix[ferroloop]: at least 100 queued, oldest ~42s (estimated from queue id); kacey[ferroloop]: 0 queued; ghost: unknown (unverified Codex app-server version 0.168.0 (verified 0.167.0)); future: unknown (unsupported queue state)"
        ),
        "{text}"
    );
    assert!(!text.contains("ghost: 0"), "{text}");
    assert!(!text.contains("future: 0"), "{text}");
}

#[test]
fn wire_status_active_and_inactive_daemon_fail_codex_queue_to_unknown_when_offline() {
    let env = TestEnv::new();
    let reg = env
        .fl()
        .args([
            "wire",
            "register",
            "Felix",
            "--project",
            "ferroloop",
            "--role",
            "web-developer",
        ])
        .output()
        .unwrap();
    assert!(reg.status.success());

    let mut table = fl_wire::identity::RouteTable::default();
    table.upsert_explicit(
        "felix",
        fl_wire::identity::VendorRoute::Codex {
            thread_id: "thread-e2e".into(),
            cwd: Some(env.work_dir.clone()),
        },
        fl_core::at::At::from_unix_millis(1_762_000_000_000),
    );
    table.save(Some(&env.routes_file)).unwrap();

    // 1. Inactive daemon + offline Codex control socket -> Unknown (never 0).
    let mut inactive_json_cmd = env.fl();
    inactive_json_cmd
        .env_remove("CODEX_HOME")
        .env_remove("FL_WIRE_CODEX_BIN")
        .env_remove("FL_WIRE_CODEX_CONTROL_SOCK")
        .args(["wire", "status", "--json"]);
    let inactive = run_json(inactive_json_cmd);
    assert_eq!(inactive["daemon"], "inactive");
    assert_eq!(inactive["codex_queues"][0]["persona"], "felix");
    assert_eq!(inactive["codex_queues"][0]["project"], "ferroloop");
    assert_eq!(inactive["codex_queues"][0]["queue"]["state"], "unknown");
    assert!(inactive["codex_queues"][0]["queue"].get("depth").is_none());

    let inactive_text_out = env
        .fl()
        .env_remove("CODEX_HOME")
        .env_remove("FL_WIRE_CODEX_BIN")
        .env_remove("FL_WIRE_CODEX_CONTROL_SOCK")
        .args(["wire", "status"])
        .output()
        .unwrap();
    assert!(inactive_text_out.status.success());
    let inactive_text = String::from_utf8(inactive_text_out.stdout).unwrap();
    assert!(
        inactive_text.contains("codex_queues=felix[ferroloop]: unknown ("),
        "{inactive_text}"
    );
    assert!(
        !inactive_text.contains("felix[ferroloop]: 0"),
        "{inactive_text}"
    );

    // 2. Live daemon + offline Codex control socket -> Unknown (never 0).
    let store_path = env.data_dir.join("fl/fl.redb");
    let store = std::sync::Arc::new(fl_store::open_store(&store_path).unwrap());
    let mut daemon = fl_wire::daemon::spawn_daemon(
        store,
        fl_wire::daemon::DaemonConfig {
            node_id: "fixture-node".into(),
            host: "fixture-host".into(),
            socket_path: env.sock_file.clone(),
            routes_path: Some(env.routes_file.clone()),
            home_override: Some(env.root.clone()),
            listen_addr: None,
            peers: vec![],
            psk: vec![],
        },
    )
    .unwrap();

    let mut active_json_cmd = env.fl();
    active_json_cmd.args(["wire", "status", "--json"]);
    let active = run_json(active_json_cmd);
    assert_eq!(active["node_id"], "fixture-node");
    assert_eq!(active["codex_queues"][0]["persona"], "felix");
    assert_eq!(active["codex_queues"][0]["project"], "ferroloop");
    assert_eq!(active["codex_queues"][0]["queue"]["state"], "unknown");
    assert_eq!(
        active["codex_queues"][0]["queue"]["verified_codex_version"],
        "0.167.0"
    );
    assert!(active["codex_queues"][0]["queue"].get("depth").is_none());

    let active_text_out = env.fl().args(["wire", "status"]).output().unwrap();
    daemon.stop();
    assert!(active_text_out.status.success());
    let active_text = String::from_utf8(active_text_out.stdout).unwrap();
    assert!(
        active_text.contains("daemon=active")
            && active_text.contains("codex_queues=felix[ferroloop]: unknown ("),
        "{active_text}"
    );
    assert!(
        !active_text.contains("felix[ferroloop]: 0"),
        "{active_text}"
    );
}

#[test]
fn daemon_hook_hijack_blocked_records_diagnostic_and_leaves_mail_unacked() {
    let env = TestEnv::new();
    let store_path = env.data_dir.join("fl/fl.redb");
    let store = std::sync::Arc::new(fl_store::open_store(&store_path).unwrap());

    let mut routes = fl_wire::identity::RouteTable::default();
    routes.upsert_explicit_with_meta(
        "feira",
        fl_wire::identity::VendorRoute::Agy {
            conversation_id: "conv-hosted-feira".into(),
            ls_pid: Some(std::process::id()),
            ls_address: Some("127.0.0.1:1".into()),
            csrf_token: Some("SECRET_CSRF".into()),
            agentapi_exe: Some(env.root.join("nonexistent-agy")),
        },
        fl_core::at::At::from_unix_millis(1_700_000_000_100),
        Some("antigravity:feira".into()),
        Some("gen-1".into()),
        false,
    );
    routes.save(Some(&env.routes_file)).unwrap();

    // Seed unread mail waiting in store for `feira`.
    {
        use fl_core::ferrowire::{MessageKind, MessageUrgency, SendWireArgs, WireAddress};
        use fl_core::store::MessageStore;
        store
            .send_wire(SendWireArgs {
                id: None,
                sender_id: None,
                sender_name: "felix".into(),
                sender_host: None,
                to: WireAddress::AgentName("feira".into()),
                kind: MessageKind::Mail,
                urgency: MessageUrgency::Normal,
                durable: true,
                subject: "Hosted inbox message".into(),
                body: "Must remain unacked across spike child hooks".into(),
                reply_to: None,
                thread_id: None,
                deadline_at: None,
                at: fl_core::at::At::from_unix_millis(1_700_000_000_200),
            })
            .unwrap();
    }

    let mut daemon = fl_wire::daemon::spawn_daemon(
        std::sync::Arc::clone(&store),
        fl_wire::daemon::DaemonConfig {
            node_id: "node-460".into(),
            host: "host-460".into(),
            socket_path: env.sock_file.clone(),
            routes_path: Some(env.routes_file.clone()),
            home_override: Some(env.root.clone()),
            listen_addr: None,
            peers: vec![],
            psk: vec![],
        },
    )
    .unwrap();

    let spike_sock = env.root.join("999999.sock");
    for (idx, hosted_env) in [Some("antigravity:feira"), Some("Feira.antigravity"), None]
        .into_iter()
        .enumerate()
    {
        let mut cmd = env.fl();
        cmd.args(["wire", "hook", "claude"])
            .env("AGENT_PERSONA", "feira")
            .env("CLAUDE_CODE_MESSAGING_SOCKET", &spike_sock)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(hid) = hosted_env {
            cmd.env("FL_HOSTED_ID", hid);
        }
        let mut child = cmd.spawn().unwrap();
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(
                serde_json::to_vec(&json!({
                    "hook_event_name": "SessionStart",
                    "session_id": format!("spike-sess-{idx}"),
                    "cwd": env.work_dir,
                }))
                .unwrap()
                .as_slice(),
            )
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "case {idx} exited non-zero");
        let parsed: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(parsed, json!({}), "case {idx} must return empty JSON");
    }

    // Check daemon status: route is still Agy, registration_diagnostics recorded 3 refusals,
    // and unread mail for `feira` is still unacked!
    let mut st_cmd = env.fl();
    st_cmd.args(["wire", "status", "--json"]);
    let st = run_json(st_cmd);
    assert_eq!(st["routes"].as_array().unwrap().len(), 1);
    assert_eq!(st["routes"][0]["persona"], "feira");
    assert_eq!(st["routes"][0]["route"]["kind"], "agy");
    assert_eq!(
        st["routes"][0]["route"]["conversation_id"],
        "conv-hosted-feira"
    );
    let reg_diags = st["registration_diagnostics"].as_array().unwrap();
    assert_eq!(reg_diags.len(), 2);
    assert_eq!(reg_diags[0]["outcome"], "refused");
    assert_eq!(reg_diags[0]["reason"], "foreign_hosted_env");
    assert_eq!(reg_diags[0]["count"], 2);
    assert!(reg_diags[0]["last_seen_at"].is_string());
    assert_eq!(reg_diags[1]["outcome"], "refused");
    assert_eq!(reg_diags[1]["reason"], "vendor_mismatch");
    assert_eq!(reg_diags[1]["count"], 1);

    let text_out = env.fl().args(["wire", "status"]).output().unwrap();
    assert!(text_out.status.success());
    let text = String::from_utf8(text_out.stdout).unwrap();
    assert!(
        text.contains(
            "registration_diagnostics=3(refused=3,replaced=0,latest=feira:refused:vendor_mismatch)"
        ),
        "{text}"
    );

    let mut read_cmd = env.fl();
    read_cmd.args(["wire", "read", "--for", "feira", "--json"]);
    let inbox = run_json(read_cmd);
    daemon.stop();
    let arr = inbox.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["subject"], "Hosted inbox message");
    assert_eq!(arr[0]["status"], "pending");
}

#[test]
fn dead_explicit_route_with_held_mail_surfaces_dead_explicit_route_in_fl_wire_status() {
    let mut exited = Command::new("sh").args(["-c", "exit 0"]).spawn().unwrap();
    let dead_pid = exited.id();
    let _ = exited.wait();

    let env = TestEnv::new();
    let store_path = env.data_dir.join("fl/fl.redb");
    let store = std::sync::Arc::new(fl_store::open_store(&store_path).unwrap());

    let mut routes = fl_wire::identity::RouteTable::default();
    routes.upsert_explicit_with_meta(
        "feira",
        fl_wire::identity::VendorRoute::Claude {
            pid: dead_pid,
            session_id: "claude-dead-460".into(),
            socket_path: env.root.join("missing.sock"),
            key_path: None,
        },
        fl_core::at::At::from_unix_millis(1_700_000_000_100),
        Some("claude:feira".into()),
        Some("gen-1".into()),
        false,
    );
    routes.save(Some(&env.routes_file)).unwrap();

    {
        use fl_core::ferrowire::{MessageKind, MessageUrgency, SendWireArgs, WireAddress};
        use fl_core::store::MessageStore;
        store
            .send_wire(SendWireArgs {
                id: None,
                sender_id: None,
                sender_name: "felix".into(),
                sender_host: None,
                to: WireAddress::AgentName("feira".into()),
                kind: MessageKind::Mail,
                urgency: MessageUrgency::Normal,
                durable: true,
                subject: "Held mail".into(),
                body: "Dead explicit PID".into(),
                reply_to: None,
                thread_id: None,
                deadline_at: None,
                at: fl_core::at::At::from_unix_millis(1_700_000_000_200),
            })
            .unwrap();
    }

    // 1. Inactive daemon `fl wire status` (JSON + text).
    let mut inactive_json_cmd = env.fl();
    inactive_json_cmd.args(["wire", "status", "--json"]);
    let inactive_json = run_json(inactive_json_cmd);
    assert_eq!(inactive_json["daemon"], "inactive");
    assert_eq!(
        inactive_json["pending_recipients"][0]["hold_reason"],
        "dead_explicit_route"
    );

    let inactive_text_out = env.fl().args(["wire", "status"]).output().unwrap();
    assert!(inactive_text_out.status.success());
    let inactive_text = String::from_utf8(inactive_text_out.stdout).unwrap();
    assert!(
        inactive_text.contains("hold=dead_explicit_route"),
        "{inactive_text}"
    );

    // 2. Active daemon `fl wire status` (JSON + text).
    let mut daemon = fl_wire::daemon::spawn_daemon(
        store,
        fl_wire::daemon::DaemonConfig {
            node_id: "node-dead".into(),
            host: "host-dead".into(),
            socket_path: env.sock_file.clone(),
            routes_path: Some(env.routes_file.clone()),
            home_override: Some(env.root.clone()),
            listen_addr: None,
            peers: vec![],
            psk: vec![],
        },
    )
    .unwrap();

    let mut active_json_cmd = env.fl();
    active_json_cmd.args(["wire", "status", "--json"]);
    let active_json = run_json(active_json_cmd);
    assert_eq!(
        active_json["pending_recipients"][0]["hold_reason"],
        "dead_explicit_route"
    );
    assert_eq!(
        active_json["dispatch_diagnostics"]["records"][0]["decisive_reason"],
        "dead_explicit_route"
    );

    let active_text_out = env.fl().args(["wire", "status"]).output().unwrap();
    daemon.stop();
    assert!(active_text_out.status.success());
    let active_text = String::from_utf8(active_text_out.stdout).unwrap();
    assert!(
        active_text.contains("decisive_reasons=dead_explicit_route=1")
            && active_text.contains("hold=dead_explicit_route"),
        "{active_text}"
    );
}

#[test]
fn daemon_hosted_replace_operator_replace_and_foreign_child_replace_e2e() {
    let env = TestEnv::new();
    let store_path = env.data_dir.join("fl/fl.redb");
    let store = std::sync::Arc::new(fl_store::open_store(&store_path).unwrap());
    let g1 = fl_hub::lifecycle::next_hydration_generation();
    let g2 = fl_hub::lifecycle::next_hydration_generation();

    let hosted_agy = fl_hub::lifecycle::HostedSessionRecord {
        session_id: "antigravity:feira".into(),
        persona: "feira".into(),
        project: "ferroloop".into(),
        role: "assistant".into(),
        vendor: "antigravity".into(),
        model: None,
        thinking_level: None,
        node_id: "local".into(),
        hosted: true,
        status: fl_hub::lifecycle::STATUS_RUNNING.into(),
        stop_reason: None,
        restart_policy: "on-failure".into(),
        restart_count: 0,
        tmux_target: None,
        host_session_id: Some("Feira.antigravity".into()),
        cwd: None,
        context_window: None,
        posture: fl_core::permissions::Posture::Auto,
        hydration_dispatched: true,
        hydration_status: fl_hub::lifecycle::HydrationStatus::Unconfirmed,
        hydration_generation: Some(g1.clone()),
        runtime_spawned: true,
        tombstone_ids: Vec::new(),
        vendor_session_id: None,
        pid: None,
        pid_start_time: None,
        created_at_ms: 1,
        updated_at_ms: 1,
        decommission_prepared_at_ms: None,
    };
    fl_hub::lifecycle::persist_hosted_session(&*store, &hosted_agy, "owner").unwrap();

    let mut daemon = fl_wire::daemon::spawn_daemon(
        std::sync::Arc::clone(&store),
        fl_wire::daemon::DaemonConfig {
            node_id: "node-rep".into(),
            host: "host-rep".into(),
            socket_path: env.sock_file.clone(),
            routes_path: Some(env.routes_file.clone()),
            home_override: Some(env.root.clone()),
            listen_addr: None,
            peers: vec![],
            psk: vec![],
        },
    )
    .unwrap();

    // 1. Hosted session's own `--replace` succeeds.
    let reg1 = env
        .fl()
        .args([
            "wire",
            "register",
            "Feira",
            "--project",
            "ferroloop",
            "--role",
            "assistant",
            "--replace",
        ])
        .env("AGENT_PERSONA", "feira")
        .env("FL_HOSTED_ID", "antigravity:feira")
        .env("AGENT_HYDRATION_GENERATION", &g1)
        .env("ANTIGRAVITY_CONVERSATION_ID", "conv-e2e-g1")
        .output()
        .unwrap();
    assert!(
        reg1.status.success(),
        "{:?}",
        String::from_utf8_lossy(&reg1.stderr)
    );

    // 2. Foreign-vendor child with `FL_HOSTED_ID=antigravity:feira` attempting `claude --replace` is refused!
    let spike_sock = env.root.join(format!("{}.sock", std::process::id()));
    let foreign = env
        .fl()
        .args([
            "wire",
            "register",
            "Feira",
            "--project",
            "ferroloop",
            "--role",
            "assistant",
            "--replace",
        ])
        .env("AGENT_PERSONA", "feira")
        .env("FL_HOSTED_ID", "antigravity:feira")
        .env("AGENT_HYDRATION_GENERATION", &g1)
        .env("CLAUDE_CODE_MESSAGING_SOCKET", &spike_sock)
        .env("CLAUDE_SESSION_ID", "spike-child-replace")
        .output()
        .unwrap();
    assert!(!foreign.status.success());

    // 3. Remodel to `claude` with `g2` succeeds and records `Replaced`.
    let mut remodeled = hosted_agy;
    remodeled.session_id = "claude:feira".into();
    remodeled.vendor = "claude".into();
    remodeled.hydration_status = fl_hub::lifecycle::HydrationStatus::Unconfirmed;
    remodeled.hydration_generation = Some(g2.clone());
    fl_hub::lifecycle::persist_hosted_session(&*store, &remodeled, "owner").unwrap();

    let reg2 = env
        .fl()
        .args([
            "wire",
            "register",
            "Feira",
            "--project",
            "ferroloop",
            "--role",
            "assistant",
            "--replace",
        ])
        .env("AGENT_PERSONA", "feira")
        .env("FL_HOSTED_ID", "claude:feira")
        .env("AGENT_HYDRATION_GENERATION", &g2)
        .env("CLAUDE_CODE_MESSAGING_SOCKET", &spike_sock)
        .env("CLAUDE_SESSION_ID", "claude-e2e-g2")
        .output()
        .unwrap();
    assert!(
        reg2.status.success(),
        "{:?}",
        String::from_utf8_lossy(&reg2.stderr)
    );

    // 4. Operator `--replace` (no `FL_HOSTED_ID`) switches to Codex and records `Replaced`.
    let op_reg = env
        .fl()
        .args([
            "wire",
            "register",
            "Feira",
            "--project",
            "ferroloop",
            "--role",
            "assistant",
            "--replace",
        ])
        .env("CODEX_THREAD_ID", "codex-op-e2e")
        .output()
        .unwrap();
    assert!(
        op_reg.status.success(),
        "{:?}",
        String::from_utf8_lossy(&op_reg.stderr)
    );

    let mut st_cmd = env.fl();
    st_cmd.args(["wire", "status", "--json"]);
    let st = run_json(st_cmd);
    daemon.stop();
    assert_eq!(st["routes"][0]["route"]["kind"], "codex");
    assert_eq!(st["routes"][0]["route"]["thread_id"], "codex-op-e2e");
    let diags = st["registration_diagnostics"].as_array().unwrap();
    assert!(
        diags
            .iter()
            .any(|d| d["outcome"] == "refused" && d["reason"] == "foreign_hosted_env"),
        "{diags:?}"
    );
    assert!(
        diags.iter().filter(|d| d["outcome"] == "replaced").count() >= 2,
        "{diags:?}"
    );
}

#[cfg(unix)]
#[test]
fn mcp_and_cli_discover_daemon_via_run_user_fallback_without_xdg_runtime_dir_and_fanout_agy_persona()
 {
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};

    let env = TestEnv::new();
    let store_path = env.data_dir.join("fl/fl.redb");
    let run_user_dir = env.root.join("run-user-synth");
    std::fs::create_dir_all(&run_user_dir).unwrap();
    let sock_in_run_user = run_user_dir.join("fl-wire.sock");
    let routes_in_run_user = run_user_dir.join("fl-wire-routes.json");

    // Write a config without `[wire] socket = ...` so the CLI/MCP must resolve the socket via `/run/user/<uid>` fallback.
    std::fs::write(
        env.cfg_dir.join("fl/config.toml"),
        format!("[state]\nstore = \"{}\"\n", store_path.display()),
    )
    .unwrap();

    let agy_log = env.root.join("agy-invocations.log");
    let fake_agy = env.root.join("fake-agy.sh");
    std::fs::write(
        &fake_agy,
        format!(
            "#!/bin/sh\necho \"$@\" >> \"{}\"\nexit 0\n",
            agy_log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake_agy, std::fs::Permissions::from_mode(0o755)).unwrap();

    // Register felicia and felix in the store and seed `routes_in_run_user` with a bare-UUID Agy route for felicia.
    let mut reg_felicia = env.fl();
    reg_felicia.args([
        "wire",
        "register",
        "felicia",
        "--project",
        "ferroloop",
        "--role",
        "assistant",
    ]);
    assert!(reg_felicia.output().unwrap().status.success());

    let mut reg_felix = env.fl();
    reg_felix.args([
        "wire",
        "register",
        "felix",
        "--project",
        "ferroloop",
        "--role",
        "assistant",
    ]);
    assert!(reg_felix.output().unwrap().status.success());

    let mut table = fl_wire::identity::RouteTable::default();
    table.upsert_explicit(
        "felicia",
        fl_wire::identity::VendorRoute::Agy {
            conversation_id: "019d0000-0000-7000-8000-00000000beef".into(),
            ls_pid: None,
            ls_address: None,
            csrf_token: None,
            agentapi_exe: Some(fake_agy),
        },
        fl_core::At::from_unix_millis(100),
    );
    table.save(Some(&routes_in_run_user)).unwrap();

    // Start daemon at `<run_user_dir>/fl-wire.sock` with `<run_user_dir>/fl-wire-routes.json`.
    let store = std::sync::Arc::new(fl_store::open_store(&store_path).unwrap());
    let mut daemon = fl_wire::daemon::spawn_daemon(
        store,
        fl_wire::daemon::DaemonConfig {
            node_id: "node-run-user".into(),
            host: "host-run-user".into(),
            socket_path: sock_in_run_user.clone(),
            routes_path: Some(routes_in_run_user.clone()),
            home_override: Some(env.root.clone()),
            listen_addr: None,
            peers: vec![],
            psk: vec![],
        },
    )
    .unwrap();

    // 1. `fl wire status --json` with `XDG_RUNTIME_DIR`, `FL_WIRE_SOCKET`, and `FL_WIRE_ROUTES` UNSET
    //    (and `--db` omitted, discovering `/run/user/<uid>` via `FL_TEST_RUN_USER_DIR`).
    let status_out = Command::new(env!("CARGO_BIN_EXE_fl"))
        .current_dir(&env.work_dir)
        .env("HOME", &env.root)
        .env("XDG_CONFIG_HOME", &env.cfg_dir)
        .env("XDG_DATA_HOME", &env.data_dir)
        .env_remove("XDG_RUNTIME_DIR")
        .env_remove("FL_WIRE_SOCKET")
        .env_remove("FL_WIRE_ROUTES")
        .env("FL_TEST_RUN_USER_DIR", &run_user_dir)
        .env("FL_WIRE_DISABLE_HOST_DISCOVERY", "1")
        .env_remove("AGENT_PERSONA")
        .env_remove("AGENT_NAME")
        .env_remove("FL_HOSTED_ID")
        .env_remove("AGENT_HYDRATION_GENERATION")
        .args(["wire", "status", "--json"])
        .output()
        .unwrap();
    assert!(
        status_out.status.success(),
        "{}",
        String::from_utf8_lossy(&status_out.stderr)
    );
    let st: Value = serde_json::from_slice(&status_out.stdout).unwrap();
    assert_eq!(
        st["node_id"].as_str(),
        Some("node-run-user"),
        "CLI status without XDG_RUNTIME_DIR must reach the active daemon via /run/user/<uid> fallback: {st}"
    );

    // 2. `fl wire mcp` (`send_message` and `broadcast_message`) with `XDG_RUNTIME_DIR` UNSET
    //    must connect to the daemon at `<run_user_dir>/fl-wire.sock`, deliver through the daemon's
    //    loaded route table, and push both direct send and `@all` broadcast fan-out!
    let mut mcp_child = Command::new(env!("CARGO_BIN_EXE_fl"))
        .current_dir(&env.work_dir)
        .env("HOME", &env.root)
        .env("XDG_CONFIG_HOME", &env.cfg_dir)
        .env("XDG_DATA_HOME", &env.data_dir)
        .env_remove("XDG_RUNTIME_DIR")
        .env_remove("FL_WIRE_SOCKET")
        .env_remove("FL_WIRE_ROUTES")
        .env("FL_TEST_RUN_USER_DIR", &run_user_dir)
        .env("FL_WIRE_DISABLE_HOST_DISCOVERY", "1")
        .env("AGENT_PERSONA", "felix")
        .env_remove("AGENT_NAME")
        .env_remove("FL_HOSTED_ID")
        .env_remove("AGENT_HYDRATION_GENERATION")
        .args(["wire", "mcp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let stdin = mcp_child.stdin.as_mut().unwrap();
        let reqs = [
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "send_message",
                    "arguments": {
                        "to": "felicia",
                        "subject": "direct-via-mcp",
                        "body": "hello direct"
                    }
                }
            }),
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "broadcast_message",
                    "arguments": {
                        "subject": "group-via-mcp",
                        "body": "hello broadcast"
                    }
                }
            }),
        ];
        for r in &reqs {
            writeln!(stdin, "{r}").unwrap();
        }
    }
    let mcp_out = mcp_child.wait_with_output().unwrap();
    assert!(mcp_out.status.success());

    let deadline = Instant::now() + Duration::from_secs(3);
    let mut invocations = String::new();
    while Instant::now() < deadline {
        invocations = std::fs::read_to_string(&agy_log).unwrap_or_default();
        if invocations.matches("agentapi send-message").count() >= 2 {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    daemon.stop();

    assert_eq!(
        invocations.matches("agentapi send-message").count(),
        2,
        "expected both direct MCP send and broadcast MCP send to push via daemon Agy route, got: {invocations}"
    );
    assert!(
        invocations.contains("direct-via-mcp") && invocations.contains("group-via-mcp"),
        "expected both direct and broadcast subjects in Agy invocations: {invocations}"
    );
    assert!(
        invocations.contains("019d0000-0000-7000-8000-00000000beef"),
        "expected Agy conversation_id in invocation: {invocations}"
    );
}

#[cfg(unix)]
#[test]
fn cli_queues_when_daemon_unreachable_and_daemon_delivers_on_startup() {
    use fl_core::MessageStore;
    use std::io::Read;
    use std::os::unix::net::UnixListener;
    use std::time::{Duration, Instant};

    let env = TestEnv::new();
    let store_path = env.data_dir.join("fl/fl.redb");
    let sock = env.root.join("delayed-daemon.sock");
    let hook_sock = env.root.join("felicia-hook.sock");
    let listener = UnixListener::bind(&hook_sock).unwrap();
    listener.set_nonblocking(true).unwrap();

    // Register felicia in store and save her webhook route in routes.json.
    let mut reg_cmd = env.fl();
    reg_cmd.args([
        "wire",
        "register",
        "felicia",
        "--project",
        "ferroloop",
        "--role",
        "assistant",
    ]);
    assert!(reg_cmd.output().unwrap().status.success());

    let mut table = fl_wire::identity::RouteTable::default();
    table.upsert(
        "felicia",
        fl_wire::identity::VendorRoute::Webhook {
            socket_path: hook_sock,
        },
        fl_core::At::from_unix_millis(100),
    );
    table.save(Some(&env.routes_file)).unwrap();

    // 0. If the store write fails while the daemon is unreachable, the client returns an error
    //    and never claims it queued anything.
    let mut bad_send = env.fl();
    bad_send.env("FL_WIRE_SOCKET", &sock).args([
        "wire",
        "send",
        "@nonexistent-group",
        "bad-subj",
        "bad-body",
        "--from",
        "felix",
    ]);
    let bad_out = bad_send.output().unwrap();
    assert!(!bad_out.status.success());
    let bad_stderr = String::from_utf8_lossy(&bad_out.stderr);
    assert!(
        !bad_stderr.contains("queued, the daemon will deliver"),
        "failed store write must not claim message was queued: {bad_stderr}"
    );

    // 1. Send with no daemon running at `sock` (and without `--no-daemon`):
    //    must queue for the daemon (`Pending`, `attempts == 0`), print notice on stderr, and NOT push in-process.
    let mut send_cmd = env.fl();
    send_cmd.env("FL_WIRE_SOCKET", &sock).args([
        "wire",
        "send",
        "felicia",
        "queued-subj",
        "queued-body",
        "--from",
        "felix",
        "--json",
    ]);
    let send_out = send_cmd.output().unwrap();
    assert!(
        send_out.status.success(),
        "{}",
        String::from_utf8_lossy(&send_out.stderr)
    );
    let stderr_text = String::from_utf8_lossy(&send_out.stderr);
    assert!(
        stderr_text.contains("queued, the daemon will deliver"),
        "expected queue notice on stderr, got: {stderr_text}"
    );
    let sent_json: Value = serde_json::from_slice(&send_out.stdout).unwrap();
    assert_eq!(sent_json["receipts"][0]["status"], "pending");
    assert_eq!(sent_json["receipts"][0]["attempts"], 0);
    assert!(
        listener.accept().is_err(),
        "CLI send without --no-daemon must not dispatch in-process"
    );

    // 2. Start the daemon at `sock`: it drains the queued message and delivers it to `felicia`!
    let store = std::sync::Arc::new(fl_store::open_store(&store_path).unwrap());
    let mut daemon = fl_wire::daemon::spawn_daemon(
        std::sync::Arc::clone(&store),
        fl_wire::daemon::DaemonConfig {
            node_id: "node-delayed".into(),
            host: "host-delayed".into(),
            socket_path: sock.clone(),
            routes_path: Some(env.routes_file.clone()),
            home_override: Some(env.root.clone()),
            listen_addr: None,
            peers: vec![],
            psk: vec![],
        },
    )
    .unwrap();

    let deadline = Instant::now() + Duration::from_secs(3);
    let mut delivered = None;
    while Instant::now() < deadline && delivered.is_none() {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut s = String::new();
            let _ = stream.read_to_string(&mut s);
            delivered = Some(s);
        } else {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    let payload = delivered.expect("daemon must pick up and deliver queued message");
    assert!(payload.contains("queued-subj"));
    assert!(store.list_wire_awaiting_daemon().unwrap().is_empty());

    // 3. While the daemon is running at `sock`, a client pointing at the WRONG socket path
    //    (`wrong-client.sock`) writes to the shared store and the running daemon picks it up
    //    via the store-backed awaiting-daemon marker and delivers it!
    let wrong_sock = env.root.join("wrong-client.sock");
    let mut wrong_sock_cmd = env.fl();
    wrong_sock_cmd.env("FL_WIRE_SOCKET", &wrong_sock).args([
        "wire",
        "send",
        "felicia",
        "wrong-sock-subj",
        "wrong-sock-body",
        "--from",
        "felix",
        "--json",
    ]);
    let wrong_out = wrong_sock_cmd.output().unwrap();
    assert!(
        wrong_out.status.success(),
        "{}",
        String::from_utf8_lossy(&wrong_out.stderr)
    );
    let wrong_stderr = String::from_utf8_lossy(&wrong_out.stderr);
    assert!(
        wrong_stderr.contains("queued, the daemon will deliver"),
        "expected queue notice on stderr, got: {wrong_stderr}"
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut delivered_wrong_sock = None;
    while Instant::now() < deadline && delivered_wrong_sock.is_none() {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut s = String::new();
            let _ = stream.read_to_string(&mut s);
            delivered_wrong_sock = Some(s);
        } else {
            std::thread::sleep(Duration::from_millis(25));
        }
    }
    daemon.stop();
    let payload_2 = delivered_wrong_sock
        .expect("running daemon must pick up message from client with wrong socket path");
    assert!(payload_2.contains("wrong-sock-subj"));
    assert!(store.list_wire_awaiting_daemon().unwrap().is_empty());
}
