//! Fixture-only checks for the legacy hydration executable boundary.
//! These child-process tests run in CI, not as a local live-capable suite.
#![cfg(unix)]

#[allow(dead_code)]
mod common;

use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;

fn script(path: &Path) {
    std::fs::write(
        path,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" > \"$FL_TEST_HYDRATE_MARKER\"\nexit 7\n",
    )
    .unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn hydrate_refuses_outside_and_missing_executables_before_spawn() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let external = outside.path().join("agent-hydrate");
    script(&external);
    let marker = root.path().join("spawned");
    let link = root.path().join("linked-hydrate");
    symlink(&external, &link).unwrap();
    for program in [&external, &link] {
        let output = common::isolated_fl(root.path())
            .unwrap()
            .env("FL_TEST_ISOLATION_ROOT", root.path())
            .env("FL_TEST_AGENT_HYDRATE_BIN", program)
            .env("FL_TEST_HYDRATE_MARKER", &marker)
            .args(["agent-msg", "hydrate", "Kayla"])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("fixture endpoint refused"));
        assert!(!marker.exists());
        assert!(!root.path().join("fl.fls").exists());
    }
    let output = common::isolated_fl(root.path())
        .unwrap()
        .env("FL_TEST_ISOLATION_ROOT", root.path())
        .env_remove("FL_TEST_AGENT_HYDRATE_BIN")
        .env("FL_TEST_HYDRATE_MARKER", &marker)
        .args(["agent-msg", "hydrate", "Kayla"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("fixture agent-hydrate executable is not configured")
    );
    assert!(!marker.exists());
    assert!(!root.path().join("fl.fls").exists());
}

#[test]
fn hydrate_runs_private_fixture_and_preserves_args_and_exit() {
    let root = tempfile::tempdir().unwrap();
    let private = root.path().join("agent-hydrate");
    let marker = root.path().join("spawned");
    script(&private);
    let output = common::isolated_fl(root.path())
        .unwrap()
        .env("FL_TEST_ISOLATION_ROOT", root.path())
        .env("FL_TEST_AGENT_HYDRATE_BIN", &private)
        .env("FL_TEST_HYDRATE_MARKER", &marker)
        .args(["agent-msg", "hydrate", "Kayla", "--replace"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(7));
    assert_eq!(
        std::fs::read_to_string(marker).unwrap().trim(),
        "Kayla --replace"
    );
}
