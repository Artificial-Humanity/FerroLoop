//! Concrete [`fl_core::VendorHostAdapter`] implementations composed at `fl-cli`.
//!
//! `fl-hub`, `fl-wire`, and `fl-host` depend only on the pure traits and domain types in
//! `fl-core::vendor_adapter` and never on this crate.

mod antigravity;
pub mod claude;
pub mod codex;
mod grok;
mod muse;

pub use claude::{ClaudeHostAdapter, ClaudePostCompactionContext};
pub use codex::CodexHostAdapter;

use std::sync::Arc;

use fl_core::{VendorHostAdapterRegistry, VendorVersionRegistry};

/// Build a [`VendorHostAdapterRegistry`] pre-populated with all concrete vendor host adapters
/// implemented in this crate.
pub fn default_vendor_host_adapter_registry() -> VendorHostAdapterRegistry {
    let registry = VendorHostAdapterRegistry::new();
    registry.register(Arc::new(ClaudeHostAdapter::new()));
    registry.register(Arc::new(CodexHostAdapter::new()));
    registry
}

/// Alias for [`default_vendor_host_adapter_registry`].
pub fn default_vendor_adapter_registry() -> VendorHostAdapterRegistry {
    default_vendor_host_adapter_registry()
}

/// Compose the version adapters separately from the hosted session operation adapters.
pub fn default_vendor_version_registry() -> VendorVersionRegistry {
    let mut registry = VendorVersionRegistry::default();
    registry.register(antigravity::AntigravityVersionAdapter);
    registry.register(claude::ClaudeHostAdapter::new());
    registry.register(codex::CodexHostAdapter::new());
    registry.register(grok::GrokVersionAdapter);
    registry.register(muse::MuseVersionAdapter);
    registry
}

fn valid_release_version(version: &str) -> Option<&str> {
    const MAX_VERSION_BYTES: usize = 128;
    (version.len() <= MAX_VERSION_BYTES && semver::Version::parse(version).is_ok())
        .then_some(version)
}

#[cfg(test)]
mod version_evidence_tests {
    use super::default_vendor_version_registry;
    use std::fs;
    use std::path::{Path, PathBuf};

    fn executable(root: &Path, relative: &str) -> PathBuf {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, []).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        path
    }

    #[test]
    fn recognizes_versioned_native_executable_paths() {
        let root = tempfile::tempdir().unwrap();
        let registry = default_vendor_version_registry();
        for (vendor, path, version) in [
            ("claude", ".local/share/claude/versions/2.1.332", "2.1.332"),
            (
                "codex",
                "packages/standalone/releases/0.172.0-beta.2-x86_64-unknown-linux-musl/bin/codex",
                "0.172.0-beta.2",
            ),
            ("grok", ".grok/downloads/grok-1.0.92-linux-x86_64", "1.0.92"),
            (
                "muse",
                ".local/bin/muse-bin-1.4.25-R9361.1",
                "1.4.25-R9361.1",
            ),
        ] {
            let path = executable(root.path(), path);
            let adapter = registry.lookup(vendor).unwrap();
            assert_eq!(
                adapter.version_from_executable(&path).as_deref(),
                Some(version)
            );
        }
        let opaque = executable(root.path(), ".local/bin/agy");
        let adapter = registry.lookup("antigravity").unwrap();
        assert_eq!(adapter.version_from_executable(&opaque), None);
        assert!(registry.lookup("unknown").is_none());
    }

    #[test]
    fn muse_launcher_needs_current_metadata_and_its_matching_executable() {
        let root = tempfile::tempdir().unwrap();
        let registry = default_vendor_version_registry();
        let adapter = registry.lookup("muse").unwrap();
        let launcher = executable(root.path(), "muse");
        let state = root.path().join(".muse-version");
        executable(root.path(), "muse-bin-1.4.26-R9999.1");
        fs::write(&state, "1.4.25-R9361.1\n").unwrap();
        assert_eq!(adapter.version_from_executable(&launcher), None);

        executable(root.path(), "muse-bin-1.4.25-R9361.1");
        assert_eq!(
            adapter.version_from_executable(&launcher).as_deref(),
            Some("1.4.25-R9361.1")
        );
        fs::write(&state, "1.4.25-R9361.1\n".repeat(128)).unwrap();
        assert_eq!(adapter.version_from_executable(&launcher), None);
    }

    #[test]
    fn codex_daemon_uses_the_observed_release_path() {
        let root = tempfile::tempdir().unwrap();
        let registry = default_vendor_version_registry();
        let adapter = registry.lookup("codex").unwrap();
        let observed = executable(
            root.path(),
            "packages/app-server-daemon/releases/0.167.0-aarch64-apple-darwin/bin/codex",
        );
        executable(
            root.path(),
            "packages/app-server-daemon/releases/0.172.0-aarch64-apple-darwin/bin/codex",
        );
        let current = executable(root.path(), "packages/app-server-daemon/current/bin/codex");
        assert_eq!(
            adapter.daemon_version_from_executable(&observed).as_deref(),
            Some("0.167.0")
        );
        assert_eq!(adapter.version_from_executable(&observed), None);
        assert_eq!(adapter.daemon_version_from_executable(&current), None);
    }
}
