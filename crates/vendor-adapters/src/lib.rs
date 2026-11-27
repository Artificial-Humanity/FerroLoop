//! Concrete [`fl_core::VendorHostAdapter`] implementations composed at `fl-cli`.
//!
//! `fl-hub`, `fl-wire`, and `fl-host` depend only on the pure traits and domain types in
//! `fl-core::vendor_adapter` and never on this crate.

mod antigravity;
pub mod claude;
pub mod codex;
mod doctor;
mod grok;
mod muse;

pub use claude::{ClaudeHostAdapter, ClaudePostCompactionContext};
pub use codex::CodexHostAdapter;

use std::sync::Arc;

use fl_core::vendor_adapter::valid_release_version;
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
    fn startup_update_policy_overrides_existing_values_and_is_idempotent() {
        let registry = default_vendor_version_registry();
        for (binary, key, value) in [
            ("agy", "AGY_CLI_DISABLE_AUTO_UPDATE", "true"),
            ("grok", "GROK_DISABLE_AUTOUPDATER", "1"),
            ("muse", "MUSE_NO_AUTO_UPDATE", "1"),
        ] {
            let adapter = registry.lookup_by_binary_name(binary).unwrap();
            let mut argv = vec![binary.into(), "--version".into()];
            let mut env = vec![
                (key.into(), "0".into()),
                ("TERM".into(), "xterm".into()),
                (key.into(), "false".into()),
            ];
            for _ in 0..2 {
                adapter.apply_cli_update_policy(&mut argv, &mut env);
            }
            assert_eq!(argv, [binary, "--version"]);
            assert_eq!(
                env,
                [("TERM".into(), "xterm".into()), (key.into(), value.into())]
            );
        }
        let adapter = registry.lookup("claude").unwrap();
        let mut argv = vec!["/some/install/claude".into(), "--version".into()];
        let mut env = vec![
            ("DISABLE_AUTOUPDATER".into(), "0".into()),
            ("TERM".into(), "xterm".into()),
        ];
        for _ in 0..2 {
            adapter.apply_cli_update_policy(&mut argv, &mut env);
        }
        assert_eq!(
            argv,
            [
                "/some/install/claude",
                "--settings",
                r#"{"env":{"DISABLE_AUTOUPDATER":"1"}}"#,
                "--version",
            ]
        );
        assert_eq!(
            env,
            [
                ("TERM".into(), "xterm".into()),
                ("DISABLE_AUTOUPDATER".into(), "1".into())
            ]
        );

        let adapter = registry.lookup("codex").unwrap();
        let mut argv = [
            "/some/install/codex",
            "--config",
            "check_for_update_on_startup=true",
            "-c",
            "model_reasoning_effort=high",
            "--config=check_for_update_on_startup=true",
            "-ccheck_for_update_on_startup = true",
            "--",
            "--config=check_for_update_on_startup=true",
        ]
        .map(str::to_owned)
        .to_vec();
        let mut env = Vec::new();
        for _ in 0..2 {
            adapter.apply_cli_update_policy(&mut argv, &mut env);
        }
        assert_eq!(
            argv,
            [
                "/some/install/codex",
                "-c",
                "check_for_update_on_startup=false",
                "-c",
                "model_reasoning_effort=high",
                "--",
                "--config=check_for_update_on_startup=true",
            ]
        );
        assert!(env.is_empty());
    }

    #[test]
    fn claude_update_policy_leaves_a_caller_settings_flag_alone() {
        let adapter = default_vendor_version_registry();
        let adapter = adapter.lookup("claude").unwrap();
        let apply = |argv: &[&str]| {
            let mut argv = argv.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
            let mut env = Vec::new();
            adapter.apply_cli_update_policy(&mut argv, &mut env);
            assert_eq!(env, [("DISABLE_AUTOUPDATER".into(), "1".into())]);
            argv
        };
        let ours = r#"{"env":{"DISABLE_AUTOUPDATER":"1"}}"#;
        for argv in [
            &[
                "claude",
                "--model",
                "opus",
                "--settings",
                "/team/settings.json",
            ][..],
            &["claude", "--settings={}", "auth", "status"],
        ] {
            assert_eq!(apply(argv), argv);
        }
        // Arguments after `--` are not options and are never changed.
        assert_eq!(
            apply(&["claude", "--", "--settings", "x"]),
            ["claude", "--settings", ours, "--", "--settings", "x"]
        );
        assert_eq!(apply(&["claude"]), ["claude", "--settings", ours]);
        // Only `--settings` itself counts, not another flag that starts with it.
        assert_eq!(
            apply(&["claude", "--settings-file", "x"]),
            ["claude", "--settings", ours, "--settings-file", "x"]
        );
        assert!(apply(&[]).is_empty());
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
        fs::write(
            &launcher,
            r#"#!/usr/bin/env bash
set -euo pipefail

channel="${MUSE_CHANNEL:-muse-stable}"
channel_url="https://api.meta.ai/muse-code/channels/$channel"

resolve_install_dir() {
  parent_directory "$(resolve_self)"
}

active_binary() {
  local dir="$1"
  local version
  version=""
  [[ ! -r "$dir/.muse-version" ]] || version="$(<"$dir/.muse-version")"
  [[ "$version" =~ $version_pattern ]] || return 1
  printf '%s/muse-bin-%s\n' "$dir" "$version"
}

main() {
  dir="$(resolve_install_dir)"
  binary="$(active_binary "$dir" 2>/dev/null || true)"
  exec "$binary" "$@"
}

main "$@"
"#,
        )
        .unwrap();
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
    fn muse_override_with_stale_launcher_metadata_has_no_version() {
        let root = tempfile::tempdir().unwrap();
        let registry = default_vendor_version_registry();
        let adapter = registry.lookup("muse").unwrap();
        let override_path = executable(root.path(), "muse");
        fs::write(
            &override_path,
            "#!/usr/bin/env bash\nexec /other/muse \"$@\"\n",
        )
        .unwrap();
        fs::write(root.path().join(".muse-version"), "1.4.25-R9361.1\n").unwrap();
        let runtime = executable(root.path(), "muse-bin-1.4.25-R9361.1");

        assert_eq!(adapter.version_from_executable(&override_path), None);
        assert_eq!(
            adapter.version_from_executable(&runtime).as_deref(),
            Some("1.4.25-R9361.1")
        );
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
