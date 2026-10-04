//! The CLI with a project whose ledger is the GitHub ledger
//! (`ledger = "github"`, GitHub ledger spec), against the in-process fake.
//! The `fl` binary reaches the fake through `FL_GITHUB_API_URL`.

use assert_cmd::Command;
use fl_core::store::{Catalog, Ledger};
use fl_github::fake::FakeGithub;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command as Sys;

/// What every pre-flight refusal starts with (GitHub ledger spec §2.4).
const PREFLIGHT: &str = "refused before any gate or adapter ran: ";

/// What a comment that could not be posted warns. No other path writes it.
const NOT_POSTED: &str = "warning: the decision's comment was not posted on issue ";

/// The recovery command a failed comment on record #1 names: the record in
/// full, never its number.
const RECOVER_1: &str = "fl github ledger comment https://github.com/acme/widgets/issues/1";

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
                .and(contains(format!(
                    "The decision and its state change stand; run `{RECOVER_1}`"
                )))
                .and(contains(format!("run `{RECOVER_1}` to post it"))),
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
                .and(contains(format!("The decision stands; run `{RECOVER_1}`")))
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
        .stderr(contains(NOT_POSTED).and(contains(RECOVER_1)));
    assert_eq!(w.fake.issue(1).comments.len(), 1);
}

/// How many of the fake's requests since the last clear are `line`.
fn requests_equal(w: &World, line: &str) -> usize {
    w.fake
        .state()
        .requests
        .iter()
        .filter(|r| *r == line)
        .count()
}

// ⚠ Spec §4.3: only what no comment marks, rendered from the ledger with
// no state line — the ledger does not record whether the state change
// completed. A second run posts nothing, and reads nothing past the
// listing.
#[test]
fn comment_posts_only_what_is_missing_and_a_second_run_posts_nothing() {
    let w = World::new();
    w.ready();
    w.fl()
        .args(["record", "move", "1", "--to", "doing"])
        .assert()
        .success();
    w.fake.state().fail_comment_next = true;
    w.fl()
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .success()
        .stderr(contains(NOT_POSTED));
    assert_eq!(w.fake.issue(1).comments.len(), 1);
    w.fake.state().requests.clear();
    w.fl()
        .args(["github", "ledger", "comment", "1"])
        .assert()
        .success()
        .stdout(contains("comments\t1 posted, 1 already there"));
    assert_eq!(
        requests_equal(&w, "POST /graphql"),
        3,
        "the decisions' listing, the runs' listing, and one blame"
    );
    let comments = w.fake.issue(1).comments;
    assert_eq!(comments.len(), 2, "{comments:?}");
    assert!(
        comments[1].contains("### fl check: passed"),
        "{}",
        comments[1]
    );
    assert!(
        comments[1].contains("| launch | no-bug | PASS |"),
        "{}",
        comments[1]
    );
    assert!(
        !comments[1].contains("A check changes no state."),
        "{}",
        comments[1]
    );
    w.fake.state().requests.clear();
    w.fl()
        .args(["github", "ledger", "comment", "1"])
        .assert()
        .success()
        .stdout(contains("comments\t0 posted, 2 already there").and(contains("posted\turn").not()));
    assert_eq!(w.fake.issue(1).comments.len(), 2);
    assert_eq!(
        requests_equal(&w, "POST /graphql"),
        1,
        "only the decisions' listing"
    );
    assert_eq!(
        requests_equal(&w, "GET /repos/acme/widgets"),
        1,
        "the tracker's own read; no visibility read when nothing is missing"
    );
}

// Oldest first; a post that fails is refused and names nothing posted; a
// re-run posts what is left.
#[test]
fn comment_posts_oldest_first_and_a_rerun_after_a_failed_post_posts_the_rest() {
    let w = World::new();
    w.ready();
    for args in [
        &["record", "move", "1", "--to", "doing"][..],
        &["check", "launch", "--project", "1", "--record", "1"][..],
    ] {
        w.fake.state().fail_comment_next = true;
        w.fl()
            .args(args)
            .assert()
            .success()
            .stderr(contains(NOT_POSTED));
    }
    w.fake.state().fail_comment_next = true;
    w.fl()
        .args(["github", "ledger", "comment", "1"])
        .assert()
        .code(2)
        .stdout(contains("posted\t").not());
    assert!(w.fake.issue(1).comments.is_empty());
    w.fl()
        .args(["github", "ledger", "comment", "1"])
        .assert()
        .success()
        .stdout(contains("comments\t2 posted, 0 already there"));
    let comments = w.fake.issue(1).comments;
    assert_eq!(comments.len(), 2, "{comments:?}");
    assert!(
        comments[0].contains("### fl move: allowed"),
        "{}",
        comments[0]
    );
    assert!(
        comments[1].contains("### fl check: passed"),
        "{}",
        comments[1]
    );
}

// ⚠ A post that fails after another landed: the one that landed is said
// as it lands, and the command fails.
#[test]
fn a_post_that_fails_after_one_landed_names_the_one_posted_and_exits_2() {
    let w = World::new();
    w.ready();
    for args in [
        &["record", "move", "1", "--to", "doing"][..],
        &["check", "launch", "--project", "1", "--record", "1"][..],
    ] {
        w.fake.state().fail_comment_next = true;
        w.fl()
            .args(args)
            .assert()
            .success()
            .stderr(contains(NOT_POSTED));
    }
    let ids = w.decision_ids();
    assert_eq!(ids.len(), 2, "{ids:?}");
    w.fake.state().fail_comment_after = Some(1);
    w.fl()
        .args(["github", "ledger", "comment", "1"])
        .assert()
        .code(2)
        .stdout(
            contains(format!("posted\t{}\n", ids[0]))
                .and(contains(format!("posted\t{}", ids[1])).not())
                .and(contains("comments\t").not()),
        );
    let comments = w.fake.issue(1).comments;
    assert_eq!(comments.len(), 1, "{comments:?}");
    assert!(
        comments[0].contains("### fl move: allowed"),
        "{}",
        comments[0]
    );
}

// ⚠ A decision made by handle before a rename is filed under the IRI it
// was made with, under the old name. The command its warning names still
// finds it after the rename.
#[test]
fn the_recovery_a_warning_names_finds_its_decision_after_a_rename() {
    let w = World::new();
    w.ready();
    w.fake.state().fail_comment_next = true;
    let out = w
        .fl()
        .args(["record", "move", "1", "--to", "doing"])
        .assert()
        .success()
        .stderr(contains(NOT_POSTED))
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8(out).unwrap();
    let warning = stderr
        .lines()
        .find(|l| l.starts_with(NOT_POSTED))
        .expect("the warning");
    let (_, tail) = warning.split_once("; run `").expect("the command it names");
    let (command, _) = tail.split_once('`').expect("the command's end");
    let args: Vec<&str> = command
        .strip_prefix("fl ")
        .expect("an fl command")
        .split(' ')
        .collect();
    w.fake.rename("acme/gadgets");
    w.fl()
        .args(&args)
        .assert()
        .success()
        .stdout(contains("comments\t1 posted, 0 already there"));
    let comments = w.fake.issue(1).comments;
    assert_eq!(comments.len(), 1, "{comments:?}");
    assert!(
        comments[0].contains("### fl move: allowed"),
        "{}",
        comments[0]
    );
}

// ⚠ Recovery by number reads only the decisions filed under the issue's
// current URL, and names that URL on stderr: after a rename, its "0
// posted" is visibly about the new URL, not the old one the decision is
// filed under. Given the URL itself, it names nothing more.
#[test]
fn recovery_by_number_names_the_url_whose_decisions_it_read() {
    let w = World::new();
    w.ready();
    w.fake.state().fail_comment_next = true;
    w.fl()
        .args(["record", "move", "1", "--to", "doing"])
        .assert()
        .success()
        .stderr(contains(NOT_POSTED));
    w.fake.rename("acme/gadgets");
    w.fl()
        .args(["github", "ledger", "comment", "1"])
        .assert()
        .success()
        .stderr(contains("read\thttps://github.com/acme/gadgets/issues/1\n"))
        .stdout(contains("comments\t0 posted, 0 already there").and(contains("moved\t").not()));
    assert!(w.fake.issue(1).comments.is_empty());
    w.fl()
        .args(["github", "ledger", "comment", "#1"])
        .assert()
        .success()
        .stderr(contains("read\thttps://github.com/acme/gadgets/issues/1\n"));
    w.fl()
        .args([
            "github",
            "ledger",
            "comment",
            "https://github.com/acme/widgets/issues/1",
        ])
        .assert()
        .success()
        .stderr(contains("read\t").not())
        .stdout(contains("comments\t1 posted, 0 already there"));
    assert_eq!(w.fake.issue(1).comments.len(), 1);
}

// ⚠ Spec §4.3: a marker on a later page still counts.
#[test]
fn comment_finds_its_markers_on_every_page() {
    let w = World::new();
    w.ready();
    w.fl()
        .args(["record", "move", "1", "--to", "doing"])
        .assert()
        .success();
    w.fl()
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .success();
    {
        let mut s = w.fake.state();
        s.issues
            .get_mut(&1)
            .unwrap()
            .comments
            .push("thanks!".into());
        s.max_per_page = 1;
    }
    w.fl()
        .args(["github", "ledger", "comment", "1"])
        .assert()
        .success()
        .stdout(contains("comments\t0 posted, 2 already there"));
    assert_eq!(w.fake.issue(1).comments.len(), 3);
}

// ⚠ A page that fails is an error, never "none posted, so post them all".
#[test]
fn a_comment_page_that_cannot_be_read_posts_nothing() {
    let w = World::new();
    w.ready();
    w.fake.state().fail_comment_next = true;
    w.fl()
        .args(["record", "move", "1", "--to", "doing"])
        .assert()
        .success();
    w.fl()
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .success();
    {
        let mut s = w.fake.state();
        s.issues
            .get_mut(&1)
            .unwrap()
            .comments
            .push("thanks!".into());
        s.max_per_page = 1;
        s.fail_page = Some(("/repos/acme/widgets/issues/1/comments".into(), 2));
    }
    w.fl()
        .args(["github", "ledger", "comment", "1"])
        .assert()
        .code(2)
        .stdout(contains("posted\t").not());
    assert_eq!(w.fake.issue(1).comments.len(), 2, "nothing posted");
}

// Spec §4.1: a transferred issue gets its comment where it is now.
#[test]
fn comment_on_a_transferred_issue_posts_where_it_is_now() {
    let w = World::new();
    w.ready();
    w.fake.state().fail_comment_next = true;
    w.fl()
        .args(["record", "move", "1", "--to", "doing"])
        .assert()
        .success();
    w.fake.transfer(1);
    w.fl()
        .args(["github", "ledger", "comment", "1"])
        .assert()
        .success()
        .stdout(
            contains("moved\thttps://github.com/elsewhere/transferred/issues/1\n")
                .and(contains("comments\t1 posted, 0 already there")),
        );
    let s = w.fake.state();
    assert_eq!(s.transferred[&1].comments.len(), 1);
    assert!(s.transferred[&1].comments[0].contains("### fl move: allowed"));
    assert!(
        s.issues[&1].comments.is_empty(),
        "nothing at the old address"
    );
}

#[test]
fn comment_on_a_finding_posts_its_decisions_on_the_findings_issue() {
    let w = World::new();
    w.finding_raised();
    w.fake.state().fail_comment_next = true;
    w.fl()
        .args(["finding", "reproduce", "2", "--gate", "1"])
        .assert()
        .success();
    w.fl()
        .args(["github", "ledger", "comment", "2"])
        .assert()
        .success()
        .stdout(contains("comments\t1 posted, 0 already there"));
    let comments = w.fake.issue(2).comments;
    assert_eq!(comments.len(), 1, "{comments:?}");
    assert!(
        comments[0].contains("### fl reproduce: accepted"),
        "{}",
        comments[0]
    );
    assert!(
        comments[0].contains("| reproduction | no-bug | FAIL |"),
        "{}",
        comments[0]
    );
    assert!(w.fake.issue(1).comments.is_empty());
}

// Spec §4.2: a recovered comment links the commit that holds its
// decision, not the ledger's head.
#[test]
fn a_recovered_comment_links_the_commit_that_holds_its_decision() {
    let w = World::new();
    w.ready();
    w.fake.state().fail_comment_next = true;
    w.fl()
        .args(["record", "move", "1", "--to", "doing"])
        .assert()
        .success();
    let decided = w.fake.ledger_head().expect("the move's commit");
    w.fl()
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .success();
    assert_ne!(w.fake.ledger_head(), Some(decided.clone()));
    w.fl()
        .args(["github", "ledger", "comment", "1"])
        .assert()
        .success()
        .stdout(contains("comments\t1 posted, 1 already there"));
    let comments = w.fake.issue(1).comments;
    assert_eq!(comments.len(), 2, "{comments:?}");
    assert!(
        comments[1].contains("### fl move: allowed"),
        "{}",
        comments[1]
    );
    assert!(
        comments[1].contains(&format!("/commit/{decided})")),
        "{}",
        comments[1]
    );
}

#[test]
fn comment_recovers_an_attempts_comment_from_the_ledger() {
    let w = World::new();
    w.ready();
    w.fake.state().fail_comment_next = true;
    w.fl()
        .args(["attempt", "1", "--budget-usd-micros", "0"])
        .assert()
        .code(1);
    w.fl()
        .args(["github", "ledger", "comment", "1"])
        .assert()
        .success()
        .stdout(contains("comments\t1 posted, 0 already there"));
    let comments = w.fake.issue(1).comments;
    assert_eq!(comments.len(), 1, "{comments:?}");
    assert!(
        comments[0].contains("### fl attempt: refused"),
        "{}",
        comments[0]
    );
    assert!(
        comments[0].contains("| claude | refused |"),
        "{}",
        comments[0]
    );
}

// ⚠ A marker in a comment someone else wrote does not stop recovery.
#[test]
fn a_marker_someone_else_posted_does_not_stop_recovery() {
    let w = World::new();
    w.ready();
    w.fake.state().fail_comment_next = true;
    w.fl()
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .success();
    let ids = w.decision_ids();
    assert_eq!(ids.len(), 1, "{ids:?}");
    {
        let mut s = w.fake.state();
        let i = s.issues.get_mut(&1).unwrap();
        i.comments.push(format!(
            "<!-- fl:decision {{\"id\":\"{}\"}} -->\n\nnot fl",
            ids[0]
        ));
        i.comment_authors = vec!["someone-else".into()];
    }
    w.fl()
        .args(["github", "ledger", "comment", "1"])
        .assert()
        .success()
        .stdout(contains("comments\t1 posted, 0 already there"));
    assert_eq!(w.fake.issue(1).comments.len(), 2);
}

// ⚠ Several developers hold a token each: a comment a colleague's machine
// posted, under a login that wrote a decision under the item, counts —
// recovery run here does not post that decision again.
#[test]
fn a_comment_by_another_account_that_published_under_the_item_counts() {
    use fl_github::ledger::layout;
    let w = World::new();
    w.ready();
    w.fl()
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .success();
    let id = "urn:uuid:00000000-0000-7000-8000-0000000000c1";
    let theirs = fl_core::Decision {
        id: fl_core::Iri::parse(id).unwrap(),
        at: fl_core::At::from_unix_millis(1),
        record: fl_core::RecordId(
            fl_core::Iri::parse("https://github.com/acme/widgets/issues/1").unwrap(),
        ),
        finding: None,
        outcome: fl_core::Outcome::Check {
            transition: fl_core::TransitionOutcome {
                transition: "launch".into(),
                passed: true,
            },
        },
        rests_on: vec![],
    };
    let (path, text) = w.only_file_in("decisions");
    let line = layout::Line::Decision(theirs).encode("colleague");
    w.fake
        .hand_commit(&[(path.as_str(), Some(format!("{text}{line}\n").as_str()))]);
    {
        let mut s = w.fake.state();
        let i = s.issues.get_mut(&1).unwrap();
        assert_eq!(i.comments.len(), 1, "the check's own comment");
        i.comments.push(format!(
            "<!-- fl:decision {{\"id\":\"{id}\"}} -->\n\ntheirs"
        ));
        i.comment_authors = vec!["fake-user".into(), "colleague".into()];
    }
    w.fl()
        .args(["github", "ledger", "comment", "1"])
        .assert()
        .success()
        .stdout(contains("comments\t0 posted, 2 already there"));
    assert_eq!(w.fake.issue(1).comments.len(), 2, "nothing posted twice");
}

// ⚠ A decision line whose id fl does not write is skipped, said on
// stderr, and the command exits 1; nothing is posted for it.
#[test]
fn comment_skips_a_decision_whose_id_fl_does_not_write() {
    use fl_github::ledger::layout;
    let w = World::new();
    w.ready();
    w.fl()
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .success();
    let hostile = fl_core::Decision {
        // A control character too: the id is printed escaped, never raw.
        id: fl_core::Iri::parse("urn:x:a-->\u{7}<b>").unwrap(),
        at: fl_core::At::from_unix_millis(1),
        record: fl_core::RecordId(
            fl_core::Iri::parse("https://github.com/acme/widgets/issues/1").unwrap(),
        ),
        finding: None,
        outcome: fl_core::Outcome::Check {
            transition: fl_core::TransitionOutcome {
                transition: "launch".into(),
                passed: true,
            },
        },
        rests_on: vec![],
    };
    let (path, text) = w.only_file_in("decisions");
    let line = layout::Line::Decision(hostile).encode("fake-user");
    w.fake
        .hand_commit(&[(path.as_str(), Some(format!("{text}{line}\n").as_str()))]);
    w.fl()
        .args(["github", "ledger", "comment", "1"])
        .assert()
        .code(1)
        .stderr(
            contains("skipped\turn:x:a-->\\u{7}<b>\t")
                .and(contains("is not one fl writes"))
                .and(contains("\u{7}").not()),
        )
        .stdout(contains("comments\t0 posted, 1 already there"));
    assert_eq!(
        w.fake.issue(1).comments.len(),
        1,
        "only the check's own comment"
    );
}

// Decision 2: a comment recovered on a repository that is not private
// shows no error detail, and no stand-in for one.
#[test]
fn comment_on_a_repository_that_is_not_private_recovers_no_error_detail() {
    let w = World::new();
    let program = w.repo.path().join("no-such-gate");
    w.gated_with("no-bug", program.to_str().unwrap());
    w.init();
    w.export();
    w.fake.state().repos[0].visibility = "public".into();
    w.fake.state().fail_comment_next = true;
    w.fl()
        .args(["record", "move", "1", "--to", "doing"])
        .assert()
        .code(2);
    w.fl()
        .args(["github", "ledger", "comment", "1"])
        .assert()
        .success()
        .stdout(contains("comments\t1 posted, 0 already there"));
    let comments = w.fake.issue(1).comments;
    assert_eq!(comments.len(), 1, "{comments:?}");
    let c = &comments[0];
    assert!(c.contains("| ERROR |"), "{c}");
    assert!(!c.contains("<details>"), "{c}");
    assert!(!c.contains("withheld"), "{c}");
    assert!(!c.contains("no-such-gate"), "{c}");
}

#[test]
fn comment_without_the_ledger_key_is_refused() {
    let w = World::bound(false);
    w.gated();
    w.fl()
        .args(["github", "ledger", "comment", "1"])
        .assert()
        .code(2)
        .stderr(contains("does not name the GitHub ledger"));
}

// docs/github-ledger.md "What a decision costs", and spec §3.4: the
// requests a steady-state `check --record` makes, by kind. A count that
// changes changes the doc and §3.4 in the same commit.
#[test]
fn a_steady_state_decision_costs_what_the_docs_say() {
    let w = World::new();
    w.ready();
    w.fl()
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .success();
    w.fake.state().requests.clear();
    w.fl()
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .success();
    let requests = w.fake.state().requests.clone();
    let containing = |part: &str| requests.iter().filter(|r| r.contains(part)).count();
    assert_eq!(
        [
            requests_equal(&w, "GET /repos/acme/widgets"),
            containing("/rules/branches/fl/ledger"),
            containing("/git/ref/heads/fl/ledger"),
            containing("/compare/"),
            requests_equal(&w, "POST /graphql"),
            containing("/git/blobs/"),
            requests_equal(&w, "GET /user"),
            requests_equal(&w, "POST /repos/acme/widgets/issues/1/comments"),
            requests_equal(&w, "GET /repos/acme/widgets/issues/1"),
        ],
        // The tracker's own read and the visibility; the rules; the head
        // twice; no compare; the format's listing, the directories'
        // listing and the commit; the two segments the last decision grew;
        // who fl writes as; the comment; the tracker's two reads of the
        // record's issue.
        [2, 1, 2, 0, 3, 2, 1, 1, 2],
        "{requests:#?}"
    );
    // ⚠ Every request, of any kind: eleven for the ledger and three for the
    // tracker. A request of a kind not counted above shows here.
    assert_eq!(requests.len(), 14, "{requests:#?}");
}

// ⚠ Spec §4.2, §8.3: a gate named with a pipe, backticks, a mention, a
// reference (`#1` and `GH-1`), a comment opener and a newline is escaped
// in its comment — it notifies no one, links nothing, opens nothing,
// breaks no table.
#[test]
fn a_gate_named_with_markup_is_escaped_in_its_comment() {
    let w = World::new();
    w.gated_as("a|b `c` @someone #1 GH-1 <!-- x\ny");
    w.init();
    w.export();
    w.fl()
        .args(["record", "move", "1", "--to", "doing"])
        .assert()
        .success();
    let comments = w.fake.issue(1).comments;
    assert_eq!(comments.len(), 1, "{comments:?}");
    let c = &comments[0];
    assert!(
        c.contains(
            r"| launch | a\|b \`c\` @&#8203;someone #&#8203;1 GH-&#8203;1 &lt;!-- x<br>y | PASS |"
        ),
        "{c}"
    );
    assert!(!c.contains("@someone"), "{c}");
    assert!(!c.contains("#1 "), "{c}");
    assert!(!c.contains("GH-1"), "{c}");
    assert_eq!(
        c.matches("<!--").count(),
        1,
        "only the marker opens a comment: {c}"
    );
}

// ⚠ A carriage return is dropped from a comment, so one inside a reference
// or a URL must not hide it from the escaping: each is neutralised as
// though the carriage return were not there.
#[test]
fn a_gate_named_with_a_carriage_return_inside_a_reference_or_a_url_is_escaped() {
    let w = World::new();
    w.gated_as("GH\r-1 G\rH-2 https:/\r/evil.example www\r.evil.example");
    w.init();
    w.export();
    w.fl()
        .args(["record", "move", "1", "--to", "doing"])
        .assert()
        .success();
    let comments = w.fake.issue(1).comments;
    assert_eq!(comments.len(), 1, "{comments:?}");
    let c = &comments[0];
    assert!(
        c.contains(
            "| launch | GH-&#8203;1 GH-&#8203;2 https:/&#8203;/evil.example \
             www&#8203;.evil.example | PASS |"
        ),
        "{c}"
    );
    assert!(!c.contains("GH-1"), "{c}");
    assert!(!c.contains("GH-2"), "{c}");
    assert!(!c.contains("https://evil"), "{c}");
    assert!(!c.contains("www.evil"), "{c}");
    assert!(!c.contains('\r'), "{c:?}");
}

/// What names this machine, read now and never written down. Paths (the
/// working tree, `$HOME`) match anywhere; words (`$USER`, the host name)
/// match whole and only from four letters on — a two-letter host name
/// would match ordinary text.
struct MachineNames {
    tree: Tree,
    home: String,
    /// `$USER` and the host name, each only when it can be checked.
    words: Vec<String>,
}

impl MachineNames {
    fn of(w: &World) -> MachineNames {
        let home = std::env::var("HOME").expect("HOME is set");
        assert!(home.len() > 1, "a home to look for");
        let tree = Tree {
            given: w.repo.path().display().to_string(),
            canonical: w.repo.path().canonicalize().unwrap().display().to_string(),
        };
        let host = Sys::new("hostname")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|h| !h.is_empty())
            .or_else(|| {
                fs::read_to_string("/proc/sys/kernel/hostname")
                    .ok()
                    .map(|h| h.trim().to_string())
            })
            .unwrap_or_default();
        let user = std::env::var("USER").unwrap_or_default();
        let mut words = Vec::new();
        for (what, name) in [("the host name", host), ("$USER", user)] {
            if name.is_empty() {
                eprintln!("scan: {what} is unset, so it is not checked");
            } else if name.chars().count() < 4 {
                eprintln!(
                    "scan: {what} has fewer than 4 letters, so it is not checked: \
                     it would match ordinary text"
                );
            } else {
                words.push(name);
            }
        }
        MachineNames { tree, home, words }
    }

    /// Every name there is to find, read from the fields themselves.
    /// The working tree stands for itself by its canonical form.
    fn all(&self) -> BTreeSet<String> {
        [&self.tree.canonical, &self.home]
            .into_iter()
            .chain(&self.words)
            .cloned()
            .collect()
    }
}

/// The working tree: one name in two forms, the path as given and the
/// canonical one. They differ where the temporary directory is reached
/// through a symlink, and `fl` runs its gates in the canonical one.
struct Tree {
    given: String,
    canonical: String,
}

impl Tree {
    /// Whether `text` holds either form.
    fn in_text(&self, text: &str) -> bool {
        text.contains(self.given.as_str()) || text.contains(self.canonical.as_str())
    }
}

#[test]
fn the_working_tree_is_found_in_either_form() {
    let tree = Tree {
        given: "/home/someone/link/w".into(),
        canonical: "/home/someone/real/w".into(),
    };
    assert!(
        tree.in_text("ran in /home/someone/real/w"),
        "canonical only"
    );
    assert!(tree.in_text("ran in /home/someone/link/w"), "as given only");
    assert!(!tree.in_text("ran in /home/someone"), "neither");
}

/// Every name in `names` that some text in `texts` holds: a path anywhere
/// (the working tree in either form), a word whole.
fn found(texts: &[String], names: &MachineNames) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for text in texts {
        if names.tree.in_text(text) {
            out.insert(names.tree.canonical.clone());
        }
        if text.contains(names.home.as_str()) {
            out.insert(names.home.clone());
        }
        for word in &names.words {
            if holds_word(text, word) {
                out.insert(word.clone());
            }
        }
    }
    out
}

/// Comments as published and as read: each raw, and again with its
/// escapes undone, so `ci\_runner` reads `ci_runner`.
fn comment_texts(comments: &[String]) -> Vec<String> {
    comments
        .iter()
        .flat_map(|c| [c.clone(), unescape_comment(c)])
        .collect()
}

/// `c` with a comment's escapes undone: each zero-width space dropped,
/// `<br>` a newline, a backslash before ASCII punctuation dropped, then
/// `&lt;`, `&gt;` and, last, `&amp;` decoded.
fn unescape_comment(c: &str) -> String {
    let c = c.replace("&#8203;", "").replace("<br>", "\n");
    let mut out = String::with_capacity(c.len());
    let mut chars = c.chars().peekable();
    while let Some(ch) = chars.next() {
        match chars.peek() {
            Some(&next) if ch == '\\' && next.is_ascii_punctuation() => {
                out.push(next);
                chars.next();
            }
            _ => out.push(ch),
        }
    }
    out.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// Ledger files as published and as read: each file raw, and every string
/// and key of every `.jsonl` line decoded — a JSON escape such as `\n`
/// before a name would otherwise hide it.
fn line_texts(files: &BTreeMap<String, String>) -> Vec<String> {
    let mut out: Vec<String> = files.values().cloned().collect();
    for (path, text) in files.iter().filter(|(p, _)| p.ends_with(".jsonl")) {
        for line in text.lines().filter(|l| !l.is_empty()) {
            let v: serde_json::Value =
                serde_json::from_str(line).unwrap_or_else(|e| panic!("{path}: {e}: {line}"));
            json_strings(&v, &mut out);
        }
    }
    out
}

/// Every string and key in `v`, at any depth.
fn json_strings(v: &serde_json::Value, out: &mut Vec<String>) {
    match v {
        serde_json::Value::String(s) => out.push(s.clone()),
        serde_json::Value::Array(a) => a.iter().for_each(|v| json_strings(v, out)),
        serde_json::Value::Object(o) => {
            for (k, v) in o {
                out.push(k.clone());
                json_strings(v, out);
            }
        }
        _ => {}
    }
}

// The scan reads a name through each escape it can be published under: a
// comment's backslash, and a ledger line's `\n` and `\u` escapes. Fixture
// names only, so the test holds on any machine.
#[test]
fn the_scan_sees_a_name_through_the_escapes_it_is_published_under() {
    let names = MachineNames {
        tree: Tree {
            given: "/home/someone/bin/lint".into(),
            canonical: "/home/someone/bin/lint".into(),
        },
        home: "/home/someone".into(),
        words: vec!["ci_runner".into()],
    };
    let comment = vec![r"| broken | on ci\_runner |".to_string()];
    assert!(
        found(&comment, &names).is_empty(),
        "raw, the escape hides it"
    );
    assert_eq!(
        found(&comment_texts(&comment), &names),
        BTreeSet::from(["ci_runner".to_string()])
    );
    let files = BTreeMap::from([(
        "runs/k/1.jsonl".to_string(),
        "{\"output_excerpt\":\"pwd\\nci_runner\"}\n\
         {\"d\":\"\\u002fhome\\u002fsomeone\\u002fbin\\u002flint\"}\n"
            .to_string(),
    )]);
    let raw: Vec<String> = files.values().cloned().collect();
    assert!(found(&raw, &names).is_empty(), "raw, the escapes hide them");
    assert_eq!(found(&line_texts(&files), &names), names.all());
}

/// Whether `text` holds `word` with no letter, digit, `-` or `_` on
/// either side.
fn holds_word(text: &str, word: &str) -> bool {
    let edge = |c: Option<char>| !c.is_some_and(|c| c.is_alphanumeric() || c == '-' || c == '_');
    text.match_indices(word).any(|(i, _)| {
        edge(text[..i].chars().next_back()) && edge(text[i + word.len()..].chars().next())
    })
}

/// A world whose gate prints where it runs and fails, with a second gate
/// whose program does not exist (its detail names its path), and every
/// kind of comment: a refused move and a failed check posted live; a check
/// whose comment failed, recovered from the ledger; an attempt; a
/// reproduction by the gate that errors (refused) and by the one that
/// fails (accepted).
fn telling(private: bool) -> World {
    let w = World::new();
    fs::write(
        w.repo.path().join("check.sh"),
        "#!/bin/sh\npwd\necho \"$HOME\"\necho \"$USER\"\nhostname\n[ ! -e bug ]\n",
    )
    .unwrap();
    w.gated();
    let broken = w.repo.path().join("no-such-gate");
    w.fl()
        .args([
            "gate",
            "add",
            "--project",
            "1",
            "--name",
            "broken",
            "--glob",
            "src/**/*.rs",
            "--program",
            broken.to_str().unwrap(),
        ])
        .assert()
        .success();
    w.init();
    w.export();
    if !private {
        w.fake.state().repos[0].visibility = "public".into();
    }
    fs::write(w.repo.path().join("bug"), "").unwrap();
    let decide = |args: &[&str], code: i32| {
        w.fl()
            .args(args)
            .assert()
            .code(code)
            .stderr(contains(NOT_POSTED).not());
    };
    decide(&["record", "move", "1", "--to", "doing"], 1);
    decide(&["check", "launch", "--project", "1", "--record", "1"], 1);
    w.fake.state().fail_comment_next = true;
    w.fl()
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .code(1)
        .stderr(contains(NOT_POSTED));
    w.fl()
        .args(["github", "ledger", "comment", "1"])
        .assert()
        .success()
        .stdout(contains("comments\t1 posted"));
    decide(&["attempt", "1", "--budget-usd-micros", "0"], 1);
    w.fl()
        .args([
            "finding", "raise", "--record", "1", "--claim", "a bug", "--by", "reviewer",
        ])
        .assert()
        .success();
    decide(&["finding", "reproduce", "2", "--gate", "2"], 2);
    decide(&["finding", "reproduce", "2", "--gate", "1"], 0);
    w
}

// ⚠ Decision 2, spec §8.3: on a repository that is not private, no
// comment, published line or commit message names this machine. The same
// decisions on a private repository show every name — in comments and in
// ledger lines, each on its own — so the scan cannot pass by looking at
// nothing, and the public repository publishes as many lines and commits.
#[test]
fn comments_and_lines_on_a_repository_that_is_not_private_hold_no_path_home_or_host_name() {
    let private = telling(true);
    let names = MachineNames::of(&private);
    let comments = [
        private.fake.issue(1).comments,
        private.fake.issue(2).comments,
    ]
    .concat();
    assert_eq!(comments.len(), 6, "every kind of comment: {comments:?}");
    let surfaces = [
        ("comments", comment_texts(&comments)),
        ("ledger lines", line_texts(&private.fake.ledger_files())),
    ];
    for (surface, texts) in &surfaces {
        assert_eq!(
            found(texts, &names),
            names.all(),
            "the private {surface} show every name"
        );
    }
    let decisions = private.decision_ids().len();
    let messages = private.fake.ledger_commit_messages().len();
    assert!(decisions > 0 && messages > 0, "{decisions} {messages}");

    let public = telling(false);
    let names = MachineNames::of(&public);
    let comments = [public.fake.issue(1).comments, public.fake.issue(2).comments].concat();
    assert_eq!(comments.len(), 6, "{comments:?}");
    assert_eq!(
        public.decision_ids().len(),
        decisions,
        "as many decision lines"
    );
    let published_messages = public.fake.ledger_commit_messages();
    assert_eq!(
        published_messages.len(),
        messages,
        "as many commit messages"
    );
    let surfaces = [
        ("comments", comment_texts(&comments)),
        ("ledger lines", line_texts(&public.fake.ledger_files())),
        ("commit messages", published_messages),
    ];
    for (surface, texts) in &surfaces {
        assert_eq!(
            found(texts, &names),
            BTreeSet::new(),
            "the public {surface} name this machine"
        );
    }
}

// ⚠ An issue another repository holds is refused before any request
// touches an issue: recovery never posts outside the bound repository.
#[test]
fn comment_on_an_issue_another_repository_holds_is_refused() {
    let w = World::new();
    w.ready();
    w.fake.state().requests.clear();
    w.fl()
        .args([
            "github",
            "ledger",
            "comment",
            "https://github.com/acme/other/issues/1",
        ])
        .assert()
        .code(2)
        .stderr(contains("searched: the issues of acme/widgets"))
        .stdout(contains("comments\t").not());
    let requests = w.fake.state().requests.clone();
    assert!(
        !requests.iter().any(|r| r.contains("/issues/")),
        "{requests:?}"
    );
    assert!(w.fake.issue(1).comments.is_empty());
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
