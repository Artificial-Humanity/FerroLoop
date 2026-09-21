use assert_cmd::Command;
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
        let mut c = Command::cargo_bin("flctl").unwrap();
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
        .stdout(contains("stale"));
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
        .stdout(contains("stale"));
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
