use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::path::Path;

use fl_core::VendorUpdateStatus;
const MAX_SETTINGS_BYTES: u64 = 1024 * 1024;

/// What a reader does with a symbolic link as the final path component.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FinalLink {
    /// The link is not followed, so the path is not a regular file.
    Refuse,
    /// The link is followed. A link whose target is missing reads as a missing file.
    Follow,
}

/// A missing file is distinct from unavailable evidence. Never wait on a FIFO
/// or follow a final symlink substituted for this file.
pub(crate) fn read_bounded_regular_file(path: &Path, limit: u64) -> io::Result<Option<String>> {
    read_bounded_regular_bytes(path, limit, FinalLink::Refuse)?
        .map(|bytes| String::from_utf8(bytes).map_err(|_| io::ErrorKind::InvalidData.into()))
        .transpose()
}

/// Like [`read_bounded_regular_file`], but `InvalidData` only means the path is not a
/// regular file or exceeds `limit`; the contents are not decoded. With
/// [`FinalLink::Follow`] a final symlink is followed, but the opened file must still be a
/// regular file, so a link to a FIFO or device is neither waited on nor read.
fn read_bounded_regular_bytes(
    path: &Path,
    limit: u64,
    links: FinalLink,
) -> io::Result<Option<Vec<u8>>> {
    let metadata = match links {
        FinalLink::Refuse => path.symlink_metadata(),
        FinalLink::Follow => path.metadata(),
    };
    let metadata = match metadata {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !metadata.is_file() || metadata.len() > limit {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let Some(file) = open_regular(path, links)? else {
        return Ok(None);
    };
    let mut contents = Vec::new();
    file.take(limit + 1).read_to_end(&mut contents)?;
    if contents.len() as u64 > limit {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(Some(contents))
}

/// Open `path` without waiting on a FIFO and confirm through the open handle that it is a
/// regular file, which closes the race with a file swapped after the caller's pre-check.
/// With [`FinalLink::Follow`], a link target removed after that pre-check reads as a
/// missing file (`Ok(None)`); with [`FinalLink::Refuse`] every open error is returned.
fn open_regular(path: &Path, links: FinalLink) -> io::Result<Option<File>> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(match links {
            FinalLink::Refuse => libc::O_NOFOLLOW | libc::O_NONBLOCK,
            FinalLink::Follow => libc::O_NONBLOCK,
        });
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if links == FinalLink::Follow && error.kind() == io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    if !file.metadata()?.is_file() {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(Some(file))
}

/// Fixed, non-sensitive reasons for one settings file whose update control is unknown.
pub(crate) struct SettingsFile {
    pub(crate) unreadable: &'static str,
    pub(crate) rejected: &'static str,
    pub(crate) invalid: &'static str,
}

macro_rules! settings_file {
    ($name:literal) => {
        SettingsFile {
            unreadable: concat!($name, " unreadable"),
            rejected: concat!($name, " not a regular file or too large"),
            invalid: concat!($name, " failed validation"),
        }
    };
}

pub(crate) const CONFIG_TOML: SettingsFile = settings_file!("config.toml");
pub(crate) const SETTINGS_JSON: SettingsFile = settings_file!("settings.json");
pub(crate) const MANAGED_SETTINGS_JSON: SettingsFile = settings_file!("managed-settings.json");
pub(crate) const MANAGED_SETTINGS_D: SettingsFile = settings_file!("managed-settings.d");

/// Read and validate one settings file. A missing file is the vendor default. Anything
/// else that is not a valid document yields a reason naming only the file and the problem.
/// `links` says whether a final symlink is followed; only managed settings follow one.
pub(crate) fn read_settings<T: Default>(
    path: &Path,
    file: &SettingsFile,
    links: FinalLink,
    parse: impl FnOnce(&str) -> Option<T>,
) -> Result<T, &'static str> {
    match read_bounded_regular_bytes(path, MAX_SETTINGS_BYTES, links) {
        Ok(None) => Ok(T::default()),
        Ok(Some(contents)) => std::str::from_utf8(&contents)
            .ok()
            .and_then(parse)
            .ok_or(file.invalid),
        Err(error) if error.kind() == io::ErrorKind::InvalidData => Err(file.rejected),
        Err(_) => Err(file.unreadable),
    }
}

pub(crate) fn configured_updates<T: Default>(
    path: &Path,
    file: &SettingsFile,
    parse: impl FnOnce(&str) -> Option<T>,
    enabled: impl FnOnce(T) -> bool,
) -> VendorUpdateStatus {
    match read_settings(path, file, FinalLink::Refuse, parse) {
        Ok(settings) => VendorUpdateStatus::from_enabled(enabled(settings)),
        Err(reason) => VendorUpdateStatus::Unknown(reason),
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
        // Never read the real `/etc` from a test.
        let system_root = home.map(|home| home.join("system-root"));
        crate::default_vendor_version_registry().doctor_facts(&VendorDoctorContext {
            home,
            path,
            env: &env,
            system_root: system_root.as_deref(),
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
            Unknown("unrecognised value")
        );
        assert_eq!(
            vendor(&observed, "grok").cli.configured_updates,
            Unknown("GROK_CONFIG_PATH overlay")
        );
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
        assert_eq!(
            codex.cli.configured_updates,
            Unknown("config.toml failed validation")
        );
        assert_eq!(codex.daemon.as_ref().unwrap().configured_updates, Disabled);

        fs::write(&daemon, r#"{"updater":null}"#).unwrap();
        let observed = facts(Some(root.path()), None, &env);
        assert_eq!(
            vendor(&observed, "codex")
                .daemon
                .as_ref()
                .unwrap()
                .configured_updates,
            Unknown("settings.json failed validation")
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
        assert_eq!(
            vendor(&observed, "codex").cli.configured_updates,
            Unknown("CODEX_HOME not an absolute directory")
        );
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
                Unknown("settings.json failed validation"),
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

    fn claude_updates(home: &Path, env: &[(&str, &OsStr)]) -> VendorUpdateStatus {
        vendor(&facts(Some(home), None, env), "claude")
            .cli
            .configured_updates
    }

    #[test]
    fn claude_reads_update_controls_from_environment_and_user_settings() {
        use VendorUpdateStatus::{Disabled, Enabled, Unknown};

        let root = tempfile::tempdir().unwrap();
        let home = root.path();
        let settings = |source: &str| write(home, ".claude/settings.json", source);
        let disable = |value: &'static str| [("DISABLE_AUTOUPDATER", OsStr::new(value))];

        // Environment only; values are matched case-insensitively.
        assert_eq!(claude_updates(home, &[]), Enabled);
        assert_eq!(claude_updates(home, &disable("1")), Disabled);
        assert_eq!(claude_updates(home, &disable("TRUE")), Disabled);
        assert_eq!(claude_updates(home, &disable("False")), Enabled);
        assert_eq!(claude_updates(home, &disable("0")), Enabled);
        assert_eq!(
            claude_updates(home, &disable("yes")),
            Unknown("unrecognised value")
        );

        // settings.json only, including the second control.
        settings(r#"{"env":{"DISABLE_AUTOUPDATER":"true"}}"#);
        assert_eq!(claude_updates(home, &[]), Disabled);
        settings(r#"{"env":{"DISABLE_UPDATES":"1","OTHER":"x"},"model":"opus"}"#);
        assert_eq!(claude_updates(home, &[]), Disabled);
        settings(r#"{"env":{"DISABLE_AUTOUPDATER":"0"}}"#);
        assert_eq!(claude_updates(home, &[]), Enabled);
        settings(r#"{"env":{"DISABLE_AUTOUPDATER":"never"}}"#);
        assert_eq!(claude_updates(home, &[]), Unknown("unrecognised value"));

        // Both sources agree.
        settings(r#"{"env":{"DISABLE_AUTOUPDATER":"1"}}"#);
        assert_eq!(claude_updates(home, &disable("true")), Disabled);
        settings(r#"{"env":{"DISABLE_AUTOUPDATER":"false"}}"#);
        assert_eq!(claude_updates(home, &disable("0")), Enabled);

        // A settings file `env` value replaces the inherited variable.
        assert_eq!(claude_updates(home, &disable("1")), Enabled);
        settings(r#"{"env":{"DISABLE_AUTOUPDATER":""}}"#);
        assert_eq!(claude_updates(home, &disable("1")), Enabled);
        settings(r#"{"env":{"DISABLE_AUTOUPDATER":"1"}}"#);
        assert_eq!(claude_updates(home, &disable("0")), Disabled);
        // A different control that disables updates still decides.
        assert_eq!(
            claude_updates(
                home,
                &[
                    ("DISABLE_AUTOUPDATER", OsStr::new("0")),
                    ("DISABLE_UPDATES", OsStr::new("1")),
                ],
            ),
            Disabled
        );

        // Malformed or invalid settings never become the default.
        let invalid = Unknown("settings.json failed validation");
        for source in [
            "{not json",
            "[]",
            r#"{"env":null}"#,
            r#"{"env":["DISABLE_AUTOUPDATER"]}"#,
            r#"{"env":{"DISABLE_AUTOUPDATER":1}}"#,
            r#"{"env":{"DISABLE_AUTOUPDATER":"1","OTHER":true}}"#,
        ] {
            settings(source);
            assert_eq!(claude_updates(home, &[]), invalid, "{source}");
            assert_eq!(claude_updates(home, &disable("1")), invalid, "{source}");
        }
        fs::write(home.join(".claude/settings.json"), b"\xff\xfe").unwrap();
        assert_eq!(claude_updates(home, &[]), invalid);

        // `$CLAUDE_CONFIG_DIR` replaces `~/.claude`.
        let config = home.join("claude-config");
        write(
            &config,
            "settings.json",
            r#"{"env":{"DISABLE_UPDATES":"1"}}"#,
        );
        settings("{}");
        assert_eq!(
            claude_updates(home, &[("CLAUDE_CONFIG_DIR", config.as_os_str())]),
            Disabled
        );
        assert_eq!(
            claude_updates(home, &[("CLAUDE_CONFIG_DIR", OsStr::new("relative"))]),
            Unknown("config directory unavailable")
        );
    }

    /// This platform's managed settings directory under the test system root.
    fn managed_dir(home: &Path) -> PathBuf {
        let system_root = home.join("system-root");
        VendorDoctorContext {
            home: None,
            path: None,
            env: &|_: &str| None,
            system_root: Some(&system_root),
        }
        .system_dir(crate::claude::CLAUDE_MANAGED_DIR)
    }

    fn managed(home: &Path, relative: &str, source: &str) -> PathBuf {
        write(&managed_dir(home), relative, source)
    }

    fn claude_hosted(home: &Path) -> VendorUpdateStatus {
        vendor(&facts(Some(home), None, &[]), "claude").hosted_cli_updates
    }

    #[test]
    fn claude_managed_settings_outrank_user_settings_and_environment() {
        use VendorUpdateStatus::{Disabled, Enabled, Unknown};

        let root = tempfile::tempdir().unwrap();
        let home = root.path();
        let disable = |value: &'static str| [("DISABLE_AUTOUPDATER", OsStr::new(value))];
        write(
            home,
            ".claude/settings.json",
            r#"{"env":{"DISABLE_AUTOUPDATER":"0"}}"#,
        );

        // The managed file beats the user file, which beats the environment.
        let main = managed(
            home,
            "managed-settings.json",
            r#"{"env":{"DISABLE_AUTOUPDATER":"1"}}"#,
        );
        assert_eq!(claude_updates(home, &disable("0")), Disabled);
        fs::write(&main, r#"{"env":{"DISABLE_AUTOUPDATER":"0"}}"#).unwrap();
        write(
            home,
            ".claude/settings.json",
            r#"{"env":{"DISABLE_AUTOUPDATER":"1"}}"#,
        );
        assert_eq!(claude_updates(home, &disable("1")), Enabled);
        // A managed file that leaves a control unset defers to the lower sources.
        fs::write(&main, r#"{"env":{"OTHER":"x"}}"#).unwrap();
        assert_eq!(claude_updates(home, &[]), Disabled);
        // Claude reads an empty managed file as `{}`.
        fs::write(&main, "").unwrap();
        assert_eq!(claude_updates(home, &[]), Disabled);

        // Drop-ins merge after the main file in name order; the later file wins per key.
        fs::write(&main, r#"{"env":{"DISABLE_AUTOUPDATER":"1"}}"#).unwrap();
        managed(
            home,
            "managed-settings.d/20-keep.json",
            r#"{"env":{"DISABLE_AUTOUPDATER":"0"}}"#,
        );
        assert_eq!(claude_updates(home, &[]), Enabled);
        let late = managed(
            home,
            "managed-settings.d/30-other.json",
            r#"{"env":{"OTHER":"x"}}"#,
        );
        assert_eq!(claude_updates(home, &[]), Enabled);
        fs::write(&late, r#"{"env":{"DISABLE_AUTOUPDATER":"true"}}"#).unwrap();
        assert_eq!(claude_updates(home, &[]), Disabled);
        // Hidden files and other extensions are not drop-ins, so they are not validated.
        managed(home, "managed-settings.d/.40-hidden.json", "{not json");
        managed(home, "managed-settings.d/50-notes.txt", "{not json");
        assert_eq!(claude_updates(home, &[]), Disabled);

        // An invalid managed file is unknown even when the lower sources agree.
        fs::write(&late, r#"{"env":{"DISABLE_AUTOUPDATER":1}}"#).unwrap();
        assert_eq!(
            claude_updates(home, &disable("1")),
            Unknown("managed-settings.d failed validation")
        );
        fs::remove_file(&late).unwrap();
        fs::write(&main, "{not json").unwrap();
        assert_eq!(
            claude_updates(home, &disable("1")),
            Unknown("managed-settings.json failed validation")
        );

        // A managed value decides even when the user file is invalid.
        fs::write(&main, r#"{"env":{"DISABLE_UPDATES":"1"}}"#).unwrap();
        write(home, ".claude/settings.json", "[]");
        assert_eq!(claude_updates(home, &[]), Disabled);
    }

    #[test]
    fn claude_bounds_the_managed_drop_ins() {
        use VendorUpdateStatus::{Disabled, Unknown};

        let root = tempfile::tempdir().unwrap();
        let home = root.path();
        for index in 0..64 {
            managed(home, &format!("managed-settings.d/{index:02}.json"), "{}");
        }
        assert_eq!(
            claude_updates(home, &[("DISABLE_UPDATES", OsStr::new("1"))]),
            Disabled
        );
        managed(home, "managed-settings.d/64.json", "{}");
        assert_eq!(
            claude_updates(home, &[("DISABLE_UPDATES", OsStr::new("1"))]),
            Unknown("managed-settings.d has too many files")
        );
        assert_eq!(
            claude_hosted(home),
            Unknown("managed-settings.d has too many files")
        );
    }

    #[test]
    fn claude_bounds_every_entry_in_the_managed_drop_in_directory() {
        use VendorUpdateStatus::{Disabled, Unknown};

        let root = tempfile::tempdir().unwrap();
        let home = root.path();
        let env = [("DISABLE_UPDATES", OsStr::new("1"))];
        // Entries that are not drop-ins still count toward the limit.
        for index in 0..255 {
            managed(home, &format!("managed-settings.d/{index:03}.txt"), "");
        }
        managed(home, "managed-settings.d/.hidden.json", "{}");
        assert_eq!(claude_updates(home, &env), Disabled);
        managed(home, "managed-settings.d/256.txt", "");
        assert_eq!(
            claude_updates(home, &env),
            Unknown("managed-settings.d has too many entries")
        );
        assert_eq!(
            claude_hosted(home),
            Unknown("managed-settings.d has too many entries")
        );
    }

    #[test]
    fn claude_managed_settings_use_the_documented_platform_directory() {
        let expected = if cfg!(target_os = "macos") {
            "/Library/Application Support/ClaudeCode"
        } else if cfg!(windows) {
            r"C:\Program Files\ClaudeCode"
        } else {
            "/etc/claude-code"
        };
        assert_eq!(crate::claude::CLAUDE_MANAGED_DIR, expected);

        // The test system root holds the same relative path.
        let root = tempfile::tempdir().unwrap();
        let home = root.path();
        managed(
            home,
            "managed-settings.json",
            r#"{"env":{"DISABLE_AUTOUPDATER":"1"}}"#,
        );
        let relative = Path::new(expected)
            .components()
            .filter(|component| matches!(component, std::path::Component::Normal(_)))
            .collect::<PathBuf>();
        assert!(
            home.join("system-root")
                .join(relative)
                .join("managed-settings.json")
                .is_file()
        );
        assert_eq!(claude_updates(home, &[]), VendorUpdateStatus::Disabled);
    }

    #[test]
    fn claude_hosted_launches_follow_only_managed_settings() {
        use VendorUpdateStatus::{Disabled, Enabled, Unknown};

        let root = tempfile::tempdir().unwrap();
        let home = root.path();
        // `--settings` outranks the user file.
        write(
            home,
            ".claude/settings.json",
            r#"{"env":{"DISABLE_AUTOUPDATER":"0"}}"#,
        );
        assert_eq!(claude_hosted(home), Disabled);

        let main = managed(
            home,
            "managed-settings.json",
            r#"{"env":{"DISABLE_AUTOUPDATER":"0"}}"#,
        );
        assert_eq!(claude_hosted(home), Enabled);
        fs::write(
            &main,
            r#"{"env":{"DISABLE_AUTOUPDATER":"0","DISABLE_UPDATES":"1"}}"#,
        )
        .unwrap();
        assert_eq!(claude_hosted(home), Disabled);
        // Keeping the other control does not undo FerroLoop's switch.
        fs::write(&main, r#"{"env":{"DISABLE_UPDATES":"0"}}"#).unwrap();
        assert_eq!(claude_hosted(home), Disabled);
        fs::write(&main, r#"{"env":{"DISABLE_AUTOUPDATER":"never"}}"#).unwrap();
        assert_eq!(claude_hosted(home), Unknown("unrecognised value"));
        fs::write(&main, r#"{"env":null}"#).unwrap();
        assert_eq!(
            claude_hosted(home),
            Unknown("managed-settings.json failed validation")
        );
        fs::remove_file(&main).unwrap();
        fs::create_dir_all(managed_dir(home)).unwrap();
        fs::write(managed_dir(home).join("managed-settings.d"), "{}").unwrap();
        assert_eq!(
            claude_hosted(home),
            Unknown("managed-settings.d unreadable")
        );
    }

    #[test]
    fn claude_rejects_oversized_or_symlinked_settings_without_following() {
        use VendorUpdateStatus::Unknown;

        let root = tempfile::tempdir().unwrap();
        let home = root.path();
        let rejected = Unknown("settings.json not a regular file or too large");
        let path = write(home, ".claude/settings.json", "{}");
        fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(MAX_SETTINGS_BYTES + 1)
            .unwrap();
        assert_eq!(
            claude_updates(home, &[("DISABLE_AUTOUPDATER", OsStr::new("1"))]),
            rejected
        );

        #[cfg(unix)]
        {
            fs::remove_file(&path).unwrap();
            let target = write(
                home,
                "elsewhere.json",
                r#"{"env":{"DISABLE_AUTOUPDATER":"1"}}"#,
            );
            std::os::unix::fs::symlink(&target, &path).unwrap();
            assert_eq!(claude_updates(home, &[]), rejected);
            fs::remove_file(&path).unwrap();
            make_fifo(&path);
            assert_eq!(claude_updates(home, &[]), rejected);
            fs::remove_file(&path).unwrap();
        }
    }

    /// Claude follows a symlinked managed file, so Doctor does too. The target must still be
    /// a regular file, and the user `settings.json` still refuses links.
    #[cfg(unix)]
    #[test]
    fn claude_follows_symlinked_managed_settings_to_regular_files_only() {
        use VendorUpdateStatus::{Disabled, Enabled, Unknown};
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let home = root.path();
        let disabling = write(
            home,
            "checkout/disable.json",
            r#"{"env":{"DISABLE_AUTOUPDATER":"1"}}"#,
        );
        let keeping = write(
            home,
            "checkout/keep.json",
            r#"{"env":{"DISABLE_AUTOUPDATER":"0"}}"#,
        );
        let main = managed_dir(home).join("managed-settings.json");
        fs::create_dir_all(managed_dir(home)).unwrap();

        // A linked managed-settings.json that disables updates outranks the environment.
        symlink(&disabling, &main).unwrap();
        let keep_env = [("DISABLE_AUTOUPDATER", OsStr::new("0"))];
        assert_eq!(claude_updates(home, &keep_env), Disabled);
        assert_eq!(claude_hosted(home), Disabled);

        // A linked drop-in is honoured and replaces the main file's value.
        let drop_in = managed_dir(home).join("managed-settings.d/50-keep.json");
        fs::create_dir_all(drop_in.parent().unwrap()).unwrap();
        symlink(&keeping, &drop_in).unwrap();
        assert_eq!(claude_updates(home, &[]), Enabled);
        assert_eq!(claude_hosted(home), Enabled);
        fs::remove_file(&drop_in).unwrap();

        // A dangling link reads as a missing file and sets nothing, as it does for Claude.
        fs::remove_file(&main).unwrap();
        symlink(home.join("checkout/missing.json"), &main).unwrap();
        assert_eq!(claude_updates(home, &keep_env), Enabled);
        assert_eq!(claude_hosted(home), Disabled);
        symlink(home.join("checkout/missing.json"), &drop_in).unwrap();
        assert_eq!(claude_updates(home, &keep_env), Enabled);
        fs::remove_file(&drop_in).unwrap();

        // A link to a FIFO or a directory is unknown, and the FIFO is never waited on.
        let fifo = home.join("checkout/fifo.json");
        make_fifo(&fifo);
        fs::remove_file(&main).unwrap();
        symlink(&fifo, &main).unwrap();
        assert_eq!(
            claude_updates(home, &[]),
            Unknown("managed-settings.json not a regular file or too large")
        );
        assert_eq!(
            claude_hosted(home),
            Unknown("managed-settings.json not a regular file or too large")
        );
        fs::remove_file(&main).unwrap();
        symlink(&fifo, &drop_in).unwrap();
        assert_eq!(
            claude_updates(home, &[]),
            Unknown("managed-settings.d not a regular file or too large")
        );
        fs::remove_file(&drop_in).unwrap();
        symlink(home.join("checkout"), &main).unwrap();
        assert_eq!(
            claude_updates(home, &[]),
            Unknown("managed-settings.json not a regular file or too large")
        );
        fs::remove_file(&main).unwrap();

        // The user settings.json still refuses a link, even to a regular file.
        fs::create_dir_all(home.join(".claude")).unwrap();
        symlink(&disabling, home.join(".claude/settings.json")).unwrap();
        assert_eq!(
            claude_updates(home, &[]),
            Unknown("settings.json not a regular file or too large")
        );
    }

    #[cfg(unix)]
    fn make_fifo(path: &Path) {
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: `path` is a valid NUL-terminated C string for the duration of the call.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    }

    /// The open-and-fstat step on its own, past the caller's metadata pre-check: a FIFO
    /// reached through a followed link must be rejected without waiting for a writer.
    /// The call runs on a thread so a blocking open fails the test instead of hanging it.
    #[cfg(unix)]
    #[test]
    fn followed_open_rejects_fifo_without_waiting() {
        use std::os::unix::fs::symlink;
        use std::sync::mpsc;
        use std::time::Duration;

        let root = tempfile::tempdir().unwrap();
        let fifo = root.path().join("fifo.json");
        make_fifo(&fifo);
        let link = root.path().join("managed-settings.json");
        symlink(&fifo, &link).unwrap();

        let (sender, receiver) = mpsc::channel();
        let opener = link.clone();
        std::thread::spawn(move || {
            let result = open_regular(&opener, FinalLink::Follow).map(|file| file.is_some());
            let _ = sender.send(result.map_err(|error| error.kind()));
        });
        let result = match receiver.recv_timeout(Duration::from_secs(5)) {
            Ok(result) => result,
            Err(_) => {
                // Release the blocked reader before failing so the thread can exit.
                let _ = fs::OpenOptions::new().write(true).open(&fifo);
                panic!("opening a linked FIFO waited for a writer");
            }
        };
        assert_eq!(result, Err(io::ErrorKind::InvalidData));
    }

    /// A link target removed between the pre-check and the open reads as a missing file
    /// when links are followed, and stays an open error when they are refused.
    #[cfg(unix)]
    #[test]
    fn followed_open_treats_vanished_target_as_missing() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let link = root.path().join("managed-settings.json");
        symlink(root.path().join("removed.json"), &link).unwrap();
        assert!(matches!(open_regular(&link, FinalLink::Follow), Ok(None)));

        let missing = root.path().join("settings.json");
        assert_eq!(
            open_regular(&missing, FinalLink::Refuse)
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
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
            VendorUpdateStatus::Unknown("config.toml not a regular file or too large")
        );
        fs::remove_file(&config).unwrap();
        fs::create_dir(config).unwrap();
        let observed = facts(Some(root.path()), None, &[]);
        assert_eq!(
            vendor(&observed, "grok").cli.configured_updates,
            VendorUpdateStatus::Unknown("config.toml not a regular file or too large")
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
