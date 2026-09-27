//! `fl manifest` and the currency guard, driven as a black box across two
//! "machines": two private config/data homes, one authoring clone and one
//! importing clone of the same repository.

use assert_cmd::Command;
use predicates::str::contains;
use std::path::Path;
use std::process::Command as Sys;

fn git(dir: &Path, args: &[&str]) {
    let out = Sys::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn repo() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    git(d.path(), &["init", "-q"]);
    git(d.path(), &["config", "user.email", "t@example.com"]);
    git(d.path(), &["config", "user.name", "t"]);
    std::fs::write(d.path().join("a.rs"), "fn a() {}").unwrap();
    git(d.path(), &["add", "-A"]);
    git(d.path(), &["commit", "-qm", "first"]);
    d
}

/// One machine: private config and data homes.
struct Machine {
    config: tempfile::TempDir,
    data: tempfile::TempDir,
}

impl Machine {
    fn new() -> Self {
        Self {
            config: tempfile::tempdir().unwrap(),
            data: tempfile::tempdir().unwrap(),
        }
    }
    fn fl(&self, cwd: &Path) -> Command {
        let mut c = Command::cargo_bin("fl").unwrap();
        c.env("XDG_CONFIG_HOME", self.config.path())
            .env("XDG_DATA_HOME", self.data.path())
            .env_remove("FL_DB")
            .current_dir(cwd);
        c
    }
}

/// The authoring machine registers the repo, adds a gate and a transition,
/// exports, and commits the manifest.
fn authored() -> (Machine, tempfile::TempDir) {
    let m = Machine::new();
    let r = repo();
    m.fl(r.path())
        .args(["project", "add", "."])
        .assert()
        .success();
    m.fl(r.path())
        .args([
            "gate",
            "add",
            "--project",
            "1",
            "--name",
            "rs",
            "--glob",
            "*.rs",
            "--program",
            "true",
        ])
        .assert()
        .success();
    m.fl(r.path())
        .args([
            "transition",
            "add",
            "--project",
            "1",
            "--name",
            "ship",
            "--from",
            "review",
            "--to",
            "done",
            "--regret",
            "high",
            "--gate",
            "1",
        ])
        .assert()
        .success();
    m.fl(r.path())
        .args(["manifest", "export", "--project", "1"])
        .assert()
        .success()
        .stdout(contains("\trs\ttrue"));
    git(r.path(), &["add", "-A"]);
    git(r.path(), &["commit", "-qm", "manifest"]);
    (m, r)
}

fn clone_of(src: &Path) -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    git(d.path(), &["clone", "-q", src.to_str().unwrap(), "."]);
    d
}

#[test]
fn a_clean_committed_manifest_checks_as_current() {
    let (m, r) = authored();
    m.fl(r.path())
        .args(["manifest", "check", "--project", "1"])
        .assert()
        .success()
        .stdout(contains("current"));
}

#[test]
fn an_uncommitted_manifest_is_refused_by_check() {
    let m = Machine::new();
    let r = repo();
    m.fl(r.path())
        .args(["project", "add", "."])
        .assert()
        .success();
    m.fl(r.path())
        .args([
            "gate",
            "add",
            "--project",
            "1",
            "--name",
            "rs",
            "--glob",
            "*.rs",
            "--program",
            "true",
        ])
        .assert()
        .success();
    m.fl(r.path())
        .args(["manifest", "export", "--project", "1"])
        .assert()
        .success();
    m.fl(r.path())
        .args(["manifest", "check", "--project", "1"])
        .assert()
        .failure()
        .stderr(contains("commit"));
}

#[test]
fn a_gate_changed_since_export_is_refused_by_check() {
    let (m, r) = authored();
    std::fs::write(r.path().join("b.rs"), "fn b() {}").unwrap();
    git(r.path(), &["add", "-A"]);
    git(r.path(), &["commit", "-qm", "more"]);
    m.fl(r.path())
        .args(["gate", "affirm", "1"])
        .assert()
        .success();
    m.fl(r.path())
        .args(["manifest", "check", "--project", "1"])
        .assert()
        .failure()
        .stderr(contains("fl manifest export"));
}

#[test]
fn another_machine_imports_and_runs_the_gate() {
    let (_author, r) = authored();
    let other = Machine::new();
    let c = clone_of(r.path());
    other
        .fl(c.path())
        .args(["manifest", "import"])
        .assert()
        .success()
        .stdout(contains("1 added"))
        // This store numbers handles on its own, so the person needs to be
        // told which handle each imported gate landed on, by name.
        .stdout(contains("gate\t1\trs"));
    other
        .fl(c.path())
        .args(["gate", "run", "1"])
        .assert()
        .success()
        .stdout(contains("PASS"));
}

#[test]
fn export_prints_a_population_commands_program() {
    let m = Machine::new();
    let r = repo();
    m.fl(r.path())
        .args(["project", "add", "."])
        .assert()
        .success();
    m.fl(r.path())
        .args([
            "gate",
            "add",
            "--project",
            "1",
            "--name",
            "files",
            "--population-from",
            "list-files-for-export-test",
            "--program",
            "true",
        ])
        .assert()
        .success();
    m.fl(r.path())
        .args(["manifest", "export", "--project", "1"])
        .assert()
        .success()
        .stdout(contains("list-files-for-export-test"));
}

#[test]
fn a_gitignored_manifest_is_refused_by_check_naming_gitignore() {
    let m = Machine::new();
    let r = repo();
    m.fl(r.path())
        .args(["project", "add", "."])
        .assert()
        .success();
    m.fl(r.path())
        .args([
            "gate",
            "add",
            "--project",
            "1",
            "--name",
            "rs",
            "--glob",
            "*.rs",
            "--program",
            "true",
        ])
        .assert()
        .success();
    m.fl(r.path())
        .args(["manifest", "export", "--project", "1"])
        .assert()
        .success();
    std::fs::write(r.path().join(".gitignore"), ".fl/manifest.json\n").unwrap();
    git(r.path(), &["add", "-A"]);
    git(r.path(), &["commit", "-qm", "ignore the manifest"]);
    m.fl(r.path())
        .args(["manifest", "check", "--project", "1"])
        .assert()
        .failure()
        .stderr(contains(".gitignore"));
}

#[test]
fn an_imported_gate_cannot_be_affirmed_locally() {
    let (_author, r) = authored();
    let other = Machine::new();
    let c = clone_of(r.path());
    other
        .fl(c.path())
        .args(["manifest", "import"])
        .assert()
        .success();
    other
        .fl(c.path())
        .args(["gate", "affirm", "1"])
        .assert()
        .failure()
        .stderr(contains("imported from a manifest"));
}

#[test]
fn a_manifest_edited_after_import_stops_the_gate_from_running() {
    let (_author, r) = authored();
    let other = Machine::new();
    let c = clone_of(r.path());
    other
        .fl(c.path())
        .args(["manifest", "import"])
        .assert()
        .success();
    let path = c.path().join(".fl/manifest.json");
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, text.replace("\"name\": \"rs\"", "\"name\": \"rs2\"")).unwrap();
    other
        .fl(c.path())
        .args(["gate", "run", "1"])
        .assert()
        .failure()
        .stderr(contains("edited by hand"));
}

#[test]
fn a_manifest_that_moved_on_since_import_stops_the_gate_until_reimport() {
    let (author, r) = authored();
    let other = Machine::new();
    let c = clone_of(r.path());
    other
        .fl(c.path())
        .args(["manifest", "import"])
        .assert()
        .success();

    std::fs::write(r.path().join("b.rs"), "fn b() {}").unwrap();
    git(r.path(), &["add", "-A"]);
    git(r.path(), &["commit", "-qm", "more"]);
    author
        .fl(r.path())
        .args(["gate", "affirm", "1"])
        .assert()
        .success();
    author
        .fl(r.path())
        .args(["manifest", "export", "--project", "1"])
        .assert()
        .success();
    git(r.path(), &["add", "-A"]);
    git(r.path(), &["commit", "-qm", "re-export"]);
    git(c.path(), &["pull", "-q"]);

    other
        .fl(c.path())
        .args(["check", "ship", "--project", "1"])
        .assert()
        .failure()
        .stderr(contains("fl manifest import"));
    other
        .fl(c.path())
        .args(["manifest", "import"])
        .assert()
        .success()
        .stdout(contains("1 changed"));
    other
        .fl(c.path())
        .args(["gate", "run", "1"])
        .assert()
        .success();
}

/// An importing machine whose working-tree manifest moved on since import.
fn stale_import() -> (Machine, tempfile::TempDir, Machine, tempfile::TempDir) {
    let (author, r) = authored();
    let other = Machine::new();
    let c = clone_of(r.path());
    other
        .fl(c.path())
        .args(["manifest", "import"])
        .assert()
        .success();
    std::fs::write(r.path().join("b.rs"), "fn b() {}").unwrap();
    git(r.path(), &["add", "-A"]);
    git(r.path(), &["commit", "-qm", "more"]);
    author
        .fl(r.path())
        .args(["gate", "affirm", "1"])
        .assert()
        .success();
    author
        .fl(r.path())
        .args(["manifest", "export", "--project", "1"])
        .assert()
        .success();
    git(r.path(), &["add", "-A"]);
    git(r.path(), &["commit", "-qm", "re-export"]);
    git(c.path(), &["pull", "-q"]);
    (author, r, other, c)
}

#[test]
fn a_gated_move_is_refused_on_a_stale_import_and_an_ungated_one_is_not() {
    let (_a, _r, other, c) = stale_import();
    other
        .fl(c.path())
        .args(["record", "add", "--project", "1", "--title", "t"])
        .assert()
        .success();
    // todo → review: no transition covers it, so no gate runs.
    other
        .fl(c.path())
        .args(["record", "move", "1", "--to", "review"])
        .assert()
        .success();
    // review → done: `ship` covers it.
    other
        .fl(c.path())
        .args(["record", "move", "1", "--to", "done"])
        .assert()
        .failure()
        .stderr(contains("fl manifest import"));
}

#[test]
fn reproduce_and_verify_are_refused_on_a_stale_import() {
    let (_a, _r, other, c) = stale_import();
    other
        .fl(c.path())
        .args(["record", "add", "--project", "1", "--title", "t"])
        .assert()
        .success();
    other
        .fl(c.path())
        .args([
            "finding", "raise", "--record", "1", "--claim", "c", "--by", "r",
        ])
        .assert()
        .success();
    other
        .fl(c.path())
        .args(["finding", "reproduce", "1", "--gate", "1"])
        .assert()
        .failure()
        .stderr(contains("fl manifest import"));
    other
        .fl(c.path())
        .args(["finding", "verify", "1"])
        .assert()
        .failure()
        .stderr(contains("fl manifest import"));
}

#[test]
fn check_refuses_a_gate_or_transition_added_since_export() {
    let (m, r) = authored();
    m.fl(r.path())
        .args([
            "gate",
            "add",
            "--project",
            "1",
            "--name",
            "late",
            "--glob",
            "*.rs",
            "--program",
            "true",
        ])
        .assert()
        .success();
    m.fl(r.path())
        .args(["manifest", "check", "--project", "1"])
        .assert()
        .failure()
        .stderr(contains("not in the manifest"));

    let (m, r) = authored();
    m.fl(r.path())
        .args([
            "transition",
            "add",
            "--project",
            "1",
            "--name",
            "late",
            "--from",
            "todo",
            "--to",
            "doing",
            "--regret",
            "low",
            "--gate",
            "1",
        ])
        .assert()
        .success();
    m.fl(r.path())
        .args(["manifest", "check", "--project", "1"])
        .assert()
        .failure()
        .stderr(contains("transitions changed"));
}

#[test]
fn a_reimport_from_another_checkout_says_the_root_moved() {
    let (_author, r) = authored();
    let other = Machine::new();
    let c1 = clone_of(r.path());
    let c2 = clone_of(r.path());
    other
        .fl(c1.path())
        .args(["manifest", "import"])
        .assert()
        .success();
    other
        .fl(c2.path())
        .args(["manifest", "import"])
        .assert()
        .success()
        .stdout(contains("moved"));
}

#[test]
fn a_missing_manifest_on_an_importing_machine_is_refused_by_path() {
    let (_author, r) = authored();
    let other = Machine::new();
    let c = clone_of(r.path());
    other
        .fl(c.path())
        .args(["manifest", "import"])
        .assert()
        .success();
    std::fs::remove_file(c.path().join(".fl/manifest.json")).unwrap();
    other
        .fl(c.path())
        .args(["gate", "run", "1"])
        .assert()
        .failure()
        .stderr(contains(".fl/manifest.json"));
}
