//! The I/O side of the installer (#205): everything that touches the file system on behalf of
//! `fl install` and `fl doctor`. `fl-core` keeps the pure model (`fl_core::install`, the
//! `fl_core::doctor` engine); this crate reads the host and hands those facts over.
//!
//! * [`FsProbe`]: the real, read-only `HostProbe` (`lstat`, `readlink`, bounded hashes), and the
//!   bounded readers [`read_bounded`] and [`open_regular`].
//! * [`observe`]: gather the doctor's facts (reads only) and run the engine.
//! * [`read_ui_build_info`]: a UI bundle's `build-info.json`.
//!
//! * [`install_from_git`], [`plan_from_git`]: `fl install --from-git <rev>` (phase A4b), a
//!   build of a reviewed commit that is then installed by the same transaction.
//! * [`apply`], [`recover`]: the transactional installer (phase A4a): `install.lock`, an
//!   append-only journal, staged and verified releases, an atomic switch of `current`, the
//!   launcher and aliases, and the receipt written last; see `docs/installer.md`.

// The crash seam reads FL_INSTALL_FAULT. It exists for test builds (debug assertions on); an
// optimized build with it enabled is refused at compile time. Build releases with
// `cargo build --release --locked -p fl-cli`, never with `--all-targets` or `--tests`.
#[cfg(all(feature = "fault-injection", not(debug_assertions)))]
compile_error!("the `fault-injection` feature must not be enabled in a release build");

mod builder;
mod engine;
mod error;
mod fault;
#[cfg(any(test, feature = "fixtures"))]
pub mod fixture;
mod fromgit;
mod fsprobe;
mod git;
mod journal;
mod lock;
mod observe;
mod process;
mod safe;
mod services;
mod tree;
mod ui;

#[cfg(test)]
mod fromgit_tests;
#[cfg(test)]
mod observe_tests;
#[cfg(test)]
mod txn_tests;

pub use builder::{
    BIN_PACKAGES, BuildCtx, BuildError, BuildJob, Builder, ENV_FIXED, ENV_PASSTHROUGH,
    ProcessBuilder, Toolchain, cargo_args, env_name_allowed, npm_ci_args, sandbox_env,
};
pub use engine::{
    ApplyArgs, ApplyReport, DEFAULT_LOCK_TIMEOUT_SECS, Decisions, Env, Outcome, RecoverAction,
    RecoverMode, RecoverReport, apply, installed_receipt, is_installed, plan_only, preflight,
    recover,
};
pub use error::{InstallError, Result};
pub use fault::{Death, FaultPoint, Faults, When, faults_from_env};
pub use fromgit::{
    BUILD_ROOT, DEFAULT_BUILD_TIMEOUT_SECS, FromGitArgs, FromGitResult, MARKER, SourceInfo,
    install as install_from_git, plan as plan_from_git,
};
pub use fsprobe::{FsProbe, open_regular, read_bounded};
pub use git::{HARDENING, hardened_env, rev_problem, tree_path_problem};
pub use journal::{Found, Journal, MAX_JOURNAL_BYTES, Pending, PendingState, read as read_journal};
pub use lock::{LockGuard, acquire as acquire_lock, pid_alive};
pub use observe::{Observe, UiDir, node_meta, observe};
pub use safe::Safe;
pub use services::{
    FakeLaunchctl, FakeManager, FakeSystemctl, Launchctl, ServiceCommand, ServiceError,
    ServiceManager, SystemctlUser, manager_for, not_executed,
};
pub use tree::{MAX_TREE_FILES, make_tree_writable, mode_of, scan_tree};
pub use ui::read_ui_build_info;
