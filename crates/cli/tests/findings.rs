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
            .success()
    );
}

struct F {
    _home: tempfile::TempDir,
    repo: tempfile::TempDir,
    db: String,
}

fn fixture() -> F {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q"]);
    git(repo.path(), &["config", "user.email", "t@example.com"]);
    git(repo.path(), &["config", "user.name", "t"]);
    fs::create_dir_all(repo.path().join("src")).unwrap();
    fs::write(repo.path().join("src/a.rs"), "fn a() {}").unwrap();
    git(repo.path(), &["add", "-A"]);
    git(repo.path(), &["commit", "-qm", "first"]);
    let db = home.path().join("t.redb").display().to_string();
    F {
        _home: home,
        repo,
        db,
    }
}

impl F {
    fn cli(&self) -> Command {
        let mut c = Command::cargo_bin("fl").unwrap();
        c.arg("--db").arg(&self.db);
        c
    }
    fn gate(&self, name: &str, program: &str) {
        self.cli()
            .args([
                "gate",
                "add",
                "--project",
                "1",
                "--name",
                name,
                "--kind",
                "command",
                "--glob",
                "src/**/*.rs",
                "--program",
                program,
            ])
            .assert()
            .success();
    }
    fn setup(&self) {
        self.cli()
            .args(["project", "add", &self.repo.path().display().to_string()])
            .assert()
            .success();
        self.cli()
            .args(["record", "add", "--project", "1", "--title", "work"])
            .assert()
            .success();
    }
}

// REQUIRED TEST 5 (spec §10), at the CLI.
#[test]
fn a_passing_gate_is_refused_as_a_reproduction() {
    let f = fixture();
    f.setup();
    f.gate("green", "true"); // id 3
    f.cli()
        .args([
            "finding", "raise", "--record", "2", "--claim", "c", "--by", "rev",
        ])
        .assert()
        .success(); // id 4
    f.cli()
        .args(["finding", "reproduce", "4", "--gate", "3"])
        .assert()
        .failure()
        .stderr(contains("PASSES").and(contains("not a reproduction")));
    f.cli()
        .args(["finding", "list", "--project", "1"])
        .assert()
        .success()
        .stdout(contains("raised"));
}

// REQUIRED TEST 6 (spec §10), at the CLI.
#[test]
fn a_raised_finding_cannot_be_assigned() {
    let f = fixture();
    f.setup();
    f.cli()
        .args([
            "finding", "raise", "--record", "2", "--claim", "c", "--by", "rev",
        ])
        .assert()
        .success(); // id 3
    f.cli()
        .args(["finding", "assign", "3", "--to", "fixer"])
        .assert()
        .failure()
        .stderr(contains("no reproduction").and(contains("withdraw")));
}

// REQUIRED TEST 7 (spec §10), at the CLI.
#[test]
fn a_repair_that_breaks_a_neighbour_does_not_close_the_finding() {
    let f = fixture();
    f.setup();
    f.gate("neighbour", "true"); // id 3
    f.gate("repro", "false"); // id 4
    // Give the neighbour a last_pass_commit.
    f.cli().args(["gate", "run", "3"]).assert().success();

    f.cli()
        .args([
            "finding", "raise", "--record", "2", "--claim", "c", "--by", "rev",
        ])
        .assert()
        .success(); // id 5
    f.cli()
        .args(["finding", "reproduce", "5", "--gate", "4"])
        .assert()
        .success();
    f.cli()
        .args(["finding", "assign", "5", "--to", "fixer"])
        .assert()
        .success();

    // The bad repair: the reproduction goes green, the neighbour goes red.
    f.cli()
        .args(["gate", "set-program", "4", "--program", "true"])
        .assert()
        .success();
    f.cli()
        .args(["gate", "set-program", "3", "--program", "false"])
        .assert()
        .success();

    f.cli()
        .args(["finding", "verify", "5"])
        .assert()
        .code(1)
        .stdout(contains("REGRESSION").and(contains("neighbour")));
}

#[test]
fn a_clean_repair_closes_the_finding() {
    let f = fixture();
    f.setup();
    f.gate("repro", "false"); // id 3
    f.cli()
        .args([
            "finding", "raise", "--record", "2", "--claim", "c", "--by", "rev",
        ])
        .assert()
        .success(); // id 4
    f.cli()
        .args(["finding", "reproduce", "4", "--gate", "3"])
        .assert()
        .success();
    f.cli()
        .args(["finding", "assign", "4", "--to", "fixer"])
        .assert()
        .success();
    f.cli()
        .args(["gate", "set-program", "3", "--program", "true"])
        .assert()
        .success();
    f.cli()
        .args(["finding", "verify", "4"])
        .assert()
        .success()
        .stdout(contains("CLOSED"));
    f.cli()
        .args(["finding", "list", "--project", "1"])
        .assert()
        .success()
        .stdout(contains("fixed"));
}

// REQUIRED TEST 8 (spec §10), at the CLI.
#[test]
fn a_withdrawal_is_counted_against_the_reviewer_and_is_readable() {
    let f = fixture();
    f.setup();
    for claim in ["a", "b"] {
        f.cli()
            .args([
                "finding", "raise", "--record", "2", "--claim", claim, "--by", "hasty",
            ])
            .assert()
            .success();
    }
    f.cli()
        .args(["finding", "withdraw", "3", "--reason", "not concrete"])
        .assert()
        .success();
    f.cli()
        .args(["finding", "withdraw", "4", "--reason", "not concrete"])
        .assert()
        .success();

    f.cli()
        .args(["finding", "list", "--project", "1", "--state", "withdrawn"])
        .assert()
        .success()
        .stdout(contains("hasty").and(contains("withdrawn: 2")));
}

// Fix-wave finding 2 (spec §7): a verify that ran no neighbours must never
// be byte-identical to a verify that ran several, and REPRODUCTION must
// carry its population the way `check` already does.
#[test]
fn verify_prints_the_reproduction_population_and_a_neighbour_summary_even_at_zero() {
    let f = fixture();
    f.setup();
    f.gate("repro", "false"); // id 3
    f.cli()
        .args([
            "finding", "raise", "--record", "2", "--claim", "c", "--by", "rev",
        ])
        .assert()
        .success(); // id 4
    f.cli()
        .args(["finding", "reproduce", "4", "--gate", "3"])
        .assert()
        .success();
    f.cli()
        .args(["finding", "assign", "4", "--to", "fixer"])
        .assert()
        .success();
    f.cli()
        .args(["gate", "set-program", "3", "--program", "true"])
        .assert()
        .success();
    f.cli()
        .args(["finding", "verify", "4"])
        .assert()
        .success()
        .stdout(
            contains("REPRODUCTION\tpasses over 1 items")
                .and(contains("NEIGHBOURS\t0 checked, 0 regressed")),
        );
}

// The other half: a neighbour that stays green must still be counted in the
// NEIGHBOURS summary, not just neighbours that regress.
#[test]
fn verify_counts_a_checked_neighbour_even_when_it_stays_green() {
    let f = fixture();
    f.setup();
    f.gate("neighbour", "true"); // id 3
    f.gate("repro", "false"); // id 4
    f.cli().args(["gate", "run", "3"]).assert().success();
    f.cli()
        .args([
            "finding", "raise", "--record", "2", "--claim", "c", "--by", "rev",
        ])
        .assert()
        .success(); // id 5
    f.cli()
        .args(["finding", "reproduce", "5", "--gate", "4"])
        .assert()
        .success();
    f.cli()
        .args(["finding", "assign", "5", "--to", "fixer"])
        .assert()
        .success();
    f.cli()
        .args(["gate", "set-program", "4", "--program", "true"])
        .assert()
        .success();
    f.cli()
        .args(["finding", "verify", "5"])
        .assert()
        .success()
        .stdout(contains("NEIGHBOURS\t1 checked, 0 regressed"));
}

// ⚠ `finding verify` used to render the verdict with `describe().1`, throwing
// away the label. A broken instrument then printed as "still fails", which
// claims evidence the run never produced — the exact opposite of what
// `ReproductionErrored` says ("a broken instrument proves nothing in either
// direction"). `check` and `gate run` printed ERROR for the same verdict.
// Nothing gated it, so a one-token revert restored it silently.
#[test]
fn verify_says_error_when_the_instrument_broke_and_fail_when_the_defect_is_real() {
    let f = fixture();
    f.setup();
    let script = f.repo.path().join("repro.sh");
    fs::write(&script, "#!/bin/sh\nexit 1\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    }
    f.gate("repro", &script.display().to_string()); // id 3
    f.cli()
        .args([
            "finding", "raise", "--record", "2", "--claim", "c", "--by", "rev",
        ])
        .assert()
        .success(); // id 4
    f.cli()
        .args(["finding", "reproduce", "4", "--gate", "3"])
        .assert()
        .success();
    f.cli()
        .args(["finding", "assign", "4", "--to", "fixer"])
        .assert()
        .success();

    // The defect is still there: a real failure, and it says so.
    f.cli()
        .args(["finding", "verify", "4"])
        .assert()
        .code(1)
        .stdout(contains("REPRODUCTION\tFAIL\tpredicate,").and(contains("examined")));

    // Now break the instrument itself. This must NOT read as a failure.
    fs::remove_file(&script).unwrap();
    f.cli()
        .args(["finding", "verify", "4"])
        .assert()
        .code(2)
        .stdout(contains("REPRODUCTION\tERROR\t").and(contains("could not be run")))
        .stdout(contains("still fails").not());
}
