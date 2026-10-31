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
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub(crate) const STATUS_LIVE: &str = "live";
pub(crate) const STATUS_ARCHIVED: &str = "archived";

/// Largest file the hub copies into the managed dir.
pub(crate) const MAX_ARCHIVE_BYTES: u64 = 5 * 1024 * 1024;

/// User-facing extensions worth keeping after the worktree is gone.
const ARCHIVE_EXTS: &[&str] = &[
    "md", "markdown", "mmd", "mermaid", "png", "jpg", "jpeg", "webp", "gif", "svg", "pdf", "docx",
    "xlsx", "pptx",
];

/// Most content revisions kept per artifact; older ones lose their snapshot and their row.
pub(crate) const MAX_VERSIONS_PER_ARTIFACT: usize = 50;

/// Largest file `record_version` reads to hash it. A revision above `MAX_ARCHIVE_BYTES` is
/// recorded (digest, size) but its bytes are not stored.
const MAX_VERSION_HASH_BYTES: u64 = 16 * 1024 * 1024;

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
    /// Content revisions, oldest first. Older registries have none.
    #[serde(default)]
    pub versions: Vec<ArtifactVersion>,
}

/// One recorded content revision of an artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ArtifactVersion {
    /// 1-based and never reused, even after older revisions are trimmed.
    pub version: u32,
    pub sha256: String,
    pub size_bytes: u64,
    pub recorded_at_ms: u64,
    /// Modification time of the source when this revision was read, to skip re-hashing an
    /// unchanged file.
    #[serde(default)]
    pub source_mtime_ms: u64,
    /// The stored copy under `<root>/versions/`; none when the file was above the archive cap.
    pub snapshot_path: Option<String>,
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
                entry.versions = std::mem::take(&mut existing.versions);
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
                for v in &entry.versions {
                    remove_snapshot(managed_dir, v);
                }
                continue;
            }
            kept.push(entry);
        }
        self.entries = kept;
        out
    }
}

impl Registry {
    /// The entry for a path the API vetted: its canonical original path, or its managed copy.
    pub(crate) fn find_for_path(&self, canon: &Path) -> Option<&ArtifactEntry> {
        let canon_str = canon.to_string_lossy();
        self.entries.iter().find(|e| {
            e.canonical_path == canon_str
                || e.managed_path
                    .as_deref()
                    .and_then(|m| fs::canonicalize(m).ok())
                    .is_some_and(|m| m == canon)
        })
    }
}

/// Where the snapshots of `canonical` live: `<root>/versions/<managed file name>/`.
fn versions_dir(root: &Path, canonical: &Path) -> PathBuf {
    root.join("versions").join(managed_file_name(canonical))
}

/// Best-effort removal of a revision's snapshot. `anchor` is any dir under the artifact root
/// (the managed dir); the snapshot is only removed when it lies under `<root>/versions`.
fn remove_snapshot(anchor: &Path, v: &ArtifactVersion) {
    let Some(snap) = v.snapshot_path.as_deref() else {
        return;
    };
    let Some(root) = anchor.parent() else { return };
    let base = root.join("versions");
    if Path::new(snap).starts_with(&base) && !snap.contains("..") {
        let _ = fs::remove_file(snap);
    }
}

/// Lowercase hex SHA-256 of `bytes`.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn mtime_ms(meta: &fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Record the current content of `canonical` as a new revision when it differs from the latest
/// recorded one. Returns whether a revision was added. The artifact must already be in the
/// registry (`upsert` first). The source is opened like `archive_file` does (no symlink at the
/// last component, `recheck` on the resolved path, regular file, size cap); an unchanged file
/// (same size and mtime as the latest revision) is not read at all. The snapshot is written
/// under the registry lock so version numbers and files stay in step.
pub(crate) fn record_version(
    root: &Path,
    canonical: &Path,
    recheck: &dyn Fn(&Path) -> bool,
) -> io::Result<bool> {
    let canon_str = canonical.to_string_lossy().to_string();
    let latest = with_registry(root, |reg| {
        reg.entries
            .iter()
            .find(|e| e.canonical_path == canon_str)
            .map(|e| e.versions.last().map(|v| (v.size_bytes, v.source_mtime_ms)))
    });
    let Some(latest) = latest else {
        return Err(io::Error::other("artifact is not in the registry"));
    };
    let src = open_nofollow(canonical)?;
    #[cfg(target_os = "linux")]
    if !recheck(&resolved_path_of(&src)?) {
        return Err(io::Error::other("artifact no longer passes vetting"));
    }
    #[cfg(not(target_os = "linux"))]
    let _ = recheck;
    let meta = src.metadata()?;
    if !meta.is_file() || meta.len() > MAX_VERSION_HASH_BYTES {
        return Err(io::Error::other(
            "artifact is not a regular file within the version size cap",
        ));
    }
    let mtime = mtime_ms(&meta);
    if latest == Some((meta.len(), mtime)) {
        return Ok(false);
    }
    let mut bytes = Vec::new();
    src.take(MAX_VERSION_HASH_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_VERSION_HASH_BYTES {
        return Err(io::Error::other("artifact grew past the version size cap"));
    }
    let digest = sha256_hex(&bytes);
    let size = bytes.len() as u64;
    let store = size <= MAX_ARCHIVE_BYTES;

    let added = with_registry(root, |reg| {
        let Some(entry) = reg
            .entries
            .iter_mut()
            .find(|e| e.canonical_path == canon_str)
        else {
            return false;
        };
        if let Some(last) = entry.versions.last_mut()
            && last.sha256 == digest
        {
            // Touched but not changed: remember the new stamp so the next scan skips it.
            last.source_mtime_ms = mtime;
            return false;
        }
        let version = entry.versions.last().map_or(1, |v| v.version + 1);
        let snapshot_path = store.then(|| {
            let dir = versions_dir(root, canonical);
            let dest = dir.join(format!("v{version}"));
            fs::create_dir_all(&dir)
                .and_then(|()| write_atomic(&dest, &bytes))
                .map(|()| dest.display().to_string())
                .map_err(|err| eprintln!("fl-hub: could not store artifact snapshot: {err}"))
                .ok()
        });
        entry.versions.push(ArtifactVersion {
            version,
            sha256: digest.clone(),
            size_bytes: size,
            recorded_at_ms: now_ms(),
            source_mtime_ms: mtime,
            snapshot_path: snapshot_path.flatten(),
        });
        while entry.versions.len() > MAX_VERSIONS_PER_ARTIFACT {
            let old = entry.versions.remove(0);
            remove_snapshot(&managed_dir(root), &old);
        }
        true
    });
    Ok(added)
}

/// The bytes of revision `version` of the entry for `canon`, read from its stored snapshot. The
/// registry is only a hint, so the snapshot must lie inside `<root>/versions` and within the
/// archive cap.
pub(crate) fn read_version_bytes(
    root: &Path,
    entry: &ArtifactEntry,
    version: u32,
) -> Result<Vec<u8>, &'static str> {
    let v = entry
        .versions
        .iter()
        .find(|v| v.version == version)
        .ok_or("version not found")?;
    let snap = v
        .snapshot_path
        .as_deref()
        .ok_or("version content was not stored")?;
    let canon = fs::canonicalize(snap).map_err(|_| "version content is missing")?;
    let base = fs::canonicalize(root.join("versions")).map_err(|_| "version content is missing")?;
    if !canon.starts_with(&base) {
        return Err("version content is missing");
    }
    let file = fs::File::open(&canon).map_err(|_| "version content is missing")?;
    let mut bytes = Vec::new();
    file.take(MAX_ARCHIVE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "version content is missing")?;
    if bytes.len() as u64 > MAX_ARCHIVE_BYTES {
        return Err("version content exceeds the size limit");
    }
    Ok(bytes)
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

// ---------------------------------------------------------------------------------------------
// MS Office structured previews (.docx / .xlsx / .pptx).
//
// An OOXML file is a ZIP of XML parts. This reads the ZIP central directory and inflates only
// the parts a preview needs, then walks their XML with a small tokenizer. No unsafe code, no
// office crate, and every limit below bounds what a hostile archive can make the hub allocate.
// ---------------------------------------------------------------------------------------------

/// Largest compressed archive the office reader accepts.
pub(crate) const MAX_OFFICE_COMPRESSED_BYTES: u64 = 32 * 1024 * 1024;
/// Most bytes all entries of one archive may declare (and may actually inflate to).
pub(crate) const MAX_OFFICE_UNCOMPRESSED_BYTES: u64 = 64 * 1024 * 1024;
/// Most entries one archive may hold.
pub(crate) const MAX_OFFICE_ENTRIES: usize = 256;
const MAX_DOCX_BLOCKS: usize = 5000;
const MAX_SHEETS: usize = 32;
const MAX_SHEET_ROWS: usize = 2000;
const MAX_SHEET_COLS: usize = 100;
const MAX_SLIDES: usize = 256;

/// Why an office file could not be previewed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OfficeError(pub String);

impl std::fmt::Display for OfficeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn oerr<T>(msg: impl Into<String>) -> Result<T, OfficeError> {
    Err(OfficeError(msg.into()))
}

struct ZipEntry {
    name: String,
    method: u16,
    comp_size: usize,
    uncomp_size: usize,
    local_offset: usize,
}

/// A parsed ZIP central directory over borrowed bytes.
pub(crate) struct ZipArchive<'a> {
    data: &'a [u8],
    entries: Vec<ZipEntry>,
}

fn u16_at(d: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        d.get(at..at.checked_add(2)?)?.try_into().ok()?,
    ))
}

fn u32_at(d: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        d.get(at..at.checked_add(4)?)?.try_into().ok()?,
    ))
}

impl<'a> ZipArchive<'a> {
    /// Parse the central directory. Rejects archives over the compressed, entry-count, or
    /// declared-uncompressed limits, encrypted entries, and ZIP64 (OOXML previews never need it).
    pub(crate) fn parse(data: &'a [u8]) -> Result<Self, OfficeError> {
        if data.len() as u64 > MAX_OFFICE_COMPRESSED_BYTES {
            return oerr("archive exceeds the 32 MiB compressed limit");
        }
        if data.len() < 22 {
            return oerr("not a ZIP archive");
        }
        // The end-of-central-directory record sits within the last 22 + 65535 bytes.
        let lowest = data.len().saturating_sub(22 + 65535);
        let mut eocd = None;
        let mut at = data.len() - 22;
        loop {
            if u32_at(data, at) == Some(0x0605_4b50) {
                eocd = Some(at);
                break;
            }
            if at == lowest {
                break;
            }
            at -= 1;
        }
        let Some(eocd) = eocd else {
            return oerr("not a ZIP archive");
        };
        let total = u16_at(data, eocd + 10).unwrap_or(0) as usize;
        let cd_size = u32_at(data, eocd + 12).unwrap_or(0) as usize;
        let cd_off = u32_at(data, eocd + 16).unwrap_or(0) as usize;
        if total == 0xFFFF || cd_size == 0xFFFF_FFFF || cd_off == 0xFFFF_FFFF {
            return oerr("ZIP64 archives are not supported");
        }
        if total > MAX_OFFICE_ENTRIES {
            return oerr("archive holds more than 256 entries");
        }
        let cd_end = cd_off
            .checked_add(cd_size)
            .filter(|end| *end <= data.len())
            .ok_or_else(|| OfficeError("corrupt ZIP central directory".into()))?;
        let mut entries = Vec::with_capacity(total);
        let mut pos = cd_off;
        let mut declared: u64 = 0;
        for _ in 0..total {
            if pos + 46 > cd_end || u32_at(data, pos) != Some(0x0201_4b50) {
                return oerr("corrupt ZIP central directory");
            }
            let flags = u16_at(data, pos + 8).unwrap_or(0);
            let method = u16_at(data, pos + 10).unwrap_or(0);
            let comp_size = u32_at(data, pos + 20).unwrap_or(0) as usize;
            let uncomp_size = u32_at(data, pos + 24).unwrap_or(0) as usize;
            let name_len = u16_at(data, pos + 28).unwrap_or(0) as usize;
            let extra_len = u16_at(data, pos + 30).unwrap_or(0) as usize;
            let comment_len = u16_at(data, pos + 32).unwrap_or(0) as usize;
            let local_offset = u32_at(data, pos + 42).unwrap_or(0) as usize;
            let name_end = pos + 46 + name_len;
            let next = name_end + extra_len + comment_len;
            if next > cd_end {
                return oerr("corrupt ZIP central directory");
            }
            if flags & 1 != 0 {
                return oerr("encrypted ZIP entries are not supported");
            }
            declared = declared.saturating_add(uncomp_size as u64);
            if declared > MAX_OFFICE_UNCOMPRESSED_BYTES {
                return oerr("archive exceeds the 64 MiB uncompressed limit");
            }
            let name = String::from_utf8_lossy(&data[pos + 46..name_end]).to_string();
            entries.push(ZipEntry {
                name,
                method,
                comp_size,
                uncomp_size,
                local_offset,
            });
            pos = next;
        }
        Ok(Self { data, entries })
    }

    pub(crate) fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|e| e.name.as_str())
    }

    pub(crate) fn contains(&self, name: &str) -> bool {
        self.entries.iter().any(|e| e.name == name)
    }

    /// Inflate the entry `name`. `budget` is what is left of the archive-wide uncompressed
    /// allowance; it shrinks by the bytes produced. `Ok(None)` when there is no such entry.
    pub(crate) fn read(
        &self,
        name: &str,
        budget: &mut u64,
    ) -> Result<Option<Vec<u8>>, OfficeError> {
        let Some(e) = self.entries.iter().find(|e| e.name == name) else {
            return Ok(None);
        };
        let bad = || OfficeError(format!("corrupt ZIP entry {name}"));
        let lo = e.local_offset;
        if u32_at(self.data, lo) != Some(0x0403_4b50) {
            return Err(bad());
        }
        let n = u16_at(self.data, lo + 26).ok_or_else(bad)? as usize;
        let x = u16_at(self.data, lo + 28).ok_or_else(bad)? as usize;
        let start = lo + 30 + n + x;
        let end = start.checked_add(e.comp_size).ok_or_else(bad)?;
        let raw = self.data.get(start..end).ok_or_else(bad)?;
        // Never trust the declared size: cap the inflater by the declaration and the budget.
        let cap = (e.uncomp_size as u64).min(*budget);
        let mut out = Vec::new();
        match e.method {
            0 => {
                if raw.len() as u64 > cap {
                    return oerr(format!("entry {name} exceeds its declared size"));
                }
                out.extend_from_slice(raw);
            }
            8 => {
                flate2::read::DeflateDecoder::new(raw)
                    .take(cap + 1)
                    .read_to_end(&mut out)
                    .map_err(|_| bad())?;
                if out.len() as u64 > cap {
                    return oerr(format!("entry {name} inflates past its declared size"));
                }
            }
            m => return oerr(format!("unsupported ZIP compression method {m}")),
        }
        *budget -= out.len() as u64;
        Ok(Some(out))
    }
}

/// One token of the XML walk. Names are local names (the namespace prefix is dropped).
#[derive(Debug, PartialEq, Eq)]
enum Xml {
    Start {
        name: String,
        attrs: String,
        empty: bool,
    },
    End(String),
    Text(String),
}

fn local_name(raw: &str) -> &str {
    raw.rsplit(':').next().unwrap_or(raw)
}

fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(semi) = rest.find(';').filter(|j| *j <= 10) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let ent = &rest[1..semi];
        let ch = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => ent
                .strip_prefix("#x")
                .or_else(|| ent.strip_prefix("#X"))
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| ent.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match ch {
            Some(c) => {
                out.push(c);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Tokenize XML text. Comments, processing instructions and DOCTYPE are skipped; only the five
/// predefined entities and numeric references are expanded, so no entity can blow up.
fn xml_tokens(xml: &str) -> Vec<Xml> {
    let mut out = Vec::new();
    let mut rest = xml;
    while !rest.is_empty() {
        let Some(lt) = rest.find('<') else {
            out.push(Xml::Text(decode_entities(rest)));
            break;
        };
        if lt > 0 {
            out.push(Xml::Text(decode_entities(&rest[..lt])));
        }
        rest = &rest[lt..];
        if let Some(body) = rest.strip_prefix("<![CDATA[") {
            let end = body.find("]]>").unwrap_or(body.len());
            out.push(Xml::Text(body[..end].to_string()));
            rest = body.get(end + 3..).unwrap_or("");
        } else if let Some(body) = rest.strip_prefix("<!--") {
            rest = body.find("-->").map_or("", |i| &body[i + 3..]);
        } else if rest.starts_with("<?") || rest.starts_with("<!") {
            rest = rest.find('>').map_or("", |i| &rest[i + 1..]);
        } else {
            // A tag; `>` inside a quoted attribute value does not end it.
            let mut quote = None;
            let mut end = None;
            for (i, c) in rest.char_indices().skip(1) {
                match (quote, c) {
                    (None, '"' | '\'') => quote = Some(c),
                    (Some(q), c) if c == q => quote = None,
                    (None, '>') => {
                        end = Some(i);
                        break;
                    }
                    _ => {}
                }
            }
            let Some(end) = end else { break };
            let inner = &rest[1..end];
            rest = &rest[end + 1..];
            if let Some(name) = inner.strip_prefix('/') {
                out.push(Xml::End(local_name(name.trim()).to_string()));
            } else {
                let empty = inner.ends_with('/');
                let inner = inner.trim_end_matches('/');
                let (name, attrs) = inner
                    .split_once(|c: char| c.is_whitespace())
                    .unwrap_or((inner, ""));
                out.push(Xml::Start {
                    name: local_name(name).to_string(),
                    attrs: attrs.to_string(),
                    empty,
                });
            }
        }
    }
    out
}

/// The value of attribute `key` (compared by local name) in a raw attribute string.
fn xml_attr(attrs: &str, key: &str) -> Option<String> {
    let mut rest = attrs.trim_start();
    while !rest.is_empty() {
        let eq = rest.find('=')?;
        let name = rest[..eq].trim();
        let after = rest[eq + 1..].trim_start();
        let q = after.chars().next().filter(|c| *c == '"' || *c == '\'')?;
        let close = after[1..].find(q)?;
        let value = &after[1..1 + close];
        if local_name(name) == key {
            return Some(decode_entities(value));
        }
        rest = after[close + 2..].trim_start();
    }
    None
}

fn read_utf8(
    zip: &ZipArchive<'_>,
    name: &str,
    budget: &mut u64,
) -> Result<Option<String>, OfficeError> {
    Ok(zip
        .read(name, budget)?
        .map(|b| String::from_utf8_lossy(&b).into_owned()))
}

/// Number in a part name such as `sheet12.xml` or `slide3.xml`.
fn part_number(name: &str, prefix: &str) -> Option<u32> {
    name.strip_prefix(prefix)?
        .strip_suffix(".xml")?
        .parse()
        .ok()
}

/// Parts named `<dir><prefix>N.xml`, in numeric order.
fn numbered_parts(zip: &ZipArchive<'_>, dir: &str, prefix: &str) -> Vec<(u32, String)> {
    let mut parts: Vec<(u32, String)> = zip
        .names()
        .filter_map(|n| {
            let file = n.strip_prefix(dir)?;
            (!file.contains('/')).then_some(())?;
            Some((part_number(file, prefix)?, n.to_string()))
        })
        .collect();
    parts.sort();
    parts
}

fn heading_level(style: &str) -> Option<u8> {
    let lower = style.to_ascii_lowercase().replace([' ', '-', '_'], "");
    let digit = lower.strip_prefix("heading")?;
    match digit.parse::<u8>() {
        Ok(n @ 1..=6) => Some(n),
        _ => None,
    }
}

/// `word/document.xml` into ordered blocks: headings, paragraphs and tables.
fn docx_blocks(xml: &str) -> (Vec<Value>, bool) {
    let mut blocks: Vec<Value> = Vec::new();
    let mut truncated = false;
    let mut tbl_depth = 0usize;
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut cell = String::new();
    let mut para = String::new();
    let mut style = String::new();
    let mut in_para = false;
    let mut in_text = false;
    for tok in xml_tokens(xml) {
        match tok {
            Xml::Start { name, attrs, empty } => match name.as_str() {
                "tbl" => {
                    tbl_depth += 1;
                    if tbl_depth == 1 {
                        rows.clear();
                    }
                }
                "tr" if tbl_depth == 1 => row.clear(),
                "tc" if tbl_depth == 1 => cell.clear(),
                "p" => {
                    in_para = true;
                    para.clear();
                    style.clear();
                }
                "pStyle" if in_para => style = xml_attr(&attrs, "val").unwrap_or_default(),
                "t" if in_para && !empty => in_text = true,
                "tab" if in_para => para.push('\t'),
                "br" | "cr" if in_para => para.push('\n'),
                _ => {}
            },
            Xml::Text(t) if in_text => para.push_str(&t),
            Xml::Text(_) => {}
            Xml::End(name) => match name.as_str() {
                "t" => in_text = false,
                "p" => {
                    in_para = false;
                    let text = para.trim().to_string();
                    if tbl_depth > 0 {
                        if !text.is_empty() {
                            if !cell.is_empty() {
                                cell.push('\n');
                            }
                            cell.push_str(&text);
                        }
                    } else if !text.is_empty() {
                        if blocks.len() >= MAX_DOCX_BLOCKS {
                            truncated = true;
                        } else if let Some(level) = heading_level(&style) {
                            blocks.push(json!({ "type": "heading", "level": level, "text": text }));
                        } else {
                            blocks.push(json!({ "type": "paragraph", "text": text }));
                        }
                    }
                }
                "tc" if tbl_depth == 1 => row.push(std::mem::take(&mut cell)),
                "tr" if tbl_depth == 1 => {
                    if rows.len() < MAX_SHEET_ROWS {
                        rows.push(std::mem::take(&mut row));
                    } else {
                        truncated = true;
                    }
                }
                "tbl" => {
                    tbl_depth = tbl_depth.saturating_sub(1);
                    if tbl_depth == 0 {
                        if blocks.len() >= MAX_DOCX_BLOCKS {
                            truncated = true;
                        } else {
                            blocks.push(
                                json!({ "type": "table", "rows": std::mem::take(&mut rows) }),
                            );
                        }
                    }
                }
                _ => {}
            },
        }
    }
    (blocks, truncated)
}

/// `xl/sharedStrings.xml` into the string table.
fn shared_strings(xml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let (mut in_si, mut in_t, mut in_phonetic) = (false, false, false);
    for tok in xml_tokens(xml) {
        match tok {
            Xml::Start { name, empty, .. } => match name.as_str() {
                "si" => {
                    in_si = true;
                    cur.clear();
                    if empty {
                        out.push(String::new());
                        in_si = false;
                    }
                }
                "rPh" => in_phonetic = true,
                "t" if in_si && !in_phonetic && !empty => in_t = true,
                _ => {}
            },
            Xml::Text(t) if in_t => cur.push_str(&t),
            Xml::Text(_) => {}
            Xml::End(name) => match name.as_str() {
                "t" => in_t = false,
                "rPh" => in_phonetic = false,
                "si" => {
                    in_si = false;
                    out.push(std::mem::take(&mut cur));
                }
                _ => {}
            },
        }
        if out.len() > 200_000 {
            break;
        }
    }
    out
}

/// Zero-based column of a cell reference such as `AB12`.
fn column_index(cell_ref: &str) -> Option<usize> {
    let letters: Vec<u8> = cell_ref
        .bytes()
        .take_while(u8::is_ascii_alphabetic)
        .collect();
    if letters.is_empty() {
        return None;
    }
    let mut n = 0usize;
    for b in letters {
        n = n
            .checked_mul(26)?
            .checked_add((b.to_ascii_uppercase() - b'A') as usize + 1)?;
    }
    Some(n - 1)
}

/// One worksheet into a dense grid (bounded by the sheet row and column caps).
fn sheet_grid(xml: &str, strings: &[String]) -> (Vec<Vec<String>>, bool) {
    let mut grid: Vec<Vec<String>> = Vec::new();
    let mut truncated = false;
    let mut row_idx = 0usize;
    let mut next_row = 0usize;
    let mut col_idx = 0usize;
    let mut next_col = 0usize;
    let mut cell_type = String::new();
    let mut value = String::new();
    let (mut in_cell, mut in_v, mut in_t) = (false, false, false);
    for tok in xml_tokens(xml) {
        match tok {
            Xml::Start { name, attrs, empty } => match name.as_str() {
                "row" => {
                    row_idx = xml_attr(&attrs, "r")
                        .and_then(|r| r.parse::<usize>().ok())
                        .map_or(next_row, |r| r.saturating_sub(1));
                    next_row = row_idx + 1;
                    next_col = 0;
                }
                "c" => {
                    col_idx = xml_attr(&attrs, "r")
                        .and_then(|r| column_index(&r))
                        .unwrap_or(next_col);
                    next_col = col_idx + 1;
                    cell_type = xml_attr(&attrs, "t").unwrap_or_default();
                    value.clear();
                    in_cell = !empty;
                }
                "v" if in_cell && !empty => in_v = true,
                "t" if in_cell && !empty => in_t = true,
                _ => {}
            },
            Xml::Text(t) if in_v || in_t => value.push_str(&t),
            Xml::Text(_) => {}
            Xml::End(name) => match name.as_str() {
                "v" => in_v = false,
                "t" => in_t = false,
                "c" if in_cell => {
                    in_cell = false;
                    let text = match cell_type.as_str() {
                        "s" => value
                            .trim()
                            .parse::<usize>()
                            .ok()
                            .and_then(|i| strings.get(i).cloned())
                            .unwrap_or_default(),
                        "b" => if value.trim() == "1" { "TRUE" } else { "FALSE" }.to_string(),
                        _ => value.clone(),
                    };
                    if text.is_empty() {
                        continue;
                    }
                    if row_idx >= MAX_SHEET_ROWS || col_idx >= MAX_SHEET_COLS {
                        truncated = true;
                        continue;
                    }
                    if grid.len() <= row_idx {
                        grid.resize_with(row_idx + 1, Vec::new);
                    }
                    let r = &mut grid[row_idx];
                    if r.len() <= col_idx {
                        r.resize(col_idx + 1, String::new());
                    }
                    r[col_idx] = text;
                }
                _ => {}
            },
        }
    }
    (grid, truncated)
}

/// Id to target pairs of a `.rels` part.
fn relationships(xml: &str) -> Vec<(String, String, String)> {
    xml_tokens(xml)
        .into_iter()
        .filter_map(|t| match t {
            Xml::Start { name, attrs, .. } if name == "Relationship" => Some((
                xml_attr(&attrs, "Id")?,
                xml_attr(&attrs, "Target")?,
                xml_attr(&attrs, "Type").unwrap_or_default(),
            )),
            _ => None,
        })
        .collect()
}

/// Resolve a relationship `target` against the directory of the part holding the rels.
fn resolve_target(base_dir: &str, target: &str) -> String {
    let mut parts: Vec<&str> = if target.starts_with('/') {
        Vec::new()
    } else {
        base_dir.split('/').filter(|s| !s.is_empty()).collect()
    };
    for seg in target.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

fn xlsx_preview(zip: &ZipArchive<'_>, budget: &mut u64) -> Result<Value, OfficeError> {
    let strings = read_utf8(zip, "xl/sharedStrings.xml", budget)?
        .map(|x| shared_strings(&x))
        .unwrap_or_default();
    let workbook = read_utf8(zip, "xl/workbook.xml", budget)?.unwrap_or_default();
    let rels = read_utf8(zip, "xl/_rels/workbook.xml.rels", budget)?
        .map(|x| relationships(&x))
        .unwrap_or_default();
    let declared: Vec<(String, Option<String>)> = xml_tokens(&workbook)
        .into_iter()
        .filter_map(|t| match t {
            Xml::Start { name, attrs, .. } if name == "sheet" => Some((
                xml_attr(&attrs, "name").unwrap_or_default(),
                xml_attr(&attrs, "id"),
            )),
            _ => None,
        })
        .collect();
    let ordinal = numbered_parts(zip, "xl/worksheets/", "sheet");
    let mut sheets = Vec::new();
    let mut any_truncated = declared.len() > MAX_SHEETS;
    // Sheets named by the workbook, with their part found by relationship, else by position.
    let count = if declared.is_empty() {
        ordinal.len()
    } else {
        declared.len()
    };
    for i in 0..count.min(MAX_SHEETS) {
        let (name, rid) = declared
            .get(i)
            .cloned()
            .unwrap_or_else(|| (format!("Sheet{}", i + 1), None));
        let by_rel = rid.and_then(|id| {
            rels.iter()
                .find(|(rid, _, _)| *rid == id)
                .map(|(_, target, _)| resolve_target("xl", target))
        });
        let part = by_rel
            .filter(|p| zip.contains(p))
            .or_else(|| ordinal.get(i).map(|(_, p)| p.clone()));
        let (rows, truncated) = match part {
            Some(p) => read_utf8(zip, &p, budget)?
                .map(|x| sheet_grid(&x, &strings))
                .unwrap_or_default(),
            None => (Vec::new(), false),
        };
        any_truncated |= truncated;
        let col_count = rows.iter().map(Vec::len).max().unwrap_or(0);
        sheets.push(json!({
            "name": name,
            "row_count": rows.len(),
            "col_count": col_count,
            "truncated": truncated,
            "rows": rows,
        }));
    }
    Ok(json!({ "kind": "xlsx", "sheets": sheets, "truncated": any_truncated }))
}

/// Text shapes of a slide or notes part: placeholder type plus paragraph texts. Paragraphs
/// outside any shape (table frames) come back as shapes with no placeholder.
fn slide_shapes(xml: &str) -> Vec<(Option<String>, Vec<String>)> {
    let mut shapes = Vec::new();
    let mut sp_depth = 0usize;
    let mut ph: Option<String> = None;
    let mut paras: Vec<String> = Vec::new();
    let mut loose: Vec<String> = Vec::new();
    let mut para = String::new();
    let (mut in_p, mut in_t) = (false, false);
    for tok in xml_tokens(xml) {
        match tok {
            Xml::Start { name, attrs, empty } => match name.as_str() {
                "sp" => {
                    sp_depth += 1;
                    if sp_depth == 1 {
                        ph = None;
                        paras.clear();
                    }
                }
                "ph" => ph = Some(xml_attr(&attrs, "type").unwrap_or_else(|| "body".into())),
                "p" => {
                    in_p = true;
                    para.clear();
                }
                "t" if in_p && !empty => in_t = true,
                "br" if in_p => para.push('\n'),
                _ => {}
            },
            Xml::Text(t) if in_t => para.push_str(&t),
            Xml::Text(_) => {}
            Xml::End(name) => match name.as_str() {
                "t" => in_t = false,
                "p" => {
                    in_p = false;
                    let text = para.trim().to_string();
                    if !text.is_empty() {
                        if sp_depth > 0 { &mut paras } else { &mut loose }.push(text);
                    }
                }
                "sp" => {
                    sp_depth = sp_depth.saturating_sub(1);
                    if sp_depth == 0 {
                        shapes.push((ph.take(), std::mem::take(&mut paras)));
                    }
                }
                _ => {}
            },
        }
    }
    if !loose.is_empty() {
        shapes.push((None, loose));
    }
    shapes
}

fn pptx_preview(zip: &ZipArchive<'_>, budget: &mut u64) -> Result<Value, OfficeError> {
    let slide_parts = numbered_parts(zip, "ppt/slides/", "slide");
    let mut slides = Vec::new();
    for (n, part) in slide_parts.iter().take(MAX_SLIDES) {
        let xml = read_utf8(zip, part, budget)?.unwrap_or_default();
        let mut title = String::new();
        let mut bullets: Vec<String> = Vec::new();
        for (ph, paras) in slide_shapes(&xml) {
            match ph.as_deref() {
                Some("title" | "ctrTitle") => {
                    if title.is_empty() {
                        title = paras.join(" ");
                    }
                }
                Some("sldNum" | "dt" | "ftr" | "hdr") => {}
                _ => bullets.extend(paras),
            }
        }
        // The notes part is named by the slide's relationships, else by the same number.
        let rels_name = format!("ppt/slides/_rels/slide{n}.xml.rels");
        let by_rel = read_utf8(zip, &rels_name, budget)?.and_then(|x| {
            relationships(&x)
                .into_iter()
                .find(|(_, _, ty)| ty.ends_with("/notesSlide"))
                .map(|(_, target, _)| resolve_target("ppt/slides", &target))
        });
        let notes_part = by_rel
            .filter(|p| zip.contains(p))
            .unwrap_or_else(|| format!("ppt/notesSlides/notesSlide{n}.xml"));
        let notes = read_utf8(zip, &notes_part, budget)?
            .map(|x| {
                slide_shapes(&x)
                    .into_iter()
                    .filter(|(ph, _)| ph.as_deref() == Some("body"))
                    .flat_map(|(_, paras)| paras)
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        slides.push(json!({
            "index": slides.len() + 1,
            "title": title,
            "bullets": bullets,
            "notes": notes,
        }));
    }
    Ok(json!({
        "kind": "pptx",
        "slides": slides,
        "truncated": slide_parts.len() > MAX_SLIDES,
    }))
}

/// Structured preview of an office file: `{ kind: "docx", blocks }`, `{ kind: "xlsx", sheets }`
/// or `{ kind: "pptx", slides }`, each with a `truncated` flag. `ext` is the lowercase extension.
pub(crate) fn office_preview(ext: &str, data: &[u8]) -> Result<Value, OfficeError> {
    let zip = ZipArchive::parse(data)?;
    let mut budget = MAX_OFFICE_UNCOMPRESSED_BYTES;
    match ext {
        "docx" => {
            let Some(xml) = read_utf8(&zip, "word/document.xml", &mut budget)? else {
                return oerr("not a Word document: word/document.xml is missing");
            };
            let (blocks, truncated) = docx_blocks(&xml);
            Ok(json!({ "kind": "docx", "blocks": blocks, "truncated": truncated }))
        }
        "xlsx" => {
            if !zip.contains("xl/workbook.xml") {
                return oerr("not an Excel workbook: xl/workbook.xml is missing");
            }
            xlsx_preview(&zip, &mut budget)
        }
        "pptx" => {
            if !zip.contains("ppt/presentation.xml") {
                return oerr("not a PowerPoint deck: ppt/presentation.xml is missing");
            }
            pptx_preview(&zip, &mut budget)
        }
        _ => oerr("unsupported office extension"),
    }
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
            versions: Vec::new(),
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

    // ---- version history ----

    fn registered(root: &Path, canon: &Path) {
        with_registry(root, |reg| reg.upsert(entry(canon, None)));
    }

    fn versions_of(root: &Path, canon: &Path) -> Vec<ArtifactVersion> {
        with_registry(root, |reg| {
            reg.find_for_path(canon)
                .map(|e| e.versions.clone())
                .unwrap_or_default()
        })
    }

    #[test]
    fn record_version_adds_a_revision_only_when_content_changes() {
        let tmp = TempDir::new().unwrap();
        let root = data_root(tmp.path());
        let file = tmp.path().join("a.md");
        fs::write(&file, "one").unwrap();
        let canon = fs::canonicalize(&file).unwrap();
        // Not registered yet: refused.
        assert!(record_version(&root, &canon, &|_| true).is_err());
        registered(&root, &canon);
        assert!(record_version(&root, &canon, &|_| true).unwrap());
        // Same content, same stamp: no new revision and no re-read.
        assert!(!record_version(&root, &canon, &|_| true).unwrap());
        // Touched with identical bytes: still one revision.
        fs::write(&file, "one").unwrap();
        assert!(!record_version(&root, &canon, &|_| true).unwrap());
        fs::write(&file, "two!").unwrap();
        assert!(record_version(&root, &canon, &|_| true).unwrap());
        let v = versions_of(&root, &canon);
        assert_eq!(v.iter().map(|v| v.version).collect::<Vec<_>>(), [1, 2]);
        assert_eq!(v[0].sha256, sha256_hex(b"one"));
        assert_eq!(v[0].sha256.len(), 64);
        assert_eq!((v[0].size_bytes, v[1].size_bytes), (3, 4));
        assert!(v[1].recorded_at_ms > 0);
        let reg = Registry::load(&root);
        let e = reg.find_for_path(&canon).unwrap();
        assert_eq!(read_version_bytes(&root, e, 1).unwrap(), b"one");
        assert_eq!(read_version_bytes(&root, e, 2).unwrap(), b"two!");
        assert_eq!(read_version_bytes(&root, e, 3), Err("version not found"));
    }

    #[test]
    fn versions_survive_an_upsert_and_are_trimmed_to_the_cap() {
        let tmp = TempDir::new().unwrap();
        let root = data_root(tmp.path());
        let file = tmp.path().join("a.md");
        let canon = {
            fs::write(&file, "0").unwrap();
            fs::canonicalize(&file).unwrap()
        };
        registered(&root, &canon);
        for i in 0..(MAX_VERSIONS_PER_ARTIFACT + 3) {
            fs::write(&file, format!("rev-{i}-{}", "x".repeat(i))).unwrap();
            assert!(record_version(&root, &canon, &|_| true).unwrap());
            // A re-scan upserts the entry again; history must stay.
            registered(&root, &canon);
        }
        let v = versions_of(&root, &canon);
        assert_eq!(v.len(), MAX_VERSIONS_PER_ARTIFACT);
        assert_eq!(v.first().unwrap().version, 4);
        assert_eq!(
            v.last().unwrap().version as usize,
            MAX_VERSIONS_PER_ARTIFACT + 3
        );
        // Trimmed revisions lose their snapshot file.
        let dir = versions_dir(&root, &canon);
        assert!(!dir.join("v1").exists() && dir.join("v4").exists());
    }

    #[test]
    fn oversize_revisions_keep_a_digest_but_no_snapshot_and_bad_snapshots_are_refused() {
        let tmp = TempDir::new().unwrap();
        let root = data_root(tmp.path());
        let file = tmp.path().join("big.pdf");
        fs::write(&file, vec![7u8; MAX_ARCHIVE_BYTES as usize + 10]).unwrap();
        let canon = fs::canonicalize(&file).unwrap();
        registered(&root, &canon);
        assert!(record_version(&root, &canon, &|_| true).unwrap());
        let v = versions_of(&root, &canon);
        assert_eq!(v.len(), 1);
        assert!(v[0].snapshot_path.is_none());
        let reg = Registry::load(&root);
        let e = reg.find_for_path(&canon).unwrap();
        assert_eq!(
            read_version_bytes(&root, e, 1),
            Err("version content was not stored")
        );

        // A registry that points a snapshot outside the versions dir is not trusted.
        let secret = tmp.path().join("secret.txt");
        fs::write(&secret, "secret").unwrap();
        let mut forged = e.clone();
        forged.versions[0].snapshot_path = Some(secret.display().to_string());
        assert_eq!(
            read_version_bytes(&root, &forged, 1),
            Err("version content is missing")
        );
        forged.versions[0].snapshot_path = Some(format!(
            "{}/../../secret.txt",
            versions_dir(&root, &canon).display()
        ));
        assert!(read_version_bytes(&root, &forged, 1).is_err());
    }

    #[test]
    fn record_version_refuses_a_symlink_and_a_failed_recheck() {
        let tmp = TempDir::new().unwrap();
        let root = data_root(tmp.path());
        let real = tmp.path().join("real.md");
        fs::write(&real, "x").unwrap();
        let link = tmp.path().join("link.md");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        registered(&root, &link);
        assert!(record_version(&root, &link, &|_| true).is_err());
        let canon = fs::canonicalize(&real).unwrap();
        registered(&root, &canon);
        assert!(record_version(&root, &canon, &|_| false).is_err());
        assert!(versions_of(&root, &canon).is_empty());
    }

    #[test]
    fn reconcile_prunes_snapshots_of_a_dropped_entry() {
        let tmp = TempDir::new().unwrap();
        let root = data_root(tmp.path());
        let file = tmp.path().join("a.md");
        fs::write(&file, "one").unwrap();
        let canon = fs::canonicalize(&file).unwrap();
        with_registry(&root, |reg| reg.upsert(entry(&canon, Some("s1"))));
        record_version(&root, &canon, &|_| true).unwrap();
        let snap = versions_dir(&root, &canon).join("v1");
        assert!(snap.exists());
        fs::remove_file(&file).unwrap();
        let out = with_registry(&root, |reg| reg.reconcile(&managed_dir(&root), None));
        assert_eq!(out.pruned, 1);
        assert!(!snap.exists());
    }

    #[test]
    fn old_registry_entries_without_versions_still_load() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("artifacts");
        fs::create_dir_all(&root).unwrap();
        let mut v = serde_json::to_value(entry(Path::new("/x/a.md"), None)).unwrap();
        v.as_object_mut().unwrap().remove("versions");
        fs::write(registry_path(&root), json!({ "entries": [v] }).to_string()).unwrap();
        let reg = Registry::load(&root);
        assert_eq!(reg.entries.len(), 1);
        assert!(reg.entries[0].versions.is_empty());
    }

    // ---- office previews ----

    /// Build a ZIP: each entry deflated when `deflate`, else stored. CRCs are zero (the reader
    /// does not check them).
    fn build_zip(entries: &[(&str, &[u8])], deflate: bool) -> Vec<u8> {
        use std::io::Write;
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, data) in entries {
            let (method, body): (u16, Vec<u8>) = if deflate {
                let mut enc =
                    flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
                enc.write_all(data).unwrap();
                (8, enc.finish().unwrap())
            } else {
                (0, data.to_vec())
            };
            let off = out.len() as u32;
            out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
            out.extend_from_slice(&[20, 0, 0, 0]);
            out.extend_from_slice(&method.to_le_bytes());
            out.extend_from_slice(&[0; 8]);
            out.extend_from_slice(&(body.len() as u32).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(&body);

            central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            central.extend_from_slice(&[20, 0, 20, 0, 0, 0]);
            central.extend_from_slice(&method.to_le_bytes());
            central.extend_from_slice(&[0; 8]);
            central.extend_from_slice(&(body.len() as u32).to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&[0; 12]);
            central.extend_from_slice(&off.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let cd_off = out.len() as u32;
        out.extend_from_slice(&central);
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(central.len() as u32).to_le_bytes());
        out.extend_from_slice(&cd_off.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    const DOCX: &str = r#"<?xml version="1.0"?><w:document xmlns:w="x"><w:body>
      <w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>Plan &amp; Scope</w:t></w:r></w:p>
      <w:p><w:r><w:t xml:space="preserve">Hello </w:t></w:r><w:r><w:t>world</w:t></w:r><w:r><w:tab/><w:t>&#65;&#x42;</w:t></w:r></w:p>
      <w:p><w:pPr><w:pStyle w:val="Heading3"/></w:pPr><w:r><w:t>Sub</w:t></w:r></w:p>
      <w:p><w:r><w:t></w:t></w:r></w:p>
      <w:tbl>
        <w:tr><w:tc><w:p><w:r><w:t>Name</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>Qty</w:t></w:r></w:p></w:tc></w:tr>
        <w:tr><w:tc><w:p><w:r><w:t>Bolt</w:t></w:r></w:p><w:p><w:r><w:t>M8</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>12</w:t></w:r></w:p></w:tc></w:tr>
      </w:tbl>
      <w:p><w:r><w:t>After</w:t></w:r></w:p>
    </w:body></w:document>"#;

    #[test]
    fn docx_preview_extracts_headings_paragraphs_and_tables_in_order() {
        for deflate in [true, false] {
            let zip = build_zip(&[("word/document.xml", DOCX.as_bytes())], deflate);
            let p = office_preview("docx", &zip).unwrap();
            assert_eq!(p["kind"], "docx");
            let blocks = p["blocks"].as_array().unwrap();
            assert_eq!(blocks.len(), 5, "{blocks:?}");
            assert_eq!(
                blocks[0],
                json!({"type":"heading","level":1,"text":"Plan & Scope"})
            );
            assert_eq!(
                blocks[1],
                json!({"type":"paragraph","text":"Hello world\tAB"})
            );
            assert_eq!(blocks[2], json!({"type":"heading","level":3,"text":"Sub"}));
            assert_eq!(
                blocks[3],
                json!({"type":"table","rows":[["Name","Qty"],["Bolt\nM8","12"]]})
            );
            assert_eq!(blocks[4], json!({"type":"paragraph","text":"After"}));
            assert_eq!(p["truncated"], false);
        }
    }

    #[test]
    fn docx_nested_tables_flatten_into_the_outer_cell() {
        let xml = r#"<w:document><w:body><w:tbl><w:tr><w:tc>
          <w:p><w:r><w:t>outer</w:t></w:r></w:p>
          <w:tbl><w:tr><w:tc><w:p><w:r><w:t>inner</w:t></w:r></w:p></w:tc></w:tr></w:tbl>
        </w:tc></w:tr></w:tbl></w:body></w:document>"#;
        let zip = build_zip(&[("word/document.xml", xml.as_bytes())], true);
        let p = office_preview("docx", &zip).unwrap();
        assert_eq!(
            p["blocks"],
            json!([{"type":"table","rows":[["outer\ninner"]]}])
        );
    }

    #[test]
    fn xlsx_preview_maps_sheets_by_relationship_and_reads_every_cell_type() {
        let workbook = r#"<workbook xmlns:r="r"><sheets>
          <sheet name="Totals" sheetId="2" r:id="rId2"/>
          <sheet name="Data &amp; More" sheetId="1" r:id="rId1"/></sheets></workbook>"#;
        let rels = r#"<Relationships>
          <Relationship Id="rId1" Type="t/worksheet" Target="worksheets/sheet1.xml"/>
          <Relationship Id="rId2" Type="t/worksheet" Target="/xl/worksheets/sheet2.xml"/></Relationships>"#;
        let strings = r#"<sst><si><t>Item</t></si><si><r><t>Rich</t></r><r><t> text</t></r><rPh><t>skip</t></rPh></si></sst>"#;
        let sheet1 = r#"<worksheet><sheetData>
          <row r="1"><c r="A1" t="s"><v>0</v></c><c r="C1" t="s"><v>1</v></c></row>
          <row r="3"><c r="B3"><v>42.5</v></c><c r="D3" t="b"><v>1</v></c><c r="E3" t="inlineStr"><is><t>inline</t></is></c></row>
        </sheetData></worksheet>"#;
        let sheet2 = r#"<worksheet><sheetData><row r="1"><c r="A1" t="str"><v>sum</v></c></row></sheetData></worksheet>"#;
        let zip = build_zip(
            &[
                ("xl/workbook.xml", workbook.as_bytes()),
                ("xl/_rels/workbook.xml.rels", rels.as_bytes()),
                ("xl/sharedStrings.xml", strings.as_bytes()),
                ("xl/worksheets/sheet1.xml", sheet1.as_bytes()),
                ("xl/worksheets/sheet2.xml", sheet2.as_bytes()),
            ],
            true,
        );
        let p = office_preview("xlsx", &zip).unwrap();
        let sheets = p["sheets"].as_array().unwrap();
        assert_eq!(sheets.len(), 2);
        assert_eq!(sheets[0]["name"], "Totals");
        assert_eq!(sheets[0]["rows"], json!([["sum"]]));
        assert_eq!(sheets[1]["name"], "Data & More");
        assert_eq!(
            sheets[1]["rows"],
            json!([
                ["Item", "", "Rich text"],
                [],
                ["", "42.5", "", "TRUE", "inline"]
            ])
        );
        assert_eq!(
            (
                sheets[1]["row_count"].clone(),
                sheets[1]["col_count"].clone()
            ),
            (json!(3), json!(5))
        );
    }

    #[test]
    fn xlsx_without_relationships_falls_back_to_sheet_order_and_caps_huge_references() {
        let workbook = r#"<workbook><sheets><sheet name="One" sheetId="1"/></sheets></workbook>"#;
        let sheet = r#"<worksheet><sheetData><row r="1"><c r="A1"><v>1</v></c><c r="XFD1048576"><v>9</v></c></row>
          <row r="1048576"><c r="A1"><v>8</v></c></row></sheetData></worksheet>"#;
        let zip = build_zip(
            &[
                ("xl/workbook.xml", workbook.as_bytes()),
                ("xl/worksheets/sheet1.xml", sheet.as_bytes()),
            ],
            true,
        );
        let p = office_preview("xlsx", &zip).unwrap();
        assert_eq!(p["sheets"][0]["rows"], json!([["1"]]));
        assert_eq!(p["sheets"][0]["truncated"], true);
        assert_eq!(p["truncated"], true);
    }

    #[test]
    fn pptx_preview_orders_slides_and_pairs_titles_bullets_and_notes() {
        let slide = |title: &str, bullets: &[&str]| {
            let paras: String = bullets
                .iter()
                .map(|b| format!("<a:p><a:r><a:t>{b}</a:t></a:r></a:p>"))
                .collect();
            format!(
                r#"<p:sld><p:cSld><p:spTree>
                <p:sp><p:nvSpPr><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>{title}</a:t></a:r></a:p></p:txBody></p:sp>
                <p:sp><p:nvSpPr><p:nvPr><p:ph idx="1"/></p:nvPr></p:nvSpPr><p:txBody>{paras}</p:txBody></p:sp>
                <p:sp><p:nvSpPr><p:nvPr><p:ph type="sldNum"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>7</a:t></a:r></a:p></p:txBody></p:sp>
                </p:spTree></p:cSld></p:sld>"#
            )
        };
        let s1 = slide("Intro", &["first", "second"]);
        let s2 = slide("Wrap up", &["done"]);
        let s10 = slide("Appendix", &[]);
        let notes = r#"<p:notes><p:cSld><p:spTree>
          <p:sp><p:nvSpPr><p:nvPr><p:ph type="sldImg"/></p:nvPr></p:nvSpPr></p:sp>
          <p:sp><p:nvSpPr><p:nvPr><p:ph type="body" idx="1"/></p:nvPr></p:nvSpPr><p:txBody>
            <a:p><a:r><a:t>Say hi</a:t></a:r></a:p><a:p><a:r><a:t>Smile</a:t></a:r></a:p></p:txBody></p:sp>
          </p:spTree></p:cSld></p:notes>"#;
        let rels2 = r#"<Relationships><Relationship Id="r1" Type="http://x/relationships/notesSlide" Target="../notesSlides/notesSlide9.xml"/></Relationships>"#;
        let zip = build_zip(
            &[
                ("ppt/presentation.xml", b"<p:presentation/>"),
                ("ppt/slides/slide10.xml", s10.as_bytes()),
                ("ppt/slides/slide2.xml", s2.as_bytes()),
                ("ppt/slides/slide1.xml", s1.as_bytes()),
                ("ppt/slides/_rels/slide2.xml.rels", rels2.as_bytes()),
                ("ppt/notesSlides/notesSlide9.xml", notes.as_bytes()),
            ],
            true,
        );
        let p = office_preview("pptx", &zip).unwrap();
        let slides = p["slides"].as_array().unwrap();
        let titles: Vec<_> = slides
            .iter()
            .map(|s| s["title"].as_str().unwrap())
            .collect();
        assert_eq!(titles, ["Intro", "Wrap up", "Appendix"]);
        assert_eq!(slides[0]["bullets"], json!(["first", "second"]));
        assert_eq!(slides[0]["notes"], "");
        assert_eq!(slides[1]["bullets"], json!(["done"]));
        assert_eq!(slides[1]["notes"], "Say hi\nSmile");
        assert_eq!(slides[2]["index"], 3);
    }

    #[test]
    fn pptx_falls_back_to_same_numbered_notes_without_a_relationship() {
        let slide = r#"<p:sld><p:sp><p:nvSpPr><p:nvPr><p:ph type="ctrTitle"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>T</a:t></a:r></a:p></p:txBody></p:sp></p:sld>"#;
        let notes = r#"<p:notes><p:sp><p:nvSpPr><p:nvPr><p:ph type="body"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>N</a:t></a:r></a:p></p:txBody></p:sp></p:notes>"#;
        let zip = build_zip(
            &[
                ("ppt/presentation.xml", b"<p/>"),
                ("ppt/slides/slide1.xml", slide.as_bytes()),
                ("ppt/notesSlides/notesSlide1.xml", notes.as_bytes()),
            ],
            false,
        );
        let p = office_preview("pptx", &zip).unwrap();
        assert_eq!(p["slides"][0]["title"], "T");
        assert_eq!(p["slides"][0]["notes"], "N");
    }

    #[test]
    fn office_preview_rejects_wrong_package_shapes() {
        assert!(office_preview("docx", b"not a zip at all, nope nope").is_err());
        assert!(office_preview("docx", b"").is_err());
        let other = build_zip(&[("hello.txt", b"hi")], true);
        for ext in ["docx", "xlsx", "pptx"] {
            let err = office_preview(ext, &other).unwrap_err();
            assert!(err.0.contains("missing"), "{ext}: {err}");
        }
        assert!(office_preview("doc", &other).is_err());
    }

    #[test]
    fn zip_limits_stop_entry_floods_zip_bombs_and_encrypted_entries() {
        // Too many entries.
        let names: Vec<String> = (0..MAX_OFFICE_ENTRIES + 1)
            .map(|i| format!("f{i}"))
            .collect();
        let entries: Vec<(&str, &[u8])> = names.iter().map(|n| (n.as_str(), &b"x"[..])).collect();
        let err = ZipArchive::parse(&build_zip(&entries, false))
            .err()
            .unwrap();
        assert!(err.0.contains("256"), "{err}");
        // Exactly at the entry limit is fine.
        assert!(ZipArchive::parse(&build_zip(&entries[..MAX_OFFICE_ENTRIES], false)).is_ok());

        // Declared uncompressed sizes above 64 MiB in total.
        let mut zip = build_zip(&[("a", b"x"), ("b", b"y")], false);
        let cd = u32_at(&zip, zip.len() - 22 + 16).unwrap() as usize;
        let huge = (40u32 * 1024 * 1024).to_le_bytes();
        zip[cd + 24..cd + 28].copy_from_slice(&huge);
        let second = cd + 46 + 1;
        zip[second + 24..second + 28].copy_from_slice(&huge);
        let err = ZipArchive::parse(&zip).err().unwrap();
        assert!(err.0.contains("64 MiB"), "{err}");

        // A deflate stream that inflates past what the directory declares.
        let big = vec![0u8; 100_000];
        let mut zip = build_zip(&[("word/document.xml", &big)], true);
        let cd = u32_at(&zip, zip.len() - 22 + 16).unwrap() as usize;
        zip[cd + 24..cd + 28].copy_from_slice(&10u32.to_le_bytes());
        let a = ZipArchive::parse(&zip).unwrap();
        let mut budget = MAX_OFFICE_UNCOMPRESSED_BYTES;
        assert!(a.read("word/document.xml", &mut budget).is_err());

        // The shared budget runs out across entries.
        let small = build_zip(&[("a", &[1u8; 100])], true);
        let a = ZipArchive::parse(&small).unwrap();
        let mut budget = 50;
        assert!(a.read("a", &mut budget).is_err());
        let mut budget = 100;
        assert_eq!(a.read("a", &mut budget).unwrap().unwrap().len(), 100);
        assert_eq!(budget, 0);
        assert_eq!(a.read("missing", &mut budget).unwrap(), None);

        // Encrypted entry.
        let mut zip = build_zip(&[("a", b"x")], false);
        let cd = u32_at(&zip, zip.len() - 22 + 16).unwrap() as usize;
        zip[cd + 8] = 1;
        let err = ZipArchive::parse(&zip).err().unwrap();
        assert!(err.0.contains("encrypted"), "{err}");

        // Compressed size cap.
        let big = vec![0u8; MAX_OFFICE_COMPRESSED_BYTES as usize + 1];
        assert!(ZipArchive::parse(&big).is_err());
    }

    #[test]
    fn zip_parse_survives_truncated_and_corrupt_input() {
        let zip = build_zip(&[("word/document.xml", DOCX.as_bytes())], true);
        for cut in [0, 5, 21, 22, zip.len() / 2, zip.len() - 1] {
            let _ = office_preview("docx", &zip[..cut]);
        }
        // A local-header offset pointing past the end is an error, not a panic.
        let mut bad = zip.clone();
        let cd = u32_at(&bad, bad.len() - 22 + 16).unwrap() as usize;
        bad[cd + 42..cd + 46].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(office_preview("docx", &bad).is_err());
        // A comment-sized scan: EOCD found with trailing bytes after it.
        let mut with_comment = zip.clone();
        with_comment.extend_from_slice(&[0u8; 40]);
        assert!(office_preview("docx", &with_comment).is_ok());
    }

    #[test]
    fn xml_tokenizer_handles_cdata_comments_quotes_and_entities() {
        let toks = xml_tokens(
            r#"<?xml version="1.0"?><!DOCTYPE x><!-- <a> --><a:b k="x>y" j='1'>t &lt;&unknown;&#xZZ; <![CDATA[<raw>]]></a:b><c/>"#,
        );
        assert_eq!(
            toks[0],
            Xml::Start {
                name: "b".into(),
                attrs: r#"k="x>y" j='1'"#.into(),
                empty: false
            }
        );
        assert_eq!(toks[1], Xml::Text("t <&unknown;&#xZZ; ".into()));
        assert_eq!(toks[2], Xml::Text("<raw>".into()));
        assert_eq!(toks[3], Xml::End("b".into()));
        assert_eq!(
            toks[4],
            Xml::Start {
                name: "c".into(),
                attrs: String::new(),
                empty: true
            }
        );
        assert_eq!(xml_attr(r#"k="x>y" j='1'"#, "j").as_deref(), Some("1"));
        assert_eq!(xml_attr(r#"w:val="H1""#, "val").as_deref(), Some("H1"));
        assert_eq!(xml_attr("k=bare", "k"), None);
        // Unterminated input does not hang or panic.
        let _ = xml_tokens("<a b=\"unterminated");
        assert_eq!(column_index("AB12"), Some(27));
        assert_eq!(column_index("12"), None);
        assert_eq!(heading_level("heading 2"), Some(2));
        assert_eq!(heading_level("Heading7"), None);
        assert_eq!(
            resolve_target("ppt/slides", "../notesSlides/n.xml"),
            "ppt/notesSlides/n.xml"
        );
    }
}
