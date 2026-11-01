//! Expiry of `FerroDeck` chat attachments (the files `POST /api/v1/sessions/:id/prompt` saves in
//! the hub's attachments directory).
//!
//! The hub deletes an attachment older than `attachment_retention_days` (see
//! [`crate::settings::Settings::attachment_retention_days`]) after each upload, once when the hub
//! starts and then every hour. `POST /api/v1/attachments/prune` lets the Owner run a sweep with
//! any age, or delete them all. Expiry does not check whether a chat message still refers to a
//! file: a deleted attachment breaks its image link in old chat transcripts.
//!
//! Only regular files directly inside the directory whose names have the upload shape
//! `<epoch_ms>-<n>-<name>` are candidates. Any other file (a `README`, `notes.md`), symlinks,
//! directories and anything else are never touched, and nothing outside the directory is ever
//! deleted. The directory is `attachments/` beside the store file, made absolute, so uploads,
//! serving and sweeps all use one directory whatever the process's working directory.
//!
//! If the attachments directory itself is a symlink, the sweep follows it (as upload and serving
//! do) and expires the upload-shaped files in the directory it points to.
//!
//! The saved retention is read strictly: a damaged settings file, an invalid stored value or an
//! unknown home directory skips the automatic sweep and refuses a manual prune that relies on the
//! setting, instead of falling back to the 7-day default.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::HubState;
use crate::api::{HttpRequest, HttpResponse, hub_home_dir};
use crate::auth::HubAuthority;
use crate::settings;

/// How often the hub's serve loop runs an automatic sweep. Its first tick fires at start.
pub(crate) const ATTACHMENT_SWEEP_INTERVAL: Duration = Duration::from_secs(60 * 60);

const SECS_PER_DAY: u64 = 24 * 60 * 60;

/// What one sweep did. `kept_count` includes files whose delete failed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub(crate) struct PrunedAttachmentsReport {
    pub deleted_count: u64,
    pub deleted_bytes: u64,
    pub kept_count: u64,
}

/// The hub's attachments directory: `attachments/` beside the store file, as an absolute path.
///
/// A relative store path is resolved against the process's working directory, the same base
/// the store itself is opened against (`fl hub serve` passes an absolute path already).
pub(crate) fn attachments_dir(state: &HubState) -> PathBuf {
    let store = &state.config.store_path;
    let store = std::path::absolute(store).unwrap_or_else(|_| store.clone());
    store
        .parent()
        .unwrap_or_else(|| Path::new("/tmp"))
        .join("attachments")
}

/// Whether `name` has the shape the hub gives an upload: `<epoch_ms>-<n>-<name>`, both numbers
/// plain decimal integers and the name non-empty. Only such files may be expired.
fn is_upload_name(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let mut parts = name.splitn(3, '-');
    let (Some(ms), Some(idx), Some(rest)) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    let integer = |s: &str| {
        !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) && s.parse::<u64>().is_ok()
    };
    integer(ms) && integer(idx) && !rest.is_empty()
}

/// Delete the expired attachments in `dir`.
///
/// - `max_age = None`: delete nothing; the report counts what is there.
/// - `Some(Duration::ZERO)`: delete every eligible file.
/// - `Some(age)`: delete the eligible files older than `age`.
///
/// Eligible means a regular file directly inside `dir` (no recursion, symlinks never followed)
/// whose name has the upload shape (see [`is_upload_name`]); other regular files are counted as
/// kept, even for `Some(Duration::ZERO)`. `dir` itself is followed if it is a symlink.
/// A file's age comes from its mtime; when the mtime is unavailable or in the future, from the
/// `<epoch_ms>-` prefix of its name; when neither is usable, the file is kept. A file that cannot
/// be deleted is counted as kept and the sweep goes on. A missing `dir` gives an empty report.
pub(crate) fn prune_expired_attachments(
    dir: &Path,
    max_age: Option<Duration>,
) -> io::Result<PrunedAttachmentsReport> {
    let mut report = PrunedAttachmentsReport::default();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(report),
        Err(e) => return Err(e),
    };
    let now = SystemTime::now();
    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let path = entry.path();
        // Never follow a symlink: a link is not a regular file and is skipped.
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.file_type().is_file() {
            continue;
        }
        if !is_upload_name(&entry.file_name()) {
            // Not a file the hub uploaded: never deleted.
            report.kept_count += 1;
            continue;
        }
        let expired = match max_age {
            None => false,
            Some(age) if age.is_zero() => true,
            Some(age) => file_age(&meta, &entry.file_name(), now).is_some_and(|a| a > age),
        };
        if !expired {
            report.kept_count += 1;
            continue;
        }
        match fs::remove_file(&path) {
            Ok(()) => {
                report.deleted_count += 1;
                report.deleted_bytes += meta.len();
            }
            Err(err) => {
                eprintln!(
                    "fl-hub: could not delete expired attachment {}: {err}",
                    path.display()
                );
                report.kept_count += 1;
            }
        }
    }
    Ok(report)
}

/// `now - mtime`, else `now - <epoch_ms>` from the name, else `None` (unknown: keep).
fn file_age(meta: &fs::Metadata, name: &std::ffi::OsStr, now: SystemTime) -> Option<Duration> {
    if let Some(age) = meta
        .modified()
        .ok()
        .and_then(|m| now.duration_since(m).ok())
    {
        return Some(age);
    }
    let stamped = UNIX_EPOCH.checked_add(Duration::from_millis(name_epoch_ms(name)?))?;
    now.duration_since(stamped).ok()
}

/// The `<epoch_ms>` of a name shaped `<epoch_ms>-…`.
fn name_epoch_ms(name: &std::ffi::OsStr) -> Option<u64> {
    let (prefix, _) = name.to_str()?.split_once('-')?;
    if prefix.is_empty() || !prefix.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    prefix.parse().ok()
}

fn days(n: u64) -> Duration {
    Duration::from_secs(n.saturating_mul(SECS_PER_DAY))
}

/// Holds the "sweep running" flag; clears it when dropped.
pub(crate) struct SweepGuard(Arc<AtomicBool>);

impl SweepGuard {
    /// Take the flag, or `None` when another sweep holds it.
    pub(crate) fn try_acquire(flag: &Arc<AtomicBool>) -> Option<Self> {
        flag.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| Self(Arc::clone(flag)))
    }
}

impl Drop for SweepGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// The saved retention in days, read strictly (see
/// [`settings::load_attachment_retention_strict`]). `Err` (with the reason) when the home
/// directory is unknown or the setting cannot be read: never delete on `Err`.
fn retention_days(state: &HubState) -> Result<u64, String> {
    let home = hub_home_dir(state)
        .ok_or_else(|| "the home directory is unknown (HOME is not set)".to_string())?;
    settings::load_attachment_retention_strict(&home)
}

/// The retention an automatic sweep should use: `None` when retention is `0` or the setting
/// cannot be read (then it says why, once for this sweep).
fn automatic_sweep_retention(state: &HubState) -> Option<u64> {
    match retention_days(state) {
        Ok(0) => None,
        Ok(days) => Some(days),
        Err(reason) => {
            eprintln!(
                "fl-hub: attachment sweep skipped: retention setting could not be read: {reason}"
            );
            None
        }
    }
}

fn sweep_with_guard(
    state: &HubState,
    retention: u64,
    guard: SweepGuard,
) -> PrunedAttachmentsReport {
    let dir = attachments_dir(state);
    let result = prune_expired_attachments(&dir, Some(days(retention)));
    drop(guard);
    match result {
        Ok(report) => {
            if report.deleted_count > 0 {
                eprintln!(
                    "fl-hub: attachment sweep deleted {} file(s), {} bytes, older than {retention} day(s)",
                    report.deleted_count, report.deleted_bytes
                );
            }
            report
        }
        Err(err) => {
            eprintln!(
                "fl-hub: attachment sweep of {} failed: {err}",
                dir.display()
            );
            PrunedAttachmentsReport::default()
        }
    }
}

/// Run an automatic sweep on this thread with the saved retention. `None` when retention is `0`,
/// the setting cannot be read, or another sweep is running.
pub(crate) fn run_attachment_sweep(state: &HubState) -> Option<PrunedAttachmentsReport> {
    let retention = automatic_sweep_retention(state)?;
    let guard = SweepGuard::try_acquire(&state.attachment_sweep_running)?;
    Some(sweep_with_guard(state, retention, guard))
}

/// Start an automatic sweep on a new thread and return at once. `None` (no thread) when
/// retention is `0`, the setting cannot be read, or another sweep is running.
pub(crate) fn spawn_attachment_sweep(
    state: &HubState,
) -> Option<std::thread::JoinHandle<PrunedAttachmentsReport>> {
    let retention = automatic_sweep_retention(state)?;
    let guard = SweepGuard::try_acquire(&state.attachment_sweep_running)?;
    let state = state.clone();
    std::thread::Builder::new()
        .name("fl-hub-attachment-sweep".into())
        .spawn(move || sweep_with_guard(&state, retention, guard))
        .map_err(|err| eprintln!("fl-hub: could not start the attachment sweep: {err}"))
        .ok()
}

/// How long a manual prune waits for a running automatic sweep to finish.
const MANUAL_PRUNE_WAIT: Duration = Duration::from_secs(5);

/// `POST /api/v1/attachments/prune`: the Owner deletes attachments now.
///
/// Body (optional): `{ "older_than_days": u64 }`, at most 365; unknown fields are refused.
/// Omitted, the saved `attachment_retention_days` applies, and when that is `0` nothing is
/// deleted; when the setting cannot be read (damaged file, invalid value, unknown home) the
/// prune is refused with `409` and deletes nothing. `0` deletes every upload-shaped attachment,
/// however young: that includes one an in-flight upload has just written and one a queued
/// (`send_now: false`) prompt still refers to by path. Deleted attachments break their image
/// links in old chat transcripts. Waits up to 5 s for a running sweep, then `409`. Returns
/// `{ "deleted_count", "deleted_bytes", "kept_count", "older_than_days", "retention_disabled" }`
/// (flat) and broadcasts `attachments.pruned` on the `attachments` channel.
pub(crate) fn handle_post_attachments_prune(
    state: &HubState,
    authority: &HubAuthority,
    req: &HttpRequest,
) -> HttpResponse {
    prune_attachments_waiting(state, authority, req, MANUAL_PRUNE_WAIT)
}

/// [`handle_post_attachments_prune`], waiting at most `wait` for a running sweep.
pub(crate) fn prune_attachments_waiting(
    state: &HubState,
    authority: &HubAuthority,
    req: &HttpRequest,
    wait: Duration,
) -> HttpResponse {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct PruneBody {
        #[serde(default)]
        older_than_days: Option<u64>,
    }

    if !authority.is_owner() {
        return HttpResponse::json(
            403,
            json!({ "error": "only Owner authority may prune attachments" }),
        );
    }
    let body: PruneBody = if req.body.iter().all(u8::is_ascii_whitespace) {
        PruneBody {
            older_than_days: None,
        }
    } else {
        let parsed = serde_json::from_slice::<serde_json::Value>(&req.body)
            .map_err(|e| e.to_string())
            .and_then(|value| {
                if value.is_object() {
                    serde_json::from_value(value).map_err(|e| e.to_string())
                } else {
                    Err("JSON body must be an object".to_string())
                }
            });
        match parsed {
            Ok(body) => body,
            Err(e) => {
                return HttpResponse::json(
                    400,
                    json!({ "error": format!("invalid attachment prune body: {e}") }),
                );
            }
        }
    };
    if let Some(n) = body.older_than_days
        && n > settings::MAX_ATTACHMENT_RETENTION_DAYS
    {
        return HttpResponse::json(
            400,
            json!({
                "error": format!(
                    "older_than_days must be between 0 and {}",
                    settings::MAX_ATTACHMENT_RETENTION_DAYS
                )
            }),
        );
    }
    let (older_than_days, max_age) = match body.older_than_days {
        Some(n) => (Some(n), Some(days(n))),
        None => match retention_days(state) {
            Ok(0) => (None, None),
            Ok(n) => (Some(n), Some(days(n))),
            Err(reason) => {
                return HttpResponse::json(
                    409,
                    json!({
                        "error": format!("retention setting could not be read: {reason}")
                    }),
                );
            }
        },
    };

    // One sweep at a time: wait a little for an automatic one to finish.
    let deadline = std::time::Instant::now() + wait;
    let guard = loop {
        if let Some(guard) = SweepGuard::try_acquire(&state.attachment_sweep_running) {
            break guard;
        }
        if std::time::Instant::now() >= deadline {
            return HttpResponse::json(
                409,
                json!({ "error": "an attachment sweep is already running; try again" }),
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let result = prune_expired_attachments(&attachments_dir(state), max_age);
    drop(guard);
    let report = match result {
        Ok(report) => report,
        Err(e) => {
            return HttpResponse::json(
                500,
                json!({ "error": format!("could not prune attachments: {e}") }),
            );
        }
    };
    let data = json!({
        "deleted_count": report.deleted_count,
        "deleted_bytes": report.deleted_bytes,
        "kept_count": report.kept_count,
        "older_than_days": older_than_days,
        "retention_disabled": older_than_days.is_none(),
    });
    state.broadcast("attachments", "attachments.pruned", data.clone());
    HttpResponse::json(200, data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{File, FileTimes};
    use tempfile::TempDir;

    const DAY: Duration = Duration::from_secs(SECS_PER_DAY);

    fn write_aged(dir: &Path, name: &str, bytes: &[u8], age: Duration) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, bytes).unwrap();
        set_mtime(&path, SystemTime::now() - age);
        path
    }

    fn set_mtime(path: &Path, when: SystemTime) {
        // Read-only open: works for directories too, and the owner may set times on any fd.
        let f = File::open(path).unwrap();
        f.set_times(FileTimes::new().set_modified(when).set_accessed(when))
            .unwrap();
    }

    /// An upload-shaped name, as `POST /api/v1/sessions/:id/prompt` saves it.
    fn up(name: &str) -> String {
        format!("1700000000000-0-{name}")
    }

    fn epoch_ms(t: SystemTime) -> u128 {
        t.duration_since(UNIX_EPOCH).unwrap().as_millis()
    }

    #[test]
    fn deletes_old_files_and_keeps_young_ones_with_exact_counts() {
        let tmp = TempDir::new().unwrap();
        let old = write_aged(tmp.path(), &up("old.png"), b"12345", 10 * DAY);
        let old2 = write_aged(tmp.path(), &up("old2.png"), b"abc", 8 * DAY);
        let young = write_aged(tmp.path(), &up("young.png"), b"xy", DAY);
        let report = prune_expired_attachments(tmp.path(), Some(7 * DAY)).unwrap();
        assert_eq!(
            report,
            PrunedAttachmentsReport {
                deleted_count: 2,
                deleted_bytes: 8,
                kept_count: 1,
            }
        );
        assert!(!old.exists());
        assert!(!old2.exists());
        assert!(young.exists());
    }

    #[test]
    fn zero_deletes_every_regular_file_and_none_deletes_nothing() {
        let tmp = TempDir::new().unwrap();
        write_aged(tmp.path(), &up("a.png"), b"1", 10 * DAY);
        write_aged(tmp.path(), &up("b.png"), b"22", Duration::ZERO);
        write_aged(tmp.path(), &up("c.png"), b"333", DAY);

        let report = prune_expired_attachments(tmp.path(), None).unwrap();
        assert_eq!(
            report,
            PrunedAttachmentsReport {
                deleted_count: 0,
                deleted_bytes: 0,
                kept_count: 3,
            }
        );
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 3);

        let report = prune_expired_attachments(tmp.path(), Some(Duration::ZERO)).unwrap();
        assert_eq!(
            report,
            PrunedAttachmentsReport {
                deleted_count: 3,
                deleted_bytes: 6,
                kept_count: 0,
            }
        );
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 0);
    }

    #[test]
    fn symlinks_and_subdirectories_are_never_touched() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("attachments");
        fs::create_dir_all(&dir).unwrap();
        let outside = tmp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        let outside_target = write_aged(&outside, "secret.txt", b"keep me", 30 * DAY);
        let inside_target = write_aged(&dir, &up("target.png"), b"inside", 30 * DAY);
        let link_in = dir.join("link-to-inside.png");
        let link_out = dir.join("link-to-outside.png");
        std::os::unix::fs::symlink(&inside_target, &link_in).unwrap();
        std::os::unix::fs::symlink(&outside_target, &link_out).unwrap();
        let sub = dir.join("sub");
        fs::create_dir_all(&sub).unwrap();
        let nested = write_aged(&sub, &up("nested.png"), b"nested", 30 * DAY);
        set_mtime(&sub, SystemTime::now() - 30 * DAY);

        let report = prune_expired_attachments(&dir, Some(Duration::ZERO)).unwrap();
        // Only the regular file directly inside is a candidate.
        assert_eq!(
            report,
            PrunedAttachmentsReport {
                deleted_count: 1,
                deleted_bytes: 6,
                kept_count: 0,
            }
        );
        assert!(!inside_target.exists());
        assert!(
            fs::symlink_metadata(&link_in).is_ok(),
            "link kept (dangling)"
        );
        assert!(fs::symlink_metadata(&link_out).is_ok(), "link kept");
        assert_eq!(fs::read(&outside_target).unwrap(), b"keep me");
        assert!(sub.is_dir());
        assert_eq!(fs::read(&nested).unwrap(), b"nested");

        // An age-based sweep leaves them alone too.
        let report = prune_expired_attachments(&dir, Some(DAY)).unwrap();
        assert_eq!(report, PrunedAttachmentsReport::default());
        assert_eq!(fs::read(&outside_target).unwrap(), b"keep me");
    }

    #[test]
    fn a_future_mtime_falls_back_to_the_name_prefix_and_an_unusable_name_is_kept() {
        let tmp = TempDir::new().unwrap();
        let future = SystemTime::now() + 365 * DAY;
        let old_ms = epoch_ms(SystemTime::now() - 10 * DAY);
        let young_ms = epoch_ms(SystemTime::now() - DAY);
        let future_ms = epoch_ms(future);
        let named_old = tmp.path().join(format!("{old_ms}-0-shot.png"));
        let named_young = tmp.path().join(format!("{young_ms}-0-shot.png"));
        let named_future = tmp.path().join(format!("{future_ms}-0-shot.png"));
        let unparsable = tmp.path().join("screenshot-1.png");
        let no_dash = tmp.path().join("1234567");
        for p in [
            &named_old,
            &named_young,
            &named_future,
            &unparsable,
            &no_dash,
        ] {
            fs::write(p, b"data").unwrap();
            set_mtime(p, future);
        }

        let report = prune_expired_attachments(tmp.path(), Some(7 * DAY)).unwrap();
        assert_eq!(
            report,
            PrunedAttachmentsReport {
                deleted_count: 1,
                deleted_bytes: 4,
                kept_count: 4,
            }
        );
        assert!(!named_old.exists());
        assert!(named_young.exists());
        assert!(named_future.exists());
        assert!(unparsable.exists());
        assert!(no_dash.exists());
    }

    #[test]
    fn a_missing_directory_is_an_empty_report() {
        let tmp = TempDir::new().unwrap();
        let report =
            prune_expired_attachments(&tmp.path().join("nope"), Some(Duration::ZERO)).unwrap();
        assert_eq!(report, PrunedAttachmentsReport::default());
    }

    #[test]
    fn name_prefix_parses_only_a_leading_run_of_digits() {
        use std::ffi::OsStr;
        assert_eq!(
            name_epoch_ms(OsStr::new("1730000000000-0-a.png")),
            Some(1_730_000_000_000)
        );
        assert_eq!(name_epoch_ms(OsStr::new("-0-a.png")), None);
        assert_eq!(name_epoch_ms(OsStr::new("12a-0-a.png")), None);
        assert_eq!(name_epoch_ms(OsStr::new("a.png")), None);
    }

    #[test]
    fn only_upload_shaped_names_are_eligible() {
        use std::ffi::OsStr;
        for ok in ["1730000000000-0-a.png", "1-2-x", "0-0-a-b-c", "1-0-.hidden"] {
            assert!(is_upload_name(OsStr::new(ok)), "{ok}");
        }
        for bad in [
            "README",
            "notes.md",
            "screenshot-1.png",
            "1234567",
            "1-0-",
            "1-0",
            "-0-a.png",
            "1--a.png",
            "1-x-a.png",
            "12a-0-a.png",
            "+1-0-a.png",
            "99999999999999999999999-0-a.png",
        ] {
            assert!(!is_upload_name(OsStr::new(bad)), "{bad}");
        }
    }

    #[test]
    fn non_upload_files_are_never_deleted_not_even_by_a_purge() {
        let tmp = TempDir::new().unwrap();
        let notes = write_aged(tmp.path(), "notes.md", b"my notes", 400 * DAY);
        let readme = write_aged(tmp.path(), "README", b"readme", 400 * DAY);
        let dated = write_aged(tmp.path(), "2024-report.pdf", b"pdf", 400 * DAY);
        let upload = write_aged(tmp.path(), &up("shot.png"), b"png", 400 * DAY);

        let report = prune_expired_attachments(tmp.path(), Some(7 * DAY)).unwrap();
        assert_eq!(
            report,
            PrunedAttachmentsReport {
                deleted_count: 1,
                deleted_bytes: 3,
                kept_count: 3,
            }
        );
        assert!(!upload.exists());

        let report = prune_expired_attachments(tmp.path(), Some(Duration::ZERO)).unwrap();
        assert_eq!(
            report,
            PrunedAttachmentsReport {
                deleted_count: 0,
                deleted_bytes: 0,
                kept_count: 3,
            }
        );
        assert_eq!(fs::read(&notes).unwrap(), b"my notes");
        assert_eq!(fs::read(&readme).unwrap(), b"readme");
        assert_eq!(fs::read(&dated).unwrap(), b"pdf");
    }

    #[test]
    fn the_sweep_guard_admits_one_holder_at_a_time() {
        let flag = Arc::new(AtomicBool::new(false));
        let first = SweepGuard::try_acquire(&flag).expect("free flag");
        assert!(SweepGuard::try_acquire(&flag).is_none());
        drop(first);
        assert!(!flag.load(Ordering::Acquire));
        assert!(SweepGuard::try_acquire(&flag).is_some());
    }
}
