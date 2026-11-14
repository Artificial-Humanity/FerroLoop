//! Opt-in fixture manifest builder. This does not activate isolation in a child
//! or itself guard any I/O. Callers must retain the finalized scope until all
//! users of its resources have stopped.

#![allow(dead_code)]

use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, ExitStatus, Output, Stdio};
use std::sync::Arc;

use fl_fixture_path::{FixtureContext, FixtureIdentity, MetadataGrant, PathAccess, StoreBackend};
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
    version: u32,
    poisoned: bool,
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
            version: 1,
            poisoned: false,
            root: tempfile::tempdir()?,
        })
    }

    fn path(&self, name: &str) -> io::Result<PathBuf> {
        let mut components = Path::new(name).components();
        if name.eq_ignore_ascii_case("manifest.json")
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
            || name.ends_with('.')
            || !matches!(components.next(), Some(Component::Normal(part)) if part == OsStr::new(name))
            || components.next().is_some()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "fixture name must be a single unreserved path component",
            ));
        }
        Ok(self.root.path().join(name))
    }

    /// Create one direct child directory owned by this scope. This does not
    /// register the directory or authorize any of its descendants as resources.
    pub fn create_directory(&mut self, name: &str) -> io::Result<PathBuf> {
        let path = self.path(name)?;
        if self
            .identities
            .iter()
            .filter_map(identity_path)
            .any(|registered| {
                registered.file_name().is_some_and(|part| {
                    part.to_str()
                        .is_some_and(|part| part.eq_ignore_ascii_case(name))
                })
            })
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "fixture directory conflicts with a registered resource",
            ));
        }
        std::fs::create_dir(&path)?;
        Ok(path)
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

    /// Opt into operation identities explicitly. Existing builders remain v1.
    pub fn enable_operations_v2(&mut self) {
        self.version = 2;
    }

    /// Create and retain an exact helper-owned config input, or retain its
    /// parent for an explicitly permitted missing config. No child transfer.
    pub fn v2_config(
        &mut self,
        name: &str,
        contents: Option<&[u8]>,
        allow_not_found: bool,
    ) -> io::Result<PathBuf> {
        if self.version != 2 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "v2 not selected",
            ));
        }
        let path = self.path(name)?;
        require_absent(&path)?;
        if let Some(contents) = contents {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)?;
            file.write_all(contents)?;
            file.flush()?;
            self.files.push(file);
        }
        self.identities.push(FixtureIdentity::ConfigRead {
            path: path.clone(),
            allow_not_found,
        });
        Ok(path)
    }

    /// Create the exact config selected by the fixture's XDG_CONFIG_HOME.
    /// The exclusively created `fl` parent and config are owned by this scope;
    /// no other descendant is registered and no child context is transferred.
    pub fn v2_selected_config(
        &mut self,
        contents: Option<&[u8]>,
        allow_not_found: bool,
    ) -> io::Result<PathBuf> {
        if self.version != 2 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "v2 not selected",
            ));
        }
        let parent = self.create_directory("fl")?;
        let path = parent.join("config.toml");
        require_absent(&path)?;
        if let Some(contents) = contents {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)?;
            file.write_all(contents)?;
            file.flush()?;
            self.files.push(file);
        }
        self.identities.push(FixtureIdentity::ConfigRead {
            path: path.clone(),
            allow_not_found,
        });
        Ok(path)
    }

    /// Create retained synthetic store bytes and, optionally, its derived
    /// sibling. These bytes are not a Redb database or authority to open one.
    pub fn v2_redb_sibling(
        &mut self,
        name: &str,
        contents: Option<&[u8]>,
        no_follow: MetadataGrant,
        follow: MetadataGrant,
    ) -> io::Result<(PathBuf, PathBuf)> {
        if self.version != 2 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "v2 not selected",
            ));
        }
        let store = self.path(name)?;
        let sibling = store.with_extension("fls");
        if store == sibling || sibling.parent() != Some(self.root.path()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid sibling",
            ));
        }
        require_absent(&store)?;
        require_absent(&sibling)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&store)?;
        file.write_all(b"synthetic fixture")?;
        file.flush()?;
        self.files.push(file);
        if let Some(contents) = contents {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&sibling)?;
            file.write_all(contents)?;
            file.flush()?;
            self.files.push(file);
        }
        self.identities.push(FixtureIdentity::Store {
            backend: StoreBackend::Redb,
            path: store.clone(),
            access: PathAccess::Existing,
        });
        self.identities.push(FixtureIdentity::RedbSiblingMetadata {
            redb_path: store.clone(),
            path: sibling.clone(),
            no_follow,
            follow,
        });
        Ok((store, sibling))
    }

    /// Provision the fixed wire-prune Redb fixture. This is not a child runner
    /// or permission to use the fixture at a runtime I/O boundary.
    #[cfg(unix)]
    pub fn v2_wire_prune_redb(&mut self) -> io::Result<PathBuf> {
        use std::os::unix::fs::OpenOptionsExt;

        if self.poisoned {
            return Err(io::Error::other("fixture builder is poisoned"));
        }
        if self.version != 2 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "v2 not selected",
            ));
        }
        let store = self.path("wire-prune.redb")?;
        let sibling = self.path("wire-prune.fls")?;
        let root = self.root.path().canonicalize()?;
        for identity in &self.identities {
            let mut paths = Vec::new();
            if let Some(path) = identity_path(identity) {
                paths.push(path);
            }
            if let FixtureIdentity::RedbSiblingMetadata { redb_path, .. } = identity {
                paths.push(redb_path);
            }
            for path in paths {
                let parent = path.parent().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "registered resource has no parent",
                    )
                })?;
                let parent = parent.canonicalize().map_err(|e| {
                    io::Error::new(e.kind(), format!("preflight registered parent: {e}"))
                })?;
                if !parent.starts_with(&root) {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "registered parent is outside the fixture root",
                    ));
                }
                match std::fs::symlink_metadata(path) {
                    Ok(metadata) if metadata.file_type().is_symlink() => {
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            "registered resource is a symlink",
                        ));
                    }
                    Ok(_) => {}
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => {
                        return Err(io::Error::new(
                            e.kind(),
                            format!("preflight registered resource: {e}"),
                        ));
                    }
                }
                let name = path.file_name().and_then(OsStr::to_str).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "unprovable resource name")
                })?;
                if parent == root
                    && (name.eq_ignore_ascii_case("wire-prune.redb")
                        || name.eq_ignore_ascii_case("wire-prune.fls"))
                {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "fixture path conflicts with a declared resource",
                    ));
                }
            }
        }
        // Treat portable case aliases as collisions even on a case-sensitive host.
        for entry in std::fs::read_dir(&root)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_str().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "unprovable fixture directory entry",
                )
            })?;
            if name.eq_ignore_ascii_case("wire-prune.redb")
                || name.eq_ignore_ascii_case("wire-prune.fls")
            {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "fixture path already exists",
                ));
            }
        }
        require_absent(&store)?;
        require_absent(&sibling)?;

        // From the instant exclusive creation is attempted, every error is
        // terminal for this builder, and private writes may have occurred.
        self.poisoned = true;
        let result = (|| {
            let file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&store)
                .map_err(|e| {
                    io::Error::new(
                        e.kind(),
                        format!("exclusive Redb creation (private writes may have occurred): {e}"),
                    )
                })?;
            drop(file);
            let database = fl_store::RedbStore::open_with_retries(&store, 0).map_err(|e| {
                io::Error::other(format!(
                    "Redb initialization (private writes may have occurred): {e}"
                ))
            })?;
            drop(database);
            let metadata = std::fs::symlink_metadata(&store).map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!("post-initialization store check (private writes occurred): {e}"),
                )
            })?;
            let canonical = store.canonicalize().map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!("post-initialization canonicalization (private writes occurred): {e}"),
                )
            })?;
            if !metadata.file_type().is_file() || canonical.parent() != Some(root.as_path()) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "post-initialization store is not a contained regular file; private writes occurred",
                ));
            }
            require_absent(&sibling).map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!("post-initialization sibling check (private writes occurred): {e}"),
                )
            })?;
            Ok(())
        })();
        result?;
        let grant = MetadataGrant {
            allowed: true,
            allow_not_found: true,
        };
        self.identities.extend([
            FixtureIdentity::Store {
                backend: StoreBackend::Redb,
                path: store.clone(),
                access: PathAccess::Existing,
            },
            FixtureIdentity::RedbSiblingMetadata {
                redb_path: store.clone(),
                path: sibling,
                no_follow: grant.clone(),
                follow: grant,
            },
        ]);
        self.poisoned = false;
        Ok(store)
    }

    #[cfg(not(unix))]
    pub fn v2_wire_prune_redb(&mut self) -> io::Result<PathBuf> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Redb fixture provisioning is unsupported on this platform",
        ))
    }

    /// Write a manifest once and validate it with the same strict loader used by
    /// the eventual child. Builder consumption prevents further registrations.
    pub fn finish(self) -> io::Result<FixtureScope> {
        if self.poisoned {
            return Err(io::Error::other(
                "fixture builder is poisoned; private writes may have occurred",
            ));
        }
        let manifest = self.root.path().join("manifest.json");
        let bytes = serde_json::to_vec(
            &serde_json::json!({ "version": self.version, "identities": self.identities }),
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

fn require_absent(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "fixture path already exists",
        )),
    }
}

fn identity_path(identity: &FixtureIdentity) -> Option<&Path> {
    match identity {
        FixtureIdentity::Store { path, .. }
        | FixtureIdentity::Route { path, .. }
        | FixtureIdentity::UnixSocket { path, .. }
        | FixtureIdentity::Executable { path, .. } => Some(path),
        FixtureIdentity::ConfigRead { path, .. }
        | FixtureIdentity::RedbSiblingMetadata { path, .. } => Some(path),
        FixtureIdentity::Tcp { .. } | FixtureIdentity::WindowsPipe { .. } => None,
    }
}

// The direct-child runner cannot prove ownership of descendants. Only the
// top-level clap help path is supported: main handles `fl --help` in parsing,
// before executing any command. Keep this exact until process-tree ownership is
// implemented; a caller assertion or a similar-looking subcommand is not proof.
fn audited_fl_args<I, S>(args: I) -> io::Result<Vec<OsString>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args: Vec<OsString> = args
        .into_iter()
        .map(|arg| arg.as_ref().to_os_string())
        .collect();
    if args.len() == 1 && args[0] == OsStr::new("--help") {
        Ok(args)
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "fixture runner supports only the audited top-level --help path",
        ))
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
        let args = audited_fl_args(args)?;
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
        let args = audited_fl_args(args)?;
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

    #[test]
    fn selected_config_owned_exact_identity_and_lifetime() {
        let mut builder = FixtureScopeBuilder::new().unwrap();
        let root = builder.root.path().to_path_buf();
        assert!(builder.v2_selected_config(Some(b"data"), false).is_err());
        assert!(!root.join("fl").exists());
        builder.enable_operations_v2();
        let config = builder.v2_selected_config(Some(b"data"), false).unwrap();
        assert_eq!(config, root.join("fl/config.toml"));
        assert!(builder.v2_selected_config(None, true).is_err());
        let scope = Arc::new(builder.finish().unwrap());
        let exact = FixtureIdentity::ConfigRead {
            path: config.clone(),
            allow_not_found: false,
        };
        assert!(scope.context().check_config_read(&exact).is_ok());
        let wrong_policy = FixtureIdentity::ConfigRead {
            path: config.clone(),
            allow_not_found: true,
        };
        assert!(scope.context().check_config_read(&wrong_policy).is_err());
        let wrong_path = FixtureIdentity::ConfigRead {
            path: root.join("config.toml"),
            allow_not_found: false,
        };
        assert!(scope.context().check_config_read(&wrong_path).is_err());
        assert_eq!(std::fs::read(&config).unwrap(), b"data");
        let retained = Arc::clone(&scope);
        drop(scope);
        assert_eq!(std::fs::read(&config).unwrap(), b"data");
        drop(retained);
        assert!(!root.exists());
    }

    #[test]
    fn selected_config_missing_policy_is_exact_and_does_not_create_file() {
        for allow in [false, true] {
            let mut builder = FixtureScopeBuilder::new().unwrap();
            builder.enable_operations_v2();
            let root = builder.root.path().to_path_buf();
            let config = builder.v2_selected_config(None, allow).unwrap();
            assert!(root.join("fl").is_dir());
            assert!(!config.exists());
            let scope = builder.finish().unwrap();
            let exact = FixtureIdentity::ConfigRead {
                path: config.clone(),
                allow_not_found: allow,
            };
            assert_eq!(scope.context().check_config_read(&exact).is_ok(), allow);
            let other = FixtureIdentity::ConfigRead {
                path: config.clone(),
                allow_not_found: !allow,
            };
            assert!(scope.context().check_config_read(&other).is_err());
            assert!(!config.exists());
            drop(scope);
            assert!(!root.exists());
        }
    }

    #[test]
    fn selected_config_refuses_existing_parent_and_declared_resource() {
        let mut builder = FixtureScopeBuilder::new().unwrap();
        builder.enable_operations_v2();
        let root = builder.root.path().to_path_buf();
        builder
            .creation_target("FL", CreationTarget::Route)
            .unwrap();
        assert!(builder.v2_selected_config(Some(b"data"), false).is_err());
        assert!(!root.join("fl").exists());
        drop(builder);
        assert!(!root.exists());

        let mut builder = FixtureScopeBuilder::new().unwrap();
        builder.enable_operations_v2();
        let root = builder.root.path().to_path_buf();
        std::fs::create_dir(root.join("fl")).unwrap();
        std::fs::write(root.join("fl/config.toml"), b"unchanged").unwrap();
        assert!(builder.v2_selected_config(Some(b"data"), false).is_err());
        assert_eq!(
            std::fs::read(root.join("fl/config.toml")).unwrap(),
            b"unchanged"
        );
        drop(builder);
        assert!(!root.exists());
    }

    #[cfg(unix)]
    #[test]
    fn selected_config_refuses_symlink_parent_and_later_leaf() {
        use std::os::unix::fs::symlink;
        let mut builder = FixtureScopeBuilder::new().unwrap();
        builder.enable_operations_v2();
        let root = builder.root.path().to_path_buf();
        symlink(root.join("absent"), root.join("fl")).unwrap();
        assert!(builder.v2_selected_config(None, true).is_err());
        assert!(
            std::fs::symlink_metadata(root.join("fl"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        drop(builder);
        assert!(!root.exists());

        let mut builder = FixtureScopeBuilder::new().unwrap();
        builder.enable_operations_v2();
        let root = builder.root.path().to_path_buf();
        std::fs::create_dir(root.join("fl")).unwrap();
        symlink(root.join("absent"), root.join("fl/config.toml")).unwrap();
        assert!(builder.v2_selected_config(None, true).is_err());
        assert!(
            std::fs::symlink_metadata(root.join("fl/config.toml"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        drop(builder);
        assert!(!root.exists());

        let mut builder = FixtureScopeBuilder::new().unwrap();
        builder.enable_operations_v2();
        let config = builder.v2_selected_config(None, true).unwrap();
        let scope = builder.finish().unwrap();
        symlink(scope.root().join("absent"), &config).unwrap();
        let exact = FixtureIdentity::ConfigRead {
            path: config,
            allow_not_found: true,
        };
        assert!(scope.context().check_config_read(&exact).is_err());
    }

    #[test]
    fn v2_owned_builder_refuses_preexisting_paths_without_adopting_them() {
        let mut builder = FixtureScopeBuilder::new().unwrap();
        builder.enable_operations_v2();
        let root = builder.root.path().to_path_buf();
        std::fs::write(root.join("config"), b"external").unwrap();
        let grant = MetadataGrant {
            allowed: true,
            allow_not_found: true,
        };
        assert!(builder.v2_config("config", None, true).is_err());
        std::fs::write(root.join("state.fls"), b"external sibling").unwrap();
        assert!(
            builder
                .v2_redb_sibling("state.redb", None, grant.clone(), grant)
                .is_err()
        );
        assert!(!root.join("state.redb").exists());
        assert_eq!(std::fs::read(root.join("config")).unwrap(), b"external");
        assert_eq!(
            std::fs::read(root.join("state.fls")).unwrap(),
            b"external sibling"
        );
        for bad in ["../config", "manifest.json/.", "a/b"] {
            assert!(builder.v2_config(bad, None, true).is_err());
        }
        drop(builder);
        assert!(!root.exists());
    }

    #[test]
    fn v2_owned_roundtrip_retains_files_and_exact_grants_until_last_owner() {
        use fl_fixture_path::SiblingOperation;
        let mut builder = FixtureScopeBuilder::new().unwrap();
        let root = builder.root.path().to_path_buf();
        assert!(builder.v2_config("config", None, true).is_err());
        builder.enable_operations_v2();
        let config = builder.v2_config("config", Some(b"config"), false).unwrap();
        let allow = MetadataGrant {
            allowed: true,
            allow_not_found: true,
        };
        let deny = MetadataGrant {
            allowed: false,
            allow_not_found: false,
        };
        let (store, sibling) = builder
            .v2_redb_sibling("state.redb", None, allow.clone(), deny.clone())
            .unwrap();
        let scope = Arc::new(builder.finish().unwrap());
        let input = FixtureIdentity::ConfigRead {
            path: config.clone(),
            allow_not_found: false,
        };
        let metadata = FixtureIdentity::RedbSiblingMetadata {
            redb_path: store.clone(),
            path: sibling.clone(),
            no_follow: allow,
            follow: deny,
        };
        assert!(scope.context().check_config_read(&input).is_ok());
        assert!(
            scope
                .context()
                .check_sibling_metadata(&metadata, SiblingOperation::NoFollow)
                .is_ok()
        );
        assert!(
            scope
                .context()
                .check_sibling_metadata(&metadata, SiblingOperation::Follow)
                .is_err()
        );
        assert!(!sibling.exists());
        assert_eq!(std::fs::read(&store).unwrap(), b"synthetic fixture");
        let owner = Arc::clone(&scope);
        drop(scope);
        assert!(config.exists() && store.exists());
        drop(owner);
        assert!(!root.exists());
    }

    // These tests never pass preflight and cannot reach Redb initialization.
    #[cfg(unix)]
    #[test]
    fn prune_redb_negative_refuses_version_collisions_and_existing_paths() {
        let mut builder = FixtureScopeBuilder::new().unwrap();
        let root = builder.root.path().to_path_buf();
        assert!(builder.v2_wire_prune_redb().is_err());
        assert!(!builder.poisoned);
        builder.enable_operations_v2();
        builder
            .creation_target("WIRE-PRUNE.REDB", CreationTarget::Route)
            .unwrap();
        assert!(builder.v2_wire_prune_redb().is_err());
        assert!(!builder.poisoned);
        assert!(!root.join("wire-prune.redb").exists());

        let mut builder = FixtureScopeBuilder::new().unwrap();
        builder.enable_operations_v2();
        let root = builder.root.path().to_path_buf();
        builder
            .creation_target("wire-prune.fls", CreationTarget::Route)
            .unwrap();
        assert!(builder.v2_wire_prune_redb().is_err());
        assert!(!builder.poisoned);
        assert!(!root.join("wire-prune.redb").exists());

        for name in [
            "wire-prune.redb",
            "wire-prune.fls",
            "WIRE-PRUNE.REDB",
            "WIRE-PRUNE.FLS",
        ] {
            let mut builder = FixtureScopeBuilder::new().unwrap();
            builder.enable_operations_v2();
            let root = builder.root.path().to_path_buf();
            std::fs::write(root.join(name), b"unchanged").unwrap();
            assert!(builder.v2_wire_prune_redb().is_err());
            assert!(!builder.poisoned);
            assert_eq!(std::fs::read(root.join(name)).unwrap(), b"unchanged");
        }
    }

    #[cfg(unix)]
    #[test]
    fn prune_redb_negative_refuses_dangling_links_without_touching_them() {
        use std::os::unix::fs::symlink;
        for name in ["wire-prune.redb", "wire-prune.fls"] {
            let mut builder = FixtureScopeBuilder::new().unwrap();
            builder.enable_operations_v2();
            let root = builder.root.path().to_path_buf();
            symlink(root.join("absent"), root.join(name)).unwrap();
            assert!(builder.v2_wire_prune_redb().is_err());
            assert!(!builder.poisoned);
            assert!(
                std::fs::symlink_metadata(root.join(name))
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            if name.ends_with("fls") {
                assert!(!root.join("wire-prune.redb").exists());
            }
        }
    }

    #[test]
    fn prune_redb_negative_poison_refuses_finish_before_manifest_io() {
        let mut builder = FixtureScopeBuilder::new().unwrap();
        let root = builder.root.path().to_path_buf();
        builder.poisoned = true;
        // An existing manifest would otherwise produce AlreadyExists. The
        // poison must take precedence without reading or writing the manifest.
        std::fs::write(root.join("manifest.json"), b"unchanged").unwrap();
        let err = match builder.finish() {
            Ok(_) => panic!("poisoned builder finished"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("poisoned"));
    }

    // Positive store initialization and reopen are CI-only, pending a separate
    // execution ruling. No ordinary test invokes this initializer.
    #[cfg(unix)]
    #[test]
    #[ignore = "positive private Redb initialization requires separate CI approval"]
    fn prune_redb_positive_initialized_reopens_and_cleans_up_ci_only() {
        let mut builder = FixtureScopeBuilder::new().unwrap();
        builder.enable_operations_v2();
        let root = builder.root.path().to_path_buf();
        let path = builder.v2_wire_prune_redb().unwrap();
        let scope = Arc::new(builder.finish().unwrap());
        let reopened = fl_store::RedbStore::open_with_retries(&path, 0).unwrap();
        drop(reopened);
        assert!(
            scope
                .context()
                .check(&FixtureIdentity::Store {
                    backend: StoreBackend::Redb,
                    path: path.clone(),
                    access: PathAccess::Existing,
                })
                .is_ok()
        );
        let owner = Arc::clone(&scope);
        drop(scope);
        assert!(path.exists());
        drop(owner);
        assert!(!root.exists());
    }

    #[test]
    fn owned_layout_retains_directories_and_refuses_unregistered_descendants() {
        let mut builder = FixtureScopeBuilder::new().unwrap();
        let home = builder.create_directory("home").unwrap();
        let stores = builder.create_directory("stores").unwrap();
        let root = builder.root.path().to_path_buf();
        assert_eq!(
            home.canonicalize().unwrap().parent(),
            Some(root.canonicalize().unwrap().as_path())
        );
        assert_eq!(
            stores.canonicalize().unwrap().parent(),
            Some(root.canonicalize().unwrap().as_path())
        );

        let mut command = std::process::Command::new("unused-fixture-program");
        command.env("SHOULD_BE_CLEARED", "yes");
        crate::common::isolate_command(&mut command, &home).unwrap();
        let env: std::collections::HashMap<_, _> = command.get_envs().collect();
        assert!(!env.contains_key(OsStr::new("SHOULD_BE_CLEARED")));
        assert_eq!(env.get(OsStr::new("HOME")), Some(&Some(home.as_os_str())));
        assert_eq!(
            env.get(OsStr::new("PATH")),
            Some(&Some(home.join("no-bin").as_os_str()))
        );

        let scope = Arc::new(builder.finish().unwrap());
        assert!(
            scope
                .context()
                .check(&FixtureIdentity::Store {
                    backend: StoreBackend::Redb,
                    path: stores.join("fl.redb"),
                    access: PathAccess::Create,
                })
                .is_err()
        );
        let retained = Arc::clone(&scope);
        drop(scope);
        assert!(home.is_dir() && stores.is_dir());
        drop(retained);
        assert!(!root.exists());
    }

    #[test]
    fn owned_layout_rejects_aliases_duplicates_and_resource_conflicts() {
        let mut builder = FixtureScopeBuilder::new().unwrap();
        let root = builder.root.path().to_path_buf();
        let child = builder.create_directory("home").unwrap();
        for name in [
            "home",
            "manifest.json",
            "MANIFEST.JSON",
            "manifest.json/.",
            "manifest.json/",
            "../outside",
            "a/b",
            "a\\b",
            ".",
            "..",
            "name.",
            "name ",
        ] {
            assert!(builder.create_directory(name).is_err(), "accepted {name:?}");
        }
        builder
            .creation_target("reserved", CreationTarget::Route)
            .unwrap();
        assert!(builder.create_directory("reserved").is_err());
        assert!(builder.create_directory("RESERVED").is_err());
        assert!(!root.join("reserved").exists());
        std::fs::write(root.join("file"), b"unchanged").unwrap();
        assert!(builder.create_directory("file").is_err());
        assert_eq!(std::fs::read(root.join("file")).unwrap(), b"unchanged");
        assert!(child.is_dir());
        drop(builder);
        assert!(!root.exists());
    }

    #[cfg(unix)]
    #[test]
    fn owned_layout_rejects_existing_symlink() {
        use std::os::unix::fs::symlink;
        let mut builder = FixtureScopeBuilder::new().unwrap();
        let root = builder.root.path().to_path_buf();
        let link = root.join("link");
        symlink(root.join("missing"), &link).unwrap();
        assert!(builder.create_directory("link").is_err());
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn direct_child_runner_only_accepts_exact_top_level_help() {
        assert_eq!(
            audited_fl_args(["--help"]).unwrap(),
            [OsString::from("--help")]
        );
        for args in [
            vec![],
            vec!["wire", "daemon"],
            vec!["agent", "spin-up"],
            vec!["--help", "wire"],
            vec!["wire", "--help"],
            vec!["--session-supervisor"],
        ] {
            assert_eq!(
                audited_fl_args(args).unwrap_err().kind(),
                io::ErrorKind::PermissionDenied
            );
        }
    }

    #[test]
    fn both_fixture_runners_refuse_descendant_capable_commands_before_spawn() {
        let scope = Arc::new(FixtureScopeBuilder::new().unwrap().finish().unwrap());
        assert_eq!(
            scope.output_fl(["wire", "daemon"]).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        let error = match scope.spawn_fl(["agent", "spin-up"]) {
            Ok(_) => panic!("a vendor-spawning command must be refused"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

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
