//! `install.lock`: one installer at a time.
//!
//! The lock is a held `flock(2)`. The kernel drops it when its holder dies, so a crash never
//! leaves a lock that needs breaking, and a live holder is never robbed: `flock` simply does
//! not grant it. The file also records the holder's pid, for the "another install is running
//! (pid N)" message and so that a record left by a process that no longer exists is reported
//! as a stale lock when the next installer takes it over.

use crate::error::{Ctx, InstallError, Result};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long between attempts while another installer holds the lock.
const POLL: Duration = Duration::from_millis(20);

/// The held lock. Dropping it (or the process dying) releases it.
#[derive(Debug)]
pub struct LockGuard {
    file: File,
    path: PathBuf,
    /// This transaction created the lock file.
    pub created: bool,
    /// A record left by a process that is no longer running was found and replaced.
    pub stale_pid: Option<u32>,
}

impl LockGuard {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Remove the lock file while holding it (rollback of a fresh root). Waiters that already
    /// opened it notice the inode no longer matches the path and start over.
    pub fn unlink(&self) -> std::io::Result<()> {
        match fs::remove_file(&self.path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        // SAFETY: the descriptor is open and owned by `self.file`.
        unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// Whether process `pid` exists (a process we may not signal still exists).
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    // SAFETY: signal 0 only checks that the process exists.
    let r = unsafe { libc::kill(pid as i32, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn read_pid(file: &mut File) -> Option<u32> {
    let mut s = String::new();
    file.seek(SeekFrom::Start(0)).ok()?;
    file.take(32).read_to_string(&mut s).ok()?;
    s.trim().parse().ok()
}

fn try_flock(file: &File) -> std::io::Result<bool> {
    // SAFETY: the descriptor is open and owned by `file`.
    let r = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if r == 0 {
        return Ok(true);
    }
    let e = std::io::Error::last_os_error();
    if e.raw_os_error() == Some(libc::EWOULDBLOCK) {
        Ok(false)
    } else {
        Err(e)
    }
}

/// Take the lock at `path`, waiting at most `timeout` (zero: try once). `prepare` runs before
/// every attempt: it makes the directory the lock lives in.
pub fn acquire(
    path: &Path,
    timeout: Duration,
    prepare: impl FnMut() -> Result<()>,
) -> Result<LockGuard> {
    acquire_with(path, timeout, prepare, || {})
}

/// [`acquire`], calling `after_open` between opening the file and locking it (tests use it to
/// unlink the file in exactly that window).
fn acquire_with(
    path: &Path,
    timeout: Duration,
    mut prepare: impl FnMut() -> Result<()>,
    mut after_open: impl FnMut(),
) -> Result<LockGuard> {
    let deadline = Instant::now() + timeout;
    loop {
        prepare()?;
        let mut created = false;
        let mut opts = OpenOptions::new();
        opts.read(true).write(true).mode(0o600);
        opts.custom_flags(libc::O_NOFOLLOW);
        let file = match opts.clone().create_new(true).open(path) {
            Ok(f) => {
                created = true;
                f
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => match opts.open(path) {
                Ok(f) => f,
                // Removed between the two opens: start over.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    return Err(InstallError::io(
                        format!("cannot open {}", path.display()),
                        e,
                    ));
                }
            },
            Err(e) => {
                return Err(InstallError::io(
                    format!("cannot create {}", path.display()),
                    e,
                ));
            }
        };
        let mut file = file;
        after_open();
        let meta = file
            .metadata()
            .ctx(|| format!("cannot stat {}", path.display()))?;
        if !meta.is_file() {
            return Err(InstallError::refused(format!(
                "{} is not a regular file",
                path.display()
            )));
        }
        if try_flock(&file).ctx(|| format!("cannot lock {}", path.display()))? {
            // The file we locked must still be the one at `path`.
            let same = fs::symlink_metadata(path)
                .map(|m| m.ino() == meta.ino() && m.dev() == meta.dev())
                .unwrap_or(false);
            if !same {
                continue;
            }
            let prev = read_pid(&mut file);
            let stale_pid = prev.filter(|p| *p != std::process::id() && !pid_alive(*p));
            file.set_len(0)
                .ctx(|| format!("cannot write {}", path.display()))?;
            file.seek(SeekFrom::Start(0))
                .ctx(|| format!("cannot write {}", path.display()))?;
            writeln!(file, "{}", std::process::id())
                .ctx(|| format!("cannot write {}", path.display()))?;
            return Ok(LockGuard {
                file,
                path: path.to_path_buf(),
                created,
                stale_pid,
            });
        }
        if Instant::now() >= deadline {
            return Err(InstallError::Busy {
                pid: read_pid(&mut file),
            });
        }
        std::thread::sleep(POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lock_path(d: &tempfile::TempDir) -> PathBuf {
        d.path().join("install.lock")
    }

    #[test]
    fn a_second_taker_is_refused_while_the_first_holds_it() {
        let d = tempfile::tempdir().unwrap();
        let p = lock_path(&d);
        let first = acquire(&p, Duration::ZERO, || Ok(())).unwrap();
        assert!(first.created);
        let err = acquire(&p, Duration::from_millis(100), || Ok(())).unwrap_err();
        match err {
            InstallError::Busy { pid } => assert_eq!(pid, Some(std::process::id())),
            other => panic!("{other}"),
        }
        drop(first);
        let again = acquire(&p, Duration::ZERO, || Ok(())).unwrap();
        assert!(!again.created, "the file stays between installs");
    }

    #[test]
    fn a_record_of_a_dead_process_is_reported_stale_and_taken_over() {
        let d = tempfile::tempdir().unwrap();
        let p = lock_path(&d);
        // A pid that cannot exist (above pid_max on Linux).
        fs::write(&p, "4194999\n").unwrap();
        assert!(!pid_alive(4_194_999));
        let g = acquire(&p, Duration::ZERO, || Ok(())).unwrap();
        assert_eq!(g.stale_pid, Some(4_194_999));
        assert_eq!(
            fs::read_to_string(&p).unwrap().trim(),
            std::process::id().to_string()
        );
    }

    #[test]
    fn a_live_pid_in_the_file_without_a_held_flock_is_not_a_lock() {
        // A held lock is a held flock; text in a file is a record, not a lock.
        let d = tempfile::tempdir().unwrap();
        let p = lock_path(&d);
        fs::write(&p, format!("{}\n", std::process::id())).unwrap();
        let g = acquire(&p, Duration::ZERO, || Ok(())).unwrap();
        assert_eq!(g.stale_pid, None);
    }

    #[test]
    fn a_lock_waits_for_the_holder_and_then_takes_over() {
        let d = tempfile::tempdir().unwrap();
        let p = lock_path(&d);
        let first = acquire(&p, Duration::ZERO, || Ok(())).unwrap();
        let p2 = p.clone();
        let t =
            std::thread::spawn(move || acquire(&p2, Duration::from_secs(5), || Ok(())).map(|_| ()));
        std::thread::sleep(Duration::from_millis(150));
        drop(first);
        t.join().unwrap().unwrap();
    }

    #[test]
    fn a_symlinked_lock_file_is_not_followed() {
        let d = tempfile::tempdir().unwrap();
        let target = d.path().join("target");
        fs::write(&target, "x").unwrap();
        let p = lock_path(&d);
        std::os::unix::fs::symlink(&target, &p).unwrap();
        assert!(acquire(&p, Duration::ZERO, || Ok(())).is_err());
        assert_eq!(fs::read_to_string(&target).unwrap(), "x");
    }

    #[test]
    fn an_unlinked_lock_file_is_recreated_by_a_waiter() {
        let d = tempfile::tempdir().unwrap();
        let p = lock_path(&d);
        let first = acquire(&p, Duration::ZERO, || Ok(())).unwrap();
        let p2 = p.clone();
        let t = std::thread::spawn(move || {
            acquire(&p2, Duration::from_secs(5), || Ok(())).map(|g| g.path().to_path_buf())
        });
        std::thread::sleep(Duration::from_millis(100));
        first.unlink().unwrap();
        drop(first);
        let got = t.join().unwrap().unwrap();
        assert_eq!(got, p);
        assert!(
            p.exists(),
            "the waiter made a fresh file and holds that one"
        );
    }

    #[test]
    fn a_file_unlinked_between_open_and_lock_is_not_a_lock() {
        let d = tempfile::tempdir().unwrap();
        let p = lock_path(&d);
        let first = acquire(&p, Duration::ZERO, || Ok(())).unwrap();
        let mut first = Some(first);
        let mut calls = 0;
        // The waiter has opened the file; before it locks, the holder unlinks and releases it.
        let g = acquire_with(
            &p,
            Duration::from_secs(5),
            || Ok(()),
            || {
                calls += 1;
                if let Some(f) = first.take() {
                    f.unlink().unwrap();
                    drop(f);
                }
            },
        )
        .unwrap();
        assert!(calls >= 2, "the waiter started over ({calls})");
        assert!(p.exists(), "it holds the file at the path, not an orphan");
        drop(g);
    }
}
