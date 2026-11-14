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
const OPERATION_MANIFEST_VERSION: u32 = 2;
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

/// Explicit permission for one metadata operation and its missing-file result.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetadataGrant {
    pub allowed: bool,
    pub allow_not_found: bool,
}

/// A requested metadata operation; grants are independently checked.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SiblingOperation {
    NoFollow,
    Follow,
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
    ConfigRead {
        path: PathBuf,
        allow_not_found: bool,
    },
    RedbSiblingMetadata {
        redb_path: PathBuf,
        path: PathBuf,
        no_follow: MetadataGrant,
        follow: MetadataGrant,
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
        if manifest.version != MANIFEST_VERSION && manifest.version != OPERATION_MANIFEST_VERSION {
            return Err(refused("unsupported fixture manifest version"));
        }
        if manifest.version == MANIFEST_VERSION && manifest.identities.iter().any(is_v2_identity) {
            return Err(refused(
                "fixture operation identities require manifest version 2",
            ));
        }
        // Validate relationships before inserting any identities; declaration order
        // does not change the authority or the collision checks.
        if manifest.version == OPERATION_MANIFEST_VERSION {
            for identity in &manifest.identities {
                match identity {
                    FixtureIdentity::ConfigRead { path, .. } => {
                        strict_resource_path(root, path)?;
                    }
                    FixtureIdentity::RedbSiblingMetadata {
                        redb_path, path, ..
                    } => {
                        strict_resource_path(root, redb_path)?;
                        strict_resource_path(root, path)?;
                        if *path != redb_path.with_extension("fls")
                            || !manifest.identities.contains(&FixtureIdentity::Store {
                                backend: StoreBackend::Redb,
                                path: redb_path.clone(),
                                access: PathAccess::Existing,
                            })
                        {
                            return Err(refused(
                                "sibling metadata requires its exact registered redb store",
                            ));
                        }
                    }
                    _ => {}
                }
            }
        }
        let mut identities = HashSet::new();
        let mut resources = HashSet::new();
        for identity in manifest.identities {
            validate_identity(root, &identity)?;
            if !resources.insert(if manifest.version == OPERATION_MANIFEST_VERSION {
                strict_resource_key(root, &identity)?
            } else {
                resource_key(&identity)?
            }) || !identities.insert(identity)
            {
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

    /// Validate an exact config-read grant and the current file shape. This is
    /// only a snapshot; a future reader must retain ownership and recheck I/O.
    pub fn check_config_read(&self, identity: &FixtureIdentity) -> io::Result<()> {
        let FixtureIdentity::ConfigRead {
            path,
            allow_not_found,
        } = identity
        else {
            return Err(refused("fixture identity does not grant a config read"));
        };
        self.check(identity)?;
        strict_leaf(path, *allow_not_found)
    }

    /// Check one independently granted sibling metadata operation at this instant.
    pub fn check_sibling_metadata(
        &self,
        identity: &FixtureIdentity,
        operation: SiblingOperation,
    ) -> io::Result<()> {
        let FixtureIdentity::RedbSiblingMetadata {
            path,
            no_follow,
            follow,
            ..
        } = identity
        else {
            return Err(refused("fixture identity does not grant sibling metadata"));
        };
        let grant = match operation {
            SiblingOperation::NoFollow => no_follow,
            SiblingOperation::Follow => follow,
        };
        if !grant.allowed {
            return Err(refused("sibling metadata operation is not granted"));
        }
        self.check(identity)?;
        if let FixtureIdentity::RedbSiblingMetadata { redb_path, .. } = identity {
            self.check(&FixtureIdentity::Store {
                backend: StoreBackend::Redb,
                path: redb_path.clone(),
                access: PathAccess::Existing,
            })?;
        }
        strict_leaf(path, grant.allow_not_found)
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
        FixtureIdentity::ConfigRead { path, .. } => {
            strict_resource_path(root, path)?;
            Ok(())
        }
        FixtureIdentity::RedbSiblingMetadata {
            redb_path, path, ..
        } => {
            strict_resource_path(root, redb_path)?;
            strict_resource_path(root, path)?;
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
        FixtureIdentity::ConfigRead { path, .. }
        | FixtureIdentity::RedbSiblingMetadata { path, .. } => Ok(ResourceKey::Path(path.clone())),
        FixtureIdentity::Tcp { address, .. } => Ok(ResourceKey::Tcp(*address)),
        FixtureIdentity::WindowsPipe { name, .. } => {
            Ok(ResourceKey::WindowsPipe(name.to_ascii_lowercase()))
        }
    }
}

fn is_v2_identity(identity: &FixtureIdentity) -> bool {
    matches!(
        identity,
        FixtureIdentity::ConfigRead { .. } | FixtureIdentity::RedbSiblingMetadata { .. }
    )
}

// Strict v2 resolution uses an existing immediate parent and distinguishes a
// genuinely missing leaf from all other errors before deduplication. No leaf
// symlink is accepted, including dangling links. This is not an ownership proof.
fn strict_resource_path(root: &Path, path: &Path) -> io::Result<PathBuf> {
    crate::validate_absolute(path)?;
    let canonical_root = std::fs::canonicalize(root).map_err(refused)?;
    let parent = path
        .parent()
        .ok_or_else(|| refused("fixture path has no parent"))?;
    let parent_meta = std::fs::symlink_metadata(parent).map_err(refused)?;
    if !parent_meta.is_dir() {
        return Err(refused("fixture parent is not a directory"));
    }
    let canonical_parent = std::fs::canonicalize(parent).map_err(refused)?;
    if !canonical_parent.starts_with(&canonical_root) {
        return Err(refused("fixture parent is outside root"));
    }
    let name = path
        .file_name()
        .ok_or_else(|| refused("fixture path has no leaf"))?;
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(refused("fixture leaf is a symlink"));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(refused(error)),
    }
    Ok(canonical_parent.join(name))
}

fn strict_leaf(path: &Path, allow_not_found: bool) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => {
            Err(refused("fixture input must be a regular file"))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound && allow_not_found => Ok(()),
        Err(error) => Err(refused(error)),
    }
}

fn strict_resource_key(root: &Path, identity: &FixtureIdentity) -> io::Result<ResourceKey> {
    match identity {
        FixtureIdentity::Store { path, .. }
        | FixtureIdentity::Route { path, .. }
        | FixtureIdentity::UnixSocket { path, .. }
        | FixtureIdentity::Executable { path, .. }
        | FixtureIdentity::ConfigRead { path, .. }
        | FixtureIdentity::RedbSiblingMetadata { path, .. } => {
            Ok(ResourceKey::Path(strict_resource_path(root, path)?))
        }
        _ => resource_key(identity),
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

    fn v2_grant(allowed: bool, allow_not_found: bool) -> MetadataGrant {
        MetadataGrant {
            allowed,
            allow_not_found,
        }
    }

    #[test]
    fn v2_operations_exact_grants_missing_and_order_independent() {
        let root = tempfile::tempdir().unwrap();
        let store = root.path().join("data.redb");
        std::fs::write(&store, b"synthetic").unwrap();
        let config = FixtureIdentity::ConfigRead {
            path: root.path().join("config"),
            allow_not_found: true,
        };
        let sibling = FixtureIdentity::RedbSiblingMetadata {
            redb_path: store.clone(),
            path: store.with_extension("fls"),
            no_follow: v2_grant(true, true),
            follow: v2_grant(false, false),
        };
        let store_id = FixtureIdentity::Store {
            backend: StoreBackend::Redb,
            path: store.clone(),
            access: PathAccess::Existing,
        };
        let path = manifest(
            root.path(),
            json!({"version": 2, "identities": [&sibling, &config, &store_id]}),
        );
        let context = FixtureContext::load(root.path(), &path).unwrap();
        assert!(context.check_config_read(&config).is_ok());
        assert!(
            context
                .check_sibling_metadata(&sibling, SiblingOperation::NoFollow)
                .is_ok()
        );
        assert!(
            context
                .check_sibling_metadata(&sibling, SiblingOperation::Follow)
                .is_err()
        );
        assert!(context.check_config_read(&sibling).is_err());
        assert!(
            context
                .check(&FixtureIdentity::Store {
                    backend: StoreBackend::Ferro,
                    path: store.with_extension("fls"),
                    access: PathAccess::Existing
                })
                .is_err()
        );
        assert!(
            context
                .check(&FixtureIdentity::Route {
                    path: root.path().join("config"),
                    access: PathAccess::Create
                })
                .is_err()
        );
        let wrong = FixtureIdentity::ConfigRead {
            path: root.path().join("config"),
            allow_not_found: false,
        };
        assert!(context.check_config_read(&wrong).is_err());
        let bad = FixtureIdentity::RedbSiblingMetadata {
            redb_path: store.clone(),
            path: store.with_extension("fls"),
            no_follow: v2_grant(true, false),
            follow: v2_grant(false, false),
        };
        assert!(
            context
                .check_sibling_metadata(&bad, SiblingOperation::NoFollow)
                .is_err()
        );
        assert!(!store.with_extension("fls").exists());
        assert_eq!(std::fs::read(&store).unwrap(), b"synthetic");
    }

    #[test]
    fn v2_manifest_rejects_wrong_version_policy_reference_duplicates_and_aliases() {
        let root = tempfile::tempdir().unwrap();
        let store = root.path().join("data.redb");
        std::fs::write(&store, b"fixture").unwrap();
        let store_id = json!({"kind":"store","backend":"redb","path":store,"access":"existing"});
        let sibling = json!({"kind":"redb_sibling_metadata","redb_path":store,"path":store.with_extension("fls"),"no_follow":{"allowed":true,"allow_not_found":true},"follow":{"allowed":false,"allow_not_found":false}});
        let config =
            json!({"kind":"config_read","path":root.path().join("config"),"allow_not_found":true});
        let cases = [
            json!({"version":1,"identities":[config.clone()]}),
            json!({"version":3,"identities":[]}),
            json!({"version":2,"identities":[{"kind":"config_read","path":root.path().join("config")}]}),
            json!({"version":2,"identities":[{"kind":"redb_sibling_metadata","redb_path":store,"path":store.with_extension("fls"),"no_follow":{"allowed":true},"follow":{"allowed":false,"allow_not_found":false}},store_id.clone()]}),
            json!({"version":2,"identities":[sibling.clone()]}),
            json!({"version":2,"identities":[sibling.clone(),{"kind":"store","backend":"ferro","path":store,"access":"existing"}]}),
            json!({"version":2,"identities":[sibling.clone(),store_id.clone(),sibling.clone()]}),
            json!({"version":2,"identities":[config.clone(),{"kind":"route","path":root.path().join("config"),"access":"create"}]}),
            json!({"version":2,"identities":[{"kind":"config_read","path":root.path().join("missing/child"),"allow_not_found":true}]}),
            json!({"version":2,"identities":[{"kind":"config_read","path":"relative","allow_not_found":true}]}),
            json!({"version":2,"identities":[{"kind":"config_read","path":root.path().join("../escape"),"allow_not_found":true}]}),
        ];
        for value in cases {
            let file = manifest(root.path(), value);
            assert!(FixtureContext::load(root.path(), &file).is_err());
        }
    }

    #[test]
    fn v2_rejects_normalized_aliases_and_unknown_operation_fields() {
        let root = tempfile::tempdir().unwrap();
        let a = root.path().join("config");
        let alias = root.path().join(".").join("config");
        for identities in [
            json!([{"kind":"config_read","path":a,"allow_not_found":true}, {"kind":"route","path":alias,"access":"create"}]),
            json!([{"kind":"config_read","path":a,"allow_not_found":true,"extra":1}]),
        ] {
            let file = manifest(root.path(), json!({"version":2,"identities":identities}));
            assert!(FixtureContext::load(root.path(), &file).is_err());
        }
    }

    #[test]
    fn v2_operation_policies_are_independent_and_rechecked_without_effects() {
        let root = tempfile::tempdir().unwrap();
        let store = root.path().join("state.redb");
        std::fs::write(&store, b"data").unwrap();
        let config_path = root.path().join("config");
        let sibling_path = store.with_extension("fls");
        let config = FixtureIdentity::ConfigRead {
            path: config_path.clone(),
            allow_not_found: false,
        };
        let sibling = FixtureIdentity::RedbSiblingMetadata {
            redb_path: store.clone(),
            path: sibling_path.clone(),
            no_follow: v2_grant(true, false),
            follow: v2_grant(true, true),
        };
        let store_id = FixtureIdentity::Store {
            backend: StoreBackend::Redb,
            path: store.clone(),
            access: PathAccess::Existing,
        };
        let file = manifest(
            root.path(),
            json!({"version":2,"identities":[&config, &sibling, &store_id]}),
        );
        let context = FixtureContext::load(root.path(), &file).unwrap();
        let before: HashSet<_> = std::fs::read_dir(root.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert!(context.check_config_read(&config).is_err());
        assert!(
            context
                .check_sibling_metadata(&sibling, SiblingOperation::NoFollow)
                .is_err()
        );
        assert!(
            context
                .check_sibling_metadata(&sibling, SiblingOperation::Follow)
                .is_ok()
        );
        let after: HashSet<_> = std::fs::read_dir(root.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(before, after);
        std::fs::write(&config_path, b"config").unwrap();
        std::fs::write(&sibling_path, b"sibling").unwrap();
        assert!(context.check_config_read(&config).is_ok());
        assert!(
            context
                .check_sibling_metadata(&sibling, SiblingOperation::NoFollow)
                .is_ok()
        );
        std::fs::remove_file(&store).unwrap();
        assert!(
            context
                .check_sibling_metadata(&sibling, SiblingOperation::Follow)
                .is_err()
        );
        assert_eq!(std::fs::read(&sibling_path).unwrap(), b"sibling");
    }

    #[cfg(unix)]
    #[test]
    fn v2_operations_refuse_symlinks_and_unprovable_parents_before_dedup() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(root.path().join("absent"), root.path().join("leaf")).unwrap();
        symlink(outside.path(), root.path().join("parent")).unwrap();
        symlink(root.path().join("loop"), root.path().join("loop")).unwrap();
        std::fs::write(root.path().join("file"), b"keep").unwrap();
        for path in [
            root.path().join("leaf"),
            root.path().join("parent/config"),
            root.path().join("file/child"),
            root.path().join("loop/child"),
            root.path().join("loop/dir/child"),
        ] {
            let file = manifest(
                root.path(),
                json!({"version":2,"identities":[{"kind":"config_read","path":path,"allow_not_found":true}]}),
            );
            assert!(FixtureContext::load(root.path(), &file).is_err());
        }
        assert_eq!(std::fs::read(root.path().join("file")).unwrap(), b"keep");
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
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
            br#"{"version":3,"identities":[]}"#,
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
