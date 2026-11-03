//! `--from-git` through the [`Builder`] seam: a fake builder stands in for `cargo`. (The
//! black-box tests in `crates/cli/tests/install_from_git.rs` run a fake `cargo` executable
//! through the real process-spawn path.)

use crate::fixture::gitrepo::{self, Repo};
use crate::*;
use fl_core::install::{HostInfo, InstallReceipt, InstallRequest, Layout, Profile};
use std::cell::RefCell;
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Ok,
    ToolchainMissing,
    BuildFails,
    /// Reports the root commit instead of the one built.
    WrongCommit,
    /// Leaves no `target/release/fl`.
    NoOutput,
}

struct Fake {
    mode: Mode,
    seen_env: RefCell<Vec<(OsString, OsString)>>,
    seen_src: RefCell<Option<PathBuf>>,
}

impl Fake {
    fn new(mode: Mode) -> Self {
        Self {
            mode,
            seen_env: RefCell::new(Vec::new()),
            seen_src: RefCell::new(None),
        }
    }
}

fn stub(path: &Path, name: &str, commit: &str, time: &str) {
    let text = format!(
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ] && [ \"$2\" = \"--json\" ]; then\n  echo '{{\"name\":\"{name}\",\"version\":\"0.1.0\",\"commit\":\"{commit}\",\"dirty\":false,\"build_time_unix\":{time},\"source\":\"source-build\",\"protocol\":{{\"ferrowire\":{{\"current\":2}}}},\"store_formats\":{{\"redb_schema\":{{\"min\":2,\"max\":8}}}}}}'\nfi\n"
    );
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

impl Builder for Fake {
    fn toolchain(&self, ctx: &BuildCtx<'_>) -> std::result::Result<Toolchain, BuildError> {
        *self.seen_env.borrow_mut() = ctx.env.to_vec();
        *self.seen_src.borrow_mut() = Some(ctx.src.to_path_buf());
        if self.mode == Mode::ToolchainMissing {
            return Err(BuildError::Unavailable("cargo is not on PATH".into()));
        }
        Ok(Toolchain {
            rustc: "rustc 0.0.0 (fake)".into(),
            cargo: "cargo 0.0.0 (fake)".into(),
        })
    }

    fn build(&self, ctx: &BuildCtx<'_>, job: &BuildJob) -> std::result::Result<(), BuildError> {
        if self.mode == Mode::BuildFails {
            fs::write(ctx.log, "error: could not compile\n").unwrap();
            return Err(BuildError::Failed("cargo build: exit status 101".into()));
        }
        // The stamp a real build.rs would read: HEAD of the build tree's own repository.
        let head = fs::read_to_string(ctx.src.join(".git/HEAD")).unwrap();
        let mut commit = head.trim().to_string();
        if self.mode == Mode::WrongCommit {
            commit = "1".repeat(40);
        }
        let time = ctx
            .env
            .iter()
            .find(|(k, _)| k == "SOURCE_DATE_EPOCH")
            .map(|(_, v)| v.to_string_lossy().into_owned())
            .unwrap();
        if self.mode != Mode::NoOutput {
            for b in &job.bins {
                stub(&ctx.src.join("target/release").join(b), b, &commit, &time);
            }
        }
        Ok(())
    }
}

struct W {
    base: tempfile::TempDir,
    repo: Repo,
    prefix: PathBuf,
}

impl Drop for W {
    fn drop(&mut self) {
        make_tree_writable(self.base.path());
    }
}

impl W {
    fn new() -> Self {
        let base = tempfile::tempdir().unwrap();
        let repo = gitrepo::make_repo(&base.path().join("repo"));
        let prefix = base.path().join("prefix");
        Self { base, repo, prefix }
    }

    fn run(
        &self,
        rev: &str,
        yes: bool,
        allow_unreviewed: bool,
        mode: Mode,
    ) -> (Fake, Result<(SourceInfo, FromGitResult)>) {
        let layout = Layout::explicit(self.prefix.to_str().unwrap()).unwrap();
        let request = InstallRequest {
            profile: Some(Profile::Minimal),
            add: vec![],
            remove: vec![],
            layout,
            adopt: false,
        };
        let env = Env::new(self.prefix.clone(), None);
        let host = HostInfo::from_consts(std::env::consts::OS, std::env::consts::ARCH).unwrap();
        let decisions = Decisions::default();
        let parent_env: Vec<(OsString, OsString)> = vec![
            ("PATH".into(), gitrepo::system_path().into()),
            ("FL_TOKEN".into(), "secret".into()),
            ("GITHUB_TOKEN".into(), "secret".into()),
            ("AWS_SECRET_ACCESS_KEY".into(), "secret".into()),
        ];
        let args = FromGitArgs {
            rev,
            repo: Some(&self.repo.dir),
            cwd: self.base.path(),
            allow_unreviewed,
            with_ui: false,
            build_timeout: Duration::from_secs(60),
            parent_env: &parent_env,
            request: &request,
            host,
            decisions: &decisions,
        };
        let fake = Fake::new(mode);
        let r = if yes {
            install_from_git(&env, &args, &fake)
        } else {
            plan_from_git(&env, &args)
        };
        (fake, r)
    }
}

#[test]
fn the_seam_installs_a_reviewed_commit_and_records_a_source_build() {
    let w = W::new();
    let (fake, r) = w.run(&w.repo.commits[1], true, false, Mode::Ok);
    let (info, res) = r.unwrap();
    assert!(info.reviewed);
    let FromGitResult::Applied { report, built } = res else {
        panic!("applied")
    };
    assert!(built);
    assert_eq!(
        report.release,
        format!("0.1.0+{}", &w.repo.commits[1][..12])
    );
    let receipt =
        InstallReceipt::parse(&fs::read_to_string(w.prefix.join("receipt.json")).unwrap()).unwrap();
    assert_eq!(receipt.schema_version, 2);
    let b = receipt.release.build.unwrap();
    assert!(b.reviewed);
    assert_eq!(
        (b.rustc.as_str(), b.cargo.as_str()),
        ("rustc 0.0.0 (fake)", "cargo 0.0.0 (fake)")
    );
    assert_eq!(
        receipt.release.commit.as_deref(),
        Some(w.repo.commits[1].as_str())
    );
    assert!(!w.prefix.join("build").exists());
    // the build ran inside the build directory, never in the source repository
    let src = fake.seen_src.borrow().clone().unwrap();
    assert!(src.starts_with(w.prefix.join("build")));
    assert!(!src.starts_with(&w.repo.dir));
    // and its environment held no secret
    let names: Vec<String> = fake
        .seen_env
        .borrow()
        .iter()
        .map(|(k, _)| k.to_string_lossy().into_owned())
        .collect();
    for n in &names {
        assert!(env_name_allowed(n), "{n}");
    }
    assert!(
        !names
            .iter()
            .any(|n| n.contains("TOKEN") || n.contains("AWS"))
    );
}

#[test]
fn a_dry_run_through_the_seam_never_calls_the_builder() {
    let w = W::new();
    let (fake, r) = w.run(&w.repo.commits[1], false, false, Mode::Ok);
    let (_, res) = r.unwrap();
    assert!(matches!(res, FromGitResult::Planned { .. }));
    assert!(fake.seen_src.borrow().is_none());
    assert!(!w.prefix.exists());
}

#[test]
fn a_failed_build_through_the_seam_exits_1_and_keeps_the_directory() {
    let w = W::new();
    let (_, r) = w.run(&w.repo.commits[1], true, false, Mode::BuildFails);
    let e = r.unwrap_err();
    assert_eq!(e.exit_code(), 1, "{e}");
    let dir = w.prefix.join("build").join(&w.repo.commits[1][..12]);
    assert!(matches!(e, InstallError::Kept { .. }), "{e}");
    assert!(e.to_string().contains("build.log"), "{e}");
    assert!(dir.join(MARKER).is_file());
    assert!(!w.prefix.join("receipt.json").exists());
    assert!(!w.prefix.join("current").exists());
}

#[test]
fn a_missing_tool_is_a_refusal_and_leaves_nothing() {
    let w = W::new();
    let (_, r) = w.run(&w.repo.commits[1], true, false, Mode::ToolchainMissing);
    let e = r.unwrap_err();
    assert_eq!(e.exit_code(), 3, "{e}");
    assert!(
        !w.prefix.exists(),
        "everything made for the build is removed again"
    );
}

#[test]
fn a_binary_of_another_commit_is_refused() {
    let w = W::new();
    let (_, r) = w.run(&w.repo.commits[1], true, false, Mode::WrongCommit);
    let e = r.unwrap_err();
    assert_eq!(e.exit_code(), 3, "{e}");
    assert!(e.to_string().contains("reports commit"), "{e}");
    assert!(!w.prefix.join("receipt.json").exists());
}

#[test]
fn a_build_that_produces_no_binary_fails_with_its_log() {
    let w = W::new();
    let (_, r) = w.run(&w.repo.commits[1], true, false, Mode::NoOutput);
    let e = r.unwrap_err();
    assert_eq!(e.exit_code(), 1, "{e}");
    assert!(e.to_string().contains("produced no regular file"), "{e}");
}

#[test]
fn the_review_rule_is_checked_before_the_builder_exists() {
    let w = W::new();
    let (fake, r) = w.run(&w.repo.commits[2], true, false, Mode::Ok);
    let e = r.unwrap_err();
    assert_eq!(e.exit_code(), 3);
    assert!(fake.seen_src.borrow().is_none());
    assert!(!w.prefix.exists());
    // with the flag it builds and says so
    let (_, r) = w.run(&w.repo.commits[2], true, true, Mode::Ok);
    let (info, _) = r.unwrap();
    assert!(!info.reviewed);
    let receipt =
        InstallReceipt::parse(&fs::read_to_string(w.prefix.join("receipt.json")).unwrap()).unwrap();
    assert!(!receipt.release.build.unwrap().reviewed);
}
