//! Pure checks: no test below launches a command or opens a socket.
mod common;

use std::ffi::OsStr;
use std::path::PathBuf;
use std::process::Command;

fn root(name: &str) -> PathBuf {
    std::env::temp_dir().join(name)
}

fn value<'a>(command: &'a Command, key: &str) -> Option<&'a OsStr> {
    command
        .get_envs()
        .find_map(|(k, v)| (k == key).then_some(v).flatten())
}

#[test]
fn command_uses_only_fixture_environment() {
    let base = root("fl-fixture-env");
    let mut cmd = Command::new("fixture-fl");
    cmd.env("INHERITED_SENTINEL", "must-be-removed");
    common::isolate_command(&mut cmd, &base).unwrap();
    assert_eq!(cmd.get_program(), OsStr::new("fixture-fl"));
    assert_eq!(value(&cmd, "INHERITED_SENTINEL"), None);
    for name in [
        "HOME",
        "XDG_RUNTIME_DIR",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "FL_WIRE_SOCKET",
        "FL_WIRE_ROUTES",
        "FL_WIRE_CODEX_CONTROL_SOCK",
        "FL_WIRE_TMUX_BIN",
        "FL_WIRE_CLAUDE_BIN",
        "FL_WIRE_CODEX_BIN",
        "FL_WIRE_AGY_BIN",
        "FL_WIRE_MUSE_BIN",
        "FL_WIRE_GROK_BIN",
    ] {
        let path = PathBuf::from(value(&cmd, name).unwrap_or_else(|| panic!("missing {name}")));
        assert!(path.starts_with(&base), "{name}: {path:?}");
    }
    assert_eq!(
        value(&cmd, "FL_WIRE_DISABLE_HOST_DISCOVERY"),
        Some(OsStr::new("1"))
    );
    #[cfg(unix)]
    assert!(PathBuf::from(value(&cmd, "FL_HOST_SOCK").unwrap()).starts_with(&base));
    #[cfg(windows)]
    assert_eq!(
        value(&cmd, "FL_HOST_SOCK"),
        Some(OsStr::new(&common::windows_pipe_suffix(&base)))
    );
}

#[test]
fn caller_can_override_for_explicit_fixture_scenarios() {
    let base = root("fl-fixture-override");
    let mut cmd = common::isolated_command("fl", &base).unwrap();
    cmd.env_remove("XDG_RUNTIME_DIR")
        .env("FL_TEST_RUN_USER_DIR", base.join("run-user"));
    assert_eq!(value(&cmd, "XDG_RUNTIME_DIR"), None);
    assert_eq!(
        value(&cmd, "FL_TEST_RUN_USER_DIR"),
        Some(base.join("run-user").as_os_str())
    );
}

#[test]
fn roots_must_be_absolute_and_socket_paths_must_fit() {
    assert!(common::isolated_command("fl", "relative").is_err());
    assert!(common::isolated_command("fl", root("fl").join("..").join("escape")).is_err());
    #[cfg(unix)]
    assert!(common::isolated_command("fl", root(&"x".repeat(150))).is_err());
}

#[test]
fn windows_suffix_is_safe_and_different_for_different_roots() {
    let a = common::windows_pipe_suffix(&root("a"));
    let b = common::windows_pipe_suffix(&root("b"));
    assert_ne!(a, b);
    assert!(
        a.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    );
}

#[cfg(unix)]
#[test]
fn store_socket_path_accepts_103_bytes_and_rejects_104() {
    use std::os::unix::ffi::OsStrExt;

    // These paths are never created; the test only examines command configuration.
    let valid = PathBuf::from(format!("/{}", "x".repeat(90)));
    let invalid = PathBuf::from(format!("/{}", "x".repeat(91)));
    assert_eq!(valid.join("fl.fls.sock").as_os_str().as_bytes().len(), 103);
    assert_eq!(
        invalid.join("fl.fls.sock").as_os_str().as_bytes().len(),
        104
    );
    assert!(common::isolated_command("fl", &valid).is_ok());
    assert!(common::isolated_command("fl", &invalid).is_err());
}
