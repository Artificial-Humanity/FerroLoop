use assert_cmd::Command;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;
use std::fs;
use std::path::Path;
use std::process::Command as Sys;

struct Fixture {
    _home: tempfile::TempDir,
    repo: tempfile::TempDir,
    db: String,
}

fn git(dir: &Path, args: &[&str]) {
    let ok = Sys::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
        .status
        .success();
    assert!(ok, "git {args:?} failed");
}

fn fixture() -> Fixture {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q"]);
    git(repo.path(), &["config", "user.email", "t@example.com"]);
    git(repo.path(), &["config", "user.name", "t"]);
    fs::create_dir_all(repo.path().join("src")).unwrap();
    fs::write(repo.path().join("src/a.rs"), "fn a() {}").unwrap();
    fs::write(repo.path().join("docs.md"), "one").unwrap();
    git(repo.path(), &["add", "-A"]);
    git(repo.path(), &["commit", "-qm", "first"]);
    let db = home.path().join("t.redb").display().to_string();
    Fixture {
        _home: home,
        repo,
        db,
    }
}

impl Fixture {
    fn cli(&self) -> Command {
        let mut c = Command::cargo_bin("fl").unwrap();
        c.arg("--db").arg(&self.db);
        c
    }

    /// Register the project, one gate over `src/**/*.rs`, and a transition.
    fn setup(&self, program: &str, regret: &str) {
        self.cli()
            .args(["project", "add", &self.repo.path().display().to_string()])
            .assert()
            .success();
        self.cli()
            .args([
                "gate",
                "add",
                "--project",
                "1",
                "--name",
                "g",
                "--kind",
                "command",
                "--glob",
                "src/**/*.rs",
                "--program",
                program,
            ])
            .assert()
            .success();
        self.cli()
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
                regret,
                "--gate",
                "2",
            ])
            .assert()
            .success();
    }
}

#[test]
fn a_passing_gate_exits_zero_and_prints_the_population() {
    let f = fixture();
    f.setup("true", "high");
    f.cli()
        .args(["check", "launch", "--project", "1"])
        .assert()
        .success()
        .stdout(contains("1"));
}

#[test]
fn a_failing_gate_exits_one() {
    let f = fixture();
    f.setup("false", "low");
    f.cli()
        .args(["check", "launch", "--project", "1"])
        .assert()
        .code(1);
}

#[test]
fn a_broken_gate_exits_two_and_is_never_reported_as_a_pass() {
    let f = fixture();
    f.setup("definitely-not-a-real-program-9f3x", "low");
    f.cli()
        .args(["check", "launch", "--project", "1"])
        .assert()
        .code(2)
        .stdout(contains("ERROR"));
}

// REQUIRED TEST 3 (spec §9), part one: a change outside the population does
// not make the gate stale.
#[test]
fn a_commit_outside_the_population_leaves_a_high_regret_gate_fresh() {
    let f = fixture();
    f.setup("true", "high");
    fs::write(f.repo.path().join("docs.md"), "two").unwrap();
    git(f.repo.path(), &["add", "-A"]);
    git(f.repo.path(), &["commit", "-qm", "docs only"]);

    f.cli()
        .args(["check", "launch", "--project", "1"])
        .assert()
        .success();
}

// REQUIRED TEST 3 (spec §9), part two: a change inside the population makes
// it stale, and stale fails at high regret.
#[test]
fn a_commit_inside_the_population_fails_a_high_regret_gate_as_stale() {
    let f = fixture();
    f.setup("true", "high");
    fs::write(f.repo.path().join("src/a.rs"), "fn a() { let _ = 1; }").unwrap();
    git(f.repo.path(), &["add", "-A"]);
    git(f.repo.path(), &["commit", "-qm", "code changed"]);

    f.cli()
        .args(["check", "launch", "--project", "1"])
        .assert()
        .code(1)
        // ⚠ Not `contains("stale")`. `check` appends a staleness NOTE —
        // `  (stale)` — to the same line, so the bare word matches even when
        // the gate failed for some other reason. Measured: mutating
        // `stale.rs` to report `FailReason::Predicate` left `contains("stale")`
        // green. Pin the reason FIELD, which only the reason can produce.
        .stdout(contains("stale, 1 examined"));
}

#[test]
fn the_same_stale_gate_only_warns_at_low_regret() {
    let f = fixture();
    f.setup("true", "low");
    fs::write(f.repo.path().join("src/a.rs"), "fn a() { let _ = 1; }").unwrap();
    git(f.repo.path(), &["add", "-A"]);
    git(f.repo.path(), &["commit", "-qm", "code changed"]);

    f.cli()
        .args(["check", "launch", "--project", "1"])
        .assert()
        .success()
        // At low regret the gate still PASSES and carries the note, so here
        // the note is exactly what is being asserted. Spelled out because its
        // high-regret sibling above means the opposite thing by the same word.
        .stdout(contains(
            "(stale: the gate's population moved since it was stamped)",
        ));
}

#[test]
fn affirming_the_gate_clears_the_staleness() {
    let f = fixture();
    f.setup("true", "high");
    fs::write(f.repo.path().join("src/a.rs"), "fn a() { let _ = 1; }").unwrap();
    git(f.repo.path(), &["add", "-A"]);
    git(f.repo.path(), &["commit", "-qm", "code changed"]);
    f.cli()
        .args(["check", "launch", "--project", "1"])
        .assert()
        .code(1);

    f.cli()
        .args(["gate", "affirm", "2", "--by", "tester"])
        .assert()
        .success();
    f.cli()
        .args(["check", "launch", "--project", "1"])
        .assert()
        .success();
}

// C5: `check` refuses the transition, and `record move` performed the very
// state change those gates exist to protect — reading nothing, running
// nothing, and exiting 0. The product's whole claim is "gate an action
// before it costs you"; this was the action, ungated.
#[test]
fn a_record_cannot_be_moved_through_a_transition_whose_gates_fail() {
    let f = fixture();
    f.setup("false", "high"); // gate 2 fails; transition `launch`: review -> done
    f.cli()
        .args(["record", "add", "--project", "1", "--title", "work"])
        .assert()
        .success(); // id 3
    f.cli()
        .args(["record", "move", "3", "--to", "review"])
        .assert()
        .success();

    // `check` says no.
    f.cli()
        .args(["check", "launch", "--project", "1"])
        .assert()
        .code(1);

    // So the move must say no too, in the same words and with the same code.
    f.cli()
        .args(["record", "move", "3", "--to", "done"])
        .assert()
        .code(1)
        .stdout(contains("FAIL").and(contains("launch")));

    // And the record must not have moved.
    f.cli()
        .args(["record", "list", "--project", "1"])
        .assert()
        .success()
        .stdout(contains("review"));
}

#[test]
fn the_same_move_is_allowed_once_the_gate_passes() {
    let f = fixture();
    f.setup("true", "high");
    f.cli()
        .args(["record", "add", "--project", "1", "--title", "work"])
        .assert()
        .success(); // id 3
    f.cli()
        .args(["record", "move", "3", "--to", "review"])
        .assert()
        .success();
    f.cli()
        .args(["record", "move", "3", "--to", "done"])
        .assert()
        .success()
        .stdout(contains("done"));
}

// The other half, and the reason this is not simply "every move is gated":
// a move no transition declares has nothing to bypass. `todo -> review` is
// not declared here, so it proceeds — and says so, rather than implying a
// check ran.
#[test]
fn a_move_no_transition_declares_is_allowed_and_says_it_was_not_gated() {
    let f = fixture();
    f.setup("false", "high"); // only `review -> done` is declared
    f.cli()
        .args(["record", "add", "--project", "1", "--title", "work"])
        .assert()
        .success(); // id 3
    f.cli()
        .args(["record", "move", "3", "--to", "doing"])
        .assert()
        .success()
        .stdout(contains("ungated"));
}
