//! Unified `FerroDeck` settings (Record 4), persisted at `<home>/.config/fl/ferrodeck-settings.json`.
//!
//! Loading never fails: a missing file gives the defaults, and a partial or damaged one keeps
//! every field it can still read and takes the default for the rest.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::artifacts::write_file_atomic;

pub(crate) const MIN_POLL_INTERVAL_SECS: u64 = 2;
pub(crate) const MAX_POLL_INTERVAL_SECS: u64 = 300;
/// How long `FerroDeck` chat attachments are kept when no setting says otherwise.
pub(crate) const DEFAULT_ATTACHMENT_RETENTION_DAYS: u64 = 7;
/// The longest attachment retention a patch may set.
pub(crate) const MAX_ATTACHMENT_RETENTION_DAYS: u64 = 365;

/// Serialises every settings read-modify-write.
static SETTINGS_LOCK: Mutex<()> = Mutex::new(());

pub(crate) fn settings_path(home: &Path) -> PathBuf {
    home.join(".config/fl/ferrodeck-settings.json")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Theme {
    Dark,
    Light,
    System,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Subsystems {
    pub ferrogate_enabled: bool,
    pub ferrostate_enabled: bool,
    pub ferrostep_enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Notifications {
    pub notify_on_pr_review: bool,
    pub notify_on_session_error: bool,
    pub notify_on_wire_urgent: bool,
    pub sound_enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct HubConnection {
    pub auto_reconnect_ws: bool,
    pub poll_interval_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Settings {
    pub theme_preference: Theme,
    pub timezone: String,
    pub subsystems: Subsystems,
    pub notifications: Notifications,
    pub hub_connection: HubConnection,
    /// Days the hub keeps uploaded chat attachments (`0..=365`, default 7). The hub deletes an
    /// attachment older than this after an upload and on its periodic sweep. `0` turns automatic
    /// deletion off. Expiry does not check whether a message still refers to the file, so an
    /// expired attachment breaks its image link in old chat transcripts.
    pub attachment_retention_days: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme_preference: Theme::Dark,
            timezone: "UTC".to_string(),
            subsystems: Subsystems {
                ferrogate_enabled: true,
                ferrostate_enabled: true,
                ferrostep_enabled: true,
            },
            notifications: Notifications {
                notify_on_pr_review: true,
                notify_on_session_error: true,
                notify_on_wire_urgent: true,
                sound_enabled: false,
            },
            hub_connection: HubConnection {
                auto_reconnect_ws: true,
                poll_interval_secs: 10,
            },
            attachment_retention_days: DEFAULT_ATTACHMENT_RETENTION_DAYS,
        }
    }
}

/// A `PUT /api/v1/settings` body: every field optional, merged field by field. Unknown fields
/// are rejected at every level.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SettingsPatch {
    pub theme_preference: Option<Theme>,
    pub timezone: Option<String>,
    pub subsystems: Option<SubsystemsPatch>,
    pub notifications: Option<NotificationsPatch>,
    pub hub_connection: Option<HubConnectionPatch>,
    /// See [`Settings::attachment_retention_days`]. A value above 365 is refused.
    pub attachment_retention_days: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SubsystemsPatch {
    pub ferrogate_enabled: Option<bool>,
    pub ferrostate_enabled: Option<bool>,
    pub ferrostep_enabled: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NotificationsPatch {
    pub notify_on_pr_review: Option<bool>,
    pub notify_on_session_error: Option<bool>,
    pub notify_on_wire_urgent: Option<bool>,
    pub sound_enabled: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HubConnectionPatch {
    pub auto_reconnect_ws: Option<bool>,
    pub poll_interval_secs: Option<u64>,
}

fn poll_interval_in_range(secs: u64) -> bool {
    (MIN_POLL_INTERVAL_SECS..=MAX_POLL_INTERVAL_SECS).contains(&secs)
}

fn retention_in_range(days: u64) -> bool {
    days <= MAX_ATTACHMENT_RETENTION_DAYS
}

fn normalize_timezone(raw: &str) -> Option<String> {
    let tz = raw.trim();
    if tz.is_empty() || tz.len() > 64 {
        return None;
    }
    if tz.eq_ignore_ascii_case("utc") {
        return Some("UTC".to_string());
    }
    if tz.eq_ignore_ascii_case("local") {
        return Some("local".to_string());
    }
    if tz.starts_with('/') || tz.ends_with('/') || tz.contains("..") || tz.contains("//") {
        return None;
    }
    if !tz
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'+' | b'-' | b'/'))
    {
        return None;
    }
    Some(tz.to_string())
}

impl Settings {
    /// Apply `patch` on top of `self`. Fails, changing nothing, when the poll interval, timezone
    /// or attachment retention is invalid.
    pub(crate) fn merged(&self, patch: SettingsPatch) -> Result<Self, String> {
        let mut next = self.clone();
        if let Some(theme) = patch.theme_preference {
            next.theme_preference = theme;
        }
        if let Some(ref raw_tz) = patch.timezone {
            let Some(tz) = normalize_timezone(raw_tz) else {
                return Err(format!("invalid timezone: {raw_tz}"));
            };
            next.timezone = tz;
        }
        if let Some(p) = patch.subsystems {
            let s = &mut next.subsystems;
            set(&mut s.ferrogate_enabled, p.ferrogate_enabled);
            set(&mut s.ferrostate_enabled, p.ferrostate_enabled);
            set(&mut s.ferrostep_enabled, p.ferrostep_enabled);
        }
        if let Some(p) = patch.notifications {
            let n = &mut next.notifications;
            set(&mut n.notify_on_pr_review, p.notify_on_pr_review);
            set(&mut n.notify_on_session_error, p.notify_on_session_error);
            set(&mut n.notify_on_wire_urgent, p.notify_on_wire_urgent);
            set(&mut n.sound_enabled, p.sound_enabled);
        }
        if let Some(p) = patch.hub_connection {
            set(
                &mut next.hub_connection.auto_reconnect_ws,
                p.auto_reconnect_ws,
            );
            if let Some(secs) = p.poll_interval_secs {
                if !poll_interval_in_range(secs) {
                    return Err(format!(
                        "poll_interval_secs must be between {MIN_POLL_INTERVAL_SECS} and \
                         {MAX_POLL_INTERVAL_SECS}"
                    ));
                }
                next.hub_connection.poll_interval_secs = secs;
            }
        }
        if let Some(days) = patch.attachment_retention_days {
            if !retention_in_range(days) {
                return Err(format!(
                    "attachment_retention_days must be between 0 and \
                     {MAX_ATTACHMENT_RETENTION_DAYS}"
                ));
            }
            next.attachment_retention_days = days;
        }
        Ok(next)
    }

    /// Read what a stored document still says: each readable, valid field overrides the default.
    fn from_stored(doc: &Value) -> Self {
        let mut s = Self::default();
        let Some(root) = doc.as_object() else {
            return s;
        };
        if let Some(theme) = root
            .get("theme_preference")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
        {
            s.theme_preference = theme;
        }
        if let Some(tz) = root
            .get("timezone")
            .and_then(Value::as_str)
            .and_then(normalize_timezone)
        {
            s.timezone = tz;
        }
        let flag = |group: &str, key: &str, slot: &mut bool| {
            if let Some(b) = root
                .get(group)
                .and_then(|g| g.get(key))
                .and_then(Value::as_bool)
            {
                *slot = b;
            }
        };
        flag(
            "subsystems",
            "ferrogate_enabled",
            &mut s.subsystems.ferrogate_enabled,
        );
        flag(
            "subsystems",
            "ferrostate_enabled",
            &mut s.subsystems.ferrostate_enabled,
        );
        flag(
            "subsystems",
            "ferrostep_enabled",
            &mut s.subsystems.ferrostep_enabled,
        );
        flag(
            "notifications",
            "notify_on_pr_review",
            &mut s.notifications.notify_on_pr_review,
        );
        flag(
            "notifications",
            "notify_on_session_error",
            &mut s.notifications.notify_on_session_error,
        );
        flag(
            "notifications",
            "notify_on_wire_urgent",
            &mut s.notifications.notify_on_wire_urgent,
        );
        flag(
            "notifications",
            "sound_enabled",
            &mut s.notifications.sound_enabled,
        );
        flag(
            "hub_connection",
            "auto_reconnect_ws",
            &mut s.hub_connection.auto_reconnect_ws,
        );
        if let Some(secs) = root
            .get("hub_connection")
            .and_then(|g| g.get("poll_interval_secs"))
            .and_then(Value::as_u64)
            .filter(|secs| poll_interval_in_range(*secs))
        {
            s.hub_connection.poll_interval_secs = secs;
        }
        if let Some(days) = root
            .get("attachment_retention_days")
            .and_then(Value::as_u64)
            .filter(|days| retention_in_range(*days))
        {
            s.attachment_retention_days = days;
        }
        s
    }
}

fn set<T>(slot: &mut T, value: Option<T>) {
    if let Some(v) = value {
        *slot = v;
    }
}

/// The saved settings, or the defaults for whatever is missing or unreadable. Never fails.
pub(crate) fn load(home: &Path) -> Settings {
    let Ok(bytes) = std::fs::read(settings_path(home)) else {
        return Settings::default();
    };
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(doc) => Settings::from_stored(&doc),
        Err(_) => Settings::default(),
    }
}

fn save(home: &Path, settings: &Settings) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(settings).map_err(io::Error::other)?;
    write_file_atomic(&settings_path(home), &bytes)
}

/// Why `update` did not apply a patch.
#[derive(Debug)]
pub(crate) enum UpdateError {
    /// The patch itself is invalid (client error).
    Invalid(String),
    /// The merged settings could not be written.
    Io(io::Error),
}

/// Merge `patch` into the saved settings and persist the result, under one lock.
pub(crate) fn update(home: &Path, patch: SettingsPatch) -> Result<Settings, UpdateError> {
    let _guard = SETTINGS_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let next = load(home).merged(patch).map_err(UpdateError::Invalid)?;
    save(home, &next).map_err(UpdateError::Io)?;
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    #[test]
    fn defaults_match_the_contract() {
        assert_eq!(
            serde_json::to_value(Settings::default()).unwrap(),
            json!({
                "theme_preference": "dark",
                "timezone": "UTC",
                "subsystems": {
                    "ferrogate_enabled": true,
                    "ferrostate_enabled": true,
                    "ferrostep_enabled": true,
                },
                "notifications": {
                    "notify_on_pr_review": true,
                    "notify_on_session_error": true,
                    "notify_on_wire_urgent": true,
                    "sound_enabled": false,
                },
                "hub_connection": { "auto_reconnect_ws": true, "poll_interval_secs": 10 },
                "attachment_retention_days": 7,
            })
        );
    }

    #[test]
    fn attachment_retention_defaults_to_seven_when_the_file_lacks_it() {
        let tmp = TempDir::new().unwrap();
        let path = settings_path(tmp.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // A file written before the field existed.
        let doc = json!({
            "theme_preference": "light",
            "hub_connection": { "auto_reconnect_ws": true, "poll_interval_secs": 10 },
        });
        std::fs::write(&path, doc.to_string()).unwrap();
        let loaded = load(tmp.path());
        assert_eq!(loaded.attachment_retention_days, 7);
        assert_eq!(loaded.theme_preference, Theme::Light);
        // Stored values outside the contract fall back to the default too.
        for (stored, want) in [
            (json!(0), 0),
            (json!(30), 30),
            (json!(365), 365),
            (json!(366), 7),
            (json!(-1), 7),
            (json!("30"), 7),
            (json!(2.5), 7),
        ] {
            let doc = json!({ "attachment_retention_days": stored });
            std::fs::write(&path, doc.to_string()).unwrap();
            assert_eq!(load(tmp.path()).attachment_retention_days, want, "{stored}");
        }
    }

    #[test]
    fn attachment_retention_patch_sets_accepts_zero_and_refuses_above_365() {
        let tmp = TempDir::new().unwrap();
        let patch = |v: Value| serde_json::from_value::<SettingsPatch>(v).unwrap();
        let out = update(
            tmp.path(),
            patch(json!({ "attachment_retention_days": 30 })),
        )
        .unwrap();
        assert_eq!(out.attachment_retention_days, 30);
        assert_eq!(load(tmp.path()).attachment_retention_days, 30);

        let res = update(
            tmp.path(),
            patch(json!({ "attachment_retention_days": 366 })),
        );
        let Err(UpdateError::Invalid(msg)) = res else {
            panic!("366 must be refused");
        };
        assert!(msg.contains("attachment_retention_days"), "{msg}");
        assert_eq!(load(tmp.path()).attachment_retention_days, 30);

        for ok in [0, 365] {
            let out = update(
                tmp.path(),
                patch(json!({ "attachment_retention_days": ok })),
            )
            .unwrap();
            assert_eq!(out.attachment_retention_days, ok);
            assert_eq!(load(tmp.path()).attachment_retention_days, ok);
        }
        for bad in [json!(-1), json!("7"), json!(1.5)] {
            assert!(
                serde_json::from_value::<SettingsPatch>(
                    json!({ "attachment_retention_days": bad })
                )
                .is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn timezone_validates_patches_and_falls_back_on_stored_load() {
        let tmp = TempDir::new().unwrap();
        let patch = |v: Value| serde_json::from_value::<SettingsPatch>(v).unwrap();

        for (input, expected) in [
            ("UTC", "UTC"),
            ("utc", "UTC"),
            ("local", "local"),
            ("America/New_York", "America/New_York"),
            (
                "America/Argentina/Buenos_Aires",
                "America/Argentina/Buenos_Aires",
            ),
            ("Etc/GMT+5", "Etc/GMT+5"),
            ("Etc/GMT-9", "Etc/GMT-9"),
            ("Asia/Tokyo", "Asia/Tokyo"),
        ] {
            let out = update(tmp.path(), patch(json!({ "timezone": input }))).unwrap();
            assert_eq!(out.timezone, expected);
            assert_eq!(load(tmp.path()).timezone, expected);
        }

        let before = load(tmp.path());
        for bad in [
            "",
            "   ",
            "../etc/passwd",
            "/America/New_York",
            "America/New_York/",
            "America//New_York",
            "America/New York",
            "America/New_York;rm -rf",
            &"A".repeat(65),
        ] {
            let res = update(tmp.path(), patch(json!({ "timezone": bad })));
            assert!(matches!(res, Err(UpdateError::Invalid(_))), "bad={bad:?}");
        }
        assert_eq!(load(tmp.path()), before);

        let path = settings_path(tmp.path());
        for bad_stored in [
            json!(""),
            json!("../escape"),
            json!("/leading/slash"),
            json!(42),
            json!(null),
        ] {
            let doc = json!({ "timezone": bad_stored });
            std::fs::write(&path, doc.to_string()).unwrap();
            assert_eq!(load(tmp.path()).timezone, "UTC", "stored={bad_stored}");
        }
    }

    #[test]
    fn partial_and_corrupt_files_fall_back_field_by_field() {
        let tmp = TempDir::new().unwrap();
        let path = settings_path(tmp.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();

        std::fs::write(&path, b"{ not json").unwrap();
        assert_eq!(load(tmp.path()), Settings::default());
        std::fs::write(&path, b"[1,2]").unwrap();
        assert_eq!(load(tmp.path()), Settings::default());

        let doc = json!({
            "theme_preference": "light",
            "subsystems": { "ferrostep_enabled": false, "ferrogate_enabled": "yes" },
            "notifications": 7,
            "hub_connection": { "poll_interval_secs": 9999 },
        });
        std::fs::write(&path, doc.to_string()).unwrap();
        let loaded = load(tmp.path());
        assert_eq!(loaded.theme_preference, Theme::Light);
        assert!(!loaded.subsystems.ferrostep_enabled);
        assert!(loaded.subsystems.ferrogate_enabled);
        assert_eq!(loaded.notifications, Settings::default().notifications);
        assert_eq!(loaded.hub_connection.poll_interval_secs, 10);
    }

    #[test]
    fn update_merges_persists_and_rejects_bad_intervals() {
        let tmp = TempDir::new().unwrap();
        let patch = |v: Value| serde_json::from_value::<SettingsPatch>(v).unwrap();
        let out = update(
            tmp.path(),
            patch(json!({ "notifications": { "sound_enabled": true } })),
        )
        .ok()
        .unwrap();
        assert!(out.notifications.sound_enabled);
        assert!(out.notifications.notify_on_pr_review);
        assert_eq!(load(tmp.path()), out);
        for bad in [1, 301] {
            let res = update(
                tmp.path(),
                patch(json!({ "hub_connection": { "poll_interval_secs": bad } })),
            );
            assert!(matches!(res, Err(UpdateError::Invalid(_))), "{bad}");
        }
        assert_eq!(load(tmp.path()), out);
        for ok in [2, 300] {
            assert!(
                update(
                    tmp.path(),
                    patch(json!({ "hub_connection": { "poll_interval_secs": ok } })),
                )
                .is_ok()
            );
        }
    }

    #[test]
    fn patch_rejects_unknown_fields_and_bad_theme() {
        for bad in [
            json!({ "colour": "red" }),
            json!({ "subsystems": { "ferrobogus_enabled": true } }),
            json!({ "theme_preference": "purple" }),
            json!({ "notifications": { "sound_enabled": "loud" } }),
        ] {
            assert!(
                serde_json::from_value::<SettingsPatch>(bad.clone()).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn concurrent_patches_to_different_fields_all_survive() {
        let tmp = TempDir::new().unwrap();
        let patch = |v: Value| serde_json::from_value::<SettingsPatch>(v).unwrap();
        let patches = [
            json!({ "theme_preference": "light" }),
            json!({ "subsystems": { "ferrogate_enabled": false } }),
            json!({ "notifications": { "sound_enabled": true } }),
            json!({ "hub_connection": { "poll_interval_secs": 42 } }),
            json!({ "hub_connection": { "auto_reconnect_ws": false } }),
        ];
        std::thread::scope(|scope| {
            for body in &patches {
                let home = tmp.path();
                scope.spawn(move || {
                    assert!(update(home, patch(body.clone())).is_ok());
                });
            }
        });
        let now = load(tmp.path());
        assert_eq!(now.theme_preference, Theme::Light);
        assert!(!now.subsystems.ferrogate_enabled);
        assert!(now.notifications.sound_enabled);
        assert_eq!(now.hub_connection.poll_interval_secs, 42);
        assert!(!now.hub_connection.auto_reconnect_ws);
    }

    #[test]
    fn a_rejected_patch_changes_nothing_even_for_its_valid_fields() {
        let tmp = TempDir::new().unwrap();
        let patch = |v: Value| serde_json::from_value::<SettingsPatch>(v).unwrap();
        let res = update(
            tmp.path(),
            patch(json!({
                "theme_preference": "light",
                "hub_connection": { "poll_interval_secs": 1 },
            })),
        );
        assert!(matches!(res, Err(UpdateError::Invalid(_))));
        assert_eq!(load(tmp.path()), Settings::default());
        assert!(
            !settings_path(tmp.path()).exists(),
            "no file for a refused patch"
        );
    }

    #[test]
    fn stored_values_outside_the_contract_fall_back_per_field() {
        let tmp = TempDir::new().unwrap();
        let path = settings_path(tmp.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        for (stored, want) in [
            (json!(1.5), 10),
            (json!(-3), 10),
            (json!(0), 10),
            (json!(2), 2),
            (json!(300), 300),
            (json!("30"), 10),
        ] {
            let doc = json!({ "hub_connection": { "poll_interval_secs": stored } });
            std::fs::write(&path, doc.to_string()).unwrap();
            assert_eq!(
                load(tmp.path()).hub_connection.poll_interval_secs,
                want,
                "{stored}"
            );
        }
        // Null or wrong-typed groups do not poison the others.
        let doc = json!({ "theme_preference": null, "subsystems": [], "notifications": { "sound_enabled": true } });
        std::fs::write(&path, doc.to_string()).unwrap();
        let loaded = load(tmp.path());
        assert_eq!(loaded.theme_preference, Theme::Dark);
        assert!(loaded.notifications.sound_enabled);
    }
}
