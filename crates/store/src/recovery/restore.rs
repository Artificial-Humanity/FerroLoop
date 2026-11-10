//! Explicit offline restore. Installation is distinct from admission readiness.
use super::{OperationLimits, RecoveryError, StoreIdentity};
use super::{admission::OfflineLease, backup::VerifiedBundle, fs::RecoveryFs};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
pub struct RestoreRequest {
    pub operation_id: uuid::Uuid,
    pub store: StoreIdentity,
    pub destination: PathBuf,
    pub staging_parent: PathBuf,
    pub selected_bundle: VerifiedBundle,
    pub limits: OperationLimits,
    pub metadata_byte_limit: u64,
    pub space_margin_bytes: u64,
    pub available_dependencies: Vec<super::readiness::DependencyResolution>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestorePhase {
    Prepared,
    ExchangeIntent,
    RelocateIntent,
    Installed,
    ReverseIntent,
    RolledBack,
    OriginalAbsenceRestored,
    FinalizeIntent,
    Finalized,
    FinalizeOriginalIntent,
    OriginalFinalized,
    OriginalAbsenceFinalized,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestoreBudget {
    pub baseline_bytes: u64,
    pub backup_bytes: u64,
    pub destination_bytes: u64,
    pub candidate_bytes: u64,
    pub rollback_new_bytes: u64,
    pub verifier_scratch_bytes: u64,
    pub metadata_bytes: u64,
    pub allocation_ceiling: u64,
    /// Initial read admission, independent of the strict new-allocation bound.
    pub read_byte_ceiling: u64,
    pub memory_ceiling: u64,
    pub metadata_payload_ceiling: u64,
    pub space_margin_bytes: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestoreJournal {
    pub version: u32,
    pub operation_id: uuid::Uuid,
    pub destination: PathBuf,
    pub staging: PathBuf,
    pub rollback: Option<PathBuf>,
    pub original: super::admission::FileBinding,
    pub original_verification: Option<super::VerificationReport>,
    pub original_read_error: Option<String>,
    pub candidate_identity: (u64, u64),
    pub candidate_sha256: String,
    pub staging_identity: (u64, u64),
    pub journal_identity: (u64, u64),
    pub operation_lock_identity: (u64, u64),
    pub journal_capacity: u64,
    pub selected_bundle_operation: uuid::Uuid,
    pub selected_bundle_sha256: String,
    pub binding: super::admission::OfflineLeaseRequest,
    pub verification: super::VerificationReport,
    pub budget: RestoreBudget,
    pub phase: RestorePhase,
    pub resume_finalize_attempts: u32,
    pub rollback_attempts: u32,
    pub current_attempt: u32,
    pub attempt_deadline_unix_ms: u128,
    pub forensic_handling_required: bool,
    pub reconciliation: Option<super::readiness::ReconciliationRecord>,
    pub absence_reconciliation: Option<super::readiness::AbsenceReconciliationRecord>,
    #[serde(skip)]
    pub(crate) log_end: u64,
    #[serde(skip)]
    pub(crate) log_hash: [u8; 32],
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum RestoreOutcome {
    Installed {
        journal: RestoreJournal,
    },
    RolledBack {
        journal: RestoreJournal,
    },
    OriginalAbsenceRestored {
        journal: RestoreJournal,
    },
    OriginalAbsenceFinalized {
        operation_id: uuid::Uuid,
    },
    Finalized {
        operation_id: uuid::Uuid,
        readiness: super::readiness::ReadinessReport,
    },
    OutcomeUnknown {
        operation_id: uuid::Uuid,
    },
}
use super::{
    admission::{self, FileBinding, OfflineAction, OperationOwnership, RestoreGuardV1},
    fs::{self, Dir, Directory, Member, PublicationIo, RecoveryEvent},
    manifest::BundleManifestV1,
    verify::{self, PrivateFile},
};
use sha2::{Digest, Sha256};
use std::{
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::MetadataExt,
    },
    path::Path,
    sync::Arc,
};
const LOG_HEADER: u64 = 72;
const GUARD_ALLOWANCE: u64 = 65536;

fn quota(l: &OperationLimits) -> RecoveryError {
    RecoveryError::Quota {
        operation_id: l.operation_id,
        resource: "restore candidate, verifier and all metadata within strict 2L",
    }
}
fn binding_identity(binding: &FileBinding) -> Option<(u64, u64)> {
    match binding {
        FileBinding::Absent => None,
        FileBinding::Readable { identity, .. } | FileBinding::Forensic { identity, .. } => {
            Some(*identity)
        }
    }
}
fn binding_bytes(binding: &FileBinding) -> u64 {
    match binding {
        FileBinding::Absent => 0,
        FileBinding::Readable { bytes, .. } | FileBinding::Forensic { bytes, .. } => *bytes,
    }
}
fn deadline_unix(
    fs: &dyn RecoveryFs,
    l: &OperationLimits,
    lease: &OfflineLease,
) -> Result<u128, RecoveryError> {
    let now = fs.io().unix_ms().map_err(|_| fs::destination(l))?;
    let remaining = l
        .deadline
        .checked_duration_since(std::time::Instant::now())
        .ok_or(RecoveryError::Deadline {
            operation_id: l.operation_id,
        })?
        .as_millis();
    now.checked_add(remaining)
        .map(|d| d.min(lease.request.expires_at_unix_ms))
        .filter(|d| *d > now)
        .ok_or(RecoveryError::Deadline {
            operation_id: l.operation_id,
        })
}
fn journal_path(j: &RestoreJournal) -> PathBuf {
    j.staging.join("journal.bin")
}
fn journal_append(
    j: &mut RestoreJournal,
    control: &Arc<PublicationIo>,
    l: &OperationLimits,
) -> Result<(), RecoveryError> {
    if control.io.unix_ms().map_err(|_| fs::destination(l))? >= j.attempt_deadline_unix_ms {
        return Err(RecoveryError::Deadline {
            operation_id: j.operation_id,
        });
    }
    let dir = Dir::open(&j.staging, l)?;
    let named_lock = PrivateFile::open(&j.staging.join("journal.bin.lock"), false, l)?;
    if named_lock.identity() != j.operation_lock_identity {
        return Err(l.verification());
    }
    if fs::identity(&dir.file, l)? != j.staging_identity {
        return Err(l.verification());
    }
    let log = PrivateFile::open(&journal_path(j), false, l)?;
    if log.identity() != j.journal_identity
        || log.file.metadata().map_err(|_| l.verification())?.len() != j.journal_capacity
    {
        return Err(l.verification());
    }
    // Journal metadata alone is writable; all image readers remain O_RDONLY.
    let fd = unsafe {
        libc::openat(
            dir.file.as_raw_fd(),
            c"journal.bin".as_ptr(),
            libc::O_RDWR | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(fs::destination(l));
    }
    let writable = unsafe { std::fs::File::from_raw_fd(fd) };
    verify::privacy(&writable, false, l)?;
    if fs::identity(&writable, l)? != log.identity() {
        return Err(l.verification());
    }
    let bytes = serde_json::to_vec(j).map_err(|_| l.verification())?;
    l.admit_memory(
        (bytes.len() as u64)
            .checked_mul(4)
            .ok_or_else(|| quota(l))?,
    )?;
    let len = u32::try_from(bytes.len()).map_err(|_| quota(l))?;
    let end = j
        .log_end
        .checked_add(LOG_HEADER)
        .and_then(|n| n.checked_add(bytes.len() as u64))
        .filter(|n| *n <= j.journal_capacity)
        .ok_or_else(|| quota(l))?;
    let mut hash = Sha256::new();
    hash.update(j.log_hash);
    hash.update(&bytes);
    let digest: [u8; 32] = hash.finalize().into();
    let mut header = Vec::with_capacity(LOG_HEADER as usize);
    header.extend_from_slice(b"RJR1");
    header.extend_from_slice(&len.to_le_bytes());
    header.extend_from_slice(&j.log_hash);
    header.extend_from_slice(&digest);
    control.write_all(&writable, &header, j.log_end, Member::RestoreJournal, l)?;
    control.write_all(
        &writable,
        &bytes,
        j.log_end + LOG_HEADER,
        Member::RestoreJournal,
        l,
    )?;
    control.sync(&writable, Member::RestoreJournal, l)?;
    dir.sync(&*control.io, Directory::RestoreJournalParent, l)?;
    j.log_end = end;
    j.log_hash = digest;
    Ok(())
}
fn original_verification(
    path: &Path,
    profile: &super::ValidationProfile,
    l: &OperationLimits,
) -> Result<super::VerificationReport, RecoveryError> {
    let held = PrivateFile::open(path, false, l)?;
    let (reader, descriptor) =
        crate::ferrostore::recovery::decode_physical_existing(&held.file, l)?;
    let expected =
        crate::ferrostore::recovery::expectations_from_reader(&reader, descriptor, profile, l)?;
    drop(reader);
    held.unchanged(l)?;
    verify::verify_physical_original(path, &expected, profile, l)
}
fn check_dependency_presence(
    manifest: &BundleManifestV1,
    available: &[super::readiness::DependencyResolution],
    l: &OperationLimits,
) -> Result<(), RecoveryError> {
    if manifest.reference_dependencies.iter().any(|d| {
        available
            .iter()
            .filter(|a| {
                a.reference_sha256 == d.reference_sha256
                    && a.catalog_identity == d.catalog_identity
                    && super::manifest::sha256(&a.evidence_sha256)
            })
            .count()
            != 1
    }) {
        return Err(fs::refused(
            l,
            "required cross-store dependency unavailable",
        ));
    }
    Ok(())
}
fn selected(
    request: &RestoreRequest,
    l: &OperationLimits,
) -> Result<
    (
        BundleManifestV1,
        String,
        super::ValidationExpectations,
        super::ValidationProfile,
        u64,
    ),
    RecoveryError,
> {
    let checked = super::catalog::retention_checked(&request.selected_bundle, l)?;
    let bundle_bytes = checked.bytes;
    let manifest = checked.manifest;
    let dir = Dir::open(request.selected_bundle.path(), l)?;
    let held = PrivateFile::open(&dir.path.join("manifest.json"), false, l)?;
    let digest = verify::hash_file(&held.file, l)?.1;
    let expected = fs::read_json(&dir, "validation.json", l)?;
    let profile = fs::read_json(&dir, "profile.json", l)?;
    Ok((manifest, digest, expected, profile, bundle_bytes))
}
fn assert_named_identity(
    dir: &Dir,
    name: &str,
    expected: (u64, u64),
    l: &OperationLimits,
) -> Result<(), RecoveryError> {
    if entry_identity(&dir.path.join(name), l)? != Some(expected) {
        return Err(l.verification());
    }
    Ok(())
}
#[allow(clippy::too_many_arguments)]
fn rename_exchange(
    from: &Dir,
    old: &str,
    to: &Dir,
    new: &str,
    event: RecoveryEvent,
    l: &OperationLimits,
    io: &dyn fs::RecoveryIo,
    first: (u64, u64),
    second: (u64, u64),
    validate: impl FnOnce() -> Result<(), RecoveryError>,
) -> Result<(), RecoveryError> {
    from.same_name(l)?;
    to.same_name(l)?;
    fs::event(io, event, l)?;
    fs::event(io, RecoveryEvent::Exchange, l)?;
    assert_named_identity(from, old, first, l)?;
    assert_named_identity(to, new, second, l)?;
    validate()?;
    let old = std::ffi::CString::new(old).map_err(|_| l.verification())?;
    let new = std::ffi::CString::new(new).map_err(|_| l.verification())?;
    #[cfg(target_os = "linux")]
    let result = unsafe {
        libc::renameat2(
            from.file.as_raw_fd(),
            old.as_ptr(),
            to.file.as_raw_fd(),
            new.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    #[cfg(not(target_os = "linux"))]
    let result = -1;
    if result != 0 {
        return Err(RecoveryError::Unsupported {
            operation_id: l.operation_id,
            reason: "native same-filesystem RENAME_EXCHANGE required; no copy fallback",
        });
    }
    Ok(())
}
fn sync_both(
    destination: &Dir,
    staging: &Dir,
    io: &dyn fs::RecoveryIo,
    l: &OperationLimits,
) -> Result<(), RecoveryError> {
    destination.sync(io, Directory::RestoreDestinationParent, l)?;
    staging.sync(io, Directory::RestoreStagingParent, l)
}
fn install(
    j: &mut RestoreJournal,
    control: &Arc<PublicationIo>,
    l: &OperationLimits,
) -> Result<(), RecoveryError> {
    let parent = Dir::open(j.destination.parent().ok_or_else(|| l.verification())?, l)?;
    let staging = Dir::open(&j.staging, l)?;
    let name = j
        .destination
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| l.verification())?
        .to_owned();
    if let Some(original) = binding_identity(&j.original) {
        j.phase = RestorePhase::ExchangeIntent;
        journal_append(j, control, l)?;
        rename_exchange(
            &staging,
            "candidate.fls",
            &parent,
            &name,
            RecoveryEvent::RestoreInstall,
            l,
            &*control.io,
            j.candidate_identity,
            original,
            || {
                matches_binding(
                    &staging.path.join("candidate.fls"),
                    &candidate_binding(j),
                    l,
                )?;
                matches_binding(&j.destination, &j.original, l)
            },
        )?;
        sync_both(&parent, &staging, &*control.io, l)?;
        j.phase = RestorePhase::RelocateIntent;
        journal_append(j, control, l)?;
        staging.rename_checked(
            "candidate.fls",
            &staging,
            "original.fls",
            &*control.io,
            RecoveryEvent::RestoreRelocate,
            l,
            || matches_binding(&staging.path.join("candidate.fls"), &j.original, l),
        )?;
        sync_both(&parent, &staging, &*control.io, l)?;
    } else {
        j.phase = RestorePhase::ExchangeIntent;
        journal_append(j, control, l)?;
        staging.rename_checked(
            "candidate.fls",
            &parent,
            &name,
            &*control.io,
            RecoveryEvent::RestoreInstall,
            l,
            || {
                matches_binding(
                    &staging.path.join("candidate.fls"),
                    &candidate_binding(j),
                    l,
                )
            },
        )?;
        sync_both(&parent, &staging, &*control.io, l)?;
    }
    j.phase = RestorePhase::Installed;
    journal_append(j, control, l)
}

pub fn restore_offline(
    request: &RestoreRequest,
    lease: &OfflineLease,
    filesystem: &dyn RecoveryFs,
) -> Result<RestoreOutcome, RecoveryError> {
    let mut l = request.limits.clone();
    l.operation_id = request.operation_id;
    l.check()?;
    if !cfg!(target_os = "linux") {
        return Err(RecoveryError::Unsupported {
            operation_id: l.operation_id,
            reason: "offline installation requires native Linux",
        });
    }
    lease.validate(filesystem, &l)?;
    let _owner = OperationOwnership::acquire(&lease.request.physical_path, &l)?;
    let physical = admission::canonical_physical(&request.destination)?;
    admission::check_restore_admission(&physical)?;
    let parent_path = physical.parent().ok_or_else(|| l.verification())?;
    if physical != request.destination
        || physical.extension().is_none_or(|e| e != "fls")
        || request.selected_bundle.path().parent() != Some(filesystem.root())
        || request.staging_parent != parent_path
        || lease.request.physical_path != physical
        || lease.request.operation_id != request.operation_id
        || request.operation_id.is_nil()
        || request.store.catalog_key != lease.request.catalog_identity
        || (!matches!(lease.request.expected_file, FileBinding::Absent)
            && request.store.alias_status != super::inventory::AliasStatus::NoAliasObserved)
    {
        return Err(fs::refused(
            &l,
            "explicit unaliased physical destination, lease and same-filesystem sibling staging required",
        ));
    }
    if let Some(identity) = binding_identity(&lease.request.expected_file)
        && (!request.store.exists
            || request.store.canonical_path.as_deref() != Some(physical.as_path())
            || request.store.filesystem_identity.as_deref()
                != Some(format!("{}:{}", identity.0, identity.1).as_str()))
    {
        return Err(fs::refused(&l, "inventory physical file identity mismatch"));
    }
    let catalog_root = Dir::open(filesystem.root(), &l)?;
    let _catalog_access = fs::CatalogLock::acquire(
        &catalog_root,
        fs::CatalogLockMode::Shared,
        false,
        filesystem.io(),
        &l,
    )?;
    let (manifest, bundle_digest, expected, profile, bundle_bytes) = selected(request, &l)?;
    if manifest.catalog_store_identity != request.store.catalog_key
        || profile.catalog_identity != request.store.catalog_key
        || manifest.validation_profile_sha256 != lease.request.profile_sha256
        || manifest.verifier_build_identity != super::manifest::BuildIdentity::current()
        || manifest.creator_build_identity != super::manifest::BuildIdentity::current()
        || lease.request.action
            != (OfflineAction::Restore {
                bundle_operation: uuid::Uuid::parse_str(&manifest.operation_id)
                    .map_err(|_| l.verification())?,
                bundle_sha256: bundle_digest.clone(),
            })
    {
        return Err(fs::refused(
            &l,
            "selected completed bundle identity, build, schema or lease binding mismatch",
        ));
    }
    check_dependency_presence(&manifest, &request.available_dependencies, &l)?;
    let parent = Dir::open(parent_path, &l)?;
    if fs::identity(&parent.file, &l)? != lease.request.parent_identity {
        return Err(l.verification());
    }
    let unit = fs::allocation_geometry(&parent, &l)?;
    let original = lease.request.expected_file.clone();
    let baseline = if matches!(original, FileBinding::Absent) {
        manifest.image_bytes
    } else {
        binding_bytes(&original)
    };
    let cap = baseline.checked_mul(2).ok_or_else(|| quota(&l))?;
    let metadata = fs::overhead_bytes(unit, &l)?
        .checked_add(unit)
        .and_then(|n| n.checked_add(request.metadata_byte_limit))
        .ok_or_else(|| quota(&l))?;
    let candidate = manifest
        .image_bytes
        .checked_add(unit - 1)
        .map(|n| n / unit * unit)
        .ok_or_else(|| quota(&l))?;
    let required = candidate.checked_add(metadata).ok_or_else(|| quota(&l))?;
    if baseline == 0
        || request.metadata_byte_limit <= GUARD_ALLOWANCE
        || required > cap
        || required > l.byte_limit
        || request.metadata_byte_limit > 512 << 20
        || filesystem
            .io()
            .available_bytes(&parent.file)
            .map_err(|_| fs::destination(&l))?
            .checked_sub(request.space_margin_bytes)
            .is_none_or(|free| required > free)
    {
        return Err(quota(&l));
    }
    let budget = RestoreBudget {
        baseline_bytes: baseline,
        backup_bytes: bundle_bytes,
        destination_bytes: binding_bytes(&original),
        candidate_bytes: candidate,
        rollback_new_bytes: 0,
        verifier_scratch_bytes: 0,
        metadata_bytes: metadata,
        allocation_ceiling: cap.min(l.byte_limit),
        read_byte_ceiling: l.byte_limit,
        memory_ceiling: l.memory_byte_limit,
        metadata_payload_ceiling: request.metadata_byte_limit,
        space_margin_bytes: request.space_margin_bytes,
    };
    let control = Arc::new(PublicationIo::new(
        filesystem.io(),
        budget.allocation_ceiling,
        metadata,
        manifest.image_bytes,
        unit,
    ));
    control.charge(
        fs::overhead_bytes(unit, &l)?
            .checked_add(unit)
            .ok_or_else(|| quota(&l))?,
        true,
        &l,
    )?;
    l.publication_io = Some(control.clone());
    let (original_report, original_read_error) = if matches!(original, FileBinding::Absent) {
        (None, None)
    } else {
        match original_verification(&physical, &profile, &l) {
            Ok(report) => (Some(report), None),
            Err(
                error @ (RecoveryError::Quota { .. }
                | RecoveryError::Deadline { .. }
                | RecoveryError::Cancelled { .. }),
            ) => return Err(error),
            Err(error) => {
                l.check()?;
                (
                    None,
                    Some(match error {
                        RecoveryError::Transient { errno, .. } => format!("errno:{errno}"),
                        _ => "original image unreadable or unverifiable".into(),
                    }),
                )
            }
        }
    };
    lease.validate(filesystem, &l)?;
    let staging_name = format!(".restore-{}-{}", request.operation_id, uuid::Uuid::now_v7());
    let staging_path = parent.path.join(&staging_name);
    drop(_catalog_access);
    let mut admission_staging_identity = None;
    let mut admission_lock_identity = None;
    let (lock, _catalog_access) = (|| {
        let staging = parent.child(&staging_name, true, &l)?;
        admission_staging_identity = Some(fs::identity(&staging.file, &l)?);
        if staging.file.metadata().map_err(|_| l.verification())?.dev()
            != parent.file.metadata().map_err(|_| l.verification())?.dev()
        {
            return Err(fs::refused(&l, "same-filesystem sibling staging required"));
        }
        let lock = RestoreOperationLock::acquire(staging, true, false, lease, filesystem.io(), &l)?;
        admission_lock_identity = Some(lock.held.identity());
        parent.sync(&*control.io, Directory::RestoreDestinationParent, &l)?;
        let _catalog_access = fs::CatalogLock::acquire(
            &catalog_root,
            fs::CatalogLockMode::Shared,
            false,
            filesystem.io(),
            &l,
        )?;
        let (_, rechecked_digest, _, _, _) = selected(request, &l)?;
        if rechecked_digest != bundle_digest {
            return Err(l.verification());
        }
        lease.validate(filesystem, &l)?;
        let guard = RestoreGuardV1 {
            version: 1,
            operation_id: request.operation_id,
            physical_path: physical.clone(),
            parent_identity: lease.request.parent_identity,
            journal: staging_path.join("journal.bin"),
            phase: "restore_pending".into(),
        };
        let guard_name = admission::guard_path(&physical)
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| l.verification())?
            .to_owned();
        // The first fence is exclusive, private, file-synced and parent-synced.
        fs::write_json(
            &parent,
            &guard_name,
            &guard,
            Member::RestoreGuard,
            &control,
            &l,
        )?;
        parent
            .sync(&*control.io, Directory::RestoreGuardParent, &l)
            .map_err(|_| RecoveryError::OutcomeUnknown {
                operation_id: l.operation_id,
            })?;
        Ok((lock, _catalog_access))
    })()
    .map_err(|source| RecoveryError::RestoreAdmissionArtifactsRetained {
        operation_id: l.operation_id,
        staging: staging_path.clone(),
        staging_identity: admission_staging_identity,
        operation_lock_identity: admission_lock_identity,
        source: Box::new(source),
    })?;
    let result = (|| {
        let staging = &lock.staging;
        let image =
            PrivateFile::open(&request.selected_bundle.path().join("image.fls"), false, &l)?;
        fs::event(&*control.io, RecoveryEvent::Create(Member::Image), &l)?;
        let candidate = PrivateFile::open(&staging.path.join("candidate.fls"), true, &l)?;
        if image.identity() == candidate.identity() {
            return Err(l.verification());
        }
        control.prepare_copy(&candidate.file, manifest.image_bytes, &l)?;
        let mut offset = 0;
        let mut buffer = [0; 65536];
        while offset < manifest.image_bytes {
            let n = (manifest.image_bytes - offset).min(buffer.len() as u64) as usize;
            fs::read_exact_at(&image.file, &mut buffer[..n], offset, &l)?;
            control.write_copy(&candidate.file, &buffer[..n], offset, &l)?;
            offset += n as u64;
        }
        control.sync(&candidate.file, Member::Image, &l)?;
        candidate.same_name(&l)?;
        let candidate_identity = candidate.identity();
        drop(candidate);
        // Verify this one candidate directly. Never copy_and_verify here.
        let verification = super::verify_existing_copy(
            &staging.path.join("candidate.fls"),
            &expected,
            &profile,
            &l,
        )?;
        if verification.image_sha256 != manifest.image_sha256
            || verification.copy_evidence.as_ref().is_none_or(|e| {
                e.verified_file_identity != candidate_identity
                    || e.verified_file_identity == image.identity()
            })
        {
            return Err(l.verification());
        }
        image.unchanged(&l)?;
        if super::catalog::retention_manifest(&request.selected_bundle, &l)? != manifest {
            return Err(l.verification());
        }
        if admission::inspect_file_binding(&physical, &l)? != original {
            return Err(l.verification());
        }
        if matches!(original, FileBinding::Readable { .. }) {
            let old = fs::source_file(&physical, &l)?;
            control.sync(&old, Member::Original, &l)?;
        }
        fs::event(
            &*control.io,
            RecoveryEvent::Create(Member::RestoreJournal),
            &l,
        )?;
        let log = PrivateFile::open(&staging.path.join("journal.bin"), true, &l)?;
        let capacity = request.metadata_byte_limit - GUARD_ALLOWANCE;
        control.allocate(&log.file, capacity, true, &l)?;
        let mut journal = RestoreJournal {
            version: 1,
            operation_id: request.operation_id,
            destination: physical.clone(),
            staging: staging.path.clone(),
            rollback: binding_identity(&original).map(|_| staging.path.join("original.fls")),
            original,
            original_verification: original_report,
            original_read_error,
            candidate_identity,
            candidate_sha256: manifest.image_sha256.clone(),
            staging_identity: fs::identity(&staging.file, &l)?,
            journal_identity: log.identity(),
            operation_lock_identity: lock.held.identity(),
            journal_capacity: capacity,
            selected_bundle_operation: uuid::Uuid::parse_str(&manifest.operation_id)
                .map_err(|_| l.verification())?,
            selected_bundle_sha256: bundle_digest,
            binding: lease.request.clone(),
            verification,
            budget,
            phase: RestorePhase::Prepared,
            resume_finalize_attempts: 0,
            rollback_attempts: 0,
            current_attempt: 0,
            attempt_deadline_unix_ms: deadline_unix(filesystem, &l, lease)?,
            forensic_handling_required: false,
            reconciliation: None,
            absence_reconciliation: None,
            log_end: 0,
            log_hash: [0; 32],
        };
        journal_append(&mut journal, &control, &l)?;
        staging.sync(&*control.io, Directory::RestoreStagingParent, &l)?;
        parent.sync(&*control.io, Directory::RestoreDestinationParent, &l)?;
        install(&mut journal, &control, &l)?;
        Ok(RestoreOutcome::Installed { journal })
    })();
    result.map_err(|error| match error {
        RecoveryError::Unsupported { .. } => error,
        _ => RecoveryError::OutcomeUnknown {
            operation_id: l.operation_id,
        },
    })
}

/// Serializes recovery actions only; ordinary/legacy clients still require
/// administrative exclusion. The stable inode is never recreated on restart.
pub(crate) struct RestoreOperationLock {
    held: PrivateFile,
    staging: Dir,
}
impl RestoreOperationLock {
    fn acquire(
        staging: Dir,
        create: bool,
        shared: bool,
        lease: &OfflineLease,
        io: Arc<dyn fs::RecoveryIo>,
        l: &OperationLimits,
    ) -> Result<Self, RecoveryError> {
        fs::event(&*io, RecoveryEvent::RestoreOperationLock, l)?;
        if create {
            fs::event(&*io, RecoveryEvent::Create(Member::RestoreOperationLock), l)?;
        }
        let held = PrivateFile::open(&staging.path.join("journal.bin.lock"), create, l)?;
        if held.file.metadata().map_err(|_| l.verification())?.len() != 0 {
            return Err(l.verification());
        }
        let lock = Self { held, staging };
        loop {
            l.check()?;
            if io.unix_ms().map_err(|_| fs::destination(l))? >= lease.request.expires_at_unix_ms {
                return Err(RecoveryError::Deadline {
                    operation_id: l.operation_id,
                });
            }
            lock.held.unchanged(l)?;
            lock.staging.same_name(l)?;
            let mode = if shared { libc::LOCK_SH } else { libc::LOCK_EX };
            if unsafe { libc::flock(lock.held.file.as_raw_fd(), mode | libc::LOCK_NB) } == 0 {
                break;
            }
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EWOULDBLOCK) {
                return Err(fs::transient(&error, l).unwrap_or_else(|| fs::destination(l)));
            }
            fs::event(&*io, RecoveryEvent::RestoreOperationLockWait, l)?;
            std::thread::park_timeout(std::time::Duration::from_millis(2));
        }
        lock.held.unchanged(l)?;
        if create {
            fs::event(&*io, RecoveryEvent::Flush(Member::RestoreOperationLock), l)?;
            lock.held.file.sync_all().map_err(|_| fs::destination(l))?;
            lock.staging
                .sync(&*io, Directory::RestoreOperationLockParent, l)?;
        }
        Ok(lock)
    }
    fn validate(&self, j: &RestoreJournal, l: &OperationLimits) -> Result<(), RecoveryError> {
        self.held.unchanged(l)?;
        self.staging.same_name(l)?;
        if self.held.identity() != j.operation_lock_identity
            || self.staging.path != j.staging
            || fs::identity(&self.staging.file, l)? != j.staging_identity
        {
            return Err(l.verification());
        }
        Ok(())
    }
}
fn existing_operation_lock(
    path: &Path,
    lease: &OfflineLease,
    filesystem: &dyn RecoveryFs,
    shared: bool,
    require_guard: bool,
) -> Result<RestoreOperationLock, RecoveryError> {
    let mut l = filesystem.reconciliation_limits().clone();
    l.operation_id = lease.request.operation_id;
    lease.validate_attestation(filesystem, &l)?;
    let now = filesystem.io().unix_ms().map_err(|_| fs::destination(&l))?;
    let remaining = lease
        .request
        .expires_at_unix_ms
        .checked_sub(now)
        .ok_or_else(|| l.verification())?;
    l.deadline = l.deadline.min(
        std::time::Instant::now()
            + std::time::Duration::from_millis(
                u64::try_from(remaining).map_err(|_| l.verification())?,
            ),
    );
    if require_guard && read_guard(lease, filesystem, &l)?.journal != path {
        return Err(l.verification());
    }
    let staging_path = path.parent().ok_or_else(|| l.verification())?;
    let prefix = format!(".restore-{}-", lease.request.operation_id);
    if path.file_name() != Some(std::ffi::OsStr::new("journal.bin"))
        || staging_path.parent() != lease.request.physical_path.parent()
        || !staging_path
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|s| {
                s.strip_prefix(&prefix)
                    .is_some_and(|suffix| uuid::Uuid::parse_str(suffix).is_ok())
            })
    {
        return Err(l.verification());
    }
    RestoreOperationLock::acquire(
        Dir::open(staging_path, &l)?,
        false,
        shared,
        lease,
        filesystem.io(),
        &l,
    )
}
pub(crate) fn load_with_held_lock(
    path: &Path,
    lease: &OfflineLease,
    filesystem: &dyn RecoveryFs,
    lock: &RestoreOperationLock,
) -> Result<RestoreJournal, RecoveryError> {
    let j = load_journal(path, lease, filesystem, true)?;
    let mut limits = filesystem.reconciliation_limits().clone();
    limits.operation_id = j.operation_id;
    lock.validate(&j, &limits)?;
    Ok(j)
}
fn immutable_digest(j: &RestoreJournal, l: &OperationLimits) -> Result<String, RecoveryError> {
    let mut fixed = j.clone();
    fixed.phase = RestorePhase::Prepared;
    fixed.resume_finalize_attempts = 0;
    fixed.rollback_attempts = 0;
    fixed.current_attempt = 0;
    fixed.attempt_deadline_unix_ms = 0;
    fixed.forensic_handling_required = false;
    fixed.reconciliation = None;
    fixed.absence_reconciliation = None;
    Ok(verify::serialized_checksum(&fixed, l)?.1)
}
fn lease_binding_matches(
    a: &super::admission::OfflineLeaseRequest,
    b: &super::admission::OfflineLeaseRequest,
) -> bool {
    a.version == b.version
        && a.operation_id == b.operation_id
        && a.action == b.action
        && a.catalog_identity == b.catalog_identity
        && a.approved_root_identity == b.approved_root_identity
        && a.physical_path == b.physical_path
        && a.parent_identity == b.parent_identity
        && a.profile_sha256 == b.profile_sha256
        && a.build == b.build
}
fn read_guard(
    lease: &OfflineLease,
    filesystem: &dyn RecoveryFs,
    l: &OperationLimits,
) -> Result<RestoreGuardV1, RecoveryError> {
    lease.validate_attestation(filesystem, l)?;
    let parent = Dir::open(
        lease
            .request
            .physical_path
            .parent()
            .ok_or_else(|| l.verification())?,
        l,
    )?;
    let path = admission::guard_path(&lease.request.physical_path);
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| l.verification())?;
    let guard: RestoreGuardV1 = fs::read_json(&parent, name, l)?;
    if guard.version != 1
        || guard.operation_id != lease.request.operation_id
        || guard.physical_path != lease.request.physical_path
        || guard.parent_identity != lease.request.parent_identity
    {
        return Err(l.verification());
    }
    Ok(guard)
}
/// Load bounded, chained durable metadata only. A torn or malformed tail is
/// refused, never interpreted as permission to guess at surviving names.
pub fn load_restore_journal(
    path: &Path,
    lease: &OfflineLease,
    filesystem: &dyn RecoveryFs,
) -> Result<RestoreJournal, RecoveryError> {
    let lock = existing_operation_lock(path, lease, filesystem, true, true)?;
    load_with_held_lock(path, lease, filesystem, &lock)
}
fn load_journal(
    path: &Path,
    lease: &OfflineLease,
    filesystem: &dyn RecoveryFs,
    require_guard: bool,
) -> Result<RestoreJournal, RecoveryError> {
    let mut l = filesystem.reconciliation_limits().clone();
    l.operation_id = lease.request.operation_id;
    l.check()?;
    let guard_path = admission::guard_path(&lease.request.physical_path);
    let guard = match std::fs::symlink_metadata(&guard_path) {
        Ok(_) => Some(read_guard(lease, filesystem, &l)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !require_guard => {
            lease.validate_attestation(filesystem, &l)?;
            None
        }
        Err(_) => return Err(l.verification()),
    };
    if guard.as_ref().is_some_and(|g| g.journal != path)
        || path.file_name() != Some(std::ffi::OsStr::new("journal.bin"))
    {
        return Err(l.verification());
    }
    let staging_path = path.parent().ok_or_else(|| l.verification())?;
    let prefix = format!(".restore-{}-", lease.request.operation_id);
    if staging_path.parent() != lease.request.physical_path.parent()
        || !staging_path
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|s| {
                s.strip_prefix(&prefix)
                    .is_some_and(|suffix| uuid::Uuid::parse_str(suffix).is_ok())
            })
    {
        return Err(l.verification());
    }
    let staging = Dir::open(staging_path, &l)?;
    let log = PrivateFile::open(path, false, &l)?;
    let capacity = log.file.metadata().map_err(|_| l.verification())?.len();
    l.quota(capacity, l.byte_limit, "journal bytes")?;
    let mut offset = 0;
    let mut previous = [0; 32];
    let mut last: Option<RestoreJournal> = None;
    let mut immutable = None;
    while offset < capacity {
        let remaining = capacity - offset;
        if remaining < LOG_HEADER {
            let mut tail = vec![0; remaining as usize];
            fs::read_exact_at(&log.file, &mut tail, offset, &l)?;
            if tail.iter().any(|b| *b != 0) {
                return Err(l.verification());
            }
            break;
        }
        let mut header = [0; LOG_HEADER as usize];
        fs::read_exact_at(&log.file, &mut header, offset, &l)?;
        if header.iter().all(|b| *b == 0) {
            let mut tail = [0; 65536];
            let mut tail_offset = offset;
            while tail_offset < capacity {
                let n = (capacity - tail_offset).min(tail.len() as u64) as usize;
                fs::read_exact_at(&log.file, &mut tail[..n], tail_offset, &l)?;
                if tail[..n].iter().any(|b| *b != 0) {
                    return Err(l.verification());
                }
                tail_offset += n as u64;
            }
            break;
        }
        if &header[..4] != b"RJR1" || header[8..40] != previous {
            return Err(l.verification());
        }
        let length =
            u32::from_le_bytes(header[4..8].try_into().map_err(|_| l.verification())?) as u64;
        let end = offset
            .checked_add(LOG_HEADER)
            .and_then(|n| n.checked_add(length))
            .filter(|n| *n <= capacity)
            .ok_or_else(|| l.verification())?;
        l.admit_memory(length.checked_mul(32).ok_or_else(|| l.verification())?)?;
        let mut bytes = vec![0; usize::try_from(length).map_err(|_| l.verification())?];
        fs::read_exact_at(&log.file, &mut bytes, offset + LOG_HEADER, &l)?;
        let mut hash = Sha256::new();
        hash.update(previous);
        hash.update(&bytes);
        let digest: [u8; 32] = hash.finalize().into();
        if header[40..72] != digest {
            return Err(l.verification());
        }
        let mut current: RestoreJournal =
            serde_json::from_slice(&bytes).map_err(|_| l.verification())?;
        if current.version != 1
            || current.operation_id != lease.request.operation_id
            || current.destination != lease.request.physical_path
            || current.staging != staging.path
            || current.staging_identity != fs::identity(&staging.file, &l)?
            || current.journal_identity != log.identity()
            || current.journal_capacity != capacity
            || !lease_binding_matches(&current.binding, &lease.request)
            || current.original != current.binding.expected_file
            || current.resume_finalize_attempts > 12
            || current.rollback_attempts > 4
            || current.current_attempt > 16
            || current.budget.read_byte_ceiling == 0
            || current.budget.backup_bytes > current.budget.read_byte_ceiling
            || current.budget.allocation_ceiling > current.budget.read_byte_ceiling
            || current.budget.rollback_new_bytes != 0
            || current.budget.verifier_scratch_bytes != 0
            || current
                .budget
                .candidate_bytes
                .checked_add(current.budget.metadata_bytes)
                .is_none_or(|n| n > current.budget.allocation_ceiling)
            || current
                .budget
                .baseline_bytes
                .checked_mul(2)
                .is_none_or(|n| current.budget.allocation_ceiling > n)
        {
            return Err(l.verification());
        }
        if let Some(old) = &last
            && (current.resume_finalize_attempts < old.resume_finalize_attempts
                || current.rollback_attempts < old.rollback_attempts
                || current.current_attempt < old.current_attempt)
        {
            return Err(l.verification());
        }
        let fixed = immutable_digest(&current, &l)?;
        if immutable
            .as_ref()
            .is_some_and(|expected| expected != &fixed)
        {
            return Err(l.verification());
        }
        immutable = Some(fixed);
        current.log_end = end;
        current.log_hash = digest;
        offset = end;
        previous = digest;
        last = Some(current);
    }
    log.unchanged(&l)?;
    let last = last.ok_or_else(|| l.verification())?;
    if guard.is_none()
        && !matches!(
            last.phase,
            RestorePhase::Finalized
                | RestorePhase::OriginalFinalized
                | RestorePhase::OriginalAbsenceFinalized
        )
    {
        return Err(l.verification());
    }
    if last.binding != lease.request {
        // A renewed attestation binds the currently surviving destination. The
        // immutable admission remains the original request in every log frame.
        lease.validate(filesystem, &l)?;
        inspect_phase_layout(&last, &l)?;
    }
    Ok(last)
}
fn attempt(
    j: &mut RestoreJournal,
    lease: &OfflineLease,
    filesystem: &dyn RecoveryFs,
    rollback: bool,
) -> Result<(OperationLimits, Arc<PublicationIo>), RecoveryError> {
    let mut l = filesystem.reconciliation_limits().clone();
    l.operation_id = j.operation_id;
    l.byte_limit = l.byte_limit.min(j.budget.read_byte_ceiling);
    l.memory_byte_limit = l.memory_byte_limit.min(j.budget.memory_ceiling);
    lease.validate_attestation(filesystem, &l)?;
    inspect_phase_layout(j, &l)?;
    if j.forensic_handling_required
        || j.rollback_attempts >= 4
        || (!rollback && j.resume_finalize_attempts >= 12)
    {
        return Err(fs::refused(
            &l,
            "bounded attempts exhausted; forensic_handling_required",
        ));
    }
    let now = filesystem.io().unix_ms().map_err(|_| fs::destination(&l))?;
    let deadline = deadline_unix(filesystem, &l, lease)?;
    let remaining = deadline.checked_sub(now).ok_or_else(|| l.verification())?;
    let duration =
        std::time::Duration::from_millis(u64::try_from(remaining).map_err(|_| l.verification())?);
    l.deadline = l.deadline.min(std::time::Instant::now() + duration);
    let parent = Dir::open(j.destination.parent().ok_or_else(|| l.verification())?, &l)?;
    let unit = fs::allocation_geometry(&parent, &l)?;
    let control = Arc::new(PublicationIo::new(
        filesystem.io(),
        j.budget.allocation_ceiling,
        j.budget.metadata_bytes,
        j.verification.image_bytes,
        unit,
    ));
    l.publication_io = Some(control.clone());
    if rollback {
        j.rollback_attempts += 1;
    } else {
        j.resume_finalize_attempts += 1;
    }
    j.current_attempt += 1;
    j.attempt_deadline_unix_ms = deadline;
    j.forensic_handling_required = j.rollback_attempts >= 4;
    journal_append(j, &control, &l)?;
    Ok((l, control))
}
fn entry_identity(path: &Path, l: &OperationLimits) -> Result<Option<(u64, u64)>, RecoveryError> {
    let (parent, name) = verify::walk_parent(path, false, l)?;
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
            return Ok(None);
        }
        return Err(l.verification());
    }
    let held = unsafe { std::fs::File::from_raw_fd(fd) };
    let m = held.metadata().map_err(|_| l.verification())?;
    if !m.is_file() || m.nlink() != 1 {
        return Err(l.verification());
    }
    Ok(Some((m.dev(), m.ino())))
}
fn candidate_binding(j: &RestoreJournal) -> FileBinding {
    FileBinding::Readable {
        identity: j.candidate_identity,
        bytes: j.verification.image_bytes,
        sha256: j.candidate_sha256.clone(),
    }
}
fn matches_binding(
    path: &Path,
    binding: &FileBinding,
    l: &OperationLimits,
) -> Result<(), RecoveryError> {
    if admission::inspect_file_binding(path, l)? != *binding {
        return Err(l.verification());
    }
    Ok(())
}
type RestoreLayout = (Option<(u64, u64)>, Option<(u64, u64)>, Option<(u64, u64)>);
fn inspect_layout(j: &RestoreJournal, l: &OperationLimits) -> Result<RestoreLayout, RecoveryError> {
    let dest = entry_identity(&j.destination, l)?;
    let candidate = entry_identity(&j.staging.join("candidate.fls"), l)?;
    let original = entry_identity(&j.staging.join("original.fls"), l)?;
    let expected_original = binding_identity(&j.original);
    let values = [dest, candidate, original];
    if values
        .iter()
        .flatten()
        .any(|id| *id != j.candidate_identity && Some(*id) != expected_original)
        || values
            .iter()
            .filter(|id| **id == Some(j.candidate_identity))
            .count()
            != 1
        || (expected_original.is_some()
            && values.iter().filter(|id| **id == expected_original).count() != 1)
    {
        return Err(l.verification());
    }
    for (path, id) in [
        (j.destination.clone(), dest),
        (j.staging.join("candidate.fls"), candidate),
        (j.staging.join("original.fls"), original),
    ] {
        if id == Some(j.candidate_identity) {
            matches_binding(&path, &candidate_binding(j), l)?;
        } else if id.is_some() {
            matches_binding(&path, &j.original, l)?;
        }
    }
    Ok((dest, candidate, original))
}
/// Admit only the complete before/after layouts of this journal phase.
/// Every name has already been checked against its held identity and digest.
fn inspect_phase_layout(j: &RestoreJournal, l: &OperationLimits) -> Result<(), RecoveryError> {
    let layout = inspect_layout(j, l)?;
    let c = Some(j.candidate_identity);
    let o = binding_identity(&j.original);
    let prepared = (o, c, None);
    let exchanged = (c, o, None);
    let installed = (c, None, o);
    let reversed = if o.is_some() {
        (o, None, c)
    } else {
        (None, c, None)
    };
    let valid = match j.phase {
        RestorePhase::Prepared => layout == prepared,
        RestorePhase::ExchangeIntent => layout == prepared || layout == exchanged,
        RestorePhase::RelocateIntent => o.is_some() && (layout == exchanged || layout == installed),
        RestorePhase::Installed | RestorePhase::FinalizeIntent | RestorePhase::Finalized => {
            layout == installed
        }
        RestorePhase::ReverseIntent => layout == installed || layout == reversed,
        RestorePhase::RolledBack => o.is_some() && layout == reversed,
        RestorePhase::FinalizeOriginalIntent | RestorePhase::OriginalFinalized => {
            o.is_some() && j.original_verification.is_some() && layout == reversed
        }
        RestorePhase::OriginalAbsenceRestored | RestorePhase::OriginalAbsenceFinalized => {
            o.is_none() && layout == reversed
        }
    };
    if !valid {
        return Err(l.verification());
    }
    Ok(())
}
fn verify_candidate_at(
    j: &RestoreJournal,
    path: &Path,
    lease: &OfflineLease,
    filesystem: &dyn RecoveryFs,
    lock: &RestoreOperationLock,
    l: &OperationLimits,
) -> Result<(), RecoveryError> {
    let bundle =
        super::catalog::select_completed_bundle(j.selected_bundle_operation, filesystem, l)?;
    let dir = Dir::open(bundle.path(), l)?;
    let manifest = super::catalog::retention_manifest(&bundle, l)?;
    if verify::hash_file(
        &PrivateFile::open(&dir.path.join("manifest.json"), false, l)?.file,
        l,
    )?
    .1 != j.selected_bundle_sha256
    {
        return Err(l.verification());
    }
    let expected: super::ValidationExpectations = fs::read_json(&dir, "validation.json", l)?;
    let profile = fs::read_json(&dir, "profile.json", l)?;
    let access = admission::RestoreAccess::obtain(j, lease, filesystem, lock, l)?;
    let report = verify::verify_restore_copy(path, &expected, &profile, l, &access)?;
    if report.image_sha256 != j.candidate_sha256
        || report.logical_sha256 != j.verification.logical_sha256
        || report.published_sequence != manifest.published_sequence
    {
        return Err(l.verification());
    }
    Ok(())
}
pub fn resume_restore(
    journal: &RestoreJournal,
    lease: &OfflineLease,
    filesystem: &dyn RecoveryFs,
) -> Result<RestoreOutcome, RecoveryError> {
    let _owner = OperationOwnership::acquire(
        &lease.request.physical_path,
        filesystem.reconciliation_limits(),
    )?;
    let lock = existing_operation_lock(&journal_path(journal), lease, filesystem, false, true)?;
    let mut j = load_with_held_lock(&journal_path(journal), lease, filesystem, &lock)?;
    let catalog_root = Dir::open(filesystem.root(), filesystem.reconciliation_limits())?;
    let _catalog_access = fs::CatalogLock::acquire(
        &catalog_root,
        fs::CatalogLockMode::Shared,
        false,
        filesystem.io(),
        filesystem.reconciliation_limits(),
    )?;
    let (l, control) = attempt(&mut j, lease, filesystem, false)?;
    let (dest, candidate, original) = inspect_layout(&j, &l)?;
    let parent = Dir::open(j.destination.parent().ok_or_else(|| l.verification())?, &l)?;
    let staging = Dir::open(&j.staging, &l)?;
    if dest == binding_identity(&j.original)
        && candidate == Some(j.candidate_identity)
        && original.is_none()
    {
        verify_candidate_at(
            &j,
            &staging.path.join("candidate.fls"),
            lease,
            filesystem,
            &lock,
            &l,
        )?;
        install(&mut j, &control, &l)?;
    } else if dest == Some(j.candidate_identity)
        && candidate == binding_identity(&j.original)
        && original.is_none()
        && candidate.is_some()
    {
        verify_candidate_at(&j, &j.destination, lease, filesystem, &lock, &l)?;
        sync_both(&parent, &staging, &*control.io, &l)?;
        j.phase = RestorePhase::RelocateIntent;
        journal_append(&mut j, &control, &l)?;
        staging.rename_checked(
            "candidate.fls",
            &staging,
            "original.fls",
            &*control.io,
            RecoveryEvent::RestoreRelocate,
            &l,
            || matches_binding(&staging.path.join("candidate.fls"), &j.original, &l),
        )?;
        sync_both(&parent, &staging, &*control.io, &l)?;
        j.phase = RestorePhase::Installed;
        journal_append(&mut j, &control, &l)?;
    } else if dest == Some(j.candidate_identity)
        && candidate.is_none()
        && original == binding_identity(&j.original)
    {
        verify_candidate_at(&j, &j.destination, lease, filesystem, &lock, &l)?;
        sync_both(&parent, &staging, &*control.io, &l)?;
        j.phase = RestorePhase::Installed;
        journal_append(&mut j, &control, &l)?;
    } else if dest == binding_identity(&j.original)
        && original == Some(j.candidate_identity)
        && candidate.is_none()
        && dest.is_some()
    {
        if j.original_verification.is_none() {
            return Err(fs::refused(
                &l,
                "forensic original is not verified rollback",
            ));
        }
        sync_both(&parent, &staging, &*control.io, &l)?;
        j.phase = RestorePhase::RolledBack;
        journal_append(&mut j, &control, &l)?;
        return Ok(RestoreOutcome::RolledBack { journal: j });
    } else {
        return Err(l.verification());
    }
    Ok(RestoreOutcome::Installed { journal: j })
}
pub fn rollback_restore(
    journal: &RestoreJournal,
    lease: &OfflineLease,
    filesystem: &dyn RecoveryFs,
) -> Result<RestoreOutcome, RecoveryError> {
    let _owner = OperationOwnership::acquire(
        &lease.request.physical_path,
        filesystem.reconciliation_limits(),
    )?;
    let lock = existing_operation_lock(&journal_path(journal), lease, filesystem, false, true)?;
    let mut j = load_with_held_lock(&journal_path(journal), lease, filesystem, &lock)?;
    let catalog_root = Dir::open(filesystem.root(), filesystem.reconciliation_limits())?;
    let _catalog_access = fs::CatalogLock::acquire(
        &catalog_root,
        fs::CatalogLockMode::Shared,
        false,
        filesystem.io(),
        filesystem.reconciliation_limits(),
    )?;
    let (l, control) = attempt(&mut j, lease, filesystem, true)?;
    if matches!(j.original, FileBinding::Absent) {
        let (dest, candidate, original) = inspect_layout(&j, &l)?;
        let parent = Dir::open(j.destination.parent().ok_or_else(|| l.verification())?, &l)?;
        let staging = Dir::open(&j.staging, &l)?;
        if original.is_some() {
            return Err(l.verification());
        }
        if dest == Some(j.candidate_identity) && candidate.is_none() {
            verify_candidate_at(&j, &j.destination, lease, filesystem, &lock, &l)?;
            j.phase = RestorePhase::ReverseIntent;
            journal_append(&mut j, &control, &l)?;
            let name = j
                .destination
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or_else(|| l.verification())?;
            parent.rename_checked(
                name,
                &staging,
                "candidate.fls",
                &*control.io,
                RecoveryEvent::RestoreRelocate,
                &l,
                || matches_binding(&j.destination, &candidate_binding(&j), &l),
            )?;
            sync_both(&parent, &staging, &*control.io, &l)?;
        } else if dest.is_none() && candidate == Some(j.candidate_identity) {
            sync_both(&parent, &staging, &*control.io, &l)?;
        } else {
            return Err(l.verification());
        }
        j.phase = RestorePhase::OriginalAbsenceRestored;
        journal_append(&mut j, &control, &l)?;
        return Ok(RestoreOutcome::OriginalAbsenceRestored { journal: j });
    }
    if j.original_verification.is_none() {
        return Err(fs::refused(
            &l,
            "verified original required; forensic material retained",
        ));
    }
    let (dest, candidate, original) = inspect_layout(&j, &l)?;
    let original_identity = binding_identity(&j.original).ok_or_else(|| l.verification())?;
    let parent = Dir::open(j.destination.parent().ok_or_else(|| l.verification())?, &l)?;
    let staging = Dir::open(&j.staging, &l)?;
    if dest == Some(original_identity)
        && original == Some(j.candidate_identity)
        && candidate.is_none()
    {
        sync_both(&parent, &staging, &*control.io, &l)?;
        j.phase = RestorePhase::RolledBack;
        journal_append(&mut j, &control, &l)?;
        return Ok(RestoreOutcome::RolledBack { journal: j });
    }
    if dest != Some(j.candidate_identity)
        || candidate.is_some()
        || original != Some(original_identity)
    {
        return Err(l.verification());
    }
    verify_candidate_at(&j, &j.destination, lease, filesystem, &lock, &l)?;
    j.phase = RestorePhase::ReverseIntent;
    journal_append(&mut j, &control, &l)?;
    let name = j
        .destination
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| l.verification())?;
    rename_exchange(
        &staging,
        "original.fls",
        &parent,
        name,
        RecoveryEvent::RestoreReverseExchange,
        &l,
        &*control.io,
        original_identity,
        j.candidate_identity,
        || {
            matches_binding(&staging.path.join("original.fls"), &j.original, &l)?;
            matches_binding(&j.destination, &candidate_binding(&j), &l)
        },
    )?;
    sync_both(&parent, &staging, &*control.io, &l)?;
    matches_binding(&j.destination, &j.original, &l)?;
    j.phase = RestorePhase::RolledBack;
    journal_append(&mut j, &control, &l)?;
    Ok(RestoreOutcome::RolledBack { journal: j })
}
pub(crate) fn finalize(
    journal: &RestoreJournal,
    lease: &OfflineLease,
    record: &super::readiness::ReconciliationRecord,
    filesystem: &dyn RecoveryFs,
    rollback_reserve: bool,
) -> Result<RestoreOutcome, RecoveryError> {
    let _owner = OperationOwnership::acquire(
        &lease.request.physical_path,
        filesystem.reconciliation_limits(),
    )?;
    let lock = existing_operation_lock(&journal_path(journal), lease, filesystem, false, true)?;
    let mut j = load_with_held_lock(&journal_path(journal), lease, filesystem, &lock)?;
    let catalog_root = Dir::open(filesystem.root(), filesystem.reconciliation_limits())?;
    let _catalog_access = fs::CatalogLock::acquire(
        &catalog_root,
        fs::CatalogLockMode::Shared,
        false,
        filesystem.io(),
        filesystem.reconciliation_limits(),
    )?;
    let (l, control) = attempt(&mut j, lease, filesystem, rollback_reserve)?;
    let (dest, candidate, original) = inspect_layout(&j, &l)?;
    let rolled_back = dest == binding_identity(&j.original)
        && dest.is_some()
        && original == Some(j.candidate_identity)
        && candidate.is_none();
    if rollback_reserve && !rolled_back {
        return Err(fs::refused(
            &l,
            "rollback-finalize reserve cannot admit selected backup generation",
        ));
    }
    let report = if rolled_back {
        j.original_verification
            .as_ref()
            .ok_or_else(|| fs::refused(&l, "forensic original cannot finalize"))?
    } else {
        if dest != Some(j.candidate_identity)
            || candidate.is_some()
            || original != binding_identity(&j.original)
        {
            return Err(l.verification());
        }
        verify_candidate_at(&j, &j.destination, lease, filesystem, &lock, &l)?;
        &j.verification
    };
    let bundle =
        super::catalog::select_completed_bundle(j.selected_bundle_operation, filesystem, &l)?;
    let manifest = super::catalog::retention_manifest(&bundle, &l)?;
    let readiness = super::readiness::assess_readiness(
        report,
        &super::manifest::DependencyManifest::from(&manifest),
        record,
    );
    if !readiness.ordinary_admission_ready {
        return Err(fs::refused(
            &l,
            "typed client, dependency and effect reconciliation required",
        ));
    }
    let now = filesystem.io().unix_ms().map_err(|_| fs::destination(&l))?;
    if record.recorded_at_unix_ms < lease.request.issued_at_unix_ms
        || record.recorded_at_unix_ms > now
        || record.operator != lease.request.operator
    {
        return Err(fs::refused(
            &l,
            "current operator reconciliation evidence required",
        ));
    }
    j.reconciliation = Some(record.clone());
    j.phase = if rolled_back {
        RestorePhase::FinalizeOriginalIntent
    } else {
        RestorePhase::FinalizeIntent
    };
    journal_append(&mut j, &control, &l)?;
    let parent = Dir::open(j.destination.parent().ok_or_else(|| l.verification())?, &l)?;
    let staging = Dir::open(&j.staging, &l)?;
    sync_both(&parent, &staging, &*control.io, &l)?;
    j.phase = if rolled_back {
        RestorePhase::OriginalFinalized
    } else {
        RestorePhase::Finalized
    };
    journal_append(&mut j, &control, &l)?;
    let guard = read_guard(lease, filesystem, &l)?;
    if guard.journal != journal_path(&j) {
        return Err(l.verification());
    }
    let path = admission::guard_path(&j.destination);
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| l.verification())?;
    let held = PrivateFile::open(&path, false, &l)?;
    fs::event(&*control.io, RecoveryEvent::RestoreGuardRemove, &l)?;
    held.unchanged(&l)?;
    parent.remove_file(name, held.identity(), &l)?;
    if parent
        .sync(&*control.io, Directory::RestoreGuardParent, &l)
        .is_err()
    {
        return Ok(RestoreOutcome::OutcomeUnknown {
            operation_id: j.operation_id,
        });
    }
    Ok(RestoreOutcome::Finalized {
        operation_id: j.operation_id,
        readiness,
    })
}

/// Negative-only handling for a changed original-absence layout. This cannot
/// adopt/verify a candidate or remove the guard, and is unavailable to renewal.
fn record_failed_original_absence(
    j: &mut RestoreJournal,
    lease: &OfflineLease,
    filesystem: &dyn RecoveryFs,
    lock: &RestoreOperationLock,
) -> Result<(), RecoveryError> {
    let mut l = filesystem.reconciliation_limits().clone();
    l.operation_id = j.operation_id;
    l.byte_limit = l.byte_limit.min(j.budget.read_byte_ceiling);
    l.memory_byte_limit = l.memory_byte_limit.min(j.budget.memory_ceiling);
    lease.validate_attestation(filesystem, &l)?;
    lock.validate(j, &l)?;
    if j.binding != lease.request
        || !matches!(j.original, FileBinding::Absent)
        || !matches!(
            j.phase,
            RestorePhase::OriginalAbsenceRestored | RestorePhase::OriginalAbsenceFinalized
        )
        || j.forensic_handling_required
        || j.rollback_attempts >= 4
    {
        return Err(fs::refused(
            &l,
            "original absence forensic handling requires unchanged initial attestation and available reserve",
        ));
    }
    let now = filesystem.io().unix_ms().map_err(|_| fs::destination(&l))?;
    let deadline = deadline_unix(filesystem, &l, lease)?;
    let remaining = deadline.checked_sub(now).ok_or_else(|| l.verification())?;
    l.deadline = l.deadline.min(
        std::time::Instant::now()
            + std::time::Duration::from_millis(
                u64::try_from(remaining).map_err(|_| l.verification())?,
            ),
    );
    let parent = Dir::open(j.destination.parent().ok_or_else(|| l.verification())?, &l)?;
    let unit = fs::allocation_geometry(&parent, &l)?;
    let control = Arc::new(PublicationIo::new(
        filesystem.io(),
        j.budget.allocation_ceiling,
        j.budget.metadata_bytes,
        j.verification.image_bytes,
        unit,
    ));
    l.publication_io = Some(control.clone());
    j.rollback_attempts += 1;
    j.current_attempt += 1;
    j.attempt_deadline_unix_ms = deadline;
    j.forensic_handling_required = true;
    journal_append(j, &control, &l)
}

pub(crate) fn finalize_absence(
    journal: &RestoreJournal,
    lease: &OfflineLease,
    record: &super::readiness::AbsenceReconciliationRecord,
    filesystem: &dyn RecoveryFs,
) -> Result<RestoreOutcome, RecoveryError> {
    let _owner = OperationOwnership::acquire(
        &lease.request.physical_path,
        filesystem.reconciliation_limits(),
    )?;
    let lock = existing_operation_lock(&journal_path(journal), lease, filesystem, false, true)?;
    let mut j = load_with_held_lock(&journal_path(journal), lease, filesystem, &lock)?;
    let catalog_root = Dir::open(filesystem.root(), filesystem.reconciliation_limits())?;
    let _catalog_access = fs::CatalogLock::acquire(
        &catalog_root,
        fs::CatalogLockMode::Shared,
        false,
        filesystem.io(),
        filesystem.reconciliation_limits(),
    )?;
    if j.binding == lease.request
        && matches!(j.original, FileBinding::Absent)
        && matches!(
            j.phase,
            RestorePhase::OriginalAbsenceRestored | RestorePhase::OriginalAbsenceFinalized
        )
        && inspect_phase_layout(&j, filesystem.reconciliation_limits()).is_err()
    {
        record_failed_original_absence(&mut j, lease, filesystem, &lock)?;
        return Err(fs::refused(
            filesystem.reconciliation_limits(),
            "original absence evidence failed; forensic_handling_required",
        ));
    }
    let (l, control) = attempt(&mut j, lease, filesystem, true)?;
    let now = filesystem.io().unix_ms().map_err(|_| fs::destination(&l))?;
    let valid = matches!(j.original, FileBinding::Absent)
        && matches!(
            j.phase,
            RestorePhase::OriginalAbsenceRestored | RestorePhase::OriginalAbsenceFinalized
        )
        && entry_identity(&j.destination, &l).is_ok_and(|id| id.is_none())
        && matches_binding(&j.staging.join("candidate.fls"), &candidate_binding(&j), &l).is_ok()
        && entry_identity(&j.staging.join("original.fls"), &l).is_ok_and(|id| id.is_none())
        && record.version == 1
        && record.operation_id == j.operation_id
        && record.candidate_sha256 == j.candidate_sha256
        && record.no_client_opened_path_while_installed
        && super::manifest::sha256(&record.evidence_sha256)
        && record.operator == lease.request.operator
        && record.recorded_at_unix_ms >= lease.request.issued_at_unix_ms
        && record.recorded_at_unix_ms <= now;
    if !valid {
        j.forensic_handling_required = true;
        journal_append(&mut j, &control, &l)?;
        return Err(fs::refused(
            &l,
            "original absence evidence failed; forensic_handling_required",
        ));
    }
    verify_candidate_at(
        &j,
        &j.staging.join("candidate.fls"),
        lease,
        filesystem,
        &lock,
        &l,
    )?;
    j.absence_reconciliation = Some(record.clone());
    j.phase = RestorePhase::OriginalAbsenceFinalized;
    journal_append(&mut j, &control, &l)?;
    let parent = Dir::open(j.destination.parent().ok_or_else(|| l.verification())?, &l)?;
    let staging = Dir::open(&j.staging, &l)?;
    sync_both(&parent, &staging, &*control.io, &l)?;
    let guard = read_guard(lease, filesystem, &l)?;
    if guard.journal != journal_path(&j) {
        return Err(l.verification());
    }
    let path = admission::guard_path(&j.destination);
    let held = PrivateFile::open(&path, false, &l)?;
    fs::event(&*control.io, RecoveryEvent::RestoreGuardRemove, &l)?;
    // Recheck absence after the last injectable boundary, before guard unlink.
    if entry_identity(&j.destination, &l)?.is_some() {
        j.forensic_handling_required = true;
        journal_append(&mut j, &control, &l)?;
        return Err(fs::refused(
            &l,
            "recreated destination; forensic_handling_required",
        ));
    }
    held.unchanged(&l)?;
    parent.remove_file(
        path.file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| l.verification())?,
        held.identity(),
        &l,
    )?;
    if parent
        .sync(&*control.io, Directory::RestoreGuardParent, &l)
        .is_err()
    {
        return Ok(RestoreOutcome::OutcomeUnknown {
            operation_id: j.operation_id,
        });
    }
    Ok(RestoreOutcome::OriginalAbsenceFinalized {
        operation_id: j.operation_id,
    })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestoreArtifact {
    pub operation_id: uuid::Uuid,
    pub path: PathBuf,
    pub identity: (u64, u64),
    pub bytes: u64,
    pub sha256: Option<String>,
    pub classification: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestoreStatus {
    pub operation_id: uuid::Uuid,
    pub phase: RestorePhase,
    pub guard_present: bool,
    pub current_attempt: u32,
    pub attempt_deadline_unix_ms: u128,
    pub time_remaining_ms: u128,
    pub resume_finalize_attempts: u32,
    pub rollback_attempts: u32,
    pub forensic_handling_required: bool,
    pub artifacts: Vec<RestoreArtifact>,
    pub total_retained_bytes: u64,
    pub staging_identity: (u64, u64),
    pub budget: RestoreBudget,
}
/// Read-only retained-artifact inventory. Missing or replaced journal-bound
/// generations refuse rather than adopting a same-named survivor.
pub fn restore_status(
    path: &Path,
    lease: &OfflineLease,
    filesystem: &dyn RecoveryFs,
) -> Result<RestoreStatus, RecoveryError> {
    let lock = existing_operation_lock(path, lease, filesystem, true, false)?;
    let j = load_journal(path, lease, filesystem, false)?;
    lock.validate(&j, filesystem.reconciliation_limits())?;
    let mut l = filesystem.reconciliation_limits().clone();
    l.operation_id = j.operation_id;
    let (dest, candidate, original) = inspect_layout(&j, &l)?;
    let mut artifacts = Vec::new();
    for (path, id) in [
        (j.destination.clone(), dest),
        (j.staging.join("candidate.fls"), candidate),
        (j.staging.join("original.fls"), original),
    ] {
        let Some(identity) = id else {
            continue;
        };
        let binding = if identity == j.candidate_identity {
            candidate_binding(&j)
        } else {
            j.original.clone()
        };
        let (bytes, sha256, classification) = match binding {
            FileBinding::Readable { bytes, sha256, .. } => (
                bytes,
                Some(sha256),
                if identity == j.candidate_identity {
                    "selected_verified_generation"
                } else if j.original_verification.is_some() {
                    "original_verified_generation"
                } else {
                    "original_forensic_unverified"
                },
            ),
            FileBinding::Forensic { bytes, .. } => {
                (bytes, None, "original_forensic_digest_unavailable")
            }
            FileBinding::Absent => return Err(l.verification()),
        };
        artifacts.push(RestoreArtifact {
            operation_id: j.operation_id,
            path,
            identity,
            bytes,
            sha256,
            classification: classification.into(),
        });
    }
    let guard_path = admission::guard_path(&j.destination);
    let guard_present = std::fs::symlink_metadata(&guard_path).is_ok();
    for path in [journal_path(&j), j.staging.join("journal.bin.lock")]
        .into_iter()
        .chain(guard_present.then_some(guard_path))
    {
        let held = PrivateFile::open(&path, false, &l)?;
        let (bytes, sha256) = verify::hash_file(&held.file, &l)?;
        held.unchanged(&l)?;
        artifacts.push(RestoreArtifact {
            operation_id: j.operation_id,
            path,
            identity: held.identity(),
            bytes,
            sha256: Some(sha256),
            classification: "restore_metadata".into(),
        });
    }
    let total_retained_bytes = artifacts.iter().try_fold(0u64, |n, a| {
        n.checked_add(a.bytes).ok_or_else(|| l.verification())
    })?;
    let now = filesystem.io().unix_ms().map_err(|_| fs::destination(&l))?;
    Ok(RestoreStatus {
        operation_id: j.operation_id,
        phase: j.phase,
        guard_present,
        current_attempt: j.current_attempt,
        attempt_deadline_unix_ms: j.attempt_deadline_unix_ms,
        time_remaining_ms: j.attempt_deadline_unix_ms.saturating_sub(now),
        resume_finalize_attempts: j.resume_finalize_attempts,
        rollback_attempts: j.rollback_attempts,
        forensic_handling_required: j.forensic_handling_required || j.rollback_attempts >= 4,
        artifacts,
        total_retained_bytes,
        staging_identity: j.staging_identity,
        budget: j.budget,
    })
}
