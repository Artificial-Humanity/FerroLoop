//! Subprocess boundaries use only explicit fixture executables and temporary homes.
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

use fl_wire::dispatcher::{
    SessionResetMode, clear_or_reforge_session, dispatch_operator_prompt_to_route,
};
use fl_wire::identity::VendorRoute;
use tempfile::TempDir;

fn run_fixture(mode: &str) {
    let tmp = TempDir::new().unwrap();
    let bin = tmp.path().join("codex-fixture");
    fs::write(
        &bin,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" >> \"$HOME/queue.log\"\n",
    )
    .unwrap();
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o700)).unwrap();
    let result = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "fixture_child", "--nocapture"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", tmp.path())
        .env("FL_WIRE_CODEX_BIN", &bin)
        // Setting an explicit fixture also disables the existing queue-to-host fallback.
        .env("FL_WIRE_TMUX_BIN", &bin)
        .env("SESSION_RESET_FIXTURE", mode)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn codex_reset_slashes_never_reach_queue() {
    run_fixture("slash");
}

#[test]
fn codex_normal_and_compact_keep_queue_fixture_path() {
    run_fixture("ordinary");
}

#[test]
fn unsupported_resets_never_spawn_queue_or_terminal_commands() {
    run_fixture("reset");
}

#[test]
fn fixture_child() {
    let Ok(mode) = std::env::var("SESSION_RESET_FIXTURE") else {
        return;
    };
    let route = VendorRoute::Codex {
        thread_id: "fixture-thread".into(),
        cwd: None,
    };
    let log = std::path::PathBuf::from(std::env::var_os("HOME").unwrap()).join("queue.log");
    if mode == "reset" {
        let routes = [
            route,
            VendorRoute::Muse {
                session_id: "fixture".into(),
                cwd: None,
                tmux_target: Some("fixture:muse".into()),
                tmux_pane: Some("%999999".into()),
            },
            VendorRoute::Grok {
                session_id: "fixture".into(),
                cwd: None,
                tmux_target: Some("fixture:grok".into()),
                tmux_pane: Some("%999999".into()),
            },
            VendorRoute::HostCli {
                vendor: " ClAuDe ".into(),
                session_id: "fixture".into(),
                cwd: None,
                tmux_target: Some("fixture:host".into()),
                tmux_pane: Some("%999999".into()),
            },
        ];
        for route in routes {
            for mode in [SessionResetMode::Clear, SessionResetMode::Reforge] {
                let error =
                    clear_or_reforge_session(&route, mode, "owner", Some("hydrate fixture"))
                        .unwrap_err();
                let value: serde_json::Value = serde_json::from_str(&error).unwrap();
                assert_eq!(value["code"], "reset_unsupported");
            }
        }
        assert!(
            !log.exists(),
            "refusal must not probe or write terminal/queue commands"
        );
    } else if mode == "slash" {
        for text in ["/new", " /NeW\n", "/clear", "\t/CLEAR "] {
            let error = dispatch_operator_prompt_to_route(&route, "owner", text).unwrap_err();
            let value: serde_json::Value = serde_json::from_str(&error).unwrap();
            assert_eq!(value["code"], "reset_unsupported");
            assert_eq!(value["vendor"], "codex");
            assert_eq!(value["reason"], "identity_unconfirmable");
            assert_eq!(value["alternative"], "remodel_same_vendor");
            assert!(value["error"].as_str().unwrap().contains("same vendor"));
        }
        assert!(!log.exists(), "reset must never spawn queue");
    } else {
        // Custom binary disables automatic control-socket discovery. No live endpoints exist.
        for text in [
            "ordinary prompt",
            " /Compact\n",
            "/new with arguments",
            "explain /clear",
        ] {
            dispatch_operator_prompt_to_route(&route, "owner", text).unwrap();
        }
        let args = fs::read_to_string(log).unwrap();
        assert_eq!(args.lines().filter(|line| *line == "queue").count(), 4);
        assert!(args.contains("ordinary prompt"));
        assert!(args.contains("/Compact"));
        assert!(args.contains("/new with arguments"));
        assert!(args.contains("explain /clear"));
    }
}
