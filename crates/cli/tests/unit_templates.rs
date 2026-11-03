//! The `ExecStart` of every `contrib/units/*` template that runs `fl` names a REAL subcommand
//! and REAL flags (#205 A4c). Each is checked against the built `fl`: its subcommand path
//! must answer `--help`, and every flag the unit passes must be in that help. Nothing is
//! started, and nothing reads a config, a store or the real systemd.

use assert_cmd::Command;
use std::fs;
use std::path::PathBuf;

fn unit(name: &str) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../contrib/units")
        .join(name);
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

fn exec_start(text: &str) -> String {
    text.lines()
        .find_map(|l| l.strip_prefix("ExecStart="))
        .expect("an ExecStart line")
        .to_string()
}

/// The `fl` units and the subcommand path each must run.
const FL_UNITS: [(&str, &[&str]); 3] = [
    ("ferrostore.service", &["store", "serve"]),
    ("ferrowire.service", &["wire", "daemon"]),
    ("ferroloop-hub.service", &["hub", "serve"]),
];

#[test]
fn an_fl_unit_runs_a_real_subcommand_with_real_flags() {
    for (name, path) in FL_UNITS {
        let exec = exec_start(&unit(name));
        let words: Vec<&str> = exec.split_whitespace().collect();
        assert_eq!(words[0], "{{LAUNCHER}}", "{name}");
        let sub: Vec<&str> = words[1..]
            .iter()
            .copied()
            .take_while(|w| !w.starts_with('-'))
            .collect();
        assert_eq!(sub, path, "{name}: {exec}");

        let out = Command::cargo_bin("fl")
            .unwrap()
            .env_clear()
            .args(&sub)
            .arg("--help")
            .output()
            .unwrap();
        let help = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(out.status.success(), "{name}: fl {sub:?} --help\n{help}");
        for flag in words.iter().filter(|w| w.starts_with("--")) {
            assert!(
                help.contains(*flag),
                "{name}: `fl {}` has no {flag}\n{help}",
                sub.join(" ")
            );
        }
    }
}

#[test]
fn the_hub_unit_ui_dir_is_the_flag_the_hub_reads() {
    // the unit's `--ui-dir` is the flag `fl hub serve` takes, ahead of `[hub] ui_dir`
    let help = Command::cargo_bin("fl")
        .unwrap()
        .env_clear()
        .args(["hub", "serve", "--help"])
        .output()
        .unwrap();
    let help = String::from_utf8_lossy(&help.stdout).into_owned();
    assert!(help.contains("--ui-dir <UI_DIR>"), "{help}");
}
