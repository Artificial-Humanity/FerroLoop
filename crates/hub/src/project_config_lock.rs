//! Serialize local project mutations across Hub and CLI processes.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};

use crate::project_init::{InitError, conflict, failed};

static LOCAL_CONFIG_LOCKS: Mutex<BTreeSet<PathBuf>> = Mutex::new(BTreeSet::new());
static CONFIG_RELEASED: Condvar = Condvar::new();

fn busy() -> InitError {
    conflict("project configuration is busy; retry")
}

/// Lock the config namespace, whose file is replaced on each write. A config symlink uses
/// its target's namespace. Keep the empty sidecar so separate openers always lock one inode.
fn lock_path(config: &Path) -> io::Result<PathBuf> {
    let target = match fs::symlink_metadata(config) {
        Ok(_) => config.canonicalize()?,
        Err(err) if err.kind() == io::ErrorKind::NotFound => config.to_path_buf(),
        Err(err) => return Err(err),
    };
    let dir = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| io::Error::other("config has no directory"))?;
    fl_store::private::create_private_dir_all(dir)?;
    let mut name = target
        .file_name()
        .ok_or_else(|| io::Error::other("config has no file name"))?
        .to_os_string();
    name.push(".projects.lock");
    Ok(dir.canonicalize()?.join(name))
}

fn open_lock(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    let opened = file.metadata()?;
    let named = fs::symlink_metadata(path)?;
    if !opened.is_file() || !named.is_file() {
        return Err(io::Error::other(
            "requires a regular lock file without symlinks",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if opened.uid() != unsafe { libc::geteuid() }
            || opened.mode() & 0o777 != 0o600
            || opened.nlink() != 1
            || opened.dev() != named.dev()
            || opened.ino() != named.ino()
        {
            return Err(io::Error::other("requires one user-owned 0600 lock inode"));
        }
    }
    Ok(file)
}

// The registry mutex only protects membership. The fenced path never waits for
// an edit to finish, and edits to independent configurations never conflict.
struct LocalConfigGuard(PathBuf);

impl LocalConfigGuard {
    fn acquire(path: PathBuf, wait: bool) -> Result<Self, InitError> {
        let mut held = LOCAL_CONFIG_LOCKS.lock().unwrap_or_else(|e| e.into_inner());
        while held.contains(&path) {
            if !wait {
                return Err(busy());
            }
            held = CONFIG_RELEASED
                .wait(held)
                .unwrap_or_else(|e| e.into_inner());
        }
        held.insert(path.clone());
        Ok(Self(path))
    }
}

impl Drop for LocalConfigGuard {
    fn drop(&mut self) {
        LOCAL_CONFIG_LOCKS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.0);
        CONFIG_RELEASED.notify_all();
    }
}

/// Hold for the entire configuration or project topology mutation.
pub struct ProjectConfigGuard {
    file: File,
    _local: LocalConfigGuard,
}

impl Drop for ProjectConfigGuard {
    fn drop(&mut self) {
        // Closing alone could leave a fork-inherited descriptor holding the lock until exec.
        let _ = self.file.unlock();
    }
}

pub(crate) fn acquire(config: &Path, wait_local: bool) -> Result<ProjectConfigGuard, InitError> {
    let io = |err| {
        failed(format!(
            "project configuration lock for {}: {err}",
            config.display()
        ))
    };
    let lock_path = lock_path(config).map_err(io)?;
    let local = LocalConfigGuard::acquire(lock_path, wait_local)?;
    let file = open_lock(&local.0).map_err(io)?;
    match file.try_lock() {
        Ok(()) => Ok(ProjectConfigGuard {
            file,
            _local: local,
        }),
        Err(TryLockError::WouldBlock) => Err(busy()),
        Err(TryLockError::Error(err)) => Err(io(err)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn independent_project_configurations_do_not_contend() {
        let dir = tempfile::tempdir().unwrap();
        let one = dir.path().join("one/config.toml");
        let two = dir.path().join("two/config.toml");
        let _one = acquire(&one, true).unwrap();
        assert!(matches!(acquire(&one, false), Err(InitError::Conflict(_))));
        let _two = acquire(&two, false).unwrap();
    }

    #[test]
    fn dropping_the_project_config_guard_releases_other_descriptor_references() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original = acquire(&path, true).unwrap();
        let inherited = original.file.try_clone().unwrap();
        drop(original);
        let _next = acquire(&path, false).unwrap();
        drop(inherited);
    }
}
