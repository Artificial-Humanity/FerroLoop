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
        std::fs::write(work_dir.join(".agents/REGISTRY.md"), "# placeholder\n").unwrap();

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

    // Register three agents: ferris (ferroloop lead), kayla (kaggle-gemma4 lead), kacey (kaggle-gemma4 scribe)
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

    // Verify .agents/REGISTRY.md was updated automatically by agent-msg register
    let reg_md = std::fs::read_to_string(env.work_dir.join(".agents/REGISTRY.md")).unwrap();
    assert!(reg_md.contains("Ferris") && reg_md.contains("custom:scribe"));

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

    // Area+role selector send: @area:kaggle-gemma4/scribe -> only kacey
    let mut role_cmd = env.fl();
    role_cmd.args([
        "wire",
        "send",
        "@area:kaggle-gemma4/scribe",
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
    let md = std::fs::read_to_string(&registry).unwrap();
    assert!(
        md.contains("**Ferris**") && md.contains("**Kayla**"),
        "{md}"
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
    let md = std::fs::read_to_string(&registry).unwrap();
    assert!(md.contains("**Ferris**"), "{md}");
    assert!(
        !md.contains("**Kayla**"),
        "a retired agent must leave the active table: {md}"
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
