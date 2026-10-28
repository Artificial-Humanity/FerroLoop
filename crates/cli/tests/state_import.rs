use std::path::Path;

use assert_cmd::Command;
use serde_json::Value;

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn fl(cfg: &tempfile::TempDir, db: &Path) -> Command {
    let mut c = Command::cargo_bin("fl").unwrap();
    c.env("XDG_CONFIG_HOME", cfg.path()).arg("--db").arg(db);
    c
}

fn fl_no_db(cfg: &tempfile::TempDir) -> Command {
    let mut c = Command::cargo_bin("fl").unwrap();
    c.env("XDG_CONFIG_HOME", cfg.path());
    c
}

fn write_config(cfg: &tempfile::TempDir, ws_store: &Path, proj_root: &Path, proj_store: &Path) {
    write(
        &cfg.path().join("fl/config.toml"),
        &format!(
            "[state]\nstore = \"{}\"\n\n[[project]]\nname = \"demo-project\"\nroot = \"{}\"\nstore = \"{}\"\n",
            ws_store.display(),
            proj_root.display(),
            proj_store.display()
        ),
    );
}

fn write_workspace_fixture(ws: &tempfile::TempDir) {
    write(
        &ws.path().join(".agents/memory/MEMORY.md"),
        "- [Keep](keep-this.md)\n",
    );
    write(
        &ws.path().join(".agents/memory/keep-this.md"),
        "---\ndescription: Keep this rule\nmetadata:\n  type: feedback\n---\nBody text.\n",
    );
    write(
        &ws.path().join("Notes/Demo Project/STATE.md"),
        "# Demo\n\n## Shipped\n\n- Did a thing\n\n## Next steps\n\n- Do more\n",
    );
}

fn run_json(cmd: &mut Command) -> Value {
    let out = cmd.assert().success().get_output().stdout.clone();
    serde_json::from_slice(&out).unwrap()
}

#[test]
fn state_import_workspace_imports_memories_and_handoffs_idempotently() {
    let cfg = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let db = cfg.path().join("t.redb");

    write(
        &ws.path().join(".agents/memory/MEMORY.md"),
        "- [Keep](keep-this.md)\n",
    );
    write(
        &ws.path().join(".agents/memory/keep-this.md"),
        "---\ndescription: Keep this rule\nmetadata:\n  type: feedback\n---\nBody text.\n",
    );
    write(
        &ws.path().join("Notes/Demo Project/STATE.md"),
        "# Demo\n\n## Shipped\n\n- Did a thing\n\n## Next steps\n\n- Do more\n",
    );

    let ws_arg = ws.path().display().to_string();
    let report = run_json(fl(&cfg, &db).args([
        "state",
        "import-workspace",
        "--workspace-root",
        &ws_arg,
        "--json",
    ]));
    assert_eq!(report["imported_memories"], 1);
    assert_eq!(report["imported_handoffs"], 1);
    assert_eq!(
        report["scopes"],
        serde_json::json!(["project:demo-project", "workspace"])
    );
    assert_eq!(report["skipped"], serde_json::json!([]));

    let list = run_json(fl(&cfg, &db).args(["state", "list", "--json"]));
    let entries = list.as_array().unwrap();
    // The workspace store holds workspace entries; the project handoff is
    // routed by `--db` to the same store, so both are visible here.
    assert_eq!(entries.len(), 2, "{list}");
    assert!(entries.iter().any(|e| e["slug"] == "keep-this"));
    assert!(entries.iter().any(|e| e["slug"] == "project-state"));

    // Re-import updates the same keys instead of duplicating entries.
    let again = run_json(fl(&cfg, &db).args([
        "state",
        "import-workspace",
        "--workspace-root",
        &ws_arg,
        "--json",
    ]));
    assert_eq!(again["imported_memories"], 1);
    let list2 = run_json(fl(&cfg, &db).args(["state", "list", "--json"]));
    assert_eq!(list2.as_array().unwrap().len(), 2);
}

#[test]
fn state_import_workspace_without_db_routes_scopes_to_distinct_stores() {
    let cfg = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    write_workspace_fixture(&ws);
    let ws_store = cfg.path().join("ws.redb");
    let proj_store = cfg.path().join("proj.redb");
    write_config(
        &cfg,
        &ws_store,
        &ws.path().join("Notes/Demo Project"),
        &proj_store,
    );

    let ws_arg = ws.path().display().to_string();
    let report = run_json(fl_no_db(&cfg).args([
        "state",
        "import-workspace",
        "--workspace-root",
        &ws_arg,
        "--json",
    ]));
    assert_eq!(report["imported_memories"], 1, "{report}");
    assert_eq!(report["imported_handoffs"], 1, "{report}");
    assert_eq!(report["skipped"], serde_json::json!([]), "{report}");

    let ws_entries = run_json(fl(&cfg, &ws_store).args(["state", "list", "--json"]));
    let ws_entries = ws_entries.as_array().unwrap();
    assert_eq!(ws_entries.len(), 1, "{ws_entries:?}");
    assert_eq!(ws_entries[0]["slug"], "keep-this");

    let proj_entries = run_json(fl(&cfg, &proj_store).args(["state", "list", "--json"]));
    let proj_entries = proj_entries.as_array().unwrap();
    assert_eq!(proj_entries.len(), 1, "{proj_entries:?}");
    assert_eq!(proj_entries[0]["slug"], "project-state");
}

#[test]
fn state_import_workspace_continues_when_one_scope_store_fails_to_open() {
    let cfg = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    write_workspace_fixture(&ws);
    let ws_store = cfg.path().join("ws.redb");
    // A directory where the project store file should be: open must fail.
    let proj_store = cfg.path().join("proj.redb");
    std::fs::create_dir_all(&proj_store).unwrap();
    write_config(
        &cfg,
        &ws_store,
        &ws.path().join("Notes/Demo Project"),
        &proj_store,
    );

    let ws_arg = ws.path().display().to_string();
    let report = run_json(fl_no_db(&cfg).args([
        "state",
        "import-workspace",
        "--workspace-root",
        &ws_arg,
        "--json",
    ]));
    assert_eq!(report["imported_memories"], 1, "{report}");
    assert_eq!(report["imported_handoffs"], 0, "{report}");
    let skipped = report["skipped"].as_array().unwrap();
    assert_eq!(skipped.len(), 1, "{report}");
    let reason = skipped[0].as_str().unwrap();
    assert!(reason.contains("project:demo-project"), "{reason}");
    assert!(reason.contains("failed to open state store"), "{reason}");

    let ws_entries = run_json(fl(&cfg, &ws_store).args(["state", "list", "--json"]));
    assert_eq!(ws_entries.as_array().unwrap().len(), 1);
}
