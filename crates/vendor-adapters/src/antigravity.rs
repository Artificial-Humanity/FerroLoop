use std::path::Path;

use fl_core::VendorVersionAdapter;

#[derive(Debug)]
pub(crate) struct AntigravityVersionAdapter;

impl VendorVersionAdapter for AntigravityVersionAdapter {
    fn vendor_id(&self) -> &'static str {
        "antigravity"
    }

    fn version_from_executable(&self, _executable: &Path) -> Option<String> {
        // The native installer overwrites a plain `agy` executable. Its updater status
        // records the last check, which does not identify the currently selected image.
        None
    }
}
