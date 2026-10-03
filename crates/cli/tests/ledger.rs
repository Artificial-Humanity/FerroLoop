//! The CLI with a project whose ledger is the GitHub ledger
//! (`ledger = "github"`, GitHub ledger spec), against the in-process fake.
//! The `fl` binary reaches the fake through `FL_GITHUB_API_URL`.

use assert_cmd::Command;
use fl_core::store::{Catalog, Ledger};
use fl_github::fake::FakeGithub;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command as Sys;

/// What every pre-flight refusal starts with (GitHub ledger spec §2.4).
const PREFLIGHT: &str = "refused before any gate or adapter ran: ";

/// What a comment that could not be posted warns. No other path writes it.
const NOT_POSTED: &str = "warning: the decision's comment was not posted on issue ";

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

/// One machine: a config home and a store of its own.
struct Machine {
    home: tempfile::TempDir,
}

impl Machine {
    fn store(&self) -> PathBuf {
        self.home.path().join("fl.redb")
    }
}

/// A git working tree whose `check.sh` fails while a file named `bug`
/// exists, the fake GitHub holding `acme/widgets`, and the first machine.
struct World {
    repo: tempfile::TempDir,
    fake: FakeGithub,
    one: Machine,
}

impl World {
    /// Bound to `acme/widgets` with the GitHub ledger.
    fn new() -> World {
        World::bound(true)
    }

    /// Bound to `acme/widgets`; with the GitHub ledger only when `ledger`.
    fn bound(ledger: bool) -> World {
        let repo = tempfile::tempdir().unwrap();
        git(repo.path(), &["init", "-q"]);
        git(repo.path(), &["config", "user.email", "t@example.com"]);
        git(repo.path(), &["config", "user.name", "t"]);
        fs::create_dir_all(repo.path().join("src")).unwrap();
        fs::write(repo.path().join("src/a.rs"), "fn a() {}").unwrap();
        let check = repo.path().join("check.sh");
        // `checked` is every run's excerpt.
        fs::write(&check, "#!/bin/sh\necho checked\n[ ! -e bug ]\n").unwrap();
        fs::set_permissions(&check, fs::Permissions::from_mode(0o755)).unwrap();
        git(repo.path(), &["add", "-A"]);
        git(repo.path(), &["commit", "-qm", "first"]);
        let w = World {
            repo,
            fake: FakeGithub::start("acme/widgets"),
            one: Machine {
                home: tempfile::tempdir().unwrap(),
            },
        };
        w.configure(&w.one, ledger);
        w
    }

    /// Writes `m`'s config: the shared working tree, `m`'s own store, and
    /// the binding.
    fn configure(&self, m: &Machine, ledger: bool) {
        let ledger = if ledger { ", ledger = \"github\"" } else { "" };
        let cfg = format!(
            "[[project]]\nroot = \"{}\"\nstore = \"{}\"\n\
             tracker = {{ github = \"acme/widgets\", credential = \"env\"{ledger} }}\n",
            self.repo.path().canonicalize().unwrap().display(),
            m.store().display()
        );
        fs::create_dir_all(m.home.path().join("config/fl")).unwrap();
        fs::write(m.home.path().join("config/fl/config.toml"), cfg).unwrap();
    }

    /// A second machine on the same working tree and repository, with the
    /// GitHub ledger.
    fn machine(&self) -> Machine {
        let m = Machine {
            home: tempfile::tempdir().unwrap(),
        };
        self.configure(&m, true);
        m
    }

    fn fl(&self) -> Command {
        self.fl_on(&self.one)
    }

    fn fl_on(&self, m: &Machine) -> Command {
        let mut c = Command::cargo_bin("fl").unwrap();
        c.env("XDG_CONFIG_HOME", m.home.path().join("config"))
            .env("XDG_DATA_HOME", m.home.path().join("data"))
            .env("FL_GITHUB_TOKEN", "t")
            .env("FL_GITHUB_API_URL", self.fake.url())
            .env_remove("GITHUB_TOKEN")
            .env_remove("FL_DB")
            .current_dir(self.repo.path());
        c
    }

    fn init(&self) {
        self.fl()
            .args(["github", "ledger", "init"])
            .assert()
            .success();
    }

    /// Exports the manifest and commits it, as the person does after `init`.
    fn export(&self) {
        self.fl()
            .args(["manifest", "export", "--project", "1"])
            .assert()
            .success();
        git(self.repo.path(), &["add", ".fl"]);
        git(self.repo.path(), &["commit", "-qm", "manifest"]);
    }

    /// A project and one record (#1), which binds the repository's node.
    fn project_and_record(&self) {
        self.fl().args(["project", "add", "."]).assert().success();
        self.fl()
            .args(["record", "add", "--project", "1", "--title", "work"])
            .assert()
            .success();
    }

    /// A project with one gate named `gate` over `src/**/*.rs`, running
    /// `program`, a transition `launch` from `todo` to `doing` over it, and
    /// one record (#1).
    fn gated_with(&self, gate: &str, program: &str) {
        self.fl().args(["project", "add", "."]).assert().success();
        self.fl()
            .args([
                "gate",
                "add",
                "--project",
                "1",
                "--name",
                gate,
                "--glob",
                "src/**/*.rs",
                "--program",
                program,
            ])
            .assert()
            .success();
        self.fl()
            .args([
                "transition",
                "add",
                "--project",
                "1",
                "--name",
                "launch",
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
        self.fl()
            .args(["record", "add", "--project", "1", "--title", "work"])
            .assert()
            .success();
    }

    /// `gated_with(gate, "./check.sh")`.
    fn gated_as(&self, gate: &str) {
        self.gated_with(gate, "./check.sh");
    }

    /// `gated_as("no-bug")`.
    fn gated(&self) {
        self.gated_as("no-bug");
    }

    /// `gated`, the ledger set up, and the manifest committed: every
    /// decision can publish.
    fn ready(&self) {
        self.gated();
        self.init();
        self.export();
    }

    /// `ready`, a file named `bug`, and a finding (#2) on record #1.
    fn finding_raised(&self) {
        self.ready();
        fs::write(self.repo.path().join("bug"), "").unwrap();
        self.fl()
            .args([
                "finding", "raise", "--record", "1", "--claim", "a bug", "--by", "reviewer",
            ])
            .assert()
            .success();
    }

    /// The ledger's files under `area/`, path and text, in path order.
    fn ledger_files_in(&self, area: &str) -> Vec<(String, String)> {
        let prefix = format!("{area}/");
        self.fake
            .ledger_files()
            .into_iter()
            .filter(|(p, _)| p.starts_with(&prefix))
            .collect()
    }

    /// The one ledger file under `area/`, path and text — refused unless
    /// there is exactly one.
    fn only_file_in(&self, area: &str) -> (String, String) {
        let mut files = self.ledger_files_in(area);
        assert_eq!(files.len(), 1, "one file under {area}/: {files:?}");
        files.remove(0)
    }

    /// Every decision id the ledger holds, in path order.
    fn decision_ids(&self) -> Vec<String> {
        self.ledger_files_in("decisions")
            .iter()
            .flat_map(|(_, text)| {
                text.lines()
                    .map(|l| {
                        serde_json::from_str::<serde_json::Value>(l).unwrap()["id"]
                            .as_str()
                            .unwrap()
                            .to_string()
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// How many gate runs `m`'s store holds, over every gate.
    fn runs(&self, m: &Machine) -> usize {
        let store = fl_store::RedbStore::open(&m.store()).unwrap();
        store
            .list_projects()
            .unwrap()
            .iter()
            .flat_map(|p| store.list_gates(&p.id).unwrap())
            .map(|g| store.gate_runs(&g.id).unwrap().len())
            .sum()
    }

    /// How many attempts `m`'s store holds.
    fn attempts(&self, m: &Machine) -> usize {
        let store = fl_store::RedbStore::open(&m.store()).unwrap();
        store
            .list_projects()
            .unwrap()
            .iter()
            .map(|p| store.attempts(&p.id).unwrap().len())
            .sum()
    }
}

// Spec §6.1 step 1.
#[test]
fn init_refuses_a_binding_that_does_not_name_the_github_ledger() {
    let w = World::bound(false);
    w.fl()
        .args(["github", "ledger", "init"])
        .assert()
        .code(2)
        .stderr(contains("does not name the GitHub ledger"));
    assert_eq!(w.fake.ledger_head(), None);
}

// Spec §6.1 steps 3, 4 and 7-9; a second run changes nothing.
#[test]
fn init_creates_the_ledger_and_says_what_the_person_does_next() {
    let w = World::new();
    w.fl()
        .args(["github", "ledger", "init"])
        .assert()
        .success()
        .stdout(
            contains("created\tfl/ledger\t")
                .and(contains("mode: detection-only"))
                .and(contains("Next: run `fl manifest export")),
        );
    let head = w.fake.ledger_head().expect("the branch");
    // Spec §6.1 step 6: set up, and it stops — the mode, not the guidance.
    w.fl()
        .args(["github", "ledger", "init"])
        .assert()
        .success()
        .stdout(
            contains("set up\tfl/ledger\t")
                // Spec §6.2: detection-only names what is missing.
                .and(contains("\nmode\tdetection-only\t"))
                .and(contains("this machine's cut-over is recorded").not())
                .and(contains("Protect the default branch").not())
                .and(contains("Next:").not()),
        );
    assert_eq!(
        w.fake.ledger_head(),
        Some(head),
        "a second run changes nothing"
    );
}

// ⚠ The mode is read before anything is created: a rules read that fails
// leaves no branch, so the run that does create the ledger is the one that
// says to export and commit the manifest.
#[test]
fn init_whose_rules_cannot_be_read_creates_nothing_and_a_rerun_says_what_to_do() {
    let w = World::new();
    w.fake.state().fail_rules_next = true;
    w.fl()
        .args(["github", "ledger", "init"])
        .assert()
        .code(2)
        .stderr(contains("rules/branches/fl/ledger"));
    assert_eq!(w.fake.ledger_head(), None, "nothing was created");
    w.fl()
        .args(["github", "ledger", "init"])
        .assert()
        .success()
        .stdout(contains("created\tfl/ledger\t").and(contains("Next: run `fl manifest export")));
}

// ⚠ The mode before the manifest's import too: on a machine that imports
// the manifest, a rules read that fails leaves its store as it was.
#[test]
fn init_whose_rules_cannot_be_read_imports_nothing_on_a_machine_that_imports() {
    let w = World::new();
    w.project_and_record();
    w.init();
    w.export();
    let head = w.fake.ledger_head();
    let two = w.machine();
    w.fake.state().fail_rules_next = true;
    w.fl_on(&two)
        .args(["github", "ledger", "init"])
        .assert()
        .code(2)
        .stderr(contains("rules/branches/fl/ledger"))
        .stdout(contains("imported\t").not());
    let store = fl_store::RedbStore::open(&two.store()).unwrap();
    assert!(!store.holds_a_ledger_root().unwrap(), "no ledger root");
    assert!(
        fl_core::store::Catalog::list_projects(&store)
            .unwrap()
            .is_empty(),
        "no project"
    );
    assert_eq!(w.fake.ledger_head(), head, "nothing changed on GitHub");
}

// ⚠ The manifest first: a machine that never imported it learns the
// ledger's first commit before `init` touches GitHub, and records its own
// cut-over (spec §6.1 steps 4 and 5).
#[test]
fn a_second_machine_learns_the_ledger_from_the_manifest_and_records_its_own_cut_over() {
    let w = World::new();
    w.project_and_record();
    w.init();
    w.export();
    let head = w.fake.ledger_head();
    let two = w.machine();
    w.fl_on(&two)
        .args(["github", "ledger", "init"])
        .assert()
        .success()
        .stdout(
            contains("imported\t")
                .and(contains("set up\tfl/ledger\t"))
                .and(contains("this machine's cut-over is recorded"))
                .and(contains("confirm\t").not()),
        );
    assert_eq!(w.fake.ledger_head(), head, "nothing changed on GitHub");
}

// ⚠ Without the manifest's root, this machine would create a second
// ledger and hide the deletion (spec §6.1 step 6).
#[test]
fn a_machine_that_never_imported_the_manifest_does_not_replace_a_deleted_ledger() {
    let w = World::new();
    w.project_and_record();
    w.init();
    w.export();
    w.fake.delete_ledger();
    let two = w.machine();
    w.fl_on(&two)
        .args(["github", "ledger", "init"])
        .assert()
        .code(2)
        .stderr(contains("fl will not start a new ledger"));
    assert_eq!(w.fake.ledger_head(), None);
}

// The authoring store wrote the manifest's root itself: `init` imports
// nothing there, and runs again cleanly.
#[test]
fn init_on_the_machine_that_authors_the_project_imports_nothing() {
    let w = World::new();
    w.project_and_record();
    w.init();
    w.export();
    w.fl()
        .args(["github", "ledger", "init"])
        .assert()
        .success()
        .stdout(contains("set up\tfl/ledger\t").and(contains("imported\t").not()));
}

// Spec §6.1 step 6: a branch no machine records and no manifest names —
// the person confirms its first commit, by name.
#[test]
fn init_adopts_an_existing_branch_only_once_its_first_commit_is_confirmed() {
    let w = World::new();
    let root = w.fake.seed_ledger();
    w.fl()
        .args(["github", "ledger", "init"])
        .assert()
        .code(1)
        .stdout(contains(format!("confirm\tfl/ledger\t{root}")));
    w.fl()
        .args([
            "github",
            "ledger",
            "init",
            "--confirm",
            "0123456789abcdef0123456789abcdef01234567",
        ])
        .assert()
        .code(2)
        .stderr(contains("is not the first commit"));
    w.fl()
        .args(["github", "ledger", "init", "--confirm", &root])
        .assert()
        .success()
        .stdout(contains(format!("adopted\tfl/ledger\t{root}")));
    // Re-running with the same, matching root on a machine that already
    // adopted it succeeds: it is the ordinary "already set up" case, not a
    // second confirmation.
    w.fl()
        .args(["github", "ledger", "init", "--confirm", &root])
        .assert()
        .success()
        .stdout(contains(format!("set up\tfl/ledger\t{root}")));
}

// Spec §6.2: `whoami` states the ledger, and the mode in force.
#[test]
fn whoami_states_the_ledger_and_the_mode_in_force() {
    let local = World::bound(false);
    local
        .fl()
        .args(["github", "whoami"])
        .assert()
        .success()
        .stdout(contains("ledger\tlocal\n").and(contains("mode\t").not()));
    let w = World::new();
    w.fl()
        .args(["github", "whoami"])
        .assert()
        .success()
        .stdout(contains("ledger\tgithub\n").and(contains("mode\tdetection-only\t")));
    w.fake
        .state()
        .rulesets
        .push(fl_github::fake_git::Ruleset::on_ledger(
            "active",
            &["non_fast_forward", "deletion"],
        ));
    w.fl()
        .args(["github", "whoami"])
        .assert()
        .success()
        .stdout(contains("mode\tprotected\n"));
}

// ⚠ Spec §6.1 step 6: a confirmation is never passed over. The branch it
// confirmed was deleted since — a new ledger in its place would hide that.
#[test]
fn a_confirmation_of_a_branch_since_deleted_creates_no_new_ledger() {
    let w = World::new();
    let root = w.fake.seed_ledger();
    w.fake.delete_ledger();
    w.fl()
        .args(["github", "ledger", "init", "--confirm", &root])
        .assert()
        .code(2)
        .stderr(contains("that branch no longer exists"));
    assert_eq!(w.fake.ledger_head(), None, "nothing was created");
}

// A confirmation that names another first commit than the one this machine
// records is refused, naming both.
#[test]
fn a_confirmation_of_another_first_commit_than_the_recorded_one_is_refused() {
    let w = World::new();
    w.init();
    let head = w.fake.ledger_head().expect("the branch");
    let other = "0123456789abcdef0123456789abcdef01234567";
    w.fl()
        .args(["github", "ledger", "init", "--confirm", other])
        .assert()
        .code(2)
        .stderr(
            contains("but this machine records")
                .and(contains(other))
                .and(contains(head.as_str())),
        );
    assert_eq!(
        w.fake.ledger_head(),
        Some(head),
        "nothing changed on GitHub"
    );
}

// Spec §2.2 and §2.3: a move flushes its run and its decision to
// `fl/ledger`, and moves the record. That the flush comes first is pinned
// in `fl-exec` (`move_record`'s tests) and the conformance suites.
#[test]
fn a_move_publishes_its_run_and_its_decision_and_moves_the_record() {
    let w = World::new();
    w.ready();
    w.fl()
        .args(["record", "move", "1", "--to", "doing"])
        .assert()
        .success();
    let runs = w.ledger_files_in("runs");
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0].1.lines().count(), 1, "{runs:?}");
    let decisions = w.ledger_files_in("decisions");
    assert_eq!(decisions.len(), 1, "{decisions:?}");
    assert!(
        decisions[0].1.contains(r#""allowed":true"#),
        "{decisions:?}"
    );
    assert!(
        w.fake
            .issue(1)
            .labels
            .contains(&"fl:record/doing".to_string())
    );
}

// Spec §2.2: `check --record` is a decision and publishes; a plain check
// decides nothing and stays local (decision 6).
#[test]
fn a_check_with_a_record_publishes_and_a_plain_check_does_not() {
    let w = World::new();
    w.ready();
    let before = w.fake.ledger_commits();
    w.fl()
        .args(["check", "launch", "--project", "1"])
        .assert()
        .success();
    assert_eq!(
        w.fake.ledger_commits(),
        before,
        "a plain check publishes nothing"
    );
    w.fl()
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .success();
    assert_eq!(w.fake.ledger_commits(), before + 1);
    assert!(w.only_file_in("decisions").1.contains(r#"{"check":"#));
}

// Spec §4.1: the ledger commit, then the state change, then the comment —
// one, on the record's issue.
#[test]
fn a_move_posts_its_comment_on_the_record_after_the_state_change() {
    let w = World::new();
    w.ready();
    w.fake.state().requests.clear();
    w.fl()
        .args(["record", "move", "1", "--to", "doing"])
        .assert()
        .success()
        .stderr(contains(NOT_POSTED).not());
    let comments = w.fake.issue(1).comments;
    assert_eq!(comments.len(), 1, "{comments:?}");
    let ids = w.decision_ids();
    assert_eq!(ids.len(), 1, "{ids:?}");
    let c = &comments[0];
    assert!(
        c.starts_with(&format!("<!-- fl:decision {{\"id\":\"{}\"}} -->\n", ids[0])),
        "{c}"
    );
    assert!(c.contains("### fl move: allowed"), "{c}");
    assert!(c.contains("From `todo` to `doing`."), "{c}");
    assert!(
        c.contains("The state change completed: the record is now `doing`."),
        "{c}"
    );
    let head = w.fake.ledger_head().expect("the ledger");
    assert!(
        c.contains(&format!("(https://github.com/acme/widgets/commit/{head})")),
        "{c}"
    );
    assert!(c.contains("| launch | no-bug | PASS | 1 |"), "{c}");
    assert!(
        c.contains("<details><summary>launch / no-bug: PASS</summary>"),
        "a private repository shows the excerpt: {c}"
    );
    assert!(
        c.contains("checked"),
        "the excerpt the public test looks for: {c}"
    );
    let requests = w.fake.state().requests.clone();
    let moved = requests
        .iter()
        .position(|r| r == "PATCH /repos/acme/widgets/issues/1")
        .expect("the state change");
    let commented = requests
        .iter()
        .position(|r| r == "POST /repos/acme/widgets/issues/1/comments")
        .expect("the comment");
    assert!(moved < commented, "{requests:#?}");
    assert!(
        requests[commented..]
            .iter()
            .all(|r| !r.contains("/git/") && r != "POST /graphql" && !r.starts_with("PATCH")),
        "nothing is published or changed after the comment: {requests:#?}"
    );
}

// Decision 11: a refused move is commented, saying it was refused.
#[test]
fn a_refused_move_is_commented_as_refused() {
    let w = World::new();
    w.ready();
    fs::write(w.repo.path().join("bug"), "").unwrap();
    w.fl()
        .args(["record", "move", "1", "--to", "doing"])
        .assert()
        .code(1);
    let comments = w.fake.issue(1).comments;
    assert_eq!(comments.len(), 1, "{comments:?}");
    assert!(
        comments[0].contains("### fl move: refused"),
        "{}",
        comments[0]
    );
    assert!(
        comments[0].contains("The move was refused: the record stays `todo`."),
        "{}",
        comments[0]
    );
}

// Decision 2: a comment on a repository that is not private shows no
// excerpt.
#[test]
fn a_moves_comment_on_a_repository_that_is_not_private_shows_no_excerpt() {
    let w = World::new();
    w.ready();
    w.fake.state().repos[0].visibility = "public".into();
    w.fl()
        .args(["record", "move", "1", "--to", "doing"])
        .assert()
        .success();
    let comments = w.fake.issue(1).comments;
    assert_eq!(comments.len(), 1, "{comments:?}");
    assert!(!comments[0].contains("<details>"), "{}", comments[0]);
    assert!(!comments[0].contains("checked"), "{}", comments[0]);
}

// ⚠ Decision 2 at the command line: a gate that cannot start names its
// program's path in its detail. On a repository that is not private the
// comment shows neither that detail nor the text that stands in for it;
// on a private one it shows the detail, so the test sees what it checks.
#[test]
fn a_comment_on_a_repository_that_is_not_private_shows_no_error_detail() {
    for private in [true, false] {
        let w = World::new();
        let program = w.repo.path().join("no-such-gate");
        w.gated_with("no-bug", program.to_str().unwrap());
        w.init();
        w.export();
        if !private {
            w.fake.state().repos[0].visibility = "public".into();
        }
        w.fl()
            .args(["record", "move", "1", "--to", "doing"])
            .assert()
            .code(2);
        let comments = w.fake.issue(1).comments;
        assert_eq!(comments.len(), 1, "{comments:?}");
        let c = &comments[0];
        assert!(c.contains("| ERROR |"), "{c}");
        assert_eq!(
            c.contains("no-such-gate"),
            private,
            "the detail, private only: {c}"
        );
        assert_eq!(c.contains("<details>"), private, "{c}");
        assert!(!c.contains("withheld"), "{c}");
    }
}

#[test]
fn a_check_with_a_record_posts_its_comment_and_a_plain_check_posts_none() {
    let w = World::new();
    w.ready();
    w.fl()
        .args(["check", "launch", "--project", "1"])
        .assert()
        .success();
    assert!(
        w.fake.issue(1).comments.is_empty(),
        "a plain check decides nothing"
    );
    w.fl()
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .success();
    let comments = w.fake.issue(1).comments;
    assert_eq!(comments.len(), 1, "{comments:?}");
    assert!(
        comments[0].contains("### fl check: passed"),
        "{}",
        comments[0]
    );
    assert!(
        comments[0].contains("A check changes no state."),
        "{}",
        comments[0]
    );
}

// ⚠ A comment that cannot be posted leaves the decision and its state
// change standing: a warning naming the recovery command, and the
// command's own exit code.
#[test]
fn a_move_whose_comment_fails_keeps_its_exit_code_and_names_the_recovery() {
    let w = World::new();
    w.ready();
    w.fake.state().fail_comment_next = true;
    w.fl()
        .args(["record", "move", "1", "--to", "doing"])
        .assert()
        .success()
        .stderr(
            contains(format!("{NOT_POSTED}1: "))
                .and(contains(
                    "The decision and its state change stand; run `fl github ledger comment 1`",
                ))
                .and(contains("run `fl github ledger comment 1` to post it")),
        );
    assert!(w.fake.issue(1).comments.is_empty());
    assert!(
        w.fake
            .issue(1)
            .labels
            .contains(&"fl:record/doing".to_string()),
        "the move stands"
    );
}

// ⚠ A state change is said to stand only when it completed: a move whose
// state change failed and whose comment cannot be posted leaves only the
// decision standing, and says so; the command still exits 2 with the
// tracker's error.
#[test]
fn a_move_whose_state_change_and_comment_both_fail_says_only_the_decision_stands() {
    let w = World::new();
    w.ready();
    w.fake.state().foreign_label_on_next_patch = true;
    w.fake.state().fail_comment_next = true;
    w.fl()
        .args(["record", "move", "1", "--to", "doing"])
        .assert()
        .code(2)
        .stderr(
            contains(format!("{NOT_POSTED}1: "))
                .and(contains(
                    "The decision stands; run `fl github ledger comment 1`",
                ))
                .and(contains("state change stand").not()),
        );
    assert_eq!(w.decision_ids().len(), 1, "the decision was published");
    assert!(w.fake.issue(1).comments.is_empty());
}

// ⚠ CI reads `check`'s exit code: a comment that cannot be posted never
// turns a pass or a fail into a refusal.
#[test]
fn a_check_whose_comment_fails_keeps_the_checks_own_exit_code() {
    let w = World::new();
    w.ready();
    for (bug, code) in [(false, 0), (true, 1)] {
        if bug {
            fs::write(w.repo.path().join("bug"), "").unwrap();
        }
        w.fake.state().fail_comment_next = true;
        w.fl()
            .args(["check", "launch", "--project", "1", "--record", "1"])
            .assert()
            .code(code)
            .stderr(contains(NOT_POSTED));
    }
}

// ⚠ Spec §4.2: the comment says whether the state change completed. One
// that fails after the flush is commented too; the command still exits 2
// with the tracker's error.
#[test]
fn a_move_whose_state_change_fails_after_its_flush_says_so_in_its_comment() {
    let w = World::new();
    w.ready();
    w.fake.state().foreign_label_on_next_patch = true;
    w.fl()
        .args(["record", "move", "1", "--to", "doing"])
        .assert()
        .code(2);
    assert_eq!(w.decision_ids().len(), 1, "the decision was published");
    let comments = w.fake.issue(1).comments;
    assert_eq!(comments.len(), 1, "{comments:?}");
    assert!(
        comments[0].contains("The move was allowed, but its state change did not complete"),
        "{}",
        comments[0]
    );
}

// Spec §4.1: a finding's decisions comment on the finding's issue.
#[test]
fn a_reproduction_posts_its_comment_on_the_findings_issue() {
    let w = World::new();
    w.finding_raised();
    w.fl()
        .args(["finding", "reproduce", "2", "--gate", "1"])
        .assert()
        .success();
    let comments = w.fake.issue(2).comments;
    assert_eq!(comments.len(), 1, "{comments:?}");
    let c = &comments[0];
    assert!(c.contains("### fl reproduce: accepted"), "{c}");
    assert!(
        c.contains(
            "The state change completed: the finding records this gate as its reproduction."
        ),
        "{c}"
    );
    assert!(c.contains("| reproduction | no-bug | FAIL |"), "{c}");
    assert!(
        w.fake.issue(1).comments.is_empty(),
        "nothing on the record's issue"
    );
}

// Decision 11: a refused reproduction is flushed and commented, and the
// command is refused as before.
#[test]
fn a_refused_reproduction_is_commented_as_refused() {
    let w = World::new();
    w.finding_raised();
    fs::remove_file(w.repo.path().join("bug")).unwrap();
    w.fl()
        .args(["finding", "reproduce", "2", "--gate", "1"])
        .assert()
        .code(2)
        .stderr(contains("currently PASSES"));
    let comments = w.fake.issue(2).comments;
    assert_eq!(comments.len(), 1, "{comments:?}");
    assert!(
        comments[0].contains("### fl reproduce: refused"),
        "{}",
        comments[0]
    );
    assert!(
        comments[0].contains("The reproduction was refused: the finding is unchanged."),
        "{}",
        comments[0]
    );
}

// ⚠ Spec §4.2: a reproduction whose state change fails after its flush is
// commented too, saying so; the command exits 2 as before.
#[test]
fn a_reproduction_whose_state_change_fails_says_so_in_its_comment() {
    let w = World::new();
    w.finding_raised();
    w.fake.state().foreign_label_on_next_patch = true;
    w.fl()
        .args(["finding", "reproduce", "2", "--gate", "1"])
        .assert()
        .code(2);
    let comments = w.fake.issue(2).comments;
    assert_eq!(comments.len(), 1, "{comments:?}");
    assert!(
        comments[0]
            .contains("The reproduction was accepted, but its state change did not complete."),
        "{}",
        comments[0]
    );
}

/// `finding_raised`, reproduced, assigned, and the bug fixed.
fn ready_to_verify(w: &World) {
    w.finding_raised();
    w.fl()
        .args(["finding", "reproduce", "2", "--gate", "1"])
        .assert()
        .success();
    w.fl()
        .args(["finding", "assign", "2", "--to", "fixer"])
        .assert()
        .success();
    fs::remove_file(w.repo.path().join("bug")).unwrap();
}

#[test]
fn a_verification_that_closes_its_finding_posts_its_comment() {
    let w = World::new();
    ready_to_verify(&w);
    w.fl().args(["finding", "verify", "2"]).assert().success();
    let comments = w.fake.issue(2).comments;
    assert_eq!(comments.len(), 2, "{comments:?}");
    assert!(
        comments[1].contains("### fl verify: closed"),
        "{}",
        comments[1]
    );
    assert!(
        comments[1].contains("The state change completed: the finding is closed."),
        "{}",
        comments[1]
    );
}

// A verification that ran but did not close is commented, saying the
// finding stays open.
#[test]
fn a_verification_that_does_not_close_its_finding_posts_its_comment() {
    let w = World::new();
    ready_to_verify(&w);
    fs::write(w.repo.path().join("bug"), "").unwrap();
    w.fl().args(["finding", "verify", "2"]).assert().code(1);
    let comments = w.fake.issue(2).comments;
    assert_eq!(comments.len(), 2, "{comments:?}");
    assert!(
        comments[1].contains("### fl verify: not closed"),
        "{}",
        comments[1]
    );
    assert!(
        comments[1].contains("The finding stays open: the repair is not done."),
        "{}",
        comments[1]
    );
}

// ⚠ Spec §4.2: a verification whose closing fails after its flush is
// commented too, saying so; the command exits 2 as before.
#[test]
fn a_verification_whose_closing_fails_says_so_in_its_comment() {
    let w = World::new();
    ready_to_verify(&w);
    w.fake.state().foreign_label_on_next_patch = true;
    w.fl().args(["finding", "verify", "2"]).assert().code(2);
    let comments = w.fake.issue(2).comments;
    assert_eq!(comments.len(), 2, "{comments:?}");
    assert!(
        comments[1]
            .contains("The finding passed its verification, but closing it did not complete."),
        "{}",
        comments[1]
    );
}

// Spec §4.1 and decision 14: an attempt's comment, on the record's issue;
// one that cannot be posted is a warning, and the exit code stays the
// attempt's own.
#[test]
fn an_attempts_comment_that_fails_keeps_the_attempts_exit_code() {
    let w = World::new();
    w.ready();
    w.fl()
        .args(["attempt", "1", "--budget-usd-micros", "0"])
        .assert()
        .code(1)
        .stderr(contains(NOT_POSTED).not());
    let comments = w.fake.issue(1).comments;
    assert_eq!(comments.len(), 1, "{comments:?}");
    let c = &comments[0];
    assert!(c.contains("### fl attempt: refused"), "{c}");
    assert!(c.contains("An attempt changes no state."), "{c}");
    assert!(c.contains("| claude | refused |"), "{c}");
    w.fake.state().fail_comment_next = true;
    w.fl()
        .args(["attempt", "1", "--budget-usd-micros", "0"])
        .assert()
        .code(1)
        .stderr(contains(NOT_POSTED).and(contains("fl github ledger comment 1")));
    assert_eq!(w.fake.issue(1).comments.len(), 1);
}

// Spec §1.5: without `ledger = "github"`, a decision stays in the local
// store, exactly as in mode A — no request touches the ledger at all.
#[test]
fn without_the_ledger_key_a_move_publishes_nothing() {
    let w = World::bound(false);
    w.gated();
    w.fake.seed_ledger();
    w.fake.state().requests.clear();
    w.fl()
        .args(["record", "move", "1", "--to", "doing"])
        .assert()
        .success();
    let requests = w.fake.state().requests.clone();
    assert!(
        requests
            .iter()
            .any(|r| r == "PATCH /repos/acme/widgets/issues/1"),
        "the move reached GitHub: {requests:#?}"
    );
    let ledger: Vec<&String> = requests
        .iter()
        .filter(|r| r.contains("/git/") || r.contains("/rules/") || r.contains("/compare/"))
        .collect();
    assert!(ledger.is_empty(), "no ledger request: {ledger:#?}");
    assert!(w.fake.issue(1).comments.is_empty(), "no comment in mode A");
    assert_eq!(w.fake.ledger_commits(), 1);
}

// Spec §3.5: `verify` passes a ledger fl wrote, and names the commit of a
// hand edit.
#[test]
fn verify_passes_a_ledger_fl_wrote_and_names_the_commit_of_a_hand_edit() {
    let w = World::new();
    w.ready();
    w.fl()
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .success();
    w.fl()
        .args(["github", "ledger", "verify"])
        .assert()
        .success()
        .stdout(contains("verified\t2 commits"));
    let (path, _) = w.only_file_in("runs");
    let bad = w.fake.hand_commit(&[(path.as_str(), Some("edited\n"))]);
    w.fl()
        .args(["github", "ledger", "verify"])
        .assert()
        .code(1)
        .stdout(contains(format!("BAD\t{bad}\trewrites lines of `{path}`")));
}

#[test]
fn verify_stops_at_its_limit_and_names_the_flag() {
    let w = World::new();
    w.ready();
    w.fl()
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .success();
    w.fl()
        .args(["github", "ledger", "verify", "--max-commits", "1"])
        .assert()
        .code(2)
        .stderr(contains("walked back 1 commit from the head").and(contains("--max-commits <n>")));
}

// Spec §3.5 check 5: one id on two different lines is not a clean ledger.
#[test]
fn verify_reports_one_id_on_two_lines_and_exits_1() {
    use fl_github::ledger::layout;
    let w = World::new();
    w.ready();
    w.fl()
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .success();
    let (_, text) = w.only_file_in("runs");
    // The published run, about another gate, filed in that gate's own
    // directory: the same id on a different line. Keys stay sorted, so the
    // line is byte-for-byte what fl would write.
    let mut line: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
    let other = "urn:uuid:00000000-0000-7000-8000-000000000077";
    line["gate"] = serde_json::Value::String(other.into());
    let other_iri = fl_core::Iri::parse(other).unwrap();
    let seg = layout::segment_path(&layout::dir(layout::Area::Runs, &other_iri), 1);
    w.fake
        .hand_commit(&[(seg.as_str(), Some(format!("{line}\n").as_str()))]);
    w.fl()
        .args(["github", "ledger", "verify"])
        .assert()
        .code(1)
        .stdout(contains("SAME ID\t").and(contains("verified\t").not()));
}

#[test]
fn verify_refuses_a_limit_of_no_commits() {
    let w = World::new();
    w.ready();
    w.fl()
        .args(["github", "ledger", "verify", "--max-commits", "0"])
        .assert()
        .code(2)
        .stderr(contains("`--max-commits` must be at least 1"));
}

// ⚠ Decision 16: on a repository that is not private,
// `--by` and `--reason` are public for good — warned, then appended.
#[test]
fn quarantine_on_a_repository_that_is_not_private_warns_then_appends() {
    let w = World::new();
    w.ready();
    w.fl()
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .success();
    let (path, _) = w.only_file_in("runs");
    w.fake.state().repos[0].visibility = "public".into();
    w.fake.state().requests.clear();
    w.fl()
        .args([
            "github",
            "ledger",
            "quarantine",
            &path,
            "1",
            "--by",
            "maintainer",
            "--reason",
            "a test",
        ])
        .assert()
        .success()
        .stderr(contains(
            "warning: acme/widgets is not private: once appended",
        ))
        .stdout(contains(format!("quarantined\t{path}\tline 1\t")));
    assert!(w.fake.ledger_files()["quarantine.jsonl"].contains(r#""quarantined_by":"maintainer""#));
    // ⚠ The visibility is read, and the warning given, before fl touches
    // the ledger: no repository read comes after the first ledger request.
    let requests = w.fake.state().requests.clone();
    let first_ledger = requests
        .iter()
        .position(|r| r.contains("/git/") || r == "POST /graphql")
        .expect("the append touched the ledger");
    let last_repo_read = requests
        .iter()
        .rposition(|r| r == "GET /repos/acme/widgets")
        .expect("the visibility was read");
    assert!(last_repo_read < first_ledger, "{requests:#?}");
}

// Decision 2: a visibility that cannot be read is not private; quarantine
// refuses before it appends anything.
#[test]
fn quarantine_whose_visibility_cannot_be_read_refuses_and_appends_nothing() {
    let w = World::new();
    w.ready();
    w.fl()
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .success();
    let (path, _) = w.only_file_in("runs");
    let head = w.fake.ledger_head();
    // The command's first repository read binds the tracker; the second is
    // the visibility read, which fails.
    w.fake.state().fail_repo_read_after = Some(1);
    w.fl()
        .args([
            "github",
            "ledger",
            "quarantine",
            &path,
            "1",
            "--by",
            "maintainer",
            "--reason",
            "a test",
        ])
        .assert()
        .code(2)
        .stderr(contains("when fl read the repository's visibility"))
        .stdout(contains("quarantined\t").not());
    assert_eq!(w.fake.ledger_head(), head, "nothing was appended");
    assert!(!w.fake.ledger_files().contains_key("quarantine.jsonl"));
}

#[test]
fn quarantine_on_a_private_repository_says_the_text_is_permanent_without_a_warning() {
    let w = World::new();
    w.ready();
    w.fl()
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .success();
    let (path, _) = w.only_file_in("runs");
    w.fl()
        .args([
            "github",
            "ledger",
            "quarantine",
            &path,
            "1",
            "--by",
            "maintainer",
            "--reason",
            "a test",
        ])
        .assert()
        .success()
        .stderr(
            contains(concat!(
                "permanent: once appended, `--by` and `--reason` are written to the ledger of ",
                "acme/widgets permanently"
            ))
            .and(contains("warning:").not())
            .and(contains("note:").not()),
        );
}

// Spec §3.3, §3.6: a quarantined line is skipped and noted by the command
// whose read skipped it.
#[test]
fn a_decision_that_reads_past_a_quarantined_line_notes_it() {
    let w = World::new();
    w.ready();
    w.fl()
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .success();
    let (path, _) = w.only_file_in("runs");
    w.fl()
        .args([
            "github",
            "ledger",
            "quarantine",
            &path,
            "1",
            "--by",
            "maintainer",
            "--reason",
            "a test",
        ])
        .assert()
        .success();
    w.fl()
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .success()
        .stderr(contains(format!(
            "note: `{path}` line 1 of the GitHub ledger is quarantined (a test)"
        )));
}

// Spec §2.4, §7: no ledger yet — refused, naming `init`, before the move's
// gate runs; the record stays.
#[test]
fn a_move_on_a_repository_with_no_ledger_is_refused_before_its_gate_runs() {
    let w = World::new();
    w.gated();
    w.export();
    w.fl()
        .args(["record", "move", "1", "--to", "doing"])
        .assert()
        .code(2)
        .stderr(contains(PREFLIGHT).and(contains("has no GitHub ledger yet")));
    assert_eq!(w.runs(&w.one), 0, "no gate ran");
    assert!(
        w.fake
            .issue(1)
            .labels
            .contains(&"fl:record/todo".to_string())
    );
}

// Spec §2.2: an ungated move is flushed too, so it pre-flights like any
// other.
#[test]
fn an_ungated_move_on_a_repository_with_no_ledger_is_refused() {
    let w = World::new();
    w.gated();
    w.export();
    w.fl()
        .args(["record", "move", "1", "--to", "done"])
        .assert()
        .code(2)
        .stderr(contains(PREFLIGHT).and(contains("has no GitHub ledger yet")));
    assert!(
        w.fake
            .issue(1)
            .labels
            .contains(&"fl:record/todo".to_string())
    );
}

// Spec §7, "`ledger_root` missing from the manifest": on this machine a
// manifest committed without the ledger's first commit is refused at the
// first decision, naming the export — so `init`'s instruction cannot be
// lost.
#[test]
fn a_decision_on_a_manifest_without_the_ledgers_first_commit_is_refused_naming_export() {
    let w = World::new();
    w.gated();
    w.export();
    w.init();
    w.fl()
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .code(2)
        .stderr(
            contains(PREFLIGHT)
                .and(contains("does not carry the GitHub ledger's first commit"))
                .and(contains("here, then commit it")),
        );
    assert_eq!(w.runs(&w.one), 0);
    w.export();
    w.fl()
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .success();
}

// The same refusal on a machine that does not author the project: its
// root came from `init --confirm`, because the manifest it imported was
// exported before the ledger existed. It cannot export, so the refusal
// names the authoring machine's export and an import here.
#[test]
fn a_manifest_without_the_first_commit_on_an_importing_machine_names_the_import() {
    let w = World::new();
    w.gated();
    w.export();
    w.init();
    let root = w.fake.ledger_head().expect("the branch");
    let two = w.machine();
    w.fl_on(&two)
        .args(["manifest", "import"])
        .assert()
        .success();
    w.fl_on(&two)
        .args(["github", "ledger", "init"])
        .assert()
        .code(1)
        .stdout(contains(format!("confirm\tfl/ledger\t{root}")));
    w.fl_on(&two)
        .args(["github", "ledger", "init", "--confirm", &root])
        .assert()
        .success();
    w.fl_on(&two)
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .code(2)
        .stderr(contains(PREFLIGHT).and(contains("then pull here and run `fl manifest import`")));
    assert_eq!(w.runs(&two), 0);
}

// Spec §2.4, §6.1 step 4: no manifest at all is refused naming the export
// on the store that authors the project.
#[test]
fn a_decision_on_a_project_with_no_manifest_names_the_export() {
    let w = World::new();
    w.gated();
    w.init();
    w.fl()
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .code(2)
        .stderr(
            contains(PREFLIGHT)
                .and(contains("there is no manifest at"))
                .and(contains("and commit the file it writes")),
        );
    assert_eq!(w.runs(&w.one), 0);
}

// On a machine that imported the project, a missing manifest names where
// it comes from and the import.
#[test]
fn a_missing_manifest_on_an_importing_machine_names_where_it_comes_from() {
    let w = World::new();
    w.ready();
    let two = w.machine();
    w.fl_on(&two)
        .args(["manifest", "import"])
        .assert()
        .success();
    fs::remove_file(w.repo.path().join(".fl/manifest.json")).unwrap();
    w.fl_on(&two)
        .args(["check", "launch", "--project", "1"])
        .assert()
        .code(2)
        .stderr(contains(
            "It comes from the machine that authors the project",
        ));
}

// Spec §2.4: the manifest must list every gate before a run can reach the
// shared ledger.
#[test]
fn a_check_on_a_project_whose_manifest_lacks_a_gate_is_refused_before_it_runs() {
    let w = World::new();
    w.ready();
    w.fl()
        .args([
            "gate",
            "add",
            "--project",
            "1",
            "--name",
            "late",
            "--glob",
            "src/**/*.rs",
            "--program",
            "true",
        ])
        .assert()
        .success();
    w.fl()
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .code(2)
        .stderr(contains(PREFLIGHT).and(contains("gate `late` is not in the manifest")));
    assert_eq!(w.runs(&w.one), 0);
}

// ⚠ Spec §6.1 step 5: a machine that knows the ledger only
// from the manifest has no cut-over until it runs `init` — refused, naming
// it, before anything runs.
#[test]
fn a_machine_with_no_cut_over_is_refused_naming_init() {
    let w = World::new();
    w.ready();
    let two = w.machine();
    w.fl_on(&two)
        .args(["manifest", "import"])
        .assert()
        .success();
    w.fl_on(&two)
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .code(2)
        .stderr(contains(PREFLIGHT).and(contains("has no cut-over")));
    assert_eq!(w.runs(&two), 0);
    w.fl_on(&two)
        .args(["github", "ledger", "init"])
        .assert()
        .success();
    w.fl_on(&two)
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .success();
}

// Spec §2.4, §5: visibility is read live, first.
#[test]
fn a_visibility_that_cannot_be_read_is_refused_before_the_gate_runs() {
    let w = World::new();
    w.ready();
    w.fake.state().fail_repo_read_after = Some(1);
    w.fl()
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .code(2)
        .stderr(contains(PREFLIGHT).and(contains("read the repository's visibility")));
    assert_eq!(w.runs(&w.one), 0);
}

// Spec §2.4: the mode is read before anything runs; rules fl cannot read
// refuse.
#[test]
fn rules_that_cannot_be_read_are_refused_before_the_gate_runs() {
    let w = World::new();
    w.ready();
    w.fake.state().fail_rules_next = true;
    w.fl()
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .code(2)
        .stderr(contains(PREFLIGHT).and(contains("rules/branches/fl/ledger")));
    assert_eq!(w.runs(&w.one), 0);
}

#[test]
fn a_reproduction_on_a_repository_with_no_ledger_is_refused_before_its_gate_runs() {
    let w = World::new();
    w.gated();
    w.export();
    fs::write(w.repo.path().join("bug"), "").unwrap();
    w.fl()
        .args([
            "finding", "raise", "--record", "1", "--claim", "a bug", "--by", "reviewer",
        ])
        .assert()
        .success();
    w.fl()
        .args(["finding", "reproduce", "2", "--gate", "1"])
        .assert()
        .code(2)
        .stderr(contains(PREFLIGHT).and(contains("has no GitHub ledger yet")));
    assert_eq!(w.runs(&w.one), 0);
}

#[test]
fn a_verification_on_a_deleted_ledger_is_refused_before_its_gates_run() {
    let w = World::new();
    w.ready();
    fs::write(w.repo.path().join("bug"), "").unwrap();
    w.fl()
        .args([
            "finding", "raise", "--record", "1", "--claim", "a bug", "--by", "reviewer",
        ])
        .assert()
        .success();
    w.fl()
        .args(["finding", "reproduce", "2", "--gate", "1"])
        .assert()
        .success();
    w.fl()
        .args(["finding", "assign", "2", "--to", "fixer"])
        .assert()
        .success();
    fs::remove_file(w.repo.path().join("bug")).unwrap();
    let before = w.runs(&w.one);
    w.fake.delete_ledger();
    w.fl()
        .args(["finding", "verify", "2"])
        .assert()
        .code(2)
        .stderr(contains(PREFLIGHT).and(contains("fl will not start a new ledger")));
    assert_eq!(w.runs(&w.one), before, "no gate ran");
}

// Decision 8: GitHub is checked before the adapter spends anything.
#[test]
fn an_attempt_on_a_repository_with_no_ledger_is_refused_before_the_adapter_runs() {
    let w = World::new();
    w.gated();
    w.export();
    w.fl()
        .args(["attempt", "1", "--budget-usd-micros", "0"])
        .assert()
        .code(2)
        .stderr(contains(PREFLIGHT).and(contains("has no GitHub ledger yet")));
    assert_eq!(w.attempts(&w.one), 0);
}

// ⚠ Decisions 8 and 14: an attempt that ran but could not
// be published keeps its own exit code, warns, and rides with the next
// flush that lands.
#[test]
fn an_attempt_whose_publish_fails_warns_keeps_its_exit_code_and_rides_with_the_next() {
    let w = World::new();
    w.ready();
    w.fake.state().fail_commits = fl_github::ledger::TRIES;
    w.fl()
        .args(["attempt", "1", "--budget-usd-micros", "0"])
        .assert()
        .code(1)
        .stdout(contains("refused\t"))
        .stderr(
            contains("warning: the attempt ran and is recorded in the local store")
                .and(contains("error:").not()),
        );
    assert_eq!(w.attempts(&w.one), 1);
    assert!(w.ledger_files_in("attempts").is_empty(), "nothing landed");
    w.fl()
        .args(["attempt", "1", "--budget-usd-micros", "0"])
        .assert()
        .code(1);
    let attempts = w.ledger_files_in("attempts");
    assert_eq!(attempts.len(), 1, "{attempts:?}");
    assert_eq!(
        attempts[0].1.lines().count(),
        2,
        "the first attempt rode with the second"
    );
}

// Spec §2.5: the report merges the local store and GitHub by id — one
// attempt published is counted once — and adds no note.
#[test]
fn stats_in_mode_b_counts_a_published_attempt_once_and_adds_no_note() {
    let w = World::new();
    w.ready();
    w.fl()
        .args(["attempt", "1", "--budget-usd-micros", "0"])
        .assert()
        .code(1);
    w.fl()
        .args(["stats", "--project", "1"])
        .assert()
        .success()
        .stdout(contains("attempts: 1\n").and(contains("note: this covers").not()));
}

// ⚠ Spec §2.5: unreachable is not a total.
#[test]
fn stats_when_github_cannot_be_reached_covers_the_local_store_and_says_so() {
    let w = World::new();
    w.ready();
    w.fake.state().down = true;
    w.fl()
        .args(["stats", "--project", "1"])
        .assert()
        .success()
        .stdout(contains(
            "note: this covers the local store only, because GitHub could not be read",
        ));
}

#[test]
fn stats_under_db_says_it_covers_the_local_store_only() {
    let w = World::new();
    w.ready();
    w.fl()
        .arg("--db")
        .arg(w.one.store())
        .args(["stats", "--project", "1"])
        .assert()
        .success()
        .stdout(contains("because --db (or $FL_DB) names the store"));
}

#[test]
fn stats_without_the_ledger_key_on_a_store_that_records_one_says_so() {
    let w = World::new();
    w.ready();
    w.configure(&w.one, false);
    w.fl()
        .args(["stats", "--project", "1"])
        .assert()
        .success()
        .stdout(contains(
            "names no `ledger = \"github\"`, though this store records",
        ));
}

// A project named by IRI from a directory bound to another store: the
// command runs on the project's store, which is not this directory's, so
// it does not read the GitHub ledger from here — even though this
// directory's binding names one.
#[test]
fn stats_on_another_projects_store_says_it_covers_the_local_store_only() {
    let w = World::new();
    w.ready();
    let iri = {
        let store = fl_store::RedbStore::open(&w.one.store()).unwrap();
        let projects = store.list_projects().unwrap();
        assert_eq!(projects.len(), 1, "{projects:?}");
        projects[0].id.iri().as_str().to_string()
    };
    let elsewhere = tempfile::tempdir().unwrap();
    let path = w.one.home.path().join("config/fl/config.toml");
    let mut cfg = fs::read_to_string(&path).unwrap();
    cfg.push_str(&format!(
        "[[project]]\nroot = \"{}\"\nstore = \"{}\"\n\
         tracker = {{ github = \"acme/widgets\", credential = \"env\", ledger = \"github\" }}\n",
        elsewhere.path().canonicalize().unwrap().display(),
        w.one.home.path().join("elsewhere.redb").display()
    ));
    fs::write(&path, cfg).unwrap();
    w.fl()
        .current_dir(elsewhere.path())
        .args(["stats", "--project", &iri])
        .assert()
        .success()
        .stdout(contains(
            "because the project is held by another project's store",
        ));
}

#[test]
fn stats_on_a_project_without_a_github_ledger_adds_no_note_and_asks_nothing() {
    let w = World::bound(false);
    w.gated();
    let before = w.fake.state().requests.len();
    w.fl()
        .args(["stats", "--project", "1"])
        .assert()
        .success()
        .stdout(contains("attempts: 0\n").and(contains("note: this covers").not()));
    assert_eq!(w.fake.state().requests.len(), before, "no request");
}

// A ledger never set up is lasting, not "cannot be read" (spec §2.5): an
// error naming `init`, never a local count.
#[test]
fn stats_on_a_ledger_never_set_up_is_refused_naming_init() {
    let w = World::new();
    w.gated();
    w.fl()
        .args(["stats", "--project", "1"])
        .assert()
        .code(2)
        .stderr(contains("has no GitHub ledger yet"));
}

// Only GitHub that cannot be reached falls back: a missing credential is
// the person's to fix, and refused as it is for every tracker command.
#[test]
fn stats_with_no_credential_is_refused_not_counted_locally() {
    let w = World::new();
    w.ready();
    w.fl()
        .env_remove("FL_GITHUB_TOKEN")
        .args(["stats", "--project", "1"])
        .assert()
        .code(2)
        .stderr(contains("FL_GITHUB_TOKEN or GITHUB_TOKEN"));
}

// `stats_reads_github` is gated on `is_stats`: every other command stays
// local-only even on a project bound with the GitHub ledger in its own
// directory (GitHub ledger spec §2.5 names only `fl stats`).
#[test]
fn a_non_stats_command_on_a_ledger_bound_project_opens_no_tracker() {
    let w = World::new();
    w.ready();
    let before = w.fake.state().requests.len();
    w.fl()
        .args(["gate", "list", "--project", "1"])
        .assert()
        .success();
    assert_eq!(w.fake.state().requests.len(), before, "no request");
}
