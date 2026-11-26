use std::path::Path;

use fl_core::VendorVersionAdapter;
use fl_core::vendor_adapter::VendorCliVersionProbe;

const AUTO_UPDATE_DISABLED: (&str, &str) = ("AGY_CLI_DISABLE_AUTO_UPDATE", "true");

#[derive(Debug)]
pub(crate) struct AntigravityVersionAdapter;

impl VendorVersionAdapter for AntigravityVersionAdapter {
    fn vendor_id(&self) -> &'static str {
        "antigravity"
    }

    fn cli_binary_name(&self) -> &'static str {
        "agy"
    }

    fn auto_update_disable_env(&self) -> Option<(&'static str, &'static str)> {
        Some(AUTO_UPDATE_DISABLED)
    }

    fn version_from_executable(&self, _executable: &Path) -> Option<String> {
        // The native installer overwrites a plain `agy` executable. Its updater status
        // records the last check, which does not identify the currently selected image.
        None
    }

    fn manual_cli_version_probe(&self, executable: &Path) -> Option<VendorCliVersionProbe> {
        (executable.file_name()? == "agy").then_some(VendorCliVersionProbe {
            env: &[AUTO_UPDATE_DISABLED],
            parse: super::valid_release_version,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_probe_requires_the_native_name_and_disables_updates() {
        let adapter = AntigravityVersionAdapter;
        assert!(
            adapter
                .manual_cli_version_probe(Path::new("/usr/bin/sh"))
                .is_none()
        );
        let probe = adapter
            .manual_cli_version_probe(Path::new("/home/owner/.local/bin/agy"))
            .unwrap();
        assert_eq!(probe.env, &[("AGY_CLI_DISABLE_AUTO_UPDATE", "true")]);
        assert_eq!((probe.parse)("1.5.18"), Some("1.5.18"));
        assert_eq!((probe.parse)("agy says 1.5.18"), None);
        assert_eq!((probe.parse)("1.5.invalid"), None);
    }
}
