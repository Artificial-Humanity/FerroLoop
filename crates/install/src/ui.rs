//! Read a UI bundle's `build-info.json`. The text is validated by `fl_core::ui_bundle`.

use crate::fsprobe::read_bounded;
use fl_core::ui_bundle::{MAX_UI_BUILD_INFO_BYTES, UI_BUILD_INFO_FILE, parse_ui_build_info};
use serde_json::{Map, Value};
use std::path::Path;

/// Read `<ui_dir>/build-info.json`: `None` for anything but a small, regular, valid file. The
/// file is opened `O_NONBLOCK|O_NOFOLLOW` and its descriptor checked, so a symlink or a FIFO in
/// its place is never followed or waited on, and the read itself is bounded.
pub fn read_ui_build_info(ui_dir: &Path) -> Option<Map<String, Value>> {
    let text = read_bounded(
        &ui_dir.join(UI_BUILD_INFO_FILE),
        MAX_UI_BUILD_INFO_BYTES as usize,
    )
    .ok()??;
    parse_ui_build_info(&text)
}
