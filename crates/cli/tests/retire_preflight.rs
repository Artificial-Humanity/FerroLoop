//! Black-box coverage of wire endpoint preflights at the actual CLI entry.
//! These tests run in CI; they construct only private stores and endpoints.
#![cfg(unix)]

#[allow(dead_code)]
mod common;

use fl_core::agent::{AgentRole, AgentStatus, RegisterAgentArgs};
use fl_core::at::At;
use fl_core::store::AgentRegistry;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::net::UnixListener;
use std::path::Path;

#[derive(Clone, Copy)]
enum Invocation {
    Retire,
    ClaudeHook,
    LegacySubcommand,
    LegacyExecutable,
    WireStatus,
    WireDaemon,
    WireRegister,
    LegacyRegister,
    LegacyExecutableRegister,
    WireSend,
    WireBroadcast,
    WireBroadcastNoDaemon,
    LegacyBroadcast,
    LegacyExecutableBroadcast,
    WireAsk,
    WireAskNoDaemon,
    WireReply,
    WireReplyNoDaemon,
    LegacySend,
    LegacyExecutableSend,
    WireRead,
    LegacyRead,
    LegacyExecutableRead,
    WireAck,
    LegacyAck,
    LegacyExecutableAck,
    WireWait,
    LegacyWait,
    LegacyExecutableWait,
    LegacyListen,
    LegacyExecutableListen,
}

fn run_refused_command(root: &Path, outside_socket: &Path, db: &Path, invocation: Invocation) {
    let mut command = if matches!(invocation, Invocation::ClaudeHook) {
        let alias = root.join("agent-bus-claude-hook");
        if !alias.exists() {
            let binary = common::isolated_fl(root).unwrap();
            symlink(binary.get_program(), &alias).unwrap();
        }
        common::isolated_command(alias, root).unwrap()
    } else if matches!(
        invocation,
        Invocation::LegacyExecutable
            | Invocation::LegacyExecutableRegister
            | Invocation::LegacyExecutableSend
            | Invocation::LegacyExecutableBroadcast
            | Invocation::LegacyExecutableRead
            | Invocation::LegacyExecutableAck
            | Invocation::LegacyExecutableWait
            | Invocation::LegacyExecutableListen
    ) {
        let alias = root.join("agent-msg");
        if !alias.exists() {
            let binary = common::isolated_fl(root).unwrap();
            symlink(binary.get_program(), &alias).unwrap();
        }
        common::isolated_command(alias, root).unwrap()
    } else {
        common::isolated_fl(root).unwrap()
    };
    command
        .current_dir(root)
        .env("FL_TEST_ISOLATION_ROOT", root)
        .env("FL_WIRE_SOCKET", outside_socket)
        .env("FL_TEST_SPAWN_MARKER", root.join("vendor-spawned"));
    if matches!(
        invocation,
        Invocation::WireRegister
            | Invocation::LegacyRegister
            | Invocation::LegacyExecutableRegister
            | Invocation::WireSend
            | Invocation::WireBroadcast
            | Invocation::WireBroadcastNoDaemon
            | Invocation::LegacyBroadcast
            | Invocation::LegacyExecutableBroadcast
            | Invocation::WireAsk
            | Invocation::WireAskNoDaemon
            | Invocation::WireReply
            | Invocation::WireReplyNoDaemon
            | Invocation::LegacySend
            | Invocation::LegacyExecutableSend
            | Invocation::WireRead
            | Invocation::LegacyRead
            | Invocation::LegacyExecutableRead
            | Invocation::WireAck
            | Invocation::LegacyAck
            | Invocation::LegacyExecutableAck
            | Invocation::WireWait
            | Invocation::LegacyWait
            | Invocation::LegacyExecutableWait
            | Invocation::LegacyListen
            | Invocation::LegacyExecutableListen
    ) {
        // Exercise the hydration and route-capture entry path; refusal must
        // happen before either can open a store or touch a route.
        command
            .env("AGENT_HYDRATION_GENERATION", "fixture-generation")
            .env("CLAUDE_SESSION_ID", "fixture-session");
    }
    match invocation {
        Invocation::ClaudeHook => {
            command
                .env("FL_DB", db)
                .env("CLAUDE_CODE_MESSAGING_SOCKET", root.join("12345.sock"))
                .stdin(std::process::Stdio::piped());
        }
        Invocation::Retire => {
            command.args([
                "--db",
                db.to_str().unwrap(),
                "agent",
                "retire",
                "felicia",
                "--by",
                "owner",
            ]);
        }
        Invocation::LegacySubcommand => {
            command.args([
                "agent-msg",
                "--db",
                db.to_str().unwrap(),
                "unregister",
                "felicia",
            ]);
        }
        Invocation::LegacyExecutable => {
            command.args(["--db", db.to_str().unwrap(), "unregister", "felicia"]);
        }
        Invocation::WireStatus => {
            command.args(["--db", db.to_str().unwrap(), "wire", "status", "--json"]);
        }
        Invocation::WireDaemon => {
            // Missing token makes the ordering observable: the socket refusal
            // must occur before a token read would return a different error.
            command.args([
                "--db",
                db.to_str().unwrap(),
                "wire",
                "daemon",
                "--token-file",
                root.join("missing-token").to_str().unwrap(),
            ]);
        }
        Invocation::WireRegister => {
            command.args([
                "--db",
                db.to_str().unwrap(),
                "wire",
                "register",
                "Felicia",
                "--project",
                "ferroloop",
                "--role",
                "assistant",
                "--replace",
            ]);
        }
        Invocation::LegacyRegister => {
            command.args([
                "agent-msg",
                "--db",
                db.to_str().unwrap(),
                "register",
                "Felicia",
                "--project",
                "ferroloop",
                "--role",
                "assistant",
                "--replace",
            ]);
        }
        Invocation::LegacyExecutableRegister => {
            command.args([
                "--db",
                db.to_str().unwrap(),
                "register",
                "Felicia",
                "--project",
                "ferroloop",
                "--role",
                "assistant",
                "--replace",
            ]);
        }
        Invocation::WireSend => {
            command.args([
                "--db",
                db.to_str().unwrap(),
                "wire",
                "send",
                "felicia",
                "subject",
                "body",
            ]);
        }
        Invocation::WireBroadcast | Invocation::WireBroadcastNoDaemon => {
            command.args([
                "--db",
                db.to_str().unwrap(),
                "wire",
                "broadcast",
                "subject",
                "body",
            ]);
            if matches!(invocation, Invocation::WireBroadcastNoDaemon) {
                command.arg("--no-daemon");
            }
        }
        Invocation::LegacyBroadcast => {
            command.args([
                "agent-msg",
                "--db",
                db.to_str().unwrap(),
                "broadcast",
                "subject",
                "body",
            ]);
        }
        Invocation::LegacyExecutableBroadcast => {
            command.args(["--db", db.to_str().unwrap(), "broadcast", "subject", "body"]);
        }
        Invocation::WireAsk | Invocation::WireAskNoDaemon => {
            command.args([
                "--db",
                db.to_str().unwrap(),
                "wire",
                "ask",
                "felicia",
                "subject",
                "body",
                "--timeout",
                "1",
            ]);
            if matches!(invocation, Invocation::WireAskNoDaemon) {
                command.arg("--no-daemon");
            }
        }
        Invocation::WireReply | Invocation::WireReplyNoDaemon => {
            command.args([
                "--db",
                db.to_str().unwrap(),
                "wire",
                "reply",
                "01a1db00-5252-7726-bef6-1d442d8e2d39",
                "body",
            ]);
            if matches!(invocation, Invocation::WireReplyNoDaemon) {
                command.arg("--no-daemon");
            }
        }
        Invocation::LegacySend => {
            command.args([
                "agent-msg",
                "--db",
                db.to_str().unwrap(),
                "send",
                "felicia",
                "subject",
                "body",
            ]);
        }
        Invocation::LegacyExecutableSend => {
            command.args([
                "--db",
                db.to_str().unwrap(),
                "send",
                "felicia",
                "subject",
                "body",
            ]);
        }
        Invocation::WireRead => {
            command.args(["--db", db.to_str().unwrap(), "wire", "read", "--ack"]);
        }
        Invocation::LegacyRead => {
            command.args(["agent-msg", "--db", db.to_str().unwrap(), "read", "--ack"]);
        }
        Invocation::LegacyExecutableRead => {
            command.args(["--db", db.to_str().unwrap(), "read", "--ack"]);
        }
        Invocation::WireWait => {
            command.args(["--db", db.to_str().unwrap(), "wire", "wait"]);
        }
        Invocation::LegacyWait => {
            command.args(["agent-msg", "--db", db.to_str().unwrap(), "wait"]);
        }
        Invocation::LegacyExecutableWait => {
            command.args(["--db", db.to_str().unwrap(), "wait"]);
        }
        Invocation::LegacyListen => {
            command.args(["agent-msg", "--db", db.to_str().unwrap(), "listen"]);
        }
        Invocation::LegacyExecutableListen => {
            command.args(["--db", db.to_str().unwrap(), "listen"]);
        }
        Invocation::WireAck => {
            command.args([
                "--db",
                db.to_str().unwrap(),
                "wire",
                "ack",
                "01a1db00-5252-7726-bef6-1d442d8e2d39",
            ]);
        }
        Invocation::LegacyAck => {
            command.args([
                "agent-msg",
                "--db",
                db.to_str().unwrap(),
                "ack",
                "01a1db00-5252-7726-bef6-1d442d8e2d39",
            ]);
        }
        Invocation::LegacyExecutableAck => {
            command.args([
                "--db",
                db.to_str().unwrap(),
                "ack",
                "01a1db00-5252-7726-bef6-1d442d8e2d39",
            ]);
        }
    }
    let output = if matches!(invocation, Invocation::ClaudeHook) {
        use std::io::Write;
        let mut child = command
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(br#"{"session_id":"fixture-session"}"#)
            .unwrap();
        child.wait_with_output().unwrap()
    } else {
        command.output().unwrap()
    };
    assert!(!output.status.success(), "command unexpectedly succeeded");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("fixture endpoint refused"), "{stderr}");
}

// CI-only: the private fixtures are the only sockets this child may see.
#[test]
fn wire_daemon_outside_socket_refuses_before_token_store_or_socket_io() {
    assert_outside_socket_refusal(Invocation::WireDaemon);
}

// An explicit socket must be checked even when the environment default is private.
#[test]
fn wire_daemon_checks_explicit_socket_before_token_read() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let socket = outside.path().join("wire.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let db = root.path().join("missing-store").join("fl.redb");
    let token = root.path().join("missing-token");
    let output = common::isolated_fl(root.path())
        .unwrap()
        .env("FL_TEST_ISOLATION_ROOT", root.path())
        .args([
            "--db",
            db.to_str().unwrap(),
            "wire",
            "daemon",
            "--socket",
            socket.to_str().unwrap(),
            "--token-file",
            token.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("fixture endpoint refused"), "{stderr}");
    assert!(!root.path().join("missing-store").exists());
    assert!(socket.exists(), "existing socket was unlinked");
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

// A private endpoint passes preflight and reaches the ordinary token error.
// No daemon is spawned and no store is opened in this case.
#[test]
fn wire_daemon_private_socket_reaches_token_read() {
    let root = tempfile::tempdir().unwrap();
    let db = root.path().join("missing-store").join("fl.redb");
    let token = root.path().join("missing-token");
    let output = common::isolated_fl(root.path())
        .unwrap()
        .env("FL_TEST_ISOLATION_ROOT", root.path())
        .args([
            "--db",
            db.to_str().unwrap(),
            "wire",
            "daemon",
            "--token-file",
            token.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("could not read"), "{stderr}");
    assert!(stderr.contains("missing-token"), "{stderr}");
    assert!(!root.path().join("missing-store").exists());
    assert!(!root.path().join("w").exists());
}

#[test]
fn claude_hook_outside_socket_refuses_before_store_or_route_mutation() {
    assert_outside_socket_refusal(Invocation::ClaudeHook);
}

#[test]
fn claude_hook_noop_inputs_do_not_require_wire() {
    use std::io::Write;
    for input in [
        "",
        r#"{"stop_hook_active":true}"#,
        r#"{"vendor":"grok","session_id":"headless"}"#,
    ] {
        let root = tempfile::tempdir().unwrap();
        let summary = root.path().join(".grok/sessions/headless/summary.json");
        std::fs::create_dir_all(summary.parent().unwrap()).unwrap();
        std::fs::write(&summary, r#"{"session_kind":"headless"}"#).unwrap();
        let outside = tempfile::tempdir().unwrap();
        let socket = outside.path().join("wire.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let alias = root.path().join("agent-bus-claude-hook");
        symlink(
            common::isolated_fl(root.path()).unwrap().get_program(),
            &alias,
        )
        .unwrap();
        let missing = root.path().join("not-created/fl.redb");
        let mut child = common::isolated_command(alias, root.path())
            .unwrap()
            .current_dir(root.path())
            .env("FL_TEST_ISOLATION_ROOT", root.path())
            .env("FL_WIRE_SOCKET", &socket)
            .env("FL_DB", &missing)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap(),
            serde_json::json!({})
        );
        assert!(!missing.parent().unwrap().exists());
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
}

#[test]
fn retire_outside_socket_refuses_before_store_creation_or_existing_state_mutation() {
    assert_outside_socket_refusal(Invocation::Retire);
}

#[test]
fn legacy_unregister_outside_socket_refuses_before_store_creation_or_existing_state_mutation() {
    assert_outside_socket_refusal(Invocation::LegacySubcommand);
}

#[test]
fn legacy_executable_unregister_refuses_before_store_creation_or_existing_state_mutation() {
    assert_outside_socket_refusal(Invocation::LegacyExecutable);
}

#[test]
fn wire_status_outside_socket_refuses_before_store_or_route_fallback() {
    assert_outside_socket_refusal(Invocation::WireStatus);
}

#[test]
fn wire_register_refuses_before_store_or_route_mutation() {
    assert_outside_socket_refusal(Invocation::WireRegister);
}

#[test]
fn legacy_register_refuses_before_store_or_route_mutation() {
    assert_outside_socket_refusal(Invocation::LegacyRegister);
}

#[test]
fn legacy_executable_register_refuses_before_store_or_route_mutation() {
    assert_outside_socket_refusal(Invocation::LegacyExecutableRegister);
}

#[test]
fn wire_send_refuses_before_store_or_route_mutation() {
    assert_outside_socket_refusal(Invocation::WireSend);
}

#[test]
fn messaging_commands_refuse_before_store_or_route_mutation() {
    for invocation in [
        Invocation::WireBroadcast,
        Invocation::WireBroadcastNoDaemon,
        Invocation::LegacyBroadcast,
        Invocation::LegacyExecutableBroadcast,
        Invocation::WireAsk,
        Invocation::WireAskNoDaemon,
        Invocation::WireReply,
        Invocation::WireReplyNoDaemon,
    ] {
        assert_outside_socket_refusal(invocation);
    }
}

// CI-only: all processes and the offline endpoint stay under the private root.
#[test]
fn messaging_commands_keep_private_offline_behavior() {
    let root = tempfile::tempdir().unwrap();
    let db = root.path().join("offline.redb");
    let run = |args: &[&str]| {
        let output = common::isolated_fl(root.path())
            .unwrap()
            .current_dir(root.path())
            .env("FL_TEST_ISOLATION_ROOT", root.path())
            .env("FL_WIRE_SOCKET", root.path().join("missing.sock"))
            .args(["--db", db.to_str().unwrap(), "wire"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()
    };
    let broadcast = run(&["broadcast", "subject", "body", "--from", "felix", "--json"]);
    assert_eq!(broadcast["message"]["subject"], "subject");
    let ask = run(&[
        "ask", "felicia", "question", "body", "--from", "felix", "--json",
    ]);
    let ask_id = ask["message"]["id"].as_str().unwrap();
    let reply = run(&["reply", ask_id, "answer", "--from", "felicia", "--json"]);
    assert_eq!(reply["message"]["body"], "answer");
}

#[test]
fn legacy_send_refuses_before_store_or_route_mutation() {
    assert_outside_socket_refusal(Invocation::LegacySend);
}

#[test]
fn legacy_executable_send_refuses_before_store_or_route_mutation() {
    assert_outside_socket_refusal(Invocation::LegacyExecutableSend);
}

#[test]
fn wire_read_refuses_before_store_or_route_mutation() {
    assert_outside_socket_refusal(Invocation::WireRead);
}

#[test]
fn legacy_read_refuses_before_store_or_route_mutation() {
    assert_outside_socket_refusal(Invocation::LegacyRead);
}

#[test]
fn legacy_executable_read_refuses_before_store_or_route_mutation() {
    assert_outside_socket_refusal(Invocation::LegacyExecutableRead);
}

#[test]
fn wire_ack_refuses_before_store_or_route_mutation() {
    assert_outside_socket_refusal(Invocation::WireAck);
}

#[test]
fn legacy_ack_refuses_before_store_or_route_mutation() {
    assert_outside_socket_refusal(Invocation::LegacyAck);
}

#[test]
fn legacy_executable_ack_refuses_before_store_or_route_mutation() {
    assert_outside_socket_refusal(Invocation::LegacyExecutableAck);
}

#[test]
fn wire_wait_refuses_before_store_or_route_mutation() {
    assert_outside_socket_refusal(Invocation::WireWait);
}

#[test]
fn legacy_wait_refuses_before_store_or_route_mutation() {
    assert_outside_socket_refusal(Invocation::LegacyWait);
}

#[test]
fn legacy_executable_wait_refuses_before_store_or_route_mutation() {
    assert_outside_socket_refusal(Invocation::LegacyExecutableWait);
}

#[test]
fn legacy_listen_refuses_before_store_or_route_mutation() {
    assert_outside_socket_refusal(Invocation::LegacyListen);
}

#[test]
fn legacy_executable_listen_refuses_before_store_or_route_mutation() {
    assert_outside_socket_refusal(Invocation::LegacyExecutableListen);
}

// Listen is intentionally long-running. A bounded child verifies that an authorized
// offline endpoint still reaches the local-store polling path rather than refusing.
#[test]
fn legacy_listen_private_offline_socket_keeps_polling() {
    use std::time::{Duration, Instant};

    let root = tempfile::tempdir().unwrap();
    let db = root.path().join("offline.redb");
    let mut child = common::isolated_fl(root.path())
        .unwrap()
        .current_dir(root.path())
        .env("FL_TEST_ISOLATION_ROOT", root.path())
        .env("FL_WIRE_SOCKET", root.path().join("missing.sock"))
        .args([
            "agent-msg",
            "--db",
            db.to_str().unwrap(),
            "--from",
            "felicia",
            "listen",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut exited = false;
    while !db.exists() && Instant::now() < deadline {
        if child.try_wait().unwrap().is_some() {
            exited = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    // Let at least one polling interval elapse, then always reap the child.
    if !exited && db.exists() {
        std::thread::sleep(Duration::from_millis(400));
        exited = child.try_wait().unwrap().is_some();
    }
    if !exited {
        child.kill().unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        db.exists(),
        "offline store was not opened: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !exited,
        "listen stopped polling: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn wire_wait_private_offline_socket_keeps_nonblocking_behavior() {
    let root = tempfile::tempdir().unwrap();
    let db = root.path().join("offline.redb");
    let output = common::isolated_fl(root.path())
        .unwrap()
        .current_dir(root.path())
        .env("FL_TEST_ISOLATION_ROOT", root.path())
        .env("FL_WIRE_SOCKET", root.path().join("missing.sock"))
        .args([
            "--db",
            db.to_str().unwrap(),
            "wire",
            "wait",
            "--for",
            "felicia",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["status"], "dispatcher_active");
    assert_eq!(result["persona"], "felicia");
}

#[test]
fn wire_ack_private_offline_socket_uses_store_normally() {
    let root = tempfile::tempdir().unwrap();
    let db = root.path().join("offline.redb");
    let output = common::isolated_fl(root.path())
        .unwrap()
        .current_dir(root.path())
        .env("FL_TEST_ISOLATION_ROOT", root.path())
        .env("FL_WIRE_SOCKET", root.path().join("missing.sock"))
        .args([
            "--db",
            db.to_str().unwrap(),
            "wire",
            "ack",
            "01a1db00-5252-7726-bef6-1d442d8e2d39",
            "--for",
            "felicia",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result, serde_json::json!([]));
}

#[test]
fn wire_read_private_offline_socket_uses_store_normally() {
    let root = tempfile::tempdir().unwrap();
    let db = root.path().join("offline.redb");
    let output = common::isolated_fl(root.path())
        .unwrap()
        .current_dir(root.path())
        .env("FL_TEST_ISOLATION_ROOT", root.path())
        .env("FL_WIRE_SOCKET", root.path().join("missing.sock"))
        .args([
            "--db",
            db.to_str().unwrap(),
            "wire",
            "read",
            "--for",
            "felicia",
            "--ack",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result, serde_json::json!([]));
}

#[test]
fn wire_send_private_offline_socket_queues_normally() {
    let root = tempfile::tempdir().unwrap();
    let db = root.path().join("offline.redb");
    let output = common::isolated_fl(root.path())
        .unwrap()
        .current_dir(root.path())
        .env("FL_TEST_ISOLATION_ROOT", root.path())
        .env("FL_WIRE_SOCKET", root.path().join("missing.sock"))
        .args([
            "--db",
            db.to_str().unwrap(),
            "wire",
            "send",
            "felicia",
            "subject",
            "body",
            "--from",
            "felix",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("queued, the daemon will deliver"));
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(result.is_object());
}

#[test]
fn wire_status_private_offline_socket_keeps_offline_fallback() {
    let root = tempfile::tempdir().unwrap();
    let db = root.path().join("offline.redb");
    let output = common::isolated_fl(root.path())
        .unwrap()
        .current_dir(root.path())
        .env("FL_TEST_ISOLATION_ROOT", root.path())
        .env("FL_WIRE_SOCKET", root.path().join("missing.sock"))
        .args(["--db", db.to_str().unwrap(), "wire", "status", "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let status: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(status["daemon"], "inactive");
}

#[test]
fn wire_register_private_offline_socket_keeps_offline_behavior() {
    let root = tempfile::tempdir().unwrap();
    let db = root.path().join("offline.redb");
    let output = common::isolated_fl(root.path())
        .unwrap()
        .current_dir(root.path())
        .env("FL_TEST_ISOLATION_ROOT", root.path())
        .env("FL_WIRE_SOCKET", root.path().join("missing.sock"))
        .args([
            "--db",
            db.to_str().unwrap(),
            "wire",
            "register",
            "Felicia",
            "--project",
            "ferroloop",
            "--role",
            "assistant",
            "--replace",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let store = fl_store::RedbStore::open(&db).unwrap();
    assert_eq!(
        store.agent_by_name("felicia").unwrap().unwrap().status,
        AgentStatus::Active
    );
}

fn assert_outside_socket_refusal(invocation: Invocation) {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let socket = outside.path().join("wire.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();

    // If a vendor or tmux override is invoked, it leaves this private marker.
    // The isolated helper still clears PATH and sets every override under root.
    let no_bin = root.path().join("no-bin");
    std::fs::create_dir(&no_bin).unwrap();
    for name in [
        "FL_WIRE_TMUX_BIN",
        "FL_WIRE_CLAUDE_BIN",
        "FL_WIRE_CODEX_BIN",
        "FL_WIRE_AGY_BIN",
        "FL_WIRE_MUSE_BIN",
        "FL_WIRE_GROK_BIN",
        "FL_WIRE_LEMONADE_BIN",
        "FL_WIRE_OLLAMA_BIN",
        "FL_HUB_CLAUDE_BIN",
        "FL_HUB_MUSE_BIN",
        "FL_HUB_AGENT_HYDRATE_BIN",
        "FL_BROWSE_ENGINE_BIN",
    ] {
        let stub = no_bin.join(name);
        std::fs::write(
            &stub,
            "#!/bin/sh\nprintf spawned > \"$FL_TEST_SPAWN_MARKER\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let routes = root.path().join("routes.json");
    let tombstones = root.path().join("decommissioned-sessions.json");
    let mut table = fl_wire::identity::RouteTable::default();
    table.upsert(
        "felicia",
        fl_wire::identity::VendorRoute::Webhook {
            socket_path: root.path().join("private-webhook.sock"),
        },
        At::from_unix_millis(10),
    );
    table.save(Some(&routes)).unwrap();
    let route_bytes = std::fs::read(&routes).unwrap();
    let tombstone_bytes = br#"{"session_ids":["existing"],"retired_personas":["existing"]}"#;
    std::fs::write(&tombstones, tombstone_bytes).unwrap();

    // Refusal must precede creation of even the parent directory of a new store.
    let missing_dir = root.path().join("not-created");
    run_refused_command(
        root.path(),
        &socket,
        &missing_dir.join("fl.redb"),
        invocation,
    );
    assert!(
        !missing_dir.exists(),
        "preflight must precede store directory creation"
    );

    // Also check an existing real registry, so retirement could have changed it.
    let db = root.path().join("existing.redb");
    let before_agent;
    let before_events;
    {
        let store = fl_store::RedbStore::open(&db).unwrap();
        before_agent = store
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
        before_events = store.registry_events_after(0).unwrap();
    }
    let before_store = std::fs::read(&db).unwrap();
    run_refused_command(root.path(), &socket, &db, invocation);
    assert_eq!(
        std::fs::read(&db).unwrap(),
        before_store,
        "store bytes changed"
    );
    {
        let store = fl_store::RedbStore::open(&db).unwrap();
        let after = store.agent_by_name("felicia").unwrap().unwrap();
        assert_eq!(after.id, before_agent.id);
        assert_eq!(after.status, AgentStatus::Active);
        assert_eq!(
            store.registry_events_after(0).unwrap().len(),
            before_events.len()
        );
    }
    assert_eq!(std::fs::read(&routes).unwrap(), route_bytes);
    assert_eq!(std::fs::read(&tombstones).unwrap(), tombstone_bytes);
    assert!(
        !root.path().join("vendor-spawned").exists(),
        "a fixture vendor was spawned"
    );
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock,
        "command must not connect to the out-of-root socket"
    );
}
