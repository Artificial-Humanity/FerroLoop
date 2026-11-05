//! A teardown leaves nothing for a boot restore: the stopped session is also gone from the newest
//! tmux-resurrect save. Every test uses its own home with a fake runtime; no real tmux, resurrect
//! directory or route table is read.

use super::*;
use crate::restore_source::{RestoreSourceOutcome, prune_session_in};
use fl_core::MemStore;
use std::fs;
use tempfile::TempDir;

const SAVE: &str = "tmux_resurrect_20261105T202724.txt";

/// A save like tmux-resurrect writes: tab separated, session name in field 2.
fn save_text() -> String {
    [
        "pane\tFeira\t0\t1\t:*\t0\t✳ Feira\t:/home/x\t1\tclaude\t:claude",
        "pane\tFiona\t0\t1\t:*\t0\tFiona\t:/home/x\t1\tcodex\t:codex resume abc",
        "pane\tFiona\t1\t0\t:\t0\tFiona\t:/home/x\t1\tzsh\t:",
        "pane\tFionaTwo\t0\t1\t:*\t0\tother\t:/home/x\t1\tzsh\t:",
        "window\tFeira\t0\t:claude\t1\t:*\tb25f,80x24,0,0,2\t:",
        "window\tFiona\t0\t:codex\t1\t:*\tb264,80x24,0,0,7\t:",
        "grouped_session\tFiona-view\tFiona\t:0\t:0",
        "state\tFiona\tFeira",
        "",
    ]
    .join("\n")
}

fn resurrect_dir(home: &TempDir) -> PathBuf {
    let dir = home.path().join(".local/share/tmux/resurrect");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(SAVE), save_text()).unwrap();
    std::os::unix::fs::symlink(SAVE, dir.join("last")).unwrap();
    dir
}

fn spun(store: &MemStore, env: &LifecycleEnv<'_>, persona: &str) -> SessionTarget {
    let req = SpinUpRequest {
        persona: persona.into(),
        project: "FerroLoop".into(),
        role: "assistant".into(),
        vendor: "codex".into(),
        ..SpinUpRequest::default()
    };
    spin_up(store, env, &req, "owner", Caller::Owner).unwrap();
    SessionTarget::resolve(store, &format!("codex:{persona}"), None).unwrap()
}

#[test]
fn prune_removes_every_line_that_names_the_session_and_keeps_a_backup() {
    let home = TempDir::new().unwrap();
    let dir = resurrect_dir(&home);
    let out = prune_session_in(&dir, "Fiona");
    assert_eq!(
        out,
        RestoreSourceOutcome::Pruned {
            file: SAVE.into(),
            lines: 5
        }
    );
    let left = fs::read_to_string(dir.join(SAVE)).unwrap();
    assert!(
        !left.contains("\tFiona\t") && !left.contains("\tFiona\n"),
        "{left}"
    );
    assert!(left.contains("pane\tFeira\t") && left.contains("window\tFeira\t"));
    assert!(
        left.contains("pane\tFionaTwo\t"),
        "a longer name is another session"
    );
    assert_eq!(
        fs::read_to_string(dir.join(format!("{SAVE}.bak"))).unwrap(),
        save_text()
    );
    assert_eq!(fs::read_link(dir.join("last")).unwrap(), Path::new(SAVE));
}

#[test]
fn prune_is_idempotent_and_leaves_the_file_alone_when_clean() {
    let home = TempDir::new().unwrap();
    let dir = resurrect_dir(&home);
    prune_session_in(&dir, "Fiona");
    let after_first = fs::read(dir.join(SAVE)).unwrap();
    fs::remove_file(dir.join(format!("{SAVE}.bak"))).unwrap();
    assert_eq!(
        prune_session_in(&dir, "Fiona"),
        RestoreSourceOutcome::Clean { file: SAVE.into() }
    );
    assert_eq!(fs::read(dir.join(SAVE)).unwrap(), after_first);
    assert!(
        !dir.join(format!("{SAVE}.bak")).exists(),
        "no rewrite, no new backup"
    );
}

#[test]
fn prune_uses_the_newest_file_when_there_is_no_last_link_and_reports_no_save() {
    let home = TempDir::new().unwrap();
    let dir = home.path().join("resurrect");
    assert_eq!(
        prune_session_in(&dir, "Fiona"),
        RestoreSourceOutcome::NoSave
    );
    fs::create_dir_all(&dir).unwrap();
    assert_eq!(
        prune_session_in(&dir, "Fiona"),
        RestoreSourceOutcome::NoSave
    );
    fs::write(
        dir.join("tmux_resurrect_20261105T100000.txt"),
        "pane\tFiona\t0\n",
    )
    .unwrap();
    fs::write(dir.join(SAVE), save_text()).unwrap();
    assert!(matches!(
        prune_session_in(&dir, "Fiona"),
        RestoreSourceOutcome::Pruned { ref file, .. } if file == SAVE
    ));
    assert_eq!(
        fs::read_to_string(dir.join("tmux_resurrect_20261105T100000.txt")).unwrap(),
        "pane\tFiona\t0\n",
        "an older save is not the restore source"
    );
}

#[test]
fn prune_failure_is_reported_not_raised() {
    let home = TempDir::new().unwrap();
    let dir = home.path().join("resurrect");
    fs::create_dir_all(&dir).unwrap();
    // A directory where the save should be a file: it cannot be read.
    fs::create_dir(dir.join(SAVE)).unwrap();
    fs::write(dir.join("tmux_resurrect_20261105T000000.txt"), "").unwrap();
    assert!(matches!(
        prune_session_in(&dir, "Fiona"),
        RestoreSourceOutcome::Failed { .. }
    ));
}

#[test]
fn teardown_prunes_the_stopped_session_from_the_restore_source() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let dir = resurrect_dir(&home);
    let fake = FakeRuntime::default();
    let env = fake.env(home.path());
    let target = spun(&store, &env, "fiona");
    assert!(fake.is_live("Fiona"));

    let out = teardown_session(&store, &env, &target, "owner", Caller::Owner, |_| {}).unwrap();
    assert!(out.runtime_stopped);
    assert!(!fake.is_live("Fiona"));
    assert_eq!(
        out.restore_source,
        Some(RestoreSourceOutcome::Pruned {
            file: SAVE.into(),
            lines: 5
        })
    );
    let left = fs::read_to_string(dir.join(SAVE)).unwrap();
    assert!(
        !left.contains("Fiona\t") && left.contains("Feira"),
        "{left}"
    );

    // Repeated: nothing more to prune, and still a success.
    let again = teardown_session(&store, &env, &target, "owner", Caller::Owner, |_| {}).unwrap();
    assert_eq!(again.status, "decommissioned");
}

#[test]
fn teardown_succeeds_when_the_restore_source_cannot_be_pruned() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    // The newest save is a directory: reading it fails.
    let dir = home.path().join(".local/share/tmux/resurrect");
    fs::create_dir_all(dir.join(SAVE)).unwrap();
    let fake = FakeRuntime::default();
    let env = fake.env(home.path());
    let target = spun(&store, &env, "fiona");

    let out = teardown_session(&store, &env, &target, "owner", Caller::Owner, |_| {}).unwrap();
    assert_eq!(out.status, "decommissioned");
    assert!(out.runtime_stopped);
    assert!(
        matches!(
            out.restore_source,
            Some(RestoreSourceOutcome::Failed { .. })
        ),
        "{:?}",
        out.restore_source
    );
    assert_eq!(
        store.agent_by_name("fiona").unwrap().unwrap().status,
        AgentStatus::Retired
    );
}

#[test]
fn teardown_with_no_resurrect_directory_reports_no_save() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let fake = FakeRuntime::default();
    let env = fake.env(home.path());
    let target = spun(&store, &env, "fiona");
    let out = teardown_session(&store, &env, &target, "owner", Caller::Owner, |_| {}).unwrap();
    assert_eq!(out.restore_source, Some(RestoreSourceOutcome::NoSave));
}

#[test]
fn teardown_that_stops_nothing_leaves_the_restore_source_alone() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let dir = resurrect_dir(&home);
    // Runtime calls off: the session (if any) keeps running and keeps being saved.
    let off = LifecycleEnv {
        home_override: Some(home.path()),
        spawn_runtime: false,
        runtime: &TmuxRuntime,
        key_probe: &ProvisionedKeys,
        workspace_root: None,
    };
    let target = spun(&store, &off, "fiona");
    let out = teardown_session(&store, &off, &target, "owner", Caller::Owner, |_| {}).unwrap();
    assert_eq!(out.restore_source, None);
    assert_eq!(fs::read_to_string(dir.join(SAVE)).unwrap(), save_text());
}
