//! Existing-only verification. No socket routing, migration, adapters or writes
//! through a decoded reader. Linux privacy is proved using no-follow handles,
//! ownership/mode/link checks and absence of extended ACLs; other hosts refuse.
use super::manifest::{CopyCreation, CopyEvidence, Readiness};
use super::{
    OperationLimits, RecoveryError, ValidationExpectations, ValidationProfile, VerificationReport,
};
use sha2::{Digest, Sha256};
use std::{
    ffi::CString,
    fs::File,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            ffi::OsStrExt,
            fs::{FileExt, MetadataExt},
        },
    },
    path::{Component, Path},
};

/// Open only an existing private standalone recovery image into a detached
/// compatible reader. This convenience API caps input at 64 MiB, working memory
/// at 256 MiB and elapsed time at 30 seconds. Full verification uses explicit
/// caller-supplied limits. No disk writer, migration or transport is attached.
pub fn open_recovery_image_existing(path: &Path) -> Result<crate::FerroStore, RecoveryError> {
    // The signature has no caller budget: this convenience reader has a finite,
    // documented admission ceiling. Full verification always uses caller limits.
    let limits = OperationLimits::new(
        std::time::Instant::now() + std::time::Duration::from_secs(30),
        64 << 20,
        256 << 20,
        super::CancelToken::new(),
    );
    let held = PrivateFile::open(path, false, &limits)?;
    let before = hash_file(&held.file, &limits)?;
    let (store, _) = crate::ferrostore::recovery::decode_existing(&held.file, &limits)?;
    held.unchanged(&limits)?;
    if before != hash_file(&held.file, &limits)? {
        return Err(limits.verification());
    }
    Ok(store)
}

/// Verify an independently staged private image without creating any image.
/// The caller supplies its staging provenance. Recorded source file identity
/// only excludes that volatile inode; it cannot prove durable provenance after
/// compaction, movement across hosts or inode reuse. Collisions fail closed.
pub fn verify_existing_copy(
    path: &Path,
    expected: &ValidationExpectations,
    profile: &ValidationProfile,
    limits: &OperationLimits,
) -> Result<VerificationReport, RecoveryError> {
    limits.check()?;
    let held = PrivateFile::open(path, false, limits)?;
    if expected.source_file_identity == Some(held.identity()) {
        return Err(RecoveryError::RefusedInput {
            operation_id: limits.operation_id,
            reason: "captured file identity encountered; independent image or new capture operation required",
        });
    }
    let before = hash_file(&held.file, limits)?;
    let (store, descriptor) = crate::ferrostore::recovery::decode_existing(&held.file, limits)?;
    let actual =
        crate::ferrostore::recovery::expectations_from_reader(&store, descriptor, profile, limits)?;
    compare(expected, &actual, limits)?;
    drop(store);
    drop(actual);
    held.unchanged(limits)?;
    // This is a real second open and decode of the supplied image. Only the
    // identity guard remains held; the first compatible reader was dropped.
    let reopened = PrivateFile::open(path, false, limits)?;
    if held.identity() != reopened.identity() {
        return Err(limits.verification());
    }
    let (reader, descriptor) =
        crate::ferrostore::recovery::decode_existing(&reopened.file, limits)?;
    let second = crate::ferrostore::recovery::expectations_from_reader(
        &reader, descriptor, profile, limits,
    )?;
    compare(expected, &second, limits)?;
    drop(reader);
    reopened.unchanged(limits)?;
    held.unchanged(limits)?;
    if before != hash_file(&reopened.file, limits)? || before != hash_file(&held.file, limits)? {
        return Err(limits.verification());
    }
    Ok(VerificationReport {
        report_version: 1,
        copy_evidence: Some(CopyEvidence {
            verified_file_identity: held.identity(),
            source_image_identity: None,
            creation: CopyCreation::CallerSuppliedExisting,
            image_sha256: before.1.clone(),
        }),
        operation_id: limits.operation_id.to_string(),
        image_sha256: before.1,
        image_bytes: before.0,
        published_sequence: expected.descriptor.sequence,
        logical_sha256: expected.logical_sha256.clone(),
        profile_version: profile.version,
        profile_sha256: expected.profile_sha256.clone(),
        canonical_encoding_version: 1,
        intrinsic_lineage_available: expected.descriptor.lineage.is_some(),
        external_publish_status: expected.descriptor.external_publish_status,
        verified_collection_count: expected.known,
        unknown_collection_count: expected.unknown,
        opaque_runtime_references: expected.opaque_runtime_references,
        query_probe_count: (expected.query_digests.len() + expected.vector_results.len()) as u64,
        exception_count: expected.exceptions,
        dependency_count: expected.dependencies,
        transient_rpc_dedupe_persisted: false,
        durable_rpc_receipts_claimed: false,
        readiness: if expected.dependencies > 0 {
            Readiness::DependenciesRequireReconciliation
        } else if expected.unknown > 0 || expected.opaque_runtime_references {
            Readiness::UnknownDomainCoverage
        } else {
            Readiness::VerifiedLocalContents
        },
        verifier_build_identity: super::manifest::BuildIdentity::current(),
    })
}
pub(crate) fn compare(
    a: &ValidationExpectations,
    b: &ValidationExpectations,
    l: &OperationLimits,
) -> Result<(), RecoveryError> {
    l.check()?;
    if a.version != 1
        || a.version != b.version
        || a.descriptor != b.descriptor
        || a.catalog_identity != b.catalog_identity
        || a.profile_sha256 != b.profile_sha256
        || a.logical_sha256 != b.logical_sha256
        || a.collection_digests != b.collection_digests
        || a.query_digests != b.query_digests
        || a.opaque_runtime_references != b.opaque_runtime_references
        || a.known != b.known
        || a.unknown != b.unknown
        || a.exceptions != b.exceptions
        || a.dependencies != b.dependencies
        || a.vector_results.len() != b.vector_results.len()
    {
        return Err(l.verification());
    }
    for (left, right) in a.vector_results.iter().zip(&b.vector_results) {
        l.check()?;
        if left.len() != right.len()
            || left.iter().zip(right).any(|((a, x), (b, y))| {
                a != b
                    || !x.is_finite()
                    || !y.is_finite()
                    || (x - y).abs() > super::profiles::VECTOR_SCORE_TOLERANCE_V1
            })
        {
            return Err(l.verification());
        }
    }
    Ok(())
}
pub fn copy_and_verify(
    image: &Path,
    copy_path: &Path,
    expected: &ValidationExpectations,
    profile: &ValidationProfile,
    limits: &OperationLimits,
) -> Result<VerificationReport, RecoveryError> {
    limits.quota(65536, limits.memory_byte_limit, "copy working memory")?;
    let source = PrivateFile::open(image, false, limits)?;
    let before = hash_file(&source.file, limits)?;
    // create_new is exclusive even for aliases: no existing target is truncated.
    if let Some(control) = &limits.publication_io {
        super::fs::event(
            &*control.io,
            super::fs::RecoveryEvent::Create(super::fs::Member::VerificationCopy),
            limits,
        )?;
    }
    let target = PrivateFile::open(copy_path, true, limits)?;
    if let Some(control) = &limits.publication_io {
        control.prepare_copy(&target.file, before.0, limits)?;
    }
    if source.identity() == target.identity() {
        return Err(limits.verification());
    }
    let result = (|| {
        let mut buffer = [0u8; 65536];
        let mut offset = 0;
        while offset < before.0 {
            limits.check()?;
            let n = (before.0 - offset).min(buffer.len() as u64) as usize;
            source
                .file
                .read_exact_at(&mut buffer[..n], offset)
                .map_err(|_| limits.verification())?;
            if let Some(control) = &limits.publication_io {
                control.write_copy(&target.file, &buffer[..n], offset, limits)?;
            } else {
                target
                    .file
                    .write_all_at(&buffer[..n], offset)
                    .map_err(|_| RecoveryError::Destination {
                        operation_id: limits.operation_id,
                    })?;
            }
            offset += n as u64;
        }
        limits.check()?;
        if let Some(control) = &limits.publication_io {
            control.sync(&target.file, super::fs::Member::VerificationCopy, limits)?;
        } else {
            target
                .file
                .sync_all()
                .map_err(|_| RecoveryError::Destination {
                    operation_id: limits.operation_id,
                })?;
        }
        target.same_name(limits)?;
        source.unchanged(limits)?;
        if before != hash_file(&source.file, limits)? || before != hash_file(&target.file, limits)?
        {
            return Err(limits.verification());
        }
        let mut report = verify_existing_copy(copy_path, expected, profile, limits)?;
        bind_created_copy(
            &mut report,
            &before,
            source.identity(),
            target.identity(),
            limits,
        )?;
        source.unchanged(limits)?;
        target.same_name(limits)?;
        if before != hash_file(&source.file, limits)? || before != hash_file(&target.file, limits)?
        {
            return Err(limits.verification());
        }
        Ok(report)
    })();
    // Failure leaves a private, explicitly named incomplete artifact. Task 4
    // owns staging/quarantine/cleanup; never unlink a substituted pathname here.
    result
}

// Bind the independently reopened verifier result to the exact handles copied
// above. Successful logical reads alone cannot establish this provenance.
fn bind_created_copy(
    report: &mut VerificationReport,
    source_hash: &(u64, String),
    source: (u64, u64),
    target: (u64, u64),
    l: &OperationLimits,
) -> Result<(), RecoveryError> {
    l.check()?;
    let evidence = report
        .copy_evidence
        .as_mut()
        .ok_or_else(|| l.verification())?;
    if report.image_sha256 != source_hash.1
        || report.image_bytes != source_hash.0
        || evidence.image_sha256 != source_hash.1
        || evidence.verified_file_identity != target
        || source == target
        || evidence.creation != CopyCreation::CallerSuppliedExisting
        || evidence.source_image_identity.is_some()
    {
        return Err(l.verification());
    }
    evidence.source_image_identity = Some(source);
    evidence.creation = CopyCreation::WrapperCreated;
    Ok(())
}

pub(crate) struct PrivateFile {
    pub(crate) file: File,
    parent: File,
    path: std::path::PathBuf,
    name: CString,
    initial: std::fs::Metadata,
}
impl PrivateFile {
    pub(crate) fn open(
        path: &Path,
        create: bool,
        l: &OperationLimits,
    ) -> Result<Self, RecoveryError> {
        l.check()?;
        if !cfg!(target_os = "linux") {
            return Err(RecoveryError::Unsupported {
                operation_id: l.operation_id,
                reason: "native private-file verification unsupported",
            });
        }
        let (parent, name) = private_parent(path, l)?;
        let flags = libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | libc::O_NONBLOCK
            | if create {
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL
            } else {
                libc::O_RDONLY
            };
        // SAFETY: valid directory descriptor and NUL-terminated single component.
        let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags, 0o600) };
        if fd < 0 {
            return Err(RecoveryError::RefusedInput {
                operation_id: l.operation_id,
                reason: "existing private image or exclusive destination required",
            });
        }
        // SAFETY: openat returned a new owned descriptor.
        let file = unsafe { File::from_raw_fd(fd) };
        let initial = file.metadata().map_err(|_| l.verification())?;
        privacy(&file, false, l)?;
        let held = Self {
            file,
            parent,
            path: path.to_path_buf(),
            name,
            initial,
        };
        held.same_name(l)?;
        Ok(held)
    }
    pub(crate) fn identity(&self) -> (u64, u64) {
        (self.initial.dev(), self.initial.ino())
    }
    pub(crate) fn same_name(&self, l: &OperationLimits) -> Result<(), RecoveryError> {
        let (parent, name) = private_parent(&self.path, l)?;
        let current = parent.metadata().map_err(|_| l.verification())?;
        let original = self.parent.metadata().map_err(|_| l.verification())?;
        if (current.dev(), current.ino()) != (original.dev(), original.ino()) {
            return Err(l.verification());
        }
        // SAFETY: existing owned directory fd and a validated single component.
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK | libc::O_RDONLY,
            )
        };
        if fd < 0 {
            return Err(l.verification());
        }
        // SAFETY: openat returned a new owned descriptor.
        let now = unsafe { File::from_raw_fd(fd) };
        privacy(&now, false, l)?;
        let m = now.metadata().map_err(|_| l.verification())?;
        if self.identity() != (m.dev(), m.ino()) || name != self.name {
            return Err(l.verification());
        }
        Ok(())
    }
    pub(crate) fn unchanged(&self, l: &OperationLimits) -> Result<(), RecoveryError> {
        self.same_name(l)?;
        let now = self.file.metadata().map_err(|_| l.verification())?;
        if now.len() != self.initial.len()
            || now.mtime() != self.initial.mtime()
            || now.mtime_nsec() != self.initial.mtime_nsec()
            || now.ctime() != self.initial.ctime()
            || now.ctime_nsec() != self.initial.ctime_nsec()
        {
            return Err(l.verification());
        }
        Ok(())
    }
}
pub(crate) fn private_parent(
    path: &Path,
    l: &OperationLimits,
) -> Result<(File, CString), RecoveryError> {
    walk_parent(path, true, l)
}
pub(crate) fn walk_parent(
    path: &Path,
    private: bool,
    l: &OperationLimits,
) -> Result<(File, CString), RecoveryError> {
    if !path.is_absolute() {
        return Err(RecoveryError::RefusedInput {
            operation_id: l.operation_id,
            reason: "absolute private path required",
        });
    }
    let mut parts = path.components().peekable();
    if parts.next() != Some(Component::RootDir) {
        return Err(l.verification());
    }
    let mut directory = File::open("/").map_err(|_| l.verification())?;
    while let Some(part) = parts.next() {
        l.check()?;
        let Component::Normal(name) = part else {
            return Err(l.verification());
        };
        let name = CString::new(name.as_bytes()).map_err(|_| l.verification())?;
        if parts.peek().is_none() {
            if private {
                privacy(&directory, true, l)?;
            }
            return Ok((directory, name));
        }
        // SAFETY: parent is a live owned fd; name is a NUL-terminated component.
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(l.verification());
        }
        // SAFETY: returned descriptor is uniquely owned.
        directory = unsafe { File::from_raw_fd(fd) };
    }
    Err(l.verification())
}
pub(crate) fn privacy(
    file: &File,
    directory: bool,
    l: &OperationLimits,
) -> Result<(), RecoveryError> {
    l.check()?;
    let m = file.metadata().map_err(|_| l.verification())?;
    // SAFETY: geteuid has no preconditions.
    if m.uid() != unsafe { libc::geteuid() }
        || m.mode() & 0o7777 != if directory { 0o700 } else { 0o600 }
        || if directory {
            !m.is_dir()
        } else {
            !m.is_file() || m.nlink() != 1
        }
    {
        return Err(RecoveryError::RefusedInput {
            operation_id: l.operation_id,
            reason: "private ownership, mode and single-link regular file required",
        });
    }
    #[cfg(target_os = "linux")]
    for name in [c"system.posix_acl_access", c"system.posix_acl_default"] {
        // SAFETY: size probe uses null buffer and zero length on an owned fd.
        let n =
            unsafe { libc::fgetxattr(file.as_raw_fd(), name.as_ptr(), std::ptr::null_mut(), 0) };
        if n >= 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ENODATA) {
            return Err(RecoveryError::Unsupported {
                operation_id: l.operation_id,
                reason: "extended ACL privacy cannot be established",
            });
        }
    }
    Ok(())
}
pub(crate) fn hash_file(file: &File, l: &OperationLimits) -> Result<(u64, String), RecoveryError> {
    l.quota(65536, l.memory_byte_limit, "hash working memory")?;
    let len = file.metadata().map_err(|_| l.verification())?.len();
    l.quota(len, l.byte_limit, "image bytes")?;
    let mut hash = Sha256::new();
    let mut offset = 0;
    let mut bytes = [0; 65536];
    while offset < len {
        l.check()?;
        let n = (len - offset).min(bytes.len() as u64) as usize;
        file.read_exact_at(&mut bytes[..n], offset)
            .map_err(|_| l.verification())?;
        hash.update(&bytes[..n]);
        offset += n as u64;
    }
    l.check()?;
    Ok((len, format!("{:x}", hash.finalize())))
}

/// Canonical encoding v1: tag:u8, length:u64 BE, bytes; lists/objects carry
/// element counts, then recursively framed members in order (object keys sorted).
/// No allocation proportional to payload; checks run at every 64 KiB.
pub(crate) struct Canonical<'a> {
    hash: Sha256,
    limits: &'a OperationLimits,
}
impl<'a> Canonical<'a> {
    pub(crate) fn new(limits: &'a OperationLimits) -> Self {
        let mut hash = Sha256::new();
        hash.update(b"ferro-recovery-canonical\0\x01");
        Self { hash, limits }
    }
    pub(crate) fn bytes(&mut self, tag: u8, bytes: &[u8]) -> Result<(), RecoveryError> {
        self.limits.check()?;
        self.hash.update([tag]);
        self.hash.update((bytes.len() as u64).to_be_bytes());
        for chunk in bytes.chunks(65536) {
            self.limits.check()?;
            self.hash.update(chunk);
        }
        Ok(())
    }
    pub(crate) fn count(&mut self, tag: u8, n: u64) -> Result<(), RecoveryError> {
        self.bytes(tag, &n.to_be_bytes())
    }
    pub(crate) fn value(&mut self, v: Option<&serde_json::Value>) -> Result<(), RecoveryError> {
        use serde_json::Value;
        match v {
            None => self.bytes(0, &[]),
            Some(Value::Null) => self.bytes(1, &[]),
            Some(Value::Bool(b)) => self.bytes(2, &[u8::from(*b)]),
            Some(Value::Number(n)) => {
                if let Some(x) = n.as_u64() {
                    self.bytes(3, &x.to_be_bytes())
                } else if let Some(x) = n.as_i64() {
                    self.bytes(4, &x.to_be_bytes())
                } else {
                    self.bytes(
                        5,
                        &n.as_f64()
                            .ok_or_else(|| self.limits.verification())?
                            .to_bits()
                            .to_be_bytes(),
                    )
                }
            }
            Some(Value::String(s)) => self.bytes(6, s.as_bytes()),
            Some(Value::Array(a)) => {
                self.count(7, a.len() as u64)?;
                for v in a {
                    self.value(Some(v))?;
                }
                Ok(())
            }
            Some(Value::Object(m)) => {
                self.count(8, m.len() as u64)?;
                let mut keys: Vec<_> = m.keys().collect();
                keys.sort_unstable();
                for k in keys {
                    self.bytes(6, k.as_bytes())?;
                    self.value(m.get(k))?;
                }
                Ok(())
            }
        }
    }
    pub(crate) fn finish(self) -> String {
        format!("{:x}", self.hash.finalize())
    }
}
pub(crate) fn value_digest(
    v: &serde_json::Value,
    l: &OperationLimits,
) -> Result<String, RecoveryError> {
    let mut h = Canonical::new(l);
    h.value(Some(v))?;
    Ok(h.finish())
}
/// A deployment can declare an exact exception/dependency without including
/// source identifiers in public diagnostics. Inputs belong to private metadata.
pub fn reference_digest(
    collection: &str,
    id: &str,
    pointer: &str,
    target_collection: &str,
    target: &str,
    l: &OperationLimits,
) -> Result<String, RecoveryError> {
    let mut h = Canonical::new(l);
    for v in [collection, id, pointer, target_collection, target] {
        h.bytes(6, v.as_bytes())?;
    }
    Ok(h.finish())
}

pub(crate) fn serialized_checksum<T: serde::Serialize>(
    value: &T,
    l: &OperationLimits,
) -> Result<(u64, String), RecoveryError> {
    struct Writer<'a> {
        hash: Sha256,
        bytes: u64,
        limits: &'a OperationLimits,
        failure: Option<RecoveryError>,
    }
    impl std::io::Write for Writer<'_> {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            let result = self.limits.quota(
                self.bytes.saturating_add(b.len() as u64),
                self.limits.byte_limit,
                "metadata bytes",
            );
            if let Err(error) = result {
                self.failure = Some(error);
                return Err(std::io::Error::other("metadata quota"));
            }
            self.bytes += b.len() as u64;
            self.hash.update(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = Writer {
        hash: Sha256::new(),
        bytes: 0,
        limits: l,
        failure: None,
    };
    serde_json::to_writer(&mut writer, value)
        .map_err(|_| writer.failure.take().unwrap_or_else(|| l.verification()))?;
    Ok((writer.bytes, format!("{:x}", writer.hash.finalize())))
}
pub(crate) fn profile_digest(
    profile: &ValidationProfile,
    l: &OperationLimits,
) -> Result<String, RecoveryError> {
    let (bytes, _) = serialized_checksum(profile, l)?;
    l.quota(
        bytes.saturating_mul(32),
        l.memory_byte_limit / 4,
        "profile memory",
    )?;
    value_digest(
        &serde_json::to_value(profile).map_err(|_| l.verification())?,
        l,
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    fn limits() -> OperationLimits {
        OperationLimits::new(
            std::time::Instant::now() + std::time::Duration::from_secs(10),
            16 << 20,
            64 << 20,
            crate::recovery::CancelToken::new(),
        )
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn external_r1_created_copy_binds_report_digest_inode_and_evidence() {
        use crate::recovery::{CancelToken, CaptureBudget, ProfileKind};
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let store = crate::FerroStore::open(dir.path().join("source.fls")).unwrap();
        let l = limits();
        let capture = store
            .capture_recovery(
                &CaptureBudget::new(
                    l.deadline,
                    64 << 20,
                    64 << 20,
                    std::time::Duration::from_secs(1),
                ),
                &CancelToken::new(),
            )
            .unwrap();
        let profile = ValidationProfile::new(ProfileKind::SmallEmpty, "fixture");
        let expected = capture.expectations(&profile, &l).unwrap();
        let image = PrivateFile::open(&dir.path().join("image.fls"), true, &l).unwrap();
        capture
            .write_image(&mut image.file.try_clone().unwrap(), &l)
            .unwrap();
        std::fs::copy(&image.path, dir.path().join("copy.fls")).unwrap();
        let copy = PrivateFile::open(&dir.path().join("copy.fls"), false, &l).unwrap();
        let report = verify_existing_copy(&copy.path, &expected, &profile, &l).unwrap();
        let hash = hash_file(&image.file, &l).unwrap();
        let source = image.identity();
        let target = copy.identity();
        for mutation in 0..6 {
            let mut changed = report.clone();
            match mutation {
                0 => changed.image_sha256 = "a".repeat(64),
                1 => changed.image_bytes += 1,
                2 => {
                    changed
                        .copy_evidence
                        .as_mut()
                        .unwrap()
                        .verified_file_identity = source
                }
                3 => changed.copy_evidence.as_mut().unwrap().image_sha256 = "b".repeat(64),
                4 => changed.copy_evidence = None,
                5 => {
                    changed
                        .copy_evidence
                        .as_mut()
                        .unwrap()
                        .source_image_identity = Some(source)
                }
                _ => unreachable!(),
            }
            assert!(
                bind_created_copy(&mut changed, &hash, source, target, &l).is_err(),
                "mutation {mutation}"
            );
        }
        assert!(bind_created_copy(&mut report.clone(), &hash, target, target, &l).is_err());
        let mut bound = report;
        bind_created_copy(&mut bound, &hash, source, target, &l).unwrap();
        assert_eq!(
            bound.independent_copy_status(&l).unwrap(),
            super::super::manifest::IndependentCopyStatus::Verified
        );
        assert_eq!(
            bound.copy_evidence.unwrap().source_image_identity,
            Some(source)
        );
    }
    #[test]
    fn canonical_types_framing_order_and_absence_are_distinct() {
        let l = limits();
        let mut absent = Canonical::new(&l);
        absent.value(None).unwrap();
        let absent = absent.finish();
        let values = [
            serde_json::json!(null),
            serde_json::json!(0),
            serde_json::json!(0.0),
            serde_json::json!("0"),
            serde_json::json!(false),
            serde_json::json!(["a", "bc"]),
            serde_json::json!(["ab", "c"]),
            serde_json::json!(["bc", "a"]),
            serde_json::json!({}),
            serde_json::json!({"a":null}),
        ];
        let mut hashes = std::collections::BTreeSet::from([absent]);
        for value in values {
            assert!(hashes.insert(value_digest(&value, &l).unwrap()));
        }
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn private_handle_detects_same_name_inode_replacement_and_no_follow_escape() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.path().join("image");
        let held = PrivateFile::open(&path, true, &limits()).unwrap();
        let other = dir.path().join("replacement");
        let _ = PrivateFile::open(&other, true, &limits()).unwrap();
        std::fs::rename(&other, &path).unwrap();
        assert!(held.same_name(&limits()).is_err());
        let link = dir.path().join("symlink");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(PrivateFile::open(&link, false, &limits()).is_err());
        let parent = dir.path().join("parent");
        std::os::unix::fs::symlink(dir.path(), &parent).unwrap();
        assert!(PrivateFile::open(&parent.join("image"), false, &limits()).is_err());
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn zero_length_and_nonprivate_files_cannot_initialize() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.path().join("empty");
        let file = PrivateFile::open(&path, true, &limits()).unwrap();
        drop(file);
        assert!(open_recovery_image_existing(&path).is_err());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(PrivateFile::open(&path, false, &limits()).is_err());
    }
}
