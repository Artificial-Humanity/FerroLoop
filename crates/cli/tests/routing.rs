//! A routed project (routing spec): two tiers — the local store and the
//! fake GitHub's `acme/widgets` — and the rule that routes each new item.

use assert_cmd::Command;
use fl_github::fake::FakeGithub;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;
use std::fs;
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

/// The config line binding the project to the fake's repository.
const BOUND: &str = "tracker = { github = \"acme/widgets\", credential = \"env\" }\n";

struct R {
    home: tempfile::TempDir,
    repo: tempfile::TempDir,
    fake: FakeGithub,
}

/// A git working tree with `src/a.rs`, whose project's config entry
/// carries `tracker` — [`BOUND`], or "" for none.
fn world(tracker: &str) -> R {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q"]);
    git(repo.path(), &["config", "user.email", "t@example.com"]);
    git(repo.path(), &["config", "user.name", "t"]);
    fs::create_dir_all(repo.path().join("src")).unwrap();
    fs::write(repo.path().join("src/a.rs"), "fn a() {}").unwrap();
    git(repo.path(), &["add", "-A"]);
    git(repo.path(), &["commit", "-qm", "first"]);
    let r = R {
        home,
        repo,
        fake: FakeGithub::start("acme/widgets"),
    };
    r.configure(r.home.path(), tracker);
    r
}

impl R {
    /// `home`'s config: this repository's project, its store under `home`,
    /// and `tracker`.
    fn configure(&self, home: &Path, tracker: &str) {
        let cfg = format!(
            "[[project]]\nroot = \"{}\"\nstore = \"{}\"\n{tracker}",
            self.repo.path().canonicalize().unwrap().display(),
            home.join("fl.redb").display()
        );
        fs::create_dir_all(home.join("config/fl")).unwrap();
        fs::write(home.join("config/fl/config.toml"), cfg).unwrap();
    }

    /// `fl` on the machine whose home is `home`.
    fn fl_at(&self, home: &Path) -> Command {
        let mut c = Command::cargo_bin("fl").unwrap();
        c.env("XDG_CONFIG_HOME", home.join("config"))
            .env("XDG_DATA_HOME", home.join("data"))
            .env("FL_GITHUB_TOKEN", "t")
            .env("FL_GITHUB_API_URL", self.fake.url())
            .env_remove("GITHUB_TOKEN")
            .env_remove("FL_DB")
            .current_dir(self.repo.path());
        c
    }

    fn fl(&self) -> Command {
        self.fl_at(self.home.path())
    }

    /// `fl args`, which must succeed; its stdout.
    fn ok(&self, args: &[&str]) -> String {
        let out = self.fl().args(args).output().unwrap();
        assert!(
            out.status.success(),
            "fl {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    /// The project, routed with the starting set.
    fn routed(&self) {
        self.ok(&["project", "add", "."]);
        self.ok(&["routing", "set", "--project", "1", "code", "local"]);
    }
}

// Routing spec §1.2, §2.3: the first set writes the starting set, and says
// what changes for a project that was local-only.
#[test]
fn the_first_set_writes_the_starting_set_and_says_how_handles_change() {
    let g = world("");
    g.ok(&["project", "add", "."]);
    g.fl()
        .args(["routing", "set", "--project", "1", "ops", "github"])
        .assert()
        .success()
        .stdout("ops\tgithub\t-\n")
        .stderr(
            contains("wrote the starting set first")
                .and(contains("security to github, sensitive"))
                .and(contains("handles change in this project"))
                .and(contains("`#3` named local item 3")),
        );
    let shown = g.ok(&["routing", "show", "--project", "1"]);
    assert_eq!(
        shown,
        "code\tlocal\t-\ndesign\tgithub\t-\nops\tgithub\t-\nproduct\tgithub\t-\n\
         security\tgithub\tsensitive\ntests\tlocal\t-\n"
    );
}

#[test]
fn a_github_bound_project_hears_that_a_bare_number_now_names_a_local_item() {
    let g = world(BOUND);
    g.ok(&["project", "add", "."]);
    g.fl()
        .args(["routing", "set", "--project", "1", "code", "local"])
        .assert()
        .success()
        .stderr(contains("a bare number such as `41` named GitHub issue 41"));
    assert!(
        g.fake.state().requests.is_empty(),
        "authoring the map needs no GitHub"
    );
}

#[test]
fn a_later_set_changes_one_area_and_says_nothing_of_handles() {
    let g = world("");
    g.routed();
    g.fl()
        .args([
            "routing",
            "set",
            "--project",
            "1",
            "code",
            "github",
            "--sensitive",
        ])
        .assert()
        .success()
        .stdout("code\tgithub\tsensitive\n")
        .stderr(contains("handles change in this project").not());
    assert!(
        g.ok(&["routing", "show", "--project", "1"])
            .starts_with("code\tgithub\tsensitive\n")
    );
}

#[test]
fn an_area_or_a_tier_that_is_not_one_is_refused_and_nothing_is_written() {
    let g = world("");
    g.ok(&["project", "add", "."]);
    g.fl()
        .args(["routing", "set", "--project", "1", "Code", "local"])
        .assert()
        .failure()
        .stderr(contains("is not an area name"));
    g.fl()
        .args(["routing", "set", "--project", "1", "code", "cloud"])
        .assert()
        .failure()
        .stderr(contains("is not a tier"));
    g.fl()
        .args(["routing", "show", "--project", "1"])
        .assert()
        .success()
        .stdout("")
        .stderr(contains("has no routing map"));
}

// Routing spec decision 22: a set that only changes a tier keeps the area's
// sensitivity.
#[test]
fn a_tier_change_keeps_the_areas_sensitivity() {
    let g = world("");
    g.routed();
    assert_eq!(
        g.ok(&["routing", "set", "--project", "1", "security", "local"]),
        "security\tlocal\tsensitive\n"
    );
}

// Routing spec decision 20: handles are numbered per store, so a routed
// store holds one project; the refusal names the remedy and writes nothing.
#[test]
fn routing_a_project_that_shares_its_store_is_refused_naming_its_own_store() {
    let g = world("");
    g.ok(&["project", "add", "."]);
    let other = tempfile::tempdir().unwrap();
    git(other.path(), &["init", "-q"]);
    git(
        other.path(),
        &[
            "-c",
            "user.email=t@example.com",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "first",
        ],
    );
    let db = g.home.path().join("fl.redb");
    g.fl()
        .args(["--db", db.to_str().unwrap(), "project", "add"])
        .arg(other.path())
        .assert()
        .success();
    g.fl()
        .args(["routing", "set", "--project", "1", "code", "local"])
        .assert()
        .failure()
        .stderr(contains("needs a store of its own").and(contains("its own `store`")));
    g.fl()
        .args(["routing", "show", "--project", "1"])
        .assert()
        .success()
        .stderr(contains("has no routing map"));
}

// Routing spec §2.3: the import that first routes a store says how handles
// change there, as the first `set` does where the project is authored.
#[test]
fn an_import_that_first_routes_a_store_says_how_handles_change() {
    let g = world("");
    g.routed();
    g.ok(&["manifest", "export", "--project", "1"]);
    let other = tempfile::tempdir().unwrap();
    g.configure(other.path(), BOUND);
    g.fl_at(other.path())
        .args(["manifest", "import"])
        .assert()
        .success()
        .stderr(contains("a bare number such as `41` named GitHub issue 41"));
    g.fl_at(other.path())
        .args(["manifest", "import"])
        .assert()
        .success()
        .stderr(contains("handles change in this project").not());
}
