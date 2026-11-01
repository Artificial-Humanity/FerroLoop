//! `fl store status` and `fl store checkpoint`, black box, on both engines.
//!
//! Every run gets private HOME, XDG and `$FL_DB` so nothing reads the
//! developer's own stores, and none of these commands may create a store.

use assert_cmd::Command;
use fl_store::{BackendKind, StoreOptions};
use predicates::str::contains;
use serde_json::Value;
use std::path::{Path, PathBuf};

struct Env {
    home: tempfile::TempDir,
    stores: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        Self {
            home: tempfile::tempdir().unwrap(),
            stores: tempfile::tempdir().unwrap(),
        }
    }

    /// `fl` with no engine selected in the environment.
    fn fl(&self) -> Command {
        let mut c = Command::cargo_bin("fl").unwrap();
        c.env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.home.path().join("config"))
            .env("XDG_DATA_HOME", self.home.path().join("data"))
            .env_remove("FL_DB")
            .env_remove("FL_STORE_BACKEND")
            .current_dir(self.home.path());
        c
    }

    fn ferro(&self) -> Command {
        let mut c = self.fl();
        c.env("FL_STORE_BACKEND", "ferro");
        c
    }

    fn logical(&self) -> PathBuf {
        self.stores.path().join("fl.redb")
    }

    fn physical_ferro(&self) -> PathBuf {
        self.stores.path().join("fl.fls")
    }

    fn files(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.stores.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
}

fn open_with(path: &Path, backend: BackendKind) {
    drop(
        fl_store::open_store_with(
            path,
            &StoreOptions {
                backend: Some(backend),
                ..StoreOptions::default()
            },
        )
        .unwrap(),
    );
}

fn status_json(mut cmd: Command, db: &Path) -> Value {
    let out = cmd
        .args(["--db"])
        .arg(db)
        .args(["store", "status", "--json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn status_on_redb_reports_a_store_that_does_not_exist_and_creates_nothing() {
    let env = Env::new();
    let v = status_json(env.fl(), &env.logical());
    assert_eq!(v["engine"], "redb");
    assert_eq!(v["exists"], false);
    assert_eq!(v["size_bytes"], Value::Null);
    assert_eq!(v["format_version"], Value::Null);
    assert_eq!(v["logical_path"], env.logical().display().to_string());
    assert_eq!(v["physical_path"], env.logical().display().to_string());
    assert!(env.files().is_empty(), "{:?}", env.files());
}

#[test]
fn status_on_redb_reports_size_and_format_version() {
    let env = Env::new();
    open_with(&env.logical(), BackendKind::Redb);
    let v = status_json(env.fl(), &env.logical());
    assert_eq!(v["engine"], "redb");
    assert_eq!(v["exists"], true);
    assert_eq!(
        v["size_bytes"].as_u64().unwrap(),
        std::fs::metadata(env.logical()).unwrap().len()
    );
    assert!(v["format_version"].is_u64(), "{v}");
}

#[test]
fn status_in_text_names_each_field() {
    let env = Env::new();
    open_with(&env.logical(), BackendKind::Redb);
    env.fl()
        .arg("--db")
        .arg(env.logical())
        .args(["store", "status"])
        .assert()
        .success()
        .stdout(contains("engine:         redb"))
        .stdout(contains("exists:         yes"))
        .stdout(contains("format version: "));
}

#[test]
fn status_on_ferro_maps_the_logical_path_and_says_the_format_version_is_not_available() {
    let env = Env::new();
    let v = status_json(env.ferro(), &env.logical());
    assert_eq!(v["engine"], "ferro");
    assert_eq!(v["exists"], false);
    assert_eq!(v["logical_path"], env.logical().display().to_string());
    assert_eq!(
        v["physical_path"],
        env.physical_ferro().display().to_string()
    );
    assert_eq!(v["format_version"], "n/a");
    assert!(env.files().is_empty(), "{:?}", env.files());

    open_with(&env.logical(), BackendKind::Ferro);
    let v = status_json(env.ferro(), &env.logical());
    assert_eq!(v["exists"], true);
    assert_eq!(
        v["size_bytes"].as_u64().unwrap(),
        std::fs::metadata(env.physical_ferro()).unwrap().len()
    );
    assert_eq!(v["format_version"], "n/a");
}

#[test]
fn status_on_an_fls_path_is_ferro_without_the_environment() {
    let env = Env::new();
    let v = status_json(env.fl(), &env.physical_ferro());
    assert_eq!(v["engine"], "ferro");
    assert_eq!(
        v["physical_path"],
        env.physical_ferro().display().to_string()
    );
}

#[test]
fn status_flags_a_redb_beside_a_ferro_selection() {
    let env = Env::new();
    open_with(&env.logical(), BackendKind::Redb);
    let v = status_json(env.ferro(), &env.logical());
    assert_eq!(v["exists"], false);
    assert_eq!(v["redb_beside_ferro"], true);
    assert_eq!(env.files(), vec!["fl.redb".to_string()]);
}

#[test]
fn status_refuses_an_unknown_engine() {
    let env = Env::new();
    let mut c = env.fl();
    c.env("FL_STORE_BACKEND", "sqlite")
        .arg("--db")
        .arg(env.logical())
        .args(["store", "status"])
        .assert()
        .failure()
        .stderr(contains("unknown store backend"));
}

#[test]
fn checkpoint_on_redb_does_nothing_and_succeeds() {
    let env = Env::new();
    env.fl()
        .arg("--db")
        .arg(env.logical())
        .args(["store", "checkpoint"])
        .assert()
        .success()
        .stdout(contains("redb has no checkpoint; nothing to do"));
    assert!(env.files().is_empty(), "{:?}", env.files());
}

#[test]
fn checkpoint_on_ferro_checkpoints_an_existing_store() {
    let env = Env::new();
    open_with(&env.logical(), BackendKind::Ferro);
    env.ferro()
        .arg("--db")
        .arg(env.logical())
        .args(["store", "checkpoint"])
        .assert()
        .success()
        .stdout(contains("checkpointed"))
        .stdout(contains("fl.fls"));
}

#[test]
fn checkpoint_on_ferro_does_not_create_a_missing_store() {
    let env = Env::new();
    env.ferro()
        .arg("--db")
        .arg(env.logical())
        .args(["store", "checkpoint"])
        .assert()
        .failure()
        .stderr(contains("no store at"));
    assert!(env.files().is_empty(), "{:?}", env.files());
}

// ---- ordinary commands with ferro selected, and the missing-directory rule ----

const STRANGER: &str = "urn:uuid:0190a1b2-c3d4-7e5f-8a6b-7c8d9e0f1a2b";

/// One entry of a [`snapshot`]: path, is-a-directory, size, mtime, file bytes.
type Entry = (PathBuf, bool, u64, Option<std::time::SystemTime>, Vec<u8>);

/// Every entry under `roots` with its kind, size, mtime and, for a file, its
/// bytes: a tree that compares equal before and after was not touched.
fn snapshot(roots: &[&Path]) -> Vec<Entry> {
    fn walk(dir: &Path, out: &mut Vec<Entry>) {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        entries.sort();
        for p in entries {
            let meta = std::fs::symlink_metadata(&p).unwrap();
            let is_dir = meta.is_dir();
            let bytes = if meta.is_file() {
                std::fs::read(&p).unwrap()
            } else {
                Vec::new()
            };
            out.push((p.clone(), is_dir, meta.len(), meta.modified().ok(), bytes));
            if is_dir {
                walk(&p, out);
            }
        }
    }
    let mut out = Vec::new();
    for r in roots {
        walk(r, &mut out);
    }
    out
}

/// A config that binds a project directory to `store`; returns the project.
fn bind_project(env: &Env, store: &Path) -> tempfile::TempDir {
    let proj = tempfile::tempdir().unwrap();
    let dir = env.home.path().join("config").join("fl");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("config.toml"),
        format!(
            "[[project]]\nroot = \"{}\"\nstore = \"{}\"\n",
            proj.path().display(),
            store.display()
        ),
    )
    .unwrap();
    proj
}

#[test]
fn ferro_with_neither_file_present_an_iri_lookup_is_not_owned_and_creates_nothing() {
    let env = Env::new();
    let proj = bind_project(&env, &env.logical());
    let before = snapshot(&[env.home.path(), env.stores.path(), proj.path()]);
    env.ferro()
        .current_dir(proj.path())
        .args(["gate", "show", STRANGER])
        .assert()
        .code(2)
        .stderr(contains("no store"));
    assert_eq!(
        before,
        snapshot(&[env.home.path(), env.stores.path(), proj.path()]),
        "the lookup touched the tree"
    );
    assert!(env.files().is_empty(), "{:?}", env.files());
}

#[test]
fn ferro_with_an_explicit_db_an_iri_lookup_is_not_owned_and_creates_nothing() {
    let env = Env::new();
    let before = snapshot(&[env.home.path(), env.stores.path()]);
    env.ferro()
        .arg("--db")
        .arg(env.logical())
        .args(["gate", "show", STRANGER])
        .assert()
        .code(2)
        .stderr(contains("no store holds"));
    assert_eq!(
        before,
        snapshot(&[env.home.path(), env.stores.path()]),
        "the lookup touched the tree"
    );
}

#[test]
fn ferro_leaves_a_stray_redb_unopened_and_says_to_migrate() {
    let env = Env::new();
    open_with(&env.logical(), BackendKind::Redb);
    let proj = bind_project(&env, &env.logical());
    let bytes = std::fs::read(env.logical()).unwrap();
    let mtime = std::fs::metadata(env.logical())
        .unwrap()
        .modified()
        .unwrap();
    let before = snapshot(&[env.home.path(), env.stores.path(), proj.path()]);

    // A command that needs the bound store is refused with the remedy.
    env.ferro()
        .current_dir(proj.path())
        .args(["project", "list"])
        .assert()
        .failure()
        .stderr(contains("migrate"));
    // An IRI lookup opens nothing and is not owned.
    env.ferro()
        .current_dir(proj.path())
        .args(["gate", "show", STRANGER])
        .assert()
        .code(2);

    assert_eq!(std::fs::read(env.logical()).unwrap(), bytes);
    assert_eq!(
        std::fs::metadata(env.logical())
            .unwrap()
            .modified()
            .unwrap(),
        mtime
    );
    assert_eq!(
        before,
        snapshot(&[env.home.path(), env.stores.path(), proj.path()]),
        "the stray redb or the tree was touched"
    );
    assert_eq!(env.files(), vec!["fl.redb".to_string()]);
}

#[test]
fn status_and_checkpoint_do_not_create_a_missing_directory_on_either_engine() {
    for ferro in [false, true] {
        for sub in ["status", "checkpoint"] {
            let env = Env::new();
            let missing = env.stores.path().join("not-yet");
            let db = missing.join("fl.redb");
            let mut cmd = if ferro { env.ferro() } else { env.fl() };
            cmd.arg("--db").arg(&db).args(["store", sub]);
            let out = cmd.output().unwrap();
            // status succeeds; checkpoint succeeds on redb and is refused on ferro.
            let expect_ok = !(ferro && sub == "checkpoint");
            assert_eq!(
                out.status.success(),
                expect_ok,
                "ferro={ferro} {sub}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(
                !missing.exists(),
                "ferro={ferro} {sub} created the directory"
            );
            assert!(env.files().is_empty(), "{:?}", env.files());
        }
    }
}

#[test]
fn status_on_an_empty_redb_file_reports_it_and_does_not_modify_it() {
    let env = Env::new();
    std::fs::write(env.logical(), b"").unwrap();
    let mtime = std::fs::metadata(env.logical())
        .unwrap()
        .modified()
        .unwrap();
    let v = status_json(env.fl(), &env.logical());
    assert_eq!(v["exists"], true);
    assert_eq!(v["size_bytes"], 0);
    assert_eq!(v["format_version"], Value::Null);
    let meta = std::fs::metadata(env.logical()).unwrap();
    assert_eq!(meta.len(), 0, "status initialised the file");
    assert_eq!(meta.modified().unwrap(), mtime);

    env.fl()
        .arg("--db")
        .arg(env.logical())
        .args(["store", "status"])
        .assert()
        .success()
        .stdout(contains("empty file"));
    assert_eq!(std::fs::metadata(env.logical()).unwrap().len(), 0);
}

#[test]
fn ferro_treats_a_redb_and_an_fls_alias_of_one_store_as_one_candidate() {
    let env = Env::new();
    let repo = tempfile::tempdir().unwrap();
    for args in [
        &["init", "-q"][..],
        &["config", "user.email", "t@example.com"],
        &["config", "user.name", "t"],
        &["commit", "-q", "--allow-empty", "-m", "first"],
    ] {
        assert!(
            std::process::Command::new("git")
                .args(args)
                .current_dir(repo.path())
                .status()
                .unwrap()
                .success()
        );
    }
    let other = tempfile::tempdir().unwrap();
    let dir = env.home.path().join("config").join("fl");
    std::fs::create_dir_all(&dir).unwrap();
    // Two config entries, one store: `fl.redb` is `fl.fls` under ferro.
    std::fs::write(
        dir.join("config.toml"),
        format!(
            "[[project]]\nroot = \"{}\"\nstore = \"{}\"\n\n[[project]]\nroot = \"{}\"\nstore = \"{}\"\n",
            repo.path().display(),
            env.logical().display(),
            other.path().display(),
            env.physical_ferro().display(),
        ),
    )
    .unwrap();
    env.ferro()
        .current_dir(repo.path())
        .args(["project", "add", repo.path().to_str().unwrap()])
        .assert()
        .success();
    env.ferro()
        .current_dir(repo.path())
        .args([
            "gate",
            "add",
            "--project",
            "1",
            "--name",
            "g",
            "--glob",
            "*.rs",
            "--program",
            "true",
        ])
        .assert()
        .success();
    let shown = env
        .ferro()
        .current_dir(repo.path())
        .args(["gate", "show", "1"])
        .output()
        .unwrap();
    let iri = serde_json::from_slice::<Value>(&shown.stdout).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    env.ferro()
        .current_dir(repo.path())
        .args(["gate", "show", &iri])
        .assert()
        .success();
}
