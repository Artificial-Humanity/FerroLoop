//! `GET /api/v1/version`: how this hub and the UI bundle it serves were built.
//!
//! Any authenticated caller (Owner or Peer), like the other read-only status GETs. The body
//! is the hub's [`BuildInfo`] (`name` is `fl-hub`) plus:
//!
//! - `ui`: the bundle's `build-info.json` (`{"commit","build_time_unix","version"}`, and an
//!   optional `"dirty"`), or `null` when the hub serves no UI, the file is absent, is not a
//!   regular file, is larger than [`MAX_UI_BUILD_INFO_BYTES`] or does not parse. A bad file is
//!   never an error: the version answer must work on a broken bundle.
//! - `code_ui_agree`: whether the UI was built from the hub's commit. `null` when either
//!   commit is unknown; a UI commit may be abbreviated (7 hex digits or more) and agrees with
//!   a hub commit it prefixes.
//!
//! Nothing here reads outside `<ui_dir>/build-info.json` and nothing of the file is echoed
//! beyond the four validated fields: no path, no other key.

use std::io::Read;
use std::path::Path;

use fl_core::build_info::BuildInfo;
use serde_json::{Map, Value};

use crate::{HttpResponse, HubState};

/// The file the UI build writes at the root of its bundle.
pub const UI_BUILD_INFO_FILE: &str = "build-info.json";
/// The most bytes of `build-info.json` the hub reads; a larger file is ignored.
pub const MAX_UI_BUILD_INFO_BYTES: u64 = 16 * 1024;
const MIN_UI_COMMIT_LEN: usize = 7;
const MAX_UI_VERSION_LEN: usize = 64;

/// This hub's build stamp.
pub fn build_info() -> BuildInfo {
    fl_core::build_info!()
}

fn is_hex_commit(s: &str) -> bool {
    (MIN_UI_COMMIT_LEN..=64).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The validated UI build info from the text of a `build-info.json`.
pub(crate) fn parse_ui_build_info(text: &str) -> Option<Map<String, Value>> {
    let Value::Object(o) = serde_json::from_str::<Value>(text).ok()? else {
        return None;
    };
    let commit = o.get("commit")?.as_str()?;
    if !is_hex_commit(commit) {
        return None;
    }
    let mut out = Map::new();
    out.insert("commit".into(), commit.into());
    out.insert(
        "build_time_unix".into(),
        o.get("build_time_unix")
            .and_then(Value::as_u64)
            .map_or(Value::Null, Value::from),
    );
    out.insert(
        "version".into(),
        o.get("version")
            .and_then(Value::as_str)
            .filter(|v| v.len() <= MAX_UI_VERSION_LEN && v.chars().all(|c| !c.is_control()))
            .map_or(Value::Null, Value::from),
    );
    if let Some(d) = o.get("dirty").and_then(Value::as_bool) {
        out.insert("dirty".into(), d.into());
    }
    Some(out)
}

/// Read `<ui_dir>/build-info.json`: `None` for anything but a small, regular, valid file.
pub(crate) fn read_ui_build_info(ui_dir: &Path) -> Option<Map<String, Value>> {
    let path = ui_dir.join(UI_BUILD_INFO_FILE);
    // `symlink_metadata`: a symlink is not followed, so the bundle cannot point the hub at
    // another file.
    let meta = std::fs::symlink_metadata(&path).ok()?;
    if !meta.is_file() || meta.len() > MAX_UI_BUILD_INFO_BYTES {
        return None;
    }
    let mut text = String::new();
    // The length can change between the check and the read: bound the read itself.
    std::fs::File::open(&path)
        .ok()?
        .take(MAX_UI_BUILD_INFO_BYTES + 1)
        .read_to_string(&mut text)
        .ok()?;
    if text.len() as u64 > MAX_UI_BUILD_INFO_BYTES {
        return None;
    }
    parse_ui_build_info(&text)
}

/// Whether the UI was built from the hub's commit; `None` when either is unknown.
pub(crate) fn code_ui_agree(hub: &BuildInfo, ui: Option<&Map<String, Value>>) -> Option<bool> {
    let ui_commit = ui?.get("commit")?.as_str()?;
    hub.commit_known()
        .then(|| hub.commit.starts_with(ui_commit))
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
