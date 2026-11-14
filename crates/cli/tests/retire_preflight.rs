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
    LegacySubcommand,
    LegacyExecutable,
    WireStatus,
}

fn run_refused_command(root: &Path, outside_socket: &Path, db: &Path, invocation: Invocation) {
    let mut command = if matches!(invocation, Invocation::LegacyExecutable) {
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
    match invocation {
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
    }
    let output = command.output().unwrap();
    assert!(!output.status.success(), "command unexpectedly succeeded");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("fixture endpoint refused"), "{stderr}");
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
