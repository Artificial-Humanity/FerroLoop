//! Durable restore admission fences. These do not exclude old binaries or cached handles.
use super::RecoveryError;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestoreGuardV1 {
    pub version: u32,
    pub operation_id: uuid::Uuid,
    pub physical_path: PathBuf,
    pub parent_identity: (u64, u64),
    pub journal: PathBuf,
    pub phase: String,
}

/// Resolve existing symlinks, including an existing parent of an absent target,
/// without creating any entry or contacting a daemon.
pub(crate) fn canonical_physical(path: &Path) -> Result<PathBuf, RecoveryError> {
    let resolution_error = |source| RecoveryError::PhysicalPathResolution {
        path: path.to_path_buf(),
        source,
    };
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(resolution_error)?
            .join(path)
    };
    let mut current = absolute.as_path();
    let mut suffix = Vec::new();
    loop {
        match fs::canonicalize(current) {
            Ok(mut resolved) => {
                for name in suffix.into_iter().rev() {
                    resolved.push(name);
                }
                return Ok(resolved);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // A dangling link is ambiguous, not an absent destination.
                match fs::symlink_metadata(current) {
                    Ok(_) => return Err(resolution_error(e)),
                    Err(missing) if missing.kind() == std::io::ErrorKind::NotFound => {}
                    Err(other) => return Err(resolution_error(other)),
                }
                if let (Some(parent), Some(name)) = (current.parent(), current.file_name()) {
                    suffix.push(name.to_os_string());
                    current = parent;
                } else {
                    return Err(resolution_error(e));
                }
            }
            Err(e) => return Err(resolution_error(e)),
        }
    }
}

pub(crate) fn guard_path(physical: &Path) -> PathBuf {
    let mut name = physical.as_os_str().to_os_string();
    name.push(".restore-guard");
    PathBuf::from(name)
}
/// Read-only observation of a present admission fence. An unknown operation ID
/// still fences admission; it never makes a malformed or unreadable guard safe.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RestoreGuardStatus {
    pub guard_path: PathBuf,
    pub operation_id: Option<uuid::Uuid>,
}

/// Inspect only the sidecar: bounded reads, no symlink following, no store open,
/// journal reads, repair or daemon contact. A metadata error counts as present.
pub fn inspect_restore_guard(
    physical_path: &Path,
) -> Result<Option<RestoreGuardStatus>, RecoveryError> {
    let physical = canonical_physical(physical_path)?;
    let path = guard_path(&physical);
    let metadata = match fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        other => other,
    };
    let operation_id = (|| {
        let meta = metadata.ok()?;
        if !meta.is_file() || meta.len() > 65536 {
            return None;
        }
        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let file = options.open(&path).ok()?;
        let held = file.metadata().ok()?;
        if !held.is_file() || held.len() > 65536 || file_identity(&held) != file_identity(&meta) {
            return None;
        }
        let mut bytes = Vec::new();
        file.take(65537).read_to_end(&mut bytes).ok()?;
        let guard: RestoreGuardV1 = serde_json::from_slice(&bytes).ok()?;
        (guard.version == 1 && !guard.operation_id.is_nil()).then_some(guard.operation_id)
    })();
    Ok(Some(RestoreGuardStatus {
        guard_path: path,
        operation_id,
    }))
}

/// Missing permits admission. Every present, unreadable or malformed guard
/// refuses; diagnostics contain only a validated operation ID, never its bytes.
pub fn check_restore_admission(physical_path: &Path) -> Result<(), RecoveryError> {
    match inspect_restore_guard(physical_path)? {
        None => Ok(()),
        Some(guard) => Err(RecoveryError::RestoreFenced {
            guard_path: guard.guard_path,
            operation_id: guard.operation_id,
        }),
    }
}

use super::manifest::BuildIdentity;
use super::{
    OperationLimits,
    fs::{self as recovery_fs, RecoveryFs},
};
use std::os::unix::fs::MetadataExt;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum OfflineAction {
    Backup,
    Restore {
        bundle_operation: uuid::Uuid,
        bundle_sha256: String,
    },
    Purge {
        quarantined_operation: uuid::Uuid,
        manifest_sha256: String,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum FileBinding {
    Absent,
    Readable {
        identity: (u64, u64),
        bytes: u64,
        sha256: String,
    },
    Forensic {
        identity: (u64, u64),
        bytes: u64,
        read_error: i32,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExclusionAttestation {
    pub legacy_and_cached_handles_excluded: bool,
    pub external_dispatch_excluded: bool,
    pub active_owner_excluded: bool,
    pub evidence: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OfflineLeaseRequest {
    pub version: u32,
    pub operation_id: uuid::Uuid,
    pub action: OfflineAction,
    pub catalog_identity: String,
    pub approved_root_identity: (u64, u64),
    pub physical_path: PathBuf,
    pub parent_identity: (u64, u64),
    pub expected_file: FileBinding,
    pub profile_sha256: String,
    pub build: BuildIdentity,
    pub issued_at_unix_ms: u128,
    pub expires_at_unix_ms: u128,
    pub operator: String,
    pub exclusion: ExclusionAttestation,
}
/// An administrative attestation, not a cryptographic token or P1 ownership.
/// Execution obtains process-local operation ownership separately.
#[derive(Debug)]
pub struct OfflineLease {
    pub(crate) request: OfflineLeaseRequest,
    path: PathBuf,
}
impl OfflineLease {
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn validate(&self, fs: &dyn RecoveryFs, l: &OperationLimits) -> Result<(), RecoveryError> {
        self.validate_attestation(fs, l)?;
        if inspect_file_binding(&self.request.physical_path, l)? != self.request.expected_file {
            return Err(recovery_fs::refused(
                l,
                "lease current-file binding changed",
            ));
        }
        Ok(())
    }
    pub(crate) fn validate_attestation(
        &self,
        fs: &dyn RecoveryFs,
        l: &OperationLimits,
    ) -> Result<(), RecoveryError> {
        validate_request(&self.request, fs, l)?;
        let root = recovery_fs::Dir::open(fs.root(), l)?;
        if self.path.parent() != Some(root.path.as_path()) {
            return Err(l.verification());
        }
        let name = self
            .path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| l.verification())?;
        let persisted: OfflineLeaseRequest = recovery_fs::read_json(&root, name, l)?;
        if persisted != self.request {
            return Err(l.verification());
        }
        Ok(())
    }
    pub fn load(
        path: &Path,
        fs: &dyn RecoveryFs,
        l: &OperationLimits,
    ) -> Result<Self, RecoveryError> {
        let root = recovery_fs::Dir::open(fs.root(), l)?;
        if path.parent() != Some(root.path.as_path()) {
            return Err(l.verification());
        }
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| l.verification())?;
        let request = recovery_fs::read_json(&root, name, l)?;
        let lease = Self {
            request,
            path: path.to_path_buf(),
        };
        lease.validate_attestation(fs, l)?;
        Ok(lease)
    }
}
pub(crate) fn file_identity(metadata: &std::fs::Metadata) -> (u64, u64) {
    (metadata.dev(), metadata.ino())
}

#[cfg(test)]
fn now_ms() -> Result<u128, RecoveryError> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .map_err(|_| RecoveryError::Refused("clock unavailable".into()))
}
fn validate_request(
    r: &OfflineLeaseRequest,
    fs: &dyn RecoveryFs,
    l: &OperationLimits,
) -> Result<(), RecoveryError> {
    l.check()?;
    let now = fs.io().unix_ms().map_err(|_| recovery_fs::destination(l))?;
    let root = recovery_fs::Dir::open(fs.root(), l)?;
    let physical = canonical_physical(&r.physical_path)?;
    let parent = recovery_fs::Dir::open(physical.parent().ok_or_else(|| l.verification())?, l)?;
    let action_valid = match &r.action {
        OfflineAction::Restore {
            bundle_operation,
            bundle_sha256,
        } => !bundle_operation.is_nil() && super::manifest::sha256(bundle_sha256),
        OfflineAction::Purge {
            quarantined_operation,
            manifest_sha256,
        } => !quarantined_operation.is_nil() && super::manifest::sha256(manifest_sha256),
        OfflineAction::Backup => !matches!(
            r.expected_file,
            FileBinding::Absent | FileBinding::Forensic { .. }
        ),
    };
    if !cfg!(target_os = "linux")
        || r.version != 1
        || r.operation_id.is_nil()
        || r.catalog_identity.is_empty()
        || r.operator.trim().is_empty()
        || r.build != BuildIdentity::current()
        || !super::manifest::sha256(&r.profile_sha256)
        || !action_valid
        || physical != r.physical_path
        || file_identity(&root.file.metadata().map_err(|_| l.verification())?)
            != r.approved_root_identity
        || file_identity(&parent.file.metadata().map_err(|_| l.verification())?)
            != r.parent_identity
        || r.expires_at_unix_ms
            .checked_sub(r.issued_at_unix_ms)
            .is_none_or(|n| n == 0 || n > 3_600_000)
        || now < r.issued_at_unix_ms
        || now >= r.expires_at_unix_ms
        || !r.exclusion.legacy_and_cached_handles_excluded
        || !r.exclusion.external_dispatch_excluded
        || !r.exclusion.active_owner_excluded
        || r.exclusion.evidence.trim().is_empty()
    {
        return Err(recovery_fs::refused(
            l,
            "valid bound lease and renewed administrative exclusion required",
        ));
    }
    Ok(())
}
/// Observe a binding without opening a store, creating it, migrating it or
/// probing a daemon. Forensic state retains a held filesystem identity and errno.
pub fn inspect_file_binding(
    path: &Path,
    l: &OperationLimits,
) -> Result<FileBinding, RecoveryError> {
    use std::os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::OpenOptionsExt,
    };
    let (parent, name) = super::verify::walk_parent(path, false, l)?;
    #[cfg(target_os = "linux")]
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    #[cfg(not(target_os = "linux"))]
    let fd = -1;
    if fd < 0 {
        if std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT) {
            return Ok(FileBinding::Absent);
        }
        return Err(recovery_fs::refused(
            l,
            "existing file identity unavailable",
        ));
    }
    // SAFETY: openat returned an owned descriptor; O_PATH cannot mutate bytes.
    let held = unsafe { std::fs::File::from_raw_fd(fd) };
    let m = held.metadata().map_err(|_| l.verification())?;
    if !m.is_file() || m.nlink() != 1 {
        return Err(recovery_fs::refused(
            l,
            "single-link unaliased regular target required",
        ));
    }
    let identity = file_identity(&m);
    let read = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path);
    let read = match read {
        Ok(read) => read,
        Err(error) => {
            return Ok(FileBinding::Forensic {
                identity,
                bytes: m.len(),
                read_error: error.raw_os_error().ok_or_else(|| l.verification())?,
            });
        }
    };
    if file_identity(&read.metadata().map_err(|_| l.verification())?) != identity {
        return Err(l.verification());
    }
    let (bytes, sha256) = super::verify::hash_file(&read, l)?;
    if bytes != m.len()
        || file_identity(&std::fs::symlink_metadata(path).map_err(|_| l.verification())?)
            != identity
    {
        return Err(l.verification());
    }
    Ok(FileBinding::Readable {
        identity,
        bytes,
        sha256,
    })
}
pub fn create_offline_lease(
    request: &OfflineLeaseRequest,
    fs: &dyn RecoveryFs,
    limits: &OperationLimits,
) -> Result<OfflineLease, RecoveryError> {
    let mut l = limits.clone();
    l.operation_id = request.operation_id;
    validate_request(request, fs, &l)?;
    if inspect_file_binding(&request.physical_path, &l)? != request.expected_file {
        return Err(recovery_fs::refused(
            &l,
            "lease current-file binding changed",
        ));
    }
    let root = recovery_fs::Dir::open(fs.root(), &l)?;
    let unit = recovery_fs::allocation_geometry(&root, &l)?;
    let control = std::sync::Arc::new(recovery_fs::PublicationIo::new(
        fs.io(),
        l.byte_limit,
        l.byte_limit,
        l.byte_limit,
        unit,
    ));
    let name = format!(
        "lease-{}-{}.json",
        request.operation_id, request.issued_at_unix_ms
    );
    recovery_fs::write_json(
        &root,
        &name,
        request,
        recovery_fs::Member::Lease,
        &control,
        &l,
    )?;
    root.sync(&*control.io, recovery_fs::Directory::LeaseParent, &l)?;
    let lease = OfflineLease {
        request: request.clone(),
        path: root.path.join(name),
    };
    lease.validate(fs, &l)?;
    Ok(lease)
}
/// Process-local operation ownership only. Neither this nor file locks prove
/// exclusion of legacy clients; that remains the administrator's attestation.
pub(crate) struct OperationOwnership {
    key: PathBuf,
}
static OWNERS: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());
impl OperationOwnership {
    pub(crate) fn acquire(path: &Path, l: &OperationLimits) -> Result<Self, RecoveryError> {
        let mut owners = OWNERS.lock().map_err(|_| l.verification())?;
        if owners.iter().any(|p| p == path) {
            return Err(recovery_fs::refused(l, "physical operation already active"));
        }
        owners.push(path.to_path_buf());
        Ok(Self {
            key: path.to_path_buf(),
        })
    }
}
impl Drop for OperationOwnership {
    fn drop(&mut self) {
        if let Ok(mut owners) = OWNERS.lock() {
            owners.retain(|p| *p != self.key);
        }
    }
}

/// No normal-open force flag can construct this operation-bound inspection access.
pub(crate) struct RestoreAccess {
    destination: PathBuf,
    staging: PathBuf,
}
impl RestoreAccess {
    pub(crate) fn obtain(
        journal: &super::restore::RestoreJournal,
        lease: &OfflineLease,
        fs: &dyn RecoveryFs,
        lock: &super::restore::RestoreOperationLock,
        l: &OperationLimits,
    ) -> Result<Self, RecoveryError> {
        let actual = super::restore::load_with_held_lock(
            &journal.staging.join("journal.bin"),
            lease,
            fs,
            lock,
        )?;
        if super::verify::serialized_checksum(&actual, l)?
            != super::verify::serialized_checksum(journal, l)?
        {
            return Err(l.verification());
        }
        Ok(Self {
            destination: actual.destination,
            staging: actual.staging,
        })
    }
    pub(crate) fn permits(&self, path: &Path) -> bool {
        path == self.destination
            || path == self.staging.join("candidate.fls")
            || path == self.staging.join("original.fls")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recovery::fs::NativeRecoveryFs;
    use crate::recovery::{CancelToken, OperationLimits};
    use std::time::{Duration, Instant};
    fn limits() -> OperationLimits {
        OperationLimits::new(
            Instant::now() + Duration::from_secs(10),
            1 << 20,
            64 << 20,
            CancelToken::new(),
        )
    }
    #[test]
    fn guard_inspection_is_bounded_and_never_admits_present_unknown_guards() {
        let root = crate::recovery::test_support::fixture_root();
        let destination = root.path().join("destination.fls");
        let guard = guard_path(&destination);
        assert!(inspect_restore_guard(&destination).unwrap().is_none());
        for bytes in [b"not-json".as_slice(), &[b'x'; 65537]] {
            fs::write(&guard, bytes).unwrap();
            let before = fs::read(&guard).unwrap();
            let status = inspect_restore_guard(&destination).unwrap().unwrap();
            assert_eq!(status.guard_path, guard);
            assert_eq!(status.operation_id, None);
            let error = check_restore_admission(&destination)
                .unwrap_err()
                .to_string();
            assert!(error.contains(&guard.display().to_string()));
            assert!(error.contains("ferrostore recovery --root <ROOT> --lease <LEASE> --request <CONTINUATION.json> status"));
            assert_eq!(fs::read(&guard).unwrap(), before);
            assert!(!destination.exists());
        }
        fs::remove_file(&guard).unwrap();
        fs::create_dir(&guard).unwrap();
        assert!(check_restore_admission(&destination).is_err());
        fs::remove_dir(&guard).unwrap();
        std::os::unix::fs::symlink(root.path().join("missing"), &guard).unwrap();
        assert_eq!(
            inspect_restore_guard(&destination)
                .unwrap()
                .unwrap()
                .operation_id,
            None
        );
        assert!(check_restore_admission(&destination).is_err());
        assert!(
            fs::symlink_metadata(&guard)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn guard_inspection_reports_only_validated_operation_ids() {
        let root = crate::recovery::test_support::fixture_root();
        let destination = root.path().join("destination.fls");
        let mut guard = RestoreGuardV1 {
            version: 1,
            operation_id: uuid::Uuid::now_v7(),
            physical_path: destination.clone(),
            parent_identity: (1, 1),
            journal: root.path().join("missing-journal.json"),
            phase: "admitted".into(),
        };
        let path = guard_path(&destination);
        fs::write(&path, serde_json::to_vec(&guard).unwrap()).unwrap();
        assert_eq!(
            inspect_restore_guard(&destination)
                .unwrap()
                .unwrap()
                .operation_id,
            Some(guard.operation_id)
        );
        assert!(
            matches!(check_restore_admission(&destination), Err(RecoveryError::RestoreFenced { guard_path, operation_id: Some(id) }) if guard_path == path && id == guard.operation_id)
        );
        for version in [0, 2] {
            guard.version = version;
            fs::write(&path, serde_json::to_vec(&guard).unwrap()).unwrap();
            assert_eq!(
                inspect_restore_guard(&destination)
                    .unwrap()
                    .unwrap()
                    .operation_id,
                None
            );
            assert!(check_restore_admission(&destination).is_err());
        }
        guard.version = 1;
        guard.operation_id = uuid::Uuid::nil();
        fs::write(&path, serde_json::to_vec(&guard).unwrap()).unwrap();
        assert_eq!(
            inspect_restore_guard(&destination)
                .unwrap()
                .unwrap()
                .operation_id,
            None
        );
        assert!(check_restore_admission(&destination).is_err());
    }

    #[test]
    fn physical_resolution_failure_is_not_a_restore_fence() {
        let root = crate::recovery::test_support::fixture_root();
        let path = root.path().join("dangling.fls");
        std::os::unix::fs::symlink(root.path().join("missing"), &path).unwrap();
        assert!(matches!(
            check_restore_admission(&path),
            Err(RecoveryError::PhysicalPathResolution { .. })
        ));
        assert!(
            !check_restore_admission(&path)
                .unwrap_err()
                .to_string()
                .contains("fenced")
        );
    }

    #[test]
    fn lease_creation_is_private_bound_and_short_lived() {
        let root = crate::recovery::test_support::fixture_root();
        let l = limits();
        let fs = NativeRecoveryFs::new(root.path().to_path_buf(), limits());
        let destination = root.path().join("destination.fls");
        let request = OfflineLeaseRequest {
            version: 1,
            operation_id: l.operation_id,
            action: OfflineAction::Restore {
                bundle_operation: uuid::Uuid::now_v7(),
                bundle_sha256: "a".repeat(64),
            },
            catalog_identity: "fixture".into(),
            approved_root_identity: file_identity(&std::fs::metadata(root.path()).unwrap()),
            physical_path: destination.clone(),
            parent_identity: file_identity(&std::fs::metadata(root.path()).unwrap()),
            expected_file: FileBinding::Absent,
            profile_sha256: "b".repeat(64),
            build: super::super::manifest::BuildIdentity::current(),
            issued_at_unix_ms: now_ms().unwrap(),
            expires_at_unix_ms: now_ms().unwrap() + 60000,
            operator: "fixture-operator".into(),
            exclusion: ExclusionAttestation {
                legacy_and_cached_handles_excluded: true,
                external_dispatch_excluded: true,
                active_owner_excluded: true,
                evidence: "fixture has no clients or dispatch".into(),
            },
        };
        let lease = create_offline_lease(&request, &fs, &l).unwrap();
        let metadata = std::fs::metadata(lease.path()).unwrap();
        assert_eq!(metadata.mode() & 0o7777, 0o600);
        assert_eq!(metadata.nlink(), 1);
        assert!(lease.path().starts_with(root.path()));
        assert!(!destination.exists());
        let mut wrong = request.clone();
        wrong.operation_id = uuid::Uuid::now_v7();
        wrong.expires_at_unix_ms = wrong.issued_at_unix_ms + 3_600_001;
        assert!(create_offline_lease(&wrong, &fs, &l).is_err());
        wrong.expires_at_unix_ms = wrong.issued_at_unix_ms + 60000;
        wrong.exclusion.legacy_and_cached_handles_excluded = false;
        assert!(create_offline_lease(&wrong, &fs, &l).is_err());
        let mutations: &[fn(&mut OfflineLeaseRequest)] = &[
            |r| r.version = 2,
            |r| r.operation_id = uuid::Uuid::nil(),
            |r| r.catalog_identity.clear(),
            |r| r.approved_root_identity = (0, 0),
            |r| r.parent_identity = (0, 0),
            |r| r.profile_sha256 = "not a digest".into(),
            |r| r.build.package_version = "wrong build".into(),
            |r| r.operator.clear(),
            |r| r.exclusion.evidence.clear(),
            |r| r.exclusion.active_owner_excluded = false,
            |r| r.exclusion.external_dispatch_excluded = false,
            |r| r.exclusion.legacy_and_cached_handles_excluded = false,
            |r| {
                r.issued_at_unix_ms += 3_600_000;
                r.expires_at_unix_ms = r.issued_at_unix_ms + 60000;
            },
            |r| {
                r.issued_at_unix_ms = 1;
                r.expires_at_unix_ms = 60001;
            },
            |r| r.action = OfflineAction::Backup,
            |r| {
                r.action = OfflineAction::Restore {
                    bundle_operation: uuid::Uuid::nil(),
                    bundle_sha256: "a".repeat(64),
                }
            },
            |r| {
                r.action = OfflineAction::Purge {
                    quarantined_operation: uuid::Uuid::now_v7(),
                    manifest_sha256: "invalid".into(),
                }
            },
            |r| {
                r.expected_file = FileBinding::Readable {
                    identity: (0, 0),
                    bytes: 1,
                    sha256: "a".repeat(64),
                }
            },
        ];
        for (index, mutate) in mutations.iter().enumerate() {
            let mut invalid = request.clone();
            invalid.operation_id = uuid::Uuid::now_v7();
            mutate(&mut invalid);
            assert!(
                create_offline_lease(&invalid, &fs, &l).is_err(),
                "invalid lease variant {index} accepted"
            );
            assert_eq!(
                std::fs::read_dir(root.path()).unwrap().count(),
                1,
                "refused lease variant {index} wrote metadata"
            );
        }
        std::fs::write(&destination, b"unexpected new file").unwrap();
        assert!(lease.validate(&fs, &l).is_err(), "absence binding changed");
    }
    #[test]
    fn lease_expiry_never_releases_restore_guard() {
        let root = crate::recovery::test_support::fixture_root();
        let destination = root.path().join("fixture.fls");
        let guard = guard_path(&destination);
        std::fs::write(&guard, b"malformed expired operation").unwrap();
        assert!(check_restore_admission(&destination).is_err());
        assert!(guard.exists());
        assert!(!destination.exists());
    }
}
