//! Recovery never opens an embedded store when a configured daemon is absent.
#[allow(dead_code)]
mod common;

use std::path::Path;
use std::process::Command;

fn recovery_fixture_command(root: &Path) -> Command {
    let endpoint = format!("unix://{}/absent.sock", root.display());
    let mut cmd = common::isolated_fl(root).unwrap();
    cmd.args([
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
    .env("XDG_CONFIG_HOME", root.join("config"))
    .env("XDG_DATA_HOME", root.join("data"));
    cmd
}

#[test]
fn recovery_command_uses_private_fixture_environment_and_endpoint() {
    use std::ffi::OsStr;
    #[cfg(unix)]
    let root = std::path::PathBuf::from("/fl-recovery-fixture");
    #[cfg(not(unix))]
    let root = std::env::temp_dir().join("fl-recovery-fixture");
    let cmd = recovery_fixture_command(&root);
    let env = |name: &str| {
        cmd.get_envs()
            .find_map(|(key, value)| (key == name).then_some(value).flatten())
    };
    assert_eq!(env("HOME"), Some(root.as_os_str()));
    assert_eq!(
        env("XDG_CONFIG_HOME"),
        Some(root.join("config").as_os_str())
    );
    assert_eq!(env("XDG_DATA_HOME"), Some(root.join("data").as_os_str()));
    assert_eq!(env("XDG_RUNTIME_DIR"), Some(root.as_os_str()));
    assert_eq!(env("FL_DB"), Some(root.join("fl.fls").as_os_str()));
    assert_eq!(env("PATH"), Some(root.join("no-bin").as_os_str()));
    let args: Vec<_> = cmd.get_args().collect();
    let endpoint = format!("unix://{}/absent.sock", root.display());
    assert!(
        args.windows(2)
            .any(|pair| pair == [OsStr::new("--endpoint"), OsStr::new(&endpoint)])
    );
}

#[test]
fn absent_owner_refuses_without_creating_store_or_loading_config() {
    let root = tempfile::tempdir().unwrap();
    let output = recovery_fixture_command(root.path()).output().unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("configured owner unavailable"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}
