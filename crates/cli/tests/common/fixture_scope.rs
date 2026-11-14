//! Opt-in fixture manifest builder. This does not activate isolation in a child
//! or itself guard any I/O. Callers must retain the finalized scope until all
//! users of its resources have stopped.

#![allow(dead_code)]

use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, ExitStatus, Output, Stdio};
use std::sync::Arc;

use fl_fixture_path::{FixtureContext, FixtureIdentity, PathAccess, StoreBackend};
use tempfile::TempDir;

/// A typed resource that the helper is permitted to declare as a creation
/// target. The exact path is registered, not the entire fixture directory.
pub enum CreationTarget {
    Store(StoreBackend),
    Route,
    UnixSocket,
}

/// Purpose is chosen by the helper, never inferred from a caller-provided address.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TcpFixturePurpose {
    WirePeer,
    Store,
    Vendor,
}

impl TcpFixturePurpose {
    fn as_str(self) -> &'static str {
        match self {
            Self::WirePeer => "wire_peer",
            Self::Store => "store",
            Self::Vendor => "vendor",
        }
    }
}

struct OwnedTcpFixture {
    purpose: TcpFixturePurpose,
    listener: TcpListener,
}

/// Owns all files and listeners it creates until finalization transfers them to the scope.
pub struct FixtureScopeBuilder {
    tcp: Vec<OwnedTcpFixture>,
    files: Vec<File>,
    identities: Vec<FixtureIdentity>,
    root: TempDir,
}

/// Owns the manifest, fixture files, parent-bound listeners and directory for its lifetime.
/// It does not own asynchronous subprocesses or child-created listeners.
pub struct FixtureScope {
    tcp: Vec<OwnedTcpFixture>,
    files: Vec<File>,
    manifest: PathBuf,
    manifest_bytes: Vec<u8>,
    context: FixtureContext,
    // Fields drop in declaration order, keeping the root until after files.
    root: TempDir,
}

// The scope is retained until its direct child is proven reaped. Descendant
// processes, child-created listeners, and pipe ownership are not supported.
trait FixtureProcess {
    type Status;
    fn try_wait(&mut self) -> io::Result<Option<Self::Status>>;
    fn kill(&mut self) -> io::Result<()>;
    fn wait(&mut self) -> io::Result<Self::Status>;
}

impl FixtureProcess for Child {
    type Status = ExitStatus;
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        Child::try_wait(self)
    }
    fn kill(&mut self) -> io::Result<()> {
        Child::kill(self)
    }
    fn wait(&mut self) -> io::Result<ExitStatus> {
        Child::wait(self)
    }
}

struct ManagedFixtureChild<P: FixtureProcess, S> {
    child: Option<P>,
    scope: Option<S>,
}

impl<P: FixtureProcess, S> ManagedFixtureChild<P, S> {
    fn new(child: P, scope: S) -> Self {
        Self {
            child: Some(child),
            scope: Some(scope),
        }
    }

    fn wait(&mut self) -> io::Result<P::Status> {
        let child = self.child.as_mut().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "fixture child already reaped")
        })?;
        let status = child.wait()?;
        self.child.take();
        Ok(status)
    }

    fn shutdown(&mut self) -> io::Result<P::Status> {
        let child = self.child.as_mut().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "fixture child already reaped")
        })?;
        if let Some(status) = child.try_wait()? {
            self.child.take();
            return Ok(status);
        }
        if let Err(kill_error) = child.kill() {
            if let Some(status) = child.try_wait()? {
                self.child.take();
                return Ok(status);
            }
            return Err(kill_error);
        }
        self.wait()
    }
}

impl<P: FixtureProcess, S> Drop for ManagedFixtureChild<P, S> {
    fn drop(&mut self) {
        if self.child.is_some() && self.shutdown().is_err() {
            // Reaping is unproven: preserve both process handle and resources
            // rather than letting a still-running child outlive its fixtures.
            if let Some(child) = self.child.take() {
                std::mem::forget(child);
            }
            if let Some(scope) = self.scope.take() {
                std::mem::forget(scope);
            }
        }
    }
}

/// Owns a direct child and retains the fixture scope until it is reaped.
/// No handle extraction or detachment is available.
pub struct FixtureChild {
    inner: ManagedFixtureChild<Child, Arc<FixtureScope>>,
}

impl FixtureChild {
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        self.inner.wait()
    }
    pub fn shutdown(&mut self) -> io::Result<ExitStatus> {
        self.inner.shutdown()
    }
}

impl FixtureScopeBuilder {
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            tcp: Vec::new(),
            files: Vec::new(),
            identities: Vec::new(),
            root: tempfile::tempdir()?,
        })
    }

    fn path(&self, name: &str) -> io::Result<PathBuf> {
        let mut components = Path::new(name).components();
        if name == "manifest.json"
            || !matches!(components.next(), Some(Component::Normal(_)))
            || components.next().is_some()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "fixture name must be a single unreserved path component",
            ));
        }
        Ok(self.root.path().join(name))
    }

    /// Create and retain a file owned by this helper for an existing route.
    pub fn create_route(&mut self, name: &str, contents: &[u8]) -> io::Result<PathBuf> {
        let path = self.path(name)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        file.write_all(contents)?;
        file.flush()?;
        self.files.push(file);
        self.identities.push(FixtureIdentity::Route {
            path: path.clone(),
            access: PathAccess::Existing,
        });
        Ok(path)
    }

    /// Create an executable fixture without accepting a caller-selected path.
    pub fn create_executable(&mut self, name: &str, contents: &[u8]) -> io::Result<PathBuf> {
        let path = self.path(name)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        file.write_all(contents)?;
        file.flush()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o700))?;
        }
        drop(file);
        self.files.push(File::open(&path)?);
        self.identities.push(FixtureIdentity::Executable {
            path: path.clone(),
            access: PathAccess::Existing,
        });
        Ok(path)
    }

    /// Bind and retain a parent-owned listener. Callers choose its purpose but
    /// never choose or supply an endpoint; the OS assigns an exact local port.
    pub fn bind_tcp(&mut self, purpose: TcpFixturePurpose) -> io::Result<SocketAddr> {
        if self.tcp.iter().any(|fixture| fixture.purpose == purpose) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "TCP fixture purpose is already registered",
            ));
        }
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let address = listener.local_addr()?;
        self.identities.push(FixtureIdentity::Tcp {
            purpose: purpose.as_str().to_owned(),
            address,
        });
        self.tcp.push(OwnedTcpFixture { purpose, listener });
        Ok(address)
    }

    /// Declare one exact helper-controlled future creation path.
    pub fn creation_target(&mut self, name: &str, kind: CreationTarget) -> io::Result<PathBuf> {
        let path = self.path(name)?;
        if path.exists()
            || self
                .identities
                .iter()
                .any(|identity| identity_path(identity) == Some(path.as_path()))
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "fixture target already exists or is registered",
            ));
        }
        let identity = match kind {
            CreationTarget::Store(backend) => FixtureIdentity::Store {
                backend,
                path: path.clone(),
                access: PathAccess::Create,
            },
            CreationTarget::Route => FixtureIdentity::Route {
                path: path.clone(),
                access: PathAccess::Create,
            },
            CreationTarget::UnixSocket => FixtureIdentity::UnixSocket {
                path: path.clone(),
                access: PathAccess::Create,
            },
        };
        self.identities.push(identity);
        Ok(path)
    }

    /// Write a manifest once and validate it with the same strict loader used by
    /// the eventual child. Builder consumption prevents further registrations.
    pub fn finish(self) -> io::Result<FixtureScope> {
        let manifest = self.root.path().join("manifest.json");
        let bytes = serde_json::to_vec(
            &serde_json::json!({ "version": 1, "identities": self.identities }),
        )?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&manifest)?;
        file.write_all(&bytes)?;
        file.flush()?;
        let context = FixtureContext::load(self.root.path(), &manifest)?;
        let mut permissions = file.metadata()?.permissions();
        permissions.set_readonly(true);
        file.set_permissions(permissions)?;
        drop(file);
        Ok(FixtureScope {
            tcp: self.tcp,
            files: self.files,
            manifest,
            manifest_bytes: bytes,
            context,
            root: self.root,
        })
    }
}

fn identity_path(identity: &FixtureIdentity) -> Option<&Path> {
    match identity {
        FixtureIdentity::Store { path, .. }
        | FixtureIdentity::Route { path, .. }
        | FixtureIdentity::UnixSocket { path, .. }
        | FixtureIdentity::Executable { path, .. } => Some(path),
        FixtureIdentity::Tcp { .. } | FixtureIdentity::WindowsPipe { .. } => None,
    }
}

impl FixtureScope {
    pub fn root(&self) -> &Path {
        self.root.path()
    }
    /// Validate the finalized manifest immediately before passing its path to a child.
    /// This detects accidental changes; it is not protection from a hostile process.
    pub fn manifest_for_transfer(&self) -> io::Result<&Path> {
        let metadata = std::fs::metadata(&self.manifest)?;
        if !metadata.is_file()
            || !metadata.permissions().readonly()
            || std::fs::read(&self.manifest)? != self.manifest_bytes
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "finalized fixture manifest changed",
            ));
        }
        Ok(&self.manifest)
    }
    pub fn context(&self) -> &FixtureContext {
        &self.context
    }

    /// Accept a connection on a retained fixture listener without exposing or
    /// cloning the listener itself. A scoped server thread can borrow `self`
    /// while a synchronous child runs; asynchronous child ownership is separate.
    pub fn accept_tcp(&self, purpose: TcpFixturePurpose) -> io::Result<(TcpStream, SocketAddr)> {
        let fixture = self
            .tcp
            .iter()
            .find(|fixture| fixture.purpose == purpose)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "TCP fixture not registered"))?;
        fixture.listener.accept()
    }

    /// Start a direct child while retaining this scope. This does not activate
    /// context transfer or guard descendant processes; those remain unsupported.
    pub fn spawn_fl<I, S>(self: &Arc<Self>, args: I) -> io::Result<FixtureChild>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.manifest_for_transfer()?;
        let child = crate::common::isolated_fl(self.root())?
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        Ok(FixtureChild {
            inner: ManagedFixtureChild::new(child, Arc::clone(self)),
        })
    }

    /// Run the isolated CLI synchronously while this scope remains borrowed.
    /// No raw Child or Command escapes; context transfer is not wired yet.
    pub fn output_fl<I, S>(&self, args: I) -> io::Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.manifest_for_transfer()?;
        crate::common::isolated_fl(self.root())?.args(args).output()
    }
}

// Windows cannot delete a read-only file during TempDir cleanup. Restore only
// that platform's writable attribute before the owned directory is removed.
#[cfg(windows)]
impl Drop for FixtureScope {
    fn drop(&mut self) {
        if let Ok(metadata) = std::fs::metadata(&self.manifest) {
            let mut permissions = metadata.permissions();
            permissions.set_readonly(false);
            let _ = std::fs::set_permissions(&self.manifest, permissions);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone)]
    struct FakeProcess {
        events: std::rc::Rc<std::cell::RefCell<Vec<&'static str>>>,
        kill_fails: bool,
        wait_fails: bool,
    }

    impl FixtureProcess for FakeProcess {
        type Status = ();
        fn try_wait(&mut self) -> io::Result<Option<()>> {
            self.events.borrow_mut().push("try_wait");
            Ok(None)
        }
        fn kill(&mut self) -> io::Result<()> {
            self.events.borrow_mut().push("kill");
            if self.kill_fails {
                Err(io::Error::other("kill failed"))
            } else {
                Ok(())
            }
        }
        fn wait(&mut self) -> io::Result<()> {
            self.events.borrow_mut().push("wait");
            if self.wait_fails {
                Err(io::Error::other("wait failed"))
            } else {
                Ok(())
            }
        }
    }

    struct FakeScope(std::rc::Rc<std::cell::RefCell<Vec<&'static str>>>);
    impl Drop for FakeScope {
        fn drop(&mut self) {
            self.0.borrow_mut().push("scope_dropped");
        }
    }

    #[test]
    fn child_drop_reaps_before_releasing_scope() {
        let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let child = ManagedFixtureChild::new(
            FakeProcess {
                events: events.clone(),
                kill_fails: false,
                wait_fails: false,
            },
            FakeScope(events.clone()),
        );
        drop(child);
        assert_eq!(
            &*events.borrow(),
            &["try_wait", "kill", "wait", "scope_dropped"]
        );
    }

    #[test]
    fn child_drop_preserves_scope_when_reap_cannot_be_proven() {
        let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let child = ManagedFixtureChild::new(
            FakeProcess {
                events: events.clone(),
                kill_fails: true,
                wait_fails: false,
            },
            FakeScope(events.clone()),
        );
        drop(child);
        assert_eq!(&*events.borrow(), &["try_wait", "kill", "try_wait"]);
    }

    #[test]
    fn child_drop_preserves_scope_if_wait_fails_after_kill() {
        let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let child = ManagedFixtureChild::new(
            FakeProcess {
                events: events.clone(),
                kill_fails: false,
                wait_fails: true,
            },
            FakeScope(events.clone()),
        );
        drop(child);
        assert_eq!(&*events.borrow(), &["try_wait", "kill", "wait"]);
    }

    #[test]
    fn explicit_wait_reaps_before_releasing_scope() {
        let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut child = ManagedFixtureChild::new(
            FakeProcess {
                events: events.clone(),
                kill_fails: false,
                wait_fails: false,
            },
            FakeScope(events.clone()),
        );
        child.wait().unwrap();
        assert_eq!(&*events.borrow(), &["wait"]);
        drop(child);
        assert_eq!(&*events.borrow(), &["wait", "scope_dropped"]);
    }

    // CI-only: runs the isolated fl child. Local audited filters exclude it.
    #[test]
    fn direct_child_retains_scope_until_reaped_ci_only() {
        let scope = Arc::new(FixtureScopeBuilder::new().unwrap().finish().unwrap());
        let root = scope.root().to_path_buf();
        let mut child = scope.spawn_fl(["--help"]).unwrap();
        drop(scope);
        assert!(root.exists());
        assert!(child.wait().unwrap().success());
        assert!(root.exists());
        drop(child);
        assert!(!root.exists());
    }

    #[test]
    fn owns_manifest_and_exact_file_resources_until_drop() {
        let mut builder = FixtureScopeBuilder::new().unwrap();
        let route = builder.create_route("routes.json", b"{}").unwrap();
        let executable = builder
            .create_executable("fixture-tool", b"fixture")
            .unwrap();
        let socket = builder
            .creation_target("wire.sock", CreationTarget::UnixSocket)
            .unwrap();
        let scope = builder.finish().unwrap();
        let root = scope.root().to_path_buf();
        assert!(scope.manifest_for_transfer().unwrap().is_file());
        assert!(
            scope
                .context()
                .check(&FixtureIdentity::Executable {
                    path: executable,
                    access: PathAccess::Existing
                })
                .is_ok()
        );
        assert!(
            scope
                .context()
                .check(&FixtureIdentity::Route {
                    path: route,
                    access: PathAccess::Existing
                })
                .is_ok()
        );
        assert!(
            scope
                .context()
                .check(&FixtureIdentity::UnixSocket {
                    path: socket,
                    access: PathAccess::Create
                })
                .is_ok()
        );
        assert!(
            scope
                .context()
                .check(&FixtureIdentity::Route {
                    path: root.join("other"),
                    access: PathAccess::Create
                })
                .is_err()
        );
        drop(scope);
        assert!(!root.exists());
    }

    #[test]
    fn changed_finalized_manifest_is_refused_before_transfer() {
        let scope = FixtureScopeBuilder::new().unwrap().finish().unwrap();
        let path = scope.manifest_for_transfer().unwrap().to_path_buf();
        let root = scope.root().to_path_buf();
        assert!(std::fs::metadata(&path).unwrap().permissions().readonly());

        // Simulate an accidental change after someone makes the file writable.
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            permissions.set_mode(0o600);
        }
        #[cfg(not(unix))]
        permissions.set_readonly(false);
        std::fs::set_permissions(&path, permissions).unwrap();
        std::fs::write(&path, b"changed").unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&path, permissions).unwrap();

        assert_eq!(
            scope.manifest_for_transfer().unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        drop(scope);
        assert!(!root.exists());
    }

    #[test]
    fn failed_finalization_cleans_up_the_root() {
        let mut builder = FixtureScopeBuilder::new().unwrap();
        let root = builder.root.path().to_path_buf();
        builder
            .creation_target("routes", CreationTarget::Route)
            .unwrap();
        builder.create_route("routes", b"{}").unwrap();
        assert!(builder.finish().is_err());
        assert!(!root.exists());
    }

    // CI-only: exercises a helper-owned private listener. Do not include this
    // in locally audited process-free test filters.
    #[test]
    fn tcp_fixture_owned_listener_ci_only() {
        let mut builder = FixtureScopeBuilder::new().unwrap();
        let address = builder.bind_tcp(TcpFixturePurpose::WirePeer).unwrap();
        let server_address = builder.bind_tcp(TcpFixturePurpose::Vendor).unwrap();
        assert!(builder.bind_tcp(TcpFixturePurpose::WirePeer).is_err());
        let scope = builder.finish().unwrap();
        let identity = FixtureIdentity::Tcp {
            purpose: TcpFixturePurpose::WirePeer.as_str().into(),
            address,
        };
        assert!(scope.context().check(&identity).is_ok());
        assert!(
            scope
                .context()
                .check(&FixtureIdentity::Tcp {
                    purpose: TcpFixturePurpose::Store.as_str().into(),
                    address,
                })
                .is_err()
        );
        let wrong_address = SocketAddr::new(address.ip(), if address.port() == 1 { 2 } else { 1 });
        assert!(
            scope
                .context()
                .check(&FixtureIdentity::Tcp {
                    purpose: TcpFixturePurpose::WirePeer.as_str().into(),
                    address: wrong_address,
                })
                .is_err()
        );
        // A second listener cannot acquire the address while the scope owns it.
        assert!(TcpListener::bind(address).is_err());
        let client = TcpStream::connect(server_address).unwrap();
        let (accepted, _) = scope.accept_tcp(TcpFixturePurpose::Vendor).unwrap();
        drop(client);
        drop(accepted);
        drop(scope);
        // Rebind the unused fixture, avoiding TIME_WAIT from the connected one.
        assert!(TcpListener::bind(address).is_ok());
    }

    #[test]
    fn rejects_duplicate_reserved_and_escaping_targets() {
        let mut builder = FixtureScopeBuilder::new().unwrap();
        builder
            .creation_target("routes", CreationTarget::Route)
            .unwrap();
        assert!(
            builder
                .creation_target("routes", CreationTarget::UnixSocket)
                .is_err()
        );
        assert!(
            builder
                .creation_target("../outside", CreationTarget::Route)
                .is_err()
        );
        assert!(
            builder
                .creation_target("manifest.json", CreationTarget::Route)
                .is_err()
        );
    }
}
