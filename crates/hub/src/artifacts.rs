//! Artifact registry (Record 29): which files the hub has shown as artifacts, plus archived
//! copies of the user-facing ones so they outlive the worktree that held them.
//!
//! Layout under `<home>/.local/share/fl/artifacts/`: `registry.json` and `managed/`. This module
//! holds the pure registry and file logic; path vetting (forbidden names, allowed roots) stays
//! with the API handlers, which call in here only with paths they already checked.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

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
    pub(crate) fn load(root: &Path) -> Self {
        let Ok(bytes) = fs::read(registry_path(root)) else {
            return Self::default();
        };
        let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
            return Self::default();
        };
        let entries = value
            .get("entries")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| serde_json::from_value(item.clone()).ok())
                    .collect()
            })
            .unwrap_or_default();
        Self { entries }
    }

    /// Temp file plus rename, so a reader never sees a half-written registry.
    pub(crate) fn save(&self, root: &Path) -> io::Result<()> {
        fs::create_dir_all(root)?;
        let bytes = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        write_atomic(&registry_path(root), &bytes)
    }

    /// Insert or refresh the entry for `entry.canonical_path`. The archived copy and the session
    /// stay unless the new entry carries a session.
    pub(crate) fn upsert(&mut self, mut entry: ArtifactEntry) {
        match self
            .entries
            .iter_mut()
            .find(|e| e.canonical_path == entry.canonical_path)
        {
            Some(existing) => {
                entry.managed_path = existing.managed_path.take();
                if entry.session_id.is_none() {
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
    let mut registry = Registry::load(root);
    let before = registry.clone();
    let out = f(&mut registry);
    if registry != before
        && let Err(err) = registry.save(root)
    {
        eprintln!("fl-hub: artifact registry save failed: {err}");
    }
    out
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

/// Copy `canonical` into the managed dir under root `root`; returns the copy's path. The caller
/// has already vetted `canonical`. The copy is refused when the file is over the size cap.
pub(crate) fn archive_file(root: &Path, canonical: &Path) -> io::Result<PathBuf> {
    let dir = managed_dir(root);
    fs::create_dir_all(&dir)?;
    let dest = dir.join(managed_file_name(canonical));
    let tmp = temp_path_beside(&dest);
    let copied = fs::copy(canonical, &tmp).and_then(|len| {
        if len > MAX_ARCHIVE_BYTES {
            Err(io::Error::other("artifact exceeds the archive size cap"))
        } else {
            fs::rename(&tmp, &dest)
        }
    });
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
    fn upsert_keeps_the_managed_copy_and_the_session_unless_replaced() {
        let mut reg = Registry::default();
        let mut first = entry(Path::new("/x/a.md"), Some("s1"));
        first.managed_path = Some("/m/copy".into());
        reg.upsert(first);
        reg.upsert(entry(Path::new("/x/a.md"), None));
        assert_eq!(reg.entries.len(), 1);
        assert_eq!(reg.entries[0].managed_path.as_deref(), Some("/m/copy"));
        assert_eq!(reg.entries[0].session_id.as_deref(), Some("s1"));
        reg.upsert(entry(Path::new("/x/a.md"), Some("s2")));
        assert_eq!(reg.entries[0].session_id.as_deref(), Some("s2"));
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
        let copy = archive_file(&root, &live).unwrap();
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
        let dest = archive_file(&root, &src).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"png-bytes");
        assert!(dest.starts_with(managed_dir(&root)));
        let big = tmp.path().join("big.png");
        fs::write(&big, vec![0u8; MAX_ARCHIVE_BYTES as usize + 1]).unwrap();
        assert!(archive_file(&root, &big).is_err());
        let names: Vec<_> = fs::read_dir(managed_dir(&root))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(names.len(), 1, "no temp file left behind: {names:?}");
    }
}
