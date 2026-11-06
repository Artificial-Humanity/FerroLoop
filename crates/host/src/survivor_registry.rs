//! Endpoint-derived survivor registry.
//!
//! The durable file is the Task 1 state path. A private temporary file is synced, then
//! atomically renamed, then the directory is synced. Names starting with `.flh-` are
//! publication leftovers. Restart does not treat them, or a truncated file, as cleanup
//! authority. The epoch high-water mark outlives retired sessions.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
#[cfg(unix)]
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use fl_host_proto::private_parent_ok;
use fl_host_proto::{HostEndpoint, HostError};
use serde::{Deserialize, Serialize};

use crate::refusal::ContainmentRefusal;

pub const MAGIC: u32 = 0x464C_4831;
const MAX_BYTES: usize = 1024 * 1024;
const MAX_ALERTS: usize = 1000;

static TEMP_SEQ: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionPhase {
    Running,
    LeaderExited,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurvivorRecord {
    pub session: String,
    pub epoch: u64,
    pub phase: SessionPhase,
    pub supervisor_pid: u32,
    pub supervisor_start: u64,
    pub vendor_pid: u32,
    pub vendor_start: u64,
    pub pgid: u32,
    pub group_leader_pid: u32,
    pub group_leader_start: u64,
    pub group_leader_reaped: bool,
    #[serde(default)]
    pub leader_code: Option<i32>,
    #[serde(default)]
    pub leader_signal: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Disk {
    magic: u32,
    epoch_high_water: u64,
    sessions: Vec<SurvivorRecord>,
    alerts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registry {
    pub high_water: u64,
    pub sessions: Vec<SurvivorRecord>,
    pub alerts: Vec<String>,
}

impl Registry {
    pub fn empty() -> Self {
        Self {
            high_water: 0,
            sessions: Vec::new(),
            alerts: Vec::new(),
        }
    }

    pub fn alloc_epoch(&self) -> Result<u64, ContainmentRefusal> {
        self.high_water
            .checked_add(1)
            .ok_or(ContainmentRefusal::EpochExhausted)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashPoint {
    BeforePublish,
    DuringTemp,
    AfterPublish,
    WindowsCreated,
    WindowsContained,
    WindowsRunning,
}

impl CrashPoint {
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "before_publish" => Self::BeforePublish,
            "during_temp" => Self::DuringTemp,
            "after_publish" => Self::AfterPublish,
            "windows_created" => Self::WindowsCreated,
            "windows_contained" => Self::WindowsContained,
            "windows_running" => Self::WindowsRunning,
            _ => return None,
        })
    }
}

pub fn registry_path(endpoint: &HostEndpoint) -> Result<PathBuf, HostError> {
    #[cfg(windows)]
    {
        let local = std::env::var_os("LOCALAPPDATA");
        Ok(host_dir_from_local(local.as_deref())?.join(endpoint.state_file_name()))
    }
    #[cfg(not(windows))]
    {
        Ok(endpoint.state_path())
    }
}

/// `LOCALAPPDATA` is required. An empty value is not a relative path.
#[cfg(any(windows, test))]
fn host_dir_from_local(local: Option<&std::ffi::OsStr>) -> Result<PathBuf, HostError> {
    match local {
        Some(local) if !local.is_empty() => Ok(PathBuf::from(local).join("FerroLoop").join("host")),
        _ => Err(HostError::ParentDenied),
    }
}

pub fn load_registry(endpoint: &HostEndpoint) -> Result<Registry, HostError> {
    load_path(&registry_path(endpoint)?)
}

pub fn write_raw_state(endpoint: &HostEndpoint, bytes: &[u8]) -> Result<(), HostError> {
    replace_atomic(&registry_path(endpoint)?, bytes, None)
}

pub fn write_registry_fixture(
    endpoint: &HostEndpoint,
    high_water: u64,
    sessions: &[SurvivorRecord],
) -> Result<(), HostError> {
    let registry = Registry {
        high_water,
        sessions: sessions.to_vec(),
        alerts: Vec::new(),
    };
    validate_registry(&registry).map_err(HostError::from)?;
    publish(&registry_path(endpoint)?, &registry, None)
}

fn load_path(path: &Path) -> Result<Registry, HostError> {
    if file_name_is_temp(path) {
        return Err(ContainmentRefusal::CorruptRegistry.into());
    }
    let bytes = match read_private(path) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return Ok(Registry::empty()),
        Err(err) => return Err(err),
    };
    parse_bytes(&bytes).map_err(HostError::from)
}

fn parse_bytes(bytes: &[u8]) -> Result<Registry, ContainmentRefusal> {
    if bytes.is_empty() || bytes.len() > MAX_BYTES {
        return Err(ContainmentRefusal::CorruptRegistry);
    }
    let disk: Disk =
        serde_json::from_slice(bytes).map_err(|_| ContainmentRefusal::CorruptRegistry)?;
    if disk.magic != MAGIC {
        return Err(ContainmentRefusal::CorruptRegistry);
    }
    let registry = Registry {
        high_water: disk.epoch_high_water,
        sessions: disk.sessions,
        alerts: disk.alerts,
    };
    validate_registry(&registry)?;
    Ok(registry)
}

fn validate_registry(registry: &Registry) -> Result<(), ContainmentRefusal> {
    if registry.alerts.len() > MAX_ALERTS {
        return Err(ContainmentRefusal::CorruptRegistry);
    }
    let mut names = std::collections::BTreeSet::new();
    for session in &registry.sessions {
        if session.epoch == 0 || session.epoch > registry.high_water {
            return Err(ContainmentRefusal::CorruptRegistry);
        }
        if fl_host_proto::SessionId::new(&session.session).is_err() {
            return Err(ContainmentRefusal::CorruptRegistry);
        }
        if !names.insert(session.session.clone()) {
            return Err(ContainmentRefusal::CorruptRegistry);
        }
    }
    Ok(())
}

pub fn publish(
    path: &Path,
    registry: &Registry,
    crash: Option<CrashPoint>,
) -> Result<(), HostError> {
    if matches!(crash, Some(CrashPoint::BeforePublish)) {
        die_for_crash();
    }
    let disk = Disk {
        magic: MAGIC,
        epoch_high_water: registry.high_water,
        sessions: registry.sessions.clone(),
        alerts: registry.alerts.clone(),
    };
    let bytes = serde_json::to_vec(&disk).map_err(|err| HostError::Io(err.to_string()))?;
    if bytes.len() > MAX_BYTES {
        return Err(ContainmentRefusal::CorruptRegistry.into());
    }
    replace_atomic(path, &bytes, crash)
}

fn file_name_is_temp(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(".flh-"))
}

fn die_for_crash() -> ! {
    // The gated vendor lives only until this process dies. Tests that must sample it
    // set FL_HOST_CRASH_HOLD_MS so the sample happens before the signal.
    if let Ok(text) = std::env::var("FL_HOST_CRASH_HOLD_MS")
        && let Ok(ms) = text.parse::<u64>()
    {
        std::thread::sleep(Duration::from_millis(ms.min(500)));
    }
    #[cfg(unix)]
    unsafe {
        libc::raise(libc::SIGKILL);
    }
    #[cfg(windows)]
    unsafe {
        windows_sys::Win32::System::Threading::TerminateProcess(
            windows_sys::Win32::System::Threading::GetCurrentProcess(),
            9,
        );
    }
    std::process::abort();
}

#[cfg(unix)]
fn replace_atomic(path: &Path, bytes: &[u8], crash: Option<CrashPoint>) -> Result<(), HostError> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
    use std::os::unix::ffi::OsStrExt;

    let parent = path.parent().ok_or(HostError::ParentDenied)?;
    let name = path.file_name().ok_or(HostError::ParentDenied)?;
    if file_name_is_temp(path) {
        return Err(HostError::EndpointStateDenied);
    }
    let parent_c = CString::new(parent.as_os_str().as_bytes())
        .map_err(|_| HostError::Protocol("registry parent".into()))?;
    let name_c =
        CString::new(name.as_bytes()).map_err(|_| HostError::Protocol("registry name".into()))?;
    // SAFETY: `parent_c` is a NUL-terminated path. O_DIRECTORY|O_NOFOLLOW refuses a non-directory
    // and a symlink parent. The descriptor is owned below.
    let dirfd = unsafe {
        libc::open(
            parent_c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if dirfd < 0 {
        return Err(HostError::ParentDenied);
    }
    let dir = unsafe { std::os::fd::OwnedFd::from_raw_fd(dirfd) };
    let st = fstat(dir.as_raw_fd())?;
    let is_dir = st.st_mode & libc::S_IFMT == libc::S_IFDIR;
    private_parent_ok(st.st_uid, unix_euid(), st.st_mode, is_dir)?;
    let temp = temp_name()?;
    // SAFETY: `temp` is a relative name. O_EXCL|O_NOFOLLOW creates a new file and refuses a symlink.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            temp.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(HostError::EndpointStateDenied);
    }
    let mut file = unsafe { File::from_raw_fd(fd) };
    // SAFETY: `file` is the descriptor just created. fchmod tightens the mode after umask.
    if unsafe { libc::fchmod(file.as_raw_fd(), 0o600) } != 0 {
        return Err(HostError::EndpointStateDenied);
    }
    file.write_all(bytes)?;
    file.sync_all()?;
    if matches!(crash, Some(CrashPoint::DuringTemp)) {
        die_for_crash();
    }
    let raw = file.into_raw_fd();
    // SAFETY: both names are relative to `dir`. renameat replaces one directory entry.
    let rc = unsafe {
        libc::renameat(
            dir.as_raw_fd(),
            temp.as_ptr(),
            dir.as_raw_fd(),
            name_c.as_ptr(),
        )
    };
    if rc != 0 {
        unsafe { libc::unlinkat(dir.as_raw_fd(), temp.as_ptr(), 0) };
        unsafe { libc::close(raw) };
        return Err(HostError::Io(std::io::Error::last_os_error().to_string()));
    }
    // SAFETY: `raw` still names the renamed file. fsync the file, then the directory.
    if unsafe { libc::fsync(raw) } != 0 {
        unsafe { libc::close(raw) };
        return Err(HostError::Io(std::io::Error::last_os_error().to_string()));
    }
    unsafe { libc::close(raw) };
    if unsafe { libc::fsync(dir.as_raw_fd()) } != 0 {
        return Err(HostError::Io(std::io::Error::last_os_error().to_string()));
    }
    if matches!(crash, Some(CrashPoint::AfterPublish)) {
        die_for_crash();
    }
    Ok(())
}

#[cfg(unix)]
fn read_private(path: &Path) -> Result<Option<Vec<u8>>, HostError> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;

    let parent = path.parent().ok_or(HostError::ParentDenied)?;
    let name = path.file_name().ok_or(HostError::ParentDenied)?;
    let parent_c = CString::new(parent.as_os_str().as_bytes())
        .map_err(|_| HostError::Protocol("registry parent".into()))?;
    let name_c =
        CString::new(name.as_bytes()).map_err(|_| HostError::Protocol("registry name".into()))?;
    // SAFETY: directory open as in `replace_atomic`. ENOENT is an empty registry.
    let dirfd = unsafe {
        libc::open(
            parent_c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if dirfd < 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ENOENT) {
            return Ok(None);
        }
        return Err(HostError::ParentDenied);
    }
    let dir = unsafe { OwnedFd::from_raw_fd(dirfd) };
    // SAFETY: `name_c` is relative. O_NOFOLLOW refuses a symlink registry.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name_c.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ENOENT) {
            return Ok(None);
        }
        return Err(HostError::EndpointStateDenied);
    }
    let mut file = unsafe { File::from_raw_fd(fd) };
    let st = fstat(file.as_raw_fd())?;
    if st.st_mode & libc::S_IFMT != libc::S_IFREG
        || st.st_uid != unix_euid()
        || st.st_mode & 0o777 != 0o600
        || st.st_nlink != 1
    {
        return Err(HostError::EndpointStateDenied);
    }
    if st.st_size < 0 || st.st_size as u64 > MAX_BYTES as u64 {
        return Err(ContainmentRefusal::CorruptRegistry.into());
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    if bytes.len() > MAX_BYTES {
        return Err(ContainmentRefusal::CorruptRegistry.into());
    }
    Ok(Some(bytes))
}

#[cfg(unix)]
fn fstat(fd: std::os::fd::RawFd) -> Result<libc::stat, HostError> {
    // SAFETY: `st` is zeroed plain data and `fd` is open.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut st) } != 0 {
        return Err(HostError::Io(std::io::Error::last_os_error().to_string()));
    }
    Ok(st)
}

#[cfg(unix)]
fn unix_euid() -> u32 {
    // SAFETY: geteuid is infallible.
    unsafe { libc::geteuid() }
}

#[cfg(unix)]
fn temp_name() -> Result<std::ffi::CString, HostError> {
    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let name = format!(".flh-{}-{}-{}", std::process::id(), seq, nanos);
    std::ffi::CString::new(name).map_err(|_| HostError::Protocol("temp name".into()))
}

#[cfg(windows)]
fn replace_atomic(path: &Path, bytes: &[u8], crash: Option<CrashPoint>) -> Result<(), HostError> {
    use std::os::windows::io::{FromRawHandle, IntoRawHandle};
    use windows_sys::Win32::Foundation::{CloseHandle, LocalFree};
    use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::{
        CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_GENERIC_READ, FILE_GENERIC_WRITE, FlushFileBuffers, MOVEFILE_REPLACE_EXISTING,
        MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;

    ensure_registry_dirs(path)?;
    let owner = current_sid()?;
    if let Some(existing) = open_reparse_file(path)? {
        require_private_file(&existing, &owner)?;
    }
    let sid = sid_string(&owner)?;
    let sddl = fl_host_proto::pipe_dacl_sddl(&sid);
    let wide_sddl = wide_str(&sddl);
    let mut raw_sd = std::ptr::null_mut();
    // SAFETY: the SDDL is NUL-terminated. On success `raw_sd` is freed with LocalFree.
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            wide_sddl.as_ptr(),
            1,
            &mut raw_sd,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 || raw_sd.is_null() {
        return Err(HostError::EndpointStateDenied);
    }
    let attrs = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: raw_sd,
        bInheritHandle: 0,
    };
    let temp = path.with_file_name(format!(
        ".flh-{}-{}",
        unsafe { GetCurrentProcessId() },
        TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let temp_w = wide_os(temp.as_os_str());
    // SAFETY: CREATE_NEW refuses an existing name. The DACL is applied at creation and the
    // handle is not inheritable.
    let handle = unsafe {
        CreateFileW(
            temp_w.as_ptr(),
            FILE_GENERIC_READ | FILE_GENERIC_WRITE,
            0,
            &attrs,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    unsafe { LocalFree(raw_sd) };
    if handle.is_null() || handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
        return Err(HostError::EndpointStateDenied);
    }
    let mut file = unsafe { File::from_raw_handle(handle) };
    file.write_all(bytes)?;
    file.sync_all()?;
    if matches!(crash, Some(CrashPoint::DuringTemp)) {
        die_for_crash();
    }
    let raw = file.into_raw_handle();
    // SAFETY: `raw` is the temp file. Flush, then close before the replace.
    unsafe { FlushFileBuffers(raw as _) };
    unsafe { CloseHandle(raw as _) };
    let dest_w = wide_os(path.as_os_str());
    // SAFETY: both paths are NUL-terminated. REPLACE_EXISTING plus WRITE_THROUGH publishes the bytes.
    let moved = unsafe {
        MoveFileExW(
            temp_w.as_ptr(),
            dest_w.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved == 0 {
        let _ = std::fs::remove_file(&temp);
        return Err(HostError::Io("registry replace".into()));
    }
    if matches!(crash, Some(CrashPoint::AfterPublish)) {
        die_for_crash();
    }
    Ok(())
}

#[cfg(windows)]
fn read_private(path: &Path) -> Result<Option<Vec<u8>>, HostError> {
    let parent = path.parent().ok_or(HostError::ParentDenied)?;
    let Some(dir) = open_directory(parent)? else {
        return Ok(None);
    };
    require_directory(&dir)?;
    drop(dir);
    let Some(handle) = open_reparse_file(path)? else {
        return Ok(None);
    };
    require_private_file(&handle, &current_sid()?)?;
    let mut file = handle.into_file();
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    if bytes.len() > MAX_BYTES {
        return Err(ContainmentRefusal::CorruptRegistry.into());
    }
    Ok(Some(bytes))
}

#[cfg(windows)]
fn wide_str(text: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(text)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(windows)]
fn wide_os(text: &std::ffi::OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    text.encode_wide().chain(std::iter::once(0)).collect()
}

#[cfg(windows)]
pub(crate) fn current_sid_string() -> Result<String, HostError> {
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::Foundation::{CloseHandle, LocalFree};
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token = std::ptr::null_mut();
    // SAFETY: the process handle is this process. A non-zero result owns `token`.
    let opened = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
    if opened == 0 {
        return Err(HostError::PeerIdentityUnavailable);
    }
    let _token = unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(token) };
    let mut needed = 0u32;
    unsafe {
        GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed);
    }
    if needed == 0 || needed > 65_536 {
        return Err(HostError::PeerIdentityUnavailable);
    }
    let mut buf = vec![0u8; needed as usize];
    // SAFETY: `buf` is writable for `needed` bytes. TokenUser writes a TOKEN_USER at the start.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            buf.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    };
    if ok == 0 {
        return Err(HostError::PeerIdentityUnavailable);
    }
    let user = unsafe { &*buf.as_ptr().cast::<TOKEN_USER>() };
    let mut raw = std::ptr::null_mut();
    // SAFETY: `user.User.Sid` is the SID inside `buf`, which stays alive for this call.
    let converted = unsafe { ConvertSidToStringSidW(user.User.Sid, &mut raw) };
    if converted == 0 || raw.is_null() {
        return Err(HostError::PeerIdentityUnavailable);
    }
    let mut len = 0usize;
    unsafe {
        while len <= 256 && *raw.add(len) != 0 {
            len += 1;
        }
    }
    let text = if len == 0 || len > 256 {
        String::new()
    } else {
        String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(raw, len) })
    };
    unsafe { LocalFree(raw.cast()) };
    let _ = CloseHandle;
    if text.is_empty() {
        Err(HostError::PeerIdentityUnavailable)
    } else {
        Ok(text)
    }
}

#[cfg(windows)]
fn ensure_registry_dirs(file: &Path) -> Result<(), HostError> {
    let host = file.parent().ok_or(HostError::ParentDenied)?;
    let ferro = host.parent().ok_or(HostError::ParentDenied)?;
    ensure_private_dir(ferro)?;
    ensure_private_dir(host)
}

#[cfg(windows)]
fn ensure_private_dir(path: &Path) -> Result<(), HostError> {
    use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
    use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;

    let sid = current_sid()?;
    let text = sid_string(&sid)?;
    let descriptor = security_descriptor(&fl_host_proto::pipe_dacl_sddl(&text))?;
    let wide = wide_os(path.as_os_str());
    let attrs = security_attributes(descriptor.as_ptr());
    // SAFETY: `wide` is NUL-terminated. The DACL is applied at creation and is not inherited.
    let ok = unsafe { CreateDirectoryW(wide.as_ptr(), &attrs) };
    if ok == 0 && unsafe { GetLastError() } != ERROR_ALREADY_EXISTS {
        return Err(HostError::ParentDenied);
    }
    let handle = open_directory(path)?.ok_or(HostError::ParentDenied)?;
    require_directory(&handle)
}

#[cfg(windows)]
fn require_directory(handle: &OwnedWinHandle) -> Result<(), HostError> {
    use windows_sys::Win32::Security::Authorization::SE_FILE_OBJECT;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    };

    let info = file_info(handle.as_raw()).map_err(|_| HostError::ParentDenied)?;
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0
    {
        return Err(HostError::ParentDenied);
    }
    require_dacl(handle.as_raw(), SE_FILE_OBJECT, &current_sid()?, false)
        .map_err(|_| HostError::ParentDenied)
}

#[cfg(windows)]
fn require_private_file(handle: &OwnedWinHandle, sid: &OwnedSid) -> Result<(), HostError> {
    use windows_sys::Win32::Security::Authorization::SE_FILE_OBJECT;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    };

    let info = file_info(handle.as_raw())?;
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0
        || info.nNumberOfLinks != 1
    {
        return Err(HostError::EndpointStateDenied);
    }
    require_dacl(handle.as_raw(), SE_FILE_OBJECT, sid, true)
}

#[cfg(windows)]
fn open_reparse_file(path: &Path) -> Result<Option<OwnedWinHandle>, HostError> {
    use windows_sys::Win32::Foundation::{
        ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, GetLastError, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ,
        FILE_SHARE_DELETE, FILE_SHARE_READ, OPEN_EXISTING,
    };

    let wide = wide_os(path.as_os_str());
    // SAFETY: OPEN_REPARSE_POINT opens the last component itself and does not follow it.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        let err = unsafe { GetLastError() };
        return if err == ERROR_FILE_NOT_FOUND || err == ERROR_PATH_NOT_FOUND {
            Ok(None)
        } else {
            Err(HostError::EndpointStateDenied)
        };
    }
    OwnedWinHandle::new(handle).map(Some)
}

#[cfg(windows)]
fn open_directory(path: &Path) -> Result<Option<OwnedWinHandle>, HostError> {
    use windows_sys::Win32::Foundation::{
        ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, GetLastError, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    let wide = wide_os(path.as_os_str());
    // SAFETY: BACKUP_SEMANTICS opens a directory. OPEN_REPARSE_POINT does not follow it.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        let err = unsafe { GetLastError() };
        return if err == ERROR_FILE_NOT_FOUND || err == ERROR_PATH_NOT_FOUND {
            Ok(None)
        } else {
            Err(HostError::ParentDenied)
        };
    }
    OwnedWinHandle::new(handle).map(Some)
}

#[cfg(windows)]
fn file_info(
    handle: windows_sys::Win32::Foundation::HANDLE,
) -> Result<windows_sys::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION, HostError> {
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    // SAFETY: `info` is plain data. GetFileInformationByHandle writes it for this handle.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe { GetFileInformationByHandle(handle, &mut info) };
    if ok == 0 {
        Err(HostError::EndpointStateDenied)
    } else {
        Ok(info)
    }
}

#[cfg(windows)]
fn require_dacl(
    handle: windows_sys::Win32::Foundation::HANDLE,
    object: i32,
    sid: &OwnedSid,
    check_owner: bool,
) -> Result<(), HostError> {
    use windows_sys::Win32::Foundation::{ERROR_SUCCESS, LocalFree};
    use windows_sys::Win32::Security::Authorization::GetSecurityInfo;
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, DACL_SECURITY_INFORMATION, EqualSid, GetAce,
        GetSecurityDescriptorControl, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
        SE_DACL_PRESENT, SE_DACL_PROTECTED,
    };

    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
    let mut owner: PSID = std::ptr::null_mut();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: the pointers are ours. On success `sd` owns the returned buffers.
    let status = unsafe {
        GetSecurityInfo(
            handle,
            object,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut sd,
        )
    };
    if status != ERROR_SUCCESS || sd.is_null() {
        return Err(HostError::EndpointStateDenied);
    }
    let result = (|| {
        let mut control = 0u16;
        let mut revision = 0u32;
        let ok = unsafe { GetSecurityDescriptorControl(sd, &mut control, &mut revision) };
        if ok == 0
            || control & SE_DACL_PRESENT == 0
            || control & SE_DACL_PROTECTED == 0
            || dacl.is_null()
        {
            return Err(HostError::EndpointStateDenied);
        }
        if check_owner && (owner.is_null() || unsafe { EqualSid(owner, sid.as_psid()) } == 0) {
            return Err(HostError::EndpointStateDenied);
        }
        let acl = unsafe { &*dacl };
        if acl.AceCount != 1 {
            return Err(HostError::EndpointStateDenied);
        }
        let mut ace = std::ptr::null_mut();
        let ok = unsafe { GetAce(dacl, 0, &mut ace) };
        if ok == 0 || ace.is_null() {
            return Err(HostError::EndpointStateDenied);
        }
        let header = unsafe { &*ace.cast::<ACE_HEADER>() };
        if header.AceType != ACCESS_ALLOWED_ACE_TYPE || header.AceFlags != 0 {
            return Err(HostError::EndpointStateDenied);
        }
        let allowed = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
        let ace_sid = (&raw const allowed.SidStart)
            .cast_mut()
            .cast::<core::ffi::c_void>();
        if unsafe { EqualSid(ace_sid, sid.as_psid()) } == 0 {
            Err(HostError::EndpointStateDenied)
        } else {
            Ok(())
        }
    })();
    unsafe { LocalFree(sd) };
    result
}

#[cfg(windows)]
fn current_sid() -> Result<OwnedSid, HostError> {
    use windows_sys::Win32::Security::{
        CopySid, GetLengthSid, GetTokenInformation, IsValidSid, SECURITY_MAX_SID_SIZE, TOKEN_QUERY,
        TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token = std::ptr::null_mut();
    // SAFETY: the process handle is this process. A non-zero result owns `token`.
    let opened = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
    if opened == 0 {
        return Err(HostError::PeerIdentityUnavailable);
    }
    let token = OwnedWinHandle::new(token)?;
    let mut needed = 0u32;
    unsafe {
        GetTokenInformation(
            token.as_raw(),
            TokenUser,
            std::ptr::null_mut(),
            0,
            &mut needed,
        );
    }
    if needed == 0 || needed > 65_536 {
        return Err(HostError::PeerIdentityUnavailable);
    }
    let words = usize::try_from(needed)
        .map_err(|_| HostError::PeerIdentityUnavailable)?
        .div_ceil(std::mem::size_of::<u64>())
        .max(1);
    let mut buf = vec![0u64; words];
    let capacity = u32::try_from(buf.len() * std::mem::size_of::<u64>())
        .map_err(|_| HostError::PeerIdentityUnavailable)?;
    let ok = unsafe {
        GetTokenInformation(
            token.as_raw(),
            TokenUser,
            buf.as_mut_ptr().cast(),
            capacity,
            &mut needed,
        )
    };
    if ok == 0 {
        return Err(HostError::PeerIdentityUnavailable);
    }
    let user = unsafe { &*buf.as_ptr().cast::<TOKEN_USER>() };
    if user.User.Sid.is_null() || unsafe { IsValidSid(user.User.Sid) } == 0 {
        return Err(HostError::PeerIdentityUnavailable);
    }
    let len = unsafe { GetLengthSid(user.User.Sid) };
    if len == 0 || len > SECURITY_MAX_SID_SIZE {
        return Err(HostError::PeerIdentityUnavailable);
    }
    let mut owned = OwnedSid {
        bytes: [0u8; SECURITY_MAX_SID_SIZE as usize],
    };
    // SAFETY: the destination is 8-aligned and SECURITY_MAX_SID_SIZE bytes. `buf` stays alive.
    let copied = unsafe { CopySid(SECURITY_MAX_SID_SIZE, owned.as_psid_mut(), user.User.Sid) };
    if copied == 0 || unsafe { IsValidSid(owned.as_psid()) } == 0 {
        return Err(HostError::PeerIdentityUnavailable);
    }
    Ok(owned)
}

#[cfg(windows)]
fn sid_string(sid: &OwnedSid) -> Result<String, HostError> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;

    let mut raw = std::ptr::null_mut();
    let ok = unsafe { ConvertSidToStringSidW(sid.as_psid(), &mut raw) };
    if ok == 0 || raw.is_null() {
        return Err(HostError::PeerIdentityUnavailable);
    }
    let mut len = 0usize;
    unsafe {
        while len <= 256 && *raw.add(len) != 0 {
            len += 1;
        }
    }
    if len == 0 || len > 256 {
        unsafe { LocalFree(raw.cast()) };
        return Err(HostError::PeerIdentityUnavailable);
    }
    let text = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(raw, len) });
    unsafe { LocalFree(raw.cast()) };
    if text.is_empty() {
        Err(HostError::PeerIdentityUnavailable)
    } else {
        Ok(text)
    }
}

#[cfg(windows)]
fn security_descriptor(sddl: &str) -> Result<SecurityDescriptor, HostError> {
    use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;

    let wide = wide_str(sddl);
    let mut raw = std::ptr::null_mut();
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            wide.as_ptr(),
            1,
            &mut raw,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 || raw.is_null() {
        Err(HostError::EndpointStateDenied)
    } else {
        Ok(SecurityDescriptor(raw))
    }
}

#[cfg(windows)]
fn security_attributes(
    descriptor: *mut core::ffi::c_void,
) -> windows_sys::Win32::Security::SECURITY_ATTRIBUTES {
    windows_sys::Win32::Security::SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<windows_sys::Win32::Security::SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    }
}

#[cfg(windows)]
struct SecurityDescriptor(*mut core::ffi::c_void);

#[cfg(windows)]
impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { windows_sys::Win32::Foundation::LocalFree(self.0) };
            self.0 = std::ptr::null_mut();
        }
    }
}

#[cfg(windows)]
impl SecurityDescriptor {
    fn as_ptr(&self) -> *mut core::ffi::c_void {
        self.0
    }
}

#[cfg(windows)]
struct OwnedWinHandle(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl Drop for OwnedWinHandle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
        }
        self.0 = std::ptr::null_mut();
    }
}

#[cfg(windows)]
impl OwnedWinHandle {
    fn new(handle: windows_sys::Win32::Foundation::HANDLE) -> Result<Self, HostError> {
        if handle.is_null() || handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            Err(HostError::PeerIdentityUnavailable)
        } else {
            Ok(Self(handle))
        }
    }

    fn as_raw(&self) -> windows_sys::Win32::Foundation::HANDLE {
        self.0
    }

    fn into_file(self) -> File {
        use std::os::windows::io::FromRawHandle;
        let handle = self.0;
        std::mem::forget(self);
        // SAFETY: `handle` is open and exclusively owned. The file closes it.
        unsafe { File::from_raw_handle(handle) }
    }
}

#[cfg(windows)]
#[repr(C, align(8))]
struct OwnedSid {
    bytes: [u8; windows_sys::Win32::Security::SECURITY_MAX_SID_SIZE as usize],
}

#[cfg(windows)]
impl OwnedSid {
    fn as_psid(&self) -> windows_sys::Win32::Security::PSID {
        self.bytes.as_ptr() as windows_sys::Win32::Security::PSID
    }

    fn as_psid_mut(&mut self) -> windows_sys::Win32::Security::PSID {
        self.bytes.as_mut_ptr() as windows_sys::Win32::Security::PSID
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_names_are_not_cleanup_authority() {
        assert!(file_name_is_temp(Path::new("/tmp/x/.flh-1")));
        assert!(!file_name_is_temp(Path::new("/tmp/x/h.sock.state.json")));
    }

    #[test]
    fn truncated_json_is_corrupt() {
        assert!(parse_bytes(b"{").is_err());
        assert!(parse_bytes(b"").is_err());
    }

    #[test]
    fn epoch_zero_and_duplicates_are_corrupt() {
        let record = SurvivorRecord {
            session: "s".into(),
            epoch: 0,
            phase: SessionPhase::Running,
            supervisor_pid: 1,
            supervisor_start: 1,
            vendor_pid: 2,
            vendor_start: 2,
            pgid: 2,
            group_leader_pid: 2,
            group_leader_start: 2,
            group_leader_reaped: false,
            leader_code: None,
            leader_signal: None,
        };
        let registry = Registry {
            high_water: 1,
            sessions: vec![record],
            alerts: Vec::new(),
        };
        assert!(validate_registry(&registry).is_err());
    }

    #[test]
    fn missing_or_empty_localappdata_is_refused() {
        use std::ffi::OsStr;

        use fl_host_proto::HostError;

        assert!(matches!(
            host_dir_from_local(None),
            Err(HostError::ParentDenied)
        ));
        assert!(matches!(
            host_dir_from_local(Some(OsStr::new(""))),
            Err(HostError::ParentDenied)
        ));
        let local = OsStr::new(r"C:\Users\me\AppData\Local");
        let path = host_dir_from_local(Some(local)).expect("local");
        assert_eq!(path, PathBuf::from(local).join("FerroLoop").join("host"));
    }

    #[cfg(windows)]
    #[test]
    fn private_registry_round_trip_rejects_reparse() {
        use fl_host_proto::HostEndpoint;

        let suffix = format!("reg-{}-{}", std::process::id(), std::process::id());
        let endpoint =
            HostEndpoint::windows(&suffix, &current_sid_string().expect("sid")).expect("endpoint");
        let record = SurvivorRecord {
            session: "kept".into(),
            epoch: 1,
            phase: SessionPhase::Running,
            supervisor_pid: 0,
            supervisor_start: 0,
            vendor_pid: 4,
            vendor_start: 4,
            pgid: 4,
            group_leader_pid: 4,
            group_leader_start: 4,
            group_leader_reaped: false,
            leader_code: None,
            leader_signal: None,
        };
        write_registry_fixture(&endpoint, 1, std::slice::from_ref(&record)).expect("write");
        let loaded = load_registry(&endpoint).expect("load");
        assert_eq!(loaded.high_water, 1);
        assert_eq!(loaded.sessions, vec![record]);
        let path = registry_path(&endpoint).expect("path");
        let bytes = std::fs::read(&path).expect("bytes");
        let target = std::env::temp_dir().join(format!("flh-reparse-{}", std::process::id()));
        std::fs::write(&target, &bytes).expect("target");
        std::fs::remove_file(&path).expect("remove");
        if std::os::windows::fs::symlink_file(&target, &path).is_ok() {
            assert!(
                load_registry(&endpoint).is_err(),
                "state open followed a reparse point"
            );
            let _ = std::fs::remove_file(&path);
        }
        let _ = std::fs::remove_file(&target);
    }
}
