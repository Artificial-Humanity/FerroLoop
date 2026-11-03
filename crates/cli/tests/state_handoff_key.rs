//! Agents that share a scope write their handoffs under their own `--key`, so one agent's
//! handoff never overwrites another's; the default key stays `current`.

use std::path::Path;

use assert_cmd::Command;
use serde_json::Value;

const SCOPE: &str = "project:demo";

fn fl(cfg: &tempfile::TempDir, db: &Path) -> Command {
    let mut c = Command::cargo_bin("fl").unwrap();
    c.env("XDG_CONFIG_HOME", cfg.path())
        .env_remove("FL_DB")
        .arg("--db")
        .arg(db)
        .current_dir(cfg.path());
    c
}

fn json(cmd: &mut Command) -> Value {
    let out = cmd.assert().success().get_output().stdout.clone();
    serde_json::from_slice(&out).unwrap()
}

fn set(cfg: &tempfile::TempDir, db: &Path, key: Option<&str>, summary: &str, by: &str) -> Value {
    let mut c = fl(cfg, db);
    c.args(["state", "handoff", "set", "--scope", SCOPE]);
    if let Some(k) = key {
        c.args(["--key", k]);
    }
    c.args(["--summary", summary, "--by", by, "--json"]);
    json(&mut c)
}

fn get(cfg: &tempfile::TempDir, db: &Path, key: Option<&str>) -> Command {
    let mut c = fl(cfg, db);
    c.args(["state", "handoff", "get", "--scope", SCOPE]);
    if let Some(k) = key {
        c.args(["--key", k]);
    }
    c.arg("--json");
    c
}

#[test]
fn handoffs_under_different_keys_do_not_overwrite_each_other() {
    let cfg = tempfile::tempdir().unwrap();
    let db = cfg.path().join("t.redb");

    set(&cfg, &db, Some("alpha"), "alpha handoff one", "alpha");
    set(&cfg, &db, Some("beta"), "beta handoff", "beta");
    set(&cfg, &db, None, "shared current handoff", "owner");

    let a = json(&mut get(&cfg, &db, Some("alpha")));
    let b = json(&mut get(&cfg, &db, Some("beta")));
    let cur = json(&mut get(&cfg, &db, None));
    assert_eq!(
        a["payload"]["handoff"]["summary"], "alpha handoff one",
        "{a}"
    );
    assert_eq!(b["payload"]["handoff"]["summary"], "beta handoff", "{b}");
    assert_eq!(
        cur["payload"]["handoff"]["summary"], "shared current handoff",
        "{cur}"
    );
    assert_eq!(cur["slug"], "current");
    assert_ne!(a["id"], b["id"]);
    assert_ne!(a["id"], cur["id"]);

    // Rewriting one key updates that entry in place and leaves the others alone.
    set(&cfg, &db, Some("alpha"), "alpha handoff two", "alpha");
    let a2 = json(&mut get(&cfg, &db, Some("alpha")));
    assert_eq!(a2["id"], a["id"], "same key, same entry");
    assert_eq!(a2["payload"]["handoff"]["summary"], "alpha handoff two");
    let b2 = json(&mut get(&cfg, &db, Some("beta")));
    assert_eq!(b2["payload"]["handoff"]["summary"], "beta handoff");
    let cur2 = json(&mut get(&cfg, &db, None));
    assert_eq!(
        cur2["payload"]["handoff"]["summary"],
        "shared current handoff"
    );

    // Exactly three handoff entries exist in the scope.
    let list = json(fl(&cfg, &db).args([
        "state",
        "list",
        "--scope",
        SCOPE,
        "--concern",
        "handoff",
        "--json",
    ]));
    assert_eq!(list.as_array().unwrap().len(), 3, "{list}");
}

#[test]
fn handoff_get_reports_a_missing_key_and_an_invalid_key() {
    let cfg = tempfile::tempdir().unwrap();
    let db = cfg.path().join("t.redb");
    set(&cfg, &db, Some("alpha"), "alpha handoff", "alpha");

    // Not found: a runtime failure (exit 1) with the store's message, not a clap parse error.
    get(&cfg, &db, Some("gamma"))
        .assert()
        .code(1)
        .stderr(predicates::str::contains("no handoff `gamma`"));
    // No shared `current` handoff exists yet either.
    get(&cfg, &db, None)
        .assert()
        .code(1)
        .stderr(predicates::str::contains("no handoff `current`"));
    // Invalid slug: a distinct validation message, not "no handoff" and not a clap usage error.
    let bad = get(&cfg, &db, Some("Not A Slug!")).assert().code(1);
    let stderr = String::from_utf8_lossy(&bad.get_output().stderr).to_string();
    assert!(stderr.contains("invalid slug `Not A Slug!`"), "{stderr}");
    assert!(!stderr.contains("no handoff"), "{stderr}");
    assert!(!stderr.contains("Usage:"), "{stderr}");
}

#[test]
fn handoff_get_ignores_a_memory_entry_with_the_same_key() {
    let cfg = tempfile::tempdir().unwrap();
    let db = cfg.path().join("t.redb");
    json(fl(&cfg, &db).args([
        "state",
        "memory",
        "set",
        "--scope",
        SCOPE,
        "--key",
        "alpha",
        "--category",
        "note",
        "--basis",
        "inference",
        "--summary",
        "a memory that shares the persona name",
        "--body",
        "body",
        "--by",
        "owner",
        "--json",
    ]));
    set(&cfg, &db, Some("alpha"), "alpha handoff", "alpha");

    // The generic reader sees two concerns under the slug and refuses to guess...
    fl(&cfg, &db)
        .args(["state", "get", "alpha", "--scope", SCOPE])
        .assert()
        .failure();
    // ...while `handoff get` always returns the handoff.
    let h = json(&mut get(&cfg, &db, Some("alpha")));
    assert_eq!(h["concern"], "handoff", "{h}");
    assert_eq!(h["payload"]["handoff"]["summary"], "alpha handoff");
}
