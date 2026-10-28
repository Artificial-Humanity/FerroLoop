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
