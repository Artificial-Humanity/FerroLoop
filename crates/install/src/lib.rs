//! The I/O side of the installer (#205): everything that touches the file system on behalf of
//! `fl install` and `fl doctor`. `fl-core` keeps the pure model (`fl_core::install`, the
//! `fl_core::doctor` engine); this crate reads the host and hands those facts over.
//!
//! * [`FsProbe`]: the real, read-only `HostProbe` (`lstat`, `readlink`, bounded hashes), and the
//!   bounded readers [`read_bounded`] and [`open_regular`].
//! * [`observe`]: gather the doctor's facts (reads only) and run the engine.
//! * [`read_ui_build_info`]: a UI bundle's `build-info.json`.
//!
//! * [`apply`], [`recover`]: the transactional installer (phase A4a): `install.lock`, an
//!   append-only journal, staged and verified releases, an atomic switch of `current`, the
//!   launcher and aliases, and the receipt written last; see `docs/installer.md`.

mod engine;
mod error;
mod fault;
#[cfg(any(test, feature = "fixtures"))]
pub mod fixture;
mod fsprobe;
mod journal;
mod lock;
mod observe;
mod safe;
mod services;
mod tree;
mod ui;

#[cfg(test)]
mod observe_tests;
#[cfg(test)]
mod txn_tests;

pub use engine::{
    ApplyArgs, ApplyReport, DEFAULT_LOCK_TIMEOUT_SECS, Decisions, Env, Outcome, RecoverAction,
    RecoverMode, RecoverReport, apply, recover,
};
pub use error::{InstallError, Result};
pub use fault::{Death, FaultPoint, Faults, When, faults_from_env};
pub use fsprobe::{FsProbe, open_regular, read_bounded};
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
