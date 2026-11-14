//! Opt-in fixture manifest builder. This does not activate isolation in a child
//! or itself guard any I/O. Callers must retain the finalized scope until all
//! users of its resources have stopped.

#![allow(dead_code)]

use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Output;

use fl_fixture_path::{FixtureContext, FixtureIdentity, PathAccess, StoreBackend};
use tempfile::TempDir;

/// A typed resource that the helper is permitted to declare as a creation
/// target. The exact path is registered, not the entire fixture directory.
pub enum CreationTarget {
    Store(StoreBackend),
    Route,
    UnixSocket,
}

/// Owns all files it creates until finalization transfers them to the scope.
pub struct FixtureScopeBuilder {
    files: Vec<File>,
    identities: Vec<FixtureIdentity>,
    root: TempDir,
}

/// Owns the manifest, fixture files and directory for its entire lifetime.
/// It does not own subprocesses or listeners; those are deliberately deferred.
pub struct FixtureScope {
    files: Vec<File>,
    manifest: PathBuf,
    manifest_bytes: Vec<u8>,
    context: FixtureContext,
    // Fields drop in declaration order, keeping the root until after files.
    root: TempDir,
}

impl FixtureScopeBuilder {
    pub fn new() -> io::Result<Self> {
        Ok(Self {
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
