//! A teardown leaves nothing for a boot restore: the stopped session is also gone from the newest
//! tmux-resurrect save. Every test uses its own home with a fake runtime; no real tmux, resurrect
//! directory or route table is read.

use super::*;
use crate::restore_source::{RestoreSourceOutcome, prune_session_in, prune_window_in};
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

#[test]
fn a_state_line_keeps_the_clients_other_session() {
    let home = TempDir::new().unwrap();
    let dir = resurrect_dir(&home);
    // The client's last session is Fiona, its current one is Feira.
    fs::write(
        dir.join(SAVE),
        "pane\tFiona\t0\nstate\tFeira\tFiona\nstate\tFiona\tFiona\n",
    )
    .unwrap();
    let out = prune_session_in(&dir, "Fiona");
    assert_eq!(
        out,
        RestoreSourceOutcome::Pruned {
            file: SAVE.into(),
            lines: 3
        }
    );
    assert_eq!(
        fs::read_to_string(dir.join(SAVE)).unwrap(),
        "state\tFeira\t\nstate\t\t\n",
        "only the field that names the session is emptied"
    );
    // A CRLF line keeps its ending.
    fs::write(dir.join(SAVE), "state\tFiona\tFeira\r\n").unwrap();
    prune_session_in(&dir, "Fiona");
    assert_eq!(
        fs::read_to_string(dir.join(SAVE)).unwrap(),
        "state\t\tFeira\r\n"
    );
}

#[test]
fn the_first_backup_is_kept_by_a_later_prune() {
    let home = TempDir::new().unwrap();
    let dir = resurrect_dir(&home);
    prune_session_in(&dir, "Fiona");
    assert!(matches!(
        prune_session_in(&dir, "Feira"),
        RestoreSourceOutcome::Pruned { .. }
    ));
    assert_eq!(
        fs::read_to_string(dir.join(format!("{SAVE}.bak"))).unwrap(),
        save_text(),
        "the content from before the first prune"
    );
}

#[test]
fn a_last_link_that_points_outside_the_directory_is_not_followed() {
    let home = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let dir = home.path().join("resurrect");
    fs::create_dir_all(&dir).unwrap();
    let older = "tmux_resurrect_20261105T100000.txt";
    let outer = outside.path().join(older);
    fs::write(&outer, save_text()).unwrap();
    // A same-named file inside the directory that is neither the link target nor the newest.
    fs::write(dir.join(older), save_text()).unwrap();
    fs::write(dir.join(SAVE), save_text()).unwrap();
    std::os::unix::fs::symlink(&outer, dir.join("last")).unwrap();

    assert!(matches!(
        prune_session_in(&dir, "Fiona"),
        RestoreSourceOutcome::Pruned { ref file, .. } if file == SAVE
    ));
    assert_eq!(fs::read_to_string(&outer).unwrap(), save_text());
    assert_eq!(fs::read_to_string(dir.join(older)).unwrap(), save_text());

    // The same for a relative link that climbs out of the directory.
    fs::remove_file(dir.join("last")).unwrap();
    fs::write(dir.join(SAVE), save_text()).unwrap();
    let rel = Path::new("..")
        .join("..")
        .join(outer.strip_prefix("/").unwrap());
    std::os::unix::fs::symlink(rel, dir.join("last")).unwrap();
    assert!(matches!(
        prune_session_in(&dir, "Fiona"),
        RestoreSourceOutcome::Pruned { ref file, .. } if file == SAVE
    ));
    assert_eq!(fs::read_to_string(&outer).unwrap(), save_text());
}

#[test]
fn a_last_link_inside_the_directory_is_followed_even_when_absolute() {
    let home = TempDir::new().unwrap();
    let dir = resurrect_dir(&home);
    let older = "tmux_resurrect_20261105T100000.txt";
    fs::write(dir.join(older), save_text()).unwrap();
    fs::remove_file(dir.join("last")).unwrap();
    std::os::unix::fs::symlink(dir.join(older), dir.join("last")).unwrap();
    assert!(matches!(
        prune_session_in(&dir, "Fiona"),
        RestoreSourceOutcome::Pruned { ref file, .. } if file == older
    ));
}

#[test]
fn teardown_prunes_a_session_the_hub_did_not_start_once_it_is_not_running() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let dir = resurrect_dir(&home);
    // Hand-launched: the session runs before the spin-up, so the spin-up does not own it.
    let fake = FakeRuntime::running(&["Fiona"]);
    let env = fake.env(home.path());
    let target = spun(&store, &env, "fiona");
    assert!(!record_runtime_spawned(&store, &target));
    // The session ended since (the person closed it), so nothing can save it again.
    fake.live.borrow_mut().remove("Fiona");

    let out = teardown_session(&store, &env, &target, "owner", Caller::Owner, |_| {}).unwrap();
    assert!(!out.runtime_stopped, "it was not ours to stop");
    assert_eq!(
        out.restore_source,
        Some(RestoreSourceOutcome::Pruned {
            file: SAVE.into(),
            lines: 5
        })
    );
    assert!(
        !fs::read_to_string(dir.join(SAVE))
            .unwrap()
            .contains("Fiona\t")
    );
}

#[test]
fn teardown_leaves_a_running_session_the_hub_did_not_start_alone() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let dir = resurrect_dir(&home);
    let fake = FakeRuntime::running(&["Fiona"]);
    let env = fake.env(home.path());
    let target = spun(&store, &env, "fiona");
    assert!(!record_runtime_spawned(&store, &target));

    let out = teardown_session(&store, &env, &target, "owner", Caller::Owner, |_| {}).unwrap();
    assert_eq!(out.status, "decommissioned");
    assert!(!out.runtime_stopped);
    assert_eq!(out.restore_source, Some(RestoreSourceOutcome::Running));
    assert!(
        fake.is_live("Fiona"),
        "a session the hub did not start is not killed"
    );
    assert!(
        !fake.calls.borrow().iter().any(|c| c.starts_with("kill")),
        "{:?}",
        fake.calls.borrow()
    );
    assert_eq!(
        fs::read_to_string(dir.join(SAVE)).unwrap(),
        save_text(),
        "a running session is not pruned"
    );
    let json = serde_json::to_value(&out).unwrap();
    assert_eq!(json["restore_source"]["outcome"], "running");
}

fn record_runtime_spawned(store: &MemStore, target: &SessionTarget) -> bool {
    record_for_target(store, target).unwrap().runtime_spawned
}

/// A save of the shared Muse session with three windows at indexes 1, 3 and 4 (a `base-index` of
/// 1 and a closed window), the last with two panes, next to an ordinary session and a grouped
/// view of the Muse session. The window index differs from the window position.
fn muse_save() -> String {
    [
        "pane\tFeira\t0\t1\t:*\t0\t✳ Feira\t:/home/x\t1\tclaude\t:claude",
        "pane\tmuse\t1\t0\t:-\t0\tada\t:/home/x\t1\tmuse\t:muse",
        "pane\tmuse\t3\t1\t:*\t0\tbob\t:/home/x\t1\tmuse\t:muse",
        "pane\tmuse\t4\t0\t:\t0\tcy\t:/home/x\t1\tmuse\t:muse",
        "pane\tmuse\t4\t0\t:\t1\tcy-shell\t:/home/x\t0\tzsh\t:",
        "window\tFeira\t0\t:claude\t1\t:*\tb25f,80x24,0,0,2\t:",
        "window\tmuse\t1\t:ada\t0\t:-\tb264,80x24,0,0,7\t:",
        "window\tmuse\t3\t:bob\t1\t:*\tb265,80x24,0,0,8\t:",
        "window\tmuse\t4\t:cy\t0\t:\tb266,80x24,0,0,9\t:",
        "grouped_session\tmuse-view\tmuse\t:3\t:3",
        "state\tmuse\tFeira",
        "",
    ]
    .join("\n")
}

fn muse_dir(home: &TempDir) -> PathBuf {
    let dir = home.path().join(".local/share/tmux/resurrect");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(SAVE), muse_save()).unwrap();
    std::os::unix::fs::symlink(SAVE, dir.join("last")).unwrap();
    dir
}

#[test]
fn a_window_prune_drops_the_window_and_its_panes_and_keeps_the_sibling_windows() {
    let home = TempDir::new().unwrap();
    let dir = muse_dir(&home);
    let out = prune_window_in(&dir, "muse", "bob");
    // The window line and the one pane line of index 3; not the windows at index 1 and 4.
    assert_eq!(
        out,
        RestoreSourceOutcome::Pruned {
            file: SAVE.into(),
            lines: 2
        }
    );
    let left = fs::read_to_string(dir.join(SAVE)).unwrap();
    let expected: String = muse_save()
        .lines()
        .filter(|l| !l.starts_with("window\tmuse\t3\t") && !l.starts_with("pane\tmuse\t3\t"))
        .map(|l| format!("{l}\n"))
        .collect();
    assert_eq!(left, expected);
    // The sibling windows keep their pane lines (both panes of index 4), the session's state and
    // grouped lines stay.
    assert_eq!(left.matches("pane\tmuse\t4\t").count(), 2);
    assert!(left.contains("pane\tmuse\t1\t") && left.contains("window\tmuse\t4\t:cy"));
    assert!(
        left.contains("grouped_session\tmuse-view\tmuse") && left.contains("state\tmuse\tFeira")
    );
    assert_eq!(
        fs::read_to_string(dir.join(format!("{SAVE}.bak"))).unwrap(),
        muse_save()
    );
}

#[test]
fn a_window_prune_maps_the_name_to_the_index_and_drops_every_pane_of_the_window() {
    let home = TempDir::new().unwrap();
    let dir = muse_dir(&home);
    assert_eq!(
        prune_window_in(&dir, "muse", "cy"),
        RestoreSourceOutcome::Pruned {
            file: SAVE.into(),
            lines: 3
        }
    );
    let left = fs::read_to_string(dir.join(SAVE)).unwrap();
    assert!(!left.contains("cy"), "{left}");
    assert!(left.contains("pane\tmuse\t3\t") && left.contains("pane\tmuse\t1\t"));
    // A name that is a prefix of another is another window; a window of another session with the
    // same index and name is not touched.
    let home2 = TempDir::new().unwrap();
    let dir2 = muse_dir(&home2);
    assert!(matches!(
        prune_window_in(&dir2, "muse", "b"),
        RestoreSourceOutcome::Clean { .. }
    ));
    assert!(matches!(
        prune_window_in(&dir2, "Feira", "bob"),
        RestoreSourceOutcome::Clean { .. }
    ));
    assert_eq!(fs::read_to_string(dir2.join(SAVE)).unwrap(), muse_save());
    // The same name and index in two sessions: only the named session's lines go.
    fs::write(
        dir2.join(SAVE),
        "window\tmuse\t0\t:ada\t1\t:*\tL\t:\nwindow\tother\t0\t:ada\t1\t:*\tL\t:\n\
         pane\tmuse\t0\t1\t:*\t0\tt\t:/x\t1\tmuse\t:\npane\tother\t0\t1\t:*\t0\tt\t:/x\t1\tzsh\t:\n",
    )
    .unwrap();
    prune_window_in(&dir2, "muse", "ada");
    assert_eq!(
        fs::read_to_string(dir2.join(SAVE)).unwrap(),
        "window\tother\t0\t:ada\t1\t:*\tL\t:\npane\tother\t0\t1\t:*\t0\tt\t:/x\t1\tzsh\t:\n"
    );
}

#[test]
fn a_window_prune_is_idempotent() {
    let home = TempDir::new().unwrap();
    let dir = muse_dir(&home);
    assert!(matches!(
        prune_window_in(&dir, "muse", "ada"),
        RestoreSourceOutcome::Pruned { .. }
    ));
    let after_first = fs::read(dir.join(SAVE)).unwrap();
    fs::remove_file(dir.join(format!("{SAVE}.bak"))).unwrap();
    assert_eq!(
        prune_window_in(&dir, "muse", "ada"),
        RestoreSourceOutcome::Clean { file: SAVE.into() }
    );
    assert_eq!(fs::read(dir.join(SAVE)).unwrap(), after_first);
    assert!(!dir.join(format!("{SAVE}.bak")).exists());
}

fn muse_spin_up(store: &MemStore, env: &LifecycleEnv<'_>, persona: &str) -> SessionTarget {
    let req = SpinUpRequest {
        persona: persona.into(),
        project: "FerroLoop".into(),
        role: "assistant".into(),
        vendor: "muse".into(),
        ..SpinUpRequest::default()
    };
    spin_up(store, env, &req, "owner", Caller::Owner).unwrap();
    SessionTarget::resolve(store, &format!("muse:{persona}"), None).unwrap()
}

#[test]
fn a_killed_muse_window_is_pruned_alone_and_the_shared_session_only_when_it_is_gone() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let dir = muse_dir(&home);
    let fake = FakeRuntime::with_muse_windows(&[]);
    let env = fake.env(home.path());
    let ada = muse_spin_up(&store, &env, "ada");
    let bob = muse_spin_up(&store, &env, "bob");

    let out = teardown_session(&store, &env, &ada, "owner", Caller::Owner, |_| {}).unwrap();
    assert!(out.runtime_stopped);
    assert!(fake.is_live("muse"), "the shared session stays");
    assert_eq!(
        out.restore_source,
        Some(RestoreSourceOutcome::Pruned {
            file: SAVE.into(),
            lines: 2
        })
    );
    let left = fs::read_to_string(dir.join(SAVE)).unwrap();
    assert!(
        !left.contains(":ada") && !left.contains("pane\tmuse\t1\t"),
        "{left}"
    );
    assert!(
        left.contains(":bob") && left.contains(":cy") && left.contains("state\tmuse\tFeira"),
        "the shared session and its other windows are not pruned: {left}"
    );

    // The last window ends the session: now the session is gone and goes from the save.
    let out = teardown_session(&store, &env, &bob, "owner", Caller::Owner, |_| {}).unwrap();
    assert!(!fake.is_live("muse"));
    assert!(matches!(
        out.restore_source,
        Some(RestoreSourceOutcome::Pruned { .. })
    ));
    let left = fs::read_to_string(dir.join(SAVE)).unwrap();
    assert!(!left.contains("\tmuse\t"), "{left}");
    assert!(
        left.contains("pane\tFeira\t") && left.contains("state\t\tFeira"),
        "{left}"
    );
}

#[test]
fn a_muse_window_that_is_still_running_is_not_pruned() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let dir = muse_dir(&home);
    // A window of the name that someone else opened: not ours, so it is not killed, and it runs.
    let fake = FakeRuntime::with_muse_windows(&["ada"]);
    let env = fake.env(home.path());
    let ada = muse_spin_up(&store, &env, "ada");
    let out = teardown_session(&store, &env, &ada, "owner", Caller::Owner, |_| {}).unwrap();
    assert_eq!(out.restore_source, Some(RestoreSourceOutcome::Running));
    assert!(fake.is_live("muse:ada"));
    assert_eq!(fs::read_to_string(dir.join(SAVE)).unwrap(), muse_save());
}

/// A record that no spin-up wrote (`tmux_target` is `None`), as the owner's decommission of a
/// session found by discovery adopts it.
fn adopted(store: &MemStore, id: &str) -> SessionTarget {
    let target = SessionTarget::resolve(store, id, None).unwrap();
    let rec = record_for_target(store, &target).unwrap();
    assert!(!rec.hosted && rec.tmux_target.is_none() && !rec.runtime_spawned);
    target
}

#[test]
fn a_session_adopted_without_a_target_is_pruned_once_it_is_not_running() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let dir = resurrect_dir(&home);
    let fake = FakeRuntime::default();
    let env = fake.env(home.path());
    let target = adopted(&store, "codex:fiona");

    let out = teardown_session(&store, &env, &target, "owner", Caller::Owner, |_| {}).unwrap();
    assert!(out.runtime_stopped);
    assert_eq!(
        out.restore_source,
        Some(RestoreSourceOutcome::Pruned {
            file: SAVE.into(),
            lines: 5
        })
    );
    assert!(
        !fs::read_to_string(dir.join(SAVE))
            .unwrap()
            .contains("Fiona\t")
    );
}

#[test]
fn a_running_session_adopted_without_a_target_is_stopped_and_pruned() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let dir = resurrect_dir(&home);
    let fake = FakeRuntime::running(&["Fiona"]);
    let env = fake.env(home.path());
    let target = adopted(&store, "codex:fiona");

    let out = teardown_session(&store, &env, &target, "owner", Caller::Owner, |_| {}).unwrap();
    assert!(out.runtime_stopped);
    assert!(!fake.is_live("Fiona"));
    assert!(
        fake.calls
            .borrow()
            .iter()
            .any(|call| call == "kill-owner Fiona:codex")
    );
    assert_eq!(
        out.restore_source,
        Some(RestoreSourceOutcome::Pruned {
            file: SAVE.into(),
            lines: 5
        })
    );
    assert!(
        !fs::read_to_string(dir.join(SAVE))
            .unwrap()
            .contains("Fiona\t")
    );
}

#[test]
fn an_adopted_vendor_window_is_pruned_and_a_sibling_window_stays() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let dir = resurrect_dir(&home);
    let fake = FakeRuntime::default();
    fake.add_session_windows(&[("Fiona", "codex"), ("Fiona", "claude")]);
    let env = fake.env(home.path());
    let target = adopted(&store, "codex:fiona");

    let out = teardown_session(&store, &env, &target, "owner", Caller::Owner, |_| {}).unwrap();
    assert!(out.runtime_stopped);
    assert!(fake.is_live("Fiona"), "session Fiona stays");
    assert!(fake.has_session_window("Fiona:claude"));
    assert!(!fake.has_session_window("Fiona:codex"));
    assert_eq!(
        out.restore_source,
        Some(RestoreSourceOutcome::Pruned {
            file: SAVE.into(),
            lines: 2
        })
    );
    let left = fs::read_to_string(dir.join(SAVE)).unwrap();
    assert!(
        !left.contains("window\tFiona\t0\t:codex") && left.contains("pane\tFiona\t1\t"),
        "{left}"
    );
    assert!(left.contains("pane\tFeira\t"), "{left}");
}

#[test]
fn an_adopted_muse_window_is_pruned_by_window_once_it_is_not_running() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let dir = muse_dir(&home);
    let fake = FakeRuntime::with_muse_windows(&["bob"]);
    let env = fake.env(home.path());
    let target = adopted(&store, "muse:ada");
    let out = teardown_session(&store, &env, &target, "owner", Caller::Owner, |_| {}).unwrap();
    assert!(matches!(
        out.restore_source,
        Some(RestoreSourceOutcome::Pruned { lines: 2, .. })
    ));
    let left = fs::read_to_string(dir.join(SAVE)).unwrap();
    assert!(!left.contains(":ada") && left.contains(":bob"), "{left}");
}

#[test]
fn prune_after_stop_prunes_the_session_when_gone_the_window_when_alone_gone_else_nothing() {
    let home = TempDir::new().unwrap();
    let dir = muse_dir(&home);
    let fake = FakeRuntime::with_muse_windows(&["ada", "bob"]);
    let env = fake.env(home.path());
    // Still running: nothing.
    assert_eq!(
        prune_after_stop(&env, "muse", Some("ada")),
        RestoreSourceOutcome::Running
    );
    assert_eq!(
        prune_after_stop(&env, "muse", None),
        RestoreSourceOutcome::Running
    );
    assert_eq!(fs::read_to_string(dir.join(SAVE)).unwrap(), muse_save());
    // The window is gone, the session stays: the window alone.
    fake.close_window("muse:ada");
    assert!(matches!(
        prune_after_stop(&env, "muse", Some("ada")),
        RestoreSourceOutcome::Pruned { lines: 2, .. }
    ));
    // The session is gone: all of it.
    fake.clear();
    assert!(matches!(
        prune_after_stop(&env, "muse", Some("bob")),
        RestoreSourceOutcome::Pruned { .. }
    ));
    assert!(
        !fs::read_to_string(dir.join(SAVE))
            .unwrap()
            .contains("\tmuse\t")
    );
}

fn write_baks(dir: &Path, n: u32) -> Vec<String> {
    (0..n)
        .map(|i| {
            let name = format!("tmux_resurrect_20261101T0000{i:02}.txt.bak");
            fs::write(dir.join(&name), "old").unwrap();
            name
        })
        .collect()
}

#[test]
fn only_the_newest_five_backups_are_kept() {
    let home = TempDir::new().unwrap();
    let dir = resurrect_dir(&home);
    let baks = write_baks(&dir, 7);
    fs::write(dir.join("notes.txt.bak"), "mine").unwrap();
    fs::write(dir.join("tmux_resurrect_20261101T000000.txt"), "pane\tx\n").unwrap();
    fs::write(dir.join("tmux_resurrect_other.bak"), "mine").unwrap();

    assert!(matches!(
        prune_session_in(&dir, "Fiona"),
        RestoreSourceOutcome::Pruned { .. }
    ));

    let mut left: Vec<String> = fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("tmux_resurrect_") && n.ends_with(".txt.bak"))
        .collect();
    left.sort();
    // The new backup of this save is the newest by name; four of the old ones stay with it.
    let mut want: Vec<String> = baks[3..].to_vec();
    want.push(format!("{SAVE}.bak"));
    assert_eq!(left, want);
    assert!(dir.join("notes.txt.bak").exists() && dir.join("tmux_resurrect_other.bak").exists());
    assert!(dir.join("tmux_resurrect_20261101T000000.txt").exists());
    assert_eq!(fs::read_link(dir.join("last")).unwrap(), Path::new(SAVE));
}

#[test]
fn the_backup_of_the_save_just_pruned_is_never_the_one_trimmed() {
    let home = TempDir::new().unwrap();
    let dir = home.path().join("resurrect");
    fs::create_dir_all(&dir).unwrap();
    // Six newer backups exist already; the pruned save is the oldest.
    for i in 0..6 {
        fs::write(
            dir.join(format!("tmux_resurrect_2027010{i}T000000.txt.bak")),
            "x",
        )
        .unwrap();
    }
    let old = "tmux_resurrect_20261101T000000.txt";
    fs::write(dir.join(old), save_text()).unwrap();
    std::os::unix::fs::symlink(old, dir.join("last")).unwrap();
    prune_session_in(&dir, "Fiona");
    assert!(dir.join(format!("{old}.bak")).exists());
    let count = fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".txt.bak"))
        .count();
    assert_eq!(count, 5);
}

#[test]
fn a_backup_that_cannot_be_written_stops_the_prune() {
    use std::os::unix::fs::PermissionsExt;
    let home = TempDir::new().unwrap();
    let dir = resurrect_dir(&home);
    // No file can be created in a read-only directory, so there is no backup.
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();
    let writable = fs::write(dir.join("probe"), "").is_ok();
    let out = prune_session_in(&dir, "Fiona");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    if writable {
        return; // A privileged user ignores the mode.
    }
    assert!(
        matches!(&out, RestoreSourceOutcome::Failed { error } if error.contains("back up to")),
        "{out:?}"
    );
    assert_eq!(fs::read_to_string(dir.join(SAVE)).unwrap(), save_text());
}

#[test]
fn the_pruned_save_and_its_backup_keep_the_permissions_of_the_save() {
    use std::os::unix::fs::PermissionsExt;
    let home = TempDir::new().unwrap();
    let dir = resurrect_dir(&home);
    fs::set_permissions(dir.join(SAVE), fs::Permissions::from_mode(0o600)).unwrap();
    prune_session_in(&dir, "Fiona");
    let mode = |n: &str| fs::metadata(dir.join(n)).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(SAVE), 0o600);
    assert_eq!(mode(&format!("{SAVE}.bak")), 0o600);
    assert!(!dir.join(format!(".{SAVE}.prune.tmp")).exists());
}
