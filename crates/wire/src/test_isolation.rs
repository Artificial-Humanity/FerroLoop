//! Process-local fixture confinement, activated explicitly by wire test binaries.
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

static ACTIVE: AtomicBool = AtomicBool::new(false);
static ROOT: OnceLock<tempfile::TempDir> = OnceLock::new();

/// Activate before any test starts; never called by production executables.
#[doc(hidden)]
pub fn enable() {
    let root = ROOT.get_or_init(|| {
        #[cfg(unix)]
        let parent = Path::new("/var/tmp");
        #[cfg(not(unix))]
        let parent = &std::env::temp_dir();
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;
        let mut builder = tempfile::Builder::new();
        builder.prefix("fw-");
        #[cfg(unix)]
        builder.permissions(std::fs::Permissions::from_mode(0o700));
        builder
            .tempdir_in(parent)
            .expect("cannot create private test root")
    });
    // SAFETY: called by the test executable startup constructor before threads exist.
    unsafe {
        std::env::set_var("TMPDIR", root.path());
        #[cfg(windows)]
        {
            std::env::set_var("TMP", root.path());
            std::env::set_var("TEMP", root.path());
        }
    }
    #[cfg(unix)]
    {
        extern "C" fn cleanup() {
            if let Some(root) = ROOT.get() {
                let _ = std::fs::remove_dir_all(root.path());
            }
        }
        // SAFETY: registering this no-argument cleanup at process startup.
        unsafe {
            libc::atexit(cleanup);
        }
    }
    fl_store::test_isolation::install(
        |path| {
            ensure_fixture(path)?;
            match std::fs::symlink_metadata(path) {
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => Err(err),
                _ => ensure_socket(path),
            }
        },
        |target| {
            let address: std::net::SocketAddr = target.parse().map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "unregistered wire test store address",
                )
            })?;
            ensure_tcp(&address)
        },
    );
    fl_host_proto::enable_hub_test_isolation();
    fl_host_proto::install_test_bind_validator(|endpoint| {
        ensure_fixture(endpoint.socket_path())
            .map_err(|err| fl_host_proto::HostError::Protocol(err.to_string()))
    });
    fl_host_proto::install_test_endpoint_validator(|endpoint| {
        ensure_socket(endpoint.socket_path())
            .map_err(|err| fl_host_proto::HostError::Protocol(err.to_string()))
    });
    ACTIVE.store(true, Ordering::SeqCst);
}

#[doc(hidden)]
pub fn is_enabled() -> bool {
    ACTIVE.load(Ordering::SeqCst)
}

/// Refuse any external or symlinked Unix socket / terminal fixture.
pub(crate) fn ensure_fixture(path: &Path) -> std::io::Result<()> {
    if !is_enabled() {
        return Ok(());
    }
    let invalid = || {
        std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "unregistered wire test fixture",
        )
    };
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(invalid());
    }
    let root = ROOT
        .get()
        .ok_or_else(invalid)?
        .path()
        .canonicalize()
        .map_err(|_| invalid())?;
    let parent = path
        .parent()
        .ok_or_else(invalid)?
        .canonicalize()
        .map_err(|_| invalid())?;
    if !parent.starts_with(root)
        || std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
    {
        return Err(invalid());
    }
    Ok(())
}

#[cfg(unix)]
fn fixture_key(path: &Path) -> std::io::Result<PathBuf> {
    let name = path.file_name().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "missing fixture name")
    })?;
    Ok(path
        .parent()
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "missing fixture parent")
        })?
        .canonicalize()?
        .join(name))
}

#[cfg(unix)]
static UNIX_FIXTURES: std::sync::Mutex<Vec<(PathBuf, u64, u64)>> =
    std::sync::Mutex::new(Vec::new());

/// Exact registered path and inode, scoped to the life of the listener.
#[cfg(unix)]
#[doc(hidden)]
pub struct UnixFixture {
    path: PathBuf,
    dev: u64,
    ino: u64,
}

#[cfg(unix)]
impl Drop for UnixFixture {
    fn drop(&mut self) {
        let mut fixtures = UNIX_FIXTURES.lock().unwrap();
        if let Some(i) = fixtures
            .iter()
            .position(|f| f == &(self.path.clone(), self.dev, self.ino))
        {
            fixtures.remove(i);
        }
    }
}

/// Register an already bound socket inside this process's private fixture root.
#[cfg(unix)]
#[doc(hidden)]
pub fn register_unix_socket(path: &Path) -> std::io::Result<UnixFixture> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    ensure_fixture(path)?;
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_socket() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "fixture is not a socket",
        ));
    }
    let fixture = UnixFixture {
        path: fixture_key(path)?,
        dev: metadata.dev(),
        ino: metadata.ino(),
    };
    UNIX_FIXTURES
        .lock()
        .unwrap()
        .push((fixture.path.clone(), fixture.dev, fixture.ino));
    Ok(fixture)
}

#[cfg(unix)]
pub(crate) fn ensure_socket(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    if !is_enabled() {
        return Ok(());
    }
    let denied = || {
        std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "unregistered wire test socket",
        )
    };
    ensure_fixture(path)?;
    let metadata = std::fs::symlink_metadata(path).map_err(|_| denied())?;
    if metadata.file_type().is_socket()
        && UNIX_FIXTURES.lock().unwrap().contains(&(
            fixture_key(path).map_err(|_| denied())?,
            metadata.dev(),
            metadata.ino(),
        ))
    {
        Ok(())
    } else {
        Err(denied())
    }
}

#[cfg(not(unix))]
pub(crate) fn ensure_socket(_path: &Path) -> std::io::Result<()> {
    if !is_enabled() {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "wire test host endpoints unsupported",
        ))
    }
}

#[cfg(unix)]
#[cfg(test)]
pub struct RegisteredUnixListener {
    listener: std::os::unix::net::UnixListener,
    _fixture: UnixFixture,
}
#[cfg(unix)]
#[cfg(test)]
impl std::ops::Deref for RegisteredUnixListener {
    type Target = std::os::unix::net::UnixListener;
    fn deref(&self) -> &Self::Target {
        &self.listener
    }
}
#[cfg(unix)]
#[cfg(test)]
pub fn bind_unix_fixture(path: impl AsRef<Path>) -> std::io::Result<RegisteredUnixListener> {
    let listener = std::os::unix::net::UnixListener::bind(path.as_ref())?;
    let fixture = register_unix_socket(path.as_ref())?;
    Ok(RegisteredUnixListener {
        listener,
        _fixture: fixture,
    })
}

pub(crate) fn tmux_fixture() -> PathBuf {
    std::env::var_os("FL_WIRE_TMUX_BIN")
        .map(PathBuf::from)
        .filter(|p| ensure_fixture(p).is_ok())
        .unwrap_or_else(|| {
            ROOT.get()
                .expect("test isolation enabled")
                .path()
                .join("no-tmux")
        })
}

#[cfg(unix)]
static EXEC_FIXTURES: std::sync::Mutex<Vec<(PathBuf, u64, u64)>> =
    std::sync::Mutex::new(Vec::new());

#[cfg(unix)]
#[doc(hidden)]
pub struct ExecutableFixture {
    path: PathBuf,
    dev: u64,
    ino: u64,
}
#[cfg(unix)]
impl Drop for ExecutableFixture {
    fn drop(&mut self) {
        let mut fixtures = EXEC_FIXTURES.lock().unwrap();
        if let Some(i) = fixtures
            .iter()
            .position(|f| f == &(self.path.clone(), self.dev, self.ino))
        {
            fixtures.remove(i);
        }
    }
}

#[cfg(unix)]
#[doc(hidden)]
pub fn register_executable(path: &Path) -> std::io::Result<ExecutableFixture> {
    use std::os::unix::fs::MetadataExt;
    ensure_fixture(path)?;
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.file_type().is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "fixture is not a regular file",
        ));
    }
    let guard = ExecutableFixture {
        path: fixture_key(path)?,
        dev: meta.dev(),
        ino: meta.ino(),
    };
    EXEC_FIXTURES
        .lock()
        .unwrap()
        .push((guard.path.clone(), guard.dev, guard.ino));
    Ok(guard)
}

#[cfg(unix)]
pub(crate) fn ensure_executable(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    if !is_enabled() {
        return Ok(());
    }
    let denied = || {
        std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "unregistered wire test executable",
        )
    };
    ensure_fixture(path)?;
    let meta = std::fs::symlink_metadata(path).map_err(|_| denied())?;
    if meta.file_type().is_file()
        && EXEC_FIXTURES.lock().unwrap().contains(&(
            fixture_key(path).map_err(|_| denied())?,
            meta.dev(),
            meta.ino(),
        ))
    {
        Ok(())
    } else {
        Err(denied())
    }
}

#[cfg(not(unix))]
pub(crate) fn ensure_executable(_path: &Path) -> std::io::Result<()> {
    if is_enabled() {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "unregistered wire test executable",
        ))
    } else {
        Ok(())
    }
}

#[cfg(all(test, unix))]
static HELD_EXEC_FIXTURES: std::sync::Mutex<Vec<ExecutableFixture>> =
    std::sync::Mutex::new(Vec::new());
#[cfg(all(test, unix))]
pub(crate) fn retain_executable_fixture(path: &Path) {
    HELD_EXEC_FIXTURES
        .lock()
        .unwrap()
        .push(register_executable(path).unwrap());
}

static TCP_FIXTURES: std::sync::Mutex<Vec<std::net::SocketAddr>> =
    std::sync::Mutex::new(Vec::new());

/// Registration owns a duplicate listener so its address cannot be reused until dropped.
#[doc(hidden)]
pub struct TcpFixture {
    addr: std::net::SocketAddr,
    _listener: std::net::TcpListener,
}
impl Drop for TcpFixture {
    fn drop(&mut self) {
        let mut fixtures = TCP_FIXTURES.lock().unwrap();
        if let Some(index) = fixtures.iter().position(|a| *a == self.addr) {
            fixtures.remove(index);
        }
    }
}
#[doc(hidden)]
pub fn register_tcp_listener(listener: &std::net::TcpListener) -> std::io::Result<TcpFixture> {
    let addr = listener.local_addr()?;
    if !addr.ip().is_loopback() || addr.port() == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "invalid wire test TCP fixture",
        ));
    }
    let guard = TcpFixture {
        addr,
        _listener: listener.try_clone()?,
    };
    TCP_FIXTURES.lock().unwrap().push(addr);
    Ok(guard)
}
pub(crate) fn ensure_tcp(addr: &std::net::SocketAddr) -> std::io::Result<()> {
    if !is_enabled() || TCP_FIXTURES.lock().unwrap().contains(addr) {
        return Ok(());
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "unregistered wire test TCP fixture",
    ))
}
