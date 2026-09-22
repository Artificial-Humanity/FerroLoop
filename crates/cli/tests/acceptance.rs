//! Spec §11. Two bars, not one. The gate bar is "a gate stopped a costly
//! action that a defect would otherwise have been carried into". The rev-2
//! bar is "a finding went from raised to closed with nobody believed at any
//! point", and Task 16's CLI tests are where that one is proved.
//!
//! ⚠ Spec §11 requires the defect in the real run to be written by somebody
//! other than whoever wrote the gate. This automated version cannot enforce
//! that, so it proves the mechanism and the human exercise proves the rest.

use assert_cmd::Command;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;
use std::fs;
use std::path::Path;
use std::process::Command as Sys;

fn git(dir: &Path, args: &[&str]) {
    assert!(
        Sys::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap()
            .status
            .success(),
        "git {args:?} failed"
    );
}

#[test]
fn a_defect_present_before_the_run_stops_the_launch() {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let db = home.path().join("t.redb").display().to_string();
    let cli = || {
        let mut c = Command::cargo_bin("fl").unwrap();
        c.arg("--db").arg(&db);
        c
    };

    git(repo.path(), &["init", "-q"]);
    git(repo.path(), &["config", "user.email", "t@example.com"]);
    git(repo.path(), &["config", "user.name", "t"]);
    fs::create_dir_all(repo.path().join("config")).unwrap();
    // A config the launch will read. Valid to begin with.
    fs::write(repo.path().join("config/run.json"), r#"{"epochs": 10}"#).unwrap();
    git(repo.path(), &["add", "-A"]);
    git(repo.path(), &["commit", "-qm", "valid config"]);

    // A gate that parses every config file. `python3 -c` stands in for a real
    // validator; the point is that it is a cheap falsifiable predicate.
    //
    // ⚠ `--arg` is `num_args = 0..`, so clap treats a bare `-c` token that
    // follows it as a new flag rather than a value (verified against
    // `fl gate add --help` and a live run: the two-token form
    // `--arg -c` fails with "unexpected argument '-c' found"). The
    // `--arg=VALUE` form sidesteps clap's flag-vs-value sniffing and is
    // what actually reaches the process as an argv value.
    cli()
        .args(["project", "add", &repo.path().display().to_string()])
        .assert()
        .success();
    cli()
        .args([
            "gate",
            "add",
            "--project",
            "1",
            "--name",
            "config-parses",
            "--kind",
            "command",
            "--glob",
            "config/*.json",
            "--program",
            "python3",
            "--arg=-c",
            "--arg=import json,sys;[json.load(open(p)) for p in sys.argv[1:]]",
            "--authored-by",
            "acceptance",
        ])
        .assert()
        .success();
    cli()
        .args([
            "transition",
            "add",
            "--project",
            "1",
            "--name",
            "launch",
            "--from",
            "review",
            "--to",
            "done",
            "--regret",
            "high",
            "--gate",
            "2",
        ])
        .assert()
        .success();

    // The launch is allowed while the config is sound.
    cli()
        .args(["check", "launch", "--project", "1"])
        .assert()
        .success();

    // Now the defect lands, exactly as it did in the story that started this:
    // present before the run, invisible until the run wasted hours.
    fs::write(repo.path().join("config/run.json"), r#"{"epochs": 10,,}"#).unwrap();
    git(repo.path(), &["add", "-A"]);
    git(repo.path(), &["commit", "-qm", "defect"]);

    // The launch is refused, and the reason is the predicate and not staleness.
    //
    // ⚠ That second clause is asserted, not just claimed: `contains("predicate")`
    // pins the fail reason fl-core reports (`FailReason::Predicate`, printed via
    // `{reason:?}` in `check.rs`). Without it, this test passed even with the
    // predicate check in `command.rs` mutated to always succeed, because the
    // same commit that breaks the config also moves the gate's population past
    // its stamp, so staleness independently fails the transition (as `Stale`)
    // at this `--regret high` transition. A green here must mean the predicate
    // itself caught the defect, not that some other guard happened to.
    cli()
        .args(["check", "launch", "--project", "1"])
        .assert()
        .code(1)
        .stdout(
            contains("FAIL")
                .and(contains("config-parses"))
                .and(contains("predicate")),
        );
}

#[test]
fn deleting_the_only_config_does_not_turn_the_gate_green() {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let db = home.path().join("t.redb").display().to_string();
    let cli = || {
        let mut c = Command::cargo_bin("fl").unwrap();
        c.arg("--db").arg(&db);
        c
    };

    git(repo.path(), &["init", "-q"]);
    git(repo.path(), &["config", "user.email", "t@example.com"]);
    git(repo.path(), &["config", "user.name", "t"]);
    fs::create_dir_all(repo.path().join("config")).unwrap();
    fs::write(repo.path().join("config/run.json"), r#"{"epochs": 10}"#).unwrap();
    git(repo.path(), &["add", "-A"]);
    git(repo.path(), &["commit", "-qm", "valid config"]);

    cli()
        .args(["project", "add", &repo.path().display().to_string()])
        .assert()
        .success();
    cli()
        .args([
            "gate",
            "add",
            "--project",
            "1",
            "--name",
            "config-parses",
            "--kind",
            "command",
            "--glob",
            "config/*.json",
            "--program",
            "true",
            "--authored-by",
            "acceptance",
        ])
        .assert()
        .success();
    cli()
        .args([
            "transition",
            "add",
            "--project",
            "1",
            "--name",
            "launch",
            "--from",
            "review",
            "--to",
            "done",
            "--regret",
            "high",
            "--gate",
            "2",
        ])
        .assert()
        .success();

    fs::remove_file(repo.path().join("config/run.json")).unwrap();
    git(repo.path(), &["add", "-A"]);
    git(repo.path(), &["commit", "-qm", "removed the config"]);

    // ⚠ The whole product in one assertion. With nothing to examine, the
    // trivially-true command must not produce a green light.
    cli()
        .args(["check", "launch", "--project", "1"])
        .assert()
        .code(1)
        .stdout(contains("empty_population"));
}
