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

// Fix round 1 — Important 1: `db_path`'s four tiers must behave identically.
// `--db` and `$FL_DB` previously returned the path unchecked, so a missing
// parent directory surfaced as a raw redb I/O error instead of being
// created, the way the `$XDG_DATA_HOME`/`$HOME` tier already did. These two
// tests drive the *built binary* as a subprocess with a controlled
// environment (rather than mutating this test process's own environment,
// which would race every other test that reads it) and cover both changed
// tiers directly.

#[test]
fn a_missing_parent_directory_for_the_db_flag_is_created() {
    let d = tempfile::tempdir().unwrap();
    let nested = d.path().join("nested").join("deep").join("t.redb");
    assert!(!nested.parent().unwrap().exists(), "fixture must start absent");

    let mut c = Command::cargo_bin("flctl").unwrap();
    c.arg("--db").arg(&nested);
    c.args(["project", "add", "/tmp/x"]).assert().success();

    assert!(nested.exists(), "store file should have been created at {nested:?}");
}

#[test]
fn a_missing_parent_directory_for_fl_db_is_created_the_same_way_as_the_db_flag() {
    let d = tempfile::tempdir().unwrap();
    let nested = d.path().join("nested").join("deep").join("t.redb");
    assert!(!nested.parent().unwrap().exists(), "fixture must start absent");

    // `env_clear` scopes the controlled environment to this child process
    // only; it never touches this test process's own environment, so it
    // cannot race any other test reading or setting env vars in parallel.
    let mut c = Command::cargo_bin("flctl").unwrap();
    c.env_clear();
    c.env("FL_DB", &nested);
    c.args(["project", "add", "/tmp/x"]).assert().success();

    assert!(nested.exists(), "store file should have been created at {nested:?}");
}

// Fix round 1 — Important 2: `transition` had no coverage at all, including
// the one piece of genuinely new logic in this task (the `--gate`-existence
// refusal). Pin `add`, `show` round-tripping what was stored, and the
// refusal naming the missing gate id.

#[test]
fn a_transition_can_be_added_and_shown() {
    let d = tempfile::tempdir().unwrap();
    cli(&d).args(["project", "add", "/tmp/x"]).assert().success();
    cli(&d)
        .args([
            "transition", "add", "--project", "1", "--name", "launch",
            "--from", "review", "--to", "done", "--regret", "low",
        ])
        .assert()
        .success()
        .stdout(contains("launch"));
    cli(&d)
        .args(["transition", "show", "--project", "1", "--name", "launch"])
        .assert()
        .success()
        .stdout(
            contains("\"name\": \"launch\"")
                .and(contains("\"from\": \"Review\""))
                .and(contains("\"to\": \"Done\""))
                .and(contains("\"regret\": \"Low\"")),
        );
}

#[test]
fn a_transition_naming_a_nonexistent_gate_is_refused_and_names_the_id() {
    let d = tempfile::tempdir().unwrap();
    cli(&d).args(["project", "add", "/tmp/x"]).assert().success();
    cli(&d)
        .args([
            "transition", "add", "--project", "1", "--name", "launch",
            "--from", "review", "--to", "done", "--regret", "low",
            "--gate", "42",
        ])
        .assert()
        .failure()
        .stderr(contains("42"));
}
