//! The UI bundle's `build-info.json`: how to validate its text, and whether the UI was built
//! from a binary's commit. Pure: reading the file is `fl_install::read_ui_build_info`. One rule
//! for the hub's `GET /api/v1/version` and for the doctor.
//!
//! Nothing here echoes more of the file than the four validated fields.

use serde_json::{Map, Value};

use crate::build_info::BuildInfo;

/// The file the UI build writes at the root of its bundle.
pub const UI_BUILD_INFO_FILE: &str = "build-info.json";
/// The most bytes of `build-info.json` read; a larger file is ignored.
pub const MAX_UI_BUILD_INFO_BYTES: u64 = 16 * 1024;
const MIN_UI_COMMIT_LEN: usize = 7;
const MAX_UI_VERSION_LEN: usize = 64;

fn is_hex_commit(s: &str) -> bool {
    (MIN_UI_COMMIT_LEN..=64).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The validated UI build info from the text of a `build-info.json`.
pub fn parse_ui_build_info(text: &str) -> Option<Map<String, Value>> {
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

/// Whether the UI was built from `binary`'s commit; `None` when either is unknown. A UI
/// commit may be abbreviated (7 hex digits or more) and agrees with a binary commit it
/// prefixes.
pub fn code_ui_agree(binary: &BuildInfo, ui: Option<&Map<String, Value>>) -> Option<bool> {
    let ui_commit = ui?.get("commit")?.as_str()?;
    binary
        .commit_known()
        .then(|| binary.commit.starts_with(ui_commit))
}
