//! The environment a spawned vendor CLI sees.
//!
//! The allow-lists, the dialects and the filter are pure and live in [`fl_core::child_env`]
//! (re-exported here, so every caller keeps its `fl_exec::child_env::...` path); read that
//! module's documentation for the rules. This module adds the two wrappers that read the
//! process environment, which `fl-core` must not do.

use fl_core::telemetry::AdapterKind;
use std::ffi::OsString;

pub use fl_core::child_env::*;

/// The environment for a `kind` child, from this process's environment. `pass` names extra
/// variables to copy from it and `extra` is applied last (see [`fl_core::child_env`]).
pub fn child_env(
    kind: AdapterKind,
    pass: &[String],
    extra: &[(OsString, OsString)],
) -> Vec<(OsString, OsString)> {
    child_env_from(kind, std::env::vars_os(), pass, extra)
}

/// The environment for a helper that is not a vendor CLI (a hydrate script, `curl`): the
/// baseline of [`child_env`] and no vendor's credentials at all.
pub fn helper_env(pass: &[String], extra: &[(OsString, OsString)]) -> Vec<(OsString, OsString)> {
    helper_env_from(std::env::vars_os(), pass, extra)
}
