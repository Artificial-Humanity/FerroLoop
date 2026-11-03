//! `GET /api/v1/version`: how this hub and the UI bundle it serves were built.
//!
//! Any authenticated caller (Owner or Peer), like the other read-only status GETs. The body
//! is the hub's [`BuildInfo`] (`name` is `fl-hub`) plus:
//!
//! - `ui`: the bundle's `build-info.json` (`{"commit","build_time_unix","version"}`, and an
//!   optional `"dirty"`), or `null` when the hub serves no UI, the file is absent, is not a
//!   regular file, is larger than [`fl_core::ui_bundle::MAX_UI_BUILD_INFO_BYTES`] or does not parse. A bad file is
//!   never an error: the version answer must work on a broken bundle.
//! - `code_ui_agree`: whether the UI was built from the hub's commit. `null` when either
//!   commit is unknown; a UI commit may be abbreviated (7 hex digits or more) and agrees with
//!   a hub commit it prefixes.
//!
//! Nothing here reads outside `<ui_dir>/build-info.json` and nothing of the file is echoed
//! beyond the four validated fields: no path, no other key.

use std::path::Path;

use fl_core::build_info::BuildInfo;
use serde_json::Value;

use crate::{HttpResponse, HubState};

// The validation and the agreement rule live in `fl_core::ui_bundle`, the file reader in
// `fl_install`, so the doctor reads the bundle the same way.
pub(crate) use fl_core::ui_bundle::code_ui_agree;
#[cfg(test)]
pub(crate) use fl_core::ui_bundle::parse_ui_build_info;
#[cfg(test)]
pub(crate) use fl_core::ui_bundle::{MAX_UI_BUILD_INFO_BYTES, UI_BUILD_INFO_FILE};
pub(crate) use fl_install::read_ui_build_info;

/// This hub's build stamp.
pub fn build_info() -> BuildInfo {
    fl_core::build_info!()
}

/// The response body for `hub` and the UI bundle at `ui_dir`.
pub(crate) fn body(hub: &BuildInfo, ui_dir: Option<&Path>) -> Value {
    let ui = ui_dir.and_then(read_ui_build_info);
    let agree = code_ui_agree(hub, ui.as_ref());
    let mut v = hub.to_json("fl-hub");
    if let Value::Object(o) = &mut v {
        o.insert("ui".into(), ui.map_or(Value::Null, Value::Object));
        o.insert(
            "code_ui_agree".into(),
            agree.map_or(Value::Null, Value::from),
        );
    }
    v
}

/// `GET /api/v1/version`.
pub(crate) fn handle_get(state: &HubState) -> HttpResponse {
    HttpResponse::json(200, body(&build_info(), state.config.ui_dir.as_deref()))
}

#[cfg(test)]
#[path = "version_api_tests.rs"]
mod tests;
