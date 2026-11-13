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
    fl_host_proto::enable_hub_test_isolation();
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
