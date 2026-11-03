//! `fl install --yes` (#205 phase A4a), driven as a black box. Every run is confined to a
//! temporary root (`--prefix` or `FL_INSTALL_ROOT`) and a temporary HOME; the tests check the
//! install, the exit codes, the crash and resume paths (through the test-only fault seam),
//! concurrency, and that `fl doctor` agrees with what was installed.

use assert_cmd::Command;
use fl_core::install::ReleaseManifest;
use fl_install::fixture;
use serde_json::Value;
use std::collections::BTreeMap;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

/// Every path under `dir` with kind, mode, size, link target or content hash, and mtime: any
/// create, write, chmod, touch or delete changes it.
fn snapshot(dir: &Path) -> BTreeMap<String, String> {
    snapshot_with(dir, true)
}

/// [`snapshot`], optionally without file mtimes (a restored file has new times).
fn snapshot_with(dir: &Path, mtimes: bool) -> BTreeMap<String, String> {
    fn walk(base: &Path, dir: &Path, mtimes: bool, out: &mut BTreeMap<String, String>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd {
            let p = e.unwrap().path();
            let m = std::fs::symlink_metadata(&p).unwrap();
            let extra = if m.file_type().is_symlink() {
                std::fs::read_link(&p).unwrap().display().to_string()
            } else if m.is_file() {
                std::fs::read_to_string(&p).unwrap_or_default()
            } else {
                String::new()
            };
            // A directory's mtime moves with any entry added or removed beneath it (a journal
            // created and closed) and a restored symlink is a new inode; what the test pins
            // is every file's bytes and mtime, and the names, modes and targets.
            let mtime = if m.is_file() && mtimes {
                format!("{:?}", m.modified().unwrap())
            } else {
                String::new()
            };
            out.insert(
                p.strip_prefix(base).unwrap().display().to_string(),
                format!(
                    "{:?} {:o} {} {mtime} {extra}",
                    m.file_type(),
                    m.permissions().mode(),
                    m.len()
                ),
            );
            if m.is_dir() {
                walk(base, &p, mtimes, out);
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, mtimes, &mut out);
    out
}

/// A scratch world: HOME, the base the prefix lives under, the artifact directory and a cwd.
struct World {
    home: tempfile::TempDir,
    base: tempfile::TempDir,
    art: tempfile::TempDir,
    cwd: tempfile::TempDir,
}

impl Drop for World {
    fn drop(&mut self) {
        // A release is sealed read-only: make it removable so no temp tree is leaked.
        fl_install::make_tree_writable(self.base.path());
        fl_install::make_tree_writable(self.home.path());
    }
}

impl World {
    fn new() -> Self {
        Self {
            home: tempfile::tempdir().unwrap(),
            base: tempfile::tempdir().unwrap(),
            art: tempfile::tempdir().unwrap(),
            cwd: tempfile::tempdir().unwrap(),
        }
    }

    fn prefix(&self) -> PathBuf {
        self.base.path().join("prefix")
    }

    fn bin(&self) -> PathBuf {
        self.prefix().join("bin")
    }

    /// `fl` with a scrubbed environment.
    fn fl(&self) -> Command {
        let mut c = Command::cargo_bin("fl").unwrap();
        c.env_clear()
            .env("HOME", self.home.path())
            .current_dir(self.cwd.path());
        c
    }

    /// A release in the artifact dir, and its manifest file.
    fn release(&self, version: &str, variant: &str) -> (ReleaseManifest, PathBuf) {
        let m = fixture::release(self.art.path(), version, variant);
        let path = self
            .cwd
            .path()
            .join(format!("manifest-{version}-{variant}.json"));
        std::fs::write(&path, m.to_json()).unwrap();
        (m, path)
    }

    /// `fl install --yes --prefix <prefix> --manifest <m> --artifact-dir <art>` plus `extra`.
    fn install(&self, manifest: &Path, extra: &[&str]) -> Command {
        let mut c = self.fl();
        c.args(["install", "--yes", "--profile", "headless", "--prefix"])
            .arg(self.prefix())
            .arg("--manifest")
            .arg(manifest)
            .arg("--artifact-dir")
            .arg(self.art.path())
            .args(extra);
        c
    }

    fn doctor(&self) -> (i32, Value) {
        let out = self
            .fl()
            .args(["doctor", "--json", "--prefix"])
            .arg(self.prefix())
            .output()
            .unwrap();
        let v: Value = serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|e| panic!("not JSON ({e}): {}", String::from_utf8_lossy(&out.stdout)));
        (out.status.code().unwrap(), v)
    }
}

fn status<'a>(v: &'a Value, id: &str) -> &'a str {
    v["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == id)
        .unwrap_or_else(|| panic!("no {id}"))["status"]
        .as_str()
        .unwrap()
}

fn detail<'a>(v: &'a Value, id: &str) -> &'a str {
    v["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == id)
        .unwrap()["detail"]
        .as_str()
        .unwrap()
}

fn out_of(c: &mut Command) -> (i32, String, String) {
    let o = c.output().unwrap();
    (
        o.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&o.stdout).into_owned(),
        String::from_utf8_lossy(&o.stderr).into_owned(),
    )
}

#[test]
fn install_then_repeat_then_doctor_passes_receipt_and_ownership() {
    let w = World::new();
    let (_, man) = w.release("0.1.0", "a");
    let home_before = snapshot(w.home.path());

    let (code, stdout, stderr) = out_of(&mut w.install(&man, &[]));
    assert_eq!(code, 0, "{stdout}\n{stderr}");
    assert!(stdout.contains("installed release 0.1.0+"), "{stdout}");
    assert!(stdout.contains("transaction 1"), "{stdout}");
    assert!(
        stdout.contains("not executed: register_service") || stdout.contains("not executed:"),
        "{stdout}"
    );
    assert!(w.prefix().join("current/bin/fl").is_file());
    assert!(w.bin().join("agent-msg").exists());

    // doctor, from this binary, agrees
    let (dcode, v) = w.doctor();
    assert_eq!(
        status(&v, "G7.receipt-integrity"),
        "pass",
        "{}",
        detail(&v, "G7.receipt-integrity")
    );
    assert_eq!(
        status(&v, "G7.path-ownership"),
        "pass",
        "{}",
        detail(&v, "G7.path-ownership")
    );
    assert_ne!(dcode, 1, "{v}");

    // repeating the same install rewrites nothing
    let mut before = snapshot(&w.prefix());
    before.remove("install.lock");
    std::thread::sleep(std::time::Duration::from_millis(15));
    let (code, stdout, _) = out_of(&mut w.install(&man, &[]));
    assert_eq!(code, 0);
    assert!(stdout.contains("already installed"), "{stdout}");
    let mut after = snapshot(&w.prefix());
    after.remove("install.lock");
    assert_eq!(before, after);

    // and HOME was never touched
    assert_eq!(snapshot(w.home.path()), home_before);
}

#[test]
fn a_root_layout_leaves_home_alone_and_doctor_finds_the_install() {
    let w = World::new();
    let (_, man) = w.release("0.1.0", "a");
    let root = w.base.path().join("root");
    let mut c = w.fl();
    c.env("FL_INSTALL_ROOT", &root)
        .args(["install", "--yes", "--profile", "full", "--manifest"])
        .arg(&man)
        .arg("--artifact-dir")
        .arg(w.art.path());
    let home_before = snapshot(w.home.path());
    let (code, stdout, stderr) = out_of(&mut c);
    assert_eq!(code, 0, "{stdout}\n{stderr}");
    assert!(root.join(".local/bin/fl").exists());
    assert!(
        root.join(".local/share/fl/current/ui/dist/index.html")
            .is_file()
    );
    assert!(
        stdout.contains("not executed: register_service ferroloop-hub.service"),
        "{stdout}"
    );
    assert!(
        !root.join(".config").exists(),
        "no unit was written into the unit directory"
    );
    assert_eq!(snapshot(w.home.path()), home_before);
    let out = w
        .fl()
        .env("FL_INSTALL_ROOT", &root)
        .args(["doctor", "--json"])
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(status(&v, "G7.receipt-integrity"), "pass");
    assert_eq!(status(&v, "G7.path-ownership"), "pass");
}

#[test]
fn the_exit_codes() {
    let w = World::new();
    let (_, man) = w.release("0.1.0", "a");
    let pre = snapshot(w.base.path());
    let code = |c: &mut Command| out_of(c);

    // 3: a plan without --yes
    let (c, so, se) = code(
        w.fl()
            .args(["install", "--prefix"])
            .arg(w.prefix())
            .arg("--manifest")
            .arg(&man),
    );
    assert_eq!(
        (c, se.as_str()),
        (3, "error: pass --yes to apply (or --dry-run)\n")
    );
    assert!(so.starts_with("fl install plan"));

    // 3: not implemented
    for flag in [vec!["--from-git", "abc123"], vec!["--gc"], vec!["--adopt"]] {
        let mut args = vec!["install", "--yes"];
        args.extend(&flag);
        let (c, so, se) = code(
            w.fl()
                .args(&args)
                .arg("--prefix")
                .arg(w.prefix())
                .arg("--manifest")
                .arg(&man)
                .arg("--artifact-dir")
                .arg(w.art.path()),
        );
        assert_eq!(c, 3, "{flag:?}: {so}{se}");
        assert!(
            se.contains("not implemented in this release"),
            "{flag:?}: {se}"
        );
    }
    // 3: a source-build manifest (the built-in one) has nothing to install
    let (c, _, se) = code(
        w.fl()
            .args(["install", "--yes", "--prefix"])
            .arg(w.prefix())
            .arg("--artifact-dir")
            .arg(w.art.path()),
    );
    assert_eq!(c, 3);
    assert!(se.contains("not implemented in this release"), "{se}");
    // 3: a system prefix
    let (c, _, _) = code(
        w.fl()
            .args([
                "install",
                "--yes",
                "--prefix",
                "/usr/local/fl",
                "--manifest",
            ])
            .arg(&man)
            .arg("--artifact-dir")
            .arg(w.art.path()),
    );
    assert_eq!(c, 3);
    // 2: malformed requests
    for args in [
        vec!["install", "--dry-run", "--yes"],
        vec!["install", "--resume", "--rollback"],
        vec!["install", "--resume", "--yes"],
        vec!["install", "--yes", "--add", "nonsense"],
        vec!["install", "--yes", "--profile", "huge"],
    ] {
        let (c, _, se) = code(
            w.fl()
                .args(&args)
                .arg("--prefix")
                .arg(w.prefix())
                .arg("--manifest")
                .arg(&man),
        );
        assert_eq!(c, 2, "{args:?}: {se}");
    }
    // 2: installing needs --artifact-dir
    let (c, _, se) = code(
        w.fl()
            .args(["install", "--yes", "--prefix"])
            .arg(w.prefix())
            .arg("--manifest")
            .arg(&man),
    );
    assert_eq!(c, 2, "{se}");
    assert!(se.contains("--artifact-dir"));
    // none of that changed anything
    assert_eq!(snapshot(w.base.path()), pre);
}

#[test]
fn a_foreign_launcher_is_refused_then_replaced_with_a_backup() {
    let w = World::new();
    let (_, man) = w.release("0.1.0", "a");
    std::fs::create_dir_all(w.bin()).unwrap();
    let launcher = w.bin().join("fl");
    std::fs::write(&launcher, "#!/bin/sh\n# mine\n").unwrap();
    let pre = snapshot(w.base.path());
    let (c, _, se) = out_of(&mut w.install(&man, &[]));
    assert_eq!(c, 3, "{se}");
    assert!(
        se.contains(&format!("--replace {}", launcher.display())),
        "{se}"
    );
    assert_eq!(snapshot(w.base.path()), pre);

    let (c, so, se) = out_of(&mut w.install(&man, &["--replace", launcher.to_str().unwrap()]));
    assert_eq!(c, 0, "{so}{se}");
    assert!(so.contains("backed up"), "{so}");
    assert_eq!(
        std::fs::read_to_string(w.prefix().join("backups/1/launcher-fl")).unwrap(),
        "#!/bin/sh\n# mine\n"
    );
    assert!(
        std::fs::symlink_metadata(&launcher)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    let (_, v) = w.doctor();
    assert_eq!(
        status(&v, "G7.path-ownership"),
        "pass",
        "{}",
        detail(&v, "G7.path-ownership")
    );
}

#[test]
fn a_symlinked_prefix_component_and_a_system_root_are_refused() {
    let w = World::new();
    let (_, man) = w.release("0.1.0", "a");
    let elsewhere = tempfile::tempdir().unwrap();
    let root = w.base.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    symlink(elsewhere.path(), root.join(".local")).unwrap();
    let (c, _, se) = out_of(
        w.fl()
            .env("FL_INSTALL_ROOT", &root)
            .args(["install", "--yes", "--manifest"])
            .arg(&man)
            .arg("--artifact-dir")
            .arg(w.art.path()),
    );
    assert_eq!(c, 3, "{se}");
    assert!(se.contains("symlink"), "{se}");
    assert_eq!(std::fs::read_dir(elsewhere.path()).unwrap().count(), 0);

    // a root that is a link into /etc: refused as a system location, nothing written there
    let hop = w.base.path().join("hop");
    symlink("/etc", &hop).unwrap();
    let (c, _, se) = out_of(
        w.fl()
            .env("FL_INSTALL_ROOT", hop.join("x"))
            .args(["install", "--yes", "--manifest"])
            .arg(&man)
            .arg("--artifact-dir")
            .arg(w.art.path()),
    );
    assert_eq!(c, 3, "{se}");
    assert!(
        se.contains("system location") || se.contains("system"),
        "{se}"
    );
}

#[test]
fn a_read_only_root_is_a_clean_error() {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let w = World::new();
    let (_, man) = w.release("0.1.0", "a");
    std::fs::set_permissions(w.base.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
    let (c, _, se) = out_of(&mut w.install(&man, &[]));
    std::fs::set_permissions(w.base.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(c, 1, "{se}");
    assert!(se.contains("Permission denied"), "{se}");
    assert!(!w.prefix().exists());
}

#[test]
fn a_tampered_artifact_is_refused_with_nothing_activated() {
    let w = World::new();
    let (m, man) = w.release("0.1.0", "a");
    std::fs::write(
        w.art.path().join(&m.artifacts[0].file).join("bin/fl"),
        "#!/bin/sh\necho fl X\n",
    )
    .unwrap();
    let pre = snapshot(w.base.path());
    let (c, _, se) = out_of(&mut w.install(&man, &[]));
    assert_eq!(c, 3, "{se}");
    assert!(se.contains("hash mismatch"), "{se}");
    assert_eq!(snapshot(w.base.path()), pre);
}

// ----------------------------------------------------------- crash and resume

fn fault(w: &World, man: &Path, spec: &str) -> (i32, String) {
    let (c, _, se) = out_of(w.install(man, &[]).env("FL_INSTALL_FAULT", spec));
    (c, se)
}

#[test]
fn a_killed_install_is_resumed_or_rolled_back_by_the_cli_and_doctor_stays_honest() {
    let w = World::new();
    let (_, man) = w.release("0.1.0", "a");
    let pre = snapshot(w.base.path());

    // Killed before anything was staged: --resume rolls back; doctor sees no install.
    let (c, _) = fault(&w, &man, "exit:before:stage");
    assert_eq!(c, 99);
    assert!(w.prefix().join("journal.jsonl").exists());
    let (_, v) = w.doctor();
    assert_eq!(status(&v, "G7.receipt-integrity"), "not-installed");
    let (c, so, se) = out_of(
        w.fl()
            .args(["install", "--resume", "--prefix"])
            .arg(w.prefix()),
    );
    assert_eq!(c, 0, "{so}{se}");
    assert!(so.contains("rolled back"), "{so}");
    assert_eq!(snapshot(w.base.path()), pre, "exactly the pre-state");
    let (_, v) = w.doctor();
    assert_eq!(status(&v, "G7.receipt-integrity"), "not-installed");

    // Killed after the launcher: --resume completes; doctor passes.
    let (c, _) = fault(&w, &man, "exit:after:write_launcher");
    assert_eq!(c, 99);
    let (c, so, _) = out_of(
        w.fl()
            .args(["install", "--resume", "--prefix"])
            .arg(w.prefix()),
    );
    assert_eq!(c, 0);
    assert!(so.contains("completed"), "{so}");
    let (_, v) = w.doctor();
    assert_eq!(status(&v, "G7.receipt-integrity"), "pass");
    assert_eq!(status(&v, "G7.path-ownership"), "pass");

    // Nothing to resume now.
    let (c, so, _) = out_of(
        w.fl()
            .args(["install", "--resume", "--prefix"])
            .arg(w.prefix()),
    );
    assert_eq!(
        (c, so.as_str()),
        (0, "nothing to resume: no interrupted install found\n")
    );
}

/// A world with release 0.1.0 installed and 0.2.0 ready to install.
fn upgrade_world() -> (World, PathBuf, PathBuf) {
    let w = World::new();
    let (_, m1) = w.release("0.1.0", "a");
    let (_, m2) = w.release("0.2.0", "b");
    let (c, _, se) = out_of(&mut w.install(&m1, &[]));
    assert_eq!(c, 0, "{se}");
    (w, m1, m2)
}

#[test]
fn killed_upgrades_are_finished_by_the_next_plain_install() {
    for point in [
        "after:begin",
        "before:stage",
        "after:stage",
        "before:verify",
        "after:publish",
        "before:link_current",
        "after:link_current",
        "before:write_receipt",
        "after:write_receipt",
    ] {
        let (w, _m1, m2) = upgrade_world();
        let old = std::fs::read_link(w.prefix().join("current")).unwrap();
        let (c, _) = fault(&w, &m2, &format!("exit:{point}"));
        assert_eq!(c, 99, "{point}");
        // A reader of `current` sees the old or the new release, never a broken link.
        assert!(w.prefix().join("current/bin/fl").is_file(), "{point}");
        assert!(w.prefix().join("journal.jsonl").exists(), "{point}");
        // The plain next run finds the journal by itself and goes on to install.
        let (c, so, se) = out_of(&mut w.install(&m2, &[]));
        assert_eq!(c, 0, "{point}: {so}{se}");
        assert!(so.contains("interrupted install"), "{point}: {so}");
        assert_ne!(
            std::fs::read_link(w.prefix().join("current")).unwrap(),
            old,
            "{point}"
        );
        assert!(!w.prefix().join("journal.jsonl").exists());
        let (_, v) = w.doctor();
        assert_eq!(status(&v, "G7.path-ownership"), "pass", "{point}");
        assert_eq!(status(&v, "G7.receipt-integrity"), "pass", "{point}");
    }
}

#[test]
fn rollback_after_a_kill_restores_exactly_the_previous_install() {
    for point in [
        "after:begin",
        "after:stage",
        "after:publish",
        "after:link_current",
        "after:write_receipt",
    ] {
        let (w, _m1, m2) = upgrade_world();
        let mut installed = snapshot_with(w.base.path(), false);
        let (c, _) = fault(&w, &m2, &format!("exit:{point}"));
        assert_eq!(c, 99, "{point}");
        let (c, so, se) = out_of(
            w.fl()
                .args(["install", "--rollback", "--prefix"])
                .arg(w.prefix()),
        );
        assert_eq!(c, 0, "{point}: {so}{se}");
        assert!(so.contains("rolled back"), "{so}");
        let mut now = snapshot_with(w.base.path(), false);
        // the lock file's record moves with every run
        now.remove("prefix/install.lock");
        installed.remove("prefix/install.lock");
        assert_eq!(now, installed, "{point}");
        let (_, v) = w.doctor();
        assert_eq!(status(&v, "G7.path-ownership"), "pass", "{point}");
    }
}

// ---------------------------------------------------------------- concurrency

#[test]
fn many_processes_installing_at_once_never_corrupt_the_install() {
    let w = World::new();
    let (_, man) = w.release("0.1.0", "a");
    let bin = assert_cmd::cargo::cargo_bin("fl");
    let spawn = |man: &Path| {
        std::process::Command::new(&bin)
            .env_clear()
            .env("HOME", w.home.path())
            .current_dir(w.cwd.path())
            .args(["install", "--yes", "--profile", "headless", "--prefix"])
            .arg(w.prefix())
            .arg("--manifest")
            .arg(man)
            .arg("--artifact-dir")
            .arg(w.art.path())
            .output()
            .unwrap()
    };
    let outs: Vec<_> = std::thread::scope(|s| {
        (0..6)
            .map(|_| s.spawn(|| spawn(&man)))
            .collect::<Vec<_>>()
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect()
    });
    for o in &outs {
        assert_eq!(
            o.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&o.stderr)
        );
    }
    let installed = outs
        .iter()
        .filter(|o| String::from_utf8_lossy(&o.stdout).contains("installed release"))
        .count();
    let already = outs
        .iter()
        .filter(|o| String::from_utf8_lossy(&o.stdout).contains("already installed"))
        .count();
    assert_eq!((installed, already), (1, 5));
    let (_, v) = w.doctor();
    assert_eq!(status(&v, "G7.path-ownership"), "pass");

    // different versions at once: both finish, one is current, the receipt agrees
    let (_, m2) = w.release("0.2.0", "b");
    let (_, m3) = w.release("0.3.0", "c");
    let mans = [m2, m3];
    let outs: Vec<_> = std::thread::scope(|s| {
        (0..6)
            .map(|i| {
                let m = &mans[i % 2];
                let spawn = &spawn;
                s.spawn(move || spawn(m))
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect()
    });
    for o in &outs {
        assert_eq!(
            o.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&o.stderr)
        );
    }
    let receipt: Value =
        serde_json::from_str(&std::fs::read_to_string(w.prefix().join("receipt.json")).unwrap())
            .unwrap();
    let name = receipt["release"]["name"].as_str().unwrap();
    assert_eq!(
        std::fs::read_link(w.prefix().join("current")).unwrap(),
        PathBuf::from(format!("releases/{name}"))
    );
    assert!(!w.prefix().join("journal.jsonl").exists());
    let (_, v) = w.doctor();
    assert_eq!(
        status(&v, "G7.path-ownership"),
        "pass",
        "{}",
        detail(&v, "G7.path-ownership")
    );
    assert_eq!(status(&v, "G7.receipt-integrity"), "pass");
}

#[test]
fn a_held_lock_makes_the_second_installer_refuse_after_its_timeout() {
    let w = World::new();
    let (_, m1) = w.release("0.1.0", "a");
    let (_, m2) = w.release("0.2.0", "b");
    assert_eq!(out_of(&mut w.install(&m1, &[])).0, 0);
    let mut before = snapshot(&w.prefix());
    before.remove("install.lock");

    // Hold the lock the way an installer does.
    let lock = std::fs::OpenOptions::new()
        .write(true)
        .open(w.prefix().join("install.lock"))
        .unwrap();
    {
        use std::os::fd::AsRawFd;
        // SAFETY: the descriptor is open for the whole call.
        assert_eq!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
    }
    let started = std::time::Instant::now();
    let (c, _, se) = out_of(&mut w.install(&m2, &["--lock-timeout", "1"]));
    assert_eq!(c, 3, "{se}");
    assert!(se.contains("another install is running"), "{se}");
    assert!(started.elapsed() >= std::time::Duration::from_millis(900));
    let (c, _, _) = out_of(&mut w.install(&m2, &["--lock-timeout", "0"]));
    assert_eq!(c, 3);
    let mut after = snapshot(&w.prefix());
    after.remove("install.lock");
    assert_eq!(before, after, "nothing but the lock record changed");
    drop(lock);
    assert_eq!(out_of(&mut w.install(&m2, &[])).0, 0);
}

#[test]
fn a_stale_lock_record_does_not_block() {
    let w = World::new();
    let (_, m1) = w.release("0.1.0", "a");
    std::fs::create_dir_all(w.prefix()).unwrap();
    std::fs::write(w.prefix().join("install.lock"), "4194999\n").unwrap();
    let (c, so, se) = out_of(&mut w.install(&m1, &["--lock-timeout", "0"]));
    assert_eq!(c, 0, "{so}{se}");
    assert!(so.contains("stale install lock"), "{so}");
}
