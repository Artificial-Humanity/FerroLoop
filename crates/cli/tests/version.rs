//! `fl --version`, `fl --version --json` and `fl version [--json]`.

use assert_cmd::Command;
use serde_json::Value;

fn fl() -> Command {
    let mut c = Command::cargo_bin("fl").unwrap();
    c.env_remove("FL_DB");
    c
}

fn stdout(args: &[&str]) -> String {
    let out = fl().args(args).assert().success().get_output().clone();
    String::from_utf8(out.stdout).unwrap()
}

fn json(args: &[&str]) -> Value {
    serde_json::from_str(&stdout(args)).expect("one JSON object")
}

#[test]
fn version_json_has_the_documented_keys() {
    let v = json(&["--version", "--json"]);
    for key in [
        "name",
        "version",
        "commit",
        "commit_short",
        "dirty",
        "build_time_unix",
        "source",
        "release_id",
        "protocol",
        "schema",
        "store_formats",
    ] {
        assert!(v.get(key).is_some(), "missing {key}: {v}");
    }
    assert_eq!(v["name"], "fl");
    assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
    // A source build never poses as a release.
    assert_eq!(v["source"], "source-build");
    // A release id exists only when `$FL_RELEASE_ID` was set at build time, and then only for
    // a tree known to be clean. Either way the build still says `source-build`.
    match &v["release_id"] {
        Value::Null => {}
        Value::String(_) => assert_eq!(v["dirty"], Value::Bool(false), "{v}"),
        other => panic!("release_id is {other}"),
    }
    assert!(v["build_time_unix"].is_u64() || v["build_time_unix"].is_null());
    assert!(v["dirty"].is_boolean() || v["dirty"].is_null());
    let commit = v["commit"].as_str().unwrap();
    let short = v["commit_short"].as_str().unwrap();
    if commit == "unknown" {
        assert_eq!(short, "unknown");
    } else {
        assert_eq!(commit.len(), 40);
        assert!(commit.starts_with(short) && short.len() == 12);
    }
    // Ranges are {min,max}, exact versions {current}; never a bare guess.
    assert!(v["protocol"]["ferrowire"]["current"].is_u64());
    let manifest = &v["schema"]["manifest"];
    assert!(manifest["min"].as_u64() <= manifest["max"].as_u64());
    assert!(v["schema"]["vault"]["current"].is_u64());
    assert!(v["schema"]["warrant_ledger"]["current"].is_u64());
    let redb = &v["store_formats"]["redb_schema"];
    assert!(redb["min"].as_u64().unwrap() < redb["max"].as_u64().unwrap());
    assert!(v["store_formats"]["ferrostore_container"]["max"].is_u64());
}

#[test]
fn every_entry_point_prints_the_same_json() {
    let a = json(&["--version", "--json"]);
    assert_eq!(a, json(&["--json", "--version"]));
    assert_eq!(a, json(&["-V", "--json"]));
    assert_eq!(a, json(&["version", "--json"]));
}

#[test]
fn human_version_carries_the_short_commit_in_every_entry_point() {
    let v = json(&["version", "--json"]);
    let short = v["commit_short"].as_str().unwrap();
    let line = stdout(&["--version"]);
    assert_eq!(line.lines().count(), 1, "{line}");
    assert!(
        line.starts_with(&format!("fl {} (", env!("CARGO_PKG_VERSION"))),
        "{line}"
    );
    if short == "unknown" {
        assert!(line.contains("unknown commit"), "{line}");
    } else {
        assert!(line.contains(short), "{line}");
    }
    if v["dirty"] == Value::Bool(true) {
        assert!(line.contains(", dirty"), "{line}");
    }
    assert_eq!(line, stdout(&["-V"]));
    assert_eq!(line, stdout(&["version"]));
}

#[test]
fn version_needs_no_store_and_no_valid_config() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = tmp.path().join("fl");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(cfg.join("config.toml"), "this is = not [valid toml").unwrap();
    for args in [
        &["version"][..],
        &["version", "--json"][..],
        &["--version", "--json"][..],
    ] {
        fl().env("XDG_CONFIG_HOME", tmp.path())
            .env("XDG_DATA_HOME", tmp.path().join("no-such-dir"))
            .args(args)
            .assert()
            .success();
    }
    // Nothing was created: no store directory appeared.
    assert!(!tmp.path().join("no-such-dir").exists());
}

#[test]
fn json_is_one_line_on_stdout_with_nothing_on_stderr() {
    let out = fl()
        .args(["--version", "--json"])
        .assert()
        .success()
        .get_output()
        .clone();
    assert!(out.stderr.is_empty());
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(text.lines().count(), 1);
    assert!(text.ends_with('\n'));
}

#[test]
fn only_the_exact_json_pair_is_json() {
    // Anything else next to `--version` is clap's: `--version` wins and prints the human line.
    let line = stdout(&["--version"]);
    assert_eq!(stdout(&["--version", "--json", "extra"]), line);
    // An unknown flag of `fl version` is an error.
    fl().args(["version", "--nope"]).assert().failure();
}
