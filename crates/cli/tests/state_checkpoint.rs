use std::path::Path;
use std::process::Command as StdCommand;

use assert_cmd::Command;
use serde_json::Value;

fn git(repo: &Path, args: &[&str]) {
    let out = StdCommand::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

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

#[test]
fn state_checkpoint_records_git_state_with_json_report() {
    let cfg = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let db = cfg.path().join("t.redb");
    let root = repo.path();

    git(root, &["init", "-b", "main"]);
    git(root, &["config", "user.name", "Checkpoint Tester"]);
    git(root, &["config", "user.email", "tester@example.invalid"]);
    std::fs::write(root.join("tracked.txt"), "initial\n").unwrap();
    git(root, &["add", "tracked.txt"]);
    git(root, &["commit", "-m", "cli checkpoint subject"]);
    std::fs::write(root.join("untracked.txt"), "new\n").unwrap();

    let report =
        run_json(fl(&cfg, &db, root).args(["state", "checkpoint", "--by", "tester", "--json"]));
    let obj = report.as_object().unwrap();
    assert_eq!(obj.len(), 4, "{report}");
    assert_eq!(report["branch"], "main");
    assert_eq!(report["head_sha"].as_str().unwrap().len(), 40);
    assert_eq!(report["dirty_files"], 1);
    assert_eq!(report["entry"]["slug"], "checkpoint-tester");
    assert_eq!(report["entry"]["scope"], "workspace");
    assert_eq!(report["entry"]["concern"], "handoff");
    assert_eq!(report["entry"]["by"], "tester");

    // The checkpoint is a durable handoff entry in the store.
    let list = run_json(fl(&cfg, &db, root).args(["state", "list", "--json"]));
    let entries = list.as_array().unwrap();
    assert_eq!(entries.len(), 1, "{list}");
    assert_eq!(entries[0]["slug"], "checkpoint-tester");

    // A non-repository cwd is a clear failure, not a bogus checkpoint.
    let plain = tempfile::tempdir().unwrap();
    fl(&cfg, &db, plain.path())
        .args(["state", "checkpoint", "--json"])
        .assert()
        .failure();

    // A blank `--by` normalizes to `fl` (entry author and default slug).
    let blank =
        run_json(fl(&cfg, &db, root).args(["state", "checkpoint", "--by", "   ", "--json"]));
    assert_eq!(blank["entry"]["by"], "fl", "{blank}");
    assert_eq!(blank["entry"]["slug"], "checkpoint-fl", "{blank}");
}
