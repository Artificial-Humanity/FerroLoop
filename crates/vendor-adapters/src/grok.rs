use std::path::Path;

use fl_core::{VendorDoctorContext, VendorUpdateStatus, VendorVersionAdapter};
use serde::Deserialize;

use crate::doctor::configured_updates;

#[derive(Debug)]
pub(crate) struct GrokVersionAdapter;

const AUTO_UPDATE_DISABLED: (&str, &str) = ("GROK_DISABLE_AUTOUPDATER", "1");

#[derive(Default, Deserialize)]
struct GrokConfig {
    #[serde(default)]
    cli: GrokCliConfig,
}

#[derive(Default, Deserialize)]
struct GrokCliConfig {
    auto_update: Option<bool>,
}

impl VendorVersionAdapter for GrokVersionAdapter {
    fn vendor_id(&self) -> &'static str {
        "grok"
    }

    fn auto_update_disable_env(&self) -> Option<(&'static str, &'static str)> {
        Some(AUTO_UPDATE_DISABLED)
    }

    fn configured_cli_updates(&self, context: &VendorDoctorContext<'_>) -> VendorUpdateStatus {
        if let Some(disable) = context.var(AUTO_UPDATE_DISABLED.0) {
            let Some(disable) = disable.to_str() else {
                return VendorUpdateStatus::Unknown;
            };
            // The vendor treats these values as unset, case-insensitively.
            if !["", "0", "false", "off", "no"]
                .iter()
                .any(|falsy| disable.eq_ignore_ascii_case(falsy))
            {
                return VendorUpdateStatus::Disabled;
            }
        }
        // These opt-in overlays can supersede the user file. Don't read inline config
        // from the environment or assume the base setting would still be effective.
        if ["GROK_CONFIG", "GROK_CONFIG_PATH"]
            .iter()
            .any(|key| context.var(key).is_some_and(|value| !value.is_empty()))
        {
            return VendorUpdateStatus::Unknown;
        }
        let Some(home) = context.config_home("GROK_HOME", ".grok") else {
            return VendorUpdateStatus::Unknown;
        };
        configured_updates(
            &home.join("config.toml"),
            |source| toml::from_str::<GrokConfig>(source).ok(),
            |config| config.cli.auto_update.unwrap_or(true),
        )
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
