//! `fl project create` (alias `init`), `add` and `list`: from a directory, a repository or a name
//! to a registered, routed project or a virtual group. Every test runs `fl` with private config
//! and data homes, so none reads or writes the developer's own.

use assert_cmd::Command;
use assert_cmd::assert::OutputAssertExt;
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
        let env = Self {
            home: tempfile::tempdir().unwrap(),
            work: tempfile::tempdir().unwrap(),
        };
        // The home directory holds the test's workspace marker: a deletion that finds no
        // workspace root anywhere refuses to remove a directory.
        std::fs::create_dir(env.home.path().join("Notes")).unwrap();
        env
    }

    fn fl(&self, cwd: &Path) -> Command {
        let mut c = Command::cargo_bin("fl").unwrap();
        c.env("HOME", self.home.path())
            .env("XDG_RUNTIME_DIR", self.home.path())
            .env("FL_HOST_SOCK", self.home.path().join("host.sock"))
            .env(
                "TMUX",
                format!("{},1,0", self.home.path().join("tmux.sock").display()),
            )
            .env("XDG_CONFIG_HOME", self.home.path().join("config"))
            .env("XDG_DATA_HOME", self.home.path().join("data"))
            .env("FL_WIRE_DISABLE_HOST_DISCOVERY", "1")
            .env_remove("FL_DB")
            .env_remove("FL_STORE_BACKEND")
            .env("FL_TEST_SPINUP_NO_SPAWN", "1")
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

    /// `fl project init <args> --json` (the earlier spelling).
    fn init_json(&self, args: &[&str]) -> Value {
        self.run_json("init", args)
    }

    fn run_json(&self, verb: &str, args: &[&str]) -> Value {
        let out = self
            .fl(self.work.path())
            .args(["project", verb])
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
    let created = env.init_json(&[
        root.to_str().unwrap(),
        "--purpose",
        "a shiny thing",
        "--git-init",
    ]);

    assert_eq!(created["slug"], "shiny");
    assert_eq!(created["kind"], "project");
    assert_eq!(created["path"], root.display().to_string());
    assert_eq!(created["has_git"], true);
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
    assert_eq!(created["store_path"], store.display().to_string());
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
fn the_plain_output_is_the_slug_the_kind_the_root_and_the_store() {
    let env = Env::new();
    let root = env.dir("plain");
    env.fl(env.work.path())
        .args(["project", "init", root.to_str().unwrap(), "--git-init"])
        .assert()
        .success()
        .stdout(format!(
            "plain\tproject\t{}\t{}\n",
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
    assert_eq!(created["slug"], "child");
    assert_eq!(
        created["path"],
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
    let first = env.init_json(&[
        root.to_str().unwrap(),
        "--github",
        "acme/steady",
        "--git-init",
    ]);
    let config = env.config();
    let head = git(&root, &["rev-parse", "HEAD"]);

    let second = env.init_json(&[
        root.to_str().unwrap(),
        "--github",
        "acme/steady",
        "--git-init",
    ]);
    assert_eq!(first["registered"], true);
    assert_eq!(second["registered"], false);
    assert_eq!(second["store_path"], first["store_path"]);
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

    let created = env.init_json(&[root.to_str().unwrap(), "--git-init"]);
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
    assert_eq!(created["slug"], "hosted_one");
    assert_eq!(created["name"], "Hosted_One");
    assert_eq!(
        created["tracker"],
        serde_json::json!({ "kind": "github", "repo": "acme/hosted", "credential": "env" })
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
    env.init_json(&[root.to_str().unwrap(), "--by", "ferris", "--git-init"]);
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
    init(&[root.to_str().unwrap(), "--slug", "Not A Slug"]).stderr(contains("project name"));
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
    env.init_json(&[first.to_str().unwrap(), "--slug", "shared"]);
    let config = env.config();
    let second = env.dir("second");
    env.fl(env.work.path())
        .args([
            "project",
            "init",
            second.to_str().unwrap(),
            "--slug",
            "shared",
        ])
        .assert()
        .failure()
        .stderr(contains("`shared` is already registered").and(contains("--slug")));
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

// --- the create shapes ------------------------------------------------------------------------

#[test]
fn create_with_a_path_takes_the_name_and_runs_no_git_by_default() {
    let env = Env::new();
    let root = env.dir("some-dir");
    let created = env.run_json(
        "create",
        &[
            "My Project",
            "--path",
            root.to_str().unwrap(),
            "--purpose",
            "pretty",
        ],
    );
    assert_eq!(created["slug"], "my-project");
    assert_eq!(created["name"], "My Project");
    assert_eq!(created["kind"], "project");
    assert_eq!(created["path"], root.display().to_string());
    assert_eq!(created["has_git"], false);
    assert_eq!(created["git"], Value::Null);
    assert!(
        !root.join(".git").exists(),
        "no repository without --git-init"
    );
    assert!(root.join(".fl/mcp.toml").is_file());
    assert!(
        env.config().contains("name = \"my-project\""),
        "{}",
        env.config()
    );
    // A directory that is not a repository is a project every command accepts.
    env.fl(&root)
        .args(["project", "list"])
        .assert()
        .success()
        .stdout(contains("project\tmy-project\t").and(contains(root.to_str().unwrap())));
}

#[test]
fn create_and_init_are_one_command_and_both_spellings_work() {
    let env = Env::new();
    let by_create = env.dir("by-create");
    let by_init = env.dir("by-init");
    let a = env.run_json(
        "create",
        &["Alpha", "--path", by_create.to_str().unwrap(), "--git-init"],
    );
    // The earlier form: the argument is the path.
    let b = env.run_json("init", &[by_init.to_str().unwrap(), "--git-init"]);
    // `init` with the new form, and `create` with the old.
    let c = env.run_json("init", &["Gamma", "--path", env.dir("c").to_str().unwrap()]);
    let d = env.run_json("create", &[env.dir("d").to_str().unwrap()]);
    assert_eq!(
        [&a, &b, &c, &d].map(|v| v["slug"].as_str().unwrap().to_string()),
        ["alpha", "by-init", "gamma", "d"]
    );
    assert_eq!(a["git"]["initial_commit"], true);
    assert_eq!(b["git"]["initial_commit"], true);
    assert_eq!(c["git"], Value::Null);
    assert_eq!(d["git"], Value::Null);
    assert_eq!(env.config().matches("[[project]]").count(), 4);
    // The earlier `--name` is the name; its slug is the kebab-case of it.
    let e = env.run_json(
        "init",
        &[env.dir("e").to_str().unwrap(), "--name", "Big Name"],
    );
    assert_eq!(
        (e["slug"].as_str(), e["name"].as_str()),
        (Some("big-name"), Some("Big Name"))
    );
}

#[test]
fn a_virtual_group_has_no_directory_store_config_or_git() {
    let env = Env::new();
    let created = env.run_json(
        "create",
        &["Council", "--virtual", "--purpose", "the strategists"],
    );
    assert_eq!(created["slug"], "council");
    assert_eq!(created["kind"], "virtual");
    assert_eq!(created["path"], Value::Null);
    assert_eq!(created["store_path"], Value::Null);
    assert_eq!(created["has_git"], false);
    assert_eq!(created["purpose"], "the strategists");
    assert_eq!(created["registered"], true);

    assert!(!env.config_path().exists(), "no config");
    let made: Vec<String> = std::fs::read_dir(env.home.path().join("data/fl"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        made,
        ["fl.fls"],
        "only the workspace store, no per-project store"
    );
    assert_eq!(
        std::fs::read_dir(env.work.path()).unwrap().count(),
        0,
        "no directory"
    );

    // Again: nothing changes. The area has no path.
    let again = env.run_json("create", &["Council", "--virtual"]);
    assert_eq!(again["registered"], false);
    env.fl(env.work.path())
        .args(["agent", "area", "list", "--json"])
        .assert()
        .success()
        .stdout(contains("\"area_key\": \"council\"").and(contains("\"path\": null")));
}

#[test]
fn a_virtual_group_takes_no_path_git_or_tracker_flags() {
    let env = Env::new();
    for flags in [
        &["--path", "/x"][..],
        &["--git-init"][..],
        &["--github", "a/b"][..],
        &["--credential", "env"][..],
    ] {
        let mut cmd = env.fl(env.work.path());
        cmd.args(["project", "create", "Nope", "--virtual"])
            .args(flags);
        cmd.assert().failure().stderr(contains("cannot be used"));
    }
    assert!(!env.config_path().exists());
    assert!(!env.home.path().join("data").exists());
}

#[test]
fn a_slug_cannot_be_a_project_and_a_virtual_group() {
    let env = Env::new();
    env.run_json("create", &["Shared", "--virtual"]);
    env.fl(env.work.path())
        .args([
            "project",
            "create",
            "Shared",
            "--path",
            env.dir("shared").to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(contains("virtual group").and(contains("fl agent area set shared --path")));
    assert!(!env.dir("shared").exists());
    assert!(!env.config_path().exists());

    env.run_json(
        "create",
        &["Solid", "--path", env.dir("solid").to_str().unwrap()],
    );
    env.fl(env.work.path())
        .args(["project", "create", "Solid", "--virtual"])
        .assert()
        .failure()
        .stderr(contains("directory-backed project"));
}

#[test]
fn add_registers_a_directory_that_is_not_a_git_repository() {
    let env = Env::new();
    let plain = env.dir("plain-add");
    std::fs::create_dir_all(&plain).unwrap();
    let db = env.home.path().join("add.fls");
    env.fl(env.work.path())
        .args([
            "--db",
            db.to_str().unwrap(),
            "project",
            "add",
            plain.to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(contains(plain.to_str().unwrap()));
    assert!(!plain.join(".git").exists(), "add never runs git");
    env.fl(env.work.path())
        .args([
            "--db",
            db.to_str().unwrap(),
            "project",
            "add",
            "/no/such/dir",
        ])
        .assert()
        .failure()
        .stderr(contains("does not exist as a directory"));
}

#[test]
fn list_shows_both_kinds_with_the_tracker_and_the_active_agents() {
    let env = Env::new();
    let root = env.dir("listed");
    env.run_json(
        "create",
        &[
            "Listed",
            "--path",
            root.to_str().unwrap(),
            "--github",
            "acme/listed",
        ],
    );
    env.run_json("create", &["Council", "--virtual"]);
    for (name, project) in [("ada", "listed"), ("bea", "council"), ("cy", "council")] {
        env.fl(env.work.path())
            .args([
                "agent",
                "register",
                name,
                "--project",
                project,
                "--role",
                "assistant",
                "--by",
                "owner",
            ])
            .assert()
            .success();
    }
    let out = env
        .fl(env.work.path())
        .args(["project", "list", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let listed: Vec<Value> = serde_json::from_slice(&out).unwrap();
    assert_eq!(listed.len(), 2);
    let by = |slug: &str| listed.iter().find(|p| p["slug"] == slug).unwrap();
    assert_eq!(by("listed")["kind"], "project");
    assert_eq!(by("listed")["tracker"]["repo"], "acme/listed");
    assert_eq!(by("listed")["active_agent_count"], 1);
    assert_eq!(by("council")["kind"], "virtual");
    assert_eq!(by("council")["active_agent_count"], 2);
    assert_eq!(by("council")["path"], Value::Null);
    env.fl(env.work.path())
        .args(["project", "list"])
        .assert()
        .success()
        .stdout(
            contains(format!(
                "project\tlisted\t{}\tacme/listed\t1\tListed\n",
                root.display()
            ))
            .and(contains("virtual\tcouncil\t-\t-\t2\tCouncil\n")),
        );
}

// --- fl project delete ------------------------------------------------------------------------

impl Env {
    fn register_agent(&self, name: &str, project: &str) {
        self.fl(self.work.path())
            .args([
                "agent",
                "register",
                name,
                "--project",
                project,
                "--role",
                "assistant",
                "--create-area",
                "--by",
                "owner",
            ])
            .assert()
            .success();
    }

    fn projects(&self) -> Vec<Value> {
        let out = self
            .fl(self.work.path())
            .args(["project", "list", "--json"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        serde_json::from_slice(&out).unwrap()
    }

    fn slugs(&self) -> Vec<String> {
        self.projects()
            .iter()
            .map(|p| p["slug"].as_str().unwrap().to_string())
            .collect()
    }

    fn agent_status(&self, name: &str) -> String {
        let out = self
            .fl(self.work.path())
            .args(["agent", "roster", "--include-retired", "--json"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let all: Vec<Value> = serde_json::from_slice(&out).unwrap();
        let agent = all.iter().find(|a| a["name_key"] == name).unwrap();
        agent["status"].as_str().unwrap().to_string()
    }
}

#[test]
fn delete_unregisters_by_default_and_prints_the_hubs_body() {
    let env = Env::new();
    let root = env.dir("doomed");
    let created = env.run_json("create", &["Doomed", "--path", root.to_str().unwrap()]);
    let store = PathBuf::from(created["store_path"].as_str().unwrap());
    env.run_json(
        "create",
        &["Kept", "--path", env.dir("kept").to_str().unwrap()],
    );
    assert!(store.is_file());

    let out = env
        .fl(env.work.path())
        .args(["project", "delete", "doomed", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let body: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(
        body,
        serde_json::json!({
            "deleted": "doomed",
            "reassigned_agents": 0,
            "decommissioned_agents": 0,
            "store_deleted": false,
            "dir_deleted": false,
        })
    );
    assert!(!env.config().contains("doomed") && env.config().contains("kept"));
    assert!(root.join(".fl/mcp.toml").is_file(), "no file is deleted");
    assert!(store.is_file());
    // The store file is still in the data directory, so the slug is still listed (with no path),
    // as the Hub lists it; `--delete-store` removes it.
    assert_eq!(env.slugs(), ["doomed", "kept"].map(String::from));
    env.fl(env.work.path())
        .args(["project", "delete", "doomed", "--delete-store"])
        .assert()
        .success()
        .stdout(contains("store_deleted=true"));
    assert!(!store.exists());
    env.fl(env.work.path())
        .args(["project", "delete", "doomed"])
        .assert()
        .failure()
        .stderr(contains("no project `doomed`"));
}

#[test]
fn delete_store_and_dir_remove_exactly_those() {
    let env = Env::new();
    let root = env.dir("scrap");
    let created = env.run_json("create", &["Scrap", "--path", root.to_str().unwrap()]);
    let store = PathBuf::from(created["store_path"].as_str().unwrap());
    let other = env.dir("other");
    env.run_json("create", &["Other", "--path", other.to_str().unwrap()]);

    env.fl(env.work.path())
        .args([
            "project",
            "delete",
            "scrap",
            "--delete-store",
            "--delete-dir",
        ])
        .assert()
        .success()
        .stdout(contains("store_deleted=true").and(contains("dir_deleted=true")));
    assert!(!store.exists() && !root.exists());
    assert!(other.is_dir());
    assert_eq!(env.slugs(), ["other"]);
}

#[test]
fn delete_refuses_a_project_with_active_agents_until_they_are_reassigned_or_retired() {
    let env = Env::new();
    for name in ["busy", "target", "third"] {
        env.run_json("create", &[name, "--path", env.dir(name).to_str().unwrap()]);
    }
    env.register_agent("ada", "busy");
    env.register_agent("bo", "busy");
    let config = env.config();

    let out = env
        .fl(env.work.path())
        .args(["project", "delete", "busy", "--delete-dir", "--json"])
        .assert()
        .code(2)
        .stderr(contains("active agents"))
        .get_output()
        .stdout
        .clone();
    let body: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(body["error"], "project has active agents");
    assert_eq!(body["active_agent_count"], 2);
    assert_eq!(env.config(), config);
    assert!(env.dir("busy").is_dir());

    // The two ways out exclude each other.
    env.fl(env.work.path())
        .args([
            "project",
            "delete",
            "busy",
            "--reassign-to",
            "target",
            "--decommission-agents",
        ])
        .assert()
        .failure();
    env.fl(env.work.path())
        .args(["project", "delete", "busy", "--reassign-to", "nosuch"])
        .assert()
        .failure()
        .stderr(contains("no project `nosuch`"));
    assert!(env.slugs().contains(&"busy".to_string()));

    let moved = env
        .fl(env.work.path())
        .args([
            "project",
            "delete",
            "busy",
            "--reassign-to",
            "target",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let moved: Value = serde_json::from_slice(&moved).unwrap();
    assert_eq!(moved["reassigned_agents"], 2);
    let target = env
        .projects()
        .into_iter()
        .find(|p| p["slug"] == "target")
        .unwrap();
    assert_eq!(target["active_agent_count"], 2);

    env.register_agent("cy", "third");
    let retired = env
        .fl(env.work.path())
        .args([
            "project",
            "delete",
            "third",
            "--decommission-agents",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let retired: Value = serde_json::from_slice(&retired).unwrap();
    assert_eq!(retired["decommissioned_agents"], 1);
    assert_eq!(env.agent_status("cy"), "retired");
    assert_eq!(env.agent_status("ada"), "active");
}

#[test]
fn delete_removes_a_virtual_group_and_refuses_files_for_it() {
    let env = Env::new();
    env.run_json("create", &["Council", "--virtual"]);
    env.fl(env.work.path())
        .args(["project", "delete", "council", "--delete-dir"])
        .assert()
        .failure()
        .stderr(contains("virtual group"));
    assert_eq!(env.slugs(), ["council"]);
    env.fl(env.work.path())
        .args(["project", "delete", "council"])
        .assert()
        .success();
    assert!(env.slugs().is_empty());
}

#[test]
fn delete_dir_refuses_the_home_directory_and_changes_nothing() {
    let env = Env::new();
    // The project is the directory `fl` sees as $HOME.
    let home = env.dir("me");
    env.run_json("create", &["Me", "--path", home.to_str().unwrap()]);
    std::fs::write(home.join("precious"), "x").unwrap();
    let config = env.config();
    env.fl(env.work.path())
        .env("HOME", &home)
        .args(["project", "delete", "me", "--delete-dir"])
        .assert()
        .failure()
        .stderr(contains("refusing to delete").and(contains("home directory")));
    assert_eq!(env.config(), config);
    assert!(home.join("precious").is_file());
    assert_eq!(env.slugs(), ["me"]);
}

#[test]
fn delete_dir_refuses_a_directory_that_is_itself_a_workspace_root() {
    let env = Env::new();
    let root = env.dir("ws");
    env.run_json("create", &["Ws", "--path", root.to_str().unwrap()]);
    std::fs::create_dir_all(root.join(".agents/memory")).unwrap();
    // Run from outside it: the directory's own markers decide, not where the caller stands.
    env.fl(env.work.path())
        .args(["project", "delete", "ws", "--delete-dir"])
        .assert()
        .failure()
        .stderr(contains("workspace root"));
    assert!(root.join(".fl/mcp.toml").is_file());
    assert_eq!(env.slugs(), ["ws"]);
}

#[test]
fn delete_dir_refuses_an_ancestor_of_the_workspace() {
    let env = Env::new();
    let outer = env.dir("outer");
    env.run_json("create", &["Outer", "--path", outer.to_str().unwrap()]);
    std::fs::create_dir_all(outer.join("inner/Notes")).unwrap();
    env.fl(&outer.join("inner"))
        .args(["project", "delete", "outer", "--delete-dir"])
        .assert()
        .failure()
        .stderr(contains("workspace root"));
    assert!(outer.join("inner/Notes").is_dir());
    assert_eq!(env.slugs(), ["outer"]);
}

#[test]
fn delete_dir_fails_closed_when_no_workspace_root_can_be_found() {
    let env = Env::new();
    // No marker at the working directory, at the home directory or above the project.
    std::fs::remove_dir(env.home.path().join("Notes")).unwrap();
    let root = env.dir("anywhere");
    env.run_json("create", &["Anywhere", "--path", root.to_str().unwrap()]);
    // The executable is looked at too, so it runs from a link or copy outside any workspace
    // (the checkout this test runs from may be inside one). A hard link opens no write
    // descriptor, so a sibling test's fork cannot hold the file open for writing (ETXTBSY).
    let bin_dir = tempfile::tempdir().unwrap();
    let copy = bin_dir.path().join("fl");
    let linked = std::fs::hard_link(assert_cmd::cargo::cargo_bin("fl"), &copy).is_ok();
    if !linked {
        std::fs::copy(assert_cmd::cargo::cargo_bin("fl"), &copy).unwrap();
    }
    let mut cmd = Command::new(&copy);
    cmd.env("HOME", env.home.path())
        .env("XDG_CONFIG_HOME", env.home.path().join("config"))
        .env("XDG_DATA_HOME", env.home.path().join("data"))
        .env_remove("FL_DB")
        .env_remove("FL_STORE_BACKEND")
        .env("FL_TEST_SPINUP_NO_SPAWN", "1")
        .current_dir(env.work.path());
    cmd.args(["project", "delete", "anywhere", "--delete-dir"]);
    // A copy (the fallback when the link crosses filesystems) was written by this process, and a
    // sibling test's fork can hold its write descriptor for an instant: exec then fails with
    // ETXTBSY (os error 26, "Text file busy"). That says nothing about the command, so retry.
    let mut out = cmd.output();
    for _ in 0..100 {
        if linked || !matches!(&out, Err(e) if e.raw_os_error() == Some(26)) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
        out = cmd.output();
    }
    out.unwrap()
        .assert()
        .failure()
        .stderr(contains("no workspace root could be found"));
    assert!(root.join(".fl/mcp.toml").is_file());
    assert_eq!(env.slugs(), ["anywhere"]);
}

#[test]
fn a_config_whose_projects_are_not_tables_stops_every_project_command_and_names_the_file() {
    let env = Env::new();
    let root = env.dir("alpha");
    env.run_json("create", &["Alpha", "--path", root.to_str().unwrap()]);
    std::fs::write(env.config_path(), "project = [1, 2]\n").unwrap();
    let named = env.config_path().display().to_string();
    for args in [
        vec!["project", "list"],
        vec![
            "project",
            "create",
            "Beta",
            "--path",
            env.dir("beta").to_str().unwrap(),
        ],
        vec!["project", "delete", "alpha"],
    ] {
        env.fl(env.work.path())
            .args(&args)
            .assert()
            .failure()
            .stderr(contains(named.as_str()).and(contains("project")));
    }
    assert_eq!(env.config(), "project = [1, 2]\n");
    assert!(!env.dir("beta").exists());
}

#[test]
fn nested_projects_keep_flat_slugs_and_explicit_names() {
    let env = Env::new();
    let root = env.dir("pokemon");
    env.fl(env.work.path())
        .args(["project", "create", "--name", "Kaggle/Pokémon", "--path"])
        .arg(&root)
        .assert()
        .failure()
        .stderr(contains("Kaggle").and(contains("--create-parent")));
    assert!(!root.exists());
    let parent = env.run_json("create", &["--virtual", "--name", "Kaggle"]);
    assert_eq!(parent["slug"], "kaggle");
    let args = [
        "--name",
        " Kaggle / Pokémon ",
        "--path",
        root.to_str().unwrap(),
    ];
    for _ in 0..2 {
        let child = env.run_json("create", &args);
        assert_eq!(child["name"], "Kaggle/Pokémon");
        assert_eq!(child["slug"], "kaggle-pokemon");
        assert_eq!(child["parent"], "kaggle");
        assert_eq!(child["ancestors"], serde_json::json!(["kaggle"]));
    }
    env.fl(env.work.path())
        .args(["project", "list"])
        .assert()
        .success()
        .stdout(contains("Kaggle/Pokémon"));
}

#[test]
fn resume_flat_uses_the_existing_project_store_without_creating_ancestors() {
    for (name, slug, extension) in [
        ("Kaggle/Pokémon", "kaggle-pok-mon", "fls"),
        ("Archive//Old", "archive-old", "redb"),
    ] {
        let env = Env::new();
        let root = env.dir("historical");
        std::fs::create_dir(&root).unwrap();
        let ws = env.home.path().join("data/fl/fl.fls");
        let store = ws.with_file_name(format!("{slug}.{extension}"));
        let mut seed = env.fl(env.work.path());
        if extension == "redb" {
            seed.env("FL_STORE_BACKEND", "redb");
        }
        seed.arg("--db")
            .arg(&store)
            .args(["project", "add"])
            .arg(&root)
            .assert()
            .success();
        let args = [
            "--resume-flat",
            "--name",
            name,
            "--slug",
            slug,
            "--path",
            root.to_str().unwrap(),
        ];
        for expected_registered in [true, false] {
            let resumed = env.run_json("create", &args);
            assert_eq!(resumed["slug"], slug);
            assert_eq!(resumed["name"], name);
            assert_eq!(resumed["parent"], Value::Null);
            assert_eq!(resumed["ancestors"], serde_json::json!([]));
            assert_eq!(resumed["registered"], expected_registered);
        }
        assert_eq!(env.slugs(), [slug]);
        assert!(root.join(".fl/mcp.toml").is_file());
        assert_ne!(
            fl_store::FerroStore::open(&ws)
                .unwrap()
                .format_version()
                .unwrap(),
            Some(10)
        );
    }
}

#[test]
fn resume_flat_refuses_missing_empty_and_wrong_root_stores_before_project_changes() {
    for fixture in [
        "missing",
        "zero-byte",
        "empty",
        "wrong-root",
        "wrong-root-redb",
    ] {
        let env = Env::new();
        let root = env.dir("requested");
        let ferro = env.home.path().join("data/fl/historical.fls");
        let store = if fixture == "wrong-root-redb" {
            ferro.with_extension("redb")
        } else {
            ferro.clone()
        };
        match fixture {
            "zero-byte" => {
                std::fs::create_dir_all(store.parent().unwrap()).unwrap();
                std::fs::File::create(&store).unwrap();
            }
            "empty" => drop(fl_store::open_store(&store).unwrap()),
            "wrong-root" | "wrong-root-redb" => {
                let other = env.dir("other");
                std::fs::create_dir(&other).unwrap();
                let mut seed = env.fl(env.work.path());
                if fixture == "wrong-root-redb" {
                    seed.env("FL_STORE_BACKEND", "redb");
                }
                seed.arg("--db")
                    .arg(&store)
                    .args(["project", "add"])
                    .arg(&other)
                    .assert()
                    .success();
            }
            _ => {}
        }
        let before = std::fs::read(&store).ok();
        env.fl(env.work.path())
            .args([
                "project",
                "create",
                "--resume-flat",
                "--name",
                "Archive/Old",
                "--slug",
                "historical",
                "--git-init",
                "--path",
            ])
            .arg(&root)
            .assert()
            .failure()
            .stderr(contains(if fixture == "missing" {
                "existing project store"
            } else {
                "does not hold exactly the project at"
            }));
        assert!(!root.exists());
        assert!(!env.config_path().exists());
        assert!(!env.home.path().join("data/fl/fl.fls").exists());
        assert_eq!(std::fs::read(&store).ok(), before);
        if fixture == "wrong-root-redb" {
            assert!(!ferro.exists());
        }
    }
}

#[test]
fn resume_flat_leaves_a_partially_initialized_ferro_store_untouched() {
    use std::time::{Duration, SystemTime};

    let env = Env::new();
    let root = env.dir("requested");
    let store = env.home.path().join("data/fl/historical.fls");
    let dir = store.parent().unwrap();
    std::fs::create_dir_all(dir).unwrap();
    let partial_header = [0u8; 64];
    std::fs::write(&store, partial_header).unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&store)
        .unwrap()
        .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000))
        .unwrap();
    let modified = std::fs::metadata(&store).unwrap().modified().unwrap();

    env.fl(env.work.path())
        .args([
            "project",
            "create",
            "--resume-flat",
            "--name",
            "Archive/Old",
            "--slug",
            "historical",
            "--git-init",
            "--path",
        ])
        .arg(&root)
        .assert()
        .failure();
    assert_eq!(
        std::fs::metadata(&store).unwrap().len(),
        partial_header.len() as u64,
        "flat recovery initialized the incomplete store"
    );
    assert_eq!(std::fs::read(&store).unwrap(), partial_header);
    assert_eq!(
        std::fs::metadata(&store).unwrap().modified().unwrap(),
        modified
    );
    let files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(files, [store]);
    assert!(!root.exists());
    assert!(!env.config_path().exists());
}

#[test]
fn resume_flat_refuses_a_wrong_root_with_forced_redb() {
    assert_resume_redb_refusal(true, false);
}

#[test]
fn resume_flat_does_not_repair_a_lone_redb_before_checking_its_owner() {
    assert_resume_redb_refusal(false, true);
}

#[test]
fn resume_flat_does_not_repair_a_forced_redb_before_checking_its_owner() {
    assert_resume_redb_refusal(true, true);
}

#[test]
fn resume_flat_honors_the_restore_fence_for_a_lone_redb() {
    use fl_core::store::Catalog;

    let env = Env::new();
    let root = env.dir("requested");
    let store = env.home.path().join("data/fl/historical.redb");
    std::fs::create_dir_all(store.parent().unwrap()).unwrap();
    let redb = fl_store::RedbStore::open(&store).unwrap();
    redb.add_project(root.to_str().unwrap()).unwrap();
    drop(redb);
    // The default backend's restore fence lives beside the destination .fls.
    std::fs::write(store.with_extension("fls.restore-guard"), "fenced").unwrap();
    env.fl(env.work.path())
        .args([
            "project",
            "create",
            "--resume-flat",
            "--name",
            "Archive/Old",
            "--slug",
            "historical",
            "--git-init",
            "--path",
        ])
        .arg(&root)
        .assert()
        .failure()
        .stderr(contains("restore admission fenced"));
    assert!(!root.exists());
    assert!(!env.config_path().exists());
    assert!(!store.with_file_name("fl.fls").exists());
    assert!(!store.with_extension("fls").exists());
}

fn assert_resume_redb_refusal(force_redb: bool, needs_repair: bool) {
    use fl_core::store::Catalog;
    use std::time::{Duration, SystemTime};

    let env = Env::new();
    let root = env.dir("requested");
    let other = env.dir("other");
    std::fs::create_dir(&other).unwrap();
    let store = env.home.path().join("data/fl/historical.redb");
    env.fl(env.work.path())
        .env("FL_STORE_BACKEND", "redb")
        .arg("--db")
        .arg(&store)
        .args(["project", "add"])
        .arg(&other)
        .assert()
        .success();
    if needs_repair {
        // Redb's header puts the recovery-required bit in the byte after its 9-byte magic.
        let mut dirty = std::fs::read(&store).unwrap();
        assert!(dirty.starts_with(b"redb\x1a\x0a\xa9\x0d\x0a"));
        assert_eq!(dirty[9] & 2, 0);
        dirty[9] |= 2;
        std::fs::write(&store, dirty).unwrap();
        assert!(
            fl_store::RedbStore::open_read_only(&store, 0)
                .err()
                .expect("dirty fixture must reject a read-only open")
                .to_string()
                .contains("repair")
        );
        // A disposable copy proves the ordinary writable open can repair these exact bytes.
        let copy = env.dir("repair-witness.redb");
        std::fs::copy(&store, &copy).unwrap();
        let repaired = fl_store::RedbStore::open_with_retries(&copy, 0).unwrap();
        let projects = repaired.list_projects().unwrap();
        assert_eq!(projects.len(), 1);
        assert_eq!(Path::new(&projects[0].root), other);
    }
    // Avoid relying on the filesystem's timestamp resolution to observe a write.
    std::fs::OpenOptions::new()
        .write(true)
        .open(&store)
        .unwrap()
        .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000))
        .unwrap();
    let store_files = || {
        let mut paths: Vec<_> = std::fs::read_dir(store.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        paths.sort();
        paths
    };
    let before_paths = store_files();
    let before_files: Vec<_> = before_paths
        .iter()
        .map(|path| {
            (
                std::fs::metadata(path).unwrap(),
                std::fs::read(path).unwrap(),
            )
        })
        .collect();

    let mut resume = env.fl(env.work.path());
    if force_redb {
        resume.env("FL_STORE_BACKEND", "redb");
    }
    let result = resume
        .args([
            "project",
            "create",
            "--resume-flat",
            "--name",
            "Archive/Old",
            "--slug",
            "historical",
            "--git-init",
            "--path",
        ])
        .arg(&root)
        .assert()
        .failure();
    assert!(!root.exists(), "checkout, Git and catalog must stay absent");
    assert!(!env.config_path().exists());
    assert_eq!(
        store_files(),
        before_paths,
        "store files or sidecars changed"
    );
    for (path, (before, bytes)) in before_paths.iter().zip(before_files) {
        let after = std::fs::metadata(path).unwrap();
        assert!(
            std::fs::read(path).unwrap() == bytes,
            "store bytes changed at {}. {result}",
            path.display()
        );
        assert_eq!(after.modified().unwrap(), before.modified().unwrap());
        assert_eq!(after.permissions(), before.permissions());
    }
    result.stderr(contains(if needs_repair {
        "repair"
    } else {
        "does not hold exactly the project at"
    }));
}

#[test]
fn resume_flat_does_not_flatten_an_existing_linked_project() {
    let env = Env::new();
    let root = env.dir("linked");
    env.run_json(
        "create",
        &[
            "Top/Child",
            "--path",
            root.to_str().unwrap(),
            "--create-parent",
        ],
    );
    env.fl(env.work.path())
        .args([
            "project",
            "create",
            "--resume-flat",
            "--name",
            "Top/Child",
            "--slug",
            "top-child",
            "--path",
        ])
        .arg(&root)
        .assert()
        .failure()
        .stderr(contains("linked project"));
    assert_eq!(
        env.projects()
            .iter()
            .find(|p| p["slug"] == "top-child")
            .unwrap()["parent"],
        "top"
    );
}

#[test]
fn nested_area_set_updates_purpose_without_repointing_the_area() {
    let env = Env::new();
    let root = env.dir("child");
    env.run_json(
        "create",
        &[
            "Portfolio/Child",
            "--path",
            root.to_str().unwrap(),
            "--create-parent",
        ],
    );
    env.fl(env.work.path())
        .args([
            "agent",
            "area",
            "set",
            "portfolio-child",
            "--purpose",
            "updated",
            "--by",
            "tester",
        ])
        .assert()
        .success();
    let listed = env.run_json("list", &[]);
    let child = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["slug"] == "portfolio-child")
        .unwrap();
    assert_eq!(child["name"], "Portfolio/Child");
    assert_eq!(child["parent"], "portfolio");
    assert_eq!(child["purpose"], "updated");
    env.fl(env.work.path())
        .args([
            "agent",
            "area",
            "set",
            "portfolio-child",
            "--path",
            "/elsewhere",
            "--purpose",
            "bad",
            "--by",
            "tester",
        ])
        .assert()
        .failure();
}

#[test]
fn nested_cli_honors_the_config_lock() {
    let env = Env::new();
    let config_dir = env.config_path();
    let config_dir = config_dir.parent().unwrap();
    std::fs::create_dir_all(config_dir).unwrap();
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let held = options
        .open(config_dir.join("config.toml.projects.lock"))
        .unwrap();
    held.try_lock().unwrap();
    let root = env.dir("busy");
    env.fl(env.work.path())
        .args(["project", "init"])
        .arg(&root)
        .assert()
        .failure()
        .stderr(contains("busy"));
    assert!(!root.exists());
    drop(held);
    env.fl(env.work.path())
        .args(["project", "init"])
        .arg(&root)
        .assert()
        .success();
}
