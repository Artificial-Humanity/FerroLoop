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
