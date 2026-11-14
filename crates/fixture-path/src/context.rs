//! Explicit fixture identities transferred to a test child.
//!
//! The manifest is an accident-prevention convention, not authentication.
//! A parent helper must create it and retain the registered fixtures until its
//! child exits. This module neither establishes that provenance nor protects
//! against same-user replacement. It is intentionally not wired to environment
//! variables or runtime I/O here.

use std::collections::HashSet;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{check_endpoint, refused};

const MANIFEST_VERSION: u32 = 1;
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;

/// Physical store backends registered by a fixture helper.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoreBackend {
    Ferro,
    Redb,
}

/// Whether a path must already exist or is an explicitly permitted creation target.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathAccess {
    Existing,
    Create,
}

/// An exact, purpose-specific fixture identity. Root membership alone never
/// registers an identity. Network addresses are numeric and loopback-only;
/// they still require explicit registration and a parent-owned listener.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum FixtureIdentity {
    Store {
        backend: StoreBackend,
        path: PathBuf,
        access: PathAccess,
    },
    Route {
        path: PathBuf,
        access: PathAccess,
    },
    UnixSocket {
        path: PathBuf,
        access: PathAccess,
    },
    Executable {
        path: PathBuf,
        access: PathAccess,
    },
    Tcp {
        purpose: String,
        address: SocketAddr,
    },
    WindowsPipe {
        purpose: String,
        name: String,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    identities: Vec<FixtureIdentity>,
}

/// An in-memory snapshot of a helper-provided fixture manifest.
///
/// Constructing a context does not itself prove that its manifest or listeners
/// were created by a helper; that must be guaranteed by the eventual caller's
/// fixture ownership and transfer protocol. The exact registered identities
/// are private and cannot be extended after loading.
pub struct FixtureContext {
    root: PathBuf,
    identities: HashSet<FixtureIdentity>,
}

impl FixtureContext {
    /// Load a versioned manifest stored under an existing private fixture root.
    /// Refuses missing or invalid manifests, unknown fields, duplicate identities,
    /// and entries that are malformed or escape the fixture root.
    pub fn load(root: &Path, manifest_path: &Path) -> io::Result<Self> {
        check_endpoint(Some(root), root)?;
        check_endpoint(Some(root), manifest_path)?;
        let metadata = std::fs::metadata(manifest_path).map_err(refused)?;
        if !metadata.is_file() || metadata.len() > MAX_MANIFEST_BYTES {
            return Err(refused("fixture manifest is not a permitted file"));
        }
        let bytes = std::fs::read(manifest_path).map_err(refused)?;
        if bytes.len() as u64 > MAX_MANIFEST_BYTES {
            return Err(refused("fixture manifest is too large"));
        }
        let manifest: Manifest =
            serde_json::from_slice(&bytes).map_err(|_| refused("fixture manifest is malformed"))?;
        if manifest.version != MANIFEST_VERSION {
            return Err(refused("unsupported fixture manifest version"));
        }
        let mut identities = HashSet::new();
        let mut resources = HashSet::new();
        for identity in manifest.identities {
            validate_identity(root, &identity)?;
            if !resources.insert(resource_key(&identity)?) || !identities.insert(identity) {
                return Err(refused("duplicate or conflicting fixture identity"));
            }
        }
        Ok(Self {
            root: root.to_path_buf(),
            identities,
        })
    }

    /// Check the exact registered identity and recheck its current filesystem
    /// containment where applicable. This does not perform or authorize I/O by
    /// itself, or replace a check at the actual I/O boundary.
    pub fn check(&self, identity: &FixtureIdentity) -> io::Result<()> {
        validate_identity(&self.root, identity)?;
        if !self.identities.contains(identity) {
            return Err(refused("fixture identity is not registered"));
        }
        Ok(())
    }

    /// Check an exact registered store or route creation target at this instant.
    ///
    /// Success is only a filesystem snapshot: it neither authorizes nor performs
    /// creation or a later open. Paths can change immediately afterwards; a
    /// future I/O boundary must retain helper ownership and recheck its target.
    pub fn check_create_snapshot(&self, identity: &FixtureIdentity) -> io::Result<()> {
        let path = match identity {
            FixtureIdentity::Store {
                path,
                access: PathAccess::Create,
                ..
            }
            | FixtureIdentity::Route {
                path,
                access: PathAccess::Create,
            } => path,
            _ => {
                return Err(refused(
                    "fixture identity is not a store or route creation target",
                ));
            }
        };
        self.check(identity)?;
        let parent = path
            .parent()
            .ok_or_else(|| refused("fixture target has no parent"))?;
        check_endpoint(Some(&self.root), parent)?;
        if !std::fs::metadata(parent).map_err(refused)?.is_dir() {
            return Err(refused("fixture target parent is not a directory"));
        }
        match std::fs::symlink_metadata(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(refused(error)),
            Ok(_) => Err(refused("fixture creation target already exists")),
        }
    }
}

fn validate_identity(root: &Path, identity: &FixtureIdentity) -> io::Result<()> {
    match identity {
        FixtureIdentity::Store { path, access, .. }
        | FixtureIdentity::Route { path, access }
        | FixtureIdentity::UnixSocket { path, access }
        | FixtureIdentity::Executable { path, access } => {
            check_endpoint(Some(root), path)?;
            if matches!(identity, FixtureIdentity::Executable { .. })
                && *access != PathAccess::Existing
            {
                return Err(refused("fixture executable must already exist"));
            }
            if *access == PathAccess::Existing {
                let metadata = std::fs::metadata(path).map_err(refused)?;
                if matches!(
                    identity,
                    FixtureIdentity::Executable { .. }
                        | FixtureIdentity::Store { .. }
                        | FixtureIdentity::Route { .. }
                ) && !metadata.is_file()
                {
                    return Err(refused("existing fixture must be a file"));
                }
                if matches!(identity, FixtureIdentity::UnixSocket { .. }) {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::FileTypeExt;
                        if !metadata.file_type().is_socket() {
                            return Err(refused("existing fixture must be a socket"));
                        }
                    }
                    #[cfg(not(unix))]
                    return Err(refused(
                        "existing Unix socket fixtures are unsupported on this platform",
                    ));
                }
            }
            Ok(())
        }
        FixtureIdentity::Tcp { purpose, address } => {
            validate_purpose(purpose)?;
            if !address.ip().is_loopback() || address.port() == 0 {
                return Err(refused(
                    "fixture TCP address must be an exact loopback listener",
                ));
            }
            Ok(())
        }
        FixtureIdentity::WindowsPipe { purpose, name } => {
            validate_purpose(purpose)?;
            let Some(suffix) = name.strip_prefix(r"\\.\pipe\") else {
                return Err(refused("fixture pipe must be a full Windows pipe identity"));
            };
            if suffix.is_empty() || suffix.contains(['\\', '/', '\0']) {
                return Err(refused("fixture pipe must be a full Windows pipe identity"));
            }
            Ok(())
        }
    }
}

#[derive(Eq, Hash, PartialEq)]
enum ResourceKey {
    Path(PathBuf),
    Tcp(SocketAddr),
    WindowsPipe(String),
}

// Resolve existing components so aliases cannot register the same resource
// twice under different kinds, access modes, or purposes.
fn resource_key(identity: &FixtureIdentity) -> io::Result<ResourceKey> {
    match identity {
        FixtureIdentity::Store { path, .. }
        | FixtureIdentity::Route { path, .. }
        | FixtureIdentity::UnixSocket { path, .. }
        | FixtureIdentity::Executable { path, .. } => {
            let mut ancestor = path.clone();
            let mut missing = Vec::new();
            while !ancestor.exists() {
                let name = ancestor
                    .file_name()
                    .ok_or_else(|| refused("invalid fixture path"))?;
                missing.push(name.to_os_string());
                ancestor.pop();
            }
            let mut canonical = std::fs::canonicalize(ancestor).map_err(refused)?;
            for component in missing.iter().rev() {
                canonical.push(component);
            }
            Ok(ResourceKey::Path(canonical))
        }
        FixtureIdentity::Tcp { address, .. } => Ok(ResourceKey::Tcp(*address)),
        FixtureIdentity::WindowsPipe { name, .. } => {
            Ok(ResourceKey::WindowsPipe(name.to_ascii_lowercase()))
        }
    }
}

fn validate_purpose(purpose: &str) -> io::Result<()> {
    if purpose.is_empty() || purpose.trim() != purpose {
        return Err(refused("fixture identity purpose is invalid"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::is_fixture_endpoint_refusal;
    use serde_json::json;

    fn manifest(root: &Path, value: serde_json::Value) -> PathBuf {
        let path = root.join("fixtures.json");
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        path
    }

    fn create_snapshot_context(root: &Path, identities: &[FixtureIdentity]) -> FixtureContext {
        let file = manifest(root, json!({"version": 1, "identities": identities}));
        FixtureContext::load(root, &file).unwrap()
    }

    fn route_target(path: PathBuf) -> FixtureIdentity {
        FixtureIdentity::Route {
            path,
            access: PathAccess::Create,
        }
    }

    fn assert_snapshot_refused(context: &FixtureContext, identity: &FixtureIdentity) {
        let error = context
            .check_create_snapshot(identity)
            .expect_err("target must be refused");
        assert!(is_fixture_endpoint_refusal(&error), "{error}");
    }

    #[test]
    fn create_snapshot_accepts_only_exact_registered_store_and_route_without_effects() {
        let root = tempfile::tempdir().unwrap();
        let route = route_target(root.path().join("route"));
        let store = FixtureIdentity::Store {
            backend: StoreBackend::Ferro,
            path: root.path().join("store"),
            access: PathAccess::Create,
        };
        let socket = FixtureIdentity::UnixSocket {
            path: root.path().join("socket"),
            access: PathAccess::Create,
        };
        let context =
            create_snapshot_context(root.path(), &[route.clone(), store.clone(), socket.clone()]);
        let before: HashSet<_> = std::fs::read_dir(root.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        context.check_create_snapshot(&route).unwrap();
        context.check_create_snapshot(&store).unwrap();
        assert_snapshot_refused(&context, &socket);
        assert_snapshot_refused(&context, &route_target(root.path().join("unregistered")));
        assert_snapshot_refused(
            &context,
            &FixtureIdentity::Store {
                backend: StoreBackend::Redb,
                path: root.path().join("store"),
                access: PathAccess::Create,
            },
        );
        assert_snapshot_refused(
            &context,
            &FixtureIdentity::Store {
                backend: StoreBackend::Ferro,
                path: root.path().join("store"),
                access: PathAccess::Existing,
            },
        );
        assert_snapshot_refused(
            &context,
            &FixtureIdentity::Route {
                path: root.path().join("store"),
                access: PathAccess::Create,
            },
        );
        let outside = tempfile::tempdir().unwrap();
        assert_snapshot_refused(&context, &route_target(outside.path().join("unregistered")));
        assert_snapshot_refused(&context, &route_target(PathBuf::from("relative")));
        assert_snapshot_refused(&context, &route_target(root.path().join("a/../route")));
        let after: HashSet<_> = std::fs::read_dir(root.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(before, after);
        assert!(!root.path().join("route").exists());
        assert!(!root.path().join("store").exists());
    }

    #[test]
    fn create_snapshot_refuses_existing_leaves_and_parent_failures_after_load() {
        let root = tempfile::tempdir().unwrap();
        let missing_parent = route_target(root.path().join("missing/target"));
        let non_directory = route_target(root.path().join("file-parent/target"));
        let file_target = route_target(root.path().join("arrived-file"));
        let dir_target = route_target(root.path().join("arrived-dir"));
        let identities = [
            missing_parent.clone(),
            non_directory.clone(),
            file_target.clone(),
            dir_target.clone(),
        ];
        let context = create_snapshot_context(root.path(), &identities);
        std::fs::write(root.path().join("file-parent"), b"parent").unwrap();
        std::fs::write(root.path().join("arrived-file"), b"payload").unwrap();
        std::fs::create_dir(root.path().join("arrived-dir")).unwrap();
        let before: HashSet<_> = std::fs::read_dir(root.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        for identity in &identities {
            assert_snapshot_refused(&context, identity);
        }
        let after: HashSet<_> = std::fs::read_dir(root.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(before, after);
        assert_eq!(
            std::fs::read(root.path().join("arrived-file")).unwrap(),
            b"payload"
        );
        assert!(!root.path().join("missing").exists());
    }

    #[cfg(unix)]
    #[test]
    fn create_snapshot_refuses_dangling_and_escaping_symlinks_without_effects() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let dangling_parent = route_target(root.path().join("dangling-parent/target"));
        let escape_parent = route_target(root.path().join("escape-parent/target"));
        let dangling_leaf = route_target(root.path().join("dangling-leaf"));
        let existing_leaf = route_target(root.path().join("existing-leaf"));
        let looping_leaf = route_target(root.path().join("looping-leaf"));
        let identities = [
            dangling_parent.clone(),
            escape_parent.clone(),
            dangling_leaf.clone(),
            existing_leaf.clone(),
            looping_leaf.clone(),
        ];
        let context = create_snapshot_context(root.path(), &identities);
        symlink(
            root.path().join("missing"),
            root.path().join("dangling-parent"),
        )
        .unwrap();
        symlink(outside.path(), root.path().join("escape-parent")).unwrap();
        symlink(
            root.path().join("missing"),
            root.path().join("dangling-leaf"),
        )
        .unwrap();
        symlink(outside.path(), root.path().join("existing-leaf")).unwrap();
        symlink(
            root.path().join("looping-leaf"),
            root.path().join("looping-leaf"),
        )
        .unwrap();
        let before: HashSet<_> = std::fs::read_dir(root.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        for identity in &identities {
            assert_snapshot_refused(&context, identity);
        }
        let after: HashSet<_> = std::fs::read_dir(root.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(before, after);
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[test]
    fn only_exact_registered_kind_value_and_purpose_are_accepted() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("wire.sock");
        let data = json!({"version": 1, "identities": [
            {"kind": "unix_socket", "path": path, "access": "create"},
            {"kind": "tcp", "purpose": "peer", "address": "127.0.0.1:4321"},
            {"kind": "windows_pipe", "purpose": "host", "name": r"\\.\pipe\fixture"}
        ]});
        let file = manifest(root.path(), data);
        let context = FixtureContext::load(root.path(), &file).unwrap();
        assert!(
            context
                .check(&FixtureIdentity::UnixSocket {
                    path: path.clone(),
                    access: PathAccess::Create
                })
                .is_ok()
        );
        assert!(
            context
                .check(&FixtureIdentity::Route {
                    path,
                    access: PathAccess::Create
                })
                .is_err()
        );
        let tcp = FixtureIdentity::Tcp {
            purpose: "peer".into(),
            address: "127.0.0.1:4321".parse().unwrap(),
        };
        assert!(context.check(&tcp).is_ok());
        assert!(
            context
                .check(&FixtureIdentity::Tcp {
                    purpose: "store".into(),
                    address: "127.0.0.1:4321".parse().unwrap()
                })
                .is_err()
        );
        assert!(
            context
                .check(&FixtureIdentity::Tcp {
                    purpose: "peer".into(),
                    address: "127.0.0.1:4322".parse().unwrap()
                })
                .is_err()
        );
        assert!(
            context
                .check(&FixtureIdentity::WindowsPipe {
                    purpose: "host".into(),
                    name: r"\\.\pipe\fixture".into()
                })
                .is_ok()
        );
        assert!(
            context
                .check(&FixtureIdentity::WindowsPipe {
                    purpose: "host".into(),
                    name: r"\\.\pipe\fixture-other".into()
                })
                .is_err()
        );
    }

    #[test]
    fn missing_malformed_unknown_duplicate_and_wrong_version_refuse() {
        let root = tempfile::tempdir().unwrap();
        assert!(FixtureContext::load(root.path(), &root.path().join("missing")).is_err());
        let path = root.path().join("manifest");
        for bytes in [
            br#"{"version":1,"identities":[],"unknown":true}"#.as_slice(),
            br#"{"version":1,"version":1,"identities":[]}"#,
            br#"{"version":2,"identities":[]}"#,
            br#"{"identities":[]}"#,
            br#"{"version":1,"identities":[{"kind":"unknown"}]}"#,
            br#"{"version":1,"identities":[{"kind":"tcp","purpose":"peer","address":"127.0.0.1:1234","extra":true}]}"#,
            br#"{"version":1,"identities":[{"kind":"tcp","purpose":"peer","address":"localhost:1234"}]}"#,
            br#"{"version":1,"identities":[{"kind":"tcp","purpose":"peer","address":"127.0.0.1:1234"},{"kind":"tcp","purpose":"peer","address":"127.0.0.1:1234"}]}"#,
            br#"{"version":1,"identities":[{"kind":"tcp","purpose":"peer","address":"8.8.8.8:53"}]}"#,
            br#"{"version":1,"identities":[{"kind":"windows_pipe","purpose":"host","name":"fixture"}]}"#,
        ] {
            std::fs::write(&path, bytes).unwrap();
            let error = match FixtureContext::load(root.path(), &path) {
                Ok(_) => panic!("invalid context accepted"),
                Err(error) => error,
            };
            assert!(is_fixture_endpoint_refusal(&error));
        }
    }

    #[test]
    fn existing_inputs_and_creation_targets_are_distinct_and_conflicts_refuse() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("route.json");
        let file = manifest(
            root.path(),
            json!({"version": 1, "identities": [
                {"kind":"route", "path": target, "access":"existing"}
            ]}),
        );
        assert!(FixtureContext::load(root.path(), &file).is_err());
        std::fs::write(&target, b"{}").unwrap();
        let context = FixtureContext::load(root.path(), &file).unwrap();
        assert!(
            context
                .check(&FixtureIdentity::Route {
                    path: target.clone(),
                    access: PathAccess::Existing
                })
                .is_ok()
        );
        assert!(
            context
                .check(&FixtureIdentity::Route {
                    path: target.clone(),
                    access: PathAccess::Create
                })
                .is_err()
        );
        let file = manifest(
            root.path(),
            json!({"version": 1, "identities": [
                {"kind":"route", "path": target, "access":"existing"},
                {"kind":"unix_socket", "path": target, "access":"create"}
            ]}),
        );
        assert!(FixtureContext::load(root.path(), &file).is_err());
    }

    #[test]
    fn outside_manifest_or_registered_path_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_file = manifest(outside.path(), json!({"version": 1, "identities": []}));
        assert!(FixtureContext::load(root.path(), &outside_file).is_err());
        let file = manifest(
            root.path(),
            json!({"version": 1, "identities": [{"kind":"route", "path": outside.path().join("routes"), "access":"create" }]}),
        );
        assert!(FixtureContext::load(root.path(), &file).is_err());
    }

    #[cfg(not(unix))]
    #[test]
    fn existing_unix_socket_fixture_is_refused_without_type_validation() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("not-a-socket");
        std::fs::write(&path, b"ordinary file").unwrap();
        let file = manifest(
            root.path(),
            json!({"version": 1, "identities": [
                {"kind": "unix_socket", "path": path, "access": "existing"}
            ]}),
        );
        let error = match FixtureContext::load(root.path(), &file) {
            Ok(_) => panic!("unverifiable Unix socket accepted"),
            Err(error) => error,
        };
        assert!(is_fixture_endpoint_refusal(&error));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_refused_at_load_and_at_check() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = root.path().join("fixture");
        let file = manifest(
            root.path(),
            json!({"version": 1, "identities": [{"kind":"unix_socket", "path": target, "access":"create" }]}),
        );
        let context = FixtureContext::load(root.path(), &file).unwrap();
        symlink(outside.path(), &target).unwrap();
        assert!(
            context
                .check(&FixtureIdentity::UnixSocket {
                    path: target,
                    access: PathAccess::Create
                })
                .is_err()
        );
        assert!(FixtureContext::load(root.path(), &file).is_err());
    }
}
