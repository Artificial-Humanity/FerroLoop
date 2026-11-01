//! The container I/O seam (spec §3.5).
//!
//! Every byte of `.fls` container I/O goes through [`ContainerIo`] (one open
//! file) and [`ContainerFs`] (the names in a directory). Production uses
//! [`File`] and [`RealFs`], which make the same system calls in the same
//! order as direct `std::fs` calls. The crash harness (`crash_sim`) puts a
//! simulated disk behind the same traits, so it can cut power after any one
//! operation.
//!
//! A new kind of container operation is added here first: a method on the
//! trait, the `File` or `RealFs` body, and the simulated body and its fault
//! model in `crash_sim`.

use fl_core::StoreError;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::Path;

/// One open container file.
pub(super) trait ContainerIo: Send + Sync {
    /// Write all of `buf` at `offset` (`pwrite`, repeated until done).
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()>;
    /// Fill all of `buf` from `offset` (`pread`, repeated until done).
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()>;
    /// `fdatasync`: the bytes written and the file size. Frames, snapshots
    /// and slot writes use it (spec §4.3).
    fn sync_data(&self) -> io::Result<()>;
    /// `fsync`: also the rest of the inode's metadata. Only first-use init,
    /// which sets the length of a new file, uses it.
    fn sync_all(&self) -> io::Result<()>;
    /// The file's length (`fstat`).
    fn len(&self) -> io::Result<u64>;
    /// `ftruncate`.
    fn set_len(&self, len: u64) -> io::Result<()>;
    /// `(dev, ino)` of the open file (`fstat`): which file this description
    /// reads, so a cache filled through it is not trusted for another file.
    fn file_id(&self) -> io::Result<(u64, u64)>;
    /// `flock(LOCK_EX)`: wait until this description holds the file's
    /// exclusive advisory lock.
    fn lock_exclusive(&self) -> io::Result<()>;
    /// `flock(LOCK_UN)`. Releasing cannot usefully fail: a lock this
    /// description does not hold is not held.
    fn unlock(&self);
}

const LOCK_EX: i32 = 2;
const LOCK_UN: i32 = 8;

// libc `flock`. No extra crate: the process already links libc.
unsafe extern "C" {
    fn flock(fd: i32, op: i32) -> i32;
}

impl ContainerIo for File {
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        FileExt::write_all_at(self, buf, offset)
    }

    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        FileExt::read_exact_at(self, buf, offset)
    }

    fn sync_data(&self) -> io::Result<()> {
        File::sync_data(self)
    }

    fn sync_all(&self) -> io::Result<()> {
        File::sync_all(self)
    }

    fn len(&self) -> io::Result<u64> {
        Ok(self.metadata()?.len())
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        File::set_len(self, len)
    }

    fn file_id(&self) -> io::Result<(u64, u64)> {
        let meta = self.metadata()?;
        Ok((meta.dev(), meta.ino()))
    }

    fn lock_exclusive(&self) -> io::Result<()> {
        let fd: RawFd = self.as_raw_fd();
        // SAFETY: `fd` is this open file's descriptor. `LOCK_EX` waits until
        // this description holds the advisory lock, or returns -1.
        if unsafe { flock(fd, LOCK_EX) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn unlock(&self) {
        // SAFETY: the descriptor is open for the life of `self`. `LOCK_UN`
        // releases the advisory lock held by this description.
        unsafe {
            flock(self.as_raw_fd(), LOCK_UN);
        }
    }
}

/// The directory side: making, opening, naming and syncing container files.
pub(super) trait ContainerFs: Send + Sync {
    /// Make the store file 0600 when it is missing (and its parent 0700 when
    /// that is missing too). `true` when this call created the file.
    fn create_store_file(&self, path: &Path) -> Result<bool, StoreError>;
    /// Open an existing file read-write.
    fn open_rw(&self, path: &Path) -> io::Result<Box<dyn ContainerIo>>;
    /// Open an existing file `O_RDONLY`.
    fn open_ro(&self, path: &Path) -> io::Result<Box<dyn ContainerIo>>;
    /// `fsync` a directory, so the entries made in it are durable.
    fn sync_dir(&self, dir: &Path) -> Result<(), StoreError>;
    /// `rename(2)`. Compaction (spec §5) is the first caller.
    #[cfg_attr(not(test), allow(dead_code, reason = "first caller: §5 compaction"))]
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    /// `link(2)`: a second name for `from`'s file. Refuses an existing `to`.
    #[cfg_attr(not(test), allow(dead_code, reason = "first caller: §5 compaction"))]
    fn link(&self, from: &Path, to: &Path) -> io::Result<()>;
    /// `unlink(2)`.
    #[cfg_attr(not(test), allow(dead_code, reason = "first caller: §5 compaction"))]
    fn unlink(&self, path: &Path) -> io::Result<()>;
    /// Report a store file whose mode is looser than 0600.
    fn warn_if_loose(&self, path: &Path);
}

/// The real file system.
pub(super) struct RealFs;

impl ContainerFs for RealFs {
    fn create_store_file(&self, path: &Path) -> Result<bool, StoreError> {
        super::create_store_file(path)
    }

    fn open_rw(&self, path: &Path) -> io::Result<Box<dyn ContainerIo>> {
        // Not `create(true)`: a file this open makes must come from
        // `create_private_file`, so it is never made with the umask's mode.
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        Ok(Box::new(file))
    }

    fn open_ro(&self, path: &Path) -> io::Result<Box<dyn ContainerIo>> {
        // `OpenOptions::read` without write is `O_RDONLY`.
        let file = OpenOptions::new().read(true).open(path)?;
        Ok(Box::new(file))
    }

    fn sync_dir(&self, dir: &Path) -> Result<(), StoreError> {
        super::sync_dir(dir)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }

    fn link(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::hard_link(from, to)
    }

    fn unlink(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }

    fn warn_if_loose(&self, path: &Path) {
        crate::private::warn_if_loose(path);
    }
}
