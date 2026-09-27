//! The CLI with a project bound to GitHub Issues, against the in-process
//! fake. The `fl` binary reaches the fake through `FL_GITHUB_API_URL`.

use assert_cmd::Command;
use fl_github::fake::FakeGithub;
use predicates::str::contains;
use std::fs;
use std::os::unix::fs::PermissionsExt;
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

struct G {
    home: tempfile::TempDir,
    repo: tempfile::TempDir,
    fake: FakeGithub,
}

/// A git repository whose `check.sh` fails while a file named `bug` exists,
/// bound to the fake's `acme/widgets` (or to nothing, when `bound` is false).
fn fixture_with(bound: bool) -> G {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q"]);
    git(repo.path(), &["config", "user.email", "t@example.com"]);
    git(repo.path(), &["config", "user.name", "t"]);
    fs::create_dir_all(repo.path().join("src")).unwrap();
    fs::write(repo.path().join("src/a.rs"), "fn a() {}").unwrap();
    let check = repo.path().join("check.sh");
    fs::write(&check, "#!/bin/sh\n[ ! -e bug ]\n").unwrap();
    fs::set_permissions(&check, fs::Permissions::from_mode(0o755)).unwrap();
    git(repo.path(), &["add", "-A"]);
    git(repo.path(), &["commit", "-qm", "first"]);
    let fake = FakeGithub::start("acme/widgets");
    let tracker = if bound {
        "tracker = { github = \"acme/widgets\", credential = \"env\" }\n"
    } else {
        ""
    };
    let cfg = format!(
        "[[project]]\nroot = \"{}\"\nstore = \"{}\"\n{tracker}",
        repo.path().canonicalize().unwrap().display(),
        home.path().join("fl.redb").display()
    );
    fs::create_dir_all(home.path().join("config/fl")).unwrap();
    fs::write(home.path().join("config/fl/config.toml"), cfg).unwrap();
    G { home, repo, fake }
}

fn fixture() -> G {
    fixture_with(true)
}

impl G {
    fn fl(&self) -> Command {
        let mut c = Command::cargo_bin("fl").unwrap();
        c.env("XDG_CONFIG_HOME", self.home.path().join("config"))
            .env("XDG_DATA_HOME", self.home.path().join("data"))
            .env("FL_GITHUB_TOKEN", "t")
            .env("FL_GITHUB_API_URL", self.fake.url())
            .env_remove("GITHUB_TOKEN")
            .env_remove("FL_DB")
            .current_dir(self.repo.path());
        c
    }

    fn project(&self) {
        self.fl().args(["project", "add", "."]).assert().success();
    }
}

#[test]
fn records_live_in_github_issues() {
    let g = fixture();
    g.project();
    g.fl()
        .args(["record", "add", "--project", "1", "--title", "fix it"])
        .assert()
        .success()
        .stdout(contains("1\tfix it"));
    assert_eq!(g.fake.issue(1).labels, vec!["fl:record", "fl:record/todo"]);
    g.fl()
        .args(["record", "list", "--project", "1"])
        .assert()
        .success()
        .stdout(contains("todo\tfix it"));
    g.fl()
        .args(["record", "move", "1", "--to", "doing"])
        .assert()
        .success();
    assert!(
        g.fake
            .issue(1)
            .labels
            .contains(&"fl:record/doing".to_string())
    );
}

#[test]
fn a_handle_may_carry_a_hash_and_an_issue_url_skips_the_local_stores() {
    let g = fixture();
    g.project();
    g.fl()
        .args(["record", "add", "--project", "1", "--title", "t"])
        .assert()
        .success();
    g.fl()
        .args(["record", "move", "#1", "--to", "doing"])
        .assert()
        .success();
    g.fl()
        .args([
            "record",
            "move",
            "https://github.com/acme/widgets/issues/1",
            "--to",
            "review",
        ])
        .assert()
        .success();
    assert!(
        g.fake
            .issue(1)
            .labels
            .contains(&"fl:record/review".to_string())
    );
}

#[test]
fn a_command_that_needs_no_tracker_never_contacts_github() {
    let g = fixture();
    g.project();
    g.fl()
        .args([
            "gate",
            "add",
            "--project",
            "1",
            "--name",
            "g",
            "--glob",
            "src/**/*.rs",
            "--program",
            "true",
        ])
        .assert()
        .success();
    g.fl()
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
            "low",
            "--gate",
            "1",
        ])
        .assert()
        .success();
    g.fl()
        .args(["gate", "list", "--project", "1"])
        .assert()
        .success();
    g.fl().args(["project", "list"]).assert().success();
    g.fl()
        .args(["check", "ship", "--project", "1"])
        .assert()
        .success();
    g.fl()
        .args(["manifest", "export", "--project", "1"])
        .assert()
        .success();
    git(g.repo.path(), &["add", ".fl"]);
    git(g.repo.path(), &["commit", "-qm", "manifest"]);
    g.fl()
        .args(["manifest", "check", "--project", "1"])
        .assert()
        .success();
    assert!(
        g.fake.state().requests.is_empty(),
        "{:?}",
        g.fake.state().requests
    );
}

#[test]
fn an_api_override_off_this_machine_is_refused() {
    let g = fixture();
    g.project();
    g.fl()
        .env("FL_GITHUB_API_URL", "http://example.com")
        .args(["record", "list", "--project", "1"])
        .assert()
        .failure()
        .stderr(contains("https://"));
}

#[test]
fn db_cannot_be_combined_with_a_github_binding() {
    let g = fixture();
    g.project();
    g.fl()
        .arg("--db")
        .arg(g.home.path().join("other.redb"))
        .args(["record", "list", "--project", "1"])
        .assert()
        .failure()
        .stderr(contains("Drop --db"));
}

#[test]
fn a_renamed_repository_is_announced_on_stderr() {
    let g = fixture();
    g.project();
    g.fl()
        .args(["record", "add", "--project", "1", "--title", "t"])
        .assert()
        .success();
    g.fake.rename("acme/gadgets");
    g.fl()
        .args(["record", "list", "--project", "1"])
        .assert()
        .success()
        .stderr(contains("now `acme/gadgets`"));
}

#[test]
fn a_missing_credential_is_refused_naming_where_fl_looked() {
    let g = fixture();
    g.project();
    g.fl()
        .env_remove("FL_GITHUB_TOKEN")
        .args(["record", "list", "--project", "1"])
        .assert()
        .failure()
        .stderr(contains("FL_GITHUB_TOKEN or GITHUB_TOKEN"));
}

#[test]
fn an_unknown_tracker_key_is_refused_not_ignored() {
    let g = fixture();
    let path = g.home.path().join("config/fl/config.toml");
    let cfg = fs::read_to_string(&path).unwrap().replace(
        "credential = \"env\" }",
        "credential = \"env\", extra = 1 }",
    );
    assert!(cfg.contains("extra = 1"), "the edit must land");
    fs::write(&path, cfg).unwrap();
    g.fl().args(["project", "list"]).assert().failure();
}

#[test]
fn an_unbound_project_keeps_its_local_tracker() {
    let g = fixture_with(false);
    g.project();
    g.fl()
        .args(["record", "add", "--project", "1", "--title", "t"])
        .assert()
        .success();
    assert!(g.fake.state().requests.is_empty());
}
