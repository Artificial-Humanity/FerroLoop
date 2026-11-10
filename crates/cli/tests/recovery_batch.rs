//! Recovery never opens an embedded store when a configured daemon is absent.
use std::process::Command;
fn command() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_fl"));
    for name in [
        "AGENT_PERSONA",
        "AGENT_NAME",
        "FL_HOSTED_ID",
        "AGENT_HYDRATION_GENERATION",
    ] {
        c.env_remove(name);
    }
    c
}
#[test]
fn absent_owner_refuses_without_creating_store_or_loading_config() {
    let root = tempfile::tempdir().unwrap();
    let endpoint = format!("unix://{}/absent.sock", root.path().display());
    let output = command()
        .args([
            "recovery",
            "batch",
            "--endpoint",
            &endpoint,
            "--policy",
            "fixture",
            "--batch",
            "019a6800-0000-7000-8000-000000000001",
            "--member",
            "workspace=019a6800-0000-7000-8000-000000000002",
        ])
        .env("HOME", root.path())
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("XDG_DATA_HOME", root.path().join("data"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("configured owner unavailable"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}
