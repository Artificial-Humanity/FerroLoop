//! `fl install --from-git` (#205 phase A4b), driven as a black box against a THROWAWAY git
//! repository and FAKE `cargo`, `rustc` and `npm` executables on a PATH the test constructs.
//! The real toolchain never runs, and every path (prefix, HOME, repository) is a temp dir.
//!
//! The fakes go through the real process-spawn path: `fl` spawns `cargo` as a child in the
//! sandboxed environment, so the allow-listed environment, the process group and the timeout
//! are exercised for real.

use assert_cmd::Command;
use fl_install::fixture::gitrepo::{self, FakeCargo, FakeTools, Repo};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// Every path under `dir`: kind, mode, size, link target or content, mtime.
fn snapshot(dir: &Path) -> BTreeMap<String, String> {
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
        let Ok(rd) = fs::read_dir(dir) else { return };
        for e in rd {
            let p = e.unwrap().path();
            let m = fs::symlink_metadata(&p).unwrap();
            let extra = if m.file_type().is_symlink() {
                fs::read_link(&p).unwrap().display().to_string()
            } else if m.is_file() {
                format!(
                    "{:?} {}",
                    m.modified().unwrap(),
                    fs::read(&p).map(|b| b.len()).unwrap_or(0)
                )
            } else {
                String::new()
            };
            out.insert(
                p.strip_prefix(base).unwrap().display().to_string(),
                format!("{:?} {:o} {extra}", m.file_type(), m.permissions().mode()),
            );
            if m.is_dir() {
                walk(base, &p, out);
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

struct World {
    home: tempfile::TempDir,
    base: tempfile::TempDir,
    repo: Repo,
    _repo_dir: tempfile::TempDir,
    tools: FakeTools,
    _tools_dir: tempfile::TempDir,
}

impl Drop for World {
    fn drop(&mut self) {
        fl_install::make_tree_writable(self.base.path());
        fl_install::make_tree_writable(self.home.path());
    }
}

impl World {
    fn new(mode: FakeCargo) -> Self {
        let repo_dir = tempfile::tempdir().unwrap();
        let repo = gitrepo::make_repo(&repo_dir.path().join("repo"));
        let tools_dir = tempfile::tempdir().unwrap();
        let tools = gitrepo::fake_tools(tools_dir.path(), mode);
        Self {
            home: tempfile::tempdir().unwrap(),
            base: tempfile::tempdir().unwrap(),
            repo,
            _repo_dir: repo_dir,
            tools,
            _tools_dir: tools_dir,
        }
    }

    fn prefix(&self) -> PathBuf {
        self.base.path().join("prefix")
    }

    fn sha12(&self, i: usize) -> String {
        self.repo.commits[i][..12].to_string()
    }

    fn release_name(&self, i: usize) -> String {
        format!("0.1.0+{}", self.sha12(i))
    }

    fn build_dir(&self, i: usize) -> PathBuf {
        self.prefix().join("build").join(self.sha12(i))
    }

    /// `fl` with a scrubbed environment and the constructed PATH.
    fn fl(&self) -> Command {
        let mut c = Command::cargo_bin("fl").unwrap();
        c.env_clear()
            .env("HOME", self.home.path())
            .env("PATH", self.tools.path())
            .current_dir(self.base.path());
        c
    }

    /// `fl install --from-git <rev> --repo <repo> --prefix <prefix> --profile headless` + extra.
    fn install_git(&self, rev: &str, extra: &[&str]) -> Command {
        self.install_git_with("headless", rev, extra)
    }

    fn install_git_with(&self, profile: &str, rev: &str, extra: &[&str]) -> Command {
        let mut c = self.fl();
        c.args(["install", "--profile", profile])
            .arg(format!("--from-git={rev}"))
            .arg("--repo")
            .arg(&self.repo.dir)
            .arg("--prefix")
            .arg(self.prefix())
            .args(extra);
        c
    }

    fn doctor(&self) -> Value {
        let out = self
            .fl()
            .args(["doctor", "--json", "--prefix"])
            .arg(self.prefix())
            .output()
            .unwrap();
        serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|e| panic!("not JSON ({e}): {}", String::from_utf8_lossy(&out.stdout)))
    }

    fn check_status(&self, id: &str) -> String {
        let v = self.doctor();
        v["checks"]
            .as_array()
            .unwrap_or_else(|| panic!("{v}"))
            .iter()
            .find(|c| c["id"] == id)
            .unwrap_or_else(|| panic!("no check {id} in {v}"))["status"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn receipt(&self) -> Value {
        serde_json::from_str(&fs::read_to_string(self.prefix().join("receipt.json")).unwrap())
            .unwrap()
    }
}

fn run(c: &mut Command) -> (i32, String, String) {
    let o = c.output().unwrap();
    (
        o.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&o.stdout).into_owned(),
        String::from_utf8_lossy(&o.stderr).into_owned(),
    )
}

#[test]
fn a_reviewed_commit_builds_and_installs() {
    let w = World::new(FakeCargo::Ok);
    let repo_before = snapshot(&w.repo.dir);
    let (c, so, se) = run(&mut w.install_git(&w.repo.commits[1], &["--yes"]));
    assert_eq!(c, 0, "{so}{se}");
    assert!(so.contains("installed release"), "{so}");
    assert!(so.contains("reviewed: an ancestor of origin/main"), "{so}");

    // the release is where A4a puts it, named <version>+<sha12>
    let rel = w.prefix().join("releases").join(w.release_name(1));
    assert!(rel.join("bin/fl").is_file());
    assert!(rel.join("bin/ferrostore").is_file());
    assert!(rel.join("contrib/units/ferrowire.service").is_file());
    assert!(!rel.join("ui").exists(), "no UI without --with-ui");
    assert_eq!(
        fs::read_link(w.prefix().join("current")).unwrap(),
        Path::new(&format!("releases/{}", w.release_name(1)))
    );
    // the installed `fl` is the one that was verified
    let out = std::process::Command::new(rel.join("bin/fl"))
        .args(["--version", "--json"])
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["commit"], w.repo.commits[1].as_str());

    // the receipt says: a source build, this commit, reviewed, and how it was built
    let r = w.receipt();
    assert_eq!(r["schema_version"], 2);
    assert_eq!(r["release"]["kind"], "source_build");
    assert_eq!(r["release"]["commit"], w.repo.commits[1].as_str());
    assert_eq!(r["release"]["build"]["reviewed"], true);
    assert_eq!(
        r["release"]["build"]["rustc"],
        "rustc 9.9.9 (fake 2026-01-01)"
    );
    assert_eq!(
        r["release"]["build"]["cargo"],
        "cargo 9.9.9 (fake 2026-01-01)"
    );
    assert_eq!(r["release"]["build"]["build_time_unix"], 1_700_000_200u64);
    let id = r["release"]["build"]["build_id"].as_str().unwrap();
    assert!(id.starts_with(&w.sha12(1)), "{id}");
    let rj: Value =
        serde_json::from_str(&fs::read_to_string(rel.join("release.json")).unwrap()).unwrap();
    assert_eq!(rj["kind"], "source_build");
    assert_eq!(rj["build"]["reviewed"], true);

    // no path of the source repository, the build directory or the fake toolchain anywhere
    for text in [
        fs::read_to_string(w.prefix().join("receipt.json")).unwrap(),
        fs::read_to_string(rel.join("release.json")).unwrap(),
    ] {
        assert!(!text.contains(w.repo.dir.to_str().unwrap()), "{text}");
        assert!(!text.contains("/build/"), "{text}");
    }

    // what cargo was asked, and where
    assert_eq!(
        fs::read_to_string(w.tools.args_dump()).unwrap().trim(),
        "build --release --locked -p fl-cli -p fl-store --bin fl --bin ferrostore"
    );
    assert_eq!(
        fs::read_to_string(w.tools.cwd_dump()).unwrap().trim(),
        w.build_dir(1).join("src").to_str().unwrap()
    );
    // the build directory is gone after success, and the source repository was not touched
    assert!(!w.prefix().join("build").exists());
    assert_eq!(snapshot(&w.repo.dir), repo_before);

    // doctor agrees
    assert_eq!(w.check_status("G7.receipt-integrity"), "pass");
    assert_eq!(w.check_status("G7.path-ownership"), "pass");
}

#[test]
fn a_second_run_is_idempotent_and_does_not_rebuild() {
    let w = World::new(FakeCargo::Ok);
    let (c, so, se) = run(&mut w.install_git(&w.repo.commits[1], &["--yes"]));
    assert_eq!(c, 0, "{so}{se}");
    assert_eq!(w.tools.builds(), 1);
    // (the lock file's mtime moves whenever an installer takes the lock)
    let stable = |w: &World| {
        let mut s = snapshot(&w.prefix());
        s.remove("install.lock");
        s
    };
    let after_first = stable(&w);
    let (c, so, se) = run(&mut w.install_git(&w.repo.commits[1], &["--yes"]));
    assert_eq!(c, 0, "{so}{se}");
    assert!(so.contains("already installed"), "{so}");
    assert!(so.contains("not rebuilt"), "{so}");
    assert_eq!(w.tools.builds(), 1, "the second run must not build");
    assert_eq!(stable(&w), after_first);
    assert_eq!(w.check_status("G7.receipt-integrity"), "pass");
}

// ------------------------------------------------------------- helpers

fn assert_nothing_installed(w: &World) {
    let p = w.prefix();
    assert!(!p.join("receipt.json").exists(), "a receipt exists");
    assert!(
        fs::symlink_metadata(p.join("current")).is_err(),
        "current exists"
    );
    let releases = p.join("releases");
    if releases.exists() {
        assert_eq!(
            fs::read_dir(&releases).unwrap().count(),
            0,
            "a release exists"
        );
    }
    assert!(!p.join("journal.jsonl").exists(), "a journal exists");
}

fn tools_in_place(w: &World, mode: FakeCargo) {
    let _ = gitrepo::fake_tools(w._tools_dir.path(), mode);
}

fn pid_gone(pid: u32) -> bool {
    for _ in 0..100 {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        let zombie = stat.rsplit(") ").next().is_some_and(|r| r.starts_with('Z'));
        if stat.is_empty() || zombie {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    false
}

// ------------------------------------------------------- the review rule

#[test]
fn a_commit_that_is_not_an_ancestor_of_origin_main_is_refused() {
    let w = World::new(FakeCargo::Ok);
    let (c, so, se) = run(&mut w.install_git(&w.repo.commits[2], &["--yes"]));
    assert_eq!(c, 3, "{so}{se}");
    assert!(se.contains("is not an ancestor of origin/main"), "{se}");
    assert!(se.contains("--allow-unreviewed"), "{se}");
    assert_eq!(w.tools.builds(), 0);
    assert!(!w.prefix().exists(), "a refusal writes nothing");
    // and a dry run is refused the same way
    let (c, _, se) = run(&mut w.install_git(&w.repo.commits[2], &["--dry-run"]));
    assert_eq!(c, 3, "{se}");
}

#[test]
fn an_unreviewed_commit_installs_only_with_the_flag_and_is_recorded_as_such() {
    let w = World::new(FakeCargo::Ok);
    let (c, so, se) = run(&mut w.install_git(&w.repo.commits[2], &["--yes", "--allow-unreviewed"]));
    assert_eq!(c, 0, "{so}{se}");
    assert!(so.contains("WARNING"), "{so}");
    assert!(so.contains("UNREVIEWED"), "{so}");
    let r = w.receipt();
    assert_eq!(r["release"]["kind"], "source_build");
    assert_eq!(r["release"]["build"]["reviewed"], false);
    assert_eq!(r["release"]["commit"], w.repo.commits[2].as_str());
    // doctor still reads the receipt, and warns about the unreviewed build
    assert_eq!(w.check_status("G7.receipt-integrity"), "warn");
    let v = w.doctor();
    let detail = v["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "G7.receipt-integrity")
        .unwrap()["detail"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        detail.contains("NOT an ancestor of origin/main"),
        "{detail}"
    );
    let rj: Value = serde_json::from_str(
        &fs::read_to_string(
            w.prefix()
                .join("releases")
                .join(w.release_name(2))
                .join("release.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(rj["build"]["reviewed"], false);
}

#[test]
fn the_flag_does_not_change_what_a_reviewed_commit_is() {
    let w = World::new(FakeCargo::Ok);
    let (c, so, se) = run(&mut w.install_git(&w.repo.commits[1], &["--yes", "--allow-unreviewed"]));
    assert_eq!(c, 0, "{so}{se}");
    assert_eq!(w.receipt()["release"]["build"]["reviewed"], true);
    assert_eq!(w.check_status("G7.receipt-integrity"), "pass");
}

#[test]
fn without_origin_main_nothing_is_reviewed() {
    let w = World::new(FakeCargo::Ok);
    gitrepo::git(
        &w.repo.dir,
        &["update-ref", "-d", "refs/remotes/origin/main"],
    );
    let (c, _, se) = run(&mut w.install_git(&w.repo.commits[0], &["--yes"]));
    assert_eq!(c, 3, "{se}");
    assert!(se.contains("origin/main is absent"), "{se}");
    assert_eq!(w.tools.builds(), 0);
    assert!(!w.prefix().exists());
    let (c, so, se) = run(&mut w.install_git(&w.repo.commits[0], &["--yes", "--allow-unreviewed"]));
    assert_eq!(c, 0, "{so}{se}");
    assert!(
        so.contains("does not exist here"),
        "the note says nothing was fetched: {so}"
    );
    assert_eq!(w.receipt()["release"]["build"]["reviewed"], false);
}

#[test]
fn a_stale_origin_main_is_noted_and_nothing_is_fetched() {
    let w = World::new(FakeCargo::Ok);
    // the fixture's commits are from 2023: origin/main is very old
    let (c, so, se) = run(&mut w.install_git(&w.repo.commits[1], &["--dry-run"]));
    assert_eq!(c, 0, "{so}{se}");
    assert!(
        so.contains("may be stale") && so.contains("never fetches"),
        "{so}"
    );
}

// -------------------------------------------------------- the revision

#[test]
fn a_revision_cannot_be_an_option_or_carry_whitespace() {
    let w = World::new(FakeCargo::Ok);
    let victim = w.base.path().join("injected");
    for rev in [
        format!("--output={}", victim.display()),
        "-x".to_string(),
        "-".to_string(),
        "a b".to_string(),
        "HEAD\tx".to_string(),
        "HEAD\nx".to_string(),
        "".to_string(),
        "^HEAD".to_string(),
        "HEAD..main".to_string(),
    ] {
        let (c, so, se) = run(&mut w.install_git(&rev, &["--dry-run"]));
        assert_eq!(c, 2, "{rev:?}: {so}{se}");
        assert!(se.contains("revision"), "{rev:?}: {se}");
    }
    assert!(!victim.exists());
    assert!(!w.prefix().exists());
    assert_eq!(w.tools.builds(), 0);
    // a well-formed name that is not a commit is a refusal
    let (c, _, se) = run(&mut w.install_git("no-such-ref", &["--dry-run"]));
    assert_eq!(c, 3, "{se}");
    assert!(se.contains("does not name a commit"), "{se}");
}

#[test]
fn the_revision_may_be_a_branch_a_tag_or_relative() {
    let w = World::new(FakeCargo::Ok);
    gitrepo::git(&w.repo.dir, &["tag", "v1", &w.repo.commits[0]]);
    for (rev, want) in [("v1", 0), ("HEAD~1", 1), ("origin/main", 1)] {
        let (c, so, se) = run(&mut w.install_git(rev, &["--dry-run", "--json"]));
        assert_eq!(c, 0, "{rev}: {so}{se}");
        let v: Value = serde_json::from_str(&so).unwrap();
        assert_eq!(
            v["from_git"]["commit"],
            w.repo.commits[want].as_str(),
            "{rev}"
        );
    }
}

#[test]
fn the_repo_defaults_to_the_current_directory_and_must_be_a_checkout() {
    let w = World::new(FakeCargo::Ok);
    fs::create_dir_all(w.repo.dir.join("docs/deeper")).unwrap();
    let mut c = w.fl();
    c.current_dir(w.repo.dir.join("docs/deeper"))
        .args([
            "install",
            "--dry-run",
            "--profile",
            "minimal",
            "--from-git",
            "HEAD~1",
            "--prefix",
        ])
        .arg(w.prefix());
    let (code, so, se) = run(&mut c);
    assert_eq!(code, 0, "{so}{se}");
    assert!(so.contains(&w.repo.commits[1]), "{so}");

    let elsewhere = tempfile::tempdir().unwrap();
    let mut c = w.fl();
    c.current_dir(elsewhere.path())
        .args(["install", "--dry-run", "--from-git", "HEAD", "--prefix"])
        .arg(w.prefix());
    let (code, _, se) = run(&mut c);
    assert_eq!(code, 3, "{se}");
    assert!(se.contains("not inside a git checkout"), "{se}");
}

#[test]
fn the_build_directory_may_not_be_inside_the_repository_or_hold_it() {
    let w = World::new(FakeCargo::Ok);
    for prefix in [w.repo.dir.join("inst"), w.repo.dir.clone()] {
        let mut c = w.fl();
        c.args(["install", "--dry-run", "--from-git", "HEAD~1", "--repo"])
            .arg(&w.repo.dir)
            .arg("--prefix")
            .arg(&prefix);
        let (code, so, se) = run(&mut c);
        assert_eq!(code, 3, "{prefix:?}: {so}{se}");
        assert!(se.contains("overlap"), "{se}");
    }
    assert!(!w.repo.dir.join("inst").exists());
}

// ------------------------------------------------------- plan, dry run

#[test]
fn a_dry_run_plans_without_building_and_writes_nothing() {
    let w = World::new(FakeCargo::Ok);
    let base_before = snapshot(w.base.path());
    let home_before = snapshot(w.home.path());
    let repo_before = snapshot(&w.repo.dir);
    let (c, so, se) = run(&mut w.install_git(&w.repo.commits[1], &["--dry-run"]));
    assert_eq!(c, 0, "{so}{se}");
    assert!(so.contains(&w.repo.commits[1]), "{so}");
    assert!(so.contains("reviewed: an ancestor of origin/main"), "{so}");
    assert!(
        so.contains(
            "cargo build --release --locked -p fl-cli -p fl-store --bin fl --bin ferrostore"
        ),
        "{so}"
    );
    assert!(so.contains(&w.build_dir(1).display().to_string()), "{so}");
    assert!(so.contains("source_build"), "{so}");
    assert!(so.contains("ui        not built"), "{so}");
    assert_eq!(w.tools.builds(), 0);
    assert_eq!(snapshot(w.base.path()), base_before);
    assert_eq!(snapshot(w.home.path()), home_before);
    assert_eq!(snapshot(&w.repo.dir), repo_before);

    // JSON carries the same facts
    let (c, so, se) = run(&mut w.install_git(&w.repo.commits[1], &["--dry-run", "--json"]));
    assert_eq!(c, 0, "{so}{se}");
    let v: Value = serde_json::from_str(&so).unwrap();
    assert_eq!(v["from_git"]["reviewed"], true);
    assert_eq!(v["from_git"]["version"], "0.1.0");
    assert_eq!(v["plan"]["release"]["kind"], "source_build");
    assert_eq!(v["plan"]["release"]["build"]["reviewed"], true);
    assert_eq!(snapshot(w.base.path()), base_before);
}

#[test]
fn without_yes_the_plan_is_printed_and_the_exit_is_3() {
    let w = World::new(FakeCargo::Ok);
    let (c, so, se) = run(&mut w.install_git(&w.repo.commits[1], &[]));
    assert_eq!(c, 3, "{so}{se}");
    assert!(so.contains("cargo build --release --locked"), "{so}");
    assert!(se.contains("pass --yes"), "{se}");
    assert_eq!(w.tools.builds(), 0);
    assert!(!w.prefix().exists());
}

#[test]
fn from_git_cannot_be_combined_with_other_sources() {
    let w = World::new(FakeCargo::Ok);
    for extra in [
        vec!["--artifact-dir", "/x"],
        vec!["--manifest", "/x"],
        vec!["--resume"],
        vec!["--rollback"],
    ] {
        let (c, _, se) = run(&mut w.install_git("HEAD", &extra));
        assert_eq!(c, 2, "{extra:?}: {se}");
    }
    // the from-git flags alone are a usage error
    let (c, _, se) = run(w.fl().args(["install", "--dry-run", "--with-ui"]));
    assert_eq!(c, 2, "{se}");
    let (c, _, se) = run(w.fl().args(["install", "--dry-run", "--allow-unreviewed"]));
    assert_eq!(c, 2, "{se}");
}

#[test]
fn a_conflict_is_found_before_anything_is_built() {
    let w = World::new(FakeCargo::Ok);
    fs::create_dir_all(w.prefix().join("bin")).unwrap();
    fs::write(w.prefix().join("bin/fl"), "somebody else's launcher").unwrap();
    let (c, so, se) = run(&mut w.install_git(&w.repo.commits[1], &["--yes"]));
    assert_eq!(c, 3, "{so}{se}");
    assert!(se.contains("--replace") && se.contains("--keep"), "{se}");
    assert_eq!(w.tools.builds(), 0, "a refusal must not cost a build");
    assert!(!w.prefix().join("build").exists());
    assert_eq!(
        fs::read_to_string(w.prefix().join("bin/fl")).unwrap(),
        "somebody else's launcher"
    );
}

// ------------------------------------------------------------ the build

#[test]
fn a_failed_build_keeps_its_directory_and_installs_nothing() {
    let w = World::new(FakeCargo::Fail);
    let (c, so, se) = run(&mut w.install_git(&w.repo.commits[1], &["--yes"]));
    assert_eq!(c, 1, "{so}{se}");
    let dir = w.build_dir(1);
    assert!(se.contains("the build failed"), "{se}");
    assert!(
        se.contains(&dir.join("build.log").display().to_string()),
        "{se}"
    );
    assert!(se.contains("is kept for diagnosis"), "{se}");
    assert!(
        fs::read_to_string(dir.join("build.log"))
            .unwrap()
            .contains("boom: could not compile")
    );
    assert!(
        dir.join(".fl-install-build").is_file(),
        "the marker says it is ours"
    );
    assert_nothing_installed(&w);
    assert_eq!(w.tools.builds(), 1);

    // the next attempt cleans the stale directory (its marker's installer is gone) and succeeds
    tools_in_place(&w, FakeCargo::Ok);
    let (c, so, se) = run(&mut w.install_git(&w.repo.commits[1], &["--yes"]));
    assert_eq!(c, 0, "{so}{se}");
    assert!(!dir.exists());
    assert_eq!(w.check_status("G7.receipt-integrity"), "pass");
}

#[test]
fn a_directory_the_installer_did_not_make_is_never_cleaned() {
    let w = World::new(FakeCargo::Ok);
    let dir = w.build_dir(1);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("precious"), "keep me").unwrap();
    let (c, _, se) = run(&mut w.install_git(&w.repo.commits[1], &["--yes"]));
    assert_eq!(c, 3, "{se}");
    assert!(
        se.contains("not a build directory this installer made"),
        "{se}"
    );
    assert_eq!(fs::read_to_string(dir.join("precious")).unwrap(), "keep me");
    assert_eq!(w.tools.builds(), 0);

    // a marker for another commit does not make it ours either
    fs::write(
        dir.join(".fl-install-build"),
        format!(
            "fl-install build directory v1\nsha={}\npid=1\n",
            "b".repeat(40)
        ),
    )
    .unwrap();
    let (c, _, _) = run(&mut w.install_git(&w.repo.commits[1], &["--yes"]));
    assert_eq!(c, 3);
    assert_eq!(fs::read_to_string(dir.join("precious")).unwrap(), "keep me");

    // and a live installer's directory is busy, not stale
    fs::write(
        dir.join(".fl-install-build"),
        format!(
            "fl-install build directory v1\nsha={}\npid={}\n",
            w.repo.commits[1],
            std::process::id()
        ),
    )
    .unwrap();
    let (c, _, se) = run(&mut w.install_git(&w.repo.commits[1], &["--yes"]));
    assert_eq!(c, 3, "{se}");
    assert!(se.contains("another install is running"), "{se}");
    assert_eq!(fs::read_to_string(dir.join("precious")).unwrap(), "keep me");
    assert_eq!(w.tools.builds(), 0);
}

#[test]
fn a_binary_that_reports_another_commit_is_refused() {
    let w = World::new(FakeCargo::WrongCommit);
    let (c, so, se) = run(&mut w.install_git(&w.repo.commits[1], &["--yes"]));
    assert_eq!(c, 3, "{so}{se}");
    assert!(se.contains("reports commit"), "{se}");
    assert!(se.contains(&w.repo.commits[1]), "{se}");
    assert!(se.contains("is kept for diagnosis"), "{se}");
    assert_nothing_installed(&w);
    assert!(w.build_dir(1).join("build.log").exists());
}

#[test]
fn a_binary_that_reports_a_dirty_tree_is_refused() {
    let w = World::new(FakeCargo::Dirty);
    let (c, so, se) = run(&mut w.install_git(&w.repo.commits[1], &["--yes"]));
    assert_eq!(c, 3, "{so}{se}");
    assert!(se.contains("dirty"), "{se}");
    assert_nothing_installed(&w);
}

#[test]
fn a_build_that_outlives_its_timeout_is_killed_with_everything_it_started() {
    let w = World::new(FakeCargo::Hang);
    let t0 = std::time::Instant::now();
    let (c, so, se) =
        run(&mut w.install_git(&w.repo.commits[1], &["--yes", "--build-timeout", "1"]));
    assert_eq!(c, 1, "{so}{se}");
    assert!(t0.elapsed().as_secs() < 60);
    assert!(se.contains("timed out and was killed"), "{se}");
    let pid: u32 = fs::read_to_string(w.tools.pid_file())
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(
        pid_gone(pid),
        "the background child {pid} outlived the timeout"
    );
    assert!(w.build_dir(1).join(".fl-install-build").exists());
    assert_nothing_installed(&w);
}

#[test]
fn a_missing_toolchain_is_a_refusal_that_leaves_no_trace() {
    let w = World::new(FakeCargo::Ok);
    let only_git = tempfile::tempdir().unwrap();
    let git = std::env::split_paths(&std::ffi::OsString::from(gitrepo::system_path()))
        .map(|d| d.join("git"))
        .find(|p| p.is_file())
        .unwrap();
    std::os::unix::fs::symlink(git, only_git.path().join("git")).unwrap();
    let mut c = w.install_git(&w.repo.commits[1], &["--yes"]);
    c.env("PATH", only_git.path());
    let (code, so, se) = run(&mut c);
    assert_eq!(code, 3, "{so}{se}");
    assert!(se.contains("cargo is not on PATH"), "{se}");
    assert!(
        !w.prefix().exists(),
        "the directories made for the build are removed again"
    );
}

// -------------------------------------------------------- the sandbox

#[test]
fn the_build_sees_only_allow_listed_environment() {
    let w = World::new(FakeCargo::Ok);
    let mut c = w.install_git(&w.repo.commits[1], &["--yes"]);
    c.env("FL_TOKEN", "secret-1")
        .env("GITHUB_TOKEN", "secret-2")
        .env("AWS_ACCESS_KEY_ID", "secret-3")
        .env("AWS_SECRET_ACCESS_KEY", "secret-4")
        .env("FL_VAULT_KEY", "secret-5")
        .env("FL_VAULT_PASSPHRASE", "secret-6")
        .env("CARGO_REGISTRY_TOKEN", "secret-7")
        .env("ANTHROPIC_API_KEY", "secret-8")
        .env("RUSTFLAGS", "-Cevil")
        .env("FL_RELEASE_ID", "forged")
        .env("GIT_DIR", "/nonexistent")
        .env("GIT_SSH_COMMAND", "evil")
        .env("CARGO_HOME", "/opt/cargo-home")
        .env("SOURCE_DATE_EPOCH", "42");
    let (code, so, se) = run(&mut c);
    assert_eq!(code, 0, "{so}{se}");
    let dump = fs::read_to_string(w.tools.env_dump()).unwrap();
    assert!(
        !dump.contains("secret-"),
        "a secret reached the build:\n{dump}"
    );
    let vars: BTreeMap<String, String> = dump
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    // the shell itself adds a few
    let shell = ["PWD", "SHLVL", "_", "OLDPWD"];
    for k in vars.keys() {
        assert!(
            fl_install::env_name_allowed(k) || shell.contains(&k.as_str()),
            "{k} reached the build but is not on the allow-list"
        );
    }
    for banned in [
        "FL_TOKEN",
        "GITHUB_TOKEN",
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "FL_VAULT_KEY",
        "FL_VAULT_PASSPHRASE",
        "CARGO_REGISTRY_TOKEN",
        "ANTHROPIC_API_KEY",
        "RUSTFLAGS",
        "FL_RELEASE_ID",
        "GIT_DIR",
        "GIT_SSH_COMMAND",
    ] {
        assert!(!vars.contains_key(banned), "{banned} leaked");
    }
    // what is passed through, and what the installer sets
    assert_eq!(vars["CARGO_HOME"], "/opt/cargo-home");
    assert_eq!(
        vars["SOURCE_DATE_EPOCH"], "1700000200",
        "the commit time, not the caller's 42"
    );
    let build = w.build_dir(1);
    assert_eq!(vars["HOME"], build.join("home").display().to_string());
    assert_eq!(vars["TMPDIR"], build.join("tmp").display().to_string());
    assert_eq!(vars["GIT_CONFIG_GLOBAL"], "/dev/null");
    assert_eq!(vars["GIT_CONFIG_NOSYSTEM"], "1");
    assert_eq!(vars["CARGO_INCREMENTAL"], "0");
    assert!(
        !vars["PATH"].split(':').any(|d| !d.starts_with('/')),
        "{}",
        vars["PATH"]
    );
}

// ----------------------------------------------- hostile repositories

fn exe_script(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// Arm the repository's own config and hooks, and the user's global config, so that every
/// program git can be told to run drops a sentinel file. Returns the sentinel directory.
fn arm(w: &World) -> PathBuf {
    let s = w.base.path().join("sentinels");
    let scripts = w.base.path().join("scripts");
    fs::create_dir_all(&s).unwrap();
    fs::create_dir_all(&scripts).unwrap();
    let touch = |name: &str| {
        let p = scripts.join(name);
        exe_script(
            &p,
            &format!(
                "touch '{}/{name}'; cat >/dev/null 2>&1; exit 0",
                s.display()
            ),
        );
        p
    };
    let hooks = w.base.path().join("hooks");
    fs::create_dir_all(&hooks).unwrap();
    let repo_hooks = w.repo.dir.join(".git/hooks");
    fs::create_dir_all(&repo_hooks).unwrap();
    for h in [
        "pre-commit",
        "post-commit",
        "post-checkout",
        "post-merge",
        "reference-transaction",
        "post-rewrite",
        "pre-auto-gc",
        "post-index-change",
        "fsmonitor-watchman",
    ] {
        for dir in [&hooks, &repo_hooks] {
            exe_script(
                &dir.join(h),
                &format!("touch '{}/hook-{h}'; exit 0", s.display()),
            );
        }
    }
    let cfg = format!(
        "[core]\n\tfsmonitor = {fsmon}\n\thooksPath = {hooks}\n\tpager = {pager}\n\tsshCommand = {ssh}\n\
         [log]\n\tshowSignature = true\n\
         [gpg]\n\tprogram = {gpg}\n\
         [filter \"evil\"]\n\tsmudge = {smudge}\n\tclean = {clean}\n\trequired = true\n\
         [diff]\n\texternal = {diff}\n\
         [uploadpack]\n\tpackObjectsHook = {pack}\n\
         [core]\n\talternateRefsCommand = {altrefs}\n",
        fsmon = touch("fsmonitor").display(),
        hooks = hooks.display(),
        pager = touch("pager").display(),
        ssh = touch("ssh").display(),
        gpg = touch("gpg").display(),
        smudge = touch("smudge").display(),
        clean = touch("clean").display(),
        diff = touch("diff").display(),
        pack = touch("pack").display(),
        altrefs = touch("altrefs").display(),
    );
    let config = w.repo.dir.join(".git/config");
    let mut existing = fs::read_to_string(&config).unwrap();
    existing.push_str(&cfg);
    fs::write(&config, existing).unwrap();
    // the same keys in the user's global config
    let global = format!(
        "[core]\n\tfsmonitor = {}\n\thooksPath = {}\n[log]\n\tshowSignature = true\n[gpg]\n\tprogram = {}\n",
        touch("g-fsmonitor").display(),
        hooks.display(),
        touch("g-gpg").display(),
    );
    fs::write(w.home.path().join(".gitconfig"), global).unwrap();
    s
}

fn sentinels(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir)
        .map(|rd| {
            rd.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

fn clear(dir: &Path) {
    for n in sentinels(dir) {
        fs::remove_file(dir.join(n)).unwrap();
    }
}

/// Plain git against the armed repository, with the user's HOME, as a careless tool would.
fn careless_git(w: &World, args: &[&str]) {
    let o = std::process::Command::new("git")
        .arg("-C")
        .arg(&w.repo.dir)
        .args(args)
        .env_clear()
        .env("PATH", gitrepo::system_path())
        .env("HOME", w.home.path())
        .env("GIT_AUTHOR_NAME", "x")
        .env("GIT_AUTHOR_EMAIL", "x@x")
        .env("GIT_COMMITTER_NAME", "x")
        .env("GIT_COMMITTER_EMAIL", "x@x")
        .output()
        .unwrap();
    let _ = o;
}

#[test]
fn a_hostile_repository_runs_nothing() {
    let w = World::new(FakeCargo::Ok);
    let s = arm(&w);

    // The controls: the armed config really does run programs when git is used carelessly.
    careless_git(&w, &["status"]);
    careless_git(&w, &["log", "-1", "--format=%ct", &w.repo.commits[1]]);
    careless_git(&w, &["cat-file", "--filters", "HEAD:stamp.txt"]);
    careless_git(&w, &["update-ref", "refs/tmp/probe", "HEAD"]);
    careless_git(&w, &["update-ref", "-d", "refs/tmp/probe"]);
    let armed = sentinels(&s);
    for want in ["fsmonitor", "gpg", "smudge", "hook-reference-transaction"] {
        assert!(
            armed.iter().any(|n| n == want),
            "control {want} did not fire: {armed:?}"
        );
    }
    clear(&s);
    let repo_before = snapshot(&w.repo.dir);

    let (c, so, se) = run(&mut w.install_git(&w.repo.commits[1], &["--yes"]));
    assert_eq!(c, 0, "{so}{se}");
    assert_eq!(
        sentinels(&s),
        Vec::<String>::new(),
        "a hostile hook, filter or config ran"
    );
    assert_eq!(
        snapshot(&w.repo.dir),
        repo_before,
        "the source repository was modified"
    );

    // the build tree held the committed bytes, not an attribute-processed checkout
    assert_eq!(
        fs::read_to_string(w.tools.src_dump().join("stamp.txt")).unwrap(),
        "$Format:%H$ $Id$\n"
    );
    assert_eq!(
        fs::read_to_string(w.tools.src_dump().join("gitattributes")).unwrap(),
        "stamp.txt export-subst ident\n* filter=evil\n"
    );
    // the installed result is the verified one
    assert_eq!(w.check_status("G7.receipt-integrity"), "pass");
}

#[test]
fn symlinks_and_submodules_in_the_commit_are_not_written() {
    let w = World::new(FakeCargo::Ok);
    std::os::unix::fs::symlink("/etc/passwd", w.repo.dir.join("evil-link")).unwrap();
    gitrepo::git(&w.repo.dir, &["add", "-A"]);
    gitrepo::git(&w.repo.dir, &["commit", "-q", "-m", "link"]);
    let c = gitrepo::git(&w.repo.dir, &["rev-parse", "HEAD"]);
    gitrepo::git(&w.repo.dir, &["update-ref", "refs/remotes/origin/main", &c]);
    let (code, so, se) = run(&mut w.install_git(&c, &["--yes"]));
    assert_eq!(code, 0, "{so}{se}");
    let ls = fs::read_to_string(w.tools.src_dump().join("ls.txt")).unwrap();
    assert!(!ls.contains("evil-link"), "{ls}");
    assert!(ls.contains("Cargo.toml"), "{ls}");
    assert!(ls.contains(".git"), "{ls}");
}

// ------------------------------------------------------------------- UI

#[test]
fn the_ui_is_built_only_on_request_and_installed_with_the_deck() {
    let w = World::new(FakeCargo::Ok);
    let c4 = gitrepo::add_ui_commit(&w.repo);
    // default: no UI, even for a profile with the deck
    let (c, so, se) = run(&mut w.install_git_with("full", &c4, &["--yes"]));
    assert_eq!(c, 0, "{so}{se}");
    let rel = w
        .prefix()
        .join("releases")
        .join(format!("0.1.0+{}", &c4[..12]));
    assert!(!rel.join("ui").exists());
    assert!(so.contains("ui        not built"), "{so}");
    assert!(
        so.contains("ships no contrib/units/ferroloop-hub.service"),
        "{so}"
    );
    assert_eq!(w.check_status("G7.receipt-integrity"), "pass");

    // --with-ui: another prefix, the UI is in the release
    let w2 = World::new(FakeCargo::Ok);
    let c4 = gitrepo::add_ui_commit(&w2.repo);
    let (c, so, se) = run(&mut w2.install_git_with("full", &c4, &["--yes", "--with-ui"]));
    assert_eq!(c, 0, "{so}{se}");
    let rel = w2
        .prefix()
        .join("releases")
        .join(format!("0.1.0+{}", &c4[..12]));
    assert_eq!(
        fs::read_to_string(rel.join("ui/dist/index.html")).unwrap(),
        "<html>ui</html>\n"
    );
    assert!(rel.join("ui/dist/assets/app.js").is_file());
    assert!(so.contains("npm ci && npm run build"), "{so}");
    assert_eq!(w2.check_status("G7.receipt-integrity"), "pass");
    assert_eq!(w2.check_status("G7.path-ownership"), "pass");
}

#[test]
fn with_ui_needs_a_ui_in_the_commit() {
    let w = World::new(FakeCargo::Ok);
    let (c, _, se) = run(&mut w.install_git(&w.repo.commits[1], &["--yes", "--with-ui"]));
    assert_eq!(c, 3, "{se}");
    assert!(se.contains("package-lock.json"), "{se}");
    assert_eq!(w.tools.builds(), 0);
}
