use std::path::Path;

use assert_cmd::Command;
use serde_json::Value;

fn fl(cfg: &tempfile::TempDir, db: &Path, cwd: &Path) -> Command {
    let mut c = Command::cargo_bin("fl").unwrap();
    c.env("XDG_CONFIG_HOME", cfg.path())
        .arg("--db")
        .arg(db)
        .current_dir(cwd);
    c
}

fn run_json(cmd: &mut Command) -> Value {
    let out = cmd.assert().success().get_output().stdout.clone();
    serde_json::from_slice(&out).unwrap()
}

fn write_widget(root: &Path) {
    std::fs::create_dir_all(root.join("src")).unwrap();
    let mut text = String::from("pub fn widget_alpha() {}\npub struct WidgetBeta;\n");
    for i in 0..40 {
        text.push_str(&format!("// filler line {i} padding the file out\n"));
    }
    std::fs::write(root.join("src/lib.rs"), text).unwrap();
}

#[test]
fn state_index_and_code_def_record_token_gain() {
    let cfg = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let db = cfg.path().join("t.redb");
    write_widget(root.path());

    let indexed = run_json(fl(&cfg, &db, root.path()).args([
        "state",
        "index",
        "--root",
        root.path().to_str().unwrap(),
        "--json",
    ]));
    assert_eq!(indexed["files_indexed"], 1, "{indexed}");
    assert_eq!(indexed["symbols_indexed"], 2, "{indexed}");
    assert_eq!(indexed["token_gain"]["files_indexed"], 1, "{indexed}");
    assert_eq!(indexed["token_gain"]["symbols_indexed"], 2, "{indexed}");
    assert_eq!(indexed["token_gain"]["queries_count"], 0, "{indexed}");

    let def = run_json(fl(&cfg, &db, root.path()).args([
        "state",
        "code-def",
        "widget_alpha",
        "--root",
        root.path().to_str().unwrap(),
        "--json",
    ]));
    assert_eq!(def["query"], "widget_alpha", "{def}");
    assert_eq!(def["kind"], "def", "{def}");
    assert_eq!(def["symbols"][0]["name"], "widget_alpha", "{def}");
    assert_eq!(def["symbols"][0]["path"], "src/lib.rs", "{def}");
    assert!(
        def["tokens_saved_this_query"].as_u64().unwrap() > 0,
        "{def}"
    );
    assert_eq!(def["token_gain"]["queries_count"], 1, "{def}");
    assert_eq!(def["token_gain"]["files_indexed"], 1, "{def}");
    assert_eq!(def["token_gain"]["symbols_indexed"], 2, "{def}");

    // Human output is one `path:start kind name — signature` line.
    let out = fl(&cfg, &db, root.path())
        .args([
            "state",
            "code-outline",
            "src/lib.rs",
            "--root",
            root.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    assert!(
        text.contains("src/lib.rs:1 fn widget_alpha — pub fn widget_alpha() {}"),
        "{text}"
    );

    // Limit 0 returns no symbols and records no token gain.
    let before = run_json(fl(&cfg, &db, root.path()).args([
        "state",
        "code-search",
        "widget",
        "--root",
        root.path().to_str().unwrap(),
        "--limit",
        "0",
        "--json",
    ]));
    assert_eq!(before["kind"], "search", "{before}");
    assert_eq!(before["symbols"].as_array().unwrap().len(), 0, "{before}");
    assert_eq!(before["tokens_saved_this_query"], 0, "{before}");
    assert_eq!(before["token_gain"]["queries_count"], 2, "{before}");

    // Blank queries are refused instead of matching everything.
    fl(&cfg, &db, root.path())
        .args(["state", "code-outline", "   ", "--json"])
        .assert()
        .failure();
}

#[test]
fn state_install_registers_ferrostate_in_project_files() {
    let cfg = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let db = cfg.path().join("t.redb");

    let installed = run_json(fl(&cfg, &db, root.path()).args(["state", "install", "--json"]));
    assert_eq!(installed["installed"], true, "{installed}");
    assert_eq!(installed["scope"], "project", "{installed}");
    assert_eq!(
        installed["updated_files"].as_array().unwrap().len(),
        5,
        "{installed}"
    );

    for rel in [
        ".fl/mcp.toml",
        ".mcp.json",
        ".codex/config.toml",
        ".agents/mcp_config.json",
        ".muse/settings.json",
    ] {
        let path = root.path().join(rel);
        let text =
            std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        assert!(text.contains("ferrostate"), "{rel}: {text}");
        assert!(text.contains("state"), "{rel}: {text}");
        assert!(text.contains("mcp"), "{rel}: {text}");
        assert!(!text.contains("ferrogate"), "{rel}: {text}");
    }

    let mcp_json: Value =
        serde_json::from_str(&std::fs::read_to_string(root.path().join(".mcp.json")).unwrap())
            .unwrap();
    assert_eq!(
        mcp_json["mcpServers"]["ferrostate"]["args"],
        serde_json::json!(["state", "mcp"]),
        "{mcp_json}"
    );
}
