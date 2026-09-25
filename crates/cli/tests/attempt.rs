use assert_cmd::Command;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;
use std::path::Path;
use std::process::Command as Sys;

fn cli(db: &str) -> Command {
    let mut c = Command::cargo_bin("fl").unwrap();
    // Fix round 1 — Important 5: isolate from the developer's own
    // ~/.config/fl/config.toml, which `fl` reads unconditionally even when
    // --db confines which store it uses. `db`'s own parent directory is a
    // scratch TempDir the caller already holds, so it doubles as an empty
    // config home with no `fl/config.toml` inside it.
    let home = Path::new(db).parent().expect("db has a parent directory");
    c.env("XDG_CONFIG_HOME", home).arg("--db").arg(db);
    c
}

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

/// A registered project, in a real git working tree.
///
/// ⚠ These tests used to register the bare string `/tmp/x`, which is not a
/// git tree and on most machines does not exist at all. That worked only
/// because `project add` stored whatever it was handed. Keep the repo alive
/// for the test's duration — dropping the TempDir deletes it.
fn project(db: &str) -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q"]);
    git(repo.path(), &["config", "user.email", "t@example.com"]);
    git(repo.path(), &["config", "user.name", "t"]);
    std::fs::write(repo.path().join("a.rs"), "fn a() {}").unwrap();
    git(repo.path(), &["add", "-A"]);
    git(repo.path(), &["commit", "-qm", "first"]);
    cli(db)
        .args(["project", "add", &repo.path().display().to_string()])
        .assert()
        .success();
    repo
}

#[test]
fn an_unknown_adapter_is_refused_and_names_the_ones_that_exist() {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("t.redb").display().to_string();
    let _repo = project(&db);
    cli(&db)
        .args(["record", "add", "--project", "1", "--title", "t"])
        .assert()
        .success();
    cli(&db)
        .args(["attempt", "1", "--adapter", "telepathy"])
        .assert()
        .failure()
        .stderr(contains("telepathy").and(contains("claude")));
}

#[test]
fn a_refused_attempt_is_still_recorded_and_shows_up_in_stats() {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("t.redb").display().to_string();
    let _repo = project(&db);
    cli(&db)
        .args(["record", "add", "--project", "1", "--title", "t"])
        .assert()
        .success();

    // A zero budget is refused by the adapter before anything is spawned.
    cli(&db)
        .args([
            "attempt",
            "1",
            "--adapter",
            "claude",
            "--budget-usd-micros",
            "0",
        ])
        .assert()
        .code(1);

    cli(&db)
        .args(["stats", "--project", "1"])
        .assert()
        .success()
        .stdout(contains("refused").and(contains("attempts: 1")));
}

#[test]
fn stats_over_a_project_with_no_attempts_says_so_rather_than_printing_nothing() {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("t.redb").display().to_string();
    let _repo = project(&db);
    cli(&db)
        .args(["stats", "--project", "1"])
        .assert()
        .success()
        .stdout(contains("attempts: 0"));
}
