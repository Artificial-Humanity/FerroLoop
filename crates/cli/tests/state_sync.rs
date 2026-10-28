use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde_json::Value;
use tempfile::TempDir;

struct Machine {
    cfg_dir: PathBuf,
    data_dir: PathBuf,
    repo_dir: PathBuf,
}

impl Machine {
    fn fl(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_fl"));
        cmd.current_dir(&self.repo_dir)
            .env("XDG_CONFIG_HOME", &self.cfg_dir)
            .env("XDG_DATA_HOME", &self.data_dir)
            .env_remove("FL_DB");
        cmd
    }
}

fn setup_two_machines(private_remote: bool) -> (TempDir, PathBuf, Machine, Machine) {
    let tmp = TempDir::new().unwrap();
    let bare = tmp.path().join("remote.git");
    assert!(
        Command::new("git")
            .args(["init", "--bare", bare.to_str().unwrap()])
            .status()
            .unwrap()
            .success()
    );

    let make_machine = |label: &str| -> Machine {
        let root = tmp.path().join(label);
        let cfg_dir = root.join("config");
        let data_dir = root.join("data");
        let repo_dir = root.join("repo");
        let store_path = data_dir.join("fl/fl.redb");
        std::fs::create_dir_all(cfg_dir.join("fl")).unwrap();
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::create_dir_all(&repo_dir).unwrap();

        assert!(
            Command::new("git")
                .args(["init", "-b", "main", repo_dir.to_str().unwrap()])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&repo_dir)
                .args(["remote", "add", "origin", bare.to_str().unwrap()])
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(repo_dir.join("README.md"), "# Working Tree\n").unwrap();
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&repo_dir)
                .args(["add", "README.md"])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&repo_dir)
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@fl.local")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@fl.local")
                .args(["commit", "-m", "initial"])
                .status()
                .unwrap()
                .success()
        );

        std::fs::write(
            cfg_dir.join("fl/config.toml"),
            format!(
                "[state]\nstore = \"{}\"\nrepo = \"{}\"\nremote = \"origin\"\nprivate = {}\n",
                store_path.display(),
                repo_dir.display(),
                private_remote,
            ),
        )
        .unwrap();

        Machine {
            cfg_dir,
            data_dir,
            repo_dir,
        }
    };

    let a = make_machine("machine_a");
    let b = make_machine("machine_b");
    (tmp, bare, a, b)
}

fn run_json(mut cmd: Command) -> Value {
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "command failed ({:?}):\nstdout: {}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn clean_merge_and_noop_push_skip_across_two_machines() {
    let (_tmp, _bare, a, b) = setup_two_machines(false);

    let mut cmd_a = a.fl();
    cmd_a.args([
        "state",
        "memory",
        "set",
        "--scope",
        "workspace",
        "--key",
        "m-a",
        "--category",
        "rule",
        "--basis",
        "ruling",
        "--summary",
        "Rule from A",
        "--body",
        "Details A",
        "--by",
        "jarvis",
        "--json",
    ]);
    let mem_a = run_json(cmd_a);

    let mut cmd_b = b.fl();
    cmd_b.args([
        "state",
        "handoff",
        "set",
        "--scope",
        "workspace",
        "--key",
        "h-b",
        "--summary",
        "Handoff from B",
        "--shipped",
        "task-1",
        "--by",
        "ferris",
        "--json",
    ]);
    let handoff_b = run_json(cmd_b);

    let mut sync_a1 = a.fl();
    sync_a1.args(["state", "sync", "--by", "jarvis", "--json"]);
    let rep_a1 = run_json(sync_a1);
    assert_eq!(rep_a1["published_revisions"], 1);

    let mut sync_b1 = b.fl();
    sync_b1.args(["state", "sync", "--by", "ferris", "--json"]);
    let rep_b1 = run_json(sync_b1);
    assert_eq!(rep_b1["imported_revisions"], 1);
    assert_eq!(rep_b1["published_revisions"], 1);
    assert_eq!(rep_b1["active_conflicts"], 0);

    let mut sync_a2 = a.fl();
    sync_a2.args(["state", "sync", "--by", "jarvis", "--json"]);
    let rep_a2 = run_json(sync_a2);
    assert_eq!(rep_a2["published_revisions"], 0);
    assert_eq!(rep_a2["committed_oid"], rep_b1["committed_oid"]);

    // Second sync with no local writes creates no new commit.
    let mut sync_a3 = a.fl();
    sync_a3.args(["state", "sync", "--by", "jarvis", "--json"]);
    let rep_a3 = run_json(sync_a3);
    assert_eq!(rep_a3["committed_oid"], rep_b1["committed_oid"]);

    // Both entries exist on Machine A.
    let mut list_a = a.fl();
    list_a.args(["state", "list", "--json"]);
    let all_a = run_json(list_a);
    let ids: Vec<&str> = all_a
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&mem_a["id"].as_str().unwrap()));
    assert!(ids.contains(&handoff_b["id"].as_str().unwrap()));
}

#[test]
fn working_tree_and_index_remain_untouched_during_sync() {
    let (_tmp, _bare, a, _b) = setup_two_machines(false);
    let index_before = std::fs::read(a.repo_dir.join(".git/index")).unwrap();
    let head_before = Command::new("git")
        .arg("-C")
        .arg(&a.repo_dir)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap()
        .stdout;

    let mut set = a.fl();
    set.args([
        "state",
        "handoff",
        "set",
        "--scope",
        "workspace",
        "--summary",
        "Check isolation",
        "--by",
        "ferris",
        "--json",
    ]);
    let _ = run_json(set);

    let mut sync = a.fl();
    sync.args(["state", "sync", "--by", "ferris", "--json"]);
    let _ = run_json(sync);

    let index_after = std::fs::read(a.repo_dir.join(".git/index")).unwrap();
    let head_after = Command::new("git")
        .arg("-C")
        .arg(&a.repo_dir)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap()
        .stdout;
    let status_out = Command::new("git")
        .arg("-C")
        .arg(&a.repo_dir)
        .args(["status", "--porcelain"])
        .output()
        .unwrap()
        .stdout;

    assert_eq!(index_before, index_after);
    assert_eq!(head_before, head_after);
    assert!(status_out.is_empty());
}

#[test]
fn same_key_conflict_blocks_writes_and_resolves_cleanly() {
    let (_tmp, _bare, a, b) = setup_two_machines(false);

    // Base memory synced to both A and B.
    let mut base_cmd = a.fl();
    base_cmd.args([
        "state",
        "memory",
        "set",
        "--scope",
        "workspace",
        "--key",
        "shared-rule",
        "--category",
        "rule",
        "--basis",
        "ruling",
        "--summary",
        "Base",
        "--body",
        "v0",
        "--by",
        "jarvis",
        "--json",
    ]);
    let _ = run_json(base_cmd);
    let mut s1 = a.fl();
    s1.args(["state", "sync", "--json"]);
    let _ = run_json(s1);
    let mut s2 = b.fl();
    s2.args(["state", "sync", "--json"]);
    let _ = run_json(s2);

    // Concurrent divergent edits on A and B.
    let mut edit_a = a.fl();
    edit_a.args([
        "state",
        "memory",
        "set",
        "--scope",
        "workspace",
        "--key",
        "shared-rule",
        "--category",
        "rule",
        "--basis",
        "ruling",
        "--summary",
        "From A",
        "--body",
        "vA",
        "--by",
        "jarvis",
        "--json",
    ]);
    let entry_a = run_json(edit_a);

    let mut edit_b = b.fl();
    edit_b.args([
        "state",
        "memory",
        "set",
        "--scope",
        "workspace",
        "--key",
        "shared-rule",
        "--category",
        "rule",
        "--basis",
        "ruling",
        "--summary",
        "From B",
        "--body",
        "vB",
        "--by",
        "ferris",
        "--json",
    ]);
    let _entry_b = run_json(edit_b);

    let mut push_a = a.fl();
    push_a.args(["state", "sync", "--json"]);
    let _ = run_json(push_a);

    let mut push_b = b.fl();
    push_b.args(["state", "sync", "--json"]);
    let rep_b = run_json(push_b);
    assert_eq!(rep_b["active_conflicts"], 1);

    // Regular writes on B are refused until conflict is resolved.
    let mut blocked = b.fl();
    blocked.args([
        "state",
        "memory",
        "set",
        "--scope",
        "workspace",
        "--key",
        "shared-rule",
        "--category",
        "rule",
        "--basis",
        "ruling",
        "--summary",
        "Blocked",
        "--body",
        "vX",
        "--by",
        "ferris",
    ]);
    assert!(!blocked.output().unwrap().status.success());

    // Resolve keeping A's head_revision and sync.
    let keep_rev = entry_a["head_revision"].as_str().unwrap();
    let mut resolve = b.fl();
    resolve.args([
        "state",
        "resolve",
        "shared-rule",
        "--scope",
        "workspace",
        "--concern",
        "memory",
        "--keep",
        keep_rev,
        "--by",
        "ferris",
        "--json",
    ]);
    let resolved = run_json(resolve);
    assert_eq!(resolved["payload"]["memory"]["summary"], "From A");

    let mut sync_b_final = b.fl();
    sync_b_final.args(["state", "sync", "--json"]);
    let rep_b_final = run_json(sync_b_final);
    assert_eq!(rep_b_final["active_conflicts"], 0);
}

#[test]
fn offline_same_key_state_id_collision_aliases_to_remote() {
    let (_tmp, _bare, a, b) = setup_two_machines(false);

    let mut create_a = a.fl();
    create_a.args([
        "state",
        "runtime",
        "set",
        "--scope",
        "workspace",
        "--key",
        "lease",
        "--value",
        "{\"holder\":\"a\"}",
        "--merge",
        "last_writer",
        "--by",
        "jarvis",
        "--json",
    ]);
    let entry_a = run_json(create_a);

    let mut create_b = b.fl();
    create_b.args([
        "state",
        "runtime",
        "set",
        "--scope",
        "workspace",
        "--key",
        "lease",
        "--value",
        "{\"holder\":\"b\"}",
        "--merge",
        "last_writer",
        "--by",
        "ferris",
        "--json",
    ]);
    let entry_b = run_json(create_b);
    assert_ne!(entry_a["id"], entry_b["id"]);

    let mut sync_a = a.fl();
    sync_a.args(["state", "sync", "--json"]);
    let _ = run_json(sync_a);

    let mut sync_b = b.fl();
    sync_b.args(["state", "sync", "--json"]);
    let _ = run_json(sync_b);

    // Querying B by its old local StateId resolves via alias to A's canonical StateId.
    let old_b_id = entry_b["id"].as_str().unwrap();
    let mut get_b = b.fl();
    get_b.args(["state", "get", old_b_id, "--json"]);
    let resolved_b = run_json(get_b);
    assert_eq!(resolved_b["id"], entry_a["id"]);
}

#[test]
fn runtime_append_and_last_writer_merge_modes_converge_without_conflict() {
    let (_tmp, _bare, a, b) = setup_two_machines(false);

    let mut base = a.fl();
    base.args([
        "state",
        "runtime",
        "set",
        "--scope",
        "workspace",
        "--key",
        "events",
        "--value",
        "[\"init\"]",
        "--merge",
        "append",
        "--by",
        "jarvis",
        "--json",
    ]);
    let _ = run_json(base);
    let mut s1 = a.fl();
    s1.args(["state", "sync", "--json"]);
    let _ = run_json(s1);
    let mut s2 = b.fl();
    s2.args(["state", "sync", "--json"]);
    let _ = run_json(s2);

    let mut up_a = a.fl();
    up_a.args([
        "state",
        "runtime",
        "set",
        "--scope",
        "workspace",
        "--key",
        "events",
        "--value",
        "[\"init\",\"from-a\"]",
        "--merge",
        "append",
        "--by",
        "jarvis",
        "--json",
    ]);
    let _ = run_json(up_a);

    let mut up_b = b.fl();
    up_b.args([
        "state",
        "runtime",
        "set",
        "--scope",
        "workspace",
        "--key",
        "events",
        "--value",
        "[\"init\",\"from-b\"]",
        "--merge",
        "append",
        "--by",
        "ferris",
        "--json",
    ]);
    let _ = run_json(up_b);

    let mut push_a = a.fl();
    push_a.args(["state", "sync", "--json"]);
    let _ = run_json(push_a);

    let mut push_b = b.fl();
    push_b.args(["state", "sync", "--json"]);
    let rep_b = run_json(push_b);
    assert_eq!(rep_b["active_conflicts"], 0);

    let mut get_b = b.fl();
    get_b.args([
        "state",
        "get",
        "events",
        "--scope",
        "workspace",
        "--concern",
        "runtime",
        "--json",
    ]);
    let merged = run_json(get_b);
    assert_eq!(
        merged["payload"]["runtime"]["value"],
        serde_json::json!(["init", "from-b", "from-a"])
    );
}

#[test]
fn tamper_and_branch_deletion_are_refused_by_sync_and_verify() {
    let (_tmp, bare, a, _b) = setup_two_machines(false);

    let mut set = a.fl();
    set.args([
        "state",
        "handoff",
        "set",
        "--scope",
        "workspace",
        "--summary",
        "Untampered",
        "--by",
        "jarvis",
        "--json",
    ]);
    let _ = run_json(set);

    let mut sync = a.fl();
    sync.args(["state", "sync", "--json"]);
    let rep = run_json(sync);
    let tip = rep["committed_oid"].as_str().unwrap().to_string();

    // 1. Tamper with an existing segment line in a fast-forward child commit on `fl/state`.
    let ls_tree = Command::new("git")
        .arg("-C")
        .arg(&bare)
        .args(["ls-tree", "-r", &tip])
        .output()
        .unwrap();
    let ls_text = String::from_utf8(ls_tree.stdout).unwrap();
    let seg_path = ls_text
        .lines()
        .filter_map(|l| l.split_once('\t').map(|(_, p)| p.trim()))
        .find(|p| p.ends_with(".jsonl"))
        .unwrap();

    let mut hash_in = Command::new("git")
        .arg("-C")
        .arg(&bare)
        .args(["hash-object", "-w", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    hash_in
        .stdin
        .take()
        .unwrap()
        .write_all(b"{\"tampered\":true}\n")
        .unwrap();
    let bad_blob = String::from_utf8(hash_in.wait_with_output().unwrap().stdout)
        .unwrap()
        .trim()
        .to_string();

    let mut mktree_leaf = Command::new("git")
        .arg("-C")
        .arg(&bare)
        .args(["mktree"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let parts: Vec<&str> = seg_path.split('/').collect();
    // seg_path is `revisions/<hash32>/0000.jsonl`
    mktree_leaf
        .stdin
        .take()
        .unwrap()
        .write_all(format!("100644 blob {bad_blob}\t{}\n", parts[2]).as_bytes())
        .unwrap();
    let leaf_tree = String::from_utf8(mktree_leaf.wait_with_output().unwrap().stdout)
        .unwrap()
        .trim()
        .to_string();

    let mut mktree_mid = Command::new("git")
        .arg("-C")
        .arg(&bare)
        .args(["mktree"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    mktree_mid
        .stdin
        .take()
        .unwrap()
        .write_all(format!("040000 tree {leaf_tree}\t{}\n", parts[1]).as_bytes())
        .unwrap();
    let mid_tree = String::from_utf8(mktree_mid.wait_with_output().unwrap().stdout)
        .unwrap()
        .trim()
        .to_string();

    let mut mktree_root = Command::new("git")
        .arg("-C")
        .arg(&bare)
        .args(["mktree"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    mktree_root
        .stdin
        .take()
        .unwrap()
        .write_all(format!("040000 tree {mid_tree}\t{}\n", parts[0]).as_bytes())
        .unwrap();
    let bad_tree = String::from_utf8(mktree_root.wait_with_output().unwrap().stdout)
        .unwrap()
        .trim()
        .to_string();

    let bad_commit = String::from_utf8(
        Command::new("git")
            .arg("-C")
            .arg(&bare)
            .env("GIT_AUTHOR_NAME", "Attacker")
            .env("GIT_AUTHOR_EMAIL", "attacker@fl.local")
            .env("GIT_COMMITTER_NAME", "Attacker")
            .env("GIT_COMMITTER_EMAIL", "attacker@fl.local")
            .args(["commit-tree", &bad_tree, "-p", &tip, "-m", "tamper"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();

    assert!(
        Command::new("git")
            .arg("-C")
            .arg(&bare)
            .args(["update-ref", "refs/heads/fl/state", &bad_commit])
            .status()
            .unwrap()
            .success()
    );

    let mut verify_tampered = a.fl();
    verify_tampered.args(["state", "verify"]);
    assert!(!verify_tampered.output().unwrap().status.success());

    let mut sync_tampered = a.fl();
    sync_tampered.args(["state", "sync"]);
    assert!(!sync_tampered.output().unwrap().status.success());

    // 2. Delete refs/heads/fl/state on the bare remote after a prior sync -> verify & sync fail.
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(&bare)
            .args(["branch", "-D", "fl/state"])
            .status()
            .unwrap()
            .success()
    );

    let mut verify_after_del = a.fl();
    verify_after_del.args(["state", "verify"]);
    assert!(!verify_after_del.output().unwrap().status.success());

    let mut sync_after_del = a.fl();
    sync_after_del.args(["state", "sync"]);
    assert!(!sync_after_del.output().unwrap().status.success());
}

#[test]
fn sensitive_entry_is_refused_on_non_private_remote() {
    let (_tmp, bare, a, _b) = setup_two_machines(false);

    let mut set = a.fl();
    set.args([
        "state",
        "runtime",
        "set",
        "--scope",
        "workspace",
        "--key",
        "secret-key",
        "--value",
        "{\"token\":\"redact\"}",
        "--sensitive",
        "--by",
        "jarvis",
        "--json",
    ]);
    let _ = run_json(set);

    let mut sync = a.fl();
    sync.args(["state", "sync"]);
    let out = sync.output().unwrap();
    assert!(!out.status.success());

    let ls = Command::new("git")
        .arg("-C")
        .arg(&bare)
        .args(["ls-remote", "--refs", ".", "refs/heads/fl/state"])
        .output()
        .unwrap();
    assert!(ls.stdout.is_empty());
}

#[test]
fn render_orders_tiers_respects_byte_cap_omits_sensitive_and_refuses_symlinks() {
    let (tmp, _bare, a, _b) = setup_two_machines(false);

    // Register agent and verify exit code 2 on collision without --replace.
    let mut reg1 = a.fl();
    reg1.args([
        "agent",
        "register",
        "Ferris",
        "--project",
        "ferroloop",
        "--role",
        "lead",
        "--create-area",
        "--by",
        "owner",
        "--json",
    ]);
    let _ = run_json(reg1);

    let mut reg_collision = a.fl();
    reg_collision.args([
        "agent",
        "register",
        "Ferris",
        "--project",
        "ferroloop",
        "--role",
        "lead",
        "--by",
        "owner",
    ]);
    assert_eq!(reg_collision.output().unwrap().status.code(), Some(2));

    // Add normal, warning, and sensitive memories.
    let mut m_note = a.fl();
    m_note.args([
        "state",
        "memory",
        "set",
        "--scope",
        "workspace",
        "--key",
        "aaa-note",
        "--category",
        "note",
        "--basis",
        "measured",
        "--summary",
        "Normal note",
        "--body",
        "body",
        "--by",
        "ferris",
        "--json",
    ]);
    let _ = run_json(m_note);

    let mut m_warn = a.fl();
    m_warn.args([
        "state",
        "memory",
        "set",
        "--scope",
        "workspace",
        "--key",
        "zzz-warn",
        "--category",
        "warning",
        "--basis",
        "ruling",
        "--summary",
        "Critical warning",
        "--body",
        "body",
        "--by",
        "ferris",
        "--json",
    ]);
    let _ = run_json(m_warn);

    let mut m_sens = a.fl();
    m_sens.args([
        "state",
        "memory",
        "set",
        "--scope",
        "workspace",
        "--key",
        "secret-mem",
        "--category",
        "warning",
        "--basis",
        "ruling",
        "--summary",
        "Hidden summary",
        "--body",
        "body",
        "--sensitive",
        "--by",
        "ferris",
        "--json",
    ]);
    let _ = run_json(m_sens);

    let mut render = a.fl();
    render.args(["state", "render", "--max-bytes", "600"]);
    let out = render.output().unwrap();
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.len() <= 600, "len {} > 600:\n{text}", text.len());
    assert!(!text.contains("Hidden summary"));
    assert!(text.find("zzz-warn").unwrap() < text.find("aaa-note").unwrap());

    // Refuse symlinked --out path.
    let real_target = tmp.path().join("real.md");
    std::fs::write(&real_target, "orig").unwrap();
    let symlink_path = tmp.path().join("symlink.md");
    std::os::unix::fs::symlink(&real_target, &symlink_path).unwrap();

    let mut render_sym = a.fl();
    render_sym.args(["state", "render", "--out", symlink_path.to_str().unwrap()]);
    assert!(!render_sym.output().unwrap().status.success());
}

#[test]
fn mcp_stdio_round_trip_and_releases_redb_lock_between_calls() {
    let (_tmp, _bare, a, _b) = setup_two_machines(false);

    let mut child = a
        .fl()
        .args(["state", "mcp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();

    let mut child_in = child.stdin.take().unwrap();
    let mut child_out = BufReader::new(child.stdout.take().unwrap());

    let mut rpc = |req: Value| -> Value {
        writeln!(child_in, "{}", serde_json::to_string(&req).unwrap()).unwrap();
        child_in.flush().unwrap();
        let mut line = String::new();
        child_out.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    };

    let init_resp = rpc(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {}
    }));
    assert_eq!(init_resp["result"]["serverInfo"]["name"], "fl-state-mcp");

    let list_resp = rpc(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {}
    }));
    assert_eq!(list_resp["result"]["tools"].as_array().unwrap().len(), 24);
    assert!(
        list_resp["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"] == "fl_memory_search"),
        "fl_memory_search must be advertised: {list_resp}"
    );
    for tool in [
        "fl_code_def",
        "fl_code_search",
        "fl_code_outline",
        "fl_batch_search",
    ] {
        assert!(
            list_resp["result"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["name"] == tool),
            "{tool} must be advertised: {list_resp}"
        );
    }

    let reg_resp = rpc(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "agent_register",
            "arguments": {
                "name": "Ferris",
                "project": "ferroloop",
                "role": "lead",
                "create_area": true,
                "by": "owner"
            }
        }
    }));
    assert_eq!(reg_resp["result"]["isError"], false);

    let mem_resp = rpc(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "tools/call",
        "params": {
            "name": "state_set_memory",
            "arguments": {
                "scope": "workspace",
                "key": "mcp-rule",
                "category": "rule",
                "basis": "ruling",
                "summary": "Written via MCP",
                "body": "MCP body",
                "pinned": true,
                "by": "ferris"
            }
        }
    }));
    assert_eq!(mem_resp["result"]["isError"], false);

    // While `fl state mcp` is alive and waiting on stdin, a separate `fl state list`
    // process must succeed without blocking on the redb file lock.
    let mut cli_list = a.fl();
    cli_list.args(["state", "list", "--json"]);
    let listed = run_json(cli_list);
    assert_eq!(listed.as_array().unwrap().len(), 1);

    let render_resp = rpc(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 5,
        "method": "tools/call",
        "params": {
            "name": "state_render",
            "arguments": {}
        }
    }));
    assert_eq!(render_resp["result"]["isError"], false);
    assert!(
        render_resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Written via MCP")
    );

    drop(child_in);
    let status = child.wait().unwrap();
    assert!(status.success());
}

#[test]
fn mcp_stdio_fl_state_checkpoint_records_git_state() {
    let (_tmp, _bare, a, _b) = setup_two_machines(false);
    std::fs::write(a.repo_dir.join("untracked.txt"), "new\n").unwrap();

    let mut child = a
        .fl()
        .args(["state", "mcp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut child_in = child.stdin.take().unwrap();
    let mut child_out = BufReader::new(child.stdout.take().unwrap());
    let mut rpc = |req: Value| -> Value {
        writeln!(child_in, "{}", serde_json::to_string(&req).unwrap()).unwrap();
        child_in.flush().unwrap();
        let mut line = String::new();
        child_out.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    };

    let init_resp = rpc(serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}
    }));
    assert_eq!(init_resp["result"]["serverInfo"]["name"], "fl-state-mcp");

    let resp = rpc(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "fl_state_checkpoint",
            "arguments": { "by": "tester" }
        }
    }));
    assert_eq!(resp["result"]["isError"], false, "{resp}");
    let report: Value =
        serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(report["entry"]["slug"], "checkpoint-tester", "{report}");
    assert_eq!(report["entry"]["concern"], "handoff", "{report}");
    assert_eq!(report["branch"], "main", "{report}");
    assert_eq!(report["dirty_files"], 1, "{report}");

    drop(child_in);
    assert!(child.wait().unwrap().success());
}

fn seed_search_entries(a: &Machine) {
    for (key, summary, body) in [
        (
            "search-weak",
            "Weak note",
            "quixotic mention in passing only",
        ),
        (
            "search-strong",
            "Strong note",
            "quixotic quixotic quixotic ranking evidence quixotic",
        ),
    ] {
        let mut set = a.fl();
        set.args([
            "state",
            "memory",
            "set",
            "--scope",
            "workspace",
            "--key",
            key,
            "--category",
            "note",
            "--basis",
            "inference",
            "--summary",
            summary,
            "--body",
            body,
            "--by",
            "test",
            "--json",
        ]);
        run_json(set);
    }
}

#[test]
fn cli_state_search_json_via_binary_returns_ranked_hits() {
    let (_tmp, _bare, a, _b) = setup_two_machines(false);
    seed_search_entries(&a);

    let mut search = a.fl();
    search.args(["state", "search", "quixotic", "--json"]);
    let value = run_json(search);

    assert_eq!(value["query"], "quixotic", "{value}");
    assert_eq!(value["total_hits"], 2, "{value}");
    let hits = value["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 2, "{value}");
    assert_eq!(hits[0]["entry"]["slug"], "search-strong", "{value}");
    assert!(hits[0]["score_millis"].as_u64().unwrap() > 0, "{value}");
}

#[test]
fn mcp_stdio_fl_memory_search_returns_ranked_hits() {
    let (_tmp, _bare, a, _b) = setup_two_machines(false);
    seed_search_entries(&a);

    let mut child = a
        .fl()
        .args(["state", "mcp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut child_in = child.stdin.take().unwrap();
    let mut child_out = BufReader::new(child.stdout.take().unwrap());
    let mut rpc = |req: Value| -> Value {
        writeln!(child_in, "{}", serde_json::to_string(&req).unwrap()).unwrap();
        child_in.flush().unwrap();
        let mut line = String::new();
        child_out.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    };

    let init_resp = rpc(serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}
    }));
    assert_eq!(init_resp["result"]["serverInfo"]["name"], "fl-state-mcp");

    let search_resp = rpc(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "fl_memory_search",
            "arguments": { "query": "quixotic" }
        }
    }));
    assert_eq!(search_resp["result"]["isError"], false, "{search_resp}");
    let text = search_resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    let value: Value = serde_json::from_str(text).unwrap();
    assert_eq!(value["query"], "quixotic", "{value}");
    assert_eq!(value["total_hits"], 2, "{value}");
    let hits = value["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 2, "{value}");
    assert_eq!(hits[0]["entry"]["slug"], "search-strong", "{value}");
    assert!(hits[0]["score_millis"].as_u64().unwrap() > 0, "{value}");

    drop(child_in);
    assert!(child.wait().unwrap().success());
}

#[test]
fn mcp_stdio_code_def_and_batch_search_record_token_gain() {
    let (_tmp, _bare, a, _b) = setup_two_machines(false);
    std::fs::create_dir_all(a.repo_dir.join("src")).unwrap();
    let mut code = String::from("pub fn widget_alpha() {}\npub struct WidgetBeta;\n");
    for i in 0..40 {
        code.push_str(&format!("// filler line {i} padding the file out\n"));
    }
    std::fs::write(a.repo_dir.join("src/lib.rs"), code).unwrap();

    let mut child = a
        .fl()
        .args(["state", "mcp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut child_in = child.stdin.take().unwrap();
    let mut child_out = BufReader::new(child.stdout.take().unwrap());
    let mut rpc = |req: Value| -> Value {
        writeln!(child_in, "{}", serde_json::to_string(&req).unwrap()).unwrap();
        child_in.flush().unwrap();
        let mut line = String::new();
        child_out.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    };

    let init_resp = rpc(serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}
    }));
    assert_eq!(init_resp["result"]["serverInfo"]["name"], "fl-state-mcp");

    let def_resp = rpc(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "fl_code_def",
            "arguments": { "q": "widget_alpha" }
        }
    }));
    assert_eq!(def_resp["result"]["isError"], false, "{def_resp}");
    let def: Value =
        serde_json::from_str(def_resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(def["query"], "widget_alpha", "{def}");
    assert_eq!(def["kind"], "def", "{def}");
    assert_eq!(def["symbols"][0]["name"], "widget_alpha", "{def}");
    assert_eq!(def["symbols"][0]["path"], "src/lib.rs", "{def}");
    assert!(
        def["tokens_saved_this_query"].as_u64().unwrap() > 0,
        "{def}"
    );
    assert_eq!(def["token_gain"]["queries_count"], 1, "{def}");

    let batch_resp = rpc(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "fl_batch_search",
            "arguments": {
                "queries": [
                    { "kind": "def", "q": "WidgetBeta" },
                    { "kind": "outline", "q": "src/lib.rs" }
                ]
            }
        }
    }));
    assert_eq!(batch_resp["result"]["isError"], false, "{batch_resp}");
    let batch: Value =
        serde_json::from_str(batch_resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(batch["results"].as_array().unwrap().len(), 2, "{batch}");
    assert_eq!(batch["results"][0]["query"], "WidgetBeta", "{batch}");
    assert_eq!(batch["results"][0]["kind"], "def", "{batch}");
    assert_eq!(
        batch["results"][0]["symbols"][0]["name"], "WidgetBeta",
        "{batch}"
    );
    assert_eq!(batch["results"][1]["kind"], "outline", "{batch}");
    assert_eq!(
        batch["results"][1]["symbols"].as_array().unwrap().len(),
        2,
        "{batch}"
    );
    assert_eq!(batch["token_gain"]["queries_count"], 3, "{batch}");

    let bad_resp = rpc(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "tools/call",
        "params": {
            "name": "fl_code_search",
            "arguments": { "q": "   " }
        }
    }));
    assert_eq!(bad_resp["result"]["isError"], true, "{bad_resp}");

    drop(child_in);
    assert!(child.wait().unwrap().success());
}
