//! The `ExecStart` of `contrib/units/ferrostore@.service` runs a REAL `ferrostore`
//! subcommand with REAL flags (#205 A4c), checked against the built binary's `--help`.
//! Nothing is served and nothing reads a store.

use std::path::PathBuf;
use std::process::Command;

#[test]
fn the_instance_unit_runs_ferrostore_serve_on_a_store() {
    let p =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../contrib/units/ferrostore@.service");
    let text = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    let exec = text
        .lines()
        .find_map(|l| l.strip_prefix("ExecStart="))
        .expect("an ExecStart line");
    let words: Vec<&str> = exec.split_whitespace().collect();
    assert_eq!(words[0], "{{PREFIX}}/current/bin/ferrostore", "{exec}");
    assert_eq!(words[1], "serve", "{exec}");
    // the instance name is the store's path: `%f`, systemd's unescaped instance as a path
    assert_eq!(&words[2..], ["--store", "%f"], "{exec}");

    let out = Command::new(env!("CARGO_BIN_EXE_ferrostore"))
        .args(["serve", "--help"])
        .env_clear()
        .output()
        .unwrap();
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{help}");
    assert!(help.contains("--store <PATH>"), "{help}");
    // an instance of a missing store is refused rather than creating one
    assert!(!words.contains(&"--create"), "{exec}");
}
