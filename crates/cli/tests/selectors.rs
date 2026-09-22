//! C6: `gate add` reached only one of the three selector kinds, so a gate
//! could only ever examine a glob. `Changed` and `Command` were implemented
//! in `fl-exec`, tested there, and unreachable by any user.
//!
//! These drive the real binary, because "implemented and tested in the
//! library" was exactly the state that hid the gap.

use assert_cmd::Command;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;
use std::fs;
use std::path::Path;
use std::process::Command as Sys;

struct F {
    _home: tempfile::TempDir,
    repo: tempfile::TempDir,
    db: String,
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

fn fixture() -> F {
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
    fn project(&self) {
        self.cli()
            .args(["project", "add", &self.repo.path().display().to_string()])
            .assert()
            .success();
    }
    fn script(&self, name: &str, body: &str) -> String {
        let p = self.repo.path().join(name);
        fs::write(&p, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        }
        p.display().to_string()
    }
}

#[test]
fn a_gate_can_examine_what_changed_since_a_ref() {
    let f = fixture();
    f.project();
    fs::write(f.repo.path().join("src/a.rs"), "fn a() { let _ = 1; }").unwrap();
    git(f.repo.path(), &["add", "-A"]);
    git(f.repo.path(), &["commit", "-qm", "touch one file"]);

    f.cli()
        .args([
            "gate",
            "add",
            "--project",
            "1",
            "--name",
            "changed",
            "--changed-since",
            "HEAD~1",
            "--program",
            "true",
        ])
        .assert()
        .success();
    // One file changed, so the gate examines exactly one — not the two in
    // the tree, which is what a glob would have found.
    f.cli()
        .args(["gate", "run", "2"])
        .assert()
        .success()
        .stdout(contains("PASS").and(contains("1 examined")));
}

#[test]
fn a_gate_can_take_its_population_from_a_program() {
    let f = fixture();
    f.project();
    let lister = f.script("list.sh", "#!/bin/sh\necho src/a.rs\necho docs.md\n");

    f.cli()
        .args([
            "gate",
            "add",
            "--project",
            "1",
            "--name",
            "listed",
            "--population-from",
            &lister,
            "--program",
            "true",
        ])
        .assert()
        .success();
    f.cli()
        .args(["gate", "run", "2"])
        .assert()
        .success()
        .stdout(contains("2 examined"));
}

// ⚠ The defect this reaches is not the missing flag, it is what the flag
// exposes. `resolve` read the population program's stdout and never looked
// at its exit status, so a lister that FAILED produced zero paths and the
// gate refused with `empty_population` — blaming the file tree for a fault
// in the listing program. An unknown population is not an empty one, and the
// two have different exit codes: 2, not 1.
#[test]
fn a_population_program_that_fails_is_an_error_and_never_an_empty_population() {
    let f = fixture();
    f.project();
    let broken = f.script(
        "broken.sh",
        "#!/bin/sh\necho 'fatal: not a git repository' >&2\nexit 128\n",
    );

    f.cli()
        .args([
            "gate",
            "add",
            "--project",
            "1",
            "--name",
            "broken-lister",
            "--population-from",
            &broken,
            "--program",
            "true",
        ])
        .assert()
        .success();
    f.cli()
        .args(["gate", "run", "2"])
        .assert()
        .code(2)
        .stdout(contains("ERROR"))
        .stdout(contains("128"))
        .stdout(contains("unknown, not empty"))
        .stdout(contains("empty_population").not());
}

#[test]
fn a_gate_that_does_not_say_what_it_examines_is_refused() {
    let f = fixture();
    f.project();
    f.cli()
        .args([
            "gate",
            "add",
            "--project",
            "1",
            "--name",
            "g",
            "--program",
            "true",
        ])
        .assert()
        // ⚠ Not `.failure()` plus the three flag names. The fallback arm in
        // `gate add` names those same three flags, so this assertion passed
        // against a PANIC when the clap group's `required(true)` was removed
        // — a test that could not tell a refusal from a crash. Pin clap's own
        // wording, and rule the crash out explicitly.
        .code(2)
        .stderr(
            contains("required arguments were not provided")
                .and(contains("--glob"))
                .and(contains("--changed-since"))
                .and(contains("--population-from"))
                .and(contains("panicked").not()),
        );
}

#[test]
fn two_populations_at_once_are_refused_rather_than_one_winning() {
    let f = fixture();
    f.project();
    f.cli()
        .args([
            "gate",
            "add",
            "--project",
            "1",
            "--name",
            "g",
            "--glob",
            "src/**/*.rs",
            "--changed-since",
            "HEAD",
            "--program",
            "true",
        ])
        .assert()
        .failure()
        .stderr(contains("cannot be used with"));
}

#[test]
fn a_population_argument_without_a_population_program_is_refused() {
    let f = fixture();
    f.project();
    f.cli()
        .args([
            "gate",
            "add",
            "--project",
            "1",
            "--name",
            "g",
            "--glob",
            "src/**/*.rs",
            "--population-arg",
            "-x",
            "--program",
            "true",
        ])
        .assert()
        .failure()
        .stderr(contains("--population-from"));
}

// C3: `project add` stored whatever string it was handed. It never checked
// that the path existed, let alone that it was a git working tree — while
// docs/getting-started.md said in print: "A project is a git working tree.
// The tool refuses anything that isn't one." A project registered this way
// fails later, at `gate add`, with an error about git rather than about the
// path that was wrong.
#[test]
fn a_project_root_that_is_not_a_directory_is_refused_at_registration() {
    let f = fixture();
    let missing = f.repo.path().join("no-such-directory");
    f.cli()
        .args(["project", "add", &missing.display().to_string()])
        .assert()
        .failure()
        .stderr(contains("does not exist"));
}

#[test]
fn a_project_root_that_is_not_a_git_working_tree_is_refused_at_registration() {
    let f = fixture();
    let plain = tempfile::tempdir().unwrap();
    f.cli()
        .args(["project", "add", &plain.path().display().to_string()])
        .assert()
        .failure()
        .stderr(contains("git working tree").and(contains("provenance")));
}

// C4: `transition add --gate N` checked that gate N existed and never that it
// belonged to the project being configured. A transition in project 1 could
// name a gate in project 2, which would then be resolved against project 1's
// working tree — a population enumerated from the wrong repository entirely.
#[test]
fn a_transition_cannot_name_a_gate_from_another_project() {
    let f = fixture();
    let other = tempfile::tempdir().unwrap();
    git(other.path(), &["init", "-q"]);
    git(other.path(), &["config", "user.email", "t@example.com"]);
    git(other.path(), &["config", "user.name", "t"]);
    fs::write(other.path().join("x.rs"), "fn x() {}").unwrap();
    git(other.path(), &["add", "-A"]);
    git(other.path(), &["commit", "-qm", "first"]);

    f.project(); // project 1, f.repo
    f.cli()
        .args(["project", "add", &other.path().display().to_string()])
        .assert()
        .success(); // project 2
    f.cli()
        .args([
            "gate",
            "add",
            "--project",
            "2",
            "--name",
            "theirs",
            "--glob",
            "*.rs",
            "--program",
            "true",
        ])
        .assert()
        .success(); // gate 3, in project 2

    f.cli()
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
            "3",
        ])
        .assert()
        .failure()
        .stderr(contains("project 2").and(contains("project 1")));
}
