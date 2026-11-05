//! `fl project init`: one command from a directory to a registered, routed project. Every test
//! runs `fl` with private config and data homes, so none reads or writes the developer's own.

use assert_cmd::Command;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command as Sys;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Sys::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

struct Env {
    home: tempfile::TempDir,
    work: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        Self {
            home: tempfile::tempdir().unwrap(),
            work: tempfile::tempdir().unwrap(),
        }
    }

    fn fl(&self, cwd: &Path) -> Command {
        let mut c = Command::cargo_bin("fl").unwrap();
        c.env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.home.path().join("config"))
            .env("XDG_DATA_HOME", self.home.path().join("data"))
            .env_remove("FL_DB")
            .env_remove("FL_STORE_BACKEND")
            .current_dir(cwd);
        c
    }

    fn config_path(&self) -> PathBuf {
        self.home.path().join("config/fl/config.toml")
    }

    fn config(&self) -> String {
        std::fs::read_to_string(self.config_path()).unwrap_or_default()
    }

    fn dir(&self, name: &str) -> PathBuf {
        self.work.path().canonicalize().unwrap().join(name)
    }

    fn init_json(&self, args: &[&str]) -> Value {
        let out = self
            .fl(self.work.path())
            .args(["project", "init"])
            .args(args)
            .arg("--json")
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        serde_json::from_slice(&out).unwrap()
    }
}

#[test]
fn a_new_directory_becomes_a_project_every_fl_command_accepts() {
    let env = Env::new();
    let root = env.dir("shiny");
    let created = env.init_json(&[root.to_str().unwrap(), "--purpose", "a shiny thing"]);

    assert_eq!(created["name"], "shiny");
    assert_eq!(created["root"], root.display().to_string());
    assert_eq!(created["git"]["initial_commit"], true);
    assert_eq!(created["mcp_catalog_created"], true);
    assert_eq!(created["tracker"], Value::Null);
    assert_eq!(
        created["areas"],
        serde_json::json!([
            "backlog", "code", "design", "product", "security", "tests", "ux"
        ])
    );
    assert_eq!(
        git(&root, &["log", "-1", "--format=%s"]),
        "chore: initial commit"
    );
    assert!(root.join(".fl/mcp.toml").is_file());

    // The config names the project, and fl reads it back: from inside the project, the
    // catalog commands find its own store and the routing map.
    let config = env.config();
    assert!(
        config.contains("[[project]]") && config.contains("name = \"shiny\""),
        "{config}"
    );
    let store = env.home.path().join("data/fl/shiny.fls");
    assert!(store.is_file());
    assert_eq!(created["store"], store.display().to_string());
    env.fl(&root)
        .args(["project", "list"])
        .assert()
        .success()
        .stdout(contains(root.to_str().unwrap()));
    env.fl(&root)
        .args(["routing", "show", "--project", "1"])
        .assert()
        .success()
        .stdout(
            contains("backlog")
                .and(contains("ux"))
                .and(contains("security")),
        );
    env.fl(&root)
        .args(["agent", "area", "list"])
        .assert()
        .success()
        .stdout(contains("shiny").and(contains("a shiny thing")));
}

#[test]
fn the_plain_output_is_the_name_the_root_and_the_store() {
    let env = Env::new();
    let root = env.dir("plain");
    env.fl(env.work.path())
        .args(["project", "init", root.to_str().unwrap()])
        .assert()
        .success()
        .stdout(format!(
            "plain\t{}\t{}\n",
            root.display(),
            env.home.path().join("data/fl/plain.fls").display()
        ))
        .stderr(contains("git init").and(contains("initial empty commit")));
}

#[test]
fn a_relative_path_means_the_current_directory() {
    let env = Env::new();
    std::fs::create_dir(env.dir("base")).unwrap();
    let created = {
        let out = env
            .fl(&env.dir("base"))
            .args(["project", "init", "./sub/../child", "--json"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        serde_json::from_slice::<Value>(&out).unwrap()
    };
    assert_eq!(created["name"], "child");
    assert_eq!(
        created["root"],
        env.dir("base").join("child").display().to_string()
    );
    assert!(!env.dir("base").join("sub").exists());
    env.fl(env.work.path())
        .args(["project", "init", ""])
        .assert()
        .failure()
        .stderr(contains("empty"));
}

#[test]
fn running_it_again_changes_nothing() {
    let env = Env::new();
    let root = env.dir("steady");
    let first = env.init_json(&[root.to_str().unwrap(), "--github", "acme/steady"]);
    let config = env.config();
    let head = git(&root, &["rev-parse", "HEAD"]);

    let second = env.init_json(&[root.to_str().unwrap(), "--github", "acme/steady"]);
    assert_eq!(first["registered"], true);
    assert_eq!(second["registered"], false);
    assert_eq!(second["project"], first["project"]);
    assert_eq!(second["git"]["initial_commit"], false);
    assert_eq!(second["mcp_catalog_created"], false);
    assert_eq!(env.config(), config);
    assert_eq!(git(&root, &["rev-parse", "HEAD"]), head);
    assert_eq!(git(&root, &["rev-list", "--count", "HEAD"]), "1");
    assert_eq!(config.matches("[[project]]").count(), 1);
}

#[test]
fn an_existing_repository_keeps_its_history_and_gets_no_commit() {
    let env = Env::new();
    let root = env.dir("old");
    std::fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q"]);
    std::fs::write(root.join("a.txt"), "a").unwrap();
    git(&root, &["add", "-A"]);
    git(
        &root,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@e.x",
            "commit",
            "-qm",
            "mine",
        ],
    );
    let head = git(&root, &["rev-parse", "HEAD"]);

    let created = env.init_json(&[root.to_str().unwrap()]);
    assert_eq!(created["git"]["initialized"], false);
    assert_eq!(created["git"]["initial_commit"], false);
    assert_eq!(created["git"]["head"], head);
    assert_eq!(git(&root, &["log", "--format=%s"]), "mine");
}

#[test]
fn the_github_binding_is_written_and_read_back_by_fl() {
    let env = Env::new();
    let root = env.dir("hosted");
    let created = env.init_json(&[
        root.to_str().unwrap(),
        "--github",
        "acme/hosted",
        "--credential",
        "env",
        "--name",
        "Hosted_One",
    ]);
    assert_eq!(created["name"], "hosted_one");
    assert_eq!(
        created["tracker"],
        serde_json::json!({ "github": "acme/hosted", "credential": "env" })
    );
    let config = env.config();
    assert!(config.contains("[project.tracker]"), "{config}");
    assert!(config.contains("credential = \"env\""), "{config}");
    // The catalog commands never contact GitHub, so this loads the binding and runs offline.
    env.fl(&root)
        .args(["project", "list"])
        .assert()
        .success()
        .stdout(contains(root.to_str().unwrap()));
}

#[test]
fn the_author_flag_names_who_made_the_commit_and_the_area() {
    let env = Env::new();
    let root = env.dir("authored");
    env.init_json(&[root.to_str().unwrap(), "--by", "ferris"]);
    assert_eq!(git(&root, &["log", "-1", "--format=%an"]), "ferris");
    env.fl(&root)
        .args(["agent", "area", "list", "--json"])
        .assert()
        .success()
        .stdout(contains("\"by\": \"ferris\""));
}

#[test]
fn bad_input_is_refused_with_a_reason_and_makes_nothing() {
    let env = Env::new();
    let root = env.dir("never");
    let init = |args: &[&str]| {
        let mut cmd = env.fl(env.work.path());
        cmd.args(["project", "init"]).args(args);
        cmd.assert().failure()
    };
    init(&[root.to_str().unwrap(), "--name", "Not A Slug"]).stderr(contains("project name"));
    init(&[root.to_str().unwrap(), "--github", "not-a-repo"]).stderr(contains("owner/repo"));
    init(&[
        root.to_str().unwrap(),
        "--github",
        "a/b",
        "--credential",
        "token",
    ])
    .stderr(contains("credential"));
    // clap itself: a credential needs the repository it reaches.
    init(&[root.to_str().unwrap(), "--credential", "env"]).stderr(contains("--github"));
    assert!(!root.exists());
    assert!(!env.config_path().exists());
    assert!(!env.home.path().join("data").exists());

    let file = env.dir("a-file");
    std::fs::write(&file, "x").unwrap();
    init(&[file.to_str().unwrap()]).stderr(contains("not a directory"));
}

#[test]
fn a_name_taken_by_another_directory_is_refused() {
    let env = Env::new();
    let first = env.dir("first");
    env.init_json(&[first.to_str().unwrap(), "--name", "shared"]);
    let config = env.config();
    let second = env.dir("second");
    env.fl(env.work.path())
        .args([
            "project",
            "init",
            second.to_str().unwrap(),
            "--name",
            "shared",
        ])
        .assert()
        .failure()
        .stderr(contains("`shared` is already registered").and(contains("--name")));
    assert!(!second.exists());
    assert_eq!(env.config(), config);
}

#[test]
fn another_projects_config_block_and_comments_survive() {
    let env = Env::new();
    let dir = env.home.path().join("config/fl");
    std::fs::create_dir_all(&dir).unwrap();
    let original =
        "# mine\n[[project]]\nname = \"keep\"\nroot = \"/keep\"\nstore = \"/keep.fls\"\n";
    std::fs::write(dir.join("config.toml"), original).unwrap();
    env.init_json(&[env.dir("added").to_str().unwrap()]);
    let after = env.config();
    assert!(after.starts_with(original), "{after}");
    assert_eq!(after.matches("[[project]]").count(), 2);
}
