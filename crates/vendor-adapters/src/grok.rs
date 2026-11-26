use std::path::Path;

use fl_core::VendorVersionAdapter;

#[derive(Debug)]
pub(crate) struct GrokVersionAdapter;

impl VendorVersionAdapter for GrokVersionAdapter {
    fn vendor_id(&self) -> &'static str {
        "grok"
    }

    fn auto_update_disable_env(&self) -> Option<(&'static str, &'static str)> {
        Some(("GROK_DISABLE_AUTOUPDATER", "1"))
    }

    fn version_from_executable(&self, executable: &Path) -> Option<String> {
        if !executable.is_absolute() || !executable.parent()?.ends_with(".grok/downloads") {
            return None;
        }
        // Some native releases put the version in the target of the `grok` link.
        // An unversioned executable or a copied launcher offers no such evidence.
        let name = executable.file_name()?.to_str()?.strip_prefix("grok-")?;
        let name = name.strip_suffix(".exe").unwrap_or(name);
        let version = [
            "-linux-aarch64",
            "-linux-x86_64",
            "-macos-aarch64",
            "-macos-x86_64",
            "-windows-aarch64",
            "-windows-x86_64",
        ]
        .into_iter()
        .find_map(|target| name.strip_suffix(target))?;
        crate::valid_release_version(version).map(str::to_owned)
    }
}
