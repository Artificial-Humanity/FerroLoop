use std::fs::OpenOptions;
use std::io::{self, Read};
use std::path::Path;

use fl_core::VendorUpdateStatus;
const MAX_SETTINGS_BYTES: u64 = 1024 * 1024;

/// A missing file is distinct from unavailable evidence. Never wait on a FIFO
/// or follow a final symlink substituted for this file.
pub(crate) fn read_bounded_regular_file(path: &Path, limit: u64) -> io::Result<Option<String>> {
    let metadata = match path.symlink_metadata() {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !metadata.is_file() || metadata.len() > limit {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let mut contents = String::new();
    file.take(limit + 1).read_to_string(&mut contents)?;
    if contents.len() as u64 > limit {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(Some(contents))
}

pub(crate) fn configured_updates<T: Default>(
    path: &Path,
    parse: impl FnOnce(&str) -> Option<T>,
    enabled: impl FnOnce(T) -> bool,
) -> VendorUpdateStatus {
    let settings = match read_bounded_regular_file(path, MAX_SETTINGS_BYTES) {
        Ok(None) => Some(T::default()),
        Ok(Some(contents)) => parse(&contents),
        Err(_) => None,
    };
    match settings.map(enabled) {
        Some(enabled) => VendorUpdateStatus::from_enabled(enabled),
        None => VendorUpdateStatus::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::{OsStr, OsString};
    use std::fs;
    use std::path::PathBuf;

    use fl_core::{
        VendorDoctorContext, VendorDoctorFacts, VendorInstalledVersion, VendorUpdateStatus,
    };

    use super::*;

    fn write(root: &Path, relative: &str, source: &str) -> PathBuf {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, source).unwrap();
        path
    }

    fn facts(
        home: Option<&Path>,
        path: Option<&OsStr>,
        env: &[(&str, &OsStr)],
    ) -> Vec<VendorDoctorFacts> {
        let env = |name: &str| {
            env.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| OsString::from(value))
        };
        crate::default_vendor_version_registry().doctor_facts(&VendorDoctorContext {
            home,
            path,
            env: &env,
        })
    }

    fn vendor<'a>(facts: &'a [VendorDoctorFacts], name: &str) -> &'a VendorDoctorFacts {
        facts.iter().find(|fact| fact.vendor == name).unwrap()
    }

    #[test]
    fn environment_and_config_update_controls_are_independent_of_hosted_policy() {
        use VendorUpdateStatus::{Disabled, Enabled, Unknown};

        let root = tempfile::tempdir().unwrap();
        let grok_home = root.path().join("grok-home");
        write(&grok_home, "config.toml", "[cli]\nauto_update = false\n");
        let env = [
            ("AGY_CLI_DISABLE_AUTO_UPDATE", OsStr::new("true")),
            ("DISABLE_UPDATES", OsStr::new("1")),
            ("GROK_DISABLE_AUTOUPDATER", OsStr::new("FaLsE")),
            ("GROK_HOME", grok_home.as_os_str()),
            ("MUSE_NO_AUTO_UPDATE", OsStr::new("0")),
        ];
        let observed = facts(Some(root.path()), None, &env);
        assert_eq!(
            observed.iter().map(|fact| fact.vendor).collect::<Vec<_>>(),
            ["antigravity", "claude", "codex", "grok", "muse"]
        );
        assert!(
            observed
                .iter()
                .all(|fact| fact.hosted_cli_updates == Disabled)
        );
        assert_eq!(
            vendor(&observed, "antigravity").cli.configured_updates,
            Disabled
        );
        assert_eq!(vendor(&observed, "claude").cli.configured_updates, Disabled);
        assert_eq!(vendor(&observed, "codex").cli.configured_updates, Enabled);
        assert_eq!(vendor(&observed, "grok").cli.configured_updates, Disabled);
        assert_eq!(vendor(&observed, "muse").cli.configured_updates, Enabled);

        let env = [
            ("AGY_CLI_DISABLE_AUTO_UPDATE", OsStr::new("unexpected")),
            ("GROK_CONFIG_PATH", OsStr::new("/overridden/grok-config")),
            ("MUSE_NO_AUTO_UPDATE", OsStr::new("1")),
        ];
        let observed = facts(Some(root.path()), None, &env);
        assert_eq!(
            vendor(&observed, "antigravity").cli.configured_updates,
            Unknown
        );
        assert_eq!(vendor(&observed, "grok").cli.configured_updates, Unknown);
        assert_eq!(vendor(&observed, "muse").cli.configured_updates, Disabled);

        // An affirmative Grok environment switch suppresses updates despite
        // an unreadable config selection.
        let env = [
            ("GROK_DISABLE_AUTOUPDATER", OsStr::new("yes")),
            ("GROK_CONFIG_PATH", OsStr::new("/overridden/grok-config")),
        ];
        let observed = facts(Some(root.path()), None, &env);
        assert_eq!(vendor(&observed, "grok").cli.configured_updates, Disabled);
    }

    #[test]
    fn codex_client_and_daemon_settings_use_the_selected_home() {
        use VendorUpdateStatus::{Disabled, Enabled, Unknown};

        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("separate-config");
        let client = write(
            &home,
            "config.toml",
            "check_for_update_on_startup = false\n",
        );
        let daemon = write(
            &home,
            "app-server-daemon/settings.json",
            r#"{"updater":{"autoUpdateEnabled":true}}"#,
        );
        let env = [("CODEX_HOME", home.as_os_str())];
        let observed = facts(Some(root.path()), None, &env);
        let codex = vendor(&observed, "codex");
        assert_eq!(codex.cli.configured_updates, Disabled);
        assert_eq!(codex.daemon.as_ref().unwrap().configured_updates, Enabled);
        assert_eq!(
            codex.daemon.as_ref().unwrap().installed_version,
            VendorInstalledVersion::Absent
        );

        fs::write(client, "check_for_update_on_startup = 'false'\n").unwrap();
        fs::write(&daemon, r#"{"updater":{"autoUpdateEnabled":false}}"#).unwrap();
        let observed = facts(Some(root.path()), None, &env);
        let codex = vendor(&observed, "codex");
        assert_eq!(codex.cli.configured_updates, Unknown);
        assert_eq!(codex.daemon.as_ref().unwrap().configured_updates, Disabled);

        fs::write(&daemon, r#"{"updater":null}"#).unwrap();
        let observed = facts(Some(root.path()), None, &env);
        assert_eq!(
            vendor(&observed, "codex")
                .daemon
                .as_ref()
                .unwrap()
                .configured_updates,
            Unknown
        );
        fs::remove_file(daemon).unwrap();
        let observed = facts(Some(root.path()), None, &env);
        assert_eq!(
            vendor(&observed, "codex")
                .daemon
                .as_ref()
                .unwrap()
                .configured_updates,
            Enabled
        );

        let missing = root.path().join("missing");
        let observed = facts(
            Some(root.path()),
            None,
            &[("CODEX_HOME", missing.as_os_str())],
        );
        assert_eq!(vendor(&observed, "codex").cli.configured_updates, Unknown);
    }

    #[test]
    fn codex_daemon_validates_the_whole_settings_document() {
        use VendorUpdateStatus::{Disabled, Unknown};

        let root = tempfile::tempdir().unwrap();
        let path = ".codex/app-server-daemon/settings.json";
        for source in [
            r#"{"updater":{"autoUpdateEnabled":false,"updateIntervalMinutes":0}}"#,
            r#"{"updater":{"autoUpdateEnabled":false,"updateIntervalMinutes":"60"}}"#,
            r#"{"updater":{"autoUpdateEnabled":false,"updateIntervalMinutes":4294967296}}"#,
            r#"{"updater":{"autoUpdateEnabled":false},"shutdownGraceSeconds":301}"#,
            r#"{"updater":{"autoUpdateEnabled":false},"shutdownGraceSeconds":null}"#,
            r#"{"updater":{"autoUpdateEnabled":false},"remoteControlEnabled":0}"#,
            r#"{"updater":{"autoUpdateEnabled":false},"featureOverrides":{"x":"no"}}"#,
        ] {
            write(root.path(), path, source);
            let observed = facts(Some(root.path()), None, &[]);
            assert_eq!(
                vendor(&observed, "codex")
                    .daemon
                    .as_ref()
                    .unwrap()
                    .configured_updates,
                Unknown,
                "{source}",
            );
        }
        write(
            root.path(),
            path,
            r#"{"updater":{"autoUpdateEnabled":false,"updateIntervalMinutes":1},"shutdownGraceSeconds":300,"remoteControlEnabled":true,"featureOverrides":{"x":false}}"#,
        );
        let observed = facts(Some(root.path()), None, &[]);
        assert_eq!(
            vendor(&observed, "codex")
                .daemon
                .as_ref()
                .unwrap()
                .configured_updates,
            Disabled,
        );
    }

    #[cfg(unix)]
    #[test]
    fn codex_daemon_selects_the_managed_install_using_retained_daemon_artifacts() {
        use VendorInstalledVersion::{Absent, Known, Unknown};
        use std::os::unix::fs::{PermissionsExt, symlink};

        let root = tempfile::tempdir().unwrap();
        let home = root.path();
        let legacy = write(
            home,
            ".codex/packages/standalone/releases/0.167.0-x86_64-unknown-linux-musl/codex",
            "legacy-image",
        );
        fs::set_permissions(&legacy, fs::Permissions::from_mode(0o700)).unwrap();
        symlink(
            legacy.parent().unwrap(),
            home.join(".codex/packages/standalone/current"),
        )
        .unwrap();
        let version = || {
            let observed = facts(Some(home), None, &[]);
            vendor(&observed, "codex")
                .daemon
                .as_ref()
                .unwrap()
                .installed_version
                .clone()
        };
        write(home, ".codex/app-server-daemon/daemon.lock", "");
        assert_eq!(version(), Absent);

        write(
            home,
            ".codex/app-server-daemon/app-server-updater.stderr.log",
            "",
        );
        assert_eq!(version(), Known("0.167.0".into()));

        write(home, ".codex/app-server-daemon/daemon.pid", "");
        assert_eq!(version(), Unknown);
        fs::remove_file(home.join(".codex/app-server-daemon/daemon.pid")).unwrap();

        let dedicated = write(
            home,
            ".codex/packages/app-server-daemon/releases/0.172.0-x86_64-unknown-linux-musl/codex",
            "daemon-image",
        );
        fs::set_permissions(&dedicated, fs::Permissions::from_mode(0o700)).unwrap();
        symlink(
            dedicated.parent().unwrap(),
            home.join(".codex/packages/app-server-daemon/current"),
        )
        .unwrap();
        assert_eq!(version(), Known("0.172.0".into()));
    }

    #[test]
    fn oversized_or_non_regular_config_does_not_report_a_default() {
        let root = tempfile::tempdir().unwrap();
        let config = write(root.path(), ".grok/config.toml", "[cli]\n");
        fs::OpenOptions::new()
            .write(true)
            .open(&config)
            .unwrap()
            .set_len(MAX_SETTINGS_BYTES + 1)
            .unwrap();
        let observed = facts(Some(root.path()), None, &[]);
        assert_eq!(
            vendor(&observed, "grok").cli.configured_updates,
            VendorUpdateStatus::Unknown
        );
        fs::remove_file(&config).unwrap();
        fs::create_dir(config).unwrap();
        let observed = facts(Some(root.path()), None, &[]);
        assert_eq!(
            vendor(&observed, "grok").cli.configured_updates,
            VendorUpdateStatus::Unknown
        );
    }

    #[cfg(unix)]
    #[test]
    fn native_version_evidence_prefers_path_and_keeps_opaque_install_unknown() {
        use VendorInstalledVersion::{Absent, Known, Unknown};
        use std::os::unix::fs::{PermissionsExt, symlink};

        let root = tempfile::tempdir().unwrap();
        let home = root.path();
        let own_bin = home.join(".local/bin");
        let path_bin = home.join("path");
        let cli_image = write(home, ".local/share/claude/versions/2.1.332", "native-image");
        let daemon_image = write(
            home,
            ".codex/packages/app-server-daemon/releases/0.172.0-x86_64-unknown-linux-musl/bin/codex",
            "daemon-image",
        );
        let opaque = write(home, ".local/bin/agy", "plain-executable");
        for path in [&cli_image, &opaque, &daemon_image] {
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        symlink(cli_image, own_bin.join("claude")).unwrap();
        symlink(
            daemon_image.parent().unwrap().parent().unwrap(),
            home.join(".codex/packages/app-server-daemon/current"),
        )
        .unwrap();
        let initial = facts(Some(home), Some(path_bin.as_os_str()), &[]);
        assert_eq!(
            vendor(&initial, "claude").cli.installed_version,
            Known("2.1.332".into())
        );
        assert_eq!(
            vendor(&initial, "antigravity").cli.installed_version,
            Unknown
        );
        assert_eq!(vendor(&initial, "muse").cli.installed_version, Absent);
        assert_eq!(
            vendor(&initial, "codex")
                .daemon
                .as_ref()
                .unwrap()
                .installed_version,
            Known("0.172.0".into())
        );

        let shadow = write(home, "path/claude", "unknown-build");
        fs::set_permissions(shadow, fs::Permissions::from_mode(0o700)).unwrap();
        let observed = facts(Some(home), Some(path_bin.as_os_str()), &[]);
        assert_eq!(vendor(&observed, "claude").cli.installed_version, Unknown);
    }
}
