//! `fl store serve`, `get`, `put`, `query`, `inspect` and `bench`: smoke tests
//! of the commands `fl store` shares with the `ferrostore` binary (whose own
//! tests in `crates/store/tests/ferrostore_bin.rs` cover them in depth), on
//! the bound store.
//!
//! Every run gets a private HOME, XDG_CONFIG_HOME, XDG_DATA_HOME,
//! XDG_RUNTIME_DIR and TMPDIR inside the sandbox, with FL_DB and
//! FL_STORE_BACKEND removed from the environment it inherits.

use assert_cmd::Command;
use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};

struct Sandbox {
    root: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        for sub in ["home", "config", "data", "run", "tmp", "stores"] {
            std::fs::create_dir(root.path().join(sub)).unwrap();
        }
        Self { root }
    }

    fn dir(&self, sub: &str) -> PathBuf {
        self.root.path().join(sub)
    }

    fn fls(&self) -> PathBuf {
        self.dir("stores").join("fl.fls")
    }

    /// `fl store <args>`, no `--db`: the bound store is the default one.
    fn fl_unbound(&self, args: &[&str]) -> Command {
        let mut c = Command::cargo_bin("fl").unwrap();
        c.env("HOME", self.dir("home"))
            .env("XDG_CONFIG_HOME", self.dir("config"))
            .env("XDG_DATA_HOME", self.dir("data"))
            .env("XDG_RUNTIME_DIR", self.dir("run"))
            .env("TMPDIR", self.dir("tmp"))
            .env_remove("FL_DB")
            .env_remove("FL_STORE_BACKEND")
            .current_dir(self.dir("home"))
            .arg("store")
            .args(args);
        c
    }

    /// `fl --db <stores/fl.redb> store <args>`.
    fn fl(&self, args: &[&str]) -> Command {
        let mut c = Command::cargo_bin("fl").unwrap();
        c.env("HOME", self.dir("home"))
            .env("XDG_CONFIG_HOME", self.dir("config"))
            .env("XDG_DATA_HOME", self.dir("data"))
            .env("XDG_RUNTIME_DIR", self.dir("run"))
            .env("TMPDIR", self.dir("tmp"))
            .env_remove("FL_DB")
            .env_remove("FL_STORE_BACKEND")
            .current_dir(self.dir("home"))
            .arg("--db")
            .arg(self.dir("stores").join("fl.redb"))
            .arg("store")
            .args(args);
        c
    }

    fn json(&self, args: &[&str]) -> Value {
        let out = self.fl(args).output().unwrap();
        assert!(
            out.status.success(),
            "fl store {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

#[test]
fn put_get_and_query_work_on_the_bound_store() {
    let sb = Sandbox::new();
    sb.fl(&["get", "docs", "a"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains(
            "`fl store get` does not create one",
        ));
    assert!(!sb.fls().exists());
    sb.json(&["put", "docs", "a", r#"{"kind": "fox", "text": "red fox"}"#]);
    assert!(sb.fls().exists());
    assert_eq!(sb.json(&["get", "docs", "a"])["kind"], "fox");
    assert_eq!(
        sb.json(&["query", "docs", "kind", "fox"])["ids"],
        serde_json::json!(["a"])
    );
    assert_eq!(
        sb.json(&["query", "--bm25", "docs", "fox"])["hits"][0]["id"],
        "a"
    );
    sb.fl(&["get", "docs", "missing"]).assert().code(1);
    sb.fl(&["put", "__meta", "x", "1"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("reserved"));
    sb.fl(&["get", "--connect", "/a/path", "docs", "a"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("--connect takes a unix://"));
}

#[test]
fn put_without_db_writes_the_default_bound_store() {
    let sb = Sandbox::new();
    sb.fl_unbound(&["put", "docs", "a", "1"]).assert().success();
    let out = sb.fl_unbound(&["get", "docs", "a"]).output().unwrap();
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout), "1\n");
    let inspected: Value = serde_json::from_slice(
        &sb.fl_unbound(&["inspect", "--json"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let path = inspected["path"].as_str().unwrap();
    assert!(path.starts_with(s(&sb.dir("data"))), "{path}");
    assert!(path.ends_with(".fls"), "{path}");
}

#[test]
fn inspect_reads_the_bound_store() {
    let sb = Sandbox::new();
    sb.fl(&["inspect"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("does not create one"));
    sb.json(&["put", "docs", "a", "{}"]);
    let v = sb.json(&["inspect", "--json"]);
    assert_eq!(v["path"], s(&sb.fls()));
    assert_eq!(v["slots"].as_array().unwrap().len(), 2);
    let docs = v["contents"]["collections"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "docs")
        .unwrap()
        .clone();
    assert_eq!(docs["docs"], 1);
    let text = sb.fl(&["inspect"]).output().unwrap();
    assert!(String::from_utf8_lossy(&text.stdout).contains("  docs: 1 docs"));
}

#[test]
fn bench_runs_on_fresh_stores_never_the_bound_one() {
    let sb = Sandbox::new();
    let v = sb.json(&[
        "bench",
        "--n",
        "2",
        "--workload",
        "put,get",
        "--engines",
        "ferro",
        "--json",
    ]);
    assert_eq!(v["target"], "fresh");
    assert_eq!(v["results"].as_array().unwrap().len(), 2);
    assert!(!sb.fls().exists(), "bench touched the bound store");
    // The private directory in $TMPDIR is gone.
    assert_eq!(std::fs::read_dir(sb.dir("tmp")).unwrap().count(), 0);
}

struct Server {
    child: Child,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn serve_serves_the_bound_store_to_get_and_put_over_connect() {
    let sb = Sandbox::new();
    let mut cmd = std::process::Command::new(assert_cmd::cargo::cargo_bin("fl"));
    cmd.env("HOME", sb.dir("home"))
        .env("XDG_CONFIG_HOME", sb.dir("config"))
        .env("XDG_DATA_HOME", sb.dir("data"))
        .env("XDG_RUNTIME_DIR", sb.dir("run"))
        .env("TMPDIR", sb.dir("tmp"))
        .env_remove("FL_DB")
        .env_remove("FL_STORE_BACKEND")
        .arg("--db")
        .arg(sb.dir("stores").join("fl.redb"))
        .args(["store", "serve", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    let mut ready = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut ready)
        .unwrap();
    let server = Server { child };
    let ready: Value = serde_json::from_str(&ready).unwrap();
    assert_eq!(ready["store"], s(&sb.fls()));
    let uri = ready["uri"].as_str().unwrap().to_string();
    assert_eq!(uri, format!("unix://{}.sock", sb.fls().display()));

    sb.fl(&["put", "--connect", &uri, "docs", "a", r#"{"served": true}"#])
        .assert()
        .success();
    assert_eq!(
        sb.json(&["get", "--connect", &uri, "docs", "a"])["served"],
        true
    );
    drop(server);
}
