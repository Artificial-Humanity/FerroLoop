use assert_cmd::Command;
use predicates::str::contains;
use std::path::{Path, PathBuf};
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

/// Private config and data homes, so no test reads the developer's own.
struct Env {
    config: tempfile::TempDir,
    data: tempfile::TempDir,
}

impl Env {
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
    fn default_store(&self) -> PathBuf {
        self.data.path().join("fl").join("fl.redb")
    }
    fn write_config(&self, body: &str) {
        let dir = self.config.path().join("fl");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), body).unwrap();
    }
}

fn bind(repo: &Path, store: &Path) -> String {
    format!(
        "[[project]]\nroot = \"{}\"\nstore = \"{}\"\n",
        repo.display(),
        store.display()
    )
}

const STRANGER: &str = "urn:uuid:0190a1b2-c3d4-7e5f-8a6b-7c8d9e0f1a2b";

// Review Focus 4.
#[test]
fn a_project_bound_in_config_uses_its_store_from_a_subdirectory() {
    let env = Env::new();
    let repo = git_repo();
    let stores = tempfile::tempdir().unwrap();
    let a = stores.path().join("a.redb");
    env.write_config(&bind(repo.path(), &a));
    let sub = repo.path().join("sub");
    std::fs::create_dir(&sub).unwrap();
    env.fl(&sub)
        .args(["project", "add", repo.path().to_str().unwrap()])
        .assert()
        .success();
    assert!(a.exists(), "the bound store was not used");
    assert!(
        !env.default_store().exists(),
        "the default store was created anyway"
    );
}

// Review Focus 4.
#[cfg(unix)]
#[test]
fn a_symlinked_working_directory_binds_like_the_real_one() {
    let env = Env::new();
    let repo = git_repo();
    let stores = tempfile::tempdir().unwrap();
    let a = stores.path().join("a.redb");
    env.write_config(&bind(repo.path(), &a));
    let links = tempfile::tempdir().unwrap();
    let link = links.path().join("via-link");
    std::os::unix::fs::symlink(repo.path(), &link).unwrap();
    env.fl(&link).args(["project", "list"]).assert().success();
    assert!(a.exists(), "a symlinked cwd did not bind");
    assert!(!env.default_store().exists());
}

#[test]
fn precedence_is_db_then_env_then_config_then_default() {
    let env = Env::new();
    let repo = git_repo();
    let stores = tempfile::tempdir().unwrap();
    let flag = stores.path().join("flag.redb");
    let var = stores.path().join("env.redb");
    let cfg = stores.path().join("cfg.redb");
    env.write_config(&bind(repo.path(), &cfg));

    env.fl(repo.path())
        .env("FL_DB", &var)
        .args(["--db", flag.to_str().unwrap(), "project", "list"])
        .assert()
        .success();
    assert!(
        flag.exists() && !var.exists() && !cfg.exists(),
        "--db did not win"
    );

    env.fl(repo.path())
        .env("FL_DB", &var)
        .args(["project", "list"])
        .assert()
        .success();
    assert!(
        var.exists() && !cfg.exists(),
        "$FL_DB did not beat the config"
    );

    env.fl(repo.path())
        .args(["project", "list"])
        .assert()
        .success();
    assert!(
        cfg.exists() && !env.default_store().exists(),
        "the config did not beat the default"
    );

    let elsewhere = tempfile::tempdir().unwrap();
    env.fl(elsewhere.path())
        .args(["project", "list"])
        .assert()
        .success();
    assert!(
        env.default_store().exists(),
        "an unbound directory did not fall to the default"
    );
}

// Review Focus 3.
#[test]
fn a_malformed_config_is_an_error_naming_the_file() {
    let env = Env::new();
    let repo = git_repo();
    env.write_config("[[project]\nroot =");
    env.fl(repo.path())
        .args(["project", "list"])
        .assert()
        .code(2)
        .stderr(contains("config.toml"));
    assert!(
        !env.default_store().exists(),
        "a broken config fell through to the default store"
    );
}

// Review Focus 3.
#[test]
fn a_relative_store_path_in_config_is_refused() {
    let env = Env::new();
    let repo = git_repo();
    env.write_config(&format!(
        "[[project]]\nroot = \"{}\"\nstore = \"stores/a.redb\"\n",
        repo.path().display()
    ));
    env.fl(repo.path())
        .args(["project", "list"])
        .assert()
        .code(2)
        .stderr(contains("absolute"));
}

/// Two repos, each bound to its own store, each registered in it.
fn two_bound_stores(
    env: &Env,
) -> (
    tempfile::TempDir,
    tempfile::TempDir,
    tempfile::TempDir,
    PathBuf,
    PathBuf,
) {
    let (ra, rb) = (git_repo(), git_repo());
    let stores = tempfile::tempdir().unwrap();
    let (a, b) = (stores.path().join("a.redb"), stores.path().join("b.redb"));
    env.write_config(&format!("{}{}", bind(ra.path(), &a), bind(rb.path(), &b)));
    env.fl(ra.path())
        .args(["project", "add", ra.path().to_str().unwrap()])
        .assert()
        .success();
    env.fl(rb.path())
        .args(["project", "add", rb.path().to_str().unwrap()])
        .assert()
        .success();
    (ra, rb, stores, a, b)
}

#[test]
fn an_iri_selects_the_store_that_holds_it() {
    let env = Env::new();
    let (ra, rb, _stores, _a, _b) = two_bound_stores(&env);
    env.fl(ra.path())
        .args([
            "gate",
            "add",
            "--project",
            "1",
            "--name",
            "in-a",
            "--glob",
            "*.rs",
            "--program",
            "true",
        ])
        .assert()
        .success();
    let shown = env
        .fl(ra.path())
        .args(["gate", "show", "1"])
        .output()
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&shown.stdout).unwrap();
    let iri = json["id"].as_str().unwrap().to_string();

    env.fl(rb.path())
        .args(["gate", "show", &iri])
        .assert()
        .success()
        .stdout(contains("in-a"));
}

// Spec §6.4a.
#[test]
fn an_iri_no_store_holds_is_not_owned_and_names_every_store_searched() {
    let env = Env::new();
    let (_ra, rb, _stores, a, b) = two_bound_stores(&env);
    env.fl(rb.path())
        .args(["gate", "show", STRANGER])
        .assert()
        .code(2)
        .stderr(contains(a.to_str().unwrap()))
        .stderr(contains(b.to_str().unwrap()));
}

#[test]
fn a_store_that_cannot_be_opened_during_an_iri_search_is_an_error_not_a_skip() {
    let env = Env::new();
    let (ra, rb) = (git_repo(), git_repo());
    let stores = tempfile::tempdir().unwrap();
    let (a, b) = (stores.path().join("a.redb"), stores.path().join("b.redb"));
    std::fs::write(&b, b"this is not a database").unwrap();
    env.write_config(&format!("{}{}", bind(ra.path(), &a), bind(rb.path(), &b)));
    env.fl(ra.path())
        .args(["project", "add", ra.path().to_str().unwrap()])
        .assert()
        .success();
    env.fl(ra.path())
        .args(["gate", "show", STRANGER])
        .assert()
        .code(2)
        .stderr(contains("could not open"))
        .stderr(contains(b.to_str().unwrap()));
}
