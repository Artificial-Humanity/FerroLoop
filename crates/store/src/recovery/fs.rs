//! Linux handle-relative recovery storage. Hooks intercept the actual I/O; they
//! never replace logical verification or authorize following a supplied symlink.
use super::{
    OperationLimits, RecoveryError,
    verify::{self, PrivateFile},
};
use sha2::{Digest, Sha256};
use std::{
    ffi::{CStr, CString},
    fs::File,
    io::{self, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{FileExt, MetadataExt},
    },
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Member {
    Image,
    VerificationCopy,
    Validation,
    Report,
    Manifest,
    Profile,
    IndexConfig,
    Config,
    Warrants,
    VaultCiphertext,
    Receipt,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Directory {
    Initialization,
    StagingBeforeVerify,
    StagingBeforePublish,
    PublicationParent,
    StagingParent,
    CatalogParent,
    QuarantineParent,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryEvent {
    Capture,
    Create(Member),
    Write(Member),
    Flush(Member),
    Sync(Directory),
    Publish,
    CatalogPublish,
    RemoveVerifier,
}
/// Implementations may fail or shorten actual destination writes. The default
/// implementation uses native operations; successful hooks alone prove nothing.
pub trait RecoveryIo: Send + Sync {
    fn before(&self, _: RecoveryEvent) -> io::Result<()> {
        Ok(())
    }
    fn write_at(&self, file: &File, bytes: &[u8], offset: u64) -> io::Result<usize> {
        file.write_at(bytes, offset)
    }
    fn available_bytes(&self, root: &File) -> io::Result<u64> {
        let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        // SAFETY: valid owned descriptor and writable statvfs storage.
        if unsafe { libc::fstatvfs(root.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fstatvfs initialized the complete structure.
        let stat = unsafe { stat.assume_init() };
        stat.f_bavail
            .checked_mul(stat.f_frsize)
            .ok_or_else(|| io::Error::other("capacity overflow"))
    }
}
pub struct NativeRecoveryIo;
impl RecoveryIo for NativeRecoveryIo {}
/// One administratively configured, private, approved root. A request may not
/// select another root. Reconciliation has explicit bounds and no source handle.
pub trait RecoveryFs: Send + Sync {
    fn root(&self) -> &Path;
    fn io(&self) -> Arc<dyn RecoveryIo>;
    fn reconciliation_limits(&self) -> &OperationLimits;
}
pub struct NativeRecoveryFs {
    root: PathBuf,
    limits: OperationLimits,
}
impl NativeRecoveryFs {
    pub fn new(approved_root: PathBuf, reconciliation_limits: OperationLimits) -> Self {
        Self {
            root: approved_root,
            limits: reconciliation_limits,
        }
    }
}
impl RecoveryFs for NativeRecoveryFs {
    fn root(&self) -> &Path {
        &self.root
    }
    fn io(&self) -> Arc<dyn RecoveryIo> {
        Arc::new(NativeRecoveryIo)
    }
    fn reconciliation_limits(&self) -> &OperationLimits {
        &self.limits
    }
}
/// P0 allocation proof is restricted to native ext4/XFS with one consistent
/// power-of-two allocation unit. Reflinks, sparse-file promises and speculative
/// future free space are never admission credits. Existing shared directories
/// must fit one unit; larger catalogs require a separately bounded policy.
pub(crate) fn allocation_geometry(root: &Dir, l: &OperationLimits) -> Result<u64, RecoveryError> {
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    let mut kind = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: live directory descriptor and writable result storage.
    if unsafe { libc::fstatvfs(root.file.as_raw_fd(), stat.as_mut_ptr()) } != 0
        || unsafe { libc::fstatfs(root.file.as_raw_fd(), kind.as_mut_ptr()) } != 0
    {
        return Err(destination(l));
    }
    let stat = unsafe { stat.assume_init() };
    let kind = unsafe { kind.assume_init() };
    let unit = stat.f_frsize;
    if !matches!(kind.f_type as u64, 0xef53 | 0x58465342)
        || !unit.is_power_of_two()
        || !(512..=65536).contains(&unit)
        || stat.f_bsize != unit
    {
        return Err(RecoveryError::Unsupported {
            operation_id: l.operation_id,
            reason: "bounded allocation geometry requires native ext4 or XFS",
        });
    }
    directory_geometry(root, unit, l)?;
    Ok(unit)
}
pub(crate) fn directory_geometry(
    dir: &Dir,
    unit: u64,
    l: &OperationLimits,
) -> Result<(), RecoveryError> {
    let m = dir.file.metadata().map_err(|_| destination(l))?;
    if m.len() > unit || m.blocks().checked_mul(512).is_none_or(|n| n > unit) {
        return Err(RecoveryError::Unsupported {
            operation_id: l.operation_id,
            reason: "directory exceeds admitted small-directory geometry",
        });
    }
    Ok(())
}
/// Six directory growth blocks, sixteen member rounding blocks, two lock
/// entries/files and two catalog receipt/temp rounding blocks. File payloads
/// are charged separately. No operation allocates beyond these member counts.
pub(crate) fn overhead_bytes(unit: u64, l: &OperationLimits) -> Result<u64, RecoveryError> {
    unit.checked_mul(26).ok_or_else(|| destination(l))
}
pub(crate) fn destination(l: &OperationLimits) -> RecoveryError {
    RecoveryError::Destination {
        operation_id: l.operation_id,
    }
}
pub(crate) fn refused(l: &OperationLimits, reason: &'static str) -> RecoveryError {
    RecoveryError::RefusedInput {
        operation_id: l.operation_id,
        reason,
    }
}
pub(crate) fn event(
    io: &dyn RecoveryIo,
    e: RecoveryEvent,
    l: &OperationLimits,
) -> Result<(), RecoveryError> {
    l.check()?;
    io.before(e).map_err(|_| destination(l))?;
    l.check()
}
fn component(name: &str, l: &OperationLimits) -> Result<CString, RecoveryError> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') {
        return Err(refused(l, "single component required"));
    }
    CString::new(name).map_err(|_| refused(l, "single component required"))
}
pub(crate) fn identity(f: &File, l: &OperationLimits) -> Result<(u64, u64), RecoveryError> {
    let m = f.metadata().map_err(|_| destination(l))?;
    Ok((m.dev(), m.ino()))
}
pub(crate) struct Dir {
    pub file: File,
    pub path: PathBuf,
}
impl Dir {
    pub fn open(path: &Path, l: &OperationLimits) -> Result<Self, RecoveryError> {
        if !cfg!(target_os = "linux") {
            return Err(RecoveryError::Unsupported {
                operation_id: l.operation_id,
                reason: "native durable backup publication unsupported",
            });
        }
        let (file, _) = verify::private_parent(&path.join(".recovery-anchor"), l)?;
        Ok(Self {
            file,
            path: path.to_path_buf(),
        })
    }
    pub fn same_name(&self, l: &OperationLimits) -> Result<(), RecoveryError> {
        let now = Self::open(&self.path, l)?;
        if identity(&now.file, l)? != identity(&self.file, l)? {
            return Err(l.verification());
        }
        Ok(())
    }
    pub fn child(
        &self,
        name: &str,
        exclusive: bool,
        l: &OperationLimits,
    ) -> Result<Self, RecoveryError> {
        self.same_name(l)?;
        let name_c = component(name, l)?;
        // SAFETY: owned parent fd and one validated component.
        let result = unsafe { libc::mkdirat(self.file.as_raw_fd(), name_c.as_ptr(), 0o700) };
        if result != 0
            && (exclusive || io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST))
        {
            return Err(destination(l));
        }
        let child = Self::open(&self.path.join(name), l)?;
        if child.file.metadata().map_err(|_| destination(l))?.dev()
            != self.file.metadata().map_err(|_| destination(l))?.dev()
        {
            return Err(refused(l, "same publication filesystem required"));
        }
        Ok(child)
    }
    pub fn exists(&self, name: &str, l: &OperationLimits) -> Result<bool, RecoveryError> {
        let name = component(name, l)?;
        let mut m = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: valid directory, component and writable stat storage. No follow.
        let r = unsafe {
            libc::fstatat(
                self.file.as_raw_fd(),
                name.as_ptr(),
                m.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if r == 0 {
            Ok(true)
        } else if io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT) {
            Ok(false)
        } else {
            Err(destination(l))
        }
    }
    pub fn sync(
        &self,
        io: &dyn RecoveryIo,
        kind: Directory,
        l: &OperationLimits,
    ) -> Result<(), RecoveryError> {
        self.same_name(l)?;
        event(io, RecoveryEvent::Sync(kind), l)?;
        self.file.sync_all().map_err(|_| destination(l))?;
        l.check()
    }
    pub fn rename(
        &self,
        name: &str,
        to: &Dir,
        new: &str,
        io: &dyn RecoveryIo,
        kind: RecoveryEvent,
        l: &OperationLimits,
    ) -> Result<(), RecoveryError> {
        self.rename_checked(name, to, new, io, kind, l, || Ok(()))
    }
    #[allow(clippy::too_many_arguments)]
    pub fn rename_checked(
        &self,
        name: &str,
        to: &Dir,
        new: &str,
        io: &dyn RecoveryIo,
        kind: RecoveryEvent,
        l: &OperationLimits,
        validate: impl FnOnce() -> Result<(), RecoveryError>,
    ) -> Result<(), RecoveryError> {
        self.same_name(l)?;
        to.same_name(l)?;
        let old = component(name, l)?;
        let new = component(new, l)?;
        event(io, kind, l)?;
        self.same_name(l)?;
        to.same_name(l)?;
        validate()?;
        #[cfg(target_os = "linux")]
        // SAFETY: live directory descriptors, validated components; exclusive rename.
        let result = unsafe {
            libc::renameat2(
                self.file.as_raw_fd(),
                old.as_ptr(),
                to.file.as_raw_fd(),
                new.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        #[cfg(not(target_os = "linux"))]
        let result = -1;
        if result != 0 {
            return Err(destination(l));
        }
        Ok(())
    }
    pub fn remove_file(
        &self,
        name: &str,
        expected: (u64, u64),
        l: &OperationLimits,
    ) -> Result<(), RecoveryError> {
        self.same_name(l)?;
        let held = PrivateFile::open(&self.path.join(name), false, l)?;
        if held.identity() != expected {
            return Err(l.verification());
        }
        let name = component(name, l)?;
        // SAFETY: validated single component relative to held private directory.
        if unsafe { libc::unlinkat(self.file.as_raw_fd(), name.as_ptr(), 0) } != 0 {
            return Err(destination(l));
        }
        Ok(())
    }
    pub fn members(&self, l: &OperationLimits) -> Result<Vec<String>, RecoveryError> {
        self.same_name(l)?;
        // SAFETY: duplicate is handed to fdopendir, which takes ownership on success.
        let fd = unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                c".".as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(destination(l));
        }
        // SAFETY: fd is a newly owned directory descriptor.
        let stream = unsafe { libc::fdopendir(fd) };
        if stream.is_null() {
            unsafe { libc::close(fd) };
            return Err(destination(l));
        }
        struct Close(*mut libc::DIR);
        impl Drop for Close {
            fn drop(&mut self) {
                unsafe { libc::closedir(self.0) };
            }
        }
        let _close = Close(stream);
        let mut names = Vec::new();
        loop {
            l.check()?;
            // SAFETY: stream remains valid and is used only by this call.
            #[cfg(target_os = "linux")]
            unsafe {
                *libc::__errno_location() = 0;
            }
            let entry = unsafe { libc::readdir(stream) };
            if entry.is_null() {
                #[cfg(target_os = "linux")]
                if unsafe { *libc::__errno_location() } != 0 {
                    return Err(destination(l));
                }
                break;
            }
            // SAFETY: readdir returns a NUL-terminated d_name valid until next call.
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }
                .to_str()
                .map_err(|_| l.verification())?;
            if name == "." || name == ".." {
                continue;
            }
            // A bundle has at most ten members; bound hostile directory enumeration.
            if names.len() >= 16 {
                return Err(l.verification());
            }
            names.push(name.to_owned());
        }
        names.sort();
        Ok(names)
    }
}
pub(crate) fn source_file(path: &Path, l: &OperationLimits) -> Result<File, RecoveryError> {
    let (parent, name) = verify::walk_parent(path, false, l)?;
    // SAFETY: existing directory and validated final component; no symlink follow.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(refused(l, "existing unaliased source required"));
    }
    // SAFETY: openat returned a new owned fd.
    let file = unsafe { File::from_raw_fd(fd) };
    let m = file.metadata().map_err(|_| destination(l))?;
    if !m.is_file() || m.nlink() != 1 || m.uid() != unsafe { libc::geteuid() } {
        return Err(refused(l, "existing unaliased source required"));
    }
    Ok(file)
}
pub(crate) struct StoreLock {
    _file: File,
}
impl StoreLock {
    pub fn acquire(
        dir: &Dir,
        physical: (u64, u64),
        l: &OperationLimits,
    ) -> Result<Self, RecoveryError> {
        Self::named(dir, &format!("{}:{}.lock", physical.0, physical.1), l)
    }
    pub fn operation(
        dir: &Dir,
        operation: uuid::Uuid,
        l: &OperationLimits,
    ) -> Result<Self, RecoveryError> {
        Self::named(dir, &format!("operation-{operation}.lock"), l)
    }
    fn named(dir: &Dir, name: &str, l: &OperationLimits) -> Result<Self, RecoveryError> {
        let name = component(name, l)?;
        dir.same_name(l)?;
        // SAFETY: held directory, validated single component, no-follow create/open.
        let fd = unsafe {
            libc::openat(
                dir.file.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDWR
                    | libc::O_CREAT
                    | libc::O_NOFOLLOW
                    | libc::O_NONBLOCK
                    | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0 {
            return Err(destination(l));
        }
        let file = unsafe { File::from_raw_fd(fd) };
        verify::privacy(&file, false, l)?;
        // SAFETY: live descriptor; flock changes no source bytes or source locks.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(RecoveryError::ConcurrentBackup {
                operation_id: l.operation_id,
            });
        }
        // Lock files contain no paths, payloads or persistent ownership claims.
        Ok(Self { _file: file })
    }
}
/// Cumulative allocation is deliberately not refunded when the verification copy
/// is removed. Both full images, all metadata and the receipt use one ceiling.
pub(crate) struct PublicationIo {
    pub io: Arc<dyn RecoveryIo>,
    total: AtomicU64,
    metadata: AtomicU64,
    pub cap: u64,
    metadata_cap: u64,
    pub image_cap: u64,
    unit: u64,
}
impl std::fmt::Debug for PublicationIo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PublicationIo")
            .field("cap", &self.cap)
            .finish_non_exhaustive()
    }
}
impl PublicationIo {
    pub fn new(
        io: Arc<dyn RecoveryIo>,
        cap: u64,
        metadata_cap: u64,
        image_cap: u64,
        unit: u64,
    ) -> Self {
        Self {
            io,
            total: AtomicU64::new(0),
            metadata: AtomicU64::new(0),
            cap,
            metadata_cap,
            image_cap,
            unit,
        }
    }
    fn add(
        counter: &AtomicU64,
        n: u64,
        cap: u64,
        l: &OperationLimits,
    ) -> Result<(), RecoveryError> {
        l.check()?;
        counter
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |v| {
                v.checked_add(n).filter(|next| *next <= cap)
            })
            .map_err(|_| RecoveryError::Quota {
                operation_id: l.operation_id,
                resource: "shared staging bytes",
            })?;
        Ok(())
    }
    pub fn charge(&self, n: u64, metadata: bool, l: &OperationLimits) -> Result<(), RecoveryError> {
        if metadata {
            Self::add(&self.metadata, n, self.metadata_cap, l)?
        }
        Self::add(&self.total, n, self.cap, l)
    }
    pub fn allocate(
        &self,
        file: &File,
        bytes: u64,
        metadata: bool,
        l: &OperationLimits,
    ) -> Result<(), RecoveryError> {
        self.charge(bytes, metadata, l)?;
        if bytes == 0 {
            return Ok(());
        }
        let length = libc::off_t::try_from(bytes).map_err(|_| destination(l))?;
        // Reserve the entire prospective file before the first write. Linux
        // fallocate avoids write-triggered speculative preallocation. Its
        // rounding allowance has already been charged at admission.
        #[cfg(target_os = "linux")]
        let result = unsafe { libc::posix_fallocate(file.as_raw_fd(), 0, length) };
        #[cfg(not(target_os = "linux"))]
        let result = {
            let _ = length;
            libc::ENOTSUP
        };
        if result != 0 {
            return Err(destination(l));
        }
        let maximum = bytes
            .checked_add(self.unit - 1)
            .map(|n| n / self.unit * self.unit)
            .ok_or_else(|| destination(l))?;
        let allocated = file
            .metadata()
            .map_err(|_| destination(l))?
            .blocks()
            .checked_mul(512)
            .ok_or_else(|| destination(l))?;
        if allocated > maximum {
            return Err(RecoveryError::Unsupported {
                operation_id: l.operation_id,
                reason: "filesystem allocation exceeds admitted geometry",
            });
        }
        l.check()
    }
    pub fn prepare_copy(
        &self,
        file: &File,
        bytes: u64,
        l: &OperationLimits,
    ) -> Result<(), RecoveryError> {
        if bytes > self.image_cap {
            return Err(RecoveryError::ImageExceedsCeiling {
                operation_id: l.operation_id,
            });
        }
        self.allocate(file, bytes, false, l)
    }
    pub fn write_all(
        &self,
        file: &File,
        bytes: &[u8],
        offset: u64,
        member: Member,
        l: &OperationLimits,
    ) -> Result<(), RecoveryError> {
        let mut done = 0;
        while done < bytes.len() {
            event(&*self.io, RecoveryEvent::Write(member), l)?;
            match self.io.write_at(file, &bytes[done..], offset + done as u64) {
                Ok(0) => return Err(destination(l)),
                Ok(n) if n <= bytes.len() - done => done += n,
                Ok(_) => return Err(destination(l)),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return Err(destination(l)),
            }
        }
        Ok(())
    }
    pub fn write_copy(
        &self,
        file: &File,
        bytes: &[u8],
        offset: u64,
        l: &OperationLimits,
    ) -> Result<(), RecoveryError> {
        if offset
            .checked_add(bytes.len() as u64)
            .is_none_or(|n| n > self.image_cap)
        {
            return Err(RecoveryError::ImageExceedsCeiling {
                operation_id: l.operation_id,
            });
        }
        self.write_all(file, bytes, offset, Member::VerificationCopy, l)
    }
    pub fn sync(
        &self,
        file: &File,
        member: Member,
        l: &OperationLimits,
    ) -> Result<(), RecoveryError> {
        event(&*self.io, RecoveryEvent::Flush(member), l)?;
        file.sync_all().map_err(|_| destination(l))?;
        l.check()
    }
}
pub(crate) struct ImageWriter<'a> {
    pub held: PrivateFile,
    control: Arc<PublicationIo>,
    limits: &'a OperationLimits,
    len: u64,
    pub failure: Option<RecoveryError>,
}
impl<'a> ImageWriter<'a> {
    pub fn new(
        path: &Path,
        control: Arc<PublicationIo>,
        limits: &'a OperationLimits,
    ) -> Result<Self, RecoveryError> {
        event(&*control.io, RecoveryEvent::Create(Member::Image), limits)?;
        Ok(Self {
            held: PrivateFile::open(path, true, limits)?,
            control,
            limits,
            len: 0,
            failure: None,
        })
    }
    fn mapped(&mut self, r: Result<(), RecoveryError>) -> io::Result<()> {
        r.map_err(|e| {
            self.failure = Some(e);
            io::Error::other("bounded image I/O failed")
        })
    }
}
impl crate::ferrostore::RecoveryImageWriter for ImageWriter<'_> {
    fn set_len(&mut self, n: u64) -> io::Result<()> {
        let r = (|| {
            if n > self.control.image_cap {
                return Err(RecoveryError::ImageExceedsCeiling {
                    operation_id: self.limits.operation_id,
                });
            }
            if self.len != 0 {
                return Err(self.limits.verification());
            }
            self.control
                .allocate(&self.held.file, n, false, self.limits)?;
            self.held
                .file
                .set_len(n)
                .map_err(|_| destination(self.limits))?;
            self.len = n;
            Ok(())
        })();
        self.mapped(r)
    }
    fn write_at(&mut self, offset: u64, bytes: &[u8]) -> io::Result<()> {
        let r = if offset
            .checked_add(bytes.len() as u64)
            .is_none_or(|n| n > self.len)
        {
            Err(self.limits.verification())
        } else {
            self.control
                .write_all(&self.held.file, bytes, offset, Member::Image, self.limits)
        };
        self.mapped(r)
    }
    fn sync_all(&mut self) -> io::Result<()> {
        let r = self
            .control
            .sync(&self.held.file, Member::Image, self.limits);
        self.mapped(r)
    }
}
pub(crate) fn write_json<T: serde::Serialize>(
    dir: &Dir,
    name: &str,
    value: &T,
    member: Member,
    control: &Arc<PublicationIo>,
    l: &OperationLimits,
) -> Result<(u64, String), RecoveryError> {
    dir.same_name(l)?;
    event(&*control.io, RecoveryEvent::Create(member), l)?;
    let (expected_bytes, expected_hash) = verify::serialized_checksum(value, l)?;
    let file = PrivateFile::open(&dir.path.join(name), true, l)?;
    control.allocate(&file.file, expected_bytes, true, l)?;
    struct Output<'a> {
        file: &'a File,
        control: &'a PublicationIo,
        l: &'a OperationLimits,
        member: Member,
        len: u64,
        expected: u64,
        hash: Sha256,
        failure: Option<RecoveryError>,
    }
    impl Write for Output<'_> {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let r = (|| {
                if self
                    .len
                    .checked_add(bytes.len() as u64)
                    .is_none_or(|n| n > self.expected)
                {
                    return Err(self.l.verification());
                }
                self.control
                    .write_all(self.file, bytes, self.len, self.member, self.l)
            })();
            if let Err(e) = r {
                self.failure = Some(e);
                return Err(io::Error::other("bounded metadata write failed"));
            }
            self.len += bytes.len() as u64;
            self.hash.update(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut output = Output {
        file: &file.file,
        control,
        l,
        member,
        len: 0,
        expected: expected_bytes,
        hash: Sha256::new(),
        failure: None,
    };
    let result = serde_json::to_writer(&mut output, value);
    if let Some(e) = output.failure {
        return Err(e);
    }
    result.map_err(|_| l.verification())?;
    control.sync(&file.file, member, l)?;
    file.same_name(l)?;
    let actual = (output.len, format!("{:x}", output.hash.finalize()));
    if actual != (expected_bytes, expected_hash) {
        return Err(l.verification());
    }
    Ok(actual)
}
pub(crate) fn read_json<T: serde::de::DeserializeOwned>(
    dir: &Dir,
    name: &str,
    l: &OperationLimits,
) -> Result<T, RecoveryError> {
    let file = PrivateFile::open(&dir.path.join(name), false, l)?;
    let len = file.file.metadata().map_err(|_| destination(l))?.len();
    l.quota(len, l.byte_limit, "metadata bytes")?;
    l.admit_memory(
        len.checked_mul(32)
            .and_then(|n| n.checked_add(65536))
            .ok_or_else(|| l.verification())?,
    )?;
    let n = usize::try_from(len).map_err(|_| l.verification())?;
    let mut bytes = vec![0; n];
    for (i, chunk) in bytes.chunks_mut(65536).enumerate() {
        l.check()?;
        file.file
            .read_exact_at(chunk, i as u64 * 65536)
            .map_err(|_| l.verification())?;
    }
    let value = serde_json::from_slice(&bytes).map_err(|_| l.verification())?;
    file.unchanged(l)?;
    l.check()?;
    Ok(value)
}
