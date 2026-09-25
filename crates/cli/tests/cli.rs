use assert_cmd::Command;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;

fn db(dir: &tempfile::TempDir) -> String {
    dir.path().join("t.redb").display().to_string()
}

fn cli(dir: &tempfile::TempDir) -> Command {
    let mut c = Command::cargo_bin("fl").unwrap();
    // Fix round 1 — Important 5: every `fl` invocation reads the user's
    // config unconditionally (even `--db`, which only confines which store
    // it uses — the config file is still read and validated), so every
    // test that drives the real binary must not read the developer's own
    // `~/.config/fl/config.toml`.
    c.env("XDG_CONFIG_HOME", dir.path())
        .arg("--db")
        .arg(db(dir));
    c
}

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

/// A real git working tree, not yet registered.
///
/// ⚠ These tests used to register the bare string `/tmp/x`, which is not a
/// git tree and on most machines does not exist. That worked only because
/// `project add` stored whatever string it was handed. Returns the TempDir:
/// dropping it deletes the repo, so bind it for the test's duration.
fn git_repo() -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q"]);
    git(repo.path(), &["config", "user.email", "t@example.com"]);
    git(repo.path(), &["config", "user.name", "t"]);
    std::fs::write(repo.path().join("a.rs"), "fn a() {}").unwrap();
    git(repo.path(), &["add", "-A"]);
    git(repo.path(), &["commit", "-qm", "first"]);
    repo
}

/// The same tree, registered against this test's default store.
fn project(d: &tempfile::TempDir) -> tempfile::TempDir {
    let repo = git_repo();
    cli(d)
        .args(["project", "add", &repo.path().display().to_string()])
        .assert()
        .success();
    repo
}

#[test]
fn a_project_can_be_added_and_listed() {
    let d = tempfile::tempdir().unwrap();
    let repo = project(&d);
    // `project add` stores the canonical root, not the string it was given,
    // so a gate resolves the same tree from any working directory.
    let canonical = repo.path().canonicalize().unwrap().display().to_string();
    cli(&d)
        .args(["project", "list"])
        .assert()
        .success()
        .stdout(contains(canonical));
}

#[test]
fn an_unknown_gate_kind_is_refused_with_an_actionable_message() {
    let d = tempfile::tempdir().unwrap();
    let _repo = project(&d);
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
    let _repo = project(&d);
    cli(&d)
        .args(["record", "add", "--project", "1", "--title", "t"])
        .assert()
        .success();
    cli(&d)
        .args(["record", "move", "1", "--to", "doing"])
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
    let _repo = project(&d);
    cli(&d)
        .args(["record", "add", "--project", "1", "--title", "t"])
        .assert()
        .success();
    cli(&d)
        .args(["record", "move", "1", "--to", "sideways"])
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

    let mut c = Command::cargo_bin("fl").unwrap();
    c.env("XDG_CONFIG_HOME", d.path()).arg("--db").arg(&nested);
    let repo = git_repo();
    c.args(["project", "add", &repo.path().display().to_string()])
        .assert()
        .success();

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
    let mut c = Command::cargo_bin("fl").unwrap();
    c.env_clear();
    c.env("FL_DB", &nested);
    // PATH survives the clear: `project add` shells out to `git` to check
    // that the root is a working tree, and a cleared PATH would make this
    // test fail for a reason that has nothing to do with the store path.
    if let Ok(path) = std::env::var("PATH") {
        c.env("PATH", path);
    }
    let repo = git_repo();
    c.args(["project", "add", &repo.path().display().to_string()])
        .assert()
        .success();

    assert!(
        nested.exists(),
        "store file should have been created at {nested:?}"
    );
}

// Fix round 1 — Important 2: `transition` had no coverage at all, including
// the one piece of genuinely new logic in this task (the `--gate`-existence
// refusal). Pin `add`, `show` round-tripping what was stored, and the
// refusal naming the missing gate id.

// The casing question this test used to hold open is DECIDED (owner,
// 2026-09-21): snake_case on the wire, everywhere. `from`/`to`/`regret` are
// therefore pinned exactly again, and case-insensitively is no longer good
// enough — a regression back to `"Review"` must fail here, not pass.
// `fl-core` holds the matching claim that each enum's serde form and its
// `as_wire` name are the same string; this test is the end-to-end half,
// against the real binary.
#[test]
fn a_transition_can_be_added_and_shown() {
    let d = tempfile::tempdir().unwrap();
    let _repo = project(&d);
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

    assert_eq!(json["from"].as_str(), Some("review"), "got {json}");
    assert_eq!(json["to"].as_str(), Some("done"), "got {json}");
    assert_eq!(json["regret"].as_str(), Some("low"), "got {json}");
}

#[test]
fn a_transition_naming_a_nonexistent_gate_is_refused_and_names_the_id() {
    let d = tempfile::tempdir().unwrap();
    let _repo = project(&d);
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

#[test]
fn an_iri_typed_in_uppercase_names_the_same_item() {
    let d = tempfile::tempdir().unwrap();
    let _repo = project(&d);
    cli(&d)
        .args([
            "gate",
            "add",
            "--project",
            "1",
            "--name",
            "g",
            "--glob",
            "*.rs",
            "--program",
            "true",
        ])
        .assert()
        .success();
    // `gate show 1` prints the gate's JSON, whose `id` is the full IRI.
    let shown = cli(&d).args(["gate", "show", "1"]).output().unwrap();
    let json: serde_json::Value = serde_json::from_slice(&shown.stdout).unwrap();
    let id = json["id"].as_str().unwrap().to_string();
    cli(&d)
        .args(["gate", "show", &id.to_uppercase()])
        .assert()
        .success();
}

#[test]
fn handle_input_at_the_edges_is_refused_by_name() {
    let d = tempfile::tempdir().unwrap();
    let _repo = project(&d);
    // The whole refusal phrase, not the bare input: `0` or `7` alone would
    // also match the temp directory in the store's path.
    for (bad, refusal) in [
        ("0", "there is no gate 0 in the store"),
        ("7", "there is no gate 7 in the store"),
        (
            "99999999999999999999",
            "`99999999999999999999` is too large to be a handle",
        ),
        ("3abc", "`3abc` is neither a handle"),
    ] {
        cli(&d)
            .args(["gate", "show", bad])
            .assert()
            .code(2)
            .stderr(contains(refusal));
    }
}

#[test]
fn a_list_over_a_project_that_does_not_exist_is_refused_not_empty() {
    let d = tempfile::tempdir().unwrap();
    let _repo = project(&d);
    // A handle that does not exist is refused at the edge, by `resolve`.
    cli(&d)
        .args(["record", "list", "--project", "9"])
        .assert()
        .code(2)
        .stderr(contains("there is no project 9 in the store"));
    // An IRI goes through to the store, whose list refuses a project it
    // never held — it must not print an empty list and exit 0.
    let stranger = "urn:uuid:0190a1b2-c3d4-7e5f-8a6b-7c8d9e0f1a2b";
    cli(&d)
        .args(["record", "list", "--project", stranger])
        .assert()
        .code(2)
        .stderr(contains("no store holds").and(contains(stranger)));
}
