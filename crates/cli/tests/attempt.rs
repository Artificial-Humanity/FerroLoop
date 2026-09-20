use assert_cmd::Command;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;

fn cli(db: &str) -> Command {
    let mut c = Command::cargo_bin("flctl").unwrap();
    c.arg("--db").arg(db);
    c
}

#[test]
fn an_unknown_adapter_is_refused_and_names_the_ones_that_exist() {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("t.redb").display().to_string();
    cli(&db).args(["project", "add", "/tmp/x"]).assert().success();
    cli(&db).args(["record", "add", "--project", "1", "--title", "t"]).assert().success();
    cli(&db)
        .args(["attempt", "2", "--adapter", "telepathy"])
        .assert()
        .failure()
        .stderr(contains("telepathy").and(contains("claude")));
}

#[test]
fn a_refused_attempt_is_still_recorded_and_shows_up_in_stats() {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("t.redb").display().to_string();
    cli(&db).args(["project", "add", "/tmp/x"]).assert().success();
    cli(&db).args(["record", "add", "--project", "1", "--title", "t"]).assert().success();

    // A zero budget is refused by the adapter before anything is spawned.
    cli(&db)
        .args(["attempt", "2", "--adapter", "claude", "--budget-usd-micros", "0"])
        .assert()
        .code(1);

    cli(&db)
        .args(["stats", "--project", "1"])
        .assert()
        .success()
        .stdout(contains("Refused").and(contains("attempts: 1")));
}

#[test]
fn stats_over_a_project_with_no_attempts_says_so_rather_than_printing_nothing() {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("t.redb").display().to_string();
    cli(&db).args(["project", "add", "/tmp/x"]).assert().success();
    cli(&db)
        .args(["stats", "--project", "1"])
        .assert()
        .success()
        .stdout(contains("attempts: 0"));
}
