//! Passive installed-runtime facts shared by the Doctor entry points.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};

use super::{VendorVersionAdapter, VendorVersionRegistry};

/// A local user configuration or environment setting, rather than a running process's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VendorUpdateStatus {
    Enabled,
    Disabled,
    /// The setting cannot be proven passively. The reason is a fixed, short phrase such as
    /// `settings.json failed validation`; being `'static`, it cannot carry file contents,
    /// account identifiers or other observed values.
    Unknown(&'static str),
}

impl VendorUpdateStatus {
    pub fn from_enabled(enabled: bool) -> Self {
        if enabled {
            Self::Enabled
        } else {
            Self::Disabled
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VendorInstalledVersion {
    /// No executable in the locations checked.
    Absent,
    /// The image or its version could not be identified from passive evidence.
    Unknown,
    Known(String),
}

impl VendorInstalledVersion {
    /// Resolve a selected executable and inspect its image with the supplied passive decoder.
    pub fn for_executable(path: &Path, decode: impl FnOnce(&Path) -> Option<String>) -> Self {
        let path = match path.canonicalize() {
            Ok(path) => path,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Self::Absent,
            Err(_) => return Self::Unknown,
        };
        match path.metadata() {
            Ok(metadata) if executable(&metadata) => {
                decode(&path).map_or(Self::Unknown, Self::Known)
            }
            Ok(_) => Self::Absent,
            Err(_) => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VendorRuntimeFacts {
    pub installed_version: VendorInstalledVersion,
    /// Setting observed in the current invocation environment or the selected user config file.
    pub configured_updates: VendorUpdateStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VendorDoctorFacts {
    pub vendor: &'static str,
    pub cli: VendorRuntimeFacts,
    pub daemon: Option<VendorRuntimeFacts>,
    /// Policy FerroLoop applies when composing a hosted CLI command.
    pub hosted_cli_updates: VendorUpdateStatus,
}

pub struct VendorDoctorContext<'a> {
    pub home: Option<&'a Path>,
    pub path: Option<&'a OsStr>,
    /// Read only named update controls and config-directory overrides for this invocation.
    pub env: &'a dyn Fn(&str) -> Option<OsString>,
}

impl VendorDoctorContext<'_> {
    pub fn var(&self, key: &str) -> Option<OsString> {
        (self.env)(key)
    }

    /// Select an absolute vendor configuration directory. A nonempty invalid override does
    /// not fall back to an unrelated home.
    pub fn config_home(&self, key: &str, default_dir: &str) -> Option<PathBuf> {
        self.var(key)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| self.home.map(|home| home.join(default_dir)))
            .filter(|path| path.is_absolute())
    }

    fn installed_cli_version(&self, adapter: &dyn VendorVersionAdapter) -> VendorInstalledVersion {
        let binary = format!(
            "{}{}",
            adapter.cli_binary_name(),
            std::env::consts::EXE_SUFFIX
        );
        let candidates = self
            .path
            .into_iter()
            .flat_map(std::env::split_paths)
            .chain(self.home.map(|home| home.join(".local/bin")))
            .filter(|dir| dir.is_absolute());
        let mut searched = false;
        for directory in candidates {
            searched = true;
            let version =
                VendorInstalledVersion::for_executable(&directory.join(&binary), |path| {
                    adapter.version_from_executable(path)
                });
            if version != VendorInstalledVersion::Absent {
                return version;
            }
        }
        if searched {
            VendorInstalledVersion::Absent
        } else {
            VendorInstalledVersion::Unknown
        }
    }
}

fn executable(metadata: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        metadata.is_file()
    }
}

impl VendorVersionRegistry {
    /// Observe each registered vendor in ID order, without starting a CLI or using the network.
    pub fn doctor_facts(&self, context: &VendorDoctorContext<'_>) -> Vec<VendorDoctorFacts> {
        self.adapters
            .values()
            .map(|adapter| {
                let mut argv = vec![adapter.cli_binary_name().into()];
                let mut env = Vec::new();
                adapter.apply_cli_update_policy(&mut argv, &mut env);
                let hosted_cli_updates = if argv.len() > 1 || !env.is_empty() {
                    VendorUpdateStatus::Disabled
                } else {
                    VendorUpdateStatus::Unknown("no hosted update control")
                };
                VendorDoctorFacts {
                    vendor: adapter.vendor_id(),
                    cli: VendorRuntimeFacts {
                        installed_version: context.installed_cli_version(adapter.as_ref()),
                        configured_updates: adapter.configured_cli_updates(context),
                    },
                    daemon: adapter.doctor_daemon_facts(context),
                    hosted_cli_updates,
                }
            })
            .collect()
    }
}
