//! Subprocess boundaries use only explicit fixture executables and temporary homes.
#[used]
#[cfg_attr(
    target_vendor = "apple",
    unsafe(link_section = "__DATA,__mod_init_func")
)]
#[cfg_attr(not(target_vendor = "apple"), unsafe(link_section = ".init_array"))]
static INITIALIZE_ISOLATION: extern "C" fn() = {
    extern "C" fn enable() {
        fl_wire::test_isolation::enable();
    }
    enable
};

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

use fl_wire::dispatcher::{
    SessionResetMode, clear_or_reforge_session, dispatch_operator_prompt_to_route,
};
use fl_wire::identity::VendorRoute;
use tempfile::TempDir;

fn run_fixture(mode: &str) {
    assert!(fl_wire::test_isolation::is_enabled());
    let result = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "fixture_child", "--nocapture"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
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
    assert!(fl_wire::test_isolation::is_enabled());
    let Ok(mode) = std::env::var("SESSION_RESET_FIXTURE") else {
        return;
    };
    // The startup constructor gives each process its own root. Create every fixture
    // here, after the child constructor, rather than passing parent-owned paths in.
    let tmp = TempDir::new().unwrap();
    let bin = tmp.path().join("codex-fixture");
    fs::write(
        &bin,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" >> \"$HOME/queue.log\"\n",
    )
    .unwrap();
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o700)).unwrap();
    let _executable = fl_wire::test_isolation::register_executable(&bin).unwrap();
    // This child runs exactly one fixture test, with no inherited vendor endpoints.
    // Setting an explicit vendor fixture disables control-socket discovery and the
    // queue-to-host fallback; tmux probes, if any, reach this same private sentinel.
    unsafe {
        std::env::set_var("HOME", tmp.path());
        std::env::set_var("XDG_CACHE_HOME", tmp.path().join(".cache"));
        std::env::set_var("XDG_CONFIG_HOME", tmp.path().join(".config"));
        std::env::set_var("XDG_DATA_HOME", tmp.path().join(".local/share"));
        std::env::set_var("FL_WIRE_CODEX_BIN", &bin);
        std::env::set_var("FL_WIRE_TMUX_BIN", &bin);
    }
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
