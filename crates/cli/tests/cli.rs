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
    cli(&d)
        .args(["project", "add", "/tmp/x"])
        .assert()
        .success();
    cli(&d)
        .args(["project", "list"])
        .assert()
        .success()
        .stdout(contains("/tmp/x"));
}

#[test]
fn an_unknown_gate_kind_is_refused_with_an_actionable_message() {
    let d = tempfile::tempdir().unwrap();
    cli(&d)
        .args(["project", "add", "/tmp/x"])
        .assert()
        .success();
    cli(&d)
        .args([
            "gate",
            "add",
            "--project",
            "1",
            "--name",
            "g",
            "--kind",
            "telepathy",
            "--glob",
            "**/*.rs",
            "--program",
            "true",
        ])
        .assert()
        .failure()
        .stderr(contains("telepathy").and(contains("command")));
}

#[test]
fn a_missing_project_is_refused_and_names_the_id() {
    let d = tempfile::tempdir().unwrap();
    cli(&d)
        .args([
            "gate",
            "add",
            "--project",
            "99",
            "--name",
            "g",
            "--kind",
            "command",
            "--glob",
            "**/*.rs",
            "--program",
            "true",
        ])
        .assert()
        .failure()
        .stderr(contains("99"));
}

#[test]
fn a_record_moves_between_states() {
    let d = tempfile::tempdir().unwrap();
    cli(&d)
        .args(["project", "add", "/tmp/x"])
        .assert()
        .success();
    cli(&d)
        .args(["record", "add", "--project", "1", "--title", "t"])
        .assert()
        .success();
    cli(&d)
        .args(["record", "move", "2", "--to", "doing"])
        .assert()
        .success();
    cli(&d)
        .args(["record", "list", "--project", "1"])
        .assert()
        .success()
        .stdout(contains("doing"));
}

#[test]
fn an_unknown_state_name_is_refused_and_lists_the_valid_ones() {
    let d = tempfile::tempdir().unwrap();
    cli(&d)
        .args(["project", "add", "/tmp/x"])
        .assert()
        .success();
    cli(&d)
        .args(["record", "add", "--project", "1", "--title", "t"])
        .assert()
        .success();
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
    assert!(
        !nested.parent().unwrap().exists(),
        "fixture must start absent"
    );

    let mut c = Command::cargo_bin("flctl").unwrap();
    c.arg("--db").arg(&nested);
    c.args(["project", "add", "/tmp/x"]).assert().success();

    assert!(
        nested.exists(),
        "store file should have been created at {nested:?}"
    );
}

#[test]
fn a_missing_parent_directory_for_fl_db_is_created_the_same_way_as_the_db_flag() {
    let d = tempfile::tempdir().unwrap();
    let nested = d.path().join("nested").join("deep").join("t.redb");
    assert!(
        !nested.parent().unwrap().exists(),
        "fixture must start absent"
    );

    // `env_clear` scopes the controlled environment to this child process
    // only; it never touches this test process's own environment, so it
    // cannot race any other test reading or setting env vars in parallel.
    let mut c = Command::cargo_bin("flctl").unwrap();
    c.env_clear();
    c.env("FL_DB", &nested);
    c.args(["project", "add", "/tmp/x"]).assert().success();

    assert!(
        nested.exists(),
        "store file should have been created at {nested:?}"
    );
}

// Fix round 1 — Important 2: `transition` had no coverage at all, including
// the one piece of genuinely new logic in this task (the `--gate`-existence
// refusal). Pin `add`, `show` round-tripping what was stored, and the
// refusal naming the missing gate id.

// Fix round 2: `from`/`to`/`regret` used to be pinned to their exact
// PascalCase wire form (`"Review"`, `"Done"`, `"Low"`), which is an accident
// of the bare `#[derive(Serialize)]` on `State`/`Regret` — not a designed
// contract. That casing is the owner's open, undecided question (see the
// parked Minor in Fix Round 1: `show` emits `"Todo"`/`"Low"` where text-mode
// output prints `"todo"`/`"low"`). A CLI test must not quietly ratify one
// side of that decision. So this parses the JSON and compares `from`/`to`/
// `regret` case-insensitively, keeping the round-trip claim (wrong field,
// wrong value, or missing output all still fail this test) without pinning
// which casing wins. `name` has no such open question, so it stays an exact
// match.
#[test]
fn a_transition_can_be_added_and_shown() {
    let d = tempfile::tempdir().unwrap();
    cli(&d)
        .args(["project", "add", "/tmp/x"])
        .assert()
        .success();
    cli(&d)
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
            "low",
        ])
        .assert()
        .success()
        .stdout(contains("launch"));
    let output = cli(&d)
        .args(["transition", "show", "--project", "1", "--name", "launch"])
        .assert()
        .success()
        .stdout(contains("\"name\": \"launch\""))
        .get_output()
        .stdout
        .clone();
    let json: serde_json::Value =
        serde_json::from_slice(&output).expect("`transition show` must print valid JSON");

    let from = json["from"]
        .as_str()
        .expect("`from` must be a string")
        .to_lowercase();
    let to = json["to"]
        .as_str()
        .expect("`to` must be a string")
        .to_lowercase();
    let regret = json["regret"]
        .as_str()
        .expect("`regret` must be a string")
        .to_lowercase();
    assert_eq!(from, "review", "got {json}");
    assert_eq!(to, "done", "got {json}");
    assert_eq!(regret, "low", "got {json}");
}

#[test]
fn a_transition_naming_a_nonexistent_gate_is_refused_and_names_the_id() {
    let d = tempfile::tempdir().unwrap();
    cli(&d)
        .args(["project", "add", "/tmp/x"])
        .assert()
        .success();
    cli(&d)
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
            "low",
            "--gate",
            "42",
        ])
        .assert()
        .failure()
        .stderr(contains("42"));
}
