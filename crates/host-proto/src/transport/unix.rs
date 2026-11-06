//! Unix endpoint ownership.
//!
//! The directory fd pins relative lock, token, and state operations. `bind`
//! and `connect` still resolve the canonical pathname, so a process that can
//! rename a trusted ancestor can win the race between this check and the
//! syscall. Peer uid checks and the mutual token proof fail closed when that
//! happens. Endpoint identity is the canonical parent plus the basename, never
//! a `/proc/self/fd` path.
//!
//! On macOS the listening socket is `socket` plus `fcntl(FD_CLOEXEC)`. That
//! window is serialized with `CLOEXEC_WINDOW`; this crate does not spawn.

use std::ffi::{CStr, CString};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream as StdUnix;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use crate::endpoint::{HostEndpoint, unix_euid};
use crate::types::{AuthToken, HostError, constant_time_eq};

#[cfg(not(any(target_os = "linux", target_os = "android")))]
static CLOEXEC_WINDOW: Mutex<()> = Mutex::new(());

fn umask_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn endpoints() -> &'static Mutex<std::collections::BTreeSet<Vec<u8>>> {
    static HELD: OnceLock<Mutex<std::collections::BTreeSet<Vec<u8>>>> = OnceLock::new();
    HELD.get_or_init(|| Mutex::new(std::collections::BTreeSet::new()))
}

struct UmaskGuard(libc::mode_t);

impl UmaskGuard {
    fn clear() -> Self {
        // SAFETY: umask returns the previous mask. Drop restores it.
        Self(unsafe { libc::umask(0) })
    }
}

impl Drop for UmaskGuard {
    fn drop(&mut self) {
        // SAFETY: restores the mask captured above.
        unsafe { libc::umask(self.0) };
    }
}

pub struct EndpointClaim(Vec<u8>);

impl Drop for EndpointClaim {
    fn drop(&mut self) {
        let mut held = endpoints().lock().unwrap_or_else(|err| err.into_inner());
        held.remove(&self.0);
    }
}

fn claim(key: Vec<u8>) -> Result<EndpointClaim, HostError> {
    let mut held = endpoints().lock().unwrap_or_else(|err| err.into_inner());
    if !held.insert(key.clone()) {
        return Err(HostError::EndpointBusy);
    }
    Ok(EndpointClaim(key))
}

pub struct UnixSetup {
    pub listener: tokio::net::UnixListener,
    pub endpoint: HostEndpoint,
    pub identity: Vec<u8>,
    pub token: Arc<RwLock<AuthToken>>,
    pub publish: super::TokenPublisher,
    pub cleanup: Box<dyn FnOnce() + Send>,
}

pub fn canonical_socket(path: &Path) -> Result<PathBuf, HostError> {
    let name = path
        .file_name()
        .ok_or(HostError::ParentDenied)?
        .to_os_string();
    if name.as_bytes().contains(&0) || name.as_bytes().contains(&b'/') {
        return Err(HostError::ParentDenied);
    }
    let parent = path.parent().ok_or(HostError::ParentDenied)?;
    let mut hops = 0u32;
    let parent = resolve_parent(parent, unix_euid(), &mut hops)?;
    let full = parent.join(name);
    if crate::endpoint::HostEndpoint::unix(&full).is_err() {
        return Err(HostError::Protocol(
            "canonical socket path does not fit".into(),
        ));
    }
    Ok(full)
}

pub fn read_token(endpoint: &HostEndpoint) -> Result<AuthToken, HostError> {
    let path = canonical_socket(endpoint.socket_path())?;
    let parent = path.parent().ok_or(HostError::ParentDenied)?;
    let dir = open_private_dir(parent, unix_euid())?;
    let name = token_name(&path)?;
    read_token_at(dir.as_raw_fd(), &name)
}

pub fn peer_of(fd: RawFd) -> Result<(), HostError> {
    let uid = peer_uid_fd(fd)?;
    crate::validate_unix_peer(crate::PeerQuery::Uid(uid), unix_euid())
}

pub fn bind_setup(requested: &HostEndpoint) -> Result<UnixSetup, HostError> {
    let socket_path = canonical_socket(requested.socket_path())?;
    let identity = socket_path.as_os_str().as_bytes().to_vec();
    let claim = claim(identity.clone())?;
    let euid = unix_euid();
    let parent = socket_path.parent().ok_or(HostError::ParentDenied)?;
    let dir = open_private_dir(parent, euid)?;
    let socket_name = file_cstr(&socket_path)?;
    let token_name = token_name(&socket_path)?;
    let state_name = state_name(&socket_path)?;
    let lock_name = lock_name(&socket_path)?;
    let lock = open_lock(dir.as_raw_fd(), &lock_name, euid)?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let err = io::Error::last_os_error();
        let busy = matches!(err.raw_os_error(), Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN);
        return Err(if busy {
            HostError::EndpointBusy
        } else {
            HostError::Io(err.to_string())
        });
    }
    reject_state(dir.as_raw_fd(), &state_name, euid)?;
    reject_bad_token(dir.as_raw_fd(), &token_name, euid)?;
    prepare_socket(dir.as_raw_fd(), &socket_name, &socket_path, euid)?;
    let (listener, dev, ino) = bind_private(&socket_path)?;
    let generated = AuthToken::generate()?;
    let dir = Arc::new(Mutex::new(dir));
    if let Err(err) = publish_token(&dir, &token_name, generated.as_bytes()) {
        unlink_socket(&dir, &socket_name, dev, ino, euid);
        return Err(err);
    }
    let token = Arc::new(RwLock::new(generated));
    let publish_dir = Arc::clone(&dir);
    let publish_name = token_name.clone();
    let publish_token_cell = Arc::clone(&token);
    let publish: super::TokenPublisher = Arc::new(move |bytes| {
        publish_token(&publish_dir, &publish_name, bytes)?;
        *publish_token_cell
            .write()
            .unwrap_or_else(|err| err.into_inner()) = AuthToken::new(*bytes);
        Ok(())
    });
    let endpoint = HostEndpoint::unix(&socket_path)?;
    let cleanup_dir = Arc::clone(&dir);
    let cleanup_token = Arc::clone(&token);
    let cleanup = Box::new(move || {
        unlink_socket(cleanup_dir.as_ref(), &socket_name, dev, ino, euid);
        unlink_token_if_ours(cleanup_dir.as_ref(), &token_name, &cleanup_token);
        drop(lock);
        drop(claim);
    }) as Box<dyn FnOnce() + Send>;
    Ok(UnixSetup {
        listener,
        endpoint,
        identity,
        token,
        publish,
        cleanup,
    })
}

fn unlink_socket(dir: &Mutex<OwnedFd>, name: &CString, dev: u64, ino: u64, euid: u32) {
    let guard = dir.lock().unwrap_or_else(|err| err.into_inner());
    let Some(st) = lstat_at(guard.as_raw_fd(), name).ok().flatten() else {
        return;
    };
    if is_socket(&st) && st.st_uid == euid && dev_ino(&st) == (dev, ino) {
        unsafe { libc::unlinkat(guard.as_raw_fd(), name.as_ptr(), 0) };
    }
}

fn unlink_token_if_ours(dir: &Mutex<OwnedFd>, name: &CString, token: &RwLock<AuthToken>) {
    let guard = dir.lock().unwrap_or_else(|err| err.into_inner());
    let current = *token
        .read()
        .unwrap_or_else(|err| err.into_inner())
        .as_bytes();
    let Ok(found) = read_token_at(guard.as_raw_fd(), name) else {
        return;
    };
    if constant_time_eq(found.as_bytes(), &current) {
        unsafe { libc::unlinkat(guard.as_raw_fd(), name.as_ptr(), 0) };
    }
}

fn resolve_parent(parent: &Path, euid: u32, hops: &mut u32) -> Result<PathBuf, HostError> {
    if *hops > 32 {
        return Err(HostError::RenameableAncestor);
    }
    let mut comps = Vec::new();
    for component in parent.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => comps.push(name.to_os_string()),
            Component::CurDir => {}
            _ => return Err(HostError::ParentDenied),
        }
    }
    if comps.is_empty() {
        return Err(HostError::ParentDenied);
    }
    let mut acc = PathBuf::from("/");
    let mut from_sticky = false;
    for (index, comp) in comps.iter().enumerate() {
        let leaf = index + 1 == comps.len();
        let next = acc.join(comp);
        let meta = match std::fs::symlink_metadata(&next) {
            Ok(meta) => meta,
            Err(err) if err.kind() == io::ErrorKind::NotFound && leaf => {
                mkdir_private(&next)?;
                let created =
                    std::fs::symlink_metadata(&next).map_err(|_| HostError::ParentDenied)?;
                check_leaf(&created, euid)?;
                return Ok(next);
            }
            Err(_) => return Err(HostError::ParentDenied),
        };
        if meta.file_type().is_symlink() {
            if leaf {
                return Err(HostError::ParentDenied);
            }
            if meta.uid() != 0 && meta.uid() != euid {
                return Err(HostError::RenameableAncestor);
            }
            *hops += 1;
            let target = std::fs::read_link(&next).map_err(|_| HostError::RenameableAncestor)?;
            let mut rest = if target.is_absolute() {
                target
            } else {
                acc.join(target)
            };
            for later in comps.iter().skip(index + 1) {
                rest.push(later);
            }
            return resolve_parent(&rest, euid, hops);
        }
        if !meta.is_dir() {
            return Err(HostError::ParentDenied);
        }
        if from_sticky && meta.uid() != 0 && meta.uid() != euid {
            return Err(HostError::RenameableAncestor);
        }
        if leaf {
            check_leaf(&meta, euid)?;
            return Ok(next);
        }
        if meta.uid() != 0 && meta.uid() != euid {
            return Err(HostError::RenameableAncestor);
        }
        if meta.mode() & 0o022 != 0 {
            if meta.mode() & 0o1000 == 0 {
                return Err(HostError::RenameableAncestor);
            }
            from_sticky = true;
        } else {
            from_sticky = false;
        }
        acc = next;
    }
    Err(HostError::ParentDenied)
}

fn check_leaf(meta: &std::fs::Metadata, euid: u32) -> Result<(), HostError> {
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(HostError::ParentDenied);
    }
    crate::private_parent_ok(meta.uid(), euid, meta.mode() & 0o777, true)
}

fn mkdir_private(path: &Path) -> Result<(), HostError> {
    let _guard = umask_lock().lock().unwrap_or_else(|err| err.into_inner());
    let _umask = UmaskGuard::clear();
    let c = cstring(path)?;
    // SAFETY: `c` is a NUL-terminated path. Mode 0700 is applied with umask 0.
    if unsafe { libc::mkdir(c.as_ptr(), 0o700) } != 0 {
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::AlreadyExists {
            return Ok(());
        }
        return Err(HostError::ParentDenied);
    }
    Ok(())
}

fn open_private_dir(path: &Path, euid: u32) -> Result<OwnedFd, HostError> {
    let c = cstring(path)?;
    // SAFETY: `c` is NUL-terminated. O_DIRECTORY|O_NOFOLLOW refuses a non-directory and a symlink.
    let fd = unsafe {
        libc::open(
            c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(HostError::ParentDenied);
    }
    // SAFETY: `fd` is a fresh descriptor this function owns.
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    let st = fstat(owned.as_raw_fd())?;
    let is_dir = st.st_mode & libc::S_IFMT == libc::S_IFDIR;
    crate::private_parent_ok(st.st_uid, euid, st.st_mode & 0o777, is_dir)?;
    Ok(owned)
}

fn open_lock(dir: RawFd, name: &CStr, euid: u32) -> Result<OwnedFd, HostError> {
    let _guard = umask_lock().lock().unwrap_or_else(|err| err.into_inner());
    let _umask = UmaskGuard::clear();
    // SAFETY: `name` is a relative NUL-terminated name. No-follow refuses a symlink.
    let fd = unsafe {
        libc::openat(
            dir,
            name.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(HostError::EndpointStateDenied);
    }
    // SAFETY: `fd` is a fresh descriptor.
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    let st = fstat(owned.as_raw_fd())?;
    if !private_file(&st, euid) {
        return Err(HostError::EndpointStateDenied);
    }
    Ok(owned)
}

fn reject_state(dir: RawFd, name: &CStr, euid: u32) -> Result<(), HostError> {
    match lstat_at(dir, name)? {
        None => Ok(()),
        Some(st) if private_file(&st, euid) => Ok(()),
        Some(_) => Err(HostError::EndpointStateDenied),
    }
}

fn reject_bad_token(dir: RawFd, name: &CStr, euid: u32) -> Result<(), HostError> {
    match lstat_at(dir, name)? {
        None => Ok(()),
        Some(st) if private_file(&st, euid) => Ok(()),
        Some(_) => Err(HostError::EndpointStateDenied),
    }
}

fn prepare_socket(dir: RawFd, name: &CStr, path: &Path, euid: u32) -> Result<(), HostError> {
    let Some(st) = lstat_at(dir, name)? else {
        return Ok(());
    };
    if !is_socket(&st) {
        return Err(HostError::NotASocket);
    }
    if st.st_uid != euid {
        return Err(HostError::EndpointBusy);
    }
    let before = dev_ino(&st);
    match StdUnix::connect(path) {
        Ok(_) => Err(HostError::EndpointBusy),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::ConnectionRefused => {
            let Some(again) = lstat_at(dir, name)? else {
                return Ok(());
            };
            if is_socket(&again) && again.st_uid == euid && dev_ino(&again) == before {
                // SAFETY: `name` is relative to the pinned directory. The inode was rechecked.
                if unsafe { libc::unlinkat(dir, name.as_ptr(), 0) } != 0 {
                    return Err(HostError::Io(io::Error::last_os_error().to_string()));
                }
                Ok(())
            } else {
                Err(HostError::EndpointBusy)
            }
        }
        Err(_) => Err(HostError::EndpointBusy),
    }
}

fn bind_private(path: &Path) -> Result<(tokio::net::UnixListener, u64, u64), HostError> {
    let bytes = path.as_os_str().as_bytes();
    // SAFETY: sockaddr_un is plain data. Zeroed memory is a valid starting value.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.is_empty() || bytes.contains(&0) {
        return Err(HostError::Protocol("socket path".into()));
    }
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    if bytes.len() >= addr.sun_path.len() {
        return Err(HostError::Protocol("socket path exceeds sun_path".into()));
    }
    for (slot, byte) in addr.sun_path.iter_mut().zip(bytes) {
        *slot = *byte as libc::c_char;
    }
    let len = libc::socklen_t::try_from(
        std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1,
    )
    .map_err(|_| HostError::Protocol("socket address".into()))?;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let kind = libc::SOCK_STREAM | libc::SOCK_CLOEXEC;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let kind = libc::SOCK_STREAM;
    // SAFETY: socket(2) with a known domain. The descriptor is checked before use.
    let raw = unsafe { libc::socket(libc::AF_UNIX, kind, 0) };
    if raw < 0 {
        return Err(HostError::Io(io::Error::last_os_error().to_string()));
    }
    // SAFETY: `raw` is a new descriptor this function owns.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        let _window = CLOEXEC_WINDOW.lock().unwrap_or_else(|err| err.into_inner());
        // SAFETY: `fd` is open. FD_CLOEXEC only sets close-on-exec.
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
            return Err(HostError::Io(io::Error::last_os_error().to_string()));
        }
    }
    // SAFETY: `addr` is a sockaddr_un and `len` covers the path plus its NUL.
    let rc = unsafe {
        libc::bind(
            fd.as_raw_fd(),
            (&raw const addr).cast::<libc::sockaddr>(),
            len,
        )
    };
    if rc != 0 {
        return Err(HostError::Io(io::Error::last_os_error().to_string()));
    }
    // `fchmod` on a Linux socket fd does not change the directory entry.
    // `chmod` the path, then `fchmod`, both before listen.
    if let Err(err) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
        let _ = std::fs::remove_file(path);
        return Err(HostError::Io(err.to_string()));
    }
    // SAFETY: `fd` is the bound socket. 0600 is applied before listen.
    if unsafe { libc::fchmod(fd.as_raw_fd(), 0o600) } != 0 {
        let _ = std::fs::remove_file(path);
        return Err(HostError::Io(io::Error::last_os_error().to_string()));
    }
    // SAFETY: `fd` is a bound stream socket.
    if unsafe { libc::listen(fd.as_raw_fd(), 128) } != 0 {
        let _ = std::fs::remove_file(path);
        return Err(HostError::Io(io::Error::last_os_error().to_string()));
    }
    // The fd's sockfs inode is not the directory entry. Record the path inode.
    let meta = std::fs::symlink_metadata(path).map_err(|err| {
        let _ = std::fs::remove_file(path);
        HostError::Io(err.to_string())
    })?;
    if meta.mode() & libc::S_IFMT != libc::S_IFSOCK {
        let _ = std::fs::remove_file(path);
        return Err(HostError::NotASocket);
    }
    let dev = meta.dev();
    let ino = meta.ino();
    let std_listener = unsafe { std::os::unix::net::UnixListener::from_raw_fd(fd.into_raw_fd()) };
    std_listener
        .set_nonblocking(true)
        .map_err(|err| HostError::Io(err.to_string()))?;
    let listener = tokio::net::UnixListener::from_std(std_listener)
        .map_err(|err| HostError::Io(err.to_string()))?;
    Ok((listener, dev, ino))
}

fn publish_token(dir: &Mutex<OwnedFd>, name: &CStr, bytes: &[u8; 32]) -> Result<(), HostError> {
    let guard = dir.lock().unwrap_or_else(|err| err.into_inner());
    let dirfd = guard.as_raw_fd();
    reject_bad_token(dirfd, name, unix_euid())?;
    let temp = temp_name()?;
    let _umask_lock = umask_lock().lock().unwrap_or_else(|err| err.into_inner());
    let _umask = UmaskGuard::clear();
    // SAFETY: `temp` is a relative name. O_EXCL|O_NOFOLLOW creates a new 0600 file.
    let fd = unsafe {
        libc::openat(
            dirfd,
            temp.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(HostError::EndpointStateDenied);
    }
    // SAFETY: `fd` is a fresh descriptor.
    let mut file = unsafe { File::from_raw_fd(fd) };
    file.write_all(bytes)?;
    file.sync_all()?;
    let raw = file.into_raw_fd();
    // SAFETY: both names are relative to `dirfd`. renameat replaces the directory entry.
    let rc = unsafe { libc::renameat(dirfd, temp.as_ptr(), dirfd, name.as_ptr()) };
    if rc != 0 {
        unsafe { libc::unlinkat(dirfd, temp.as_ptr(), 0) };
        unsafe { libc::close(raw) };
        return Err(HostError::Io(io::Error::last_os_error().to_string()));
    }
    // SAFETY: `raw` is still the renamed file. fsync pushes the bytes before the directory sync.
    unsafe { libc::fsync(raw) };
    unsafe { libc::close(raw) };
    // SAFETY: `dirfd` is the parent directory. fsync publishes the rename.
    unsafe { libc::fsync(dirfd) };
    Ok(())
}

fn read_token_at(dir: RawFd, name: &CStr) -> Result<AuthToken, HostError> {
    // SAFETY: `name` is relative. O_NOFOLLOW refuses a symlink token.
    let fd = unsafe {
        libc::openat(
            dir,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(HostError::EndpointStateDenied);
    }
    // SAFETY: `fd` is a fresh descriptor.
    let mut file = unsafe { File::from_raw_fd(fd) };
    let st = fstat(file.as_raw_fd())?;
    if !private_file(&st, unix_euid()) {
        return Err(HostError::EndpointStateDenied);
    }
    let mut buf = [0u8; 32];
    file.read_exact(&mut buf)?;
    let mut extra = [0u8; 1];
    if file.read(&mut extra)? != 0 {
        return Err(HostError::EndpointStateDenied);
    }
    Ok(AuthToken::new(buf))
}

fn private_file(st: &libc::stat, euid: u32) -> bool {
    st.st_mode & libc::S_IFMT == libc::S_IFREG
        && st.st_uid == euid
        && st.st_mode & 0o777 == 0o600
        && st.st_nlink == 1
}

fn is_socket(st: &libc::stat) -> bool {
    st.st_mode & libc::S_IFMT == libc::S_IFSOCK
}

fn dev_ino(st: &libc::stat) -> (u64, u64) {
    // `dev_t` and `ino_t` are narrower than `u64` on some Unix targets.
    #[allow(clippy::unnecessary_cast)]
    {
        (st.st_dev as u64, st.st_ino as u64)
    }
}

fn fstat(fd: RawFd) -> Result<libc::stat, HostError> {
    // SAFETY: `stat` is zeroed plain data and `fd` is open.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut st) } != 0 {
        return Err(HostError::Io(io::Error::last_os_error().to_string()));
    }
    Ok(st)
}

fn lstat_at(dir: RawFd, name: &CStr) -> Result<Option<libc::stat>, HostError> {
    // SAFETY: `stat` is zeroed. AT_SYMLINK_NOFOLLOW does not follow a symlink entry.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatat(dir, name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) } != 0 {
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(HostError::Io(err.to_string()));
    }
    Ok(Some(st))
}

fn temp_name() -> Result<CString, HostError> {
    let mut bytes = [0u8; 8];
    getrandom::getrandom(&mut bytes).map_err(|err| HostError::Io(err.to_string()))?;
    let mut text = String::from(".flh-");
    for byte in bytes {
        text.push(char::from(b"0123456789abcdef"[(byte >> 4) as usize]));
        text.push(char::from(b"0123456789abcdef"[(byte & 0xf) as usize]));
    }
    CString::new(text).map_err(|_| HostError::Protocol("temp name".into()))
}

fn token_name(socket: &Path) -> Result<CString, HostError> {
    suffixed(socket, ".token")
}

fn state_name(socket: &Path) -> Result<CString, HostError> {
    suffixed(socket, ".state.json")
}

fn lock_name(socket: &Path) -> Result<CString, HostError> {
    suffixed(socket, ".lock")
}

fn suffixed(socket: &Path, suffix: &str) -> Result<CString, HostError> {
    let mut name = socket
        .file_name()
        .ok_or(HostError::ParentDenied)?
        .to_os_string();
    name.push(suffix);
    CString::new(name.into_vec()).map_err(|_| HostError::Protocol("endpoint file name".into()))
}

fn file_cstr(path: &Path) -> Result<CString, HostError> {
    let name = path.file_name().ok_or(HostError::ParentDenied)?;
    CString::new(name.as_bytes().to_vec()).map_err(|_| HostError::Protocol("file name".into()))
}

fn cstring(path: &Path) -> Result<CString, HostError> {
    CString::new(path.as_os_str().as_bytes().to_vec()).map_err(|_| HostError::ParentDenied)
}

pub fn peer_uid_fd(fd: RawFd) -> Result<u32, HostError> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // SAFETY: ucred is plain data. Zeroed memory is a valid credential value.
        let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
        let mut len = libc::socklen_t::try_from(std::mem::size_of::<libc::ucred>())
            .map_err(|_| HostError::PeerIdentityUnavailable)?;
        // SAFETY: `cred` is writable and `len` starts as its size. `fd` is a socket.
        let rc = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&raw mut cred).cast::<libc::c_void>(),
                &raw mut len,
            )
        };
        if rc != 0 {
            return Err(HostError::PeerIdentityUnavailable);
        }
        if usize::try_from(len).ok() != Some(std::mem::size_of::<libc::ucred>()) {
            return Err(HostError::PeerIdentityUnavailable);
        }
        let bytes = unsafe {
            std::slice::from_raw_parts(
                (&raw const cred).cast::<u8>(),
                std::mem::size_of::<libc::ucred>(),
            )
        };
        interpret_ucred(bytes)
    }
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    ))]
    {
        let mut uid: libc::uid_t = 0;
        let mut gid: libc::gid_t = 0;
        // SAFETY: `uid` and `gid` are writable. The peer identity is the uid only.
        if unsafe { libc::getpeereid(fd, &raw mut uid, &raw mut gid) } != 0 {
            return Err(HostError::PeerIdentityUnavailable);
        }
        Ok(uid)
    }
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    )))]
    {
        let _ = fd;
        Err(HostError::PeerIdentityUnavailable)
    }
}

pub fn interpret_ucred(bytes: &[u8]) -> Result<u32, HostError> {
    let size = std::mem::size_of::<libc::ucred>();
    if bytes.len() != size {
        return Err(HostError::PeerIdentityUnavailable);
    }
    // SAFETY: the length matches `ucred`. The copy lands in aligned zeroed storage.
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), (&raw mut cred).cast::<u8>(), size);
    }
    Ok(cred.uid)
}
