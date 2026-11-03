//! Tests of the install transaction: every step, every crash point, resume and rollback,
//! conflicts and decisions, refusals, concurrency, and isolation.

use crate::fixture;
use crate::*;
use fl_core::install::{
    HostInfo, InstallReceipt, InstallRequest, Layout, Product, Profile, ReleaseManifest, Step,
    retention,
};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    /// `--prefix <base>/prefix`: the launcher lives in `<prefix>/bin`.
    Explicit,
    /// `FL_INSTALL_ROOT=<base>/root`: launcher in `<root>/.local/bin`, units elsewhere.
    UnderRoot,
}

struct World {
    base: tempfile::TempDir,
    art: tempfile::TempDir,
    kind: Kind,
}

impl Drop for World {
    fn drop(&mut self) {
        make_tree_writable(self.base.path());
    }
}

impl World {
    fn new(kind: Kind) -> Self {
        Self {
            base: tempfile::tempdir().unwrap(),
            art: tempfile::tempdir().unwrap(),
            kind,
        }
    }

    fn root(&self) -> PathBuf {
        match self.kind {
            Kind::Explicit => self.base.path().join("prefix"),
            Kind::UnderRoot => self.base.path().join("root"),
        }
    }

    fn layout(&self) -> Layout {
        let r = self.root();
        match self.kind {
            Kind::Explicit => Layout::explicit(r.to_str().unwrap()).unwrap(),
            Kind::UnderRoot => Layout::under_root(r.to_str().unwrap()).unwrap(),
        }
    }

    fn prefix(&self) -> PathBuf {
        PathBuf::from(self.layout().prefix)
    }

    fn bin(&self) -> PathBuf {
        PathBuf::from(self.layout().bin_dir)
    }

    fn env(&self) -> Env {
        let mut e = Env::new(self.root(), None);
        e.lock_timeout = Duration::from_secs(10);
        e
    }

    fn release(&self, version: &str, variant: &str) -> ReleaseManifest {
        fixture::release(self.art.path(), version, variant)
    }

    fn request(&self, profile: Option<Profile>) -> InstallRequest {
        InstallRequest {
            // The default is `headless`: it has aliases (FerroWire) and no UI.
            profile: profile.or(Some(Profile::Headless)),
            add: vec![],
            remove: vec![],
            layout: self.layout(),
            adopt: false,
        }
    }

    fn apply_with(
        &self,
        m: &ReleaseManifest,
        profile: Option<Profile>,
        decisions: &Decisions,
        env: &Env,
    ) -> Result<ApplyReport> {
        let req = self.request(profile);
        let host = HostInfo::from_consts(std::env::consts::OS, std::env::consts::ARCH).unwrap();
        apply(
            env,
            &ApplyArgs {
                request: &req,
                manifest: m,
                host,
                artifact_dir: Some(self.art.path()),
                decisions,
            },
        )
    }

    fn install(&self, m: &ReleaseManifest) -> ApplyReport {
        self.apply_with(m, None, &Decisions::default(), &self.env())
            .unwrap_or_else(|e| panic!("install failed: {e}"))
    }

    fn install_faulty(
        &self,
        m: &ReleaseManifest,
        decisions: &Decisions,
        faults: Faults,
    ) -> Result<ApplyReport> {
        let mut env = self.env();
        env.faults = faults;
        self.apply_with(m, None, decisions, &env)
    }

    fn receipt(&self) -> InstallReceipt {
        InstallReceipt::parse(&fs::read_to_string(self.prefix().join("receipt.json")).unwrap())
            .unwrap()
    }

    /// Everything under the base directory, with the base path normalised away so two worlds
    /// compare equal. Mode, size and content (or link target); no time.
    fn snap(&self) -> BTreeMap<String, String> {
        snapshot(self.base.path(), false)
    }
}

fn snapshot(base: &Path, with_mtime: bool) -> BTreeMap<String, String> {
    let marker = base.to_string_lossy().into_owned();
    fn walk(
        base: &Path,
        dir: &Path,
        marker: &str,
        with_mtime: bool,
        out: &mut BTreeMap<String, String>,
    ) {
        let Ok(rd) = fs::read_dir(dir) else { return };
        for e in rd {
            let e = e.unwrap();
            let p = e.path();
            let m = fs::symlink_metadata(&p).unwrap();
            let rel = p.strip_prefix(base).unwrap().to_string_lossy().into_owned();
            let desc = if m.file_type().is_symlink() {
                format!(
                    "symlink -> {}",
                    fs::read_link(&p)
                        .unwrap()
                        .to_string_lossy()
                        .replace(marker, "<ROOT>")
                )
            } else if m.is_dir() {
                format!("dir {:o}", m.permissions().mode() & 0o7777)
            } else {
                let body = fs::read_to_string(&p)
                    .unwrap_or_default()
                    .replace(marker, "<ROOT>");
                format!(
                    "file {:o} {} {body}",
                    m.permissions().mode() & 0o7777,
                    m.len()
                )
            };
            let t = if with_mtime {
                format!(" @{:?}", m.modified().unwrap())
            } else {
                String::new()
            };
            out.insert(rel, format!("{desc}{t}"));
            if m.is_dir() {
                walk(base, &p, marker, with_mtime, out);
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(base, base, &marker, with_mtime, &mut out);
    out
}

// ------------------------------------------------------------ the layout

#[test]
fn a_fresh_install_lays_out_a_release_current_launcher_aliases_and_receipt() {
    for kind in [Kind::Explicit, Kind::UnderRoot] {
        let w = World::new(kind);
        let m = w.release("0.1.0", "a");
        let name = fixture::release_name(&m);
        let r = w.install(&m);
        assert_eq!(r.outcome, Outcome::Installed);
        assert_eq!(r.txn, Some(1));
        assert_eq!(
            r.steps,
            [
                Step::Begin,
                Step::Stage,
                Step::Verify,
                Step::Publish,
                Step::LinkCurrent,
                Step::WriteLauncher,
                Step::WriteAliases,
                Step::WriteReceipt,
                Step::Commit
            ]
        );
        let p = w.prefix();
        let rel = p.join("releases").join(&name);
        assert_eq!(
            fs::read_to_string(rel.join("bin/fl")).unwrap(),
            "#!/bin/sh\necho fl a\n"
        );
        assert!(rel.join("bin/ferrostore").is_file());
        assert!(
            rel.join("contrib/units/ferrostore.service").is_file(),
            "a unit TEMPLATE is staged inside the release"
        );
        assert!(
            !rel.join("NOTICE").exists(),
            "an undeclared file is not staged"
        );
        let rj: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(rel.join("release.json")).unwrap()).unwrap();
        assert_eq!(rj["name"], name.as_str());
        // immutable: read-only files and directories
        assert_eq!(mode_of(&rel.join("bin/fl")).unwrap(), 0o555);
        assert_eq!(mode_of(&rel.join("release.json")).unwrap(), 0o444);
        assert_eq!(mode_of(&rel).unwrap(), 0o555);
        assert_eq!(mode_of(&rel.join("contrib/units")).unwrap(), 0o555);
        // current -> releases/<name>, relative and atomic
        assert_eq!(
            fs::read_link(p.join("current")).unwrap(),
            PathBuf::from(format!("releases/{name}"))
        );
        assert!(p.join("current/bin/fl").is_file());
        // the stable launcher is a symlink into current; the aliases are symlinks to it
        let launcher = w.bin().join("fl");
        assert_eq!(fs::read_link(&launcher).unwrap(), p.join("current/bin/fl"));
        assert_eq!(
            fs::read_link(w.bin().join("agent-msg")).unwrap(),
            PathBuf::from("fl")
        );
        assert_eq!(
            fs::read_link(w.bin().join("agent-bus-mcp")).unwrap(),
            PathBuf::from("fl")
        );
        assert!(
            w.bin().join("agent-msg").is_file(),
            "the alias resolves through the launcher"
        );
        // lock kept, journal closed, no staging or temp leftovers
        assert!(p.join("install.lock").is_file());
        assert!(!p.join("journal.jsonl").exists());
        let leftovers: Vec<_> = fs::read_dir(p.join("releases"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(leftovers, std::slice::from_ref(&name));
        // the receipt owns it all, with hashes, and carries no clock
        let rc = w.receipt();
        assert_eq!(rc.release.name, name);
        assert_eq!(rc.journal.len(), 1);
        assert_eq!((rc.journal[0].seq, rc.journal[0].at_ms), (1, 0));
        assert_eq!(rc.aliases.len(), 2);
        let bin_fl = p.join("releases").join(&name).join("bin/fl");
        let owned = rc.owned_path(bin_fl.to_str().unwrap()).unwrap();
        assert_eq!(
            owned.sha256.as_deref(),
            Some(fl_core::install::sha256_hex(b"#!/bin/sh\necho fl a\n").as_str())
        );
        assert!(rc.owned_path(p.join("current").to_str().unwrap()).is_some());
        assert!(rc.owned_path(launcher.to_str().unwrap()).is_some());
        let text = fs::read_to_string(p.join("receipt.json")).unwrap();
        assert!(!text.to_lowercase().contains("token") && !text.contains("secret"));
        // only ferroloop's and ferrostore's files for the default (minimal) selection
        assert!(!rel.join("ui").exists(), "minimal installs no UI");
    }
}

#[test]
fn the_receipt_is_deterministic_for_the_same_inputs() {
    let a = World::new(Kind::Explicit);
    let b = World::new(Kind::Explicit);
    a.install(&a.release("0.1.0", "a"));
    b.install(&b.release("0.1.0", "a"));
    let norm = |w: &World| {
        fs::read_to_string(w.prefix().join("receipt.json"))
            .unwrap()
            .replace(w.base.path().to_str().unwrap(), "<ROOT>")
    };
    assert_eq!(norm(&a), norm(&b));
    assert_eq!(a.snap(), b.snap());
}

#[test]
fn mutable_data_is_never_inside_a_release_and_never_touched() {
    let w = World::new(Kind::UnderRoot);
    let data = w.root().join(".local/share/fl-data");
    fs::create_dir_all(&data).unwrap();
    fs::write(data.join("store.fls"), "precious").unwrap();
    let cfg = w.root().join(".config/fl");
    fs::create_dir_all(&cfg).unwrap();
    fs::write(cfg.join("config.toml"), "x = 1\n").unwrap();
    let before = snapshot(&data, true);
    let cfg_before = snapshot(&cfg, true);
    w.install(&w.release("0.1.0", "a"));
    w.install(&w.release("0.2.0", "b"));
    assert_eq!(snapshot(&data, true), before);
    assert_eq!(snapshot(&cfg, true), cfg_before);
    let rc = w.receipt();
    assert!(
        rc.paths
            .iter()
            .all(|p| !p.path.contains("fl-data") && !p.path.contains(".config/fl"))
    );
}

#[test]
fn a_repeated_install_is_a_noop_that_rewrites_nothing() {
    let w = World::new(Kind::UnderRoot);
    let m = w.release("0.1.0", "a");
    w.install(&m);
    let mut before = snapshot(w.base.path(), true);
    // The lock file records the holder's pid and is touched by every run.
    before.retain(|k, _| !k.ends_with("install.lock"));
    std::thread::sleep(Duration::from_millis(20));
    let again = w.install(&m);
    assert_eq!(again.outcome, Outcome::AlreadyInstalled);
    assert!(again.render_text().contains("already installed"));
    let mut after = snapshot(w.base.path(), true);
    after.retain(|k, _| !k.ends_with("install.lock"));
    assert_eq!(
        before, after,
        "bytes, modes, links and mtimes of releases/, current and receipt"
    );
    assert_eq!(w.receipt().journal.len(), 1, "no second transaction");
}

#[test]
fn a_different_version_installs_side_by_side_and_flips_current() {
    let w = World::new(Kind::Explicit);
    let m1 = w.release("0.1.0", "a");
    let m2 = w.release("0.2.0", "b");
    w.install(&m1);
    let launcher_before = fs::read_link(w.bin().join("fl")).unwrap();
    let r2 = w.install(&m2);
    assert_eq!(r2.txn, Some(2));
    assert!(
        !r2.steps.contains(&Step::WriteLauncher),
        "the launcher already points at current"
    );
    assert!(!r2.steps.contains(&Step::WriteAliases));
    let p = w.prefix();
    let (n1, n2) = (fixture::release_name(&m1), fixture::release_name(&m2));
    assert!(
        p.join("releases").join(&n1).join("bin/fl").is_file(),
        "the old release stays"
    );
    assert_eq!(
        fs::read_link(p.join("current")).unwrap(),
        PathBuf::from(format!("releases/{n2}"))
    );
    assert_eq!(fs::read_link(w.bin().join("fl")).unwrap(), launcher_before);
    assert_eq!(
        fs::read_to_string(w.bin().join("fl")).unwrap(),
        "#!/bin/sh\necho fl b\n"
    );
    let rc = w.receipt();
    assert_eq!(rc.release.name, n2);
    assert_eq!(rc.journal.iter().map(|j| j.seq).collect::<Vec<_>>(), [1, 2]);
    for n in [&n1, &n2] {
        assert!(
            rc.owned_path(p.join("releases").join(n).join("bin/fl").to_str().unwrap())
                .is_some()
        );
    }
    // going back to the first version is an ordinary install too (flip back)
    let r3 = w.install(&m1);
    assert_eq!(r3.outcome, Outcome::Installed);
    assert_eq!(
        fs::read_link(p.join("current")).unwrap(),
        PathBuf::from(format!("releases/{n1}"))
    );
}

#[test]
fn retention_keeps_current_and_two_previous_and_only_reports_the_rest() {
    let w = World::new(Kind::Explicit);
    let mut names = Vec::new();
    let mut last = None;
    for (i, v) in ["0.1.0", "0.2.0", "0.3.0", "0.4.0"].iter().enumerate() {
        let m = w.release(v, &format!("v{i}"));
        names.push(fixture::release_name(&m));
        last = Some(w.install(&m));
    }
    let ret = last.unwrap().retention.unwrap();
    assert_eq!(
        ret.keep,
        [names[3].clone(), names[2].clone(), names[1].clone()]
    );
    assert_eq!(ret.gc_eligible, [names[0].clone()]);
    for n in &names {
        assert!(
            w.prefix().join("releases").join(n).is_dir(),
            "no GC runs: {n} is still there"
        );
    }
    assert_eq!(retention(&w.receipt()), ret);
}

#[test]
fn a_reader_always_sees_a_complete_release_through_current_during_flips() {
    let w = World::new(Kind::Explicit);
    let ms: Vec<_> = (0..6)
        .map(|i| w.release(&format!("0.{}.0", i + 1), &format!("v{i}")))
        .collect();
    w.install(&ms[0]);
    let cur = w.prefix().join("current");
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let seen = std::sync::Arc::new(std::sync::Mutex::new(std::collections::BTreeSet::new()));
    let readers: Vec<_> = (0..3)
        .map(|_| {
            let (cur, stop, seen) = (cur.clone(), stop.clone(), seen.clone());
            std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    // `current` always exists, and everything behind it is complete.
                    let target = fs::read_link(&cur).expect("current never disappears");
                    let rj = fs::read_to_string(cur.join("release.json")).expect("release.json");
                    let v: serde_json::Value =
                        serde_json::from_str(&rj).expect("complete release.json");
                    assert!(
                        cur.join("bin/fl").is_file()
                            && fs::metadata(cur.join("bin/fl")).unwrap().len() > 0
                    );
                    seen.lock()
                        .unwrap()
                        .insert(target.to_string_lossy().into_owned());
                    let _ = v;
                }
            })
        })
        .collect();
    // Flip until a reader has seen two different releases (bounded by time, not by a count of
    // reads: a slow machine only takes longer). A reader that sees an inconsistent release
    // panics, which fails the join below.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    for m in ms.iter().skip(1).cycle() {
        w.install(m);
        if seen.lock().unwrap().len() >= 2 || std::time::Instant::now() >= deadline {
            break;
        }
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    for h in readers {
        h.join().expect("a reader saw an inconsistent release");
    }
    assert!(
        seen.lock().unwrap().len() >= 2,
        "at least one switch was observed"
    );
}

// ---------------------------------------------------- faults and recovery

fn completes(points: &[FaultPoint], p: FaultPoint) -> bool {
    let verify = points
        .iter()
        .position(|x| *x == FaultPoint::before(Step::Verify))
        .expect("a staged transaction has a verify step");
    points.iter().position(|x| *x == p).unwrap() >= verify
}

/// Run `scenario` clean once, then crash it at EVERY point between steps and check that
/// `--resume`, `--rollback` and the next plain install each end in exactly "fully installed"
/// or exactly the pre-state, by full directory snapshots.
fn crash_matrix(kind: Kind, setup: &dyn Fn(&World), decisions: &Decisions, version: &str) {
    let go = |faults: Faults| -> (World, Result<ApplyReport>) {
        let w = World::new(kind);
        setup(&w);
        let m = w.release(version, "next");
        let r = w.install_faulty(&m, decisions, faults);
        (w, r)
    };
    let (clean, r) = go(Faults::none());
    let post = clean.snap();
    let steps = r.unwrap().steps;
    let pre = {
        let w = World::new(kind);
        setup(&w);
        w.snap()
    };
    assert_ne!(pre, post);
    let points = FaultPoint::all(&steps);
    assert!(points.len() >= 13, "{points:?}");
    for &point in &points {
        // Once the `commit` line is written the receipt is, so the transaction is committed
        // and only a leftover journal remains to be cleaned.
        let finished = point.step == Step::Commit;
        let cleaned = if point == FaultPoint::after(Step::Commit) {
            RecoverAction::NothingToDo
        } else {
            RecoverAction::Cleaned
        };
        let will_complete = completes(&points, point);

        // 1. crash, then `--resume`
        let (w, r) = go(Faults::crash(point));
        match r {
            Err(InstallError::Crashed(p)) => assert_eq!(p, point),
            other => panic!("{point}: {other:?}"),
        }
        let rep = recover(&w.env(), &w.layout(), RecoverMode::Auto).unwrap();
        if finished {
            assert_eq!(rep.action, cleaned, "{point}");
        } else if will_complete {
            assert_eq!(rep.action, RecoverAction::Completed, "{point}: {rep:?}");
        } else {
            assert_eq!(rep.action, RecoverAction::RolledBack, "{point}: {rep:?}");
        }
        let want = if finished || will_complete {
            &post
        } else {
            &pre
        };
        assert_eq!(&w.snap(), want, "resume after {point}");

        // 2. crash, then `--rollback`
        let (w, _) = go(Faults::crash(point));
        let rep = recover(&w.env(), &w.layout(), RecoverMode::Rollback).unwrap();
        if finished {
            assert_eq!(rep.action, cleaned, "{point}");
            assert_eq!(w.snap(), post);
        } else {
            assert_eq!(rep.action, RecoverAction::RolledBack, "{point}");
            assert_eq!(w.snap(), pre, "rollback after {point}");
        }

        // 3. crash, then the next plain install notices the journal by itself
        let (w, _) = go(Faults::crash(point));
        let m = w.release(version, "next");
        let again = w.install_faulty(&m, decisions, Faults::none()).unwrap();
        if finished {
            assert_eq!(again.outcome, Outcome::AlreadyInstalled);
            assert_eq!(
                again.recovered.as_ref().map(|r| r.action),
                (cleaned != RecoverAction::NothingToDo).then_some(cleaned)
            );
        } else if will_complete {
            assert_eq!(
                again.recovered.as_ref().unwrap().action,
                RecoverAction::Completed,
                "{point}"
            );
        } else {
            assert_eq!(
                again.recovered.as_ref().unwrap().action,
                RecoverAction::RolledBack,
                "{point}"
            );
            assert_eq!(again.outcome, Outcome::Installed);
        }
        assert_eq!(w.snap(), post, "the next install after {point}");
        // a crash never leaves the journal of a finished transaction behind
        assert!(!w.prefix().join("journal.jsonl").exists());
    }
}

#[test]
fn every_crash_point_of_a_fresh_install_resumes_or_rolls_back_cleanly() {
    crash_matrix(Kind::Explicit, &|_| {}, &Decisions::default(), "0.1.0");
}

#[test]
fn every_crash_point_of_a_fresh_install_under_a_root_layout() {
    crash_matrix(Kind::UnderRoot, &|_| {}, &Decisions::default(), "0.1.0");
}

#[test]
fn every_crash_point_of_an_upgrade_leaves_the_old_release_selected_or_the_new_one() {
    let setup = |w: &World| {
        let m = w.release("0.1.0", "first");
        w.install(&m);
    };
    crash_matrix(Kind::Explicit, &setup, &Decisions::default(), "0.2.0");
    crash_matrix(Kind::UnderRoot, &setup, &Decisions::default(), "0.2.0");
}

#[test]
fn every_crash_point_of_a_replace_restores_or_completes_with_the_backup() {
    let setup = |w: &World| {
        fs::create_dir_all(w.bin()).unwrap();
        fs::write(w.bin().join("fl"), "#!/bin/sh\n# my launcher\n").unwrap();
    };
    for kind in [Kind::Explicit, Kind::UnderRoot] {
        replace_matrix(kind, &setup);
    }
}

/// `crash_matrix` with a decision whose path is rebuilt for each world.
fn replace_matrix(kind: Kind, setup: &dyn Fn(&World)) {
    let dec = |w: &World| Decisions {
        replace: vec![w.bin().join("fl").to_string_lossy().into_owned()],
        keep: vec![],
    };
    let go = |faults: Faults| -> (World, Result<ApplyReport>) {
        let w = World::new(kind);
        setup(&w);
        let m = w.release("0.1.0", "next");
        let d = dec(&w);
        let r = w.install_faulty(&m, &d, faults);
        (w, r)
    };
    let (clean, r) = go(Faults::none());
    let post = clean.snap();
    let steps = r.unwrap().steps;
    assert!(steps.contains(&Step::Backup));
    assert_eq!(
        fs::read_to_string(clean.prefix().join("backups/1/launcher-fl")).unwrap(),
        "#!/bin/sh\n# my launcher\n"
    );
    let pre = {
        let w = World::new(kind);
        setup(&w);
        w.snap()
    };
    let points = FaultPoint::all(&steps);
    let verify = points
        .iter()
        .position(|x| *x == FaultPoint::before(Step::Verify))
        .unwrap();
    for (i, &point) in points.iter().enumerate() {
        let finished = point.step == Step::Commit;
        let (w, r) = go(Faults::crash(point));
        assert!(matches!(r, Err(InstallError::Crashed(_))), "{point}");
        recover(&w.env(), &w.layout(), RecoverMode::Auto).unwrap();
        let want = if finished || i >= verify { &post } else { &pre };
        assert_eq!(&w.snap(), want, "resume after {point}");
        let (w, _) = go(Faults::crash(point));
        recover(&w.env(), &w.layout(), RecoverMode::Rollback).unwrap();
        assert_eq!(
            &w.snap(),
            if finished { &post } else { &pre },
            "rollback after {point}"
        );
    }
}

#[test]
fn a_failing_step_rolls_everything_back_and_reports_the_cause() {
    for setup_first in [false, true] {
        // The steps this scenario runs, from a clean twin.
        let steps = {
            let w = World::new(Kind::Explicit);
            if setup_first {
                w.install(&w.release("0.1.0", "first"));
            }
            w.install(&w.release("0.2.0", "second")).steps
        };
        let mut tried = 0;
        for step in steps.iter().copied().filter(|s| *s != Step::Begin) {
            let errno = match step {
                Step::Verify | Step::Commit => libc::EIO,
                Step::Publish | Step::LinkCurrent => libc::EACCES,
                _ => libc::ENOSPC,
            };
            let w = World::new(Kind::Explicit);
            if setup_first {
                w.install(&w.release("0.1.0", "first"));
            }
            let pre = w.snap();
            let m = w.release("0.2.0", "second");
            let e = w
                .install_faulty(&m, &Decisions::default(), Faults::fail(step, errno))
                .unwrap_err();
            assert_eq!(e.exit_code(), 1, "{step:?}: {e}");
            assert!(
                matches!(e, InstallError::RolledBack { .. }),
                "{step:?}: {e:?}"
            );
            assert!(e.to_string().contains("rolled back"), "{e}");
            assert_eq!(
                w.snap(),
                pre,
                "{step:?} (setup {setup_first}) left no trace"
            );
            tried += 1;
        }
        assert!(tried >= 6, "{steps:?}");
    }
}

#[test]
fn a_full_disk_while_staging_any_file_rolls_back() {
    let w0 = World::new(Kind::Explicit);
    let n_files = {
        let m = w0.release("0.1.0", "a");
        w0.install(&m);
        w0.receipt()
            .paths
            .iter()
            .filter(|p| p.kind == fl_core::install::OwnedPathKind::File)
            .count()
    };
    assert!(n_files >= 3);
    for n in 0..n_files {
        let w = World::new(Kind::Explicit);
        let pre = w.snap();
        let m = w.release("0.1.0", "a");
        let faults = Faults {
            fail_stage_file: Some((n, libc::ENOSPC)),
            ..Faults::none()
        };
        let e = w
            .install_faulty(&m, &Decisions::default(), faults)
            .unwrap_err();
        assert!(e.to_string().contains("No space left"), "{e}");
        assert_eq!(w.snap(), pre, "file {n}");
    }
}

#[test]
fn a_crash_during_the_rollback_itself_is_finished_by_the_next_rollback() {
    for undo in 0..=5usize {
        let w = World::new(Kind::Explicit);
        w.install(&w.release("0.1.0", "first"));
        let pre = w.snap();
        let m = w.release("0.2.0", "second");
        let e = w
            .install_faulty(
                &m,
                &Decisions::default(),
                Faults::crash(FaultPoint::after(Step::WriteReceipt)),
            )
            .unwrap_err();
        assert!(matches!(e, InstallError::Crashed(_)));
        let mut env = w.env();
        env.faults = Faults {
            die_in_rollback: Some((undo, Death::Return)),
            ..Faults::none()
        };
        let e = recover(&env, &w.layout(), RecoverMode::Rollback).unwrap_err();
        assert!(matches!(e, InstallError::Crashed(_)), "undo {undo}: {e:?}");
        recover(&w.env(), &w.layout(), RecoverMode::Rollback).unwrap();
        assert_eq!(w.snap(), pre, "rollback crashed after undo action {undo}");
    }
}

#[test]
fn a_receipt_only_change_is_a_short_transaction_that_also_recovers() {
    // `--add ferrostep` has no files of its own: only the receipt changes.
    let run = |faults: Faults| {
        let w = World::new(Kind::Explicit);
        let m = w.release("0.1.0", "a");
        w.install(&m);
        let mut req = w.request(None);
        req.add = vec![Product::Ferrostep];
        let host = HostInfo::from_consts(std::env::consts::OS, std::env::consts::ARCH).unwrap();
        let mut env = w.env();
        env.faults = faults;
        let r = apply(
            &env,
            &ApplyArgs {
                request: &req,
                manifest: &m,
                host,
                artifact_dir: Some(w.art.path()),
                decisions: &Decisions::default(),
            },
        );
        (w, req, m, r)
    };
    let (clean, req, m, r) = run(Faults::none());
    let r = r.unwrap();
    assert_eq!(r.steps, [Step::Begin, Step::WriteReceipt, Step::Commit]);
    let post = clean.snap();
    assert!(clean.receipt().products.contains(&Product::Ferrostep));
    assert_eq!(clean.receipt().journal.len(), 2);

    for point in FaultPoint::all(&r.steps) {
        let (w, _, _, r) = run(Faults::crash(point));
        assert!(matches!(r, Err(InstallError::Crashed(_))), "{point}");
        recover(&w.env(), &w.layout(), RecoverMode::Auto).unwrap();
        // no staging in this transaction: every point resumes forward
        assert_eq!(w.snap(), post, "resume after {point}");
    }
    // the same request again changes nothing
    let host = HostInfo::from_consts(std::env::consts::OS, std::env::consts::ARCH).unwrap();
    let again = apply(
        &clean.env(),
        &ApplyArgs {
            request: &req,
            manifest: &m,
            host,
            artifact_dir: Some(clean.art.path()),
            decisions: &Decisions::default(),
        },
    )
    .unwrap();
    assert_eq!(again.outcome, Outcome::AlreadyInstalled);
}

#[test]
fn adding_files_to_an_installed_release_is_refused_not_mixed_in() {
    let w = World::new(Kind::Explicit);
    let m = w.release("0.1.0", "a");
    w.install(&m);
    let pre = w.snap();
    let r = w
        .apply_with(&m, Some(Profile::Full), &Decisions::default(), &w.env())
        .unwrap_err();
    assert_eq!(r.exit_code(), 3, "{r}");
    assert!(r.to_string().contains("selection"), "{r}");
    assert_eq!(w.snap(), pre);
}

// ------------------------------------------------------------ conflicts

fn foreign_launcher(w: &World) -> PathBuf {
    fs::create_dir_all(w.bin()).unwrap();
    let p = w.bin().join("fl");
    fs::write(&p, "#!/bin/sh\n# hand-built\n").unwrap();
    p
}

#[test]
fn a_foreign_launcher_is_a_conflict_apply_refuses_and_leaves_alone() {
    let w = World::new(Kind::Explicit);
    let launcher = foreign_launcher(&w);
    let pre = w.snap();
    let m = w.release("0.1.0", "a");
    let e = w
        .apply_with(&m, None, &Decisions::default(), &w.env())
        .unwrap_err();
    assert_eq!(e.exit_code(), 3);
    let msg = e.to_string();
    assert!(msg.contains("need an explicit decision"), "{msg}");
    assert!(
        msg.contains(&format!("--replace {}", launcher.display())),
        "{msg}"
    );
    assert!(msg.contains("--keep"), "{msg}");
    assert_eq!(w.snap(), pre, "nothing, not even a directory, was created");
}

#[test]
fn replace_backs_the_old_file_up_and_the_receipt_owns_the_backup() {
    let w = World::new(Kind::Explicit);
    let launcher = foreign_launcher(&w);
    let m = w.release("0.1.0", "a");
    let d = Decisions {
        replace: vec![launcher.to_string_lossy().into_owned()],
        keep: vec![],
    };
    let r = w.apply_with(&m, None, &d, &w.env()).unwrap();
    assert_eq!(r.backups.len(), 1);
    let backup = w.prefix().join("backups/1/launcher-fl");
    assert_eq!(
        fs::read_to_string(&backup).unwrap(),
        "#!/bin/sh\n# hand-built\n"
    );
    assert!(
        fs::symlink_metadata(&launcher)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    let rc = w.receipt();
    let owned = rc.owned_path(backup.to_str().unwrap()).unwrap();
    assert_eq!(
        owned.sha256.as_deref(),
        Some(fl_core::install::sha256_hex(b"#!/bin/sh\n# hand-built\n").as_str())
    );
    // a second backup lands in a new numbered directory
    let w2 = w;
    fs::remove_file(&launcher).unwrap();
    fs::write(&launcher, "again").unwrap();
    let m2 = w2.release("0.2.0", "b");
    let r2 = w2.apply_with(&m2, None, &d, &w2.env());
    // the launcher is now a plain file the receipt does not own, so it is a conflict again
    let r2 = r2.unwrap();
    assert!(
        w2.prefix().join("backups/2/launcher-fl").is_file(),
        "{:?}",
        r2.backups
    );
}

#[test]
fn keep_leaves_a_foreign_alias_alone_and_does_not_claim_it() {
    let w = World::new(Kind::Explicit);
    fs::create_dir_all(w.bin()).unwrap();
    let alias = w.bin().join("agent-msg");
    symlink("/somewhere/else", &alias).unwrap();
    let m = w.release("0.1.0", "a");
    let e = w
        .apply_with(&m, Some(Profile::Headless), &Decisions::default(), &w.env())
        .unwrap_err();
    assert_eq!(e.exit_code(), 3, "{e}");
    let d = Decisions {
        replace: vec![],
        keep: vec![alias.to_string_lossy().into_owned()],
    };
    let r = w
        .apply_with(&m, Some(Profile::Headless), &d, &w.env())
        .unwrap();
    assert_eq!(r.kept.len(), 1);
    assert_eq!(
        fs::read_link(&alias).unwrap(),
        PathBuf::from("/somewhere/else")
    );
    let rc = w.receipt();
    assert!(rc.owned_alias(alias.to_str().unwrap()).is_none());
    assert!(
        rc.owned_alias(w.bin().join("agent-bus-mcp").to_str().unwrap())
            .is_some()
    );
    // run again with the same decision: still a no-op
    let r = w
        .apply_with(&m, Some(Profile::Headless), &d, &w.env())
        .unwrap();
    assert_eq!(r.outcome, Outcome::AlreadyInstalled);
}

#[test]
fn keeping_the_launcher_skips_the_aliases_too() {
    let w = World::new(Kind::Explicit);
    let launcher = foreign_launcher(&w);
    let m = w.release("0.1.0", "a");
    let d = Decisions {
        replace: vec![],
        keep: vec![launcher.to_string_lossy().into_owned()],
    };
    let r = w
        .apply_with(&m, Some(Profile::Headless), &d, &w.env())
        .unwrap();
    assert!(!r.steps.contains(&Step::WriteLauncher) && !r.steps.contains(&Step::WriteAliases));
    assert_eq!(
        fs::read_to_string(&launcher).unwrap(),
        "#!/bin/sh\n# hand-built\n"
    );
    assert!(!w.bin().join("agent-msg").exists());
    assert!(w.receipt().aliases.is_empty());
}

#[test]
fn an_edited_owned_launcher_is_a_conflict_again() {
    let w = World::new(Kind::Explicit);
    let m = w.release("0.1.0", "a");
    w.install(&m);
    let launcher = w.bin().join("fl");
    fs::remove_file(&launcher).unwrap();
    symlink("/usr/bin/true", &launcher).unwrap();
    let e = w
        .apply_with(&m, None, &Decisions::default(), &w.env())
        .unwrap_err();
    assert_eq!(e.exit_code(), 3);
    assert!(e.to_string().contains("edited"), "{e}");
    assert_eq!(
        fs::read_link(&launcher).unwrap(),
        PathBuf::from("/usr/bin/true")
    );
}

#[test]
fn decisions_on_a_non_conflict_or_both_ways_are_a_usage_error() {
    let w = World::new(Kind::Explicit);
    let m = w.release("0.1.0", "a");
    let pre = w.snap();
    for d in [
        Decisions {
            replace: vec![w.bin().join("fl").to_string_lossy().into_owned()],
            keep: vec![],
        },
        Decisions {
            replace: vec!["/nowhere".into()],
            keep: vec!["/nowhere".into()],
        },
    ] {
        let e = w.apply_with(&m, None, &d, &w.env()).unwrap_err();
        assert_eq!(e.exit_code(), 2, "{e}");
    }
    assert_eq!(w.snap(), pre);
}

#[test]
fn a_foreign_unit_file_is_listed_not_executed_and_left_untouched() {
    let w = World::new(Kind::UnderRoot);
    let units = PathBuf::from(w.layout().unit_dir);
    fs::create_dir_all(&units).unwrap();
    let unit = units.join("ferroloop-hub.service");
    fs::write(&unit, "[Unit]\nDescription=mine\n").unwrap();
    let m = w.release("0.1.0", "a");
    let fake = FakeSystemctl::systemctl();
    let mut env = w.env();
    env.services = Some(Box::new(fake.clone()));
    let r = w
        .apply_with(&m, Some(Profile::Full), &Decisions::default(), &env)
        .unwrap();
    assert_eq!(
        fs::read_to_string(&unit).unwrap(),
        "[Unit]\nDescription=mine\n"
    );
    assert!(
        r.not_executed
            .iter()
            .any(|l| l.starts_with("not executed: register_service ferroloop-hub.service")),
        "{:?}",
        r.not_executed
    );
    assert!(r.not_executed.iter().any(|l| l.contains("enable_at_boot")));
    assert!(fake.executed().is_empty(), "no service command is ever run");
    assert!(
        !fake.planned().is_empty(),
        "but they were planned through the seam"
    );
    assert_eq!(
        fs::read_dir(&units).unwrap().count(),
        1,
        "no unit was written outside the release"
    );
    assert!(w.receipt().units.is_empty());
}

// ------------------------------------------------------------- refusals

#[test]
fn a_hash_mismatch_refuses_before_anything_exists() {
    let w = World::new(Kind::Explicit);
    let m = w.release("0.1.0", "a");
    let pre = w.snap();
    let name = m.artifacts[0].file.clone();
    fs::write(
        w.art.path().join(&name).join("bin/fl"),
        "tampered after the manifest was made",
    )
    .unwrap();
    let e = w
        .apply_with(&m, None, &Decisions::default(), &w.env())
        .unwrap_err();
    assert_eq!(e.exit_code(), 3);
    assert!(e.to_string().contains("larger than"), "{e}");
    assert_eq!(w.snap(), pre);
    // a same-size edit is caught by the hash
    fs::write(
        w.art.path().join(&name).join("bin/fl"),
        "#!/bin/sh\necho fl X\n",
    )
    .unwrap();
    let e = w
        .apply_with(&m, None, &Decisions::default(), &w.env())
        .unwrap_err();
    assert!(e.to_string().contains("hash mismatch"), "{e}");
    assert_eq!(w.snap(), pre);
}

#[test]
fn a_hash_mismatch_during_an_upgrade_keeps_the_working_release() {
    let w = World::new(Kind::Explicit);
    w.install(&w.release("0.1.0", "a"));
    let pre = w.snap();
    let m2 = w.release("0.2.0", "b");
    fs::write(
        w.art
            .path()
            .join(&m2.artifacts[0].file)
            .join("bin/ferrostore"),
        "bad",
    )
    .unwrap();
    assert!(
        w.apply_with(&m2, None, &Decisions::default(), &w.env())
            .is_err()
    );
    assert_eq!(w.snap(), pre);
}

#[test]
fn symlinks_and_special_files_in_an_artifact_are_refused() {
    let w = World::new(Kind::Explicit);
    let m = w.release("0.1.0", "a");
    let pre = w.snap();
    let tree = w.art.path().join(&m.artifacts[0].file);
    symlink("/etc/passwd", tree.join("bin/evil")).unwrap();
    let e = w
        .apply_with(&m, None, &Decisions::default(), &w.env())
        .unwrap_err();
    assert!(e.to_string().contains("symlink"), "{e}");
    fs::remove_file(tree.join("bin/evil")).unwrap();
    // a symlinked artifact root
    let real = w.art.path().join("real");
    fs::rename(&tree, &real).unwrap();
    symlink(&real, &tree).unwrap();
    let e = w
        .apply_with(&m, None, &Decisions::default(), &w.env())
        .unwrap_err();
    assert_eq!(e.exit_code(), 3, "{e}");
    assert_eq!(w.snap(), pre);
}

#[test]
fn path_traversal_names_are_refused_by_the_manifest_and_the_scan() {
    let w = World::new(Kind::Explicit);
    let pre = w.snap();
    for mutate in [
        (|m: &mut ReleaseManifest| m.artifacts[0].file = "../outside".into())
            as fn(&mut ReleaseManifest),
        |m| m.artifacts[0].file = "/etc".into(),
        |m| m.products[0].files[0].path = "../../bin/fl".into(),
        |m| m.products[0].files[0].path = "/abs/fl".into(),
        |m| m.products[0].files[0].path = "bin/./fl".into(),
    ] {
        let mut m = w.release("0.1.0", "a");
        mutate(&mut m);
        let e = w
            .apply_with(&m, None, &Decisions::default(), &w.env())
            .unwrap_err();
        assert_eq!(e.exit_code(), 3, "{e}");
        assert_eq!(w.snap(), pre);
    }
    // a name in the tree the manifest alphabet does not allow
    let m = w.release("0.1.0", "a");
    fs::write(
        w.art.path().join(&m.artifacts[0].file).join("bin/odd name"),
        "x",
    )
    .unwrap();
    let e = w
        .apply_with(&m, None, &Decisions::default(), &w.env())
        .unwrap_err();
    assert!(e.to_string().contains("artifact path"), "{e}");
    // a declared file that the artifact lacks
    let w2 = World::new(Kind::Explicit);
    let m = w2.release("0.1.0", "a");
    let tree = w2.art.path().join(&m.artifacts[0].file);
    fs::remove_file(tree.join("bin/fl")).unwrap();
    // (the manifest still pins the old digest, so this is a hash refusal)
    assert!(
        w2.apply_with(&m, None, &Decisions::default(), &w2.env())
            .is_err()
    );
}

#[test]
fn the_release_json_name_is_reserved() {
    let w = World::new(Kind::Explicit);
    let mut m = w.release("0.1.0", "a");
    m.products[0].files.push(fl_core::install::ProductFile {
        path: "release.json".into(),
        kind: fl_core::install::FileKind::UnitTemplate,
    });
    fs::write(
        w.art.path().join(&m.artifacts[0].file).join("release.json"),
        "x",
    )
    .unwrap();
    let m2 = {
        let mut m2 = m.clone();
        let s = scan_tree(&w.art.path().join(&m.artifacts[0].file), u64::MAX / 2).unwrap();
        m2.artifacts[0].sha256 = s.digest;
        m2.artifacts[0].size = s.total_bytes;
        m2
    };
    let e = w
        .apply_with(&m2, None, &Decisions::default(), &w.env())
        .unwrap_err();
    assert!(e.to_string().contains("release.json"), "{e}");
}

#[test]
fn a_symlinked_prefix_component_is_refused_and_nothing_is_written_through_it() {
    let w = World::new(Kind::UnderRoot);
    let elsewhere = tempfile::tempdir().unwrap();
    fs::create_dir_all(w.root()).unwrap();
    symlink(elsewhere.path(), w.root().join(".local")).unwrap();
    let m = w.release("0.1.0", "a");
    let e = w
        .apply_with(&m, None, &Decisions::default(), &w.env())
        .unwrap_err();
    assert_eq!(e.exit_code(), 3, "{e}");
    assert!(e.to_string().contains("symlink"), "{e}");
    assert_eq!(fs::read_dir(elsewhere.path()).unwrap().count(), 0);

    // the same for a component deeper down, and for the prefix directory itself
    let w = World::new(Kind::UnderRoot);
    fs::create_dir_all(w.root().join(".local/share")).unwrap();
    symlink(elsewhere.path(), w.root().join(".local/share/fl")).unwrap();
    let m = w.release("0.1.0", "a");
    assert!(
        w.apply_with(&m, None, &Decisions::default(), &w.env())
            .is_err()
    );
    assert_eq!(fs::read_dir(elsewhere.path()).unwrap().count(), 0);

    let w = World::new(Kind::Explicit);
    symlink(elsewhere.path(), w.root()).unwrap();
    let m = w.release("0.1.0", "a");
    let e = w
        .apply_with(&m, None, &Decisions::default(), &w.env())
        .unwrap_err();
    assert_eq!(e.exit_code(), 3, "{e}");
    assert_eq!(fs::read_dir(elsewhere.path()).unwrap().count(), 0);
}

#[test]
fn a_symlinked_releases_or_bin_directory_is_refused() {
    let elsewhere = tempfile::tempdir().unwrap();
    for sub in ["releases", "bin"] {
        let w = World::new(Kind::Explicit);
        fs::create_dir_all(w.prefix()).unwrap();
        symlink(elsewhere.path(), w.prefix().join(sub)).unwrap();
        let m = w.release("0.1.0", "a");
        let e = w
            .apply_with(&m, None, &Decisions::default(), &w.env())
            .unwrap_err();
        assert_eq!(e.exit_code(), 3, "{sub}: {e}");
        assert_eq!(fs::read_dir(elsewhere.path()).unwrap().count(), 0, "{sub}");
    }
}

#[test]
fn a_root_that_resolves_into_a_system_tree_is_refused() {
    let w = World::new(Kind::Explicit);
    let m = w.release("0.1.0", "a");
    let hop = w.base.path().join("hop");
    symlink("/etc", &hop).unwrap();
    for root in [
        hop.join("fl"),
        PathBuf::from("/usr/local/fl"),
        PathBuf::from("/"),
    ] {
        let mut env = w.env();
        env.declared_root = root.clone();
        let layout = Layout {
            prefix: root.join("p").to_string_lossy().into_owned(),
            bin_dir: root.join("b").to_string_lossy().into_owned(),
            unit_dir: root.join("u").to_string_lossy().into_owned(),
        };
        let mut req = w.request(None);
        req.layout = layout;
        let host = HostInfo::from_consts(std::env::consts::OS, std::env::consts::ARCH).unwrap();
        let e = apply(
            &env,
            &ApplyArgs {
                request: &req,
                manifest: &m,
                host,
                artifact_dir: Some(w.art.path()),
                decisions: &Decisions::default(),
            },
        )
        .unwrap_err();
        assert_eq!(e.exit_code(), 3, "{}: {e}", root.display());
    }
    // a layout path outside the declared root is refused too
    let mut req = w.request(None);
    req.layout = Layout {
        prefix: "/tmp/elsewhere/p".into(),
        bin_dir: "/tmp/elsewhere/b".into(),
        unit_dir: "/tmp/elsewhere/u".into(),
    };
    let host = HostInfo::from_consts(std::env::consts::OS, std::env::consts::ARCH).unwrap();
    let e = apply(
        &w.env(),
        &ApplyArgs {
            request: &req,
            manifest: &m,
            host,
            artifact_dir: Some(w.art.path()),
            decisions: &Decisions::default(),
        },
    )
    .unwrap_err();
    assert_eq!(e.exit_code(), 3, "{e}");
    assert!(!Path::new("/tmp/elsewhere").exists());
}

#[test]
fn a_read_only_root_is_a_clean_error_with_nothing_left_behind() {
    // Root can write anywhere: the refusal cannot be shown as root.
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let w = World::new(Kind::Explicit);
    let pre = w.snap();
    fs::set_permissions(w.base.path(), fs::Permissions::from_mode(0o555)).unwrap();
    let m = w.release("0.1.0", "a");
    let e = w
        .apply_with(&m, None, &Decisions::default(), &w.env())
        .unwrap_err();
    assert_eq!(e.exit_code(), 1, "{e}");
    assert!(e.to_string().contains("Permission denied"), "{e}");
    assert_eq!(w.snap(), pre);
    fs::set_permissions(w.base.path(), fs::Permissions::from_mode(0o755)).unwrap();

    // a prefix that exists but cannot be written: the lock cannot be created
    fs::create_dir_all(w.prefix()).unwrap();
    fs::set_permissions(w.prefix(), fs::Permissions::from_mode(0o555)).unwrap();
    let pre = w.snap();
    let e = w
        .apply_with(&m, None, &Decisions::default(), &w.env())
        .unwrap_err();
    assert_eq!(e.exit_code(), 1, "{e}");
    assert_eq!(w.snap(), pre);
    fs::set_permissions(w.prefix(), fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn a_source_build_manifest_is_refused_without_changing_anything() {
    let w = World::new(Kind::Explicit);
    let pre = w.snap();
    let m = fl_core::install::default_manifest();
    let e = w
        .apply_with(&m, None, &Decisions::default(), &w.env())
        .unwrap_err();
    assert_eq!(e.exit_code(), 3);
    assert!(
        e.to_string().contains("not implemented in this release"),
        "{e}"
    );
    assert_eq!(w.snap(), pre);
}

#[test]
fn installing_needs_an_artifact_dir() {
    let w = World::new(Kind::Explicit);
    let m = w.release("0.1.0", "a");
    let req = w.request(None);
    let host = HostInfo::from_consts(std::env::consts::OS, std::env::consts::ARCH).unwrap();
    let e = apply(
        &w.env(),
        &ApplyArgs {
            request: &req,
            manifest: &m,
            host,
            artifact_dir: None,
            decisions: &Decisions::default(),
        },
    )
    .unwrap_err();
    assert_eq!(e.exit_code(), 2, "{e}");
}

// ---------------------------------------------------------- lock and race

#[test]
fn a_live_lock_holder_is_never_robbed_and_nothing_changes() {
    let w = World::new(Kind::Explicit);
    w.install(&w.release("0.1.0", "a"));
    let pre = w.snap();
    let guard = acquire_lock(&w.prefix().join("install.lock"), Duration::ZERO, || Ok(())).unwrap();
    let mut env = w.env();
    env.lock_timeout = Duration::from_millis(150);
    let m2 = w.release("0.2.0", "b");
    let e = w
        .apply_with(&m2, None, &Decisions::default(), &env)
        .unwrap_err();
    assert!(
        matches!(e, InstallError::Busy { pid: Some(p) } if p == std::process::id()),
        "{e:?}"
    );
    assert_eq!(e.exit_code(), 3);
    assert!(e.to_string().contains("another install is running"));
    // the lock file is rewritten with the holder's pid only; everything else is unchanged
    let mut after = w.snap();
    let mut before = pre;
    after.remove("prefix/install.lock");
    before.remove("prefix/install.lock");
    assert_eq!(after, before);
    drop(guard);
}

#[test]
fn a_stale_lock_record_is_recovered() {
    let w = World::new(Kind::Explicit);
    fs::create_dir_all(w.prefix()).unwrap();
    fs::write(w.prefix().join("install.lock"), "4194999\n").unwrap();
    let r = w.install(&w.release("0.1.0", "a"));
    assert!(
        r.notes
            .iter()
            .any(|n| n.contains("stale") && n.contains("4194999")),
        "{:?}",
        r.notes
    );
    assert_eq!(r.outcome, Outcome::Installed);
}

#[test]
fn concurrent_installers_never_corrupt_the_install() {
    for same in [true, false] {
        let w = std::sync::Arc::new(World::new(Kind::Explicit));
        let ms: Vec<_> = (0..4)
            .map(|i| w.release(&format!("0.{}.0", if same { 1 } else { i + 1 }), "x"))
            .collect();
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let w = w.clone();
                let m = ms[i % ms.len()].clone();
                std::thread::spawn(move || {
                    let mut env = w.env();
                    env.lock_timeout = Duration::from_secs(60);
                    w.apply_with(&m, None, &Decisions::default(), &env)
                })
            })
            .collect();
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        for r in &results {
            assert!(r.is_ok(), "{:?}", r.as_ref().err());
        }
        let installed = results
            .iter()
            .filter(|r| r.as_ref().unwrap().outcome == Outcome::Installed)
            .count();
        let rc = w.receipt();
        if same {
            assert_eq!(installed, 1, "exactly one transaction did the work");
            assert_eq!(rc.journal.len(), 1);
        } else {
            assert_eq!(installed, rc.journal.len());
        }
        // consistent: receipt, current and launcher agree; the journal is closed
        let cur = fs::read_link(w.prefix().join("current")).unwrap();
        assert_eq!(cur, PathBuf::from(format!("releases/{}", rc.release.name)));
        assert!(w.bin().join("fl").is_file());
        assert!(!w.prefix().join("journal.jsonl").exists());
        let seqs: Vec<u64> = rc.journal.iter().map(|j| j.seq).collect();
        assert_eq!(seqs, (1..=seqs.len() as u64).collect::<Vec<_>>());
        for p in &rc.paths {
            assert!(
                fs::symlink_metadata(&p.path).is_ok(),
                "{} owned but missing",
                p.path
            );
        }
    }
}

#[test]
fn a_corrupt_journal_is_refused_not_guessed_at() {
    let w = World::new(Kind::Explicit);
    fs::create_dir_all(w.prefix()).unwrap();
    fs::write(w.prefix().join("journal.jsonl"), "garbage\nmore garbage\n").unwrap();
    let m = w.release("0.1.0", "a");
    let e = w
        .apply_with(&m, None, &Decisions::default(), &w.env())
        .unwrap_err();
    assert_eq!(e.exit_code(), 3, "{e}");
    assert!(e.to_string().contains("corrupt"));
    assert!(
        w.prefix().join("journal.jsonl").exists(),
        "left for a person to look at"
    );
    assert!(!w.prefix().join("releases").join("x").exists());
}

// ------------------------------------------------------ isolation, source

#[test]
fn the_installer_touches_only_the_declared_root() {
    let home = tempfile::tempdir().unwrap();
    let w = World::new(Kind::UnderRoot);
    let before = snapshot(home.path(), true);
    let mut env = w.env();
    env.home = Some(home.path().to_path_buf());
    w.apply_with(
        &w.release("0.1.0", "a"),
        Some(Profile::Full),
        &Decisions::default(),
        &env,
    )
    .unwrap();
    assert_eq!(snapshot(home.path(), true), before, "HOME is unchanged");
    // everything the install made is inside the root
    for e in fs::read_dir(w.base.path()).unwrap() {
        let e = e.unwrap();
        assert_eq!(e.file_name(), "root");
    }
}

#[test]
fn the_default_layout_expands_tilde_for_links_and_owns_them_by_real_target() {
    let home = tempfile::tempdir().unwrap();
    let w = World::new(Kind::Explicit);
    let m = w.release("0.1.0", "a");
    let layout = Layout::user_default();
    let mut env = Env::new(home.path().to_path_buf(), Some(home.path().to_path_buf()));
    env.lock_timeout = Duration::from_secs(10);
    let req = InstallRequest {
        profile: None,
        add: vec![],
        remove: vec![],
        layout,
        adopt: false,
    };
    let host = HostInfo::from_consts(std::env::consts::OS, std::env::consts::ARCH).unwrap();
    let args = ApplyArgs {
        request: &req,
        manifest: &m,
        host,
        artifact_dir: Some(w.art.path()),
        decisions: &Decisions::default(),
    };
    let r = apply(&env, &args).unwrap();
    assert_eq!(r.outcome, Outcome::Installed);
    let launcher = home.path().join(".local/bin/fl");
    let target = fs::read_link(&launcher).unwrap();
    assert!(!target.to_string_lossy().contains('~'), "{target:?}");
    assert_eq!(target, home.path().join(".local/share/fl/current/bin/fl"));
    assert!(launcher.is_file());
    // the receipt stores `~/` forms for paths and the real target for the link
    let rc = InstallReceipt::parse(
        &fs::read_to_string(home.path().join(".local/share/fl/receipt.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(rc.layout.prefix, "~/.local/share/fl");
    assert_eq!(
        rc.owned_path("~/.local/bin/fl")
            .unwrap()
            .link_target
            .as_deref(),
        target.to_str()
    );
    // and a re-run recognises its own work
    assert_eq!(
        apply(&env, &args).unwrap().outcome,
        Outcome::AlreadyInstalled
    );
    make_tree_writable(home.path());
}

#[test]
fn the_installer_source_has_no_process_spawning_outside_tests() {
    // Services are a seam: nothing in the installer's own code starts a process.
    for (name, src) in [
        ("engine", include_str!("engine.rs")),
        ("services", include_str!("services.rs")),
        ("safe", include_str!("safe.rs")),
        ("lock", include_str!("lock.rs")),
        ("journal", include_str!("journal.rs")),
        ("tree", include_str!("tree.rs")),
        ("fault", include_str!("fault.rs")),
    ] {
        let code = src.split("#[cfg(test)]").next().unwrap();
        for banned in ["process::Command", "Command::new", ".spawn("] {
            let hits: Vec<_> = code
                .lines()
                .filter(|l| l.contains(banned) && !l.trim_start().starts_with("//"))
                .collect();
            // `ServiceCommand::new("systemctl", ..)` is a description, not a process
            let hits: Vec<_> = hits
                .into_iter()
                .filter(|l| !l.contains("ServiceCommand"))
                .collect();
            assert!(hits.is_empty(), "{name} spawns a process: {hits:?}");
        }
    }
}

// ------------------------------------------- foreign changes during recovery

#[test]
fn rollback_never_removes_a_launcher_somebody_else_put_there_after_the_crash() {
    let w = World::new(Kind::Explicit);
    let m = w.release("0.1.0", "a");
    let e = w
        .install_faulty(
            &m,
            &Decisions::default(),
            Faults::crash(FaultPoint::after(Step::WriteLauncher)),
        )
        .unwrap_err();
    assert!(matches!(e, InstallError::Crashed(_)));
    // After the crash the user replaces our launcher link with their own.
    let launcher = w.bin().join("fl");
    fs::remove_file(&launcher).unwrap();
    symlink("/usr/bin/env", &launcher).unwrap();
    recover(&w.env(), &w.layout(), RecoverMode::Rollback).unwrap();
    assert_eq!(
        fs::read_link(&launcher).unwrap(),
        PathBuf::from("/usr/bin/env")
    );
    assert!(
        !w.prefix().join("current").exists(),
        "what was ours is gone"
    );
    assert!(
        !w.prefix()
            .join("releases")
            .join(fixture::release_name(&m))
            .exists()
    );
}

#[test]
fn finishing_an_install_never_overwrites_a_file_that_appeared_at_a_link() {
    let w = World::new(Kind::Explicit);
    let m = w.release("0.1.0", "a");
    let e = w
        .install_faulty(
            &m,
            &Decisions::default(),
            Faults::crash(FaultPoint::before(Step::WriteLauncher)),
        )
        .unwrap_err();
    assert!(matches!(e, InstallError::Crashed(_)));
    fs::create_dir_all(w.bin()).unwrap();
    fs::write(w.bin().join("fl"), "the user's own launcher\n").unwrap();
    // The resume cannot finish (the launcher slot is taken), so it undoes instead.
    let rep = recover(&w.env(), &w.layout(), RecoverMode::Auto).unwrap();
    assert_eq!(rep.action, RecoverAction::RolledBack, "{rep:?}");
    assert!(
        rep.detail.contains("could not be completed"),
        "{}",
        rep.detail
    );
    assert_eq!(
        fs::read_to_string(w.bin().join("fl")).unwrap(),
        "the user's own launcher\n"
    );
    assert!(!w.prefix().join("receipt.json").exists());
    assert!(!w.prefix().join("current").exists());
}

#[test]
fn an_edited_release_that_is_not_selected_can_be_replaced_with_a_backup() {
    let w = World::new(Kind::Explicit);
    let m1 = w.release("0.1.0", "a");
    let m2 = w.release("0.2.0", "b");
    w.install(&m1);
    w.install(&m2); // 0.2.0 is selected; 0.1.0 is an older release
    let n1 = fixture::release_name(&m1);
    let rel1 = w.prefix().join("releases").join(&n1);
    // someone edits a file of the old release (it is sealed read-only: they chmod first)
    fs::set_permissions(rel1.join("bin"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(rel1.join("bin/fl"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(rel1.join("bin/fl"), "edited").unwrap();
    let e = w
        .apply_with(&m1, None, &Decisions::default(), &w.env())
        .unwrap_err();
    assert_eq!(e.exit_code(), 3, "{e}");
    assert!(
        e.to_string()
            .contains(&format!("--replace {}", rel1.display())),
        "{e}"
    );
    let d = Decisions {
        replace: vec![rel1.to_string_lossy().into_owned()],
        keep: vec![],
    };
    let r = w.apply_with(&m1, None, &d, &w.env()).unwrap();
    assert_eq!(r.outcome, Outcome::Installed);
    assert_eq!(
        fs::read_to_string(rel1.join("bin/fl")).unwrap(),
        "#!/bin/sh\necho fl a\n"
    );
    let backup = w.prefix().join("backups/1").join(format!("release-{n1}"));
    assert_eq!(fs::read_to_string(backup.join("bin/fl")).unwrap(), "edited");
    assert!(w.receipt().owned_path(backup.to_str().unwrap()).is_some());
    make_tree_writable(&w.prefix());
}

#[test]
fn the_selected_release_cannot_be_replaced_while_it_is_selected() {
    let w = World::new(Kind::Explicit);
    let m1 = w.release("0.1.0", "a");
    w.install(&m1);
    let rel1 = w.prefix().join("releases").join(fixture::release_name(&m1));
    fs::set_permissions(rel1.join("bin"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(rel1.join("bin/fl"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(rel1.join("bin/fl"), "edited").unwrap();
    let pre = w.snap();
    let d = Decisions {
        replace: vec![rel1.to_string_lossy().into_owned()],
        keep: vec![],
    };
    let e = w.apply_with(&m1, None, &d, &w.env()).unwrap_err();
    assert_eq!(e.exit_code(), 3, "{e}");
    assert!(e.to_string().contains("selected"), "{e}");
    assert_eq!(w.snap(), pre);
}

// -------------------------------------------------- forged journals (security)

/// Crash a fresh install right after `begin`, then rewrite the journal: `edit` changes the
/// intent, and a `stage` and a `publish` line are appended so an undo would act on the release.
fn forged_world(edit: &dyn Fn(&mut serde_json::Value, &World)) -> World {
    let w = World::new(Kind::Explicit);
    let m = w.release("0.1.0", "a");
    let e = w
        .install_faulty(
            &m,
            &Decisions::default(),
            Faults::crash(FaultPoint::after(Step::Begin)),
        )
        .unwrap_err();
    assert!(matches!(e, InstallError::Crashed(_)));
    let jp = w.prefix().join("journal.jsonl");
    let text = fs::read_to_string(&jp).unwrap();
    let mut first: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
    edit(&mut first["intent"], &w);
    let mut out = format!("{first}\n");
    for (seq, op) in [(1, "stage"), (2, "publish")] {
        out.push_str(&format!(
            "{{\"v\":1,\"txn\":1,\"seq\":{seq},\"op\":\"{op}\"}}\n"
        ));
    }
    fs::write(&jp, out).unwrap();
    // sentinels: one outside the root, one inside the root but outside releases/
    fs::create_dir_all(w.base.path().join("outside")).unwrap();
    fs::write(w.prefix().join("sentinel"), "keep me").unwrap();
    w
}

#[test]
fn a_forged_but_parseable_journal_is_refused_and_touches_nothing() {
    type Edit = fn(&mut serde_json::Value, &World);
    let root = |w: &World| w.root().to_string_lossy().into_owned();
    let _ = root;
    let cases: Vec<(&str, Edit)> = vec![
        ("empty release name", |i, _| {
            i["release"]["name"] = "".into();
            i["release"]["version"] = "".into();
        }),
        ("release name ..", |i, _| i["release"]["name"] = "..".into()),
        ("release name ../x", |i, _| {
            i["release"]["name"] = "../x".into()
        }),
        ("release name absolute", |i, _| {
            i["release"]["name"] = "/tmp/x".into()
        }),
        ("release name leading dash", |i, _| {
            i["release"]["name"] = "-rf".into()
        }),
        ("release name unrelated to its version", |i, _| {
            i["release"]["name"] = "9.9.9+aaaaaaaaaaaa".into()
        }),
        ("created dir with ..", |i, w| {
            i["created_dirs"] = serde_json::json!([format!("{}/../outside", w.root().display())]);
        }),
        ("created dir outside the root", |i, w| {
            i["created_dirs"] = serde_json::json!([w.base.path().join("outside")]);
        }),
        ("created dir relative", |i, _| {
            i["created_dirs"] = serde_json::json!(["outside"]);
        }),
        ("launcher outside the root", |i, w| {
            i["launcher"] = serde_json::json!({
                "name": "fl",
                "path": w.base.path().join("outside/fl"),
                "target": "x", "write": true
            });
        }),
        ("launcher over the receipt", |i, w| {
            i["launcher"] = serde_json::json!({
                "name": "fl",
                "path": w.prefix().join("receipt.json"),
                "target": "x", "write": true
            });
        }),
        ("alias named with a slash", |i, w| {
            i["aliases"] = serde_json::json!([{
                "name": "../x",
                "path": format!("{}/../x", w.bin().display()),
                "target": "fl", "write": true
            }]);
        }),
        ("backup out of the backups directory", |i, w| {
            i["backups"] = serde_json::json!([{
                "path": w.prefix().join("sentinel"),
                "to": w.prefix().join("releases"),
                "kind": "file", "sha256": "a".repeat(64)
            }]);
        }),
        ("layout of another install", |i, _| {
            i["layout"]["prefix"] = "/somewhere/else".into()
        }),
        ("file path escaping the release", |i, _| {
            i["files"] = serde_json::json!([{
                "path": "../evil", "sha256": "a".repeat(64), "size": 1, "exec": false
            }]);
        }),
        ("previous current outside releases", |i, _| {
            i["prev_current"] = "../../x".into()
        }),
    ];
    for (name, edit) in cases {
        let w = forged_world(&edit);
        let before = w.snap();
        for what in ["resume", "rollback", "apply"] {
            let e = match what {
                "resume" => recover(&w.env(), &w.layout(), RecoverMode::Auto).unwrap_err(),
                "rollback" => recover(&w.env(), &w.layout(), RecoverMode::Rollback).unwrap_err(),
                _ => w
                    .apply_with(
                        &w.release("0.1.0", "a"),
                        None,
                        &Decisions::default(),
                        &w.env(),
                    )
                    .unwrap_err(),
            };
            assert_eq!(e.exit_code(), 3, "{name} / {what}: {e}");
            assert!(
                e.to_string().contains("not valid") || e.to_string().contains("different layout"),
                "{name} / {what}: {e}"
            );
            assert_eq!(w.snap(), before, "{name} / {what} touched the disk");
        }
        assert_eq!(
            fs::read_to_string(w.prefix().join("sentinel")).unwrap(),
            "keep me",
            "{name}"
        );
        assert!(w.base.path().join("outside").is_dir(), "{name}");
        assert!(w.prefix().join("releases").is_dir(), "{name}");
    }
}

#[test]
fn a_genuine_journal_still_recovers_after_validation() {
    // The same forging helper with no change: the journal is the real one plus two lines the
    // installer itself writes, so it is valid and the undo runs.
    let w = forged_world(&|_, _| {});
    let rep = recover(&w.env(), &w.layout(), RecoverMode::Rollback).unwrap();
    assert_eq!(rep.action, RecoverAction::RolledBack);
    assert_eq!(
        fs::read_to_string(w.prefix().join("sentinel")).unwrap(),
        "keep me"
    );
}

// ------------------------------------------------ a release dir that is not ours

#[test]
fn a_refused_publish_never_deletes_a_release_directory_that_is_not_ours() {
    let w = World::new(Kind::Explicit);
    w.install(&w.release("0.1.0", "first"));
    let pre = w.snap();
    let m2 = w.release("0.2.0", "second");
    let name2 = fixture::release_name(&m2);
    let e = w
        .install_faulty(
            &m2,
            &Decisions::default(),
            Faults::crash(FaultPoint::before(Step::Publish)),
        )
        .unwrap_err();
    assert!(matches!(e, InstallError::Crashed(_)));
    // After planning, somebody creates the directory the publish would rename onto.
    let foreign = w.prefix().join("releases").join(&name2);
    fs::create_dir(&foreign).unwrap();
    fs::write(foreign.join("theirs"), "not yours").unwrap();
    let rep = recover(&w.env(), &w.layout(), RecoverMode::Auto).unwrap();
    assert_eq!(rep.action, RecoverAction::RolledBack, "{rep:?}");
    assert_eq!(
        fs::read_to_string(foreign.join("theirs")).unwrap(),
        "not yours"
    );
    assert!(
        !w.prefix().join("releases/.stage-2").exists(),
        "the stage goes"
    );
    let mut after = w.snap();
    after.remove(&format!("prefix/releases/{name2}"));
    after.remove(&format!("prefix/releases/{name2}/theirs"));
    assert_eq!(after, pre, "everything else is the pre-state");
    // The old release is still the selected one.
    assert!(w.prefix().join("current/bin/fl").is_file());
}

#[test]
fn a_rollback_that_finds_its_own_published_release_removes_it() {
    let w = World::new(Kind::Explicit);
    w.install(&w.release("0.1.0", "first"));
    let pre = w.snap();
    let m2 = w.release("0.2.0", "second");
    for point in [
        FaultPoint::after(Step::Publish),
        FaultPoint::before(Step::LinkCurrent),
    ] {
        let e = w
            .install_faulty(&m2, &Decisions::default(), Faults::crash(point))
            .unwrap_err();
        assert!(matches!(e, InstallError::Crashed(_)));
        recover(&w.env(), &w.layout(), RecoverMode::Rollback).unwrap();
        assert_eq!(w.snap(), pre, "{point}");
    }
}

// ------------------------------------------------------ hostile pre-existing paths

fn outside() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    fs::write(d.path().join("precious"), "outside the root").unwrap();
    d
}

fn untouched(d: &tempfile::TempDir) {
    assert_eq!(
        fs::read_dir(d.path()).unwrap().count(),
        1,
        "something was written outside the root"
    );
    assert_eq!(
        fs::read_to_string(d.path().join("precious")).unwrap(),
        "outside the root"
    );
}

#[test]
fn a_hostile_current_is_refused_and_replaced_without_following_it() {
    for as_link in [true, false] {
        let out = outside();
        let w = World::new(Kind::Explicit);
        fs::create_dir_all(w.prefix()).unwrap();
        let cur = w.prefix().join("current");
        if as_link {
            symlink(out.path(), &cur).unwrap();
        } else {
            fs::create_dir(&cur).unwrap();
            fs::write(cur.join("theirs"), "a real directory").unwrap();
        }
        let m = w.release("0.1.0", "a");
        let pre = w.snap();
        let e = w
            .apply_with(&m, None, &Decisions::default(), &w.env())
            .unwrap_err();
        assert_eq!(e.exit_code(), 3, "{e}");
        assert_eq!(w.snap(), pre);
        untouched(&out);
        let d = Decisions {
            replace: vec![cur.to_string_lossy().into_owned()],
            keep: vec![],
        };
        w.apply_with(&m, None, &d, &w.env()).unwrap();
        untouched(&out);
        let backup = w.prefix().join("backups/1/current-current");
        if as_link {
            assert_eq!(fs::read_link(&backup).unwrap(), out.path());
        } else {
            assert_eq!(
                fs::read_to_string(backup.join("theirs")).unwrap(),
                "a real directory"
            );
        }
        assert!(w.prefix().join("current/bin/fl").is_file());
    }
}

#[test]
fn a_hostile_release_directory_is_refused_and_replaced_without_following_it() {
    let out = outside();
    let w = World::new(Kind::Explicit);
    let m = w.release("0.1.0", "a");
    let rel = w.prefix().join("releases").join(fixture::release_name(&m));
    fs::create_dir_all(w.prefix().join("releases")).unwrap();
    symlink(out.path(), &rel).unwrap();
    let pre = w.snap();
    let e = w
        .apply_with(&m, None, &Decisions::default(), &w.env())
        .unwrap_err();
    assert_eq!(e.exit_code(), 3, "{e}");
    assert_eq!(w.snap(), pre);
    untouched(&out);
    let d = Decisions {
        replace: vec![rel.to_string_lossy().into_owned()],
        keep: vec![],
    };
    w.apply_with(&m, None, &d, &w.env()).unwrap();
    untouched(&out);
    assert!(
        rel.join("bin/fl").is_file()
            && !fs::symlink_metadata(&rel).unwrap().file_type().is_symlink()
    );
}

#[test]
fn a_hostile_staging_name_is_refused_not_written_through() {
    for as_link in [true, false] {
        let out = outside();
        let w = World::new(Kind::Explicit);
        fs::create_dir_all(w.prefix().join("releases")).unwrap();
        let stage = w.prefix().join("releases/.stage-1");
        if as_link {
            symlink(out.path(), &stage).unwrap();
        } else {
            fs::create_dir(&stage).unwrap();
        }
        let pre = w.snap();
        let m = w.release("0.1.0", "a");
        let e = w
            .apply_with(&m, None, &Decisions::default(), &w.env())
            .unwrap_err();
        assert_eq!(e.exit_code(), 3, "{e}");
        assert!(e.to_string().contains("no journal owns it"), "{e}");
        untouched(&out);
        // the lock file this attempt made is removed again; everything else is as it was
        assert_eq!(w.snap(), pre);
    }
}
