use assert_cmd::Command;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;

fn db(dir: &tempfile::TempDir) -> String {
    dir.path().join("t.redb").display().to_string()
}

fn cli(dir: &tempfile::TempDir) -> Command {
    let mut c = Command::cargo_bin("flctl").unwrap();
    c.arg("--db").arg(db(dir));
    c
}

#[test]
fn a_project_can_be_added_and_listed() {
    let d = tempfile::tempdir().unwrap();
    cli(&d).args(["project", "add", "/tmp/x"]).assert().success();
    cli(&d).args(["project", "list"]).assert().success().stdout(contains("/tmp/x"));
}

#[test]
fn an_unknown_gate_kind_is_refused_with_an_actionable_message() {
    let d = tempfile::tempdir().unwrap();
    cli(&d).args(["project", "add", "/tmp/x"]).assert().success();
    cli(&d)
        .args(["gate", "add", "--project", "1", "--name", "g", "--kind", "telepathy",
               "--glob", "**/*.rs", "--program", "true"])
        .assert()
        .failure()
        .stderr(contains("telepathy").and(contains("command")));
}

#[test]
fn a_missing_project_is_refused_and_names_the_id() {
    let d = tempfile::tempdir().unwrap();
    cli(&d)
        .args(["gate", "add", "--project", "99", "--name", "g", "--kind", "command",
               "--glob", "**/*.rs", "--program", "true"])
        .assert()
        .failure()
        .stderr(contains("99"));
}

#[test]
fn a_record_moves_between_states() {
    let d = tempfile::tempdir().unwrap();
    cli(&d).args(["project", "add", "/tmp/x"]).assert().success();
    cli(&d).args(["record", "add", "--project", "1", "--title", "t"]).assert().success();
    cli(&d).args(["record", "move", "2", "--to", "doing"]).assert().success();
    cli(&d).args(["record", "list", "--project", "1"]).assert().success().stdout(contains("doing"));
}

#[test]
fn an_unknown_state_name_is_refused_and_lists_the_valid_ones() {
    let d = tempfile::tempdir().unwrap();
    cli(&d).args(["project", "add", "/tmp/x"]).assert().success();
    cli(&d).args(["record", "add", "--project", "1", "--title", "t"]).assert().success();
    cli(&d)
        .args(["record", "move", "2", "--to", "sideways"])
        .assert()
        .failure()
        .stderr(contains("needs_human"));
}
