//! `fl state search --mode auto|lexical|vector|hybrid` and the `mode`
//! argument of the `fl_memory_search` MCP tool, end to end on throwaway
//! stores. The vector leg is the deterministic local hashing embedder: no
//! network and no model download.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::Stdio;

use assert_cmd::Command;
use serde_json::{Value, json};
use tempfile::TempDir;

/// `backend`: `None` is the default engine (FerroStore); `Some("redb")` forces redb.
fn fl(cfg: &TempDir, db: &Path, backend: Option<&str>) -> Command {
    let mut c = Command::cargo_bin("fl").unwrap();
    c.env("XDG_CONFIG_HOME", cfg.path())
        .env("XDG_DATA_HOME", cfg.path().join("data"))
        .env_remove("FL_DB")
        .env_remove("FL_STORE_BACKEND")
        .current_dir(cfg.path())
        .arg("--db")
        .arg(db);
    if let Some(b) = backend {
        c.env("FL_STORE_BACKEND", b);
    }
    c
}

fn set_memory(
    cfg: &TempDir,
    db: &Path,
    backend: Option<&str>,
    scope: &str,
    key: &str,
    s: &str,
    b: &str,
) {
    fl(cfg, db, backend)
        .args([
            "state",
            "memory",
            "set",
            "--scope",
            scope,
            "--key",
            key,
            "--category",
            "note",
            "--basis",
            "inference",
            "--summary",
            s,
            "--body",
            b,
            "--by",
            "test",
            "--json",
        ])
        .assert()
        .success();
}

/// The shared corpus. `build-cache` has the near word `checkpoint` only;
/// `exact-term` has the whole term `checkpointing`.
fn seed(cfg: &TempDir, db: &Path, backend: Option<&str>) {
    for (key, summary, body) in [
        (
            "build-cache",
            "checkpoint the build cache",
            "Nightly job writes a checkpoint of the build cache to disk.",
        ),
        (
            "exact-term",
            "quarterly planning",
            "checkpointing quarterly planning agenda",
        ),
        (
            "lunch-notes",
            "meeting notes about lunch",
            "The team agreed on sandwiches for the offsite.",
        ),
    ] {
        set_memory(cfg, db, backend, "workspace", key, summary, body);
    }
}

fn search(cfg: &TempDir, db: &Path, backend: Option<&str>, args: &[&str]) -> Value {
    let out = fl(cfg, db, backend)
        .args(["state", "search"])
        .args(args)
        .arg("--json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&out).unwrap()
}

fn slugs(v: &Value) -> Vec<String> {
    v["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["entry"]["slug"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn lexical_vector_and_hybrid_are_distinct_end_to_end() {
    let cfg = TempDir::new().unwrap();
    let db = cfg.path().join("s.redb");
    seed(&cfg, &db, None);
    let q = "checkpointing";

    let lex = search(&cfg, &db, None, &[q, "--mode", "lexical"]);
    assert_eq!(lex["mode"], "lexical", "{lex}");
    assert_eq!(slugs(&lex), ["exact-term"], "{lex}");
    assert!(lex["hits"][0]["score_millis"].as_u64().unwrap() > 0);

    let vec = search(&cfg, &db, None, &[q, "--mode", "vector"]);
    assert_eq!(vec["mode"], "vector", "{vec}");
    assert!(slugs(&vec).contains(&"build-cache".to_string()), "{vec}");
    assert!(vec["hits"][0]["score_millis"].as_u64().unwrap() > 0);
    assert_eq!(
        vec["total_hits"].as_u64().unwrap() as usize,
        slugs(&vec).len()
    );

    let hyb = search(&cfg, &db, None, &[q, "--mode", "hybrid"]);
    assert_eq!(hyb["mode"], "hybrid", "{hyb}");
    let hs = slugs(&hyb);
    assert_eq!(hs[0], "exact-term", "{hyb}");
    assert!(hs.contains(&"build-cache".to_string()), "{hyb}");
    assert!(
        hyb["hits"][0]["score_millis"].as_u64() > hyb["hits"][1]["score_millis"].as_u64(),
        "{hyb}"
    );
}

#[test]
fn every_mode_is_deterministic_across_runs_and_stores() {
    let a = TempDir::new().unwrap();
    let b = TempDir::new().unwrap();
    let (da, db_) = (a.path().join("s.redb"), b.path().join("s.redb"));
    seed(&a, &da, None);
    seed(&b, &db_, None);
    for mode in ["auto", "lexical", "vector", "hybrid"] {
        let one = search(&a, &da, None, &["checkpointing", "--mode", mode]);
        let again = search(&a, &da, None, &["checkpointing", "--mode", mode]);
        let other = search(&b, &db_, None, &["checkpointing", "--mode", mode]);
        assert!(!slugs(&one).is_empty(), "{mode}");
        assert_eq!(slugs(&one), slugs(&again), "{mode} repeat");
        assert_eq!(slugs(&one), slugs(&other), "{mode} across stores");
        let scores = |v: &Value| -> Vec<u64> {
            v["hits"]
                .as_array()
                .unwrap()
                .iter()
                .map(|h| h["score_millis"].as_u64().unwrap())
                .collect()
        };
        assert_eq!(scores(&one), scores(&other), "{mode} scores");
    }
}

#[test]
fn scope_concern_and_limit_filters_apply_in_every_mode() {
    let cfg = TempDir::new().unwrap();
    let db = cfg.path().join("s.redb");
    set_memory(
        &cfg,
        &db,
        None,
        "workspace",
        "ws-cache",
        "checkpoint workspace cache",
        "x",
    );
    set_memory(
        &cfg,
        &db,
        None,
        "project:alpha",
        "proj-cache",
        "checkpoint project cache",
        "x",
    );
    set_memory(
        &cfg,
        &db,
        None,
        "project:alpha",
        "proj-cache-2",
        "checkpoint project cache two",
        "x",
    );
    for mode in ["auto", "lexical", "vector", "hybrid"] {
        let all = search(&cfg, &db, None, &["checkpoint cache", "--mode", mode]);
        assert_eq!(all["total_hits"], 3, "{mode}: {all}");

        let scoped = search(
            &cfg,
            &db,
            None,
            &[
                "checkpoint cache",
                "--mode",
                mode,
                "--scope",
                "project:alpha",
            ],
        );
        assert_eq!(scoped["total_hits"], 2, "{mode}: {scoped}");
        assert!(
            slugs(&scoped).iter().all(|s| s.starts_with("proj-cache")),
            "{mode}: {scoped}"
        );

        let limited = search(
            &cfg,
            &db,
            None,
            &["checkpoint cache", "--mode", mode, "--limit", "1"],
        );
        assert_eq!(limited["hits"].as_array().unwrap().len(), 1, "{mode}");
        assert_eq!(
            limited["total_hits"], 3,
            "{mode}: total is before the limit"
        );

        let wrong_concern = search(
            &cfg,
            &db,
            None,
            &["checkpoint cache", "--mode", mode, "--concern", "runtime"],
        );
        assert_eq!(wrong_concern["total_hits"], 0, "{mode}: {wrong_concern}");
    }
}

#[test]
fn writes_update_the_vector_leg_and_old_entries_are_backfilled() {
    let cfg = TempDir::new().unwrap();
    let db = cfg.path().join("s.redb");
    // Written before any vector search ever ran: backfilled by the first one.
    seed(&cfg, &db, None);
    let v = |cfg: &TempDir, db: &Path| {
        slugs(&search(
            cfg,
            db,
            None,
            &["checkpointing", "--mode", "vector"],
        ))
    };
    assert!(v(&cfg, &db).contains(&"build-cache".to_string()));

    // Create.
    set_memory(
        &cfg,
        &db,
        None,
        "workspace",
        "fresh",
        "checkpoint replay",
        "replaying one",
    );
    assert!(v(&cfg, &db).contains(&"fresh".to_string()));

    // Update: the same key now holds unrelated text.
    set_memory(
        &cfg,
        &db,
        None,
        "workspace",
        "build-cache",
        "lunch sandwiches",
        "offsite menu",
    );
    assert!(!v(&cfg, &db).contains(&"build-cache".to_string()));
    assert!(v(&cfg, &db).contains(&"fresh".to_string()));
}

#[test]
fn default_output_is_unchanged_golden() {
    let cfg = TempDir::new().unwrap();
    let ferro_db = cfg.path().join("f.redb");
    let redb_db = cfg.path().join("r.redb");
    seed(&cfg, &ferro_db, None);
    seed(&cfg, &redb_db, Some("redb"));

    // redb: BM25 only, exactly the original JSON shape (no `mode`, no `note`).
    let plain = search(&cfg, &redb_db, Some("redb"), &["checkpointing"]);
    let mut keys: Vec<&str> = plain
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, ["hits", "query", "total_hits"], "{plain}");
    assert_eq!(plain["query"], "checkpointing");
    assert_eq!(slugs(&plain), ["exact-term"]);
    let hit_keys: Vec<String> = {
        let mut k: Vec<String> = plain["hits"][0]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        k.sort();
        k
    };
    assert_eq!(
        hit_keys,
        [
            "entry",
            "estimated_tokens",
            "matched_terms",
            "score_millis",
            "snippet"
        ]
    );
    // `--mode auto` is the default, byte for byte.
    assert_eq!(
        plain,
        search(
            &cfg,
            &redb_db,
            Some("redb"),
            &["checkpointing", "--mode", "auto"]
        )
    );

    // ferro: the fused ranking it always had, reported as `knowledge`.
    let fused = search(&cfg, &ferro_db, None, &["checkpointing"]);
    assert_eq!(fused["mode"], "knowledge", "{fused}");
    assert_eq!(
        fused,
        search(&cfg, &ferro_db, None, &["checkpointing", "--mode", "auto"])
    );
    let hyb = search(
        &cfg,
        &ferro_db,
        None,
        &["checkpointing", "--mode", "hybrid"],
    );
    assert_eq!(slugs(&fused), slugs(&hyb));

    // Text output keeps its tab-separated line format.
    let out = fl(&cfg, &redb_db, Some("redb"))
        .args(["state", "search", "checkpointing"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    let cols: Vec<&str> = text.trim_end().split('\t').collect();
    assert_eq!(cols.len(), 6, "{text}");
    assert_eq!(&cols[1..4], ["workspace", "memory", "exact-term"], "{text}");
    assert!(cols[4].starts_with("tokens="), "{text}");
}

#[test]
fn vector_and_hybrid_are_refused_on_redb_and_lexical_still_works() {
    let cfg = TempDir::new().unwrap();
    let db = cfg.path().join("r.redb");
    seed(&cfg, &db, Some("redb"));
    for mode in ["vector", "hybrid"] {
        let assert = fl(&cfg, &db, Some("redb"))
            .args(["state", "search", "checkpointing", "--mode", mode])
            .assert()
            .failure();
        let err = String::from_utf8_lossy(&assert.get_output().stderr).to_string();
        assert!(err.contains("needs the FerroStore engine"), "{mode}: {err}");
    }
    let lex = search(
        &cfg,
        &db,
        Some("redb"),
        &["checkpointing", "--mode", "lexical"],
    );
    assert_eq!(lex["mode"], "lexical");
    assert_eq!(slugs(&lex), ["exact-term"]);
}

#[test]
fn an_unknown_mode_is_rejected_with_the_valid_values() {
    let cfg = TempDir::new().unwrap();
    let db = cfg.path().join("s.redb");
    let assert = fl(&cfg, &db, None)
        .args(["state", "search", "x", "--mode", "bm25"])
        .assert()
        .failure();
    let err = String::from_utf8_lossy(&assert.get_output().stderr).to_string();
    assert!(err.contains("auto, lexical, vector, hybrid"), "{err}");
}

#[test]
fn mcp_fl_memory_search_takes_a_mode_and_defaults_to_auto() {
    let cfg = TempDir::new().unwrap();
    let db = cfg.path().join("s.redb");
    seed(&cfg, &db, None);

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_fl"));
    child
        .current_dir(cfg.path())
        .env("XDG_CONFIG_HOME", cfg.path())
        .env("XDG_DATA_HOME", cfg.path().join("data"))
        .env_remove("FL_DB")
        .env_remove("FL_STORE_BACKEND")
        .arg("--db")
        .arg(&db)
        .args(["state", "mcp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    let mut child = child.spawn().unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut id = 0;
    let mut call = |args: Value| -> Value {
        id += 1;
        let req = json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": { "name": "fl_memory_search", "arguments": args }
        });
        writeln!(input, "{req}").unwrap();
        input.flush().unwrap();
        let mut line = String::new();
        output.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    };
    let payload = |resp: &Value| -> Value {
        assert_eq!(resp["result"]["isError"], false, "{resp}");
        serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
    };

    let lex = payload(&call(json!({"query": "checkpointing", "mode": "lexical"})));
    assert_eq!(lex["mode"], "lexical");
    assert_eq!(slugs(&lex), ["exact-term"]);
    let vec = payload(&call(json!({"query": "checkpointing", "mode": "vector"})));
    assert_eq!(vec["mode"], "vector");
    assert!(slugs(&vec).contains(&"build-cache".to_string()), "{vec}");
    let hyb = payload(&call(json!({"query": "checkpointing", "mode": "hybrid"})));
    assert_eq!(hyb["mode"], "hybrid");
    assert_eq!(slugs(&hyb)[0], "exact-term");
    let default = payload(&call(json!({"query": "checkpointing"})));
    assert_eq!(default["mode"], "knowledge", "default is auto: {default}");
    assert_eq!(slugs(&default), slugs(&hyb));

    let bad = call(json!({"query": "checkpointing", "mode": "bm25"}));
    assert_eq!(bad["result"]["isError"], true, "{bad}");

    drop(input);
    assert!(child.wait().unwrap().success());
}
