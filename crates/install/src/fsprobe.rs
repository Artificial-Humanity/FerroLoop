//! The real, read-only [`HostProbe`]: `lstat`, `readlink`, and a bounded read to hash a file.
//! Shared by `fl install --dry-run` and `fl doctor`. Nothing here writes.

use fl_core::install::{BUDGET_EXCEEDED, HostProbe, MAX_FINGERPRINT_BYTES, PathState, fingerprint};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Open `path` to read it, without blocking and without following a symlink. A FIFO swapped in
/// since an earlier `lstat` would otherwise block the open, and a symlink would be followed.
fn open_nofollow(path: &Path) -> std::io::Result<fs::File> {
    let mut opts = fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    }
    opts.open(path)
}

/// Read at most `max` bytes of a regular file, from a descriptor opened `O_NONBLOCK|O_NOFOLLOW`
/// and checked with `fstat`, so nothing between an `lstat` and the open can swap in a FIFO or a
/// symlink. Refuses a symlink, a directory and anything larger. `Ok(None)`: the file does not
/// exist.
pub fn read_bounded(path: &Path, max: usize) -> Result<Option<String>, String> {
    let f = match open_nofollow(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            // O_NOFOLLOW on a symlink fails (ELOOP / EMLINK); say what it is.
            return Err(match fs::symlink_metadata(path) {
                Ok(m) if !m.file_type().is_file() => "is not a regular file".to_string(),
                _ => e.kind().to_string(),
            });
        }
    };
    let meta = f.metadata().map_err(|e| e.kind().to_string())?;
    if !meta.is_file() {
        return Err("is not a regular file".into());
    }
    if meta.len() > max as u64 {
        return Err(format!("is larger than {max} bytes"));
    }
    let mut buf = String::new();
    f.take(max as u64 + 1)
        .read_to_string(&mut buf)
        .map_err(|e| e.kind().to_string())?;
    if buf.len() > max {
        return Err(format!("is larger than {max} bytes"));
    }
    Ok(Some(buf))
}

/// A failure while reading a file the probe opened, in words that do not depend
/// on the platform's error text.
fn read_failure(e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::InvalidData => e.to_string(),
        k => k.to_string(),
    }
}

/// [`open_nofollow`], then check THAT descriptor is a regular file of bounded size. The `lstat`
/// that chose to open it may be stale. `Ok(None)`: not a regular file any more.
pub fn open_regular(path: &Path) -> Result<Option<fs::File>, String> {
    let f = open_nofollow(path).map_err(|e| e.kind().to_string())?;
    let meta = f.metadata().map_err(|e| e.kind().to_string())?;
    if !meta.is_file() {
        return Ok(None);
    }
    if meta.len() > MAX_FINGERPRINT_BYTES {
        return Err(format!("file is larger than {MAX_FINGERPRINT_BYTES} bytes"));
    }
    Ok(Some(f))
}

/// The real probe: `lstat`, `readlink`, and a read to hash a file. No write.
pub struct FsProbe {
    /// What a leading `~/` expands to.
    pub home: Option<PathBuf>,
    /// Past this instant every probe answers [`PathState::Unreadable`] with
    /// [`BUDGET_EXCEEDED`] instead of reading.
    pub deadline: Option<Instant>,
}

impl FsProbe {
    pub fn new(home: Option<PathBuf>) -> Self {
        Self {
            home,
            deadline: None,
        }
    }

    /// `$HOME` (unset or empty: none).
    pub fn from_env() -> Self {
        Self::new(
            std::env::var_os("HOME")
                .filter(|h| !h.is_empty())
                .map(PathBuf::from),
        )
    }

    pub fn with_deadline(mut self, deadline: Instant) -> Self {
        self.deadline = Some(deadline);
        self
    }

    /// The real path of a displayed one: a leading `~/` is the home directory.
    pub fn resolve(&self, shown: &str) -> Result<PathBuf, String> {
        match shown.strip_prefix("~/") {
            Some(rest) => self
                .home
                .as_ref()
                .map(|h| h.join(rest))
                .ok_or_else(|| "HOME is not set".to_string()),
            None => Ok(PathBuf::from(shown)),
        }
    }
}

impl HostProbe for FsProbe {
    fn expand(&self, shown: &str) -> String {
        self.resolve(shown)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| shown.to_string())
    }

    fn probe(&self, shown: &str) -> PathState {
        if self.deadline.is_some_and(|d| Instant::now() >= d) {
            return PathState::Unreadable {
                reason: BUDGET_EXCEEDED.into(),
            };
        }
        let path = match self.resolve(shown) {
            Ok(p) => p,
            Err(reason) => return PathState::Unreadable { reason },
        };
        let meta = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return PathState::Missing,
            Err(e) => {
                return PathState::Unreadable {
                    reason: e.kind().to_string(),
                };
            }
        };
        let ft = meta.file_type();
        if ft.is_symlink() {
            match fs::read_link(&path) {
                Ok(t) => PathState::Symlink {
                    target: t.to_string_lossy().into_owned(),
                },
                Err(e) => PathState::Unreadable {
                    reason: e.kind().to_string(),
                },
            }
        } else if ft.is_dir() {
            PathState::Dir
        } else if ft.is_file() {
            match open_regular(&path) {
                Ok(Some(f)) => match fingerprint(f) {
                    Ok(fp) => PathState::File(fp),
                    Err(e) => PathState::Unreadable {
                        reason: read_failure(&e),
                    },
                },
                // Replaced by a non-file between the lstat and the open.
                Ok(None) => PathState::Other,
                Err(reason) => PathState::Unreadable { reason },
            }
        } else {
            PathState::Other
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fifo_is_neither_opened_blocking_nor_read() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("pipe");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        // The open returns at once (O_NONBLOCK) and the descriptor is not a file.
        assert!(matches!(open_regular(&fifo), Ok(None)));
        let probe = FsProbe::new(None);
        assert_eq!(probe.probe(fifo.to_str().unwrap()), PathState::Other);
    }

    #[test]
    fn a_symlink_swapped_in_after_the_lstat_is_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real");
        fs::write(&target, "x").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(open_regular(&link).is_err(), "O_NOFOLLOW must refuse it");
    }

    #[test]
    fn an_oversized_file_is_reported_unreadable_without_being_read() {
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("big");
        let f = fs::File::create(&big).unwrap();
        f.set_len(MAX_FINGERPRINT_BYTES + 1).unwrap(); // sparse: no data is written
        let probe = FsProbe::new(None);
        match probe.probe(big.to_str().unwrap()) {
            PathState::Unreadable { reason } => assert!(reason.contains("larger than"), "{reason}"),
            other => panic!("{other:?}"),
        }
        let ok = dir.path().join("ok");
        fs::write(&ok, "hello").unwrap();
        assert!(matches!(
            probe.probe(ok.to_str().unwrap()),
            PathState::File(_)
        ));
    }

    #[test]
    fn a_probe_past_its_deadline_reads_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let ok = dir.path().join("ok");
        fs::write(&ok, "hello").unwrap();
        let probe = FsProbe::new(None).with_deadline(Instant::now());
        assert_eq!(
            probe.probe(ok.to_str().unwrap()),
            PathState::Unreadable {
                reason: BUDGET_EXCEEDED.into()
            }
        );
    }
}
