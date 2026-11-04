//! Cross-process broker-marker and vault-lock behavior.
#![cfg(unix)]

use fl_gate::{GateError, SetSecretArgs, Vault};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

const DIR: &str = "FL_GATE_MARKER_RACE_DIR";
const ROLE: &str = "FL_GATE_MARKER_RACE_ROLE";

fn vault(dir: &Path) -> Vault {
    Vault::open(dir.join("vault.enc"), [9; 32]).unwrap()
}

#[test]
fn marker_race_child() {
    let (Some(dir), Some(role)) = (std::env::var_os(DIR), std::env::var_os(ROLE)) else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    let role = role.to_string_lossy();
    let mut vault = vault(&dir);
    std::fs::write(dir.join(format!("ready-{role}")), b"ready").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !dir.join("go").exists() {
        assert!(Instant::now() < deadline, "start signal missing");
        std::thread::yield_now();
    }
    let outcome = if role == "marker" {
        vault.require_broker().unwrap();
        "configured"
    } else {
        match vault.set(SetSecretArgs {
            name: "RACE".into(),
            value: "example-value".into(),
            ..Default::default()
        }) {
            Ok(_) => "written",
            Err(GateError::DispatchAuthority(_)) => "refused",
            Err(e) => panic!("unexpected write result: {e}"),
        }
    };
    std::fs::write(dir.join(format!("result-{role}")), outcome).unwrap();
}

#[test]
fn marker_creation_and_vault_write_are_serialized_across_processes() {
    for _ in 0..8 {
        let dir = TempDir::new().unwrap();
        let mut initial = vault(dir.path());
        initial
            .set(SetSecretArgs {
                name: "EXISTING".into(),
                value: "value".into(),
                ..Default::default()
            })
            .unwrap();
        let mut children = Vec::new();
        for role in ["marker", "writer"] {
            children.push(
                Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "marker_race_child", "--nocapture"])
                    .env(DIR, dir.path())
                    .env(ROLE, role)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .unwrap(),
            );
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while !(dir.path().join("ready-marker").exists()
            && dir.path().join("ready-writer").exists())
        {
            assert!(Instant::now() < deadline, "workers did not become ready");
            std::thread::yield_now();
        }
        std::fs::write(dir.path().join("go"), b"go").unwrap();
        for mut child in children {
            while child.try_wait().unwrap().is_none() {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("worker failed to finish");
                }
                std::thread::yield_now();
            }
            assert!(child.wait().unwrap().success());
        }
        assert_eq!(
            std::fs::read_to_string(dir.path().join("result-marker")).unwrap(),
            "configured"
        );
        let result = std::fs::read_to_string(dir.path().join("result-writer")).unwrap();
        assert!(result == "written" || result == "refused");
        let mut reopened = vault(dir.path());
        assert_eq!(reopened.get_metadata("RACE").is_some(), result == "written");
        assert!(matches!(
            reopened.set(SetSecretArgs {
                name: "LATER".into(),
                value: "value".into(),
                ..Default::default()
            }),
            Err(GateError::DispatchAuthority(_))
        ));
    }
}
