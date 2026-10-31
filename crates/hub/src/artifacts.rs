//! Artifact registry (Record 29): which files the hub has shown as artifacts, plus archived
//! copies of the user-facing ones so they outlive the worktree that held them.
//!
//! Layout under `<home>/.local/share/fl/artifacts/`: `registry.json` and `managed/`. This module
//! holds the pure registry and file logic; path vetting (forbidden names, allowed roots) stays
//! with the API handlers, which call in here only with paths they already checked.

use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub(crate) const STATUS_LIVE: &str = "live";
pub(crate) const STATUS_ARCHIVED: &str = "archived";

/// Largest file the hub copies into the managed dir.
pub(crate) const MAX_ARCHIVE_BYTES: u64 = 5 * 1024 * 1024;

/// User-facing extensions worth keeping after the worktree is gone.
const ARCHIVE_EXTS: &[&str] = &[
    "md", "markdown", "mmd", "mermaid", "png", "jpg", "jpeg", "webp", "gif", "svg", "pdf",
];

const MAX_MANAGED_NAME_LEN: usize = 120;

/// A leftover `.tmp-*` file in `managed/` is swept once it is older than this, so a concurrent
/// archive that is still writing one is never disturbed.
const STALE_TEMP_AGE: Duration = Duration::from_secs(60 * 60);

/// One registry lock for the whole process: every read-modify-write goes through it.
static REGISTRY_LOCK: Mutex<()> = Mutex::new(());
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// `<home>/.local/share/fl/artifacts`.
pub(crate) fn data_root(home: &Path) -> PathBuf {
    home.join(".local/share/fl/artifacts")
}

pub(crate) fn managed_dir(root: &Path) -> PathBuf {
    root.join("managed")
}

fn registry_path(root: &Path) -> PathBuf {
    root.join("registry.json")
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ArtifactEntry {
    pub path: String,
    pub canonical_path: String,
    pub managed_path: Option<String>,
    pub name: String,
    pub ext: String,
    pub kind: String,
    pub size_bytes: u64,
    pub session_id: Option<String>,
    /// `live` while the original exists, `archived` once only the managed copy does.
    pub status: String,
    pub updated_at_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Registry {
    pub entries: Vec<ArtifactEntry>,
}

/// What `Registry::reconcile` found.
#[derive(Debug, Default)]
pub(crate) struct Reconciled {
    /// Entries whose original is gone but whose managed copy remains (status `archived`).
    pub archived: Vec<ArtifactEntry>,
    /// Entries still backed by their original file.
    pub live: usize,
    /// Entries dropped because neither the original nor a managed copy exists.
    pub pruned: usize,
}

impl Registry {
    /// A missing or unreadable registry is empty; one malformed entry is skipped, not fatal.
    /// Production code goes through [`with_registry`], which also keeps a damaged file.
    #[cfg(test)]
    pub(crate) fn load(root: &Path) -> Self {
        Self::load_checked(root).0
    }

    /// [`Registry::load`], plus whether the file on disk held something that was dropped: text
    /// that is not JSON, or an entry that does not decode. A save would then overwrite it.
    fn load_checked(root: &Path) -> (Self, bool) {
        let Ok(bytes) = fs::read(registry_path(root)) else {
            return (Self::default(), false);
        };
        let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
            return (Self::default(), true);
        };
        let Some(items) = value.get("entries").and_then(Value::as_array) else {
            return (Self::default(), !bytes.is_empty());
        };
        let entries: Vec<ArtifactEntry> = items
            .iter()
            .filter_map(|item| serde_json::from_value(item.clone()).ok())
            .collect();
        let dropped = entries.len() != items.len();
        (Self { entries }, dropped)
    }

    /// Temp file plus rename, so a reader never sees a half-written registry.
    pub(crate) fn save(&self, root: &Path) -> io::Result<()> {
        fs::create_dir_all(root)?;
        let bytes = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        write_atomic(&registry_path(root), &bytes)
    }

    /// Insert or refresh the entry for `entry.canonical_path`. The archived copy stays, and so
    /// does an existing session: the first owner keeps the entry, a later caller cannot retag it.
    /// An entry with no session takes the new entry's session.
    pub(crate) fn upsert(&mut self, mut entry: ArtifactEntry) {
        match self
            .entries
            .iter_mut()
            .find(|e| e.canonical_path == entry.canonical_path)
        {
            Some(existing) => {
                entry.managed_path = existing.managed_path.take();
                if existing.session_id.is_some() {
                    entry.session_id = existing.session_id.take();
                }
                *existing = entry;
            }
            None => self.entries.push(entry),
        }
    }

    /// Re-check every tracked entry (only those of `session_ids` when given) against the disk:
    /// original present means live, only the managed copy means archived, neither means dropped.
    pub(crate) fn reconcile(
        &mut self,
        managed_dir: &Path,
        session_ids: Option<&[&str]>,
    ) -> Reconciled {
        let mut out = Reconciled::default();
        let now = now_ms();
        let mut kept = Vec::with_capacity(self.entries.len());
        for mut entry in std::mem::take(&mut self.entries) {
            let tracked = session_ids.is_none_or(|ids| {
                entry
                    .session_id
                    .as_deref()
                    .is_some_and(|sid| ids.contains(&sid))
            });
            if !tracked {
                kept.push(entry);
                continue;
            }
            if Path::new(&entry.canonical_path).exists() {
                if entry.status != STATUS_LIVE {
                    entry.status = STATUS_LIVE.to_string();
                    entry.updated_at_ms = now;
                }
                out.live += 1;
            } else if entry
                .managed_path
                .as_deref()
                .and_then(|m| valid_managed_path(managed_dir, m))
                .is_some()
            {
                if entry.status != STATUS_ARCHIVED {
                    entry.status = STATUS_ARCHIVED.to_string();
                    entry.updated_at_ms = now;
                }
                out.archived.push(entry.clone());
            } else {
                out.pruned += 1;
                continue;
            }
            kept.push(entry);
        }
        self.entries = kept;
        out
    }
}

/// Run `f` on the registry under the process-wide lock, and save it when `f` changed it. A
/// failed save is logged and otherwise ignored: the registry is a cache of what is on disk.
pub(crate) fn with_registry<R>(root: &Path, f: impl FnOnce(&mut Registry) -> R) -> R {
    let _guard = REGISTRY_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (mut registry, damaged) = Registry::load_checked(root);
    let before = registry.clone();
    let out = f(&mut registry);
    if registry != before {
        // The save replaces the file, and with it whatever part of it could not be read (a
        // corrupt file would otherwise wipe every entry, and strand their managed copies).
        // Keep that file once, beside the registry, for repair by hand.
        if damaged && let Err(err) = fs::copy(registry_path(root), corrupt_backup_path(root)) {
            eprintln!("fl-hub: could not back up the damaged artifact registry: {err}");
        }
        if let Err(err) = registry.save(root) {
            eprintln!("fl-hub: artifact registry save failed: {err}");
        }
    }
    out
}

/// Where a damaged `registry.json` is kept when the next save would overwrite it.
pub(crate) fn corrupt_backup_path(root: &Path) -> PathBuf {
    root.join("registry.json.corrupt")
}

/// The managed copy at `managed_path`, when it is an existing file inside `managed_dir`. The
/// registry file is only a hint, so a path it names counts only after this check.
pub(crate) fn valid_managed_path(managed_dir: &Path, managed_path: &str) -> Option<PathBuf> {
    let canon = fs::canonicalize(managed_path).ok()?;
    let dir = fs::canonicalize(managed_dir).ok()?;
    (canon.starts_with(&dir) && canon.is_file()).then_some(canon)
}

/// Whether the hub keeps an archived copy of this file.
pub(crate) fn is_archivable(path: &Path, size_bytes: u64) -> bool {
    size_bytes <= MAX_ARCHIVE_BYTES
        && path
            .extension()
            .and_then(|s| s.to_str())
            .is_some_and(|ext| ARCHIVE_EXTS.contains(&ext.to_ascii_lowercase().as_str()))
}

/// A file name safe to join onto the managed dir: no separators, no leading dot, bounded length.
pub(crate) fn sanitize_file_name(raw: &str) -> String {
    let mut name: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if name.starts_with('.') {
        name.replace_range(..1, "_");
    }
    if name.len() > MAX_MANAGED_NAME_LEN {
        // The name is ASCII by now, so any byte index is a char boundary. Keep the extension.
        let ext_start = name.rfind('.').filter(|i| name.len() - i <= 16);
        let tail = ext_start.map_or("", |i| &name[i..]).to_string();
        name.truncate(MAX_MANAGED_NAME_LEN - tail.len());
        name.push_str(&tail);
    }
    if name.is_empty() {
        name.push_str("artifact");
    }
    name
}

/// `<hash of the canonical path>-<sanitised file name>`: two files with one name never collide.
pub(crate) fn managed_file_name(canonical: &Path) -> String {
    let digest = Sha256::digest(canonical.to_string_lossy().as_bytes());
    let prefix: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    let name = canonical
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("artifact");
    format!("{prefix}-{}", sanitize_file_name(name))
}

/// Open `canonical` for reading without following a symlink at its last component.
fn open_nofollow(canonical: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(canonical)
}

/// Where an open file really is now (Linux: the `/proc/self/fd` link). A parent directory that was
/// swapped for a symlink after vetting shows up here, which `O_NOFOLLOW` alone does not catch.
#[cfg(target_os = "linux")]
fn resolved_path_of(file: &fs::File) -> io::Result<PathBuf> {
    use std::os::fd::AsRawFd;
    fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

/// Copy at most `MAX_ARCHIVE_BYTES` from `src` to a new file at `tmp`. A longer source is an
/// error, reached after reading one byte past the cap, never the whole source.
fn copy_capped(src: impl Read, tmp: &Path) -> io::Result<()> {
    let mut out = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(tmp)?;
    let copied = io::copy(&mut src.take(MAX_ARCHIVE_BYTES + 1), &mut out)?;
    if copied > MAX_ARCHIVE_BYTES {
        return Err(io::Error::other("artifact exceeds the archive size cap"));
    }
    Ok(())
}

/// Remove `.tmp-*` files in `dir` older than `STALE_TEMP_AGE` (left by a crashed archive).
fn sweep_stale_temps(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let stale = entry.file_name().to_string_lossy().starts_with(".tmp-")
            && entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > STALE_TEMP_AGE);
        if stale {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// The managed copy of `canonical` when it is already current: same size as the original and not
/// older than it. Such a file need not be copied again.
pub(crate) fn current_managed_copy(root: &Path, canonical: &Path) -> Option<PathBuf> {
    let dest = managed_dir(root).join(managed_file_name(canonical));
    let src = fs::metadata(canonical).ok()?;
    let copy = fs::metadata(&dest).ok()?;
    (copy.is_file() && copy.len() == src.len() && copy.modified().ok()? >= src.modified().ok()?)
        .then_some(dest)
}

/// Copy `canonical` into the managed dir under root `root`; returns the copy's path.
///
/// The caller vetted `canonical` earlier, but a session can write its worktree, so the path may
/// have changed since. The source is opened without following a symlink, must be a regular file
/// within the size cap, and the path the open handle resolves to must pass `recheck` (the
/// caller's vetting, run again). The copy reads at most the cap plus one byte.
pub(crate) fn archive_file(
    root: &Path,
    canonical: &Path,
    recheck: &dyn Fn(&Path) -> bool,
) -> io::Result<PathBuf> {
    let dir = managed_dir(root);
    fs::create_dir_all(&dir)?;
    sweep_stale_temps(&dir);
    let src = open_nofollow(canonical)?;
    #[cfg(target_os = "linux")]
    if !recheck(&resolved_path_of(&src)?) {
        return Err(io::Error::other("artifact no longer passes vetting"));
    }
    #[cfg(not(target_os = "linux"))]
    let _ = recheck;
    let meta = src.metadata()?;
    if !meta.is_file() || meta.len() > MAX_ARCHIVE_BYTES {
        return Err(io::Error::other(
            "artifact is not a regular file within the archive size cap",
        ));
    }
    let dest = dir.join(managed_file_name(canonical));
    let tmp = temp_path_beside(&dest);
    let copied = copy_capped(src, &tmp).and_then(|()| fs::rename(&tmp, &dest));
    if copied.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    copied.map(|()| dest)
}

fn temp_path_beside(dest: &Path) -> PathBuf {
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = format!(".tmp-{}-{n}", std::process::id());
    dest.with_file_name(name)
}

fn write_atomic(dest: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = temp_path_beside(dest);
    let result = fs::write(&tmp, bytes).and_then(|()| fs::rename(&tmp, dest));
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Write `bytes` to `dest` atomically (temp file in the same dir, then rename).
pub(crate) fn write_file_atomic(dest: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    write_atomic(dest, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn entry(canonical: &Path, session: Option<&str>) -> ArtifactEntry {
        ArtifactEntry {
            path: canonical.display().to_string(),
            canonical_path: canonical.display().to_string(),
            managed_path: None,
            name: "a.md".into(),
            ext: "md".into(),
            kind: "markdown".into(),
            size_bytes: 1,
            session_id: session.map(str::to_string),
            status: STATUS_LIVE.into(),
            updated_at_ms: 1,
        }
    }

    #[test]
    fn missing_and_corrupt_registry_load_as_empty_and_bad_entries_are_skipped() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("artifacts");
        assert!(Registry::load(&root).entries.is_empty());
        fs::create_dir_all(&root).unwrap();
        fs::write(registry_path(&root), b"{ not json").unwrap();
        assert!(Registry::load(&root).entries.is_empty());
        let good = serde_json::to_value(entry(Path::new("/x/a.md"), None)).unwrap();
        let doc = serde_json::json!({ "entries": [good, { "path": 3 }, "junk"] });
        fs::write(registry_path(&root), doc.to_string()).unwrap();
        assert_eq!(Registry::load(&root).entries.len(), 1);
    }

    #[test]
    fn registry_round_trips_through_an_atomic_save() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("artifacts");
        let mut reg = Registry::default();
        reg.upsert(entry(Path::new("/x/a.md"), Some("s1")));
        reg.save(&root).unwrap();
        assert_eq!(Registry::load(&root), reg);
        let leftovers: Vec<_> = fs::read_dir(&root)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(leftovers, ["registry.json"]);
    }

    #[test]
    fn upsert_keeps_the_managed_copy_and_the_first_session_owner() {
        let mut reg = Registry::default();
        let mut first = entry(Path::new("/x/a.md"), Some("s1"));
        first.managed_path = Some("/m/copy".into());
        reg.upsert(first);
        reg.upsert(entry(Path::new("/x/a.md"), None));
        assert_eq!(reg.entries.len(), 1);
        assert_eq!(reg.entries[0].managed_path.as_deref(), Some("/m/copy"));
        assert_eq!(reg.entries[0].session_id.as_deref(), Some("s1"));
        // A later caller cannot retag an entry that already has an owner.
        reg.upsert(entry(Path::new("/x/a.md"), Some("s2")));
        assert_eq!(reg.entries[0].session_id.as_deref(), Some("s1"));
        // An untagged entry takes the first session that claims it.
        reg.upsert(entry(Path::new("/x/b.md"), None));
        reg.upsert(entry(Path::new("/x/b.md"), Some("s3")));
        assert_eq!(reg.entries[1].session_id.as_deref(), Some("s3"));
        reg.upsert(entry(Path::new("/x/b.md"), Some("s4")));
        assert_eq!(reg.entries[1].session_id.as_deref(), Some("s3"));
    }

    #[test]
    fn sanitised_names_cannot_traverse_or_collide() {
        for raw in ["../../etc/passwd", "..", ".hidden", "a/b\\c", "", "x y\0z"] {
            let clean = sanitize_file_name(raw);
            assert!(!clean.is_empty());
            assert!(!clean.contains('/') && !clean.contains('\\') && !clean.contains('\0'));
            assert!(!clean.starts_with('.'), "{clean}");
        }
        assert_eq!(sanitize_file_name("../../etc/passwd"), "_._.._etc_passwd");
        let long = format!("{}.png", "n".repeat(500));
        let clean = sanitize_file_name(&long);
        assert!(clean.len() <= MAX_MANAGED_NAME_LEN && clean.ends_with(".png"));

        let a = managed_file_name(Path::new("/w1/notes.md"));
        let b = managed_file_name(Path::new("/w2/notes.md"));
        assert_ne!(a, b);
        assert!(a.ends_with("-notes.md") && !a.contains('/'));
    }

    #[test]
    fn archivable_means_whitelisted_extension_within_the_size_cap() {
        assert!(is_archivable(Path::new("/x/a.MD"), 10));
        assert!(is_archivable(Path::new("/x/a.pdf"), MAX_ARCHIVE_BYTES));
        assert!(!is_archivable(Path::new("/x/a.pdf"), MAX_ARCHIVE_BYTES + 1));
        assert!(!is_archivable(Path::new("/x/a.rs"), 10));
        assert!(!is_archivable(Path::new("/x/noext"), 10));
    }

    #[test]
    fn reconcile_marks_live_archives_and_prunes() {
        let tmp = TempDir::new().unwrap();
        let root = data_root(tmp.path());
        let live = tmp.path().join("live.md");
        let gone_kept = tmp.path().join("gone-kept.md");
        let gone_lost = tmp.path().join("gone-lost.md");
        fs::write(&live, "l").unwrap();
        let copy = archive_file(&root, &live, &|_| true).unwrap();
        let mut kept = entry(&gone_kept, Some("s1"));
        kept.managed_path = Some(copy.display().to_string());
        // A managed path outside the managed dir never counts, even when it exists.
        let mut escaped = entry(&gone_lost, Some("s1"));
        escaped.managed_path = Some(live.display().to_string());
        let mut reg = Registry {
            entries: vec![
                entry(&live, Some("s1")),
                kept,
                escaped,
                entry(&tmp.path().join("other.md"), Some("s2")),
            ],
        };
        let out = reg.reconcile(&managed_dir(&root), Some(&["s1"]));
        assert_eq!((out.live, out.archived.len(), out.pruned), (1, 1, 1));
        assert_eq!(out.archived[0].status, STATUS_ARCHIVED);
        // The other session's missing entry was not examined.
        assert_eq!(reg.entries.len(), 3);
        let all = reg.reconcile(&managed_dir(&root), None);
        assert_eq!((all.live, all.archived.len(), all.pruned), (1, 1, 1));
    }

    #[test]
    fn archive_file_copies_atomically_and_refuses_oversize() {
        let tmp = TempDir::new().unwrap();
        let root = data_root(tmp.path());
        let src = tmp.path().join("pic.png");
        fs::write(&src, b"png-bytes").unwrap();
        let dest = archive_file(&root, &src, &|_| true).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"png-bytes");
        assert!(dest.starts_with(managed_dir(&root)));
        let big = tmp.path().join("big.png");
        fs::write(&big, vec![0u8; MAX_ARCHIVE_BYTES as usize + 1]).unwrap();
        assert!(archive_file(&root, &big, &|_| true).is_err());
        let names: Vec<_> = fs::read_dir(managed_dir(&root))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(names.len(), 1, "no temp file left behind: {names:?}");
    }

    fn managed_names(root: &Path) -> Vec<String> {
        fs::read_dir(managed_dir(root))
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn archive_file_refuses_a_symlink_at_the_source_path() {
        let tmp = TempDir::new().unwrap();
        let root = data_root(tmp.path());
        let secret = tmp.path().join("secret.txt");
        fs::write(&secret, "TOKEN=1").unwrap();
        // A vetted report.md swapped for a link to a sensitive file after the vet.
        let link = tmp.path().join("report.md");
        std::os::unix::fs::symlink(&secret, &link).unwrap();
        assert!(archive_file(&root, &link, &|_| true).is_err());
        assert!(
            managed_names(&root).is_empty(),
            "{:?}",
            managed_names(&root)
        );
    }

    #[test]
    fn archive_file_refuses_a_link_to_a_forbidden_looking_file_and_a_failed_recheck() {
        let tmp = TempDir::new().unwrap();
        let root = data_root(tmp.path());
        let env = tmp.path().join(".env");
        fs::write(&env, "TOKEN=1").unwrap();
        let link = tmp.path().join("notes.md");
        std::os::unix::fs::symlink(&env, &link).unwrap();
        let not_forbidden = |p: &Path| p.file_name().is_some_and(|n| n != ".env");
        assert!(archive_file(&root, &link, &not_forbidden).is_err());
        assert!(managed_names(&root).is_empty());

        // A regular file whose resolved path fails the caller's vetting is not copied either, and
        // the callback sees the path the open handle resolved to.
        let real = fs::canonicalize(tmp.path()).unwrap().join("real.md");
        fs::write(&real, "# ok").unwrap();
        let seen = Mutex::new(Vec::new());
        let record_and_refuse = |p: &Path| {
            seen.lock().unwrap().push(p.to_path_buf());
            false
        };
        assert!(archive_file(&root, &real, &record_and_refuse).is_err());
        assert_eq!(*seen.lock().unwrap(), std::slice::from_ref(&real));
        assert!(managed_names(&root).is_empty());
        assert!(archive_file(&root, &real, &|_| true).is_ok());
    }

    #[test]
    fn a_source_that_grows_past_the_cap_is_refused_after_reading_one_byte_too_many() {
        let tmp = TempDir::new().unwrap();
        let tmp_file = tmp.path().join(".tmp-grow");
        // An endless source: the copy must stop at cap + 1 bytes, not read it all.
        let endless = io::repeat(7);
        let err = copy_capped(endless, &tmp_file).unwrap_err();
        assert!(err.to_string().contains("size cap"), "{err}");
        assert_eq!(
            fs::metadata(&tmp_file).unwrap().len(),
            MAX_ARCHIVE_BYTES + 1
        );
        // And through archive_file: refused, nothing left in managed/.
        let root = data_root(tmp.path());
        let big = tmp.path().join("big.pdf");
        fs::write(&big, vec![0u8; MAX_ARCHIVE_BYTES as usize + 1]).unwrap();
        assert!(archive_file(&root, &big, &|_| true).is_err());
        assert!(managed_names(&root).is_empty());
        // Exactly at the cap is fine.
        let exact = tmp.path().join("exact.pdf");
        fs::write(&exact, vec![0u8; MAX_ARCHIVE_BYTES as usize]).unwrap();
        assert!(archive_file(&root, &exact, &|_| true).is_ok());
    }

    #[test]
    fn archive_sweeps_only_stale_temp_files() {
        let tmp = TempDir::new().unwrap();
        let root = data_root(tmp.path());
        let dir = managed_dir(&root);
        fs::create_dir_all(&dir).unwrap();
        let old = dir.join(".tmp-1-1");
        let fresh = dir.join(".tmp-2-2");
        let keep = dir.join("0000-kept.md");
        for f in [&old, &fresh, &keep] {
            fs::write(f, "x").unwrap();
        }
        let two_hours_ago = SystemTime::now() - Duration::from_secs(2 * 60 * 60);
        for f in [&old, &keep] {
            fs::File::options()
                .write(true)
                .open(f)
                .unwrap()
                .set_modified(two_hours_ago)
                .unwrap();
        }
        let src = tmp.path().join("a.md");
        fs::write(&src, "# a").unwrap();
        archive_file(&root, &src, &|_| true).unwrap();
        assert!(!old.exists(), "stale temp swept");
        assert!(fresh.exists(), "recent temp left for a concurrent archive");
        assert!(keep.exists(), "only .tmp-* files are swept");
    }

    #[test]
    fn current_managed_copy_needs_equal_size_and_a_copy_not_older_than_the_source() {
        let tmp = TempDir::new().unwrap();
        let root = data_root(tmp.path());
        let src = tmp.path().join("a.md");
        fs::write(&src, "# a").unwrap();
        assert!(current_managed_copy(&root, &src).is_none());
        let dest = archive_file(&root, &src, &|_| true).unwrap();
        assert_eq!(current_managed_copy(&root, &src), Some(dest.clone()));
        fs::write(&src, "# longer").unwrap();
        assert!(current_managed_copy(&root, &src).is_none(), "size differs");
        fs::write(&src, "# b").unwrap();
        let later = SystemTime::now() + Duration::from_secs(60);
        fs::File::options()
            .write(true)
            .open(&src)
            .unwrap()
            .set_modified(later)
            .unwrap();
        assert!(current_managed_copy(&root, &src).is_none(), "source newer");
    }

    #[test]
    fn a_damaged_registry_is_kept_aside_before_the_next_save_replaces_it() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("artifacts");
        fs::create_dir_all(&root).unwrap();
        let good = serde_json::to_value(entry(Path::new("/x/old.md"), Some("s1"))).unwrap();
        // One good entry, one that no longer decodes (as after a schema change).
        let doc = serde_json::json!({ "entries": [good, { "path": 3 }] });
        fs::write(registry_path(&root), doc.to_string()).unwrap();
        with_registry(&root, |reg| {
            reg.upsert(entry(Path::new("/x/new.md"), None));
        });
        let kept = fs::read_to_string(corrupt_backup_path(&root)).unwrap();
        assert_eq!(kept, doc.to_string(), "the original text must survive");
        assert_eq!(Registry::load(&root).entries.len(), 2);

        // Unparseable text is kept too, and a registry with nothing dropped leaves no backup.
        let _ = fs::remove_file(corrupt_backup_path(&root));
        fs::write(registry_path(&root), b"{ truncated").unwrap();
        with_registry(&root, |reg| reg.upsert(entry(Path::new("/x/a.md"), None)));
        assert_eq!(
            fs::read(corrupt_backup_path(&root)).unwrap(),
            b"{ truncated"
        );
        let _ = fs::remove_file(corrupt_backup_path(&root));
        with_registry(&root, |reg| reg.upsert(entry(Path::new("/x/b.md"), None)));
        assert!(!corrupt_backup_path(&root).exists());
    }

    #[test]
    fn a_read_only_use_of_a_damaged_registry_does_not_touch_it() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("artifacts");
        fs::create_dir_all(&root).unwrap();
        fs::write(registry_path(&root), b"not json").unwrap();
        let n = with_registry(&root, |reg| reg.entries.len());
        assert_eq!(n, 0);
        assert_eq!(fs::read(registry_path(&root)).unwrap(), b"not json");
        assert!(!corrupt_backup_path(&root).exists());
    }

    #[test]
    fn concurrent_registry_updates_are_not_lost() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("artifacts");
        std::thread::scope(|scope| {
            for i in 0..16 {
                let root = &root;
                scope.spawn(move || {
                    with_registry(root, |reg| {
                        reg.upsert(entry(Path::new(&format!("/x/{i}.md")), Some("s")));
                    });
                });
            }
        });
        assert_eq!(Registry::load(&root).entries.len(), 16);
    }

    #[test]
    fn write_atomic_replaces_a_symlink_instead_of_writing_through_it() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("precious.txt");
        fs::write(&target, b"keep").unwrap();
        let dest = tmp.path().join("settings.json");
        std::os::unix::fs::symlink(&target, &dest).unwrap();
        write_file_atomic(&dest, b"new").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"keep");
        assert_eq!(fs::read(&dest).unwrap(), b"new");
        assert!(
            !fs::symlink_metadata(&dest)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn sanitised_names_stay_within_the_length_cap_and_keep_a_short_extension() {
        let long = format!("{}.md", "n".repeat(500));
        let name = sanitize_file_name(&long);
        assert_eq!(name.len(), MAX_MANAGED_NAME_LEN);
        assert!(name.ends_with(".md"));
        // A long "extension" is not preserved, but the cap still holds.
        let odd = format!("a.{}", "e".repeat(300));
        assert!(sanitize_file_name(&odd).len() <= MAX_MANAGED_NAME_LEN);
        for raw in ["..", ".", "...md", "", "\u{202e}md.exe", "a/../b", "é"] {
            let clean = sanitize_file_name(raw);
            assert!(!clean.is_empty() && !clean.starts_with('.'), "{raw:?}");
            assert!(
                clean
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            );
        }
    }
}
