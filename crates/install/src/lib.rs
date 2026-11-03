//! The I/O side of the installer (#205): everything that touches the file system on behalf of
//! `fl install` and `fl doctor`. `fl-core` keeps the pure model (`fl_core::install`, the
//! `fl_core::doctor` engine); this crate reads the host and hands those facts over.
//!
//! * [`FsProbe`]: the real, read-only `HostProbe` (`lstat`, `readlink`, bounded hashes), and the
//!   bounded readers [`read_bounded`] and [`open_regular`].
//! * [`observe`]: gather the doctor's facts (reads only) and run the engine.
//! * [`read_ui_build_info`]: a UI bundle's `build-info.json`.
//!
//! The transactional installer's writes (phase A4) belong here too.

mod fsprobe;
mod observe;
mod ui;

#[cfg(test)]
mod observe_tests;

pub use fsprobe::{FsProbe, open_regular, read_bounded};
pub use observe::{Observe, UiDir, node_meta, observe};
pub use ui::read_ui_build_info;
