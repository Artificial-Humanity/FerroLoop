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
    pub original_profile: super::ValidationProfile,
    pub available_dependencies: Vec<super::readiness::DependencyResolution>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestorePhase {
    Admitted,
    Abandoned,
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
    pub selected_profile: super::ValidationProfile,
    pub selected_dependencies: super::manifest::DependencyManifest,
    pub selected_sequence: u64,
    pub selected_logical_sha256: String,
    pub generation_metadata_sha256: [u8; 32],
    pub terminal_evidence_context: Option<TerminalEvidenceContext>,
    pub original_dependencies: super::manifest::DependencyManifest,
    pub original_profile: super::ValidationProfile,
    pub candidate_bytes: u64,
    pub probe_identities: [(u64, u64); 2],
    pub candidate_identity: (u64, u64),
    pub candidate_sha256: String,
    pub staging_identity: (u64, u64),
    pub journal_identity: (u64, u64),
    pub operation_lock_identity: (u64, u64),
    pub journal_capacity: u64,
    pub selected_bundle_operation: uuid::Uuid,
    pub selected_bundle_sha256: String,
    pub binding: super::admission::OfflineLeaseRequest,
    pub verification: Option<super::VerificationReport>,
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
    #[serde(skip)]
    pub torn_tail_bytes: u64,
    #[serde(skip)]
    pub(crate) log_frames: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum RestoreOutcome {
    Abandoned {
        operation_id: uuid::Uuid,
    },
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
const GUARD_TEMP_NAME: &str = "restore-guard.tmp";

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
// Recheck after the final injectable digest read, immediately before mutation.
fn lease_time_remaining(
    expires_at_unix_ms: u128,
    io: &dyn fs::RecoveryIo,
    l: &OperationLimits,
) -> Result<std::time::Duration, RecoveryError> {
    l.check()?;
    let now = io.unix_ms().map_err(|_| fs::destination(l))?;
    l.check()?;
    let remaining = expires_at_unix_ms
        .checked_sub(now)
        .filter(|ms| *ms > 0)
        .ok_or(RecoveryError::Deadline {
            operation_id: l.operation_id,
        })?;
    Ok(std::time::Duration::from_millis(
        u64::try_from(remaining).map_err(|_| l.verification())?,
    ))
}
fn cap_lease_deadline(
    expires_at_unix_ms: u128,
    io: &dyn fs::RecoveryIo,
    l: &mut OperationLimits,
) -> Result<(), RecoveryError> {
    // Sampling before the injectable clock cannot extend authority by its latency.
    let started = std::time::Instant::now();
    let remaining = lease_time_remaining(expires_at_unix_ms, io, l)?;
    l.deadline = l.deadline.min(
        started
            .checked_add(remaining)
            .ok_or_else(|| l.verification())?,
    );
    l.check()
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
// No phase action may run until this record's complete write, file sync and
// journal-parent sync have all returned Ok. A torn trailing record authorizes
// no action; the last valid phase and every live identity are checked on retry.
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
    let scoped = journal_metadata_scope(j, l)?;
    let l = &scoped;
    let caps = super::restore_metadata::Caps::new(j.journal_capacity, l)?;
    let (payload_bytes, digest) =
        super::restore_metadata::measure_prefixed(&*j, caps.payload, &j.log_hash, l)?;
    let next_frame = caps.next_frame(j.log_frames, payload_bytes, l)?;
    let len = u32::try_from(payload_bytes).map_err(|_| quota(l))?;
    let end = j
        .log_end
        .checked_add(LOG_HEADER)
        .and_then(|n| n.checked_add(payload_bytes))
        .filter(|n| *n <= j.journal_capacity)
        .ok_or_else(|| quota(l))?;
    // All fallible bound/count/admission checks precede repair or header writes.
    l.admit_memory(65536 + LOG_HEADER)?;
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
    if j.torn_tail_bytes != 0 {
        let end = j
            .log_end
            .checked_add(j.torn_tail_bytes)
            .filter(|end| *end <= j.journal_capacity)
            .ok_or_else(|| l.verification())?;
        // Under the exclusive stable op lock, persist the erased payload before
        // erasing any header bytes. A crash must never expose a zero header with
        // a nonzero payload. Both barriers are repair-only; RJR1 stays unchanged.
        l.admit_memory(65536)?;
        let zeroes = [0; 65536];
        let header_end = j.log_end.saturating_add(LOG_HEADER).min(end);
        let mut cursor = end;
        while cursor > header_end {
            let start = cursor.saturating_sub(zeroes.len() as u64).max(header_end);
            control.write_all(
                &writable,
                &zeroes[..(cursor - start) as usize],
                start,
                Member::RestoreJournal,
                l,
            )?;
            cursor = start;
        }
        control.sync(&writable, Member::RestoreJournal, l)?;
        control.write_all(
            &writable,
            &zeroes[..(header_end - j.log_end) as usize],
            j.log_end,
            Member::RestoreJournal,
            l,
        )?;
        control.sync(&writable, Member::RestoreJournal, l)?;
        j.torn_tail_bytes = 0;
    }
    let mut header = [0; LOG_HEADER as usize];
    header[..4].copy_from_slice(b"RJR1");
    header[4..8].copy_from_slice(&len.to_le_bytes());
    header[8..40].copy_from_slice(&j.log_hash);
    header[40..72].copy_from_slice(&digest);
    control.write_all(&writable, &header, j.log_end, Member::RestoreJournal, l)?;
    let mut hash = Sha256::new();
    hash.update(j.log_hash);
    let mut stream = JournalStream {
        file: &writable,
        control,
        limits: l,
        start: j.log_end + LOG_HEADER,
        expected: payload_bytes,
        emitted: 0,
        flushed: 0,
        used: 0,
        scratch: [0; 65536],
        hash,
        failure: None,
    };
    serde_json::to_writer(&mut stream, &*j)
        .map_err(|_| stream.failure.take().unwrap_or_else(|| l.verification()))?;
    stream.drain()?;
    if stream.emitted != payload_bytes
        || stream.flushed != payload_bytes
        || <[u8; 32]>::from(stream.hash.finalize()) != digest
    {
        return Err(l.verification());
    }
    control.sync(&writable, Member::RestoreJournal, l)?;
    dir.sync(&*control.io, Directory::RestoreJournalParent, l)?;
    j.log_end = end;
    j.log_hash = digest;
    j.log_frames = next_frame;
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
    l: &mut OperationLimits,
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
    let (manifest, bundle_bytes, manifest_memory) = {
        let checked = super::catalog::retention_checked(&request.selected_bundle, l)?;
        (
            checked.manifest,
            checked.bytes,
            checked.manifest_memory_bytes,
        )
    };
    // The opaque retained files are streamed, not owned models. Only the
    // returned manifest survives the bundle check's local handle/model scope.
    *l = l.with_retained_memory(manifest_memory)?;
    let dir = Dir::open(request.selected_bundle.path(), l)?;
    let held = PrivateFile::open(&dir.path.join("manifest.json"), false, l)?;
    let digest = verify::hash_file(&held.file, l)?.1;
    let (expected, _) = fs::read_json_retained(&dir, "validation.json", l)?;
    let (profile, _) = fs::read_json_retained(&dir, "profile.json", l)?;
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
    let old = std::ffi::CString::new(old).map_err(|_| l.verification())?;
    let new = std::ffi::CString::new(new).map_err(|_| l.verification())?;
    validate()?;
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
        #[cfg(target_os = "linux")]
        let error = std::io::Error::last_os_error();
        #[cfg(not(target_os = "linux"))]
        let error = std::io::Error::from_raw_os_error(libc::ENOSYS);
        return Err(exchange_error(&error, l));
    }
    Ok(())
}
fn exchange_error(error: &std::io::Error, l: &OperationLimits) -> RecoveryError {
    if matches!(error.raw_os_error(), Some(libc::EINVAL | libc::ENOSYS)) {
        RecoveryError::Unsupported {
            operation_id: l.operation_id,
            reason: "native same-filesystem RENAME_EXCHANGE required; no copy fallback",
        }
    } else {
        fs::transient(error, l).unwrap_or_else(|| fs::destination(l))
    }
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
    lease: &OfflineLease,
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
                matches_binding(&j.destination, &j.original, l)?;
                lease_time_remaining(lease.request.expires_at_unix_ms, &*control.io, l)?;
                Ok(())
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
            || {
                matches_binding(&staging.path.join("candidate.fls"), &j.original, l)?;
                lease_time_remaining(lease.request.expires_at_unix_ms, &*control.io, l)?;
                Ok(())
            },
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
                )?;
                lease_time_remaining(lease.request.expires_at_unix_ms, &*control.io, l)?;
                Ok(())
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
    cap_lease_deadline(lease.request.expires_at_unix_ms, &*filesystem.io(), &mut l)?;
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
    let metadata_caps = super::restore_metadata::Caps::new(
        request
            .metadata_byte_limit
            .checked_sub(GUARD_ALLOWANCE)
            .ok_or_else(|| quota(&l))?,
        &l,
    )?;
    let scoped = metadata_caps.scope(&l)?;
    let mut caller_shape = super::restore_metadata::Shape::new(metadata_caps, &scoped);
    caller_shape.profile(&request.original_profile)?;
    caller_shape.resolutions(&request.available_dependencies)?;
    caller_shape.lease(&lease.request)?;
    caller_shape.path(&request.destination)?;
    caller_shape.path(&request.staging_parent)?;
    l = caller_shape.finish(3, &scoped)?;
    super::restore_metadata::measure(
        &(
            &request.original_profile,
            &request.available_dependencies,
            &lease.request,
        ),
        metadata_caps.payload,
        &l,
    )?;
    let catalog_root = Dir::open(filesystem.root(), &l)?;
    let _catalog_access = fs::CatalogLock::acquire(
        &catalog_root,
        fs::CatalogLockMode::Shared,
        false,
        filesystem.io(),
        &l,
    )?;
    let (manifest, bundle_digest, expected, profile, bundle_bytes) = selected(request, &mut l)?;
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
    let mut shape = super::restore_metadata::Shape::new(metadata_caps, &l);
    shape.profile(&profile)?;
    shape.dependencies(&manifest.reference_dependencies)?;
    // Selected profile/dependencies coexist with caller-owned original metadata.
    l = shape.finish(3, &l)?;
    super::restore_metadata::measure(
        &(
            &profile,
            &request.original_profile,
            &manifest.reference_dependencies,
            &lease.request,
        ),
        metadata_caps.payload,
        &l,
    )?;
    check_dependency_presence(&manifest, &request.available_dependencies, &l)?;
    if request.original_profile.catalog_identity != request.store.catalog_key {
        return Err(fs::refused(
            &l,
            "original validation profile must bind this store",
        ));
    }

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
        .checked_add(unit.checked_mul(4).ok_or_else(|| quota(&l))?)
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
            .checked_add(unit.checked_mul(4).ok_or_else(|| quota(&l))?)
            .ok_or_else(|| quota(&l))?,
        true,
        &l,
    )?;
    l.publication_io = Some(control.clone());
    let (original_report, original_read_error) = if matches!(original, FileBinding::Absent) {
        (None, None)
    } else {
        match original_verification(&physical, &request.original_profile, &l) {
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
    let capacity = request.metadata_byte_limit - GUARD_ALLOWANCE;
    let maximum_identity = (u64::MAX, u64::MAX);
    // Keep one owned journal throughout admission. Before touching the namespace,
    // maximum-width filesystem identities conservatively bound every real frame.
    let mut journal = RestoreJournal {
        version: 1,
        operation_id: request.operation_id,
        destination: physical.clone(),
        staging: staging_path.clone(),
        rollback: binding_identity(&original).map(|_| staging_path.join("original.fls")),
        original: original.clone(),
        original_verification: original_report.clone(),
        original_read_error: original_read_error.clone(),
        selected_profile: profile.clone(),
        selected_dependencies: super::manifest::DependencyManifest {
            references: manifest.reference_dependencies.clone(),
        },
        selected_sequence: manifest.published_sequence,
        selected_logical_sha256: manifest.logical_sha256.clone(),
        generation_metadata_sha256: [0; 32],
        terminal_evidence_context: None,
        original_dependencies: super::manifest::DependencyManifest {
            references: request.original_profile.dependencies.clone(),
        },
        original_profile: request.original_profile.clone(),
        candidate_bytes: manifest.image_bytes,
        probe_identities: [maximum_identity; 2],
        candidate_identity: maximum_identity,
        candidate_sha256: manifest.image_sha256.clone(),
        staging_identity: maximum_identity,
        journal_identity: maximum_identity,
        operation_lock_identity: maximum_identity,
        journal_capacity: capacity,
        selected_bundle_operation: uuid::Uuid::parse_str(&manifest.operation_id)
            .map_err(|_| l.verification())?,
        selected_bundle_sha256: bundle_digest.clone(),
        binding: lease.request.clone(),
        verification: None,
        budget: budget.clone(),
        phase: RestorePhase::Admitted,
        resume_finalize_attempts: 0,
        rollback_attempts: 0,
        current_attempt: 0,
        attempt_deadline_unix_ms: deadline_unix(filesystem, &l, lease)?,
        forensic_handling_required: false,
        reconciliation: None,
        absence_reconciliation: None,
        log_end: 0,
        log_hash: [0; 32],
        torn_tail_bytes: 0,
        log_frames: 0,
    };
    check_terminal_reservation(&journal, &l)?;
    drop(_catalog_access);
    let mut admission_staging_identity = None;
    let mut admission_lock_identity = None;
    let (lock, _catalog_access, mut journal) = (|| {
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
        let (_, rechecked_digest, _, _, _) = selected(request, &mut l.clone())?;
        if rechecked_digest != bundle_digest {
            return Err(l.verification());
        }
        lease.validate(filesystem, &l)?;
        let staging = &lock.staging;
        fs::event(&*control.io, RecoveryEvent::Create(Member::Image), &l)?;
        let candidate = PrivateFile::open(&staging.path.join("candidate.fls"), true, &l)?;
        fs::event(
            &*control.io,
            RecoveryEvent::Create(Member::RestoreProbe),
            &l,
        )?;
        let probe_a = PrivateFile::open(&staging.path.join("exchange-probe-a"), true, &l)?;
        fs::event(
            &*control.io,
            RecoveryEvent::Create(Member::RestoreProbe),
            &l,
        )?;
        let probe_b = PrivateFile::open(&staging.path.join("exchange-probe-b"), true, &l)?;
        fs::event(
            &*control.io,
            RecoveryEvent::Create(Member::RestoreJournal),
            &l,
        )?;
        let log = PrivateFile::open(&staging.path.join("journal.bin"), true, &l)?;
        control.allocate(&log.file, capacity, true, &l)?;
        journal.probe_identities = [probe_a.identity(), probe_b.identity()];
        journal.candidate_identity = candidate.identity();
        journal.staging_identity = fs::identity(&staging.file, &l)?;
        journal.journal_identity = log.identity();
        journal.operation_lock_identity = lock.held.identity();
        journal.attempt_deadline_unix_ms = deadline_unix(filesystem, &l, lease)?;
        journal.generation_metadata_sha256 = super::restore_metadata::measure(
            &generation_metadata(&journal),
            metadata_caps.payload,
            &l,
        )?
        .1;
        check_generation_metadata(&journal, &l)?;
        if journal.binding.build != super::manifest::BuildIdentity::current() {
            return Err(l.verification());
        }
        check_terminal_reservation(&journal, &l)?;
        journal_append(&mut journal, &control, &l)?;
        staging.sync(&*control.io, Directory::RestoreStagingParent, &l)?;
        parent.sync(&*control.io, Directory::RestoreDestinationParent, &l)?;
        rename_exchange(
            staging,
            "exchange-probe-a",
            staging,
            "exchange-probe-b",
            RecoveryEvent::RestoreExchangeProbe,
            &l,
            &*control.io,
            probe_a.identity(),
            probe_b.identity(),
            || Ok(()),
        )?;
        sync_both(&parent, staging, &*control.io, &l)?;
        // Probe files remain owned by this operation. Record their post-exchange names.
        // Their identities are an unordered pair: neither file is ever trusted as an image.
        write_restore_pin(&journal, filesystem, &control, &l)?;
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
        // Never create the admission name until the private guard is complete.
        // This reserved name lives inside the journal-bound operation staging;
        // an interrupted temp is retained through typed abandon for inspection.
        let expected_guard = fs::write_json(
            staging,
            GUARD_TEMP_NAME,
            &guard,
            Member::RestoreGuard,
            &control,
            &l,
        )?;
        let prepared = PrivateFile::open(&staging.path.join(GUARD_TEMP_NAME), false, &l)?;
        if prepared
            .file
            .metadata()
            .map_err(|_| l.verification())?
            .dev()
            != parent.file.metadata().map_err(|_| l.verification())?.dev()
        {
            return Err(fs::refused(
                &l,
                "same-filesystem guard publication required",
            ));
        }
        if verify::hash_file(&prepared.file, &l)? != expected_guard {
            return Err(l.verification());
        }
        prepared.unchanged(&l)?;
        staging.sync(&*control.io, Directory::RestoreStagingParent, &l)?;
        staging.rename_checked(
            GUARD_TEMP_NAME,
            &parent,
            &guard_name,
            &*control.io,
            RecoveryEvent::Publish,
            &l,
            || {
                lease.validate(filesystem, &l)?;
                lock.validate(&journal, &l)?;
                prepared.unchanged(&l)
            },
        )?;
        let published = PrivateFile::open(&parent.path.join(&guard_name), false, &l)?;
        // Rename changes ctime, so bind the new name by held inode and hash.
        if published.identity() != prepared.identity()
            || verify::hash_file(&published.file, &l)? != expected_guard
        {
            return Err(l.verification());
        }
        published.unchanged(&l)?;
        // Retain the authority observed before the injectable publication syncs.
        let publication_authority = PrivateFile::open(lease.path(), false, &l)?;
        // The new guard name and removed temporary name both need durability.
        parent
            .sync(&*control.io, Directory::RestoreGuardParent, &l)
            .and_then(|_| staging.sync(&*control.io, Directory::RestoreStagingParent, &l))
            .map_err(|_| RecoveryError::OutcomeUnknown {
                operation_id: l.operation_id,
            })?;
        lease.validate(filesystem, &l)?;
        // All injected reads finish before these raw held/name checks. A later
        // read must not let an earlier guard or authority attestation go stale.
        publication_authority.unchanged(&l)?;
        lock.validate(&journal, &l)?;
        parent.same_name(&l)?;
        published.unchanged(&l)?;
        Ok((lock, _catalog_access, journal))
    })()
    .map_err(|source| RecoveryError::RestoreAdmissionArtifactsRetained {
        operation_id: l.operation_id,
        fence_state: match std::fs::symlink_metadata(admission::guard_path(&physical)) {
            Ok(_) => super::RestoreAdmissionFenceState::Fenced,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                super::RestoreAdmissionFenceState::Unfenced
            }
            Err(_) => super::RestoreAdmissionFenceState::Uncertain,
        },
        staging: staging_path.clone(),
        staging_identity: admission_staging_identity,
        operation_lock_identity: admission_lock_identity,
        source: Box::new(source),
    })?;
    let result = (|| {
        let staging = &lock.staging;
        let image =
            PrivateFile::open(&request.selected_bundle.path().join("image.fls"), false, &l)?;
        let candidate = PrivateFile::open(&staging.path.join("candidate.fls"), false, &l)?;
        let candidate_writer =
            fs::writable_existing(&staging.path.join("candidate.fls"), &candidate, &l)?;
        if image.identity() == candidate.identity() {
            return Err(l.verification());
        }
        control.prepare_copy(&candidate_writer, manifest.image_bytes, &l)?;
        let mut offset = 0;
        let mut buffer = [0; 65536];
        while offset < manifest.image_bytes {
            let n = (manifest.image_bytes - offset).min(buffer.len() as u64) as usize;
            fs::read_exact_at(&image.file, &mut buffer[..n], offset, &l)?;
            control.write_copy(&candidate_writer, &buffer[..n], offset, &l)?;
            offset += n as u64;
        }
        control.sync(&candidate_writer, Member::Image, &l)?;
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
        if verification.verifier_build_identity != journal.binding.build
            || verification.image_sha256 != manifest.image_sha256
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
        journal.verification = Some(verification);
        journal.phase = RestorePhase::Prepared;
        journal_append(&mut journal, &control, &l)?;
        sync_both(&parent, staging, &*control.io, &l)?;
        install(&mut journal, lease, &control, &l)?;
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
    let j = load_journal(path, lease, filesystem, false)?;
    let mut limits = filesystem.reconciliation_limits().clone();
    limits.operation_id = j.operation_id;
    lock.validate(&j, &limits)?;
    Ok(j)
}
fn immutable_digest(j: &RestoreJournal, l: &OperationLimits) -> Result<String, RecoveryError> {
    let caps = super::restore_metadata::Caps::new(j.journal_capacity, l)?;
    let scoped = caps.scope(l)?;
    // A borrowed serialization view enumerates only immutable fields. No whole
    // journal clone, profile clone, accepted-context clone, or payload Vec.
    let view = (
        &j.version,
        generation_metadata(j),
        &j.rollback,
        &j.binding,
        &j.budget,
        &j.journal_capacity,
        &j.generation_metadata_sha256,
    );
    let (_, digest) = super::restore_metadata::measure(&view, caps.payload, &scoped)?;
    // Only this fixed64-byte diagnostic encoding allocates, after admission.
    scoped.admit_memory(64)?;
    use std::fmt::Write as _;
    let mut text = String::with_capacity(64);
    for byte in digest {
        write!(&mut text, "{byte:02x}").map_err(|_| l.verification())?;
    }
    Ok(text)
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
/// Load bounded, chained durable metadata without writing. A final failed frame
/// retains the last valid phase when its surviving bytes fit its authenticated
/// extent or one maximum frame; live phase layouts still govern later mutation.
pub fn load_restore_journal(
    path: &Path,
    lease: &OfflineLease,
    filesystem: &dyn RecoveryFs,
) -> Result<RestoreJournal, RecoveryError> {
    let lock = existing_operation_lock(path, lease, filesystem, true, false)?;
    load_with_held_lock(path, lease, filesystem, &lock)
}
// A failed record's length is untrusted. A later self-validating frame
// anywhere in the allocation (including its claimed payload) forbids repair.
// Candidate meaning, previous-chain linkage and JSON are deliberately irrelevant.
fn reject_later_complete_frame(
    file: &std::fs::File,
    offset: u64,
    capacity: u64,
    l: &OperationLimits,
    buffer: &mut [u8; 65536],
) -> Result<(), RecoveryError> {
    let Some(last_start) = capacity.checked_sub(LOG_HEADER) else {
        return Ok(());
    };
    let mut cursor = offset.checked_add(1).ok_or_else(|| l.verification())?;
    while cursor <= last_start {
        l.check()?;
        let n = (capacity - cursor).min(buffer.len() as u64) as usize;
        fs::read_exact_at(file, &mut buffer[..n], cursor, l)?;
        // Overlap three bytes so magic crossing a scan-window boundary is seen.
        let positions = (n - 3).min(usize::try_from(last_start - cursor + 1).unwrap_or(usize::MAX));
        for index in 0..positions {
            if &buffer[index..index + 4] != b"RJR1" {
                continue;
            }
            l.check()?;
            let at = cursor
                .checked_add(index as u64)
                .ok_or_else(|| l.verification())?;
            let mut header = [0; LOG_HEADER as usize];
            fs::read_exact_at(file, &mut header, at, l)?;
            l.check()?;
            let length =
                u32::from_le_bytes(header[4..8].try_into().map_err(|_| l.verification())?) as u64;
            let payload = at.checked_add(LOG_HEADER).ok_or_else(|| l.verification())?;
            let Some(end) = payload.checked_add(length).filter(|end| *end <= capacity) else {
                continue;
            };
            let mut hash = Sha256::new();
            hash.update(&header[8..40]);
            let mut position = payload;
            while position < end {
                l.check()?;
                let bytes = (end - position).min(buffer.len() as u64) as usize;
                fs::read_exact_at(file, &mut buffer[..bytes], position, l)?;
                l.check()?;
                hash.update(&buffer[..bytes]);
                position = position
                    .checked_add(bytes as u64)
                    .ok_or_else(|| l.verification())?;
            }
            let digest: [u8; 32] = hash.finalize().into();
            if header[40..72] == digest {
                return Err(l.verification());
            }
            // Hashing reused the scan buffer. Restore this exact window before
            // considering the next byte; candidates never advance the search.
            l.check()?;
            fs::read_exact_at(file, &mut buffer[..n], cursor, l)?;
        }
        cursor = cursor
            .checked_add(positions as u64)
            .ok_or_else(|| l.verification())?;
    }
    Ok(())
}
fn torn_tail_extent(
    file: &std::fs::File,
    offset: u64,
    capacity: u64,
    claimed_end: Option<u64>,
    l: &OperationLimits,
) -> Result<u64, RecoveryError> {
    l.admit_memory(65536)?;
    let mut buffer = [0; 65536];
    reject_later_complete_frame(file, offset, capacity, l, &mut buffer)?;
    let zero_suffix =
        claimed_end.unwrap_or_else(|| offset.saturating_add(LOG_HEADER).min(capacity));
    let mut cursor = offset;
    let mut nonzero_end = offset;
    while cursor < capacity {
        l.check()?;
        let n = (capacity - cursor).min(buffer.len() as u64) as usize;
        fs::read_exact_at(file, &mut buffer[..n], cursor, l)?;
        for (index, byte) in buffer[..n].iter().enumerate() {
            if *byte != 0 {
                let at = cursor + index as u64;
                if at >= zero_suffix {
                    return Err(l.verification());
                }
                nonzero_end = at + 1;
            }
        }
        cursor += n as u64;
    }
    nonzero_end
        .checked_sub(offset)
        .ok_or_else(|| l.verification())
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
    let metadata_caps = super::restore_metadata::Caps::new(capacity, &l)?;
    // Charge actual capacities already held by caller and guard, alongside the
    // bootstrap scratch, before inspecting the first journal payload. This same
    // scope is carried into min(current M,saved M), never reset per frame.
    let mut caller_shape = super::restore_metadata::Shape::new(metadata_caps, &l);
    caller_shape.lease(&lease.request)?;
    caller_shape.path(&staging.path)?;
    caller_shape.path(&guard_path)?;
    caller_shape.text(&prefix)?;
    if let Some(guard) = &guard {
        caller_shape.path(&guard.physical_path)?;
        caller_shape.path(&guard.journal)?;
        caller_shape.text(&guard.phase)?;
    }
    l = caller_shape.finish(1, &l)?;
    let mut frames = 0u64;
    let mut saved_memory = None;
    let mut offset = 0;
    let mut previous = [0; 32];
    let mut last: Option<RestoreJournal> = None;
    let mut immutable = None;
    let mut torn_tail_bytes = 0;
    while offset < capacity {
        let remaining = capacity - offset;
        if remaining < LOG_HEADER {
            torn_tail_bytes = torn_tail_extent(&log.file, offset, capacity, None, &l)?;
            if torn_tail_bytes != 0 && last.is_none() {
                return Err(l.verification());
            }
            break;
        }
        let max_frame_end = offset
            .checked_add(LOG_HEADER)
            .and_then(|n| n.checked_add(metadata_caps.payload))
            .ok_or_else(|| l.verification())?
            .min(capacity);
        let mut header = [0; LOG_HEADER as usize];
        fs::read_exact_at(&log.file, &mut header, offset, &l)?;
        if header.iter().all(|b| *b == 0) {
            // Header-page loss can leave durable payload from this final frame.
            // A prior authenticated frame is still required to recover state.
            torn_tail_bytes =
                torn_tail_extent(&log.file, offset, capacity, Some(max_frame_end), &l)?;
            if torn_tail_bytes != 0 && last.is_none() {
                return Err(l.verification());
            }
            break;
        }
        let length =
            u32::from_le_bytes(header[4..8].try_into().map_err(|_| l.verification())?) as u64;
        let end = offset
            .checked_add(LOG_HEADER)
            .and_then(|n| n.checked_add(length))
            .filter(|n| *n <= capacity);
        let header_valid = &header[..4] == b"RJR1" && header[8..40] == previous;
        let mut trusted_end = None;
        let parsed = (|| {
            if !header_valid {
                return Err(l.verification());
            }
            let end = end.ok_or_else(|| l.verification())?;
            // Hash and validate syntax using fixed stack scratch BEFORE cap/count
            // admission. A malformed final candidate follows the unchanged R3
            // scanner/zero-suffix path, without an owned decode or frame charge.
            let digest = frame_hash(&log.file, offset + LOG_HEADER, end, previous, &l)?;
            if header[40..72] != digest {
                return Err(l.verification());
            }
            trusted_end = Some(end);
            let memory = saved_memory_ceiling(&log.file, offset + LOG_HEADER, end, &l)?;
            let mut effective = l.clone();
            if let Some(memory) = memory {
                effective.memory_byte_limit = effective.memory_byte_limit.min(memory);
                effective
                    .admit_memory(JOURNAL_BOOTSTRAP_BYTES)
                    .map_err(journal_memory_refusal)?;
            }
            // Lead ruling: an authenticated, syntax-valid overcap frame is a hard
            // quota refusal, even if typed-invalid. The bounded writer cannot
            // produce one; foreign/tampered/buggy records must never be repaired
            // or unfenced as torn. Do not route this error through Verification.
            if length > metadata_caps.payload {
                effective.admit_memory(path.as_os_str().len() as u64)?;
                return Err(RecoveryError::RestoreJournalFrameQuota {
                    operation_id: effective.operation_id,
                    journal: path.to_path_buf(),
                    offset,
                    declared_bytes: length,
                    cap: metadata_caps.payload,
                });
            }
            let next = frames
                .checked_add(1)
                .ok_or_else(|| effective.verification())?;
            if next > super::restore_metadata::MAX_FRAMES {
                effective.admit_memory(path.as_os_str().len() as u64)?;
                return Err(RecoveryError::RestoreJournalFrameCountQuota {
                    operation_id: effective.operation_id,
                    journal: path.to_path_buf(),
                    offset,
                    frame_index: next,
                    cap: super::restore_metadata::MAX_FRAMES,
                    declared_bytes: length,
                });
            }
            let memory = memory.ok_or_else(|| effective.verification())?;
            let scoped = if saved_memory.is_none() {
                metadata_caps
                    .scope(&effective)
                    .map_err(journal_memory_refusal)?
            } else {
                effective
            };
            // caps reserves payload and both previous/current decoded models.
            #[cfg(test)]
            FRAME_OWNED_ALLOCATIONS.with(|count| count.set(count.get() + 1));
            let mut bytes = vec![0; usize::try_from(length).map_err(|_| scoped.verification())?];
            fs::read_exact_at(&log.file, &mut bytes, offset + LOG_HEADER, &scoped)?;
            // Rehash the bytes about to decode; never bind an independently read
            // bootstrap budget to changed frame data.
            let mut hash = Sha256::new();
            hash.update(previous);
            hash.update(&bytes);
            if <[u8; 32]>::from(hash.finalize()) != digest {
                return Err(scoped.verification());
            }
            super::restore_metadata::preflight_json(&bytes, metadata_caps, &scoped)?;
            let current: RestoreJournal =
                serde_json::from_slice(&bytes).map_err(|_| scoped.verification())?;
            if current.budget.memory_ceiling != memory {
                return Err(scoped.verification());
            }
            Ok((current, digest, end, scoped))
        })();
        let (mut current, digest, end, frame_limits) = match parsed {
            Ok(frame) => frame,
            Err(error @ RecoveryError::Verification { .. }) => {
                if last.is_none() {
                    return Err(error);
                }
                // Only chain and payload authentication establish the extent.
                // A torn header can leave any plausible, shorter length behind.
                let extent = trusted_end.or(Some(max_frame_end));
                torn_tail_bytes = torn_tail_extent(&log.file, offset, capacity, extent, &l)?;
                if torn_tail_bytes == 0 {
                    return Err(error);
                }
                break;
            }
            Err(error) => return Err(error),
        };
        // Count was preflighted before decode; advance only for accepted frames.
        l = frame_limits;
        frames = metadata_caps.next_frame(frames, length, &l)?;
        if saved_memory.is_some_and(|old| old != current.budget.memory_ceiling) {
            return Err(l.verification());
        }
        saved_memory = Some(current.budget.memory_ceiling);
        l.memory_byte_limit = l.memory_byte_limit.min(current.budget.memory_ceiling);
        // Keep the two decoded models, caller authority, guard and all later
        // digest/shape/readiness serializer work in one cumulative scope.
        let model_limits = l.clone();
        let l = &model_limits;
        if current.version != 1
            || current.operation_id != lease.request.operation_id
            || current.destination != lease.request.physical_path
            || current.staging != staging.path
            || current.staging_identity != fs::identity(&staging.file, l)?
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
        check_generation_metadata(&current, l)?;
        check_terminal_metadata(&current, last.as_ref(), l)?;
        if current.verification.is_none()
            && !matches!(
                current.phase,
                RestorePhase::Admitted | RestorePhase::Abandoned
            )
        {
            return Err(l.verification());
        }
        if let Some(old) = &last {
            if old.verification.is_some() && old.verification != current.verification {
                return Err(l.verification());
            }
            if old.verification.is_none()
                && current.verification.is_some()
                && !(old.phase == RestorePhase::Admitted && current.phase == RestorePhase::Prepared)
            {
                return Err(l.verification());
            }
        }
        if let Some(old) = &last
            && (current.resume_finalize_attempts < old.resume_finalize_attempts
                || current.rollback_attempts < old.rollback_attempts
                || current.current_attempt < old.current_attempt)
        {
            return Err(l.verification());
        }
        let fixed = immutable_digest(&current, l)?;
        if immutable
            .as_ref()
            .is_some_and(|expected| expected != &fixed)
        {
            return Err(l.verification());
        }
        immutable = Some(fixed);
        current.log_frames = frames;
        current.log_end = end;
        current.log_hash = digest;
        offset = end;
        previous = digest;
        last = Some(current);
    }
    log.unchanged(&l)?;
    let mut last = last.ok_or_else(|| l.verification())?;
    last.torn_tail_bytes = torn_tail_bytes;
    if guard.is_none()
        && !matches!(
            last.phase,
            RestorePhase::Admitted
                | RestorePhase::Abandoned
                | RestorePhase::Finalized
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
    attempt_scoped(j, lease, filesystem, rollback, None)
}
fn attempt_scoped(
    j: &mut RestoreJournal,
    lease: &OfflineLease,
    filesystem: &dyn RecoveryFs,
    rollback: bool,
    retained: Option<&OperationLimits>,
) -> Result<(OperationLimits, Arc<PublicationIo>), RecoveryError> {
    let mut l = retained
        .unwrap_or_else(|| filesystem.reconciliation_limits())
        .clone();
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
        j.candidate_bytes,
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
        bytes: j.candidate_bytes,
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
            if j.verification.is_none()
                && matches!(j.phase, RestorePhase::Admitted | RestorePhase::Abandoned)
            {
                let partial = PrivateFile::open(&path, false, l)?;
                if partial.identity() != j.candidate_identity
                    || partial.file.metadata().map_err(|_| l.verification())?.len()
                        > j.candidate_bytes
                {
                    return Err(l.verification());
                }
                partial.unchanged(l)?;
                continue;
            }
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
    if !phase_layout_matches(j, layout) {
        return Err(l.verification());
    }
    Ok(())
}
fn phase_layout_matches(j: &RestoreJournal, layout: RestoreLayout) -> bool {
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
    match j.phase {
        RestorePhase::Admitted | RestorePhase::Abandoned => layout == prepared,
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
    }
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
    let (manifest, manifest_memory) = {
        let checked = super::catalog::retention_checked(&bundle, l)?;
        (checked.manifest, checked.manifest_memory_bytes)
    };
    let mut models = l.with_retained_memory(manifest_memory)?;
    let l = &models;
    if verify::hash_file(
        &PrivateFile::open(&dir.path.join("manifest.json"), false, l)?.file,
        l,
    )?
    .1 != j.selected_bundle_sha256
    {
        return Err(l.verification());
    }
    let (expected, _): (super::ValidationExpectations, _) =
        fs::read_json_retained(&dir, "validation.json", &mut models)?;
    let (profile, _) = fs::read_json_retained(&dir, "profile.json", &mut models)?;
    let l = &models;
    let access = admission::RestoreAccess::obtain(j, lease, filesystem, lock, l)?;
    let report = verify::verify_restore_copy(path, &expected, &profile, l, &access)?;
    if report.image_sha256 != j.candidate_sha256
        || report.logical_sha256
            != j.verification
                .as_ref()
                .ok_or_else(|| l.verification())?
                .logical_sha256
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
    let required_action = match j.phase {
        RestorePhase::Prepared
        | RestorePhase::ExchangeIntent
        | RestorePhase::RelocateIntent
        | RestorePhase::Installed
        | RestorePhase::RolledBack
        | RestorePhase::OriginalAbsenceRestored => None,
        RestorePhase::ReverseIntent => Some(
            "restore resume refused: use typed rollback_restore to complete the rollback already started",
        ),
        RestorePhase::Admitted | RestorePhase::Abandoned => Some(
            "restore resume refused: use typed abandon_restore for the unprepared restore or its retained cleanup",
        ),
        RestorePhase::FinalizeIntent | RestorePhase::Finalized => Some(
            "restore resume refused: use typed finalize_restore with the accepted reconciliation",
        ),
        RestorePhase::FinalizeOriginalIntent | RestorePhase::OriginalFinalized => Some(
            "restore resume refused: use typed finalize_rollback with the accepted reconciliation",
        ),
        RestorePhase::OriginalAbsenceFinalized => {
            Some("restore resume refused: use typed finalize_original_absence for retained cleanup")
        }
    };
    if let Some(required_action) = required_action {
        return Err(fs::refused(
            filesystem.reconciliation_limits(),
            required_action,
        ));
    }
    if j.phase == RestorePhase::OriginalAbsenceRestored {
        // This rollback is complete. Reinstalling its retained candidate requires
        // a new operation, even though its layout also looks prepared.
        continuation_limits(&j, lease, filesystem, &lock)?;
        return Ok(RestoreOutcome::OriginalAbsenceRestored { journal: j });
    }
    // Reject unavailable or corrupt selected evidence before spending a resume
    // attempt. A reversed generation needs only its preserved journal evidence.
    let precheck = continuation_limits(&j, lease, filesystem, &lock)?;
    let (dest, candidate, original) = inspect_layout(&j, &precheck)?;
    let forward_candidate = if dest == binding_identity(&j.original)
        && candidate == Some(j.candidate_identity)
        && original.is_none()
    {
        Some(j.staging.join("candidate.fls"))
    } else if dest == Some(j.candidate_identity)
        && ((candidate == binding_identity(&j.original)
            && original.is_none()
            && candidate.is_some())
            || (candidate.is_none() && original == binding_identity(&j.original)))
    {
        Some(j.destination.clone())
    } else {
        None
    };
    if let Some(path) = forward_candidate {
        if matches!(
            j.phase,
            RestorePhase::ExchangeIntent | RestorePhase::RelocateIntent
        ) && !matches!(j.original, FileBinding::Absent)
            && j.original_verification.is_none()
            && path == j.destination
        {
            // The forward exchange already installed the verified image. Its
            // durable preservation evidence permits completing relocation even
            // without the bundle, when the original cannot support rollback.
            verify_preserved_candidate(&j, &path, &precheck)?;
        } else {
            verify_candidate_at(&j, &path, lease, filesystem, &lock, &precheck)?;
        }
    }
    // Completed continuations retain all authority, layout and evidence checks,
    // but must not consume the budget needed for typed finalization.
    match j.phase {
        RestorePhase::Installed => return Ok(RestoreOutcome::Installed { journal: j }),
        RestorePhase::RolledBack => {
            if j.original_verification.is_none() {
                return Err(fs::refused(
                    &precheck,
                    "forensic original is not verified rollback",
                ));
            }
            return Ok(RestoreOutcome::RolledBack { journal: j });
        }
        _ => {}
    }
    let (l, control) = attempt(&mut j, lease, filesystem, false)?;
    let (dest, candidate, original) = inspect_layout(&j, &l)?;
    let parent = Dir::open(j.destination.parent().ok_or_else(|| l.verification())?, &l)?;
    let staging = Dir::open(&j.staging, &l)?;
    if dest == binding_identity(&j.original)
        && candidate == Some(j.candidate_identity)
        && original.is_none()
    {
        install(&mut j, lease, &control, &l)?;
    } else if dest == Some(j.candidate_identity)
        && candidate == binding_identity(&j.original)
        && original.is_none()
        && candidate.is_some()
    {
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
            || {
                matches_binding(&staging.path.join("candidate.fls"), &j.original, &l)?;
                lease_time_remaining(lease.request.expires_at_unix_ms, &*control.io, &l)?;
                Ok(())
            },
        )?;
        sync_both(&parent, &staging, &*control.io, &l)?;
        j.phase = RestorePhase::Installed;
        journal_append(&mut j, &control, &l)?;
    } else if dest == Some(j.candidate_identity)
        && candidate.is_none()
        && original == binding_identity(&j.original)
    {
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
    if !matches!(
        j.phase,
        RestorePhase::ExchangeIntent
            | RestorePhase::RelocateIntent
            | RestorePhase::Installed
            | RestorePhase::ReverseIntent
            | RestorePhase::RolledBack
            | RestorePhase::OriginalAbsenceRestored
    ) {
        return Err(fs::refused(
            filesystem.reconciliation_limits(),
            match j.phase {
                RestorePhase::Admitted | RestorePhase::Prepared | RestorePhase::Abandoned => {
                    "rollback unavailable in this phase; use abandon_restore"
                }
                RestorePhase::FinalizeIntent | RestorePhase::Finalized => {
                    "rollback unavailable in this phase; use finalize_restore with the accepted reconciliation"
                }
                RestorePhase::FinalizeOriginalIntent | RestorePhase::OriginalFinalized => {
                    "rollback unavailable in this phase; use finalize_rollback with the accepted reconciliation"
                }
                RestorePhase::OriginalAbsenceFinalized => {
                    "rollback unavailable in this phase; use finalize_original_absence"
                }
                _ => "rollback requires installed or rollback-continuation phase",
            },
        ));
    }
    let precheck = continuation_limits(&j, lease, filesystem, &lock)?;
    if !matches!(j.original, FileBinding::Absent) && j.original_verification.is_none() {
        return Err(fs::refused(
            &precheck,
            "verified original required; forensic material retained",
        ));
    }
    let (dest, candidate, original) = inspect_layout(&j, &precheck)?;
    if j.phase == RestorePhase::ExchangeIntent && dest != Some(j.candidate_identity) {
        return Err(fs::refused(
            &precheck,
            "rollback requires the completed forward exchange; use abandon_restore before exchange",
        ));
    }
    let candidate_path = if dest == Some(j.candidate_identity) {
        j.destination.clone()
    } else if candidate == Some(j.candidate_identity) {
        j.staging.join("candidate.fls")
    } else if original == Some(j.candidate_identity) {
        j.staging.join("original.fls")
    } else {
        return Err(precheck.verification());
    };
    verify_preserved_candidate(&j, &candidate_path, &precheck)?;
    match j.phase {
        RestorePhase::RolledBack => return Ok(RestoreOutcome::RolledBack { journal: j }),
        RestorePhase::OriginalAbsenceRestored => {
            return Ok(RestoreOutcome::OriginalAbsenceRestored { journal: j });
        }
        _ => {}
    }
    let (l, control) = attempt(&mut j, lease, filesystem, true)?;
    if matches!(j.original, FileBinding::Absent) {
        let (dest, candidate, original) = inspect_layout(&j, &l)?;
        let parent = Dir::open(j.destination.parent().ok_or_else(|| l.verification())?, &l)?;
        let staging = Dir::open(&j.staging, &l)?;
        if original.is_some() {
            return Err(l.verification());
        }
        if dest == Some(j.candidate_identity) && candidate.is_none() {
            verify_preserved_candidate(&j, &j.destination, &l)?;
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
                || {
                    matches_binding(&j.destination, &candidate_binding(&j), &l)?;
                    lease_time_remaining(lease.request.expires_at_unix_ms, &*control.io, &l)?;
                    Ok(())
                },
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
    if dest == Some(j.candidate_identity)
        && candidate == Some(original_identity)
        && original.is_none()
    {
        // ExchangeIntent cannot describe the normalized layout. Persist the
        // relocation intent first so either side of the rename remains loadable.
        // Together with the attempt, reverse intent and terminal this uses the
        // existing four-frame rollback schedule.
        if j.phase == RestorePhase::ExchangeIntent {
            j.phase = RestorePhase::RelocateIntent;
            journal_append(&mut j, &control, &l)?;
        }
        staging.rename_checked(
            "candidate.fls",
            &staging,
            "original.fls",
            &*control.io,
            RecoveryEvent::RestoreRelocate,
            &l,
            || {
                matches_binding(&staging.path.join("candidate.fls"), &j.original, &l)?;
                lease_time_remaining(lease.request.expires_at_unix_ms, &*control.io, &l)?;
                Ok(())
            },
        )?;
        sync_both(&parent, &staging, &*control.io, &l)?;
    }
    let (dest, candidate, original) = inspect_layout(&j, &l)?;
    if dest != Some(j.candidate_identity)
        || candidate.is_some()
        || original != Some(original_identity)
    {
        return Err(l.verification());
    }
    verify_preserved_candidate(&j, &j.destination, &l)?;
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
            matches_binding(&j.destination, &candidate_binding(&j), &l)?;
            lease_time_remaining(lease.request.expires_at_unix_ms, &*control.io, &l)?;
            Ok(())
        },
    )?;
    sync_both(&parent, &staging, &*control.io, &l)?;
    matches_binding(&j.destination, &j.original, &l)?;
    j.phase = RestorePhase::RolledBack;
    journal_append(&mut j, &control, &l)?;
    Ok(RestoreOutcome::RolledBack { journal: j })
}
// Runtime authority is separate from the immutable accepting-lease evidence.
fn continuation_limits(
    j: &RestoreJournal,
    lease: &OfflineLease,
    filesystem: &dyn RecoveryFs,
    lock: &RestoreOperationLock,
) -> Result<OperationLimits, RecoveryError> {
    let mut l = filesystem.reconciliation_limits().clone();
    l.operation_id = j.operation_id;
    l.byte_limit = l.byte_limit.min(j.budget.read_byte_ceiling);
    l.memory_byte_limit = l.memory_byte_limit.min(j.budget.memory_ceiling);
    let mut l = journal_metadata_scope(j, &l)?;
    let caps = super::restore_metadata::Caps::new(j.journal_capacity, &l)?;
    let mut shape = super::restore_metadata::Shape::new(caps, &l);
    shape.lease(&lease.request)?;
    l = shape.finish(1, &l)?;
    lease.validate_attestation(filesystem, &l)?;
    lock.validate(j, &l)?;
    if !lease_binding_matches(&j.binding, &lease.request)
        || (j.forensic_handling_required && !is_terminal(&j.phase))
    {
        return Err(l.verification());
    }
    // An unchanged admission lease binds the original. Renewals bind the current
    // generation; never demand that installed bytes equal admission's original.
    if j.binding != lease.request {
        lease.validate(filesystem, &l)?;
    }
    inspect_phase_layout(j, &l)?;
    let now = filesystem.io().unix_ms().map_err(|_| fs::destination(&l))?;
    let deadline = deadline_unix(filesystem, &l, lease)?;
    let remaining = deadline.checked_sub(now).ok_or_else(|| l.verification())?;
    l.deadline = l.deadline.min(
        std::time::Instant::now()
            + std::time::Duration::from_millis(
                u64::try_from(remaining).map_err(|_| l.verification())?,
            ),
    );
    Ok(l)
}

fn terminal_control(
    j: &RestoreJournal,
    filesystem: &dyn RecoveryFs,
    l: &OperationLimits,
) -> Result<(OperationLimits, Arc<PublicationIo>), RecoveryError> {
    let parent = Dir::open(j.destination.parent().ok_or_else(|| l.verification())?, l)?;
    let unit = fs::allocation_geometry(&parent, l)?;
    let control = Arc::new(PublicationIo::new(
        filesystem.io(),
        j.budget.allocation_ceiling,
        j.budget.metadata_bytes,
        j.candidate_bytes,
        unit,
    ));
    let mut limits = l.clone();
    limits.publication_io = Some(control.clone());
    Ok((limits, control))
}

// A frame must have room for every future scalar/phase and one accepted
// context/evidence pair before the admission name can be published. The saved
// capacity is the only byte budget; current process build text is not an input.
// Encoded JSON string bytes, including quotes and escapes. At the default
// 1MiB metadata budget P=10,168; these leave 1,613 evidence bytes in the
// dependency-bearing unit fixture (maximum-operator mandatory record: 1,205).
// 126 plain operator bytes and 254 plain attestation bytes accommodate shift
// changes and a ticket-backed exclusion statement; escapes consume that room.
const ACCEPTING_OPERATOR_ENCODED_MAX: u64 = 128;
const ACCEPTING_EXCLUSION_EVIDENCE_ENCODED_MAX: u64 = 256;
const TERMINAL_CONTEXT_ENVELOPE_MAX: u64 = 768; // Current schema maximum: 732.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TerminalCaps {
    operator: u64,
    lease: u64,
    evidence: u64,
}
fn replace_encoded<A: Serialize, B: Serialize>(
    bytes: u64,
    old: &A,
    new: &B,
    cap: u64,
    l: &OperationLimits,
) -> Result<u64, RecoveryError> {
    use super::restore_metadata::measure;
    let old_bytes = measure(old, cap, l)?.0;
    let new_bytes = measure(new, cap, l)?.0;
    bytes
        .checked_sub(old_bytes)
        .and_then(|n| n.checked_add(new_bytes))
        .ok_or_else(|| l.verification())
}

// Borrow the real build, including every available/unavailable string. This
// maximum template deliberately permits an impossible copy/source combination:
// every valid report is smaller. Schema additions must update this audit.
#[derive(Serialize)]
struct MaximumCopyEvidence {
    verified_file_identity: (u64, u64),
    source_image_identity: Option<(u64, u64)>,
    creation: super::manifest::CopyCreation,
    image_sha256: &'static str,
}
#[derive(Serialize)]
struct MaximumVerificationReport<'a> {
    report_version: u32,
    copy_evidence: Option<MaximumCopyEvidence>,
    operation_id: &'static str,
    image_sha256: &'static str,
    image_bytes: u64,
    published_sequence: u64,
    logical_sha256: &'static str,
    profile_version: u32,
    profile_sha256: &'static str,
    canonical_encoding_version: u32,
    intrinsic_lineage_available: bool,
    external_publish_status: crate::ferrostore::ExternalPublishStatus,
    verified_collection_count: u64,
    unknown_collection_count: u64,
    opaque_runtime_references: bool,
    query_probe_count: u64,
    exception_count: u64,
    dependency_count: u64,
    transient_rpc_dedupe_persisted: bool,
    durable_rpc_receipts_claimed: bool,
    readiness: super::manifest::Readiness,
    verifier_build_identity: &'a super::manifest::BuildIdentity,
}
const RESERVATION_SHA: &str = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
fn maximum_report(build: &super::manifest::BuildIdentity) -> MaximumVerificationReport<'_> {
    MaximumVerificationReport {
        report_version: u32::MAX,
        copy_evidence: Some(MaximumCopyEvidence {
            verified_file_identity: (u64::MAX, u64::MAX),
            source_image_identity: Some((u64::MAX, u64::MAX)),
            creation: super::manifest::CopyCreation::CallerSuppliedExisting,
            image_sha256: RESERVATION_SHA,
        }),
        operation_id: "ffffffff-ffff-ffff-ffff-ffffffffffff",
        image_sha256: RESERVATION_SHA,
        image_bytes: u64::MAX,
        published_sequence: u64::MAX,
        logical_sha256: RESERVATION_SHA,
        profile_version: u32::MAX,
        profile_sha256: RESERVATION_SHA,
        canonical_encoding_version: u32::MAX,
        intrinsic_lineage_available: false,
        external_publish_status:
            crate::ferrostore::ExternalPublishStatus::ExclusionUnprovenBeforeSingleOwnership,
        verified_collection_count: u64::MAX,
        unknown_collection_count: u64::MAX,
        opaque_runtime_references: false,
        query_probe_count: u64::MAX,
        exception_count: u64::MAX,
        dependency_count: u64::MAX,
        transient_rpc_dedupe_persisted: false,
        durable_rpc_receipts_claimed: false,
        readiness: super::manifest::Readiness::DependenciesRequireReconciliation,
        verifier_build_identity: build,
    }
}
fn terminal_caps(j: &RestoreJournal, l: &OperationLimits) -> Result<TerminalCaps, RecoveryError> {
    use super::restore_metadata::Caps;
    let p = Caps::new(j.journal_capacity, l)?.payload;
    let (reserved, operator, lease) = terminal_reservation_base(j, p, l)?;
    let evidence = p.checked_sub(reserved).ok_or_else(|| {
        fs::refused(
            l,
            "immutable restore metadata leaves no terminal reservation",
        )
    })?;
    Ok(TerminalCaps {
        operator,
        lease,
        evidence,
    })
}
fn terminal_reservation_base(
    j: &RestoreJournal,
    p: u64,
    l: &OperationLimits,
) -> Result<(u64, u64, u64), RecoveryError> {
    use super::restore_metadata::measure;
    // Exact field substitutions preserve the serializer's existing field names,
    // punctuation and immutable fields, without owning a normalized journal.
    let mut b = measure(j, p, l)?.0;
    b = replace_encoded(b, &j.verification, &None::<()>, p, l)?;
    b = replace_encoded(b, &j.terminal_evidence_context, &None::<()>, p, l)?;
    b = replace_encoded(b, &j.reconciliation, &None::<()>, p, l)?;
    b = replace_encoded(b, &j.absence_reconciliation, &None::<()>, p, l)?;
    b = replace_encoded(b, &j.phase, &RestorePhase::OriginalAbsenceFinalized, p, l)?;
    for counter in [
        j.resume_finalize_attempts,
        j.rollback_attempts,
        j.current_attempt,
    ] {
        b = replace_encoded(b, &counter, &u32::MAX, p, l)?;
    }
    b = replace_encoded(b, &j.attempt_deadline_unix_ms, &u128::MAX, p, l)?;
    b = replace_encoded(b, &j.forensic_handling_required, &false, p, l)?;
    b = replace_encoded(b, &j.generation_metadata_sha256, &[255u8; 32], p, l)?;
    let r = measure(&maximum_report(&j.binding.build), p, l)?.0;
    let operator = ACCEPTING_OPERATOR_ENCODED_MAX;
    let binding = &j.binding;
    let admission_exclusion = measure(&binding.exclusion.evidence, p, l)?.0;
    let mut lease = measure(binding, p, l)?.0;
    lease = lease
        .checked_sub(measure(&binding.operator, p, l)?.0)
        .and_then(|n| n.checked_add(ACCEPTING_OPERATOR_ENCODED_MAX))
        .and_then(|n| n.checked_sub(admission_exclusion))
        .and_then(|n| n.checked_add(ACCEPTING_EXCLUSION_EVIDENCE_ENCODED_MAX))
        .ok_or_else(|| l.verification())?;
    lease = replace_encoded(lease, &binding.issued_at_unix_ms, &u128::MAX, p, l)?;
    lease = replace_encoded(lease, &binding.expires_at_unix_ms, &u128::MAX, p, l)?;
    // Use the same tagged representation as FileBinding without allocating its
    // SHA string. Readable's maximal representation exceeds Forensic and Absent.
    #[derive(Serialize)]
    #[serde(tag = "state", rename_all = "snake_case")]
    enum MaximumFile {
        Readable {
            identity: (u64, u64),
            bytes: u64,
            sha256: &'static str,
        },
    }
    lease = replace_encoded(
        lease,
        &binding.expected_file,
        &MaximumFile::Readable {
            identity: (u64::MAX, u64::MAX),
            bytes: u64::MAX,
            sha256: RESERVATION_SHA,
        },
        p,
        l,
    )?;
    for flag in [
        binding.exclusion.legacy_and_cached_handles_excluded,
        binding.exclusion.external_dispatch_excluded,
        binding.exclusion.active_owner_excluded,
    ] {
        lease = replace_encoded(lease, &flag, &false, p, l)?;
    }
    let reserved = b
        .checked_add(r)
        .and_then(|n| n.checked_add(TERMINAL_CONTEXT_ENVELOPE_MAX))
        .and_then(|n| n.checked_add(lease))
        .ok_or_else(|| l.verification())?;
    Ok((reserved, operator, lease))
}

#[derive(Serialize)]
struct CanonicalResolution<'a> {
    reference_sha256: &'a str,
    catalog_identity: &'a str,
    observed_sequence: u64,
    evidence_sha256: &'static str,
}
struct CanonicalDependencies<'a>(&'a [super::profiles::ReferenceDependency]);
impl Serialize for CanonicalDependencies<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut seq = serializer.serialize_seq(Some(self.0.len()))?;
        for d in self.0 {
            seq.serialize_element(&CanonicalResolution {
                reference_sha256: &d.reference_sha256,
                catalog_identity: &d.catalog_identity,
                observed_sequence: d.required_sequence.unwrap_or(u64::MAX),
                evidence_sha256: RESERVATION_SHA,
            })?;
        }
        seq.end()
    }
}
#[derive(Serialize)]
struct CanonicalReconciliation<'a> {
    version: u32,
    operation_id: uuid::Uuid,
    image_sha256: &'a str,
    published_sequence: u64,
    operator: &'a str,
    recorded_at_unix_ms: u128,
    affected_scopes: [&'a str; 1],
    identity_confirmed: bool,
    reference_closure_confirmed: bool,
    external_effect_coverage_confirmed: bool,
    source_publication_resolution_sha256: &'static str,
    clients: CanonicalClients,
    dependencies: CanonicalDependencies<'a>,
    external_outcomes: [(); 0],
    unresolved_items: [(); 0],
    index: super::readiness::IndexReadiness,
}
#[derive(Serialize)]
struct CanonicalClients {
    clients_invalidated: bool,
    rpc_sessions_restarted: bool,
    transactions_drained: bool,
    caches_invalidated: bool,
    retry_assumptions_reconciled: bool,
    evidence_sha256: &'static str,
}
fn check_terminal_reservation(
    j: &RestoreJournal,
    l: &OperationLimits,
) -> Result<(), RecoveryError> {
    let maximum_payload =
        super::restore_metadata::Caps::new((512 << 20) - GUARD_ALLOWANCE, l)?.payload;
    match check_terminal_reservation_bounded(j, maximum_payload, l) {
        Err(RecoveryError::Quota {
            resource: "restore metadata bytes",
            ..
        }) => Err(terminal_reservation_quota(j, maximum_payload + 1, l)),
        result => result,
    }
}
fn terminal_reservation_quota(
    j: &RestoreJournal,
    required_payload: u64,
    l: &OperationLimits,
) -> RecoveryError {
    let Some(required_bytes) = required_payload
        .checked_add(super::restore_metadata::FRAME_HEADER)
        .and_then(|n| n.checked_mul(super::restore_metadata::MAX_FRAMES))
        .and_then(|n| n.checked_add(GUARD_ALLOWANCE))
    else {
        return l.verification();
    };
    RecoveryError::RestoreMetadataReservationQuota {
        operation_id: l.operation_id,
        offered_bytes: j.budget.metadata_payload_ceiling,
        required_bytes,
        action: if required_bytes > 512 << 20 {
            "required reservation exceeds the supported 536870912-byte maximum; increasing metadata_byte_limit cannot admit this restore"
        } else {
            // A larger limit may widen its own serialized budget scalars.
            "increase metadata_byte_limit with headroom for encoded budget widths, within the supported 536870912-byte maximum"
        },
    }
}
fn check_terminal_reservation_bounded(
    j: &RestoreJournal,
    p: u64,
    l: &OperationLimits,
) -> Result<(), RecoveryError> {
    use super::{
        manifest::Availability,
        readiness::*,
        restore_metadata::{Caps, measure},
    };
    let offered_payload = Caps::new(j.journal_capacity, l)?.payload;
    let (reserved, _, _) = terminal_reservation_base(j, p, l)?;
    let mut required_payload = reserved;
    for original in [false, true] {
        if original && j.original_verification.is_none() {
            continue;
        }
        let dependencies = if original {
            &j.original_dependencies
        } else {
            &j.selected_dependencies
        };
        // This is a NEW-admission refusal only. Legacy graph validation and
        // typed exits deliberately do not call this helper.
        for (i, d) in dependencies.references.iter().enumerate() {
            l.check()?;
            if dependencies.references[..i].iter().any(|old| {
                old.reference_sha256 == d.reference_sha256
                    && old.catalog_identity == d.catalog_identity
            }) {
                return Err(fs::refused(l, "duplicate terminal dependency identity"));
            }
        }
        let (sha, sequence, profile) = if original {
            let report = j
                .original_verification
                .as_ref()
                .ok_or_else(|| l.verification())?;
            (
                report.image_sha256.as_str(),
                report.published_sequence,
                &j.original_profile,
            )
        } else {
            (
                j.candidate_sha256.as_str(),
                j.selected_sequence,
                &j.selected_profile,
            )
        };
        let scope = if profile.catalog_identity.trim().is_empty() {
            j.destination.to_str().ok_or_else(|| l.verification())?
        } else {
            profile.catalog_identity.as_str()
        };
        let template = CanonicalReconciliation {
            version: 1,
            operation_id: j.operation_id,
            image_sha256: sha,
            published_sequence: sequence,
            operator: &j.binding.operator,
            recorded_at_unix_ms: u128::MAX,
            affected_scopes: [scope],
            identity_confirmed: true,
            reference_closure_confirmed: true,
            external_effect_coverage_confirmed: true,
            source_publication_resolution_sha256: RESERVATION_SHA,
            clients: CanonicalClients {
                clients_invalidated: true,
                rpc_sessions_restarted: true,
                transactions_drained: true,
                caches_invalidated: true,
                retry_assumptions_reconciled: true,
                evidence_sha256: RESERVATION_SHA,
            },
            dependencies: CanonicalDependencies(&dependencies.references),
            external_outcomes: [],
            unresolved_items: [],
            index: IndexReadiness::Degraded,
        };
        // The future operator also occurs in reconciliation, independently of
        // its occurrence in the accepting lease. Reserve both at their maxima.
        let template_bytes = measure(&template, p, l)?.0;
        let q = template_bytes
            .checked_sub(measure(&j.binding.operator, p, l)?.0)
            .and_then(|n| n.checked_add(ACCEPTING_OPERATOR_ENCODED_MAX))
            .ok_or_else(|| l.verification())?;
        required_payload =
            required_payload.max(reserved.checked_add(q).ok_or_else(|| l.verification())?);
        // The borrowed template has no heap allocation. Admit its owned model,
        // the selected synthetic report and readiness scratch BEFORE allocating
        // any of them, one role at a time. Immutable loaded metadata stays borrowed.
        // The readiness witness still borrows admission values; a historical
        // admission operator may exceed the new accepting allowance.
        let scratch = q
            .max(template_bytes)
            .checked_mul(64)
            .and_then(|n| n.checked_add(8192))
            .and_then(|n| {
                n.checked_add(
                    (dependencies.references.len() as u64)
                        .checked_mul(std::mem::size_of::<&DependencyResolution>() as u64)?,
                )
            })
            .ok_or_else(|| l.verification())?;
        let scoped = l.with_retained_memory(scratch)?;
        let record = ReconciliationRecord {
            version: 1,
            operation_id: j.operation_id,
            image_sha256: sha.into(),
            published_sequence: sequence,
            operator: j.binding.operator.clone(),
            recorded_at_unix_ms: u128::MAX,
            affected_scopes: vec![scope.into()],
            identity_confirmed: true,
            reference_closure_confirmed: true,
            external_effect_coverage_confirmed: true,
            source_publication_resolution_sha256: RESERVATION_SHA.into(),
            clients: ClientReconciliation {
                clients_invalidated: true,
                rpc_sessions_restarted: true,
                transactions_drained: true,
                caches_invalidated: true,
                retry_assumptions_reconciled: true,
                evidence_sha256: RESERVATION_SHA.into(),
            },
            dependencies: dependencies
                .references
                .iter()
                .map(|d| DependencyResolution {
                    reference_sha256: d.reference_sha256.clone(),
                    catalog_identity: d.catalog_identity.clone(),
                    observed_sequence: d.required_sequence.unwrap_or(u64::MAX),
                    evidence_sha256: RESERVATION_SHA.into(),
                })
                .collect(),
            external_outcomes: vec![],
            unresolved_items: vec![],
            index: IndexReadiness::Degraded,
        };
        // assess_readiness reads only identity, sequence and dependency count
        // from the report; these are the actual immutable selected generation.
        let selected_report;
        let report = if original {
            j.original_verification
                .as_ref()
                .ok_or_else(|| l.verification())?
        } else {
            selected_report = super::VerificationReport {
                report_version: 1,
                copy_evidence: None,
                operation_id: j.operation_id.to_string(),
                image_sha256: sha.into(),
                image_bytes: j.candidate_bytes,
                published_sequence: sequence,
                logical_sha256: j.selected_logical_sha256.clone(),
                profile_version: profile.version,
                profile_sha256: j.binding.profile_sha256.clone(),
                canonical_encoding_version: 1,
                intrinsic_lineage_available: false,
                external_publish_status:
                    crate::ferrostore::ExternalPublishStatus::ExclusionUnprovenBeforeSingleOwnership,
                verified_collection_count: 0,
                unknown_collection_count: 0,
                opaque_runtime_references: false,
                query_probe_count: 0,
                exception_count: 0,
                dependency_count: dependencies.references.len() as u64,
                transient_rpc_dedupe_persisted: false,
                durable_rpc_receipts_claimed: false,
                readiness: super::manifest::Readiness::VerifiedLocalContents,
                verifier_build_identity: super::manifest::BuildIdentity {
                    package_version: String::new(),
                    source_revision: Availability::Unavailable,
                    executable_sha256: Availability::Unavailable,
                },
            };
            &selected_report
        };
        scoped.check()?;
        if !assess_readiness(report, dependencies, &record).ordinary_admission_ready {
            return Err(fs::refused(
                &scoped,
                "mandatory terminal reconciliation cannot be satisfied",
            ));
        }
    }
    if matches!(j.original, FileBinding::Absent) {
        #[derive(Serialize)]
        struct Absence<'a> {
            version: u32,
            operation_id: uuid::Uuid,
            candidate_sha256: &'a str,
            operator: &'a str,
            recorded_at_unix_ms: u128,
            no_client_opened_path_while_installed: bool,
            evidence_sha256: &'static str,
        }
        let absence = Absence {
            version: 1,
            operation_id: j.operation_id,
            candidate_sha256: &j.candidate_sha256,
            operator: &j.binding.operator,
            recorded_at_unix_ms: u128::MAX,
            no_client_opened_path_while_installed: true,
            evidence_sha256: RESERVATION_SHA,
        };
        let q = measure(&absence, p, l)?
            .0
            .checked_sub(measure(&j.binding.operator, p, l)?.0)
            .and_then(|n| n.checked_add(ACCEPTING_OPERATOR_ENCODED_MAX))
            .ok_or_else(|| l.verification())?;
        required_payload =
            required_payload.max(reserved.checked_add(q).ok_or_else(|| l.verification())?);
    }
    if required_payload > offered_payload {
        return Err(terminal_reservation_quota(j, required_payload, l));
    }
    Ok(())
}

// Shape admission has already bounded the borrowed string capacity. JSON
// encodes each input byte in at most six output bytes plus the two quotes.
// This finite counting-only bound permits an exact scalar diagnostic; full
// lease/evidence measurements remain under the existing frame payload bound.
fn accepting_string_bytes(value: &str, l: &OperationLimits) -> Result<u64, RecoveryError> {
    let encoded_bound = (value.len() as u64)
        .checked_mul(6)
        .and_then(|n| n.checked_add(2))
        .ok_or_else(|| l.verification())?;
    Ok(super::restore_metadata::measure(value, encoded_bound, l)?.0)
}

fn terminal_input_quota(
    l: &OperationLimits,
    offered_bytes: u64,
    limit_bytes: u64,
    resource: &'static str,
) -> Result<(), RecoveryError> {
    l.check()?;
    if offered_bytes > limit_bytes {
        return Err(RecoveryError::RestoreTerminalInputQuota {
            operation_id: l.operation_id,
            resource,
            offered_bytes,
            limit_bytes,
        });
    }
    Ok(())
}

// Count borrowed inputs against the immutable reservation BEFORE any
// evidence/context clone or durable attempt. Shape charges caller capacities and
// all simultaneously live clones, including readiness results.
fn terminal_input_scope(
    j: &RestoreJournal,
    lease: &OfflineLease,
    record: Option<&super::readiness::ReconciliationRecord>,
    absence: Option<&super::readiness::AbsenceReconciliationRecord>,
    l: &OperationLimits,
) -> Result<OperationLimits, RecoveryError> {
    use super::restore_metadata as metadata;
    let caps = metadata::Caps::new(j.journal_capacity, l)?;
    let mut shape = metadata::Shape::new(caps, l);
    shape.lease(&lease.request)?;
    if let Some(record) = record {
        shape.reconciliation(record)?;
        shape.readiness_scratch(record.dependencies.len())?;
    }
    if let Some(record) = absence {
        shape.absence(record)?;
    }
    let scoped = shape.finish(2, l)?;
    let reserved = terminal_caps(j, &scoped)?;
    // Scalar checks report exact escaping/quote costs. Full lease/evidence
    // above P can still hit the tighter existing metadata-byte refusal first.
    terminal_input_quota(
        l,
        accepting_string_bytes(&lease.request.operator, &scoped)?,
        reserved.operator,
        "restore accepting operator",
    )?;
    terminal_input_quota(
        l,
        accepting_string_bytes(&lease.request.exclusion.evidence, &scoped)?,
        ACCEPTING_EXCLUSION_EVIDENCE_ENCODED_MAX,
        "restore accepting lease exclusion evidence",
    )?;
    terminal_input_quota(
        l,
        metadata::measure(&lease.request, caps.payload, &scoped)?.0,
        reserved.lease,
        "restore accepting lease",
    )?;
    if let Some(record) = record {
        terminal_input_quota(
            l,
            metadata::measure(record, caps.payload, &scoped)?.0,
            reserved.evidence,
            "restore accepting evidence",
        )?;
    }
    if let Some(record) = absence {
        terminal_input_quota(
            l,
            metadata::measure(record, caps.payload, &scoped)?.0,
            reserved.evidence,
            "restore accepting evidence",
        )?;
    }
    Ok(scoped)
}

fn accepting_context(
    j: &RestoreJournal,
    lease: &OfflineLease,
    mode: TerminalMode,
    record: Option<&super::readiness::ReconciliationRecord>,
    absence: Option<&super::readiness::AbsenceReconciliationRecord>,
    filesystem: &dyn RecoveryFs,
    l: &OperationLimits,
) -> Result<TerminalEvidenceContext, RecoveryError> {
    use super::restore_metadata::{Caps, measure};
    let cap = Caps::new(j.journal_capacity, l)?.payload;
    let accepted_at = filesystem.io().unix_ms().map_err(|_| fs::destination(l))?;
    if accepted_at < lease.request.issued_at_unix_ms
        || accepted_at >= lease.request.expires_at_unix_ms
    {
        return Err(l.verification());
    }
    Ok(TerminalEvidenceContext {
        mode,
        accepting_lease: lease.request.clone(),
        accepting_lease_sha256: measure(&lease.request, cap, l)?.1,
        admission_generation_sha256: j.generation_metadata_sha256,
        prepared_report_sha256: j
            .verification
            .as_ref()
            .map(|report| measure(report, cap, l).map(|m| m.1))
            .transpose()?,
        reconciliation_sha256: if let Some(record) = record {
            Some(measure(record, cap, l)?.1)
        } else if let Some(record) = absence {
            Some(measure(record, cap, l)?.1)
        } else {
            None
        },
        accepted_at_unix_ms: accepted_at,
    })
}

fn verify_preserved_candidate(
    j: &RestoreJournal,
    path: &Path,
    l: &OperationLimits,
) -> Result<(), RecoveryError> {
    // The admission report proves this exact immutable image; rollback does not
    // need a surviving completed bundle, receipt, or logical re-verification.
    check_generation_metadata(j, l)?;
    let report = j.verification.as_ref().ok_or_else(|| l.verification())?;
    if report
        .copy_evidence
        .as_ref()
        .is_none_or(|copy| copy.verified_file_identity != j.candidate_identity)
    {
        return Err(l.verification());
    }
    matches_binding(path, &candidate_binding(j), l)
}

fn is_terminal(phase: &RestorePhase) -> bool {
    matches!(
        phase,
        RestorePhase::Finalized
            | RestorePhase::OriginalFinalized
            | RestorePhase::OriginalAbsenceFinalized
            | RestorePhase::Abandoned
    )
}
fn read_held_guard(
    held: &PrivateFile,
    l: &OperationLimits,
) -> Result<(RestoreGuardV1, OperationLimits), RecoveryError> {
    let len = held.file.metadata().map_err(|_| l.verification())?.len();
    l.quota(len, GUARD_ALLOWANCE, "restore guard metadata")?;
    l.quota(len, l.byte_limit, "metadata bytes")?;
    let decoded = len.checked_mul(32).ok_or_else(|| quota(l))?;
    let scoped = l.with_retained_memory(decoded.checked_add(65536).ok_or_else(|| quota(l))?)?;
    let mut bytes = vec![0; usize::try_from(len).map_err(|_| l.verification())?];
    for (index, chunk) in bytes.chunks_mut(65536).enumerate() {
        fs::read_exact_at(&held.file, chunk, index as u64 * 65536, &scoped)?;
    }
    let guard = serde_json::from_slice(&bytes).map_err(|_| l.verification())?;
    held.unchanged(&scoped)?;
    Ok((guard, scoped))
}
fn guard_matches_journal(
    guard: &RestoreGuardV1,
    j: &RestoreJournal,
    l: &OperationLimits,
) -> Result<(), RecoveryError> {
    if guard.version != 1
        || guard.operation_id != j.operation_id
        || guard.physical_path != j.destination
        || guard.parent_identity != j.binding.parent_identity
        || guard.journal != journal_path(j)
        || guard.phase != "restore_pending"
    {
        return Err(l.verification());
    }
    Ok(())
}
fn pin_matches_journal(
    pin: &RestorePin,
    j: &RestoreJournal,
    l: &OperationLimits,
) -> Result<(), RecoveryError> {
    if pin.version != 1
        || pin.restore_operation != j.operation_id
        || pin.bundle_operation != j.selected_bundle_operation
        || pin.bundle_sha256 != j.selected_bundle_sha256
        || pin.destination != j.destination
        || pin.journal != journal_path(j)
        || pin.journal_identity != j.journal_identity
        || pin.operation_lock_identity != j.operation_lock_identity
    {
        return Err(l.verification());
    }
    Ok(())
}

// Retain the exact checked generations through all injectable authority reads,
// removal events and syncs. The fixed three-member array bounds descriptor and
// metadata growth; the scope includes their paths, lease metadata and hash scratch.
struct TerminalFileWitness {
    file: std::fs::File,
    initial: std::fs::Metadata,
}
impl TerminalFileWitness {
    fn unchanged(&self, path: &Path, l: &OperationLimits) -> Result<(), RecoveryError> {
        let now = self.file.metadata().map_err(|_| l.verification())?;
        if entry_identity(path, l)? != Some((self.initial.dev(), self.initial.ino()))
            || now.dev() != self.initial.dev()
            || now.ino() != self.initial.ino()
            || now.len() != self.initial.len()
            || now.mode() != self.initial.mode()
            || now.nlink() != 1
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
struct TerminalWitness {
    authority: PrivateFile,
    files: [Option<TerminalFileWitness>; 3],
    limits: OperationLimits,
}
fn terminal_witness(
    j: &RestoreJournal,
    lease: &OfflineLease,
    l: &OperationLimits,
) -> Result<TerminalWitness, RecoveryError> {
    let path_bytes = j
        .destination
        .as_os_str()
        .len()
        .checked_add(
            j.staging
                .as_os_str()
                .len()
                .checked_mul(2)
                .ok_or_else(|| quota(l))?,
        )
        .and_then(|n| n.checked_add(lease.path().as_os_str().len()))
        .ok_or_else(|| quota(l))?;
    let retained = u64::try_from(path_bytes)
        .map_err(|_| quota(l))?
        .checked_mul(8)
        .and_then(|n| n.checked_add(16384 + 65536))
        .ok_or_else(|| quota(l))?;
    let scoped = l.with_retained_memory(retained)?;
    let authority = PrivateFile::open(lease.path(), false, &scoped)?;
    // Bound this descriptor's length under the existing lease read ceiling,
    // including if it grows after open. Later typed authority validation is
    // independently checked against this held snapshot.
    verify::hash_file(&authority.file, &scoped)?;
    authority.unchanged(&scoped)?;
    let layout = inspect_layout(j, &scoped)?;
    if !phase_layout_matches(j, layout) {
        return Err(scoped.verification());
    }
    let mut files: [Option<TerminalFileWitness>; 3] = [None, None, None];
    for (slot, (path, identity)) in files.iter_mut().zip([
        (j.destination.clone(), layout.0),
        (j.staging.join("candidate.fls"), layout.1),
        (j.staging.join("original.fls"), layout.2),
    ]) {
        let Some(identity) = identity else {
            continue;
        };
        let binding = if identity == j.candidate_identity {
            candidate_binding(j)
        } else {
            j.original.clone()
        };
        let file = if matches!(binding, FileBinding::Forensic { .. }) {
            // Keep an unreadable original's O_PATH descriptor without changing
            // its original forensic classification or granting a readable proof.
            if admission::inspect_file_binding(&path, &scoped)? != binding {
                return Err(scoped.verification());
            }
            let (parent, name) = verify::walk_parent(&path, false, &scoped)?;
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
                return Err(scoped.verification());
            }
            unsafe { std::fs::File::from_raw_fd(fd) }
        } else {
            let held = PrivateFile::open(&path, false, &scoped)?;
            if held.identity() != identity {
                return Err(scoped.verification());
            }
            if identity == j.candidate_identity
                && j.verification.is_none()
                && j.phase == RestorePhase::Abandoned
            {
                if held
                    .file
                    .metadata()
                    .map_err(|_| scoped.verification())?
                    .len()
                    > j.candidate_bytes
                {
                    return Err(scoped.verification());
                }
                // Partial bytes have no accepted complete digest, but still
                // retain a checked snapshot before the final authority read.
                verify::hash_file(&held.file, &scoped)?;
            } else {
                let (bytes, sha256) = verify::hash_file(&held.file, &scoped)?;
                if binding
                    != (FileBinding::Readable {
                        identity,
                        bytes,
                        sha256,
                    })
                {
                    return Err(scoped.verification());
                }
            }
            held.unchanged(&scoped)?;
            held.file
        };
        let initial = file.metadata().map_err(|_| scoped.verification())?;
        if (initial.dev(), initial.ino()) != identity {
            return Err(scoped.verification());
        }
        *slot = Some(TerminalFileWitness { file, initial });
    }
    let witness = TerminalWitness {
        authority,
        files,
        limits: scoped,
    };
    witness.unchanged(j, false)?;
    Ok(witness)
}
impl TerminalWitness {
    // Only raw opens/stat/name checks occur here. In particular no injected
    // OpenFile, digest read or JSON decode can follow the final authority read.
    fn unchanged(&self, j: &RestoreJournal, guard_absent: bool) -> Result<(), RecoveryError> {
        let l = &self.limits;
        self.authority.unchanged(l)?;
        for (held, path) in self.files.iter().zip([
            j.destination.clone(),
            j.staging.join("candidate.fls"),
            j.staging.join("original.fls"),
        ]) {
            if let Some(held) = held {
                held.unchanged(&path, l)?;
            } else if entry_identity(&path, l)?.is_some() {
                return Err(l.verification());
            }
        }
        if guard_absent && entry_identity(&admission::guard_path(&j.destination), l)?.is_some() {
            return Err(l.verification());
        }
        l.check()
    }
}

// A failed read is not evidence of a changed generation. Only successfully
// observed names/identities or successfully hashed bytes establish a mismatch.
fn original_absence_layout_changed(
    j: &RestoreJournal,
    l: &OperationLimits,
) -> Result<bool, RecoveryError> {
    if entry_identity(&j.destination, l)?.is_some()
        || entry_identity(&j.staging.join("original.fls"), l)?.is_some()
        || entry_identity(&j.staging.join("candidate.fls"), l)? != Some(j.candidate_identity)
    {
        return Ok(true);
    }
    let held = PrivateFile::open(&j.staging.join("candidate.fls"), false, l)?;
    if held.identity() != j.candidate_identity {
        return Ok(true);
    }
    let (bytes, sha256) = verify::hash_file(&held.file, l)?;
    held.unchanged(l)?;
    Ok(bytes != j.candidate_bytes || sha256 != j.candidate_sha256)
}

fn final_terminal_authority(
    j: &RestoreJournal,
    lease: &OfflineLease,
    filesystem: &dyn RecoveryFs,
    lock: &RestoreOperationLock,
    l: &OperationLimits,
) -> Result<(), RecoveryError> {
    // This read may inject OpenFile; callers perform their final held/name and
    // absence checks AFTER it. No accepting-evidence time is compared to B.
    lease.validate_attestation(filesystem, l)?;
    lock.validate(j, l)?;
    let now = filesystem.io().unix_ms().map_err(|_| fs::destination(l))?;
    if now < lease.request.issued_at_unix_ms || now >= lease.request.expires_at_unix_ms {
        return Err(l.verification());
    }
    l.check()
}

// Requires a durable terminal record. It performs no journal append and charges
// no attempts. Only guard and exact operation pin can be removed.
fn terminal_cleanup(
    j: &RestoreJournal,
    lease: &OfflineLease,
    filesystem: &dyn RecoveryFs,
    lock: &RestoreOperationLock,
    control: &Arc<PublicationIo>,
    l: &OperationLimits,
) -> Result<(), RecoveryError> {
    if !is_terminal(&j.phase) {
        return Err(l.verification());
    }
    check_terminal_metadata(j, Some(j), l)?;
    let guard_path = admission::guard_path(&j.destination);
    let guard_identity = entry_identity(&guard_path, l)?;
    let witness = terminal_witness(j, lease, l).map_err(|error| {
        if guard_identity.is_none() {
            RecoveryError::OutcomeUnknown {
                operation_id: j.operation_id,
            }
        } else {
            error
        }
    })?;
    let l = &witness.limits;
    let parent = Dir::open(j.destination.parent().ok_or_else(|| l.verification())?, l)?;
    let name = guard_path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| l.verification())?;
    if let Some(expected) = guard_identity {
        parent.remove_file_checked(name, expected, l, |held| {
            let (guard, scoped) = read_held_guard(held, l)?;
            guard_matches_journal(&guard, j, &scoped)?;
            fs::event(&*control.io, RecoveryEvent::RestoreGuardRemove, &scoped)?;
            final_terminal_authority(j, lease, filesystem, lock, &scoped)?;
            held.unchanged(&scoped)?;
            witness.unchanged(j, false)
        })?;
    }
    let finish = (|| {
        // This includes the absent-guard branch: authority/layout observations
        // survive all later reads and all sync hooks, even absent-pin retries.
        parent.sync(&*control.io, Directory::RestoreGuardParent, l)?;
        final_terminal_authority(j, lease, filesystem, lock, l)?;
        witness.unchanged(j, true)?;
        let catalog = Dir::open(&filesystem.root().join(".catalog"), l)?;
        let pins = catalog.child("restore-pins", false, l)?;
        let pin_name = format!("{}.json", j.operation_id);
        let temp_name = format!("{pin_name}.tmp");
        if let Some(expected) = entry_identity(&pins.path.join(&pin_name), l)? {
            pins.remove_file_checked(&pin_name, expected, l, |held| {
                let (pin, scoped) = read_restore_pin(held, l)?;
                pin_matches_journal(&pin, j, &scoped)?;
                fs::event(&*control.io, RecoveryEvent::RestorePinRemove, &scoped)?;
                final_terminal_authority(j, lease, filesystem, lock, &scoped)?;
                held.unchanged(&scoped)?;
                witness.unchanged(j, true)
            })?;
        }
        // The journal-bound operation owns this exact temporary name under
        // its operation lock. Do not decode: torn writes must remain cleanable.
        if let Some(expected) = entry_identity(&pins.path.join(&temp_name), l)? {
            pins.remove_file_checked(&temp_name, expected, l, |held| {
                fs::event(&*control.io, RecoveryEvent::RestorePinRemove, l)?;
                final_terminal_authority(j, lease, filesystem, lock, l)?;
                held.unchanged(l)?;
                witness.unchanged(j, true)
            })?;
        }
        pins.sync(&*control.io, Directory::RestorePinParent, l)?;
        final_terminal_authority(j, lease, filesystem, lock, l)?;
        witness.unchanged(j, true)?;
        if entry_identity(&pins.path.join(&pin_name), l)?.is_some()
            || entry_identity(&pins.path.join(&temp_name), l)?.is_some()
        {
            return Err(l.verification());
        }
        catalog.sync(&*control.io, Directory::CatalogParent, l)?;
        final_terminal_authority(j, lease, filesystem, lock, l)?;
        witness.unchanged(j, true)?;
        if entry_identity(&pins.path.join(&pin_name), l)?.is_some()
            || entry_identity(&pins.path.join(&temp_name), l)?.is_some()
        {
            return Err(l.verification());
        }
        Ok(())
    })();
    if finish.is_err() {
        return Err(RecoveryError::OutcomeUnknown {
            operation_id: j.operation_id,
        });
    }
    finish
}
fn cleanup_outcome(
    result: Result<(), RecoveryError>,
    outcome: RestoreOutcome,
) -> Result<RestoreOutcome, RecoveryError> {
    match result {
        Ok(()) => Ok(outcome),
        Err(RecoveryError::OutcomeUnknown { operation_id }) => {
            Ok(RestoreOutcome::OutcomeUnknown { operation_id })
        }
        Err(error) => Err(error),
    }
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
    let lock = existing_operation_lock(&journal_path(journal), lease, filesystem, false, false)?;
    let mut j = load_with_held_lock(&journal_path(journal), lease, filesystem, &lock)?;
    let catalog_root = Dir::open(filesystem.root(), filesystem.reconciliation_limits())?;
    let _catalog_access = fs::CatalogLock::acquire(
        &catalog_root,
        fs::CatalogLockMode::Shared,
        false,
        filesystem.io(),
        filesystem.reconciliation_limits(),
    )?;
    let (initial, intent, terminal, mode) = if rollback_reserve {
        (
            RestorePhase::RolledBack,
            RestorePhase::FinalizeOriginalIntent,
            RestorePhase::OriginalFinalized,
            TerminalMode::Original,
        )
    } else {
        (
            RestorePhase::Installed,
            RestorePhase::FinalizeIntent,
            RestorePhase::Finalized,
            TerminalMode::Selected,
        )
    };
    if j.phase != initial && j.phase != intent && j.phase != terminal {
        return Err(fs::refused(
            filesystem.reconciliation_limits(),
            "finalization requires its completed generation-specific phase",
        ));
    }
    let base = continuation_limits(&j, lease, filesystem, &lock)?;
    if j.phase == terminal {
        // No attempt, caller evidence validation/clone, bundle lookup or append.
        check_terminal_metadata(&j, Some(&j), &base)?;
        let stored = j
            .reconciliation
            .as_ref()
            .ok_or_else(|| base.verification())?;
        let (report, dependencies) = if rollback_reserve {
            (j.original_verification.as_ref(), &j.original_dependencies)
        } else {
            (j.verification.as_ref(), &j.selected_dependencies)
        };
        let readiness = super::readiness::assess_readiness(
            report.ok_or_else(|| base.verification())?,
            dependencies,
            stored,
        );
        let (l, control) = terminal_control(&j, filesystem, &base)?;
        return cleanup_outcome(
            terminal_cleanup(&j, lease, filesystem, &lock, &control, &l),
            RestoreOutcome::Finalized {
                operation_id: j.operation_id,
                readiness,
            },
        );
    }
    let first = j.phase == initial;
    let scoped = if first {
        terminal_input_scope(&j, lease, Some(record), None, &base)?
    } else {
        let caps = super::restore_metadata::Caps::new(j.journal_capacity, &base)?;
        let mut shape = super::restore_metadata::Shape::new(caps, &base);
        shape.reconciliation(record)?;
        shape.readiness_scratch(record.dependencies.len())?;
        shape.finish(1, &base)?
    };
    let (report, dependencies) = if rollback_reserve {
        (
            j.original_verification
                .as_ref()
                .ok_or_else(|| fs::refused(&scoped, "forensic original cannot finalize"))?,
            &j.original_dependencies,
        )
    } else {
        (
            j.verification
                .as_ref()
                .ok_or_else(|| scoped.verification())?,
            &j.selected_dependencies,
        )
    };
    let accepted = if first {
        record
    } else {
        check_terminal_metadata(&j, Some(&j), &scoped)?;
        let stored = j
            .reconciliation
            .as_ref()
            .ok_or_else(|| scoped.verification())?;
        let cap = super::restore_metadata::Caps::new(j.journal_capacity, &scoped)?.payload;
        if super::restore_metadata::measure(record, cap, &scoped)?.1
            != super::restore_metadata::measure(stored, cap, &scoped)?.1
        {
            return Err(fs::refused(
                &scoped,
                "intent retry requires the already accepted reconciliation",
            ));
        }
        stored
    };
    let readiness = super::readiness::assess_readiness(report, dependencies, accepted);
    if !readiness.ordinary_admission_ready {
        return Err(fs::refused(
            &scoped,
            "typed client, dependency and effect reconciliation required",
        ));
    }
    if first {
        let now = filesystem
            .io()
            .unix_ms()
            .map_err(|_| fs::destination(&scoped))?;
        if record.recorded_at_unix_ms < lease.request.issued_at_unix_ms
            || record.recorded_at_unix_ms > now
            || record.operator != lease.request.operator
        {
            return Err(fs::refused(
                &scoped,
                "current operator reconciliation evidence required",
            ));
        }
    }
    let context = if first {
        Some(accepting_context(
            &j,
            lease,
            mode,
            Some(record),
            None,
            filesystem,
            &scoped,
        )?)
    } else {
        None
    };
    let evidence = if first { Some(record.clone()) } else { None };
    let (attempt_limits, control) =
        attempt_scoped(&mut j, lease, filesystem, rollback_reserve, Some(&scoped))?;
    let mut l = scoped;
    l.deadline = l.deadline.min(attempt_limits.deadline);
    l.publication_io = Some(control.clone());
    if first {
        j.reconciliation = evidence;
        j.terminal_evidence_context = context;
        j.phase = intent;
        journal_append(&mut j, &control, &l)?;
    }
    let parent = Dir::open(j.destination.parent().ok_or_else(|| l.verification())?, &l)?;
    let staging = Dir::open(&j.staging, &l)?;
    sync_both(&parent, &staging, &*control.io, &l)?;
    j.phase = terminal;
    journal_append(&mut j, &control, &l)?;
    cleanup_outcome(
        terminal_cleanup(&j, lease, filesystem, &lock, &control, &l),
        RestoreOutcome::Finalized {
            operation_id: j.operation_id,
            readiness,
        },
    )
}

/// Negative-only handling for a changed original-absence layout. This cannot
/// adopt/verify a candidate or remove the guard, and is unavailable to renewal.
fn record_failed_original_absence(
    j: &mut RestoreJournal,
    lease: &OfflineLease,
    filesystem: &dyn RecoveryFs,
    lock: &RestoreOperationLock,
    retained: &OperationLimits,
) -> Result<(), RecoveryError> {
    let mut l = retained.clone();
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
        j.candidate_bytes,
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
    let lock = existing_operation_lock(&journal_path(journal), lease, filesystem, false, false)?;
    let mut j = load_with_held_lock(&journal_path(journal), lease, filesystem, &lock)?;
    let catalog_root = Dir::open(filesystem.root(), filesystem.reconciliation_limits())?;
    let _catalog_access = fs::CatalogLock::acquire(
        &catalog_root,
        fs::CatalogLockMode::Shared,
        false,
        filesystem.io(),
        filesystem.reconciliation_limits(),
    )?;
    if !matches!(j.original, FileBinding::Absent)
        || !matches!(
            j.phase,
            RestorePhase::OriginalAbsenceRestored | RestorePhase::OriginalAbsenceFinalized
        )
    {
        return Err(fs::refused(
            filesystem.reconciliation_limits(),
            "absence finalization requires restored original absence",
        ));
    }
    let retry = j.phase == RestorePhase::OriginalAbsenceFinalized;
    if !retry {
        let now = filesystem
            .io()
            .unix_ms()
            .map_err(|_| fs::destination(filesystem.reconciliation_limits()))?;
        // Malformed attestation is an ordinary pre-attempt refusal, never forensic.
        if record.version != 1
            || record.operation_id != j.operation_id
            || record.candidate_sha256 != j.candidate_sha256
            || !record.no_client_opened_path_while_installed
            || !super::manifest::sha256(&record.evidence_sha256)
            || record.operator != lease.request.operator
            || record.recorded_at_unix_ms < lease.request.issued_at_unix_ms
            || record.recorded_at_unix_ms > now
        {
            return Err(fs::refused(
                filesystem.reconciliation_limits(),
                "matching typed original-absence reconciliation required",
            ));
        }
    }
    if !retry && j.binding == lease.request {
        let mut inspection = filesystem.reconciliation_limits().clone();
        inspection.operation_id = j.operation_id;
        inspection.byte_limit = inspection.byte_limit.min(j.budget.read_byte_ceiling);
        inspection.memory_byte_limit = inspection.memory_byte_limit.min(j.budget.memory_ceiling);
        let inspection = journal_metadata_scope(&j, &inspection)?;
        let (inspection, _control) = terminal_control(&j, filesystem, &inspection)?;
        if original_absence_layout_changed(&j, &inspection)? {
            record_failed_original_absence(&mut j, lease, filesystem, &lock, &inspection)?;
            return Err(fs::refused(
                filesystem.reconciliation_limits(),
                "original absence layout changed; forensic_handling_required",
            ));
        }
    }
    let base = continuation_limits(&j, lease, filesystem, &lock)?;
    if retry {
        let (l, control) = terminal_control(&j, filesystem, &base)?;
        return cleanup_outcome(
            terminal_cleanup(&j, lease, filesystem, &lock, &control, &l),
            RestoreOutcome::OriginalAbsenceFinalized {
                operation_id: j.operation_id,
            },
        );
    }
    let scoped = terminal_input_scope(&j, lease, None, Some(record), &base)?;
    verify_preserved_candidate(&j, &j.staging.join("candidate.fls"), &scoped)?;
    let context = accepting_context(
        &j,
        lease,
        TerminalMode::OriginalAbsence,
        None,
        Some(record),
        filesystem,
        &scoped,
    )?;
    let evidence = record.clone();
    let (attempt_limits, control) = attempt_scoped(&mut j, lease, filesystem, true, Some(&scoped))?;
    let mut l = scoped;
    l.deadline = l.deadline.min(attempt_limits.deadline);
    l.publication_io = Some(control.clone());
    j.absence_reconciliation = Some(evidence);
    j.terminal_evidence_context = Some(context);
    j.phase = RestorePhase::OriginalAbsenceFinalized;
    journal_append(&mut j, &control, &l)?;
    let result = terminal_cleanup(&j, lease, filesystem, &lock, &control, &l);
    // Retain the existing initial-call negative-only classification if a callback
    // recreated the absent destination. A terminal retry never enters this path.
    if result.is_err() && entry_identity(&j.destination, &l)?.is_some() {
        j.forensic_handling_required = true;
        journal_append(&mut j, &control, &l)?;
    }
    cleanup_outcome(
        result,
        RestoreOutcome::OriginalAbsenceFinalized {
            operation_id: j.operation_id,
        },
    )
}

fn inspect_abandon_layout(j: &RestoreJournal, l: &OperationLimits) -> Result<(), RecoveryError> {
    if !matches!(
        j.phase,
        RestorePhase::Admitted
            | RestorePhase::Prepared
            | RestorePhase::ExchangeIntent
            | RestorePhase::Abandoned
    ) {
        return Err(fs::refused(
            l,
            "abandon requires untouched pre-exchange layout",
        ));
    }
    let (destination, candidate, original) = inspect_layout(j, l)?;
    if destination != binding_identity(&j.original)
        || candidate != Some(j.candidate_identity)
        || original.is_some()
    {
        return Err(l.verification());
    }
    let held = PrivateFile::open(&j.staging.join("candidate.fls"), false, l)?;
    if held.identity() != j.candidate_identity
        || held.file.metadata().map_err(|_| l.verification())?.len() > j.candidate_bytes
    {
        return Err(l.verification());
    }
    if j.verification.is_some() {
        matches_binding(&j.staging.join("candidate.fls"), &candidate_binding(j), l)?;
    } else if !matches!(j.phase, RestorePhase::Admitted | RestorePhase::Abandoned) {
        return Err(l.verification());
    }
    held.unchanged(l)
}

/// Release only the guard and selected pin for an untouched pre-exchange restore.
/// All candidate, original, journal, probe and lock artifacts remain for inspection.
pub fn abandon_restore(
    journal: &RestoreJournal,
    lease: &OfflineLease,
    filesystem: &dyn RecoveryFs,
) -> Result<RestoreOutcome, RecoveryError> {
    let _owner = OperationOwnership::acquire(
        &lease.request.physical_path,
        filesystem.reconciliation_limits(),
    )?;
    let lock = existing_operation_lock(&journal_path(journal), lease, filesystem, false, false)?;
    let mut j = load_with_held_lock(&journal_path(journal), lease, filesystem, &lock)?;
    let catalog_root = Dir::open(filesystem.root(), filesystem.reconciliation_limits())?;
    let _catalog_access = fs::CatalogLock::acquire(
        &catalog_root,
        fs::CatalogLockMode::Shared,
        false,
        filesystem.io(),
        filesystem.reconciliation_limits(),
    )?;
    inspect_abandon_layout(&j, filesystem.reconciliation_limits())?;
    let base = continuation_limits(&j, lease, filesystem, &lock)?;
    if j.phase == RestorePhase::Abandoned {
        let (l, control) = terminal_control(&j, filesystem, &base)?;
        return cleanup_outcome(
            terminal_cleanup(&j, lease, filesystem, &lock, &control, &l),
            RestoreOutcome::Abandoned {
                operation_id: j.operation_id,
            },
        );
    }
    let scoped = terminal_input_scope(&j, lease, None, None, &base)?;
    let context = accepting_context(
        &j,
        lease,
        TerminalMode::Abandon,
        None,
        None,
        filesystem,
        &scoped,
    )?;
    let (attempt_limits, control) = attempt_scoped(&mut j, lease, filesystem, true, Some(&scoped))?;
    let mut l = scoped;
    l.deadline = l.deadline.min(attempt_limits.deadline);
    l.publication_io = Some(control.clone());
    j.terminal_evidence_context = Some(context);
    j.phase = RestorePhase::Abandoned;
    journal_append(&mut j, &control, &l)?;
    cleanup_outcome(
        terminal_cleanup(&j, lease, filesystem, &lock, &control, &l),
        RestoreOutcome::Abandoned {
            operation_id: j.operation_id,
        },
    )
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
    pub torn_tail_bytes: u64,
    pub current_attempt: u32,
    pub attempt_deadline_unix_ms: u128,
    pub time_remaining_ms: u128,
    pub resume_finalize_attempts: u32,
    pub rollback_attempts: u32,
    pub forensic_handling_required: bool,
    pub artifacts: Vec<RestoreArtifact>,
    pub total_retained_bytes: u64,
    pub staging_identity: (u64, u64),
    /// Validated evidence already accepted for generation finalization.
    pub accepted_reconciliation: Option<super::readiness::ReconciliationRecord>,
    /// Validated evidence already accepted for original-absence finalization.
    pub accepted_absence_reconciliation: Option<super::readiness::AbsenceReconciliationRecord>,
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
        let (bytes, sha256, classification) = if identity == j.candidate_identity
            && j.verification.is_none()
            && matches!(j.phase, RestorePhase::Admitted | RestorePhase::Abandoned)
        {
            let held = PrivateFile::open(&path, false, &l)?;
            let bytes = held.file.metadata().map_err(|_| fs::destination(&l))?.len();
            if held.identity() != identity || bytes > j.candidate_bytes {
                return Err(l.verification());
            }
            held.unchanged(&l)?;
            (bytes, None, "selected_unverified_candidate")
        } else {
            match binding {
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
            }
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
    // Unpublished guards are operation artifacts, never admission fences.
    // They intentionally survive abandon; no pathname grants deletion authority.
    let temporary_guard = j.staging.join(GUARD_TEMP_NAME);
    if entry_identity(&temporary_guard, &l)?.is_some() {
        let held = PrivateFile::open(&temporary_guard, false, &l)?;
        l.quota(
            held.file.metadata().map_err(|_| l.verification())?.len(),
            GUARD_ALLOWANCE,
            "restore guard metadata",
        )?;
        let (bytes, sha256) = verify::hash_file(&held.file, &l)?;
        held.unchanged(&l)?;
        artifacts.push(RestoreArtifact {
            operation_id: j.operation_id,
            path: temporary_guard,
            identity: held.identity(),
            bytes,
            sha256: Some(sha256),
            classification: "unpublished_restore_guard".into(),
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
        torn_tail_bytes: j.torn_tail_bytes,
        current_attempt: j.current_attempt,
        attempt_deadline_unix_ms: j.attempt_deadline_unix_ms,
        time_remaining_ms: j.attempt_deadline_unix_ms.saturating_sub(now),
        resume_finalize_attempts: j.resume_finalize_attempts,
        rollback_attempts: j.rollback_attempts,
        forensic_handling_required: j.forensic_handling_required || j.rollback_attempts >= 4,
        artifacts,
        total_retained_bytes,
        staging_identity: j.staging_identity,
        accepted_reconciliation: j.reconciliation,
        accepted_absence_reconciliation: j.absence_reconciliation,
        budget: j.budget,
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RestorePin {
    version: u32,
    restore_operation: uuid::Uuid,
    bundle_operation: uuid::Uuid,
    bundle_sha256: String,
    destination: PathBuf,
    journal: PathBuf,
    journal_identity: (u64, u64),
    operation_lock_identity: (u64, u64),
}
impl RestorePin {
    fn for_journal(j: &RestoreJournal) -> Self {
        Self {
            version: 1,
            restore_operation: j.operation_id,
            bundle_operation: j.selected_bundle_operation,
            bundle_sha256: j.selected_bundle_sha256.clone(),
            destination: j.destination.clone(),
            journal: journal_path(j),
            journal_identity: j.journal_identity,
            operation_lock_identity: j.operation_lock_identity,
        }
    }
}
fn write_restore_pin(
    j: &RestoreJournal,
    filesystem: &dyn RecoveryFs,
    control: &Arc<PublicationIo>,
    l: &OperationLimits,
) -> Result<(), RecoveryError> {
    // Called while holding catalog SH and after durable Admitted + op lock.
    let pin = RestorePin::for_journal(j);
    let bytes = verify::serialized_checksum(&pin, l)?.0;
    l.quota(bytes, GUARD_ALLOWANCE / 2, "restore pin metadata")?;
    l.admit_memory(bytes.checked_mul(32).ok_or_else(|| quota(l))?)?;
    let catalog = Dir::open(&filesystem.root().join(".catalog"), l)?;
    let pins = catalog.child("restore-pins", false, l)?;
    catalog.sync(&*control.io, Directory::CatalogParent, l)?;
    let name = format!("{}.json", j.operation_id);
    let temp_name = format!("{name}.tmp");
    let expected = fs::write_json(&pins, &temp_name, &pin, Member::RestorePin, control, l)?;
    // The operation-scoped temp is deliberately not a pin. A crash before
    // this no-replace publication leaves only a private, undecoded cleanup artifact.
    let prepared = PrivateFile::open(&pins.path.join(&temp_name), false, l)?;
    if verify::hash_file(&prepared.file, l)? != expected {
        return Err(l.verification());
    }
    prepared.unchanged(l)?;
    pins.rename_checked(
        &temp_name,
        &pins,
        &name,
        &*control.io,
        RecoveryEvent::Publish,
        l,
        || prepared.unchanged(l),
    )?;
    // Rename changes ctime; bind the published name to the same inode and bytes.
    let published = PrivateFile::open(&pins.path.join(&name), false, l)?;
    if published.identity() != prepared.identity()
        || verify::hash_file(&published.file, l)? != expected
    {
        return Err(l.verification());
    }
    published.unchanged(l)?;
    pins.sync(&*control.io, Directory::RestorePinParent, l)?;
    catalog.sync(&*control.io, Directory::CatalogParent, l)?;
    pins.same_name(l)?;
    published.unchanged(l)
}
/// Read only the held private descriptor. Keep the decoded pin reservation
/// in the returned scope through validation and hashing by the caller.
fn read_restore_pin(
    held: &PrivateFile,
    scan: &OperationLimits,
) -> Result<(RestorePin, OperationLimits), RecoveryError> {
    let len = held.file.metadata().map_err(|_| scan.verification())?.len();
    // The fixed pin ceiling precedes every payload allocation and read. This
    // descriptor is also the sole source of the length and decoded bytes.
    scan.quota(len, GUARD_ALLOWANCE / 2, "restore pin metadata")?;
    scan.quota(len, scan.byte_limit, "metadata bytes")?;
    let decoded = len.checked_mul(32).ok_or_else(|| quota(scan))?;
    let mut pin_limits = scan.with_retained_memory(decoded)?;
    // Retain R2's conservative aggregate: prior evidence + iterator scratch +
    // decoded pin + JSON scratch + read/hash buffer, before vec!/serde allocation.
    pin_limits.admit_memory(decoded.checked_add(65536).ok_or_else(|| quota(scan))?)?;
    // A later hash rechecks this SAME descriptor's length against the fixed cap,
    // even if its inode grows after decode. General image limits cannot widen it.
    pin_limits.byte_limit = pin_limits.byte_limit.min(GUARD_ALLOWANCE / 2);
    let n = usize::try_from(len).map_err(|_| scan.verification())?;
    let mut bytes = vec![0; n];
    for (index, chunk) in bytes.chunks_mut(65536).enumerate() {
        pin_limits.check()?;
        fs::read_exact_at(&held.file, chunk, index as u64 * 65536, &pin_limits)?;
    }
    let pin = serde_json::from_slice(&bytes).map_err(|_| scan.verification())?;
    pin_limits.check()?;
    Ok((pin, pin_limits))
}
/// Stream private typed pins under catalog EX, retaining one bounded pin only.
/// Reserve closure capacity and iterator scratch before each pin read/parse.
/// Carry that scope plus the decoded pin reservation through validation/hash.
pub(crate) fn check_restore_pins<S>(
    catalog: &Dir,
    target: uuid::Uuid,
    l: &OperationLimits,
    state: &mut S,
    mut reserve: impl FnMut(&mut S, usize, &OperationLimits) -> Result<OperationLimits, RecoveryError>,
    mut retain: impl FnMut(&mut S, &str, &PrivateFile, &OperationLimits) -> Result<(), RecoveryError>,
) -> Result<(), RecoveryError> {
    if !catalog.exists("restore-pins", l)? {
        return Ok(());
    }
    let pins = Dir::open(&catalog.path.join("restore-pins"), l)?;
    // Admit iterator scratch even for an empty directory. The per-pin scope is
    // rebuilt from l, so the initial reservation is not charged twice.
    let _iteration = reserve(state, 0, l)?;
    pins.visit_names(l, |name| {
        let mut checked_descriptor = false;
        let result = (|| {
            if let Some(text) = name.strip_suffix(".json.tmp") {
                let op = uuid::Uuid::parse_str(text).map_err(|_| l.verification())?;
                if op.is_nil() || op.to_string() != text {
                    return Err(l.verification());
                }
                // Catalog EX excludes a live writer (publication holds SH). Only
                // a canonical private regular temp from a stopped admission is ignored;
                // it neither protects a bundle nor enters the retained pin closure.
                let scan = reserve(state, 0, l)?;
                let held = PrivateFile::open(&pins.path.join(name), false, &scan)?;
                checked_descriptor = true;
                return held.unchanged(&scan);
            }
            let scan = reserve(state, 1, l)?;
            let text = name.strip_suffix(".json").ok_or_else(|| l.verification())?;
            let op = uuid::Uuid::parse_str(text).map_err(|_| l.verification())?;
            if op.is_nil() || op.to_string() != text {
                return Err(l.verification());
            }
            let held = PrivateFile::open(&pins.path.join(name), false, &scan)?;
            checked_descriptor = true;
            // Decode the capped descriptor that retain() later hashes. A path
            // reopen here could validate a different pin after directory replacement.
            let (pin, pin_limits) = read_restore_pin(&held, &scan)?;
            let prefix = format!(".restore-{op}-");
            let staging = pin.journal.parent().ok_or_else(|| l.verification())?;
            let suffix_valid = staging
                .file_name()
                .and_then(|s| s.to_str())
                .and_then(|s| s.strip_prefix(&prefix))
                .is_some_and(|s| {
                    uuid::Uuid::parse_str(s).is_ok_and(|id| !id.is_nil() && id.to_string() == s)
                });
            if pin.version != 1
                || pin.restore_operation != op
                || pin.bundle_operation.is_nil()
                || !super::manifest::sha256(&pin.bundle_sha256)
                || !pin.destination.is_absolute()
                || !pin.journal.is_absolute()
                || pin.journal.file_name() != Some(std::ffi::OsStr::new("journal.bin"))
                || staging.parent() != pin.destination.parent()
                || !suffix_valid
                || pin.journal_identity == (0, 0)
                || pin.operation_lock_identity == (0, 0)
            {
                return Err(l.verification());
            }
            if pin.bundle_operation == target {
                return Err(fs::refused(
                    l,
                    "selected bundle is pinned by an unfinished restore",
                ));
            }
            retain(state, name, &held, &pin_limits)?;
            held.unchanged(&pin_limits)
        })();
        result.map_err(|error| {
            if matches!(error, RecoveryError::Verification { .. })
                && (checked_descriptor
                    || PrivateFile::open(&pins.path.join(name), false, l).is_ok())
            {
                error.at_metadata_entry(|| Path::new(".catalog/restore-pins").join(name))
            } else {
                error
            }
        })
    })
}

#[cfg(all(test, target_os = "linux"))]
mod pin_descriptor_tests {
    use super::*;
    use std::os::unix::fs::{FileExt, MetadataExt, PermissionsExt};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct Reads {
        calls: AtomicUsize,
        bytes: AtomicUsize,
        identities: std::sync::Mutex<Vec<(u64, u64)>>,
    }
    impl fs::RecoveryIo for Reads {
        fn read_exact_at(
            &self,
            file: &std::fs::File,
            bytes: &mut [u8],
            offset: u64,
        ) -> std::io::Result<()> {
            let metadata = file.metadata()?;
            self.identities
                .lock()
                .unwrap()
                .push((metadata.dev(), metadata.ino()));
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.bytes.fetch_add(bytes.len(), Ordering::SeqCst);
            file.read_exact_at(bytes, offset)
        }
    }
    fn limits(reads: Arc<Reads>) -> OperationLimits {
        let mut l = OperationLimits::new(
            std::time::Instant::now() + std::time::Duration::from_secs(30),
            1 << 20,
            16 << 20,
            super::super::CancelToken::new(),
        );
        l.publication_io = Some(Arc::new(PublicationIo::new(
            reads,
            1 << 20,
            1 << 20,
            1 << 20,
            4096,
        )));
        l
    }
    fn private_dir(path: &Path) {
        std::fs::create_dir(path).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    fn pin_bytes(root: &Path, operation: uuid::Uuid, bundle: uuid::Uuid) -> Vec<u8> {
        let staging = root.join(format!(".restore-{operation}-{}", uuid::Uuid::now_v7()));
        serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "restore_operation": operation,
            "bundle_operation": bundle,
            "bundle_sha256": "a".repeat(64),
            "destination": root.join("destination.fls"),
            "journal": staging.join("journal.bin"),
            "journal_identity": [1, 2],
            "operation_lock_identity": [1, 3],
        }))
        .unwrap()
    }
    fn write_pin(path: &Path, bytes: &[u8]) {
        std::fs::write(path, bytes).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[test]
    fn restore_pin_review14_resource_errors_keep_type_without_diagnostic_allocation() {
        for case in ["memory", "deadline", "cancelled"] {
            let root = super::super::test_support::fixture_root();
            let operation = uuid::Uuid::now_v7();
            let path = root.path().join(format!("{operation}.json"));
            write_pin(
                &path,
                &pin_bytes(root.path(), operation, uuid::Uuid::now_v7()),
            );
            let reads = Arc::new(Reads::default());
            let mut l = limits(reads.clone());
            let held = PrivateFile::open(&path, false, &l).unwrap();
            match case {
                "memory" => l.memory_byte_limit = 1,
                "deadline" => l.deadline = std::time::Instant::now(),
                "cancelled" => l.cancel.cancel(),
                _ => unreachable!(),
            }
            let error = read_restore_pin(&held, &l)
                .unwrap_err()
                .at_metadata_entry(|| panic!("resource refusal allocated a diagnostic path"));
            assert!(
                match case {
                    "memory" => matches!(error, RecoveryError::Quota { .. }),
                    "deadline" => matches!(error, RecoveryError::Deadline { .. }),
                    "cancelled" => matches!(error, RecoveryError::Cancelled { .. }),
                    _ => false,
                },
                "{case}: {error:?}"
            );
            assert_eq!(reads.calls.load(Ordering::SeqCst), 0);
        }
    }

    #[test]
    fn restore_pin_oversized_held_descriptor_refuses_before_payload_admission_or_read() {
        let root = super::super::test_support::fixture_root();
        let operation = uuid::Uuid::now_v7();
        let path = root.path().join(format!("{operation}.json"));
        let mut bytes = pin_bytes(root.path(), operation, uuid::Uuid::now_v7());
        bytes.resize(32769, b' '); // Valid JSON; general metadata limit permits it.
        write_pin(&path, &bytes);
        let reads = Arc::new(Reads::default());
        let l = limits(reads.clone());
        let held = PrivateFile::open(&path, false, &l).unwrap();
        let admitted = l.admitted_memory_bytes();
        assert!(matches!(
            read_restore_pin(&held, &l),
            Err(RecoveryError::Quota {
                resource: "restore pin metadata",
                ..
            })
        ));
        assert_eq!(reads.calls.load(Ordering::SeqCst), 0);
        assert_eq!(l.admitted_memory_bytes(), admitted);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        held.unchanged(&l).unwrap();
    }

    #[test]
    fn restore_pin_scan_ignores_only_private_canonical_regular_temps_without_reads() {
        for case in 0..9 {
            let root = super::super::test_support::fixture_root();
            let pins = root.path().join("restore-pins");
            private_dir(&pins);
            let operation = uuid::Uuid::parse_str("01234567-89ab-7cde-8fab-0123456789ab").unwrap();
            let name = match case {
                1 => format!("{}.json.tmp", uuid::Uuid::nil()),
                2 => format!("{}.json.tmp", operation.to_string().to_uppercase()),
                3 => format!("{operation}.json.tmp.extra"),
                4 => format!("{operation}.json"),
                _ => format!("{operation}.json.tmp"),
            };
            let path = pins.join(&name);
            match case {
                5 => std::os::unix::fs::symlink("missing", &path).unwrap(),
                6 => private_dir(&path),
                _ => write_pin(&path, b"torn undecodable metadata"),
            }
            if case == 7 {
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            }
            if case == 8 {
                std::fs::hard_link(&path, root.path().join("alias")).unwrap();
            }
            let reads = Arc::new(Reads::default());
            let l = limits(reads.clone());
            let catalog = Dir::open(root.path(), &l).unwrap();
            let mut retained = 0usize;
            let result = check_restore_pins(
                &catalog,
                uuid::Uuid::now_v7(),
                &l,
                &mut retained,
                |_, _, l| Ok(l.clone()),
                |count, _, _, _| {
                    *count += 1;
                    Ok(())
                },
            );
            if case == 0 {
                result.unwrap();
            } else {
                assert!(result.is_err(), "case {case}");
            }
            assert_eq!(retained, 0, "a temp must never protect a bundle");
            // Malformed final pins are decoded and refuse. Every temp variant
            // is either ignored without payload reads or refused before them.
            if case != 4 {
                assert_eq!(reads.bytes.load(Ordering::SeqCst), 0);
            }
            assert!(
                std::fs::symlink_metadata(path).is_ok(),
                "scan must be read-only"
            );
        }
    }

    // This exercises the exact after-open/before-decode helper boundary.
    // A pins target; B is a well-formed pin for another bundle. Replacing the
    // entire directory preserves both inodes and defeats a name-only comparison.
    fn directory_replacement_reads_and_hashes_a(replacement_len: Option<usize>) {
        let root = super::super::test_support::fixture_root();
        let pins = root.path().join("restore-pins");
        let replacement = root.path().join("replacement-pins");
        let parked = root.path().join("parked-pins");
        private_dir(&pins);
        private_dir(&replacement);
        let operation = uuid::Uuid::now_v7();
        let target = uuid::Uuid::now_v7();
        let other = uuid::Uuid::now_v7();
        let name = format!("{operation}.json");
        let a = pin_bytes(root.path(), operation, target);
        let mut b = pin_bytes(root.path(), operation, other);
        if let Some(len) = replacement_len {
            b.resize(len, b' ');
        }
        write_pin(&pins.join(&name), &a);
        write_pin(&replacement.join(&name), &b);
        let reads = Arc::new(Reads::default());
        let l = limits(reads.clone());
        let held = PrivateFile::open(&pins.join(&name), false, &l).unwrap();
        let identity_a = held.identity();
        std::fs::rename(&pins, &parked).unwrap();
        std::fs::rename(&replacement, &pins).unwrap();
        let named_b = std::fs::metadata(pins.join(&name)).unwrap();
        assert_ne!(identity_a, (named_b.dev(), named_b.ino()));

        let (pin, scope) = read_restore_pin(&held, &l).unwrap();
        // Reopening B would decode `other`, allowing target's pin to be missed.
        assert_eq!(pin.version, 1);
        assert_eq!(pin.restore_operation, operation);
        assert_eq!(pin.bundle_operation, target);
        assert_eq!(pin.bundle_sha256, "a".repeat(64));
        assert_eq!(reads.bytes.load(Ordering::SeqCst), a.len());
        assert_eq!(scope.byte_limit, 32768);
        let (len, hash) = verify::hash_file(&held.file, &scope).unwrap();
        assert_eq!(len, a.len() as u64);
        assert_eq!(hash, format!("{:x}", Sha256::digest(&a)));
        assert_eq!(reads.bytes.load(Ordering::SeqCst), 2 * a.len());
        assert!(
            reads
                .identities
                .lock()
                .unwrap()
                .iter()
                .all(|id| *id == identity_a)
        );
        // Binding loss must fail even though the held bytes are valid and stable.
        assert!(held.unchanged(&scope).is_err());
        assert_eq!(std::fs::read(parked.join(&name)).unwrap(), a);
        assert_eq!(std::fs::read(pins.join(&name)).unwrap(), b);
    }
    #[test]
    fn restore_pin_directory_replacement_cannot_validate_b_and_hash_a() {
        directory_replacement_reads_and_hashes_a(None);
    }
    #[test]
    fn restore_pin_directory_replacement_never_reads_oversized_b() {
        directory_replacement_reads_and_hashes_a(Some(131072));
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::recovery::{admission::*, manifest::BundleManifestV1};
    use std::fs;
    use std::os::unix::fs::MetadataExt;
    #[test]
    fn exchange_error_classifies_native_errno_without_fallback() {
        let l = OperationLimits::new(
            std::time::Instant::now() + std::time::Duration::from_secs(10),
            1 << 20,
            1 << 20,
            super::super::CancelToken::new(),
        );
        for errno in [libc::EINVAL, libc::ENOSYS] {
            assert!(
                matches!(
                    exchange_error(&std::io::Error::from_raw_os_error(errno), &l),
                    RecoveryError::Unsupported { operation_id, .. } if operation_id == l.operation_id
                ),
                "errno={errno}"
            );
        }
        for expected in [
            libc::EMFILE,
            libc::ENFILE,
            libc::ENOMEM,
            libc::EINTR,
            libc::EAGAIN,
        ] {
            assert!(
                matches!(
                    exchange_error(&std::io::Error::from_raw_os_error(expected), &l),
                    RecoveryError::Transient { operation_id, errno } if operation_id == l.operation_id && errno == expected
                ),
                "errno={expected}"
            );
        }
        for error in [
            std::io::Error::from_raw_os_error(libc::EIO),
            std::io::Error::from_raw_os_error(libc::ENOSPC),
            std::io::Error::from_raw_os_error(libc::EXDEV),
            std::io::Error::other("no native errno"),
        ] {
            assert!(
                matches!(
                    exchange_error(&error, &l),
                    RecoveryError::Destination { operation_id } if operation_id == l.operation_id
                ),
                "{error:?}"
            );
        }
    }
    mod fixture_support {
        #![allow(dead_code)]
        use crate::recovery::test_support;
        use crate::{
            FerroStore,
            recovery::{
                admission::*,
                backup::*,
                fs::{NativeRecoveryFs, RecoveryFs},
                manifest::{Availability, BuildIdentity},
                *,
            },
        };
        use std::{
            fs,
            os::unix::fs::MetadataExt,
            path::{Path, PathBuf},
            time::{Duration, Instant},
        };
        pub fn limits() -> OperationLimits {
            OperationLimits::new(
                Instant::now() + Duration::from_secs(3500),
                256 << 20,
                1 << 30,
                CancelToken::new(),
            )
        }
        pub struct Fixture {
            pub root: tempfile::TempDir,
            pub source: PathBuf,
            pub backups: PathBuf,
        }
        impl Fixture {
            pub fn new() -> Self {
                let root = test_support::fixture_root();
                let source = root.path().join("source.fls");
                let backups = root.path().join("backups");
                fs::create_dir(&backups).unwrap();
                fs::set_permissions(
                    &backups,
                    std::os::unix::fs::PermissionsExt::from_mode(0o700),
                )
                .unwrap();
                let owner = FerroStore::open(&source).unwrap();
                // Real append history, never padding or a metadata accounting exemption.
                for n in 0..80 {
                    owner
                        .put_doc(
                            "fixture_unknown",
                            "one",
                            &serde_json::json!({"n":n,"body":"x".repeat(256<<10)}),
                        )
                        .unwrap();
                }
                drop(owner);
                Self {
                    root,
                    source,
                    backups,
                }
            }
            pub fn filesystem(&self) -> NativeRecoveryFs {
                NativeRecoveryFs::new(self.backups.clone(), limits())
            }
            pub fn identity(&self) -> StoreIdentity {
                let map = collect_inventory(&InventoryInputs {
                    roots: vec![self.root.path().to_path_buf()],
                    bindings: vec![BindingInput::config(
                        "fixture",
                        self.source.clone(),
                        Some("ferro".into()),
                        "fixture",
                    )],
                    ..Default::default()
                })
                .unwrap();
                map.stores
                    .iter()
                    .find(|s| s.identity.canonical_path.as_deref() == Some(&self.source))
                    .unwrap()
                    .identity
                    .clone()
            }
            pub fn backup_request(&self) -> BackupRequest {
                let store = self.identity();
                let l = limits();
                let deadline = l.deadline;
                BackupRequest {
                    profile: ValidationProfile::new(
                        ProfileKind::SmallEmpty,
                        store.catalog_key.clone(),
                    ),
                    store,
                    source_map_sha256: "a".repeat(64),
                    operation_id: l.operation_id,
                    batch_id: None,
                    destination: self.backups.clone(),
                    capture: CaptureBudget::new(
                        deadline,
                        256 << 20,
                        512 << 20,
                        Duration::from_secs(1),
                    ),
                    limits: l,
                    image_byte_limit: None,
                    metadata_byte_limit: 1 << 20,
                    space_margin_bytes: 4096,
                    configs: vec![],
                    excluded_vault_key: None,
                    code_index_configuration: Availability::Unavailable,
                }
            }
            pub fn bundle(&self) -> VerifiedBundle {
                let owner = FerroStore::open(&self.source).unwrap();
                let bundle = backup(
                    &owner,
                    &self.backup_request(),
                    &self.filesystem(),
                    &CancelToken::new(),
                )
                .unwrap();
                match bundle {
                    PublicationOutcome::Durable(bundle) => bundle,
                    _ => panic!("fixture publication not durable"),
                }
            }
            pub fn lease_request(
                &self,
                action: OfflineAction,
                operation_id: uuid::Uuid,
                profile_sha256: String,
                destination: &Path,
            ) -> OfflineLeaseRequest {
                let file_id = |p: &Path| {
                    let m = fs::metadata(p).unwrap();
                    (m.dev(), m.ino())
                };
                let now = self.filesystem().io().unix_ms().unwrap();
                OfflineLeaseRequest {
                    version: 1,
                    operation_id,
                    action,
                    catalog_identity: self.identity().catalog_key,
                    approved_root_identity: file_id(&self.backups),
                    physical_path: destination.to_path_buf(),
                    parent_identity: file_id(destination.parent().unwrap()),
                    expected_file: inspect_file_binding(destination, &limits()).unwrap(),
                    profile_sha256,
                    build: BuildIdentity {
                        package_version: env!("CARGO_PKG_VERSION").into(),
                        source_revision: Availability::Unavailable,
                        executable_sha256: Availability::Unavailable,
                    },
                    issued_at_unix_ms: now,
                    expires_at_unix_ms: now + 3600000,
                    operator: "fixture operator".into(),
                    exclusion: ExclusionAttestation {
                        legacy_and_cached_handles_excluded: true,
                        external_dispatch_excluded: true,
                        active_owner_excluded: true,
                        evidence: "all fixture handles closed; no dispatch".into(),
                    },
                }
            }
        }
    }
    use fixture_support::{Fixture, limits};
    fn prepare_restore(
        f: &Fixture,
        destination: &std::path::Path,
    ) -> (RestoreRequest, OfflineLease) {
        let bundle = f.bundle();
        prepare_restore_bundle(f, destination, bundle)
    }
    fn prepare_restore_bundle(
        f: &Fixture,
        destination: &std::path::Path,
        bundle: crate::recovery::backup::VerifiedBundle,
    ) -> (RestoreRequest, OfflineLease) {
        let manifest: BundleManifestV1 =
            serde_json::from_slice(&fs::read(bundle.path().join("manifest.json")).unwrap())
                .unwrap();
        let l = limits();
        let binding = f.lease_request(
            OfflineAction::Restore {
                bundle_operation: uuid::Uuid::parse_str(&manifest.operation_id).unwrap(),
                bundle_sha256: format!(
                    "{:x}",
                    sha2::Sha256::digest(fs::read(bundle.path().join("manifest.json")).unwrap())
                ),
            },
            l.operation_id,
            manifest.validation_profile_sha256.clone(),
            destination,
        );
        let lease = create_offline_lease(&binding, &f.filesystem(), &l).unwrap();
        (
            RestoreRequest {
                operation_id: l.operation_id,
                store: f.identity(),
                destination: destination.to_path_buf(),
                staging_parent: destination.parent().unwrap().to_path_buf(),
                selected_bundle: bundle,
                limits: l,
                metadata_byte_limit: 1 << 20,
                space_margin_bytes: 4096,
                original_profile: crate::recovery::ValidationProfile::new(
                    crate::recovery::ProfileKind::SmallEmpty,
                    f.identity().catalog_key,
                ),
                available_dependencies: vec![],
            },
            lease,
        )
    }

    #[test]
    fn restore_access_rejects_held_lock_b_even_with_original_name_a() {
        use std::os::unix::fs::PermissionsExt;
        let f = Fixture::new();
        let (request, lease) = prepare_restore(&f, &f.source);
        let RestoreOutcome::Installed { journal } =
            restore_offline(&request, &lease, &f.filesystem()).unwrap()
        else {
            panic!("not installed")
        };
        let path = journal.staging.join("journal.bin");
        let before = fs::read(&path).unwrap();
        let guard = f.source.with_extension("fls.restore-guard");
        let guard_before = fs::read(&guard).unwrap();
        let name = journal.staging.join("journal.bin.lock");
        let a = journal.staging.join("parked-a.lock");
        let b = journal.staging.join("held-b.lock");
        fs::rename(&name, &a).unwrap();
        fs::write(&name, b"").unwrap();
        fs::set_permissions(&name, fs::Permissions::from_mode(0o600)).unwrap();
        let filesystem = f.filesystem();
        let lock = existing_operation_lock(&path, &lease, &filesystem, true, false).unwrap();
        assert_ne!(lock.held.identity(), journal.operation_lock_identity);
        fs::rename(&name, &b).unwrap();
        fs::rename(&a, &name).unwrap();
        let named = fs::metadata(&name).unwrap();
        assert_eq!((named.dev(), named.ino()), journal.operation_lock_identity);
        assert!(
            admission::RestoreAccess::obtain(&journal, &lease, &filesystem, &lock, &limits())
                .is_err()
        );
        assert_eq!(fs::read(path).unwrap(), before);
        assert_eq!(fs::read(&name).unwrap(), fs::read(&b).unwrap());
        assert_eq!(
            fs::read(&guard).unwrap(),
            guard_before,
            "rejected operation must preserve guard bytes"
        );
    }

    fn journal_frame_ranges(bytes: &[u8]) -> Vec<std::ops::Range<usize>> {
        let mut frames = Vec::new();
        let mut offset = 0;
        while bytes.get(offset..offset + 4) == Some(b"RJR1") {
            let len =
                u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap()) as usize;
            let end = offset + 72 + len;
            assert!(end <= bytes.len());
            frames.push(offset..end);
            offset = end;
        }
        frames
    }
    fn journal_fixture_write(path: &std::path::Path, bytes: &[u8]) {
        use std::os::unix::fs::FileExt;
        let file = fs::OpenOptions::new().write(true).open(path).unwrap();
        assert_eq!(file.metadata().unwrap().len(), bytes.len() as u64);
        file.write_all_at(bytes, 0).unwrap();
        file.sync_all().unwrap();
    }
    #[test]
    fn restore_tail_repair_every_payload_and_header_write_prefix_loads_and_retries() {
        let f = Fixture::new();
        let (request, lease) = prepare_restore(&f, &f.source);
        let RestoreOutcome::Installed { journal } =
            restore_offline(&request, &lease, &f.filesystem()).unwrap()
        else {
            panic!("not installed")
        };
        assert!(journal.budget.allocation_ceiling <= journal.budget.baseline_bytes * 2);
        let required = journal.budget.candidate_bytes + journal.budget.metadata_bytes;
        assert!(required <= journal.budget.allocation_ceiling);
        assert!(
            journal.budget.allocation_ceiling - required >= 512 << 10,
            "explicit strict-2L headroom"
        );
        assert!(lease.request.expires_at_unix_ms - lease.request.issued_at_unix_ms <= 3_600_000);
        let path = journal.staging.join("journal.bin");
        let original = fs::read(&path).unwrap();
        let frames = journal_frame_ranges(&original);
        let last = frames.last().unwrap();
        let mut damaged = original.clone();
        damaged[last.start + 40] ^= 1; // guaranteed failed final record
        let extent = damaged[last.clone()].iter().rposition(|b| *b != 0).unwrap() + 1;
        let payload_start = last.start + 72;
        let repair_end = last.start + extent;
        let assert_image = |image: &[u8], label: &str| {
            journal_fixture_write(&path, image);
            let before = fs::metadata(&path).unwrap();
            let filesystem = f.filesystem();
            let _lock = existing_operation_lock(&path, &lease, &filesystem, false, true).unwrap();
            let loaded = load_with_held_lock(&path, &lease, &filesystem, &_lock).unwrap();
            assert_eq!(loaded.phase, RestorePhase::RelocateIntent, "{label}");
            let expected = image[last.start..]
                .iter()
                .rposition(|b| *b != 0)
                .map_or(0, |n| n as u64 + 1);
            assert_eq!(loaded.torn_tail_bytes, expected, "{label}");
            assert_eq!(fs::read(&path).unwrap(), image, "read-only load: {label}");
            let after = fs::metadata(&path).unwrap();
            assert_eq!(
                (before.mtime(), before.mtime_nsec()),
                (after.mtime(), after.mtime_nsec()),
                "{label}"
            );
            let mut repaired = loaded.clone();
            repaired.current_attempt += 1;
            repaired.resume_finalize_attempts += 1;
            repaired.phase = RestorePhase::Installed;
            let mut l = filesystem.reconciliation_limits().clone();
            l.operation_id = repaired.operation_id;
            let dir = Dir::open(&repaired.staging, &l).unwrap();
            let unit = super::fs::allocation_geometry(&dir, &l).unwrap();
            let control = Arc::new(PublicationIo::new(
                filesystem.io(),
                repaired.budget.allocation_ceiling,
                repaired.budget.metadata_bytes,
                repaired.candidate_bytes,
                unit,
            ));
            journal_append(&mut repaired, &control, &l).unwrap();
            let clean = load_with_held_lock(&path, &lease, &filesystem, &_lock).unwrap();
            assert_eq!(clean.phase, RestorePhase::Installed, "{label}");
            assert_eq!(clean.torn_tail_bytes, 0, "{label}");
            assert_eq!(clean.current_attempt, loaded.current_attempt + 1, "{label}");
            assert_eq!(clean.current_attempt, repaired.current_attempt);
            assert_eq!(fs::metadata(&path).unwrap().len(), original.len() as u64);
            assert_eq!(
                fs::metadata(&f.source).unwrap().ino(),
                journal.candidate_identity.1
            );
        };
        // Match reverse-chunk repair order, including every short-write prefix.
        let mut image = damaged.clone();
        let mut cursor = repair_end;
        while cursor > payload_start {
            let start = cursor.saturating_sub(65536).max(payload_start);
            for prefix in 0..=cursor - start {
                let mut visible = image.clone();
                visible[start..start + prefix].fill(0);
                // One persisted byte image covers either observation of this
                // prefix before the first successful sync; repeating identical
                // bytes cannot add a distinct durable crash state.
                assert_image(&visible, &format!("payload {start}:{prefix} before-sync"));
            }
            image[start..cursor].fill(0);
            cursor = start;
        }
        assert_image(&image, "payload complete after-first-sync");
        // Header zeroing is reachable only after payload sync succeeded.
        for prefix in 0..=72 {
            let mut visible = image.clone();
            visible[last.start..last.start + prefix].fill(0);
            assert_image(&visible, &format!("header {prefix} after-first-sync"));
        }
        // The fully erased image is clean (diagnostic zero), rather than torn.
    }

    struct ScanReadCut {
        reads: std::sync::atomic::AtomicUsize,
        cancel: super::super::CancelToken,
        stop_at: usize,
    }
    impl super::fs::RecoveryIo for ScanReadCut {
        fn read_exact_at(
            &self,
            file: &std::fs::File,
            bytes: &mut [u8],
            offset: u64,
        ) -> std::io::Result<()> {
            use std::os::unix::fs::FileExt;
            file.read_exact_at(bytes, offset)?;
            if self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1 == self.stop_at {
                self.cancel.cancel();
            }
            Ok(())
        }
    }
    #[test]
    fn later_frame_scan_checks_cancel_between_scan_and_hash_chunks() {
        use std::os::unix::fs::FileExt;
        let root = crate::recovery::test_support::fixture_root();
        let path = root.path().join("scan.bin");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        let capacity = 3 * 65536;
        file.set_len(capacity).unwrap();
        let mut header = [0u8; 72];
        header[..4].copy_from_slice(b"RJR1");
        header[4..8].copy_from_slice(&(2 * 65536u32).to_le_bytes());
        // Zero digest is intentionally invalid, but hashing spans two chunks.
        file.write_all_at(&header, 1).unwrap();
        for stop_at in [1, 2, 3] {
            let mut l = limits();
            let cancel = l.cancel.clone();
            let io = Arc::new(ScanReadCut {
                reads: std::sync::atomic::AtomicUsize::new(0),
                cancel,
                stop_at,
            });
            l.publication_io = Some(Arc::new(PublicationIo::new(
                io.clone(),
                1 << 30,
                1 << 30,
                1 << 30,
                4096,
            )));
            let mut buffer = [0; 65536];
            assert!(matches!(
                reject_later_complete_frame(&file, 0, capacity, &l, &mut buffer),
                Err(RecoveryError::Cancelled { .. })
            ));
            assert_eq!(io.reads.load(std::sync::atomic::Ordering::SeqCst), stop_at);
        }
        let mut l = limits();
        l.deadline = std::time::Instant::now();
        let mut buffer = [0; 65536];
        assert!(matches!(
            reject_later_complete_frame(&file, 0, capacity, &l, &mut buffer),
            Err(RecoveryError::Deadline { .. })
        ));
        assert_eq!(file.metadata().unwrap().len(), capacity);
        let mut retained = [0; 72];
        file.read_exact_at(&mut retained, 1).unwrap();
        assert_eq!(retained, header);
    }
    // INERT focused streaming/loader regressions. Runs only in approved fixture runner.
    fn rewrite_chained_frames(capacity: usize, payloads: &[Vec<u8>]) -> Vec<u8> {
        let mut bytes = vec![0; capacity];
        let mut previous = [0; 32];
        let mut offset = 0;
        for payload in payloads {
            let mut hash = Sha256::new();
            hash.update(previous);
            hash.update(payload);
            let digest: [u8; 32] = hash.finalize().into();
            let end = offset + 72 + payload.len();
            assert!(end <= capacity);
            bytes[offset..offset + 4].copy_from_slice(b"RJR1");
            bytes[offset + 4..offset + 8].copy_from_slice(&(payload.len() as u32).to_le_bytes());
            bytes[offset + 8..offset + 40].copy_from_slice(&previous);
            bytes[offset + 40..offset + 72].copy_from_slice(&digest);
            bytes[offset + 72..end].copy_from_slice(payload);
            previous = digest;
            offset = end;
        }
        bytes
    }
    fn streaming_fixture() -> (Fixture, OfflineLease, RestoreJournal) {
        let f = Fixture::new();
        let (request, lease) = prepare_restore(&f, &f.source);
        let RestoreOutcome::Installed { journal } =
            restore_offline(&request, &lease, &f.filesystem()).unwrap()
        else {
            panic!("not installed")
        };
        (f, lease, journal)
    }
    #[test]
    fn streaming_frame_schedule_is_cumulative_and_admitted_before_serializer() {
        use crate::recovery::restore_metadata::{Caps, MAX_FRAMES, WRITER_MAX_FRAMES};
        let l = limits();
        let capacity = (1 << 20) - 65536;
        let caps = Caps::new(capacity, &l).unwrap();
        assert_eq!(WRITER_MAX_FRAMES, 5 + 12 * 4 + 4 * 4);
        assert_eq!(WRITER_MAX_FRAMES, 69);
        const { assert!(WRITER_MAX_FRAMES <= MAX_FRAMES) };
        let mut end = 0u64;
        for n in 0..MAX_FRAMES {
            assert_eq!(caps.next_frame(n, caps.payload, &l).unwrap(), n + 1);
            end += 72 + caps.payload;
            assert!(end <= capacity);
        }
        assert!(caps.next_frame(MAX_FRAMES, caps.payload, &l).is_err());
        assert!(caps.next_frame(0, caps.payload + 1, &l).is_err());
        // Poison serializer makes admission ordering observable, without allocating
        // an oversized output merely to test its subsequent refusal.
        struct Poison;
        impl serde::Serialize for Poison {
            fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
                panic!("serializer invoked before admission")
            }
        }
        let mut low = l.clone();
        low.memory_byte_limit = 65535;
        assert!(matches!(
            crate::recovery::restore_metadata::measure(&Poison, caps.payload, &low),
            Err(RecoveryError::Quota { .. })
        ));
    }
    #[test]
    fn streaming_saved_memory_bootstrap_is_unambiguous_and_nonallocating() {
        let root = crate::recovery::test_support::fixture_root();
        let path = root.path().join("bootstrap.json");
        let l = limits();
        let cases: &[(&[u8], Option<u64>, bool)] = &[
            (br#"{"budget":{"memory_ceiling":65536}}"#, Some(65536), true),
            (b"[]", None, true),
            (b"null", None, true),
            (br#""text""#, None, true),
            (b"123", None, true),
            (br#"[{"budget":{"memory_ceiling":1}}]"#, None, true),
            (
                br#"{"\u0062udget":{"memory_\u0063eiling":65536}}"#,
                Some(65536),
                true,
            ),
            (
                br#"{"other":{"budget":{"memory_ceiling":1}},"budget":{"memory_ceiling":65536}}"#,
                Some(65536),
                true,
            ),
            (
                br#"{"budget":{"memory_ceiling":1,"memory_\u0063eiling":2}}"#,
                None,
                true,
            ),
            (
                br#"{"budget":{"memory_ceiling":1},"\u0062udget":{"memory_ceiling":2}}"#,
                None,
                true,
            ),
            (
                br#"{"budget":{"memory_ceiling":18446744073709551616}}"#,
                None,
                true,
            ),
            (br#"{"budget":{"memory_ceiling":1e3}}"#, None, true),
            (br#"{"budget":{"memory_ceiling":-1}}"#, None, true),
            (br#"{"budget":{"memory_ceiling":"65536"}}"#, None, true),
            (br#"{"budget":[],"memory_ceiling":65536}"#, None, true),
            (br#"{"budget":{"memory_ceiling":65536}}false"#, None, false),
            (br#"{"budget":{"memory_ceiling":065536}}"#, None, false),
            (
                br#"{"budget":{"memory_ceiling":65536},"s":"\uD800"}"#,
                None,
                false,
            ),
            (
                b"{\"budget\":{\"memory_ceiling\":65536},\"s\":\"\xff\"}",
                None,
                false,
            ),
        ];
        for (bytes, expected, valid) in cases {
            fs::write(&path, bytes).unwrap();
            let file = fs::File::open(&path).unwrap();
            let result = saved_memory_ceiling(&file, 0, bytes.len() as u64, &l);
            if *valid {
                assert_eq!(result.unwrap(), *expected, "{bytes:?}");
            } else {
                assert!(
                    matches!(result, Err(RecoveryError::Verification { .. })),
                    "{bytes:?}"
                );
            }
        }
        let mut deep = br#"{"budget":{"memory_ceiling":65536},"x":"#.to_vec();
        deep.extend(std::iter::repeat_n(b'[', 33));
        deep.extend_from_slice(b"0");
        deep.extend(std::iter::repeat_n(b']', 33));
        deep.push(b'}');
        fs::write(&path, &deep).unwrap();
        let file = fs::File::open(&path).unwrap();
        assert!(
            matches!(saved_memory_ceiling(&file,0,deep.len() as u64,&l),Err(RecoveryError::Quota{resource,..}) if resource.starts_with("journal JSON nesting depth"))
        );
        // Boundary-crossing keys/escapes/UTF8 are parsed without retaining text.
        let mut long = br#"{"s":""#.to_vec();
        long.extend(std::iter::repeat_n(b'x', 4091));
        long.extend_from_slice("é\",\"budget\":{\"memory_\\u0063eiling\":65536}}".as_bytes());
        fs::write(&path, &long).unwrap();
        let file = fs::File::open(&path).unwrap();
        assert_eq!(
            saved_memory_ceiling(&file, 0, long.len() as u64, &l).unwrap(),
            Some(65536)
        );
    }
    #[test]
    fn streaming_overcap_final_and_nonfinal_refuse_without_decoding_or_changing_journal() {
        let (f, lease, journal) = streaming_fixture();
        let path = journal_path(&journal);
        let original = fs::read(&path).unwrap();
        let frames = journal_frame_ranges(&original);
        let cap = crate::recovery::restore_metadata::Caps::new(journal.journal_capacity, &limits())
            .unwrap()
            .payload as usize;
        let baseline: Vec<Vec<u8>> = frames
            .iter()
            .map(|r| original[r.start + 72..r.end].to_vec())
            .collect();
        for index in [0, baseline.len() - 1] {
            for typed in [true, false] {
                let mut payloads = baseline.clone();
                if !typed {
                    payloads[index] = b"{}".to_vec();
                }
                payloads[index].resize(cap + 1, b' ');
                let bytes = rewrite_chained_frames(original.len(), &payloads);
                journal_fixture_write(&path, &bytes);
                FRAME_OWNED_ALLOCATIONS.with(|n| n.set(0));
                let error = load_restore_journal(&path, &lease, &f.filesystem()).unwrap_err();
                assert!(
                    matches!(error,RecoveryError::RestoreJournalFrameQuota{declared_bytes,cap:c,..} if declared_bytes==cap as u64+1 && c==cap as u64)
                );
                assert!(error.to_string().contains(&path.display().to_string()));
                assert!(error.to_string().contains("no repair or unfence attempted"));
                assert_eq!(FRAME_OWNED_ALLOCATIONS.with(|n| n.get()), index as u64);
                assert_eq!(fs::read(&path).unwrap(), bytes);
                assert!(admission::guard_path(&f.source).exists());
            }
        }
        let mut payloads = baseline;
        payloads.last_mut().unwrap().resize(cap, b' ');
        let bytes = rewrite_chained_frames(original.len(), &payloads);
        journal_fixture_write(&path, &bytes);
        let loaded = load_restore_journal(&path, &lease, &f.filesystem()).unwrap();
        assert_eq!(loaded.log_frames, frames.len() as u64);
        assert_eq!(loaded.torn_tail_bytes, 0);
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }
    #[test]
    fn streaming_count_96_accepts_and_97_final_or_nonfinal_refuses_before_owned_decode() {
        let (f, lease, journal) = streaming_fixture();
        let path = journal_path(&journal);
        let original = fs::read(&path).unwrap();
        let frames = journal_frame_ranges(&original);
        let last = frames.last().unwrap();
        let body = original[last.start + 72..last.end].to_vec();
        for count in [96usize, 97, 98] {
            let payloads = vec![body.clone(); count];
            let bytes = rewrite_chained_frames(original.len(), &payloads);
            journal_fixture_write(&path, &bytes);
            FRAME_OWNED_ALLOCATIONS.with(|n| n.set(0));
            let result = load_restore_journal(&path, &lease, &f.filesystem());
            if count == 96 {
                let loaded = result.unwrap();
                assert_eq!(loaded.log_frames, 96);
                assert_eq!(loaded.torn_tail_bytes, 0);
            } else {
                let error = result.unwrap_err();
                assert!(matches!(
                    error,
                    RecoveryError::RestoreJournalFrameCountQuota {
                        frame_index: 97,
                        cap: 96,
                        ..
                    }
                ));
                assert!(error.to_string().contains(&path.display().to_string()));
                assert!(error.to_string().contains("no repair or unfence attempted"));
            }
            assert_eq!(FRAME_OWNED_ALLOCATIONS.with(|n| n.get()), 96);
            assert_eq!(fs::read(&path).unwrap(), bytes);
        }
        // Bad candidate header/hash/syntax at count97 is still a final R3 tail,
        // without allocating candidate97 or turning it into a count attempt.
        for defect in 0..3 {
            let mut payloads = vec![body.clone(); 96];
            payloads.push(if defect == 2 {
                b"{".to_vec()
            } else {
                body.clone()
            });
            let mut bytes = rewrite_chained_frames(original.len(), &payloads);
            let ranges = journal_frame_ranges(&bytes);
            let start = ranges.last().unwrap().start;
            if defect == 0 {
                bytes[start] ^= 1;
            }
            if defect == 1 {
                bytes[start + 40] ^= 1;
            }
            journal_fixture_write(&path, &bytes);
            FRAME_OWNED_ALLOCATIONS.with(|n| n.set(0));
            let loaded = load_restore_journal(&path, &lease, &f.filesystem()).unwrap();
            assert_eq!(loaded.log_frames, 96);
            assert!(loaded.torn_tail_bytes > 0);
            assert_eq!(FRAME_OWNED_ALLOCATIONS.with(|n| n.get()), 96);
            assert_eq!(fs::read(&path).unwrap(), bytes);
        }
        // Syntax-valid typed-invalid frame97 is still hard quota, as ruled.
        let mut payloads = vec![body; 96];
        payloads.push(b"{}".to_vec());
        let bytes = rewrite_chained_frames(original.len(), &payloads);
        journal_fixture_write(&path, &bytes);
        assert!(matches!(
            load_restore_journal(&path, &lease, &f.filesystem()),
            Err(RecoveryError::RestoreJournalFrameCountQuota {
                frame_index: 97,
                ..
            })
        ));
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }
    #[test]
    fn streaming_first_owned_frame_obeys_saved_memory_when_current_memory_is_larger() {
        let (f, lease, journal) = streaming_fixture();
        let path = journal_path(&journal);
        let original = fs::read(&path).unwrap();
        let frames = journal_frame_ranges(&original);
        for saved in [
            JOURNAL_BOOTSTRAP_BYTES - 1,
            JOURNAL_BOOTSTRAP_BYTES,
            2 * JOURNAL_BOOTSTRAP_BYTES,
        ] {
            let payloads: Vec<Vec<u8>> = frames
                .iter()
                .map(|r| {
                    let mut model: RestoreJournal =
                        serde_json::from_slice(&original[r.start + 72..r.end]).unwrap();
                    model.budget.memory_ceiling = saved;
                    serde_json::to_vec(&model).unwrap()
                })
                .collect();
            let bytes = rewrite_chained_frames(original.len(), &payloads);
            journal_fixture_write(&path, &bytes);
            FRAME_OWNED_ALLOCATIONS.with(|n| n.set(0));
            let filesystem = f.filesystem();
            assert!(filesystem.reconciliation_limits().memory_byte_limit > saved);
            assert!(
                matches!(load_restore_journal(&path,&lease,&filesystem),Err(RecoveryError::Quota{resource,..}) if resource.starts_with("working memory"))
            );
            assert_eq!(FRAME_OWNED_ALLOCATIONS.with(|n| n.get()), 0);
            assert_eq!(fs::read(&path).unwrap(), bytes);
        }
    }
    #[test]
    fn streaming_tiny_saved_memory_precedes_overcap_and_count_quota_without_mutation() {
        let (f, lease, journal) = streaming_fixture();
        let path = journal_path(&journal);
        let original = fs::read(&path).unwrap();
        let frames = journal_frame_ranges(&original);
        let last = frames.last().unwrap();
        let body = original[last.start + 72..last.end].to_vec();
        let mut model: RestoreJournal = serde_json::from_slice(&body).unwrap();
        model.budget.memory_ceiling = 1;
        let tiny = serde_json::to_vec(&model).unwrap();
        let cap = crate::recovery::restore_metadata::Caps::new(journal.journal_capacity, &limits())
            .unwrap()
            .payload as usize;
        let mut oversized = tiny.clone();
        oversized.resize(cap + 1, b' ');
        let mut many = vec![body; 96];
        many.push(tiny);
        for payloads in [vec![oversized], many] {
            let bytes = rewrite_chained_frames(original.len(), &payloads);
            journal_fixture_write(&path, &bytes);
            let error = load_restore_journal(&path, &lease, &f.filesystem()).unwrap_err();
            assert!(
                matches!(error,RecoveryError::Quota{resource,..} if resource.starts_with("working memory"))
            );
            assert!(error.to_string().contains("journal left unchanged"));
            assert!(error.to_string().contains("no repair or unfence attempted"));
            assert_eq!(fs::read(&path).unwrap(), bytes);
            assert!(admission::guard_path(&f.source).exists());
        }
    }
    struct StreamCut {
        event: RecoveryEvent,
        occurrence: usize,
        seen: std::sync::atomic::AtomicUsize,
    }
    impl super::fs::RecoveryIo for StreamCut {
        fn before(&self, event: RecoveryEvent) -> std::io::Result<()> {
            if event == self.event
                && self.seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1
                    == self.occurrence
            {
                return Err(std::io::Error::other("stream cut"));
            }
            Ok(())
        }
    }
    #[test]
    fn streaming_failed_payload_and_durability_do_not_advance_runtime_count_and_torn_retry_repairs()
    {
        let (f, lease, journal) = streaming_fixture();
        let path = journal_path(&journal);
        let original = fs::read(&path).unwrap();
        let filesystem = f.filesystem();
        let lock = existing_operation_lock(&path, &lease, &filesystem, false, true).unwrap();
        for (event, occurrence) in [
            (RecoveryEvent::Write(Member::RestoreJournal), 2),
            (RecoveryEvent::Flush(Member::RestoreJournal), 1),
            (RecoveryEvent::Sync(Directory::RestoreJournalParent), 1),
        ] {
            journal_fixture_write(&path, &original);
            let mut j = load_with_held_lock(&path, &lease, &filesystem, &lock).unwrap();
            let old = (j.log_end, j.log_hash, j.log_frames);
            j.current_attempt += 1;
            j.resume_finalize_attempts += 1;
            let l = limits();
            let dir = Dir::open(&j.staging, &l).unwrap();
            let unit = super::fs::allocation_geometry(&dir, &l).unwrap();
            let io = Arc::new(StreamCut {
                event,
                occurrence,
                seen: std::sync::atomic::AtomicUsize::new(0),
            });
            let control = Arc::new(PublicationIo::new(
                io,
                j.budget.allocation_ceiling,
                j.budget.metadata_bytes,
                j.candidate_bytes,
                unit,
            ));
            assert!(journal_append(&mut j, &control, &l).is_err());
            assert_eq!((j.log_end, j.log_hash, j.log_frames), old);
            let mut loaded = load_with_held_lock(&path, &lease, &filesystem, &lock).unwrap();
            if matches!(event, RecoveryEvent::Write(_)) {
                assert_eq!(loaded.log_frames, old.2);
                assert!(loaded.torn_tail_bytes > 0);
                let control = Arc::new(PublicationIo::new(
                    filesystem.io(),
                    loaded.budget.allocation_ceiling,
                    loaded.budget.metadata_bytes,
                    loaded.candidate_bytes,
                    unit,
                ));
                loaded.current_attempt += 1;
                loaded.resume_finalize_attempts += 1;
                journal_append(&mut loaded, &control, &l).unwrap();
                let repaired = load_with_held_lock(&path, &lease, &filesystem, &lock).unwrap();
                assert_eq!(repaired.log_frames, old.2 + 1);
                assert_eq!(repaired.torn_tail_bytes, 0);
            }
            assert_eq!(fs::metadata(&path).unwrap().len(), original.len() as u64);
        }
        journal_fixture_write(&path, &original);
        let mut j = load_with_held_lock(&path, &lease, &filesystem, &lock).unwrap();
        let old = (j.log_end, j.log_hash, j.log_frames);
        let mut low = limits();
        low.memory_byte_limit = 65535;
        let dir = Dir::open(&j.staging, &limits()).unwrap();
        let unit = super::fs::allocation_geometry(&dir, &limits()).unwrap();
        let control = Arc::new(PublicationIo::new(
            filesystem.io(),
            j.budget.allocation_ceiling,
            j.budget.metadata_bytes,
            j.candidate_bytes,
            unit,
        ));
        assert!(matches!(
            journal_append(&mut j, &control, &low),
            Err(RecoveryError::Quota { .. })
        ));
        assert_eq!((j.log_end, j.log_hash, j.log_frames), old);
        assert_eq!(fs::read(&path).unwrap(), original);
    }

    #[test]
    fn streaming_nonobject_roots_reach_hard_frame_quotas_without_changing_journal_or_guard() {
        let (f, lease, journal) = streaming_fixture();
        let path = journal_path(&journal);
        let original = fs::read(&path).unwrap();
        let guard = admission::guard_path(&f.source);
        let guard_bytes = fs::read(&guard).unwrap();
        let frames = journal_frame_ranges(&original);
        let last = frames.last().unwrap();
        let body = original[last.start + 72..last.end].to_vec();
        let cap = crate::recovery::restore_metadata::Caps::new(journal.journal_capacity, &limits())
            .unwrap()
            .payload as usize;
        for root in [b"[]".as_slice(), b"null", br#""text""#, b"123"] {
            for trailing in [false, true] {
                for count_quota in [false, true] {
                    let mut candidate = root.to_vec();
                    if !count_quota {
                        candidate.resize(cap + 1, b' ');
                    }
                    let mut payloads = if count_quota {
                        vec![body.clone(); 96]
                    } else {
                        vec![]
                    };
                    payloads.push(candidate);
                    if trailing {
                        payloads.push(body.clone());
                    }
                    let bytes = rewrite_chained_frames(original.len(), &payloads);
                    journal_fixture_write(&path, &bytes);
                    FRAME_OWNED_ALLOCATIONS.with(|n| n.set(0));
                    let error = load_restore_journal(&path, &lease, &f.filesystem()).unwrap_err();
                    if count_quota {
                        assert!(matches!(
                            error,
                            RecoveryError::RestoreJournalFrameCountQuota {
                                frame_index: 97,
                                cap: 96,
                                ..
                            }
                        ));
                    } else {
                        assert!(
                            matches!(error,RecoveryError::RestoreJournalFrameQuota{declared_bytes,cap:c,..} if declared_bytes==cap as u64+1 && c==cap as u64)
                        );
                    }
                    assert!(error.to_string().contains(&path.display().to_string()));
                    assert!(error.to_string().contains("no repair or unfence attempted"));
                    assert_eq!(
                        FRAME_OWNED_ALLOCATIONS.with(|n| n.get()),
                        if count_quota { 96 } else { 0 }
                    );
                    assert_eq!(fs::read(&path).unwrap(), bytes);
                    assert_eq!(fs::read(&guard).unwrap(), guard_bytes);
                }
            }
        }
    }
    #[test]
    fn streaming_depth_exhaustion_is_hard_under_over_cap_and_at_97_without_repair_or_unfence() {
        let (f, lease, journal) = streaming_fixture();
        let path = journal_path(&journal);
        let original = fs::read(&path).unwrap();
        let guard = admission::guard_path(&f.source);
        let guard_bytes = fs::read(&guard).unwrap();
        let frames = journal_frame_ranges(&original);
        let last = frames.last().unwrap();
        let body = original[last.start + 72..last.end].to_vec();
        let cap = crate::recovery::restore_metadata::Caps::new(journal.journal_capacity, &limits())
            .unwrap()
            .payload as usize;
        let mut deep = vec![b'['; 33];
        deep.push(b'0');
        deep.extend(std::iter::repeat_n(b']', 33));
        for prefix in [0usize, 96] {
            for oversized in [false, true] {
                for trailing in [false, true] {
                    let mut candidate = deep.clone();
                    if oversized {
                        candidate.resize(cap + 1, b' ');
                    }
                    let mut payloads = vec![body.clone(); prefix];
                    payloads.push(candidate);
                    if trailing {
                        payloads.push(body.clone());
                    }
                    let bytes = rewrite_chained_frames(original.len(), &payloads);
                    journal_fixture_write(&path, &bytes);
                    FRAME_OWNED_ALLOCATIONS.with(|n| n.set(0));
                    let error = load_restore_journal(&path, &lease, &f.filesystem()).unwrap_err();
                    assert!(
                        matches!(error,RecoveryError::Quota{resource,..} if resource=="journal JSON nesting depth exceeds 32; journal left unchanged; no repair or unfence attempted")
                    );
                    assert_eq!(FRAME_OWNED_ALLOCATIONS.with(|n| n.get()), prefix as u64);
                    assert_eq!(fs::read(&path).unwrap(), bytes);
                    assert_eq!(fs::read(&guard).unwrap(), guard_bytes);
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalMode {
    Selected,
    Original,
    OriginalAbsence,
    Abandon,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalEvidenceContext {
    pub mode: TerminalMode,
    pub accepting_lease: admission::OfflineLeaseRequest,
    pub accepting_lease_sha256: [u8; 32],
    pub admission_generation_sha256: [u8; 32],
    pub prepared_report_sha256: Option<[u8; 32]>,
    pub reconciliation_sha256: Option<[u8; 32]>,
    pub accepted_at_unix_ms: u128,
}
#[derive(Serialize)]
struct GenerationMetadata<'a> {
    operation_id: uuid::Uuid,
    destination: &'a Path,
    staging: &'a Path,
    staging_identity: (u64, u64),
    journal_identity: (u64, u64),
    operation_lock_identity: (u64, u64),
    selected_operation: uuid::Uuid,
    selected_manifest_sha256: &'a str,
    selected_profile: &'a super::ValidationProfile,
    selected_dependencies: &'a super::manifest::DependencyManifest,
    candidate_identity: (u64, u64),
    probe_identities: [(u64, u64); 2],
    candidate_bytes: u64,
    candidate_sha256: &'a str,
    selected_sequence: u64,
    selected_logical_sha256: &'a str,
    original: &'a FileBinding,
    original_profile: &'a super::ValidationProfile,
    original_dependencies: &'a super::manifest::DependencyManifest,
    original_verification: &'a Option<super::VerificationReport>,
    original_read_error: &'a Option<String>,
}
fn generation_metadata(j: &RestoreJournal) -> GenerationMetadata<'_> {
    GenerationMetadata {
        operation_id: j.operation_id,
        destination: &j.destination,
        staging: &j.staging,
        staging_identity: j.staging_identity,
        journal_identity: j.journal_identity,
        operation_lock_identity: j.operation_lock_identity,
        selected_operation: j.selected_bundle_operation,
        selected_manifest_sha256: &j.selected_bundle_sha256,
        selected_profile: &j.selected_profile,
        selected_dependencies: &j.selected_dependencies,
        candidate_identity: j.candidate_identity,
        probe_identities: j.probe_identities,
        candidate_bytes: j.candidate_bytes,
        candidate_sha256: &j.candidate_sha256,
        selected_sequence: j.selected_sequence,
        selected_logical_sha256: &j.selected_logical_sha256,
        original: &j.original,
        original_profile: &j.original_profile,
        original_dependencies: &j.original_dependencies,
        original_verification: &j.original_verification,
        original_read_error: &j.original_read_error,
    }
}
fn check_generation_metadata(j: &RestoreJournal, l: &OperationLimits) -> Result<(), RecoveryError> {
    use super::restore_metadata as metadata;
    let caps = metadata::Caps::new(j.journal_capacity, l)?;
    let scoped = caps.scope(l)?;
    let mut shape = metadata::Shape::new(caps, &scoped);
    shape.profile(&j.selected_profile)?;
    shape.profile(&j.original_profile)?;
    shape.dependencies(&j.selected_dependencies.references)?;
    shape.dependencies(&j.original_dependencies.references)?;
    shape.lease(&j.binding)?;
    shape.path(&j.destination)?;
    shape.path(&j.staging)?;
    if let Some(path) = &j.rollback {
        shape.path(path)?;
    }
    for text in [
        &j.selected_bundle_sha256,
        &j.candidate_sha256,
        &j.selected_logical_sha256,
    ] {
        shape.text(text)?;
    }
    if let FileBinding::Readable { sha256, .. } = &j.original {
        shape.text(sha256)?;
    }
    if let Some(text) = &j.original_read_error {
        shape.text(text)?;
    }
    if let Some(report) = &j.original_verification {
        shape.report(report)?;
    }
    if let Some(report) = &j.verification {
        shape.report(report)?;
    }
    let scoped = shape.finish(2, &scoped)?;
    if j.selected_profile.catalog_identity != j.binding.catalog_identity
        || j.original_profile.catalog_identity != j.binding.catalog_identity
        || j.selected_dependencies.references != j.selected_profile.dependencies
        || j.original_dependencies.references != j.original_profile.dependencies
        || verify::profile_digest(&j.selected_profile, &scoped)? != j.binding.profile_sha256
        || metadata::measure(&generation_metadata(j), caps.payload, &scoped)?.1
            != j.generation_metadata_sha256
    {
        return Err(l.verification());
    }
    if let Some(report) = &j.original_verification {
        let FileBinding::Readable {
            identity,
            bytes,
            sha256,
        } = &j.original
        else {
            return Err(l.verification());
        };
        if report.report_version != 1
            || report.profile_version != j.original_profile.version
            || report.image_bytes != *bytes
            || report.image_sha256 != *sha256
            || report.profile_sha256 != verify::profile_digest(&j.original_profile, &scoped)?
            || report.dependency_count != j.original_dependencies.references.len() as u64
            || report.operation_id != j.operation_id.to_string()
            || report
                .copy_evidence
                .as_ref()
                .is_none_or(|copy| copy.verified_file_identity != *identity)
        {
            return Err(l.verification());
        }
    }
    if let Some(report) = &j.verification
        && (report.report_version != 1
            || report.verifier_build_identity != j.binding.build
            || report.profile_version != j.selected_profile.version
            || report.image_bytes != j.candidate_bytes
            || report.image_sha256 != j.candidate_sha256
            || report.published_sequence != j.selected_sequence
            || report.logical_sha256 != j.selected_logical_sha256
            || report.profile_sha256 != j.binding.profile_sha256
            || report.dependency_count != j.selected_dependencies.references.len() as u64
            || report.operation_id != j.operation_id.to_string()
            || report
                .copy_evidence
                .as_ref()
                .is_none_or(|copy| copy.verified_file_identity != j.candidate_identity))
    {
        return Err(l.verification());
    }
    Ok(())
}
fn evidence_digest(
    j: &RestoreJournal,
    mode: TerminalMode,
    cap: u64,
    l: &OperationLimits,
) -> Result<Option<[u8; 32]>, RecoveryError> {
    use super::restore_metadata::measure;
    match mode {
        TerminalMode::Selected | TerminalMode::Original => {
            if j.absence_reconciliation.is_some() {
                return Err(l.verification());
            }
            j.reconciliation
                .as_ref()
                .ok_or_else(|| l.verification())
                .and_then(|r| measure(r, cap, l).map(|r| Some(r.1)))
        }
        TerminalMode::OriginalAbsence => {
            if j.reconciliation.is_some() {
                return Err(l.verification());
            }
            j.absence_reconciliation
                .as_ref()
                .ok_or_else(|| l.verification())
                .and_then(|r| measure(r, cap, l).map(|r| Some(r.1)))
        }
        TerminalMode::Abandon => {
            if j.reconciliation.is_some() || j.absence_reconciliation.is_some() {
                return Err(l.verification());
            }
            Ok(None) // Untouched-original proof is generation_metadata, never effect readiness.
        }
    }
}
fn check_terminal_metadata(
    j: &RestoreJournal,
    old: Option<&RestoreJournal>,
    l: &OperationLimits,
) -> Result<(), RecoveryError> {
    use super::restore_metadata as metadata;
    let caps = metadata::Caps::new(j.journal_capacity, l)?;
    let Some(context) = &j.terminal_evidence_context else {
        // Intent and terminal phases require their durable acceptance context,
        // including Abandoned, whose untouched-original proof has no reconciliation.
        if matches!(
            j.phase,
            RestorePhase::FinalizeIntent
                | RestorePhase::Finalized
                | RestorePhase::FinalizeOriginalIntent
                | RestorePhase::OriginalFinalized
                | RestorePhase::OriginalAbsenceFinalized
                | RestorePhase::Abandoned
        ) || old.is_some_and(|old| old.terminal_evidence_context.is_some())
            || j.reconciliation.is_some()
            || j.absence_reconciliation.is_some()
        {
            return Err(l.verification());
        }
        return Ok(());
    };
    let scoped = caps.scope(l)?;
    let mut shape = metadata::Shape::new(caps, &scoped);
    shape.lease(&context.accepting_lease)?;
    if let Some(record) = &j.reconciliation {
        shape.reconciliation(record)?;
        shape.readiness_scratch(record.dependencies.len())?;
    }
    if let Some(record) = &j.absence_reconciliation {
        shape.absence(record)?;
    }
    let scoped = shape.finish(2, &scoped)?;
    let authority = &context.accepting_lease;
    let authority_file_matches = match context.mode {
        TerminalMode::Selected => {
            matches!(&authority.expected_file, FileBinding::Readable { identity, bytes, sha256 }
            if *identity == j.candidate_identity && *bytes == j.candidate_bytes && *sha256 == j.candidate_sha256)
        }
        TerminalMode::Original | TerminalMode::Abandon => authority.expected_file == j.original,
        TerminalMode::OriginalAbsence => authority.expected_file == FileBinding::Absent,
    };
    let file_context_valid = authority == &j.binding || authority_file_matches;
    let mode_phase = match context.mode {
        TerminalMode::Selected => matches!(
            j.phase,
            RestorePhase::FinalizeIntent | RestorePhase::Finalized
        ),
        TerminalMode::Original => matches!(
            j.phase,
            RestorePhase::FinalizeOriginalIntent | RestorePhase::OriginalFinalized
        ),
        TerminalMode::OriginalAbsence => j.phase == RestorePhase::OriginalAbsenceFinalized,
        TerminalMode::Abandon => j.phase == RestorePhase::Abandoned,
    };
    if !mode_phase
        || !file_context_valid
        || !lease_binding_matches(&j.binding, authority)
        || authority
            .expires_at_unix_ms
            .checked_sub(authority.issued_at_unix_ms)
            .is_none_or(|duration| duration == 0 || duration > 3_600_000)
        || context.accepted_at_unix_ms < authority.issued_at_unix_ms
        || context.accepted_at_unix_ms >= authority.expires_at_unix_ms
        || authority.operator.trim().is_empty()
        || authority.exclusion.evidence.trim().is_empty()
        || !authority.exclusion.legacy_and_cached_handles_excluded
        || !authority.exclusion.external_dispatch_excluded
        || !authority.exclusion.active_owner_excluded
        || metadata::measure(authority, caps.payload, &scoped)?.1 != context.accepting_lease_sha256
        || context.admission_generation_sha256 != j.generation_metadata_sha256
        || j.verification
            .as_ref()
            .map(|r| metadata::measure(r, caps.payload, &scoped).map(|r| r.1))
            .transpose()?
            != context.prepared_report_sha256
        || evidence_digest(j, context.mode, caps.payload, &scoped)? != context.reconciliation_sha256
    {
        return Err(l.verification());
    }
    metadata::measure(
        &(context, &j.reconciliation, &j.absence_reconciliation),
        caps.payload,
        &scoped,
    )?;
    if context.mode == TerminalMode::OriginalAbsence && !matches!(j.original, FileBinding::Absent) {
        return Err(l.verification());
    }
    // Time/operator validation deliberately uses accepting authority, never the
    // CURRENT lease used by the caller to authorize a cleanup retry.
    if let Some(record) = &j.reconciliation {
        if record.operator != authority.operator
            || record.recorded_at_unix_ms < authority.issued_at_unix_ms
            || record.recorded_at_unix_ms > context.accepted_at_unix_ms
        {
            return Err(l.verification());
        }
        let (report, dependencies) = if context.mode == TerminalMode::Selected {
            (
                j.verification.as_ref().ok_or_else(|| l.verification())?,
                &j.selected_dependencies,
            )
        } else {
            (
                j.original_verification
                    .as_ref()
                    .ok_or_else(|| l.verification())?,
                &j.original_dependencies,
            )
        };
        if !super::readiness::assess_readiness(report, dependencies, record)
            .ordinary_admission_ready
        {
            return Err(l.verification());
        }
    }
    if let Some(record) = &j.absence_reconciliation
        && (!matches!(j.original, FileBinding::Absent)
            || record.version != 1
            || record.operation_id != j.operation_id
            || record.candidate_sha256 != j.candidate_sha256
            || !record.no_client_opened_path_while_installed
            || !super::manifest::sha256(&record.evidence_sha256)
            || record.operator != authority.operator
            || record.recorded_at_unix_ms < authority.issued_at_unix_ms
            || record.recorded_at_unix_ms > context.accepted_at_unix_ms)
    {
        return Err(l.verification());
    }
    if old.is_some_and(|old| {
        old.terminal_evidence_context.is_some()
            && (old.reconciliation != j.reconciliation
                || old.absence_reconciliation != j.absence_reconciliation)
    }) {
        return Err(l.verification());
    }
    match old.and_then(|old| old.terminal_evidence_context.as_ref()) {
        Some(previous) if previous == context => Ok(()),
        Some(_) => Err(l.verification()),
        None => {
            let previous = old.ok_or_else(|| l.verification())?;
            let admission_phase = match context.mode {
                TerminalMode::Selected => {
                    previous.phase == RestorePhase::Installed
                        && j.phase == RestorePhase::FinalizeIntent
                }
                TerminalMode::Original => {
                    previous.phase == RestorePhase::RolledBack
                        && j.phase == RestorePhase::FinalizeOriginalIntent
                }
                // Explicit coordinator clarification: these own terminal records
                // are admission intents; no extra phase or fabricated evidence.
                TerminalMode::OriginalAbsence => {
                    previous.phase == RestorePhase::OriginalAbsenceRestored
                        && j.phase == RestorePhase::OriginalAbsenceFinalized
                }
                TerminalMode::Abandon => {
                    matches!(
                        previous.phase,
                        RestorePhase::Admitted
                            | RestorePhase::Prepared
                            | RestorePhase::ExchangeIntent
                    ) && j.phase == RestorePhase::Abandoned
                }
            };
            if admission_phase {
                Ok(())
            } else {
                Err(l.verification())
            }
        }
    }
}

// Full live journal capacities, including accepted authority/evidence, are
// inspected before serialization; Caps also reserves previous/current decode,
// guard/pin, small path/iterator and fixed serializer/hash/parser scratch scopes.
fn journal_metadata_scope(
    j: &RestoreJournal,
    l: &OperationLimits,
) -> Result<OperationLimits, RecoveryError> {
    use super::restore_metadata::{Caps, Shape};
    let mut effective = l.clone();
    effective.memory_byte_limit = effective.memory_byte_limit.min(j.budget.memory_ceiling);
    let caps = Caps::new(j.journal_capacity, &effective)?;
    let scoped = caps.scope(&effective)?;
    let mut shape = Shape::new(caps, &scoped);
    shape.profile(&j.selected_profile)?;
    shape.profile(&j.original_profile)?;
    shape.dependencies(&j.selected_dependencies.references)?;
    shape.dependencies(&j.original_dependencies.references)?;
    shape.lease(&j.binding)?;
    shape.path(&j.destination)?;
    shape.path(&j.staging)?;
    if let Some(path) = &j.rollback {
        shape.path(path)?;
    }
    for text in [
        &j.selected_bundle_sha256,
        &j.candidate_sha256,
        &j.selected_logical_sha256,
    ] {
        shape.text(text)?;
    }
    if let FileBinding::Readable { sha256, .. } = &j.original {
        shape.text(sha256)?;
    }
    if let Some(text) = &j.original_read_error {
        shape.text(text)?;
    }
    for report in [&j.original_verification, &j.verification]
        .into_iter()
        .flatten()
    {
        shape.report(report)?;
    }
    if let Some(context) = &j.terminal_evidence_context {
        shape.lease(&context.accepting_lease)?;
    }
    if let Some(record) = &j.reconciliation {
        shape.reconciliation(record)?;
        shape.readiness_scratch(record.dependencies.len())?;
    }
    if let Some(record) = &j.absence_reconciliation {
        shape.absence(record)?;
    }
    shape.finish(1, &scoped)
}

// A fixed scratch writer; neither serialization pass owns the payload. Its
// exact range is admitted before even the header write. A short/failed write
// cannot advance the durable frame count/hash/end.
struct JournalStream<'a> {
    file: &'a std::fs::File,
    control: &'a PublicationIo,
    limits: &'a OperationLimits,
    start: u64,
    expected: u64,
    emitted: u64,
    flushed: u64,
    used: usize,
    scratch: [u8; 65536],
    hash: Sha256,
    failure: Option<RecoveryError>,
}
impl JournalStream<'_> {
    fn drain(&mut self) -> Result<(), RecoveryError> {
        self.limits.check()?;
        if self.used != 0 {
            self.control.write_all(
                self.file,
                &self.scratch[..self.used],
                self.start
                    .checked_add(self.flushed)
                    .ok_or_else(|| self.limits.verification())?,
                Member::RestoreJournal,
                self.limits,
            )?;
            self.flushed += self.used as u64;
            self.used = 0;
        }
        Ok(())
    }
    fn accept(&mut self, mut bytes: &[u8]) -> Result<usize, RecoveryError> {
        self.limits.check()?;
        let length = bytes.len();
        let end = self
            .emitted
            .checked_add(length as u64)
            .filter(|n| *n <= self.expected)
            .ok_or_else(|| self.limits.verification())?;
        self.hash.update(bytes);
        while !bytes.is_empty() {
            let n = bytes.len().min(self.scratch.len() - self.used);
            self.scratch[self.used..self.used + n].copy_from_slice(&bytes[..n]);
            self.used += n;
            bytes = &bytes[n..];
            if self.used == self.scratch.len() {
                self.drain()?;
            }
        }
        self.emitted = end;
        Ok(length)
    }
}
impl std::io::Write for JournalStream<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        match self.accept(bytes) {
            Ok(n) => Ok(n),
            Err(error) => {
                self.failure = Some(error);
                Err(std::io::Error::other("restore journal stream"))
            }
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self.drain() {
            Ok(()) => Ok(()),
            Err(error) => {
                self.failure = Some(error);
                Err(std::io::Error::other("restore journal stream"))
            }
        }
    }
}

const JOURNAL_BOOTSTRAP_BYTES: u64 = 2 * 65536 + LOG_HEADER;

fn frame_hash(
    file: &std::fs::File,
    start: u64,
    end: u64,
    previous: [u8; 32],
    l: &OperationLimits,
) -> Result<[u8; 32], RecoveryError> {
    l.admit_memory(JOURNAL_BOOTSTRAP_BYTES)?;
    let mut scratch = [0; 65536];
    let mut hash = Sha256::new();
    hash.update(previous);
    let mut position = start;
    while position < end {
        l.check()?;
        let n = (end - position).min(scratch.len() as u64) as usize;
        fs::read_exact_at(file, &mut scratch[..n], position, l)?;
        hash.update(&scratch[..n]);
        position += n as u64;
    }
    Ok(hash.finalize().into())
}

// Bootstrap uses fixed stack scratch only, not serde's growable reader string
// scratch. Full RFC8259 syntax, UTF8, escapes, depth, trailing bytes, numeric
// grammar and unique real budget.memory_ceiling are checked before owned decode.
// Strings/keys are never accumulated. The only key retention is a fixed14-byte
// ASCII matcher (escaped Unicode equivalent keys are recognized).
struct JournalJson<'a> {
    file: &'a std::fs::File,
    l: &'a OperationLimits,
    position: u64,
    end: u64,
    used: usize,
    available: usize,
    scratch: [u8; 4096],
    budget_seen: bool,
    memory_seen: bool,
    ambiguous_budget: bool,
    memory: Option<u64>,
}
impl JournalJson<'_> {
    fn peek(&mut self) -> Result<Option<u8>, RecoveryError> {
        self.l.check()?;
        if self.used == self.available {
            if self.position == self.end {
                return Ok(None);
            }
            self.available = (self.end - self.position).min(self.scratch.len() as u64) as usize;
            fs::read_exact_at(
                self.file,
                &mut self.scratch[..self.available],
                self.position,
                self.l,
            )?;
            self.position += self.available as u64;
            self.used = 0;
        }
        Ok(Some(self.scratch[self.used]))
    }
    fn take(&mut self) -> Result<u8, RecoveryError> {
        let byte = self.peek()?.ok_or_else(|| self.l.verification())?;
        self.used += 1;
        Ok(byte)
    }
    fn expect(&mut self, byte: u8) -> Result<(), RecoveryError> {
        if self.take()? != byte {
            return Err(self.l.verification());
        }
        Ok(())
    }
    fn ws(&mut self) -> Result<(), RecoveryError> {
        while matches!(self.peek()?, Some(b' ' | b'\n' | b'\r' | b'\t')) {
            self.take()?;
        }
        Ok(())
    }
    fn hex(&mut self) -> Result<u16, RecoveryError> {
        let mut n = 0;
        for _ in 0..4 {
            let b = self.take()?;
            n = (n << 4)
                | match b {
                    b'0'..=b'9' => (b - b'0') as u16,
                    b'a'..=b'f' => (b - b'a' + 10) as u16,
                    b'A'..=b'F' => (b - b'A' + 10) as u16,
                    _ => return Err(self.l.verification()),
                };
        }
        Ok(n)
    }
    fn string(&mut self) -> Result<([u8; 14], usize), RecoveryError> {
        self.expect(b'"')?;
        let mut key = [0; 14];
        let mut count = 0usize;
        loop {
            let b = self.take()?;
            if b == b'"' {
                return Ok((key, count));
            }
            let scalar = match b {
                0..=31 => return Err(self.l.verification()),
                b'\\' => match self.take()? {
                    b'"' => 34,
                    b'\\' => 92,
                    b'/' => 47,
                    b'b' => 8,
                    b'f' => 12,
                    b'n' => 10,
                    b'r' => 13,
                    b't' => 9,
                    b'u' => {
                        let high = self.hex()?;
                        match high {
                            0xD800..=0xDBFF => {
                                self.expect(b'\\')?;
                                self.expect(b'u')?;
                                let low = self.hex()?;
                                if !(0xDC00..=0xDFFF).contains(&low) {
                                    return Err(self.l.verification());
                                }
                                0x10000 + (((high as u32 - 0xD800) << 10) | (low as u32 - 0xDC00))
                            }
                            0xDC00..=0xDFFF => return Err(self.l.verification()),
                            _ => high as u32,
                        }
                    }
                    _ => return Err(self.l.verification()),
                },
                32..=127 => b as u32,
                _ => {
                    let length = match b {
                        0xC2..=0xDF => 2,
                        0xE0..=0xEF => 3,
                        0xF0..=0xF4 => 4,
                        _ => return Err(self.l.verification()),
                    };
                    let mut utf8 = [0; 4];
                    utf8[0] = b;
                    for byte in utf8.iter_mut().take(length).skip(1) {
                        *byte = self.take()?;
                    }
                    let text =
                        std::str::from_utf8(&utf8[..length]).map_err(|_| self.l.verification())?;
                    text.chars().next().ok_or_else(|| self.l.verification())? as u32
                }
            };
            if count < key.len() {
                key[count] = if scalar < 128 { scalar as u8 } else { 255 };
            }
            count = count.saturating_add(1);
        }
    }
    fn number(&mut self) -> Result<Option<u64>, RecoveryError> {
        let negative = self.peek()? == Some(b'-');
        if negative {
            self.take()?;
        }
        let first = self.take()?;
        let mut value = match first {
            b'0'..=b'9' => Some((first - b'0') as u64),
            _ => return Err(self.l.verification()),
        };
        if first == b'0' && matches!(self.peek()?, Some(b'0'..=b'9')) {
            return Err(self.l.verification());
        }
        while matches!(self.peek()?, Some(b'0'..=b'9')) {
            let b = self.take()?;
            value = value
                .and_then(|v| v.checked_mul(10))
                .and_then(|v| v.checked_add((b - b'0') as u64));
        }
        let mut integer = !negative;
        if self.peek()? == Some(b'.') {
            integer = false;
            self.take()?;
            if !matches!(self.peek()?, Some(b'0'..=b'9')) {
                return Err(self.l.verification());
            }
            while matches!(self.peek()?, Some(b'0'..=b'9')) {
                self.take()?;
            }
        }
        if matches!(self.peek()?, Some(b'e' | b'E')) {
            integer = false;
            self.take()?;
            if matches!(self.peek()?, Some(b'+' | b'-')) {
                self.take()?;
            }
            if !matches!(self.peek()?, Some(b'0'..=b'9')) {
                return Err(self.l.verification());
            }
            while matches!(self.peek()?, Some(b'0'..=b'9')) {
                self.take()?;
            }
        }
        Ok(if integer { value } else { None })
    }
    fn value(&mut self, depth: usize, context: u8) -> Result<(), RecoveryError> {
        if depth > 32 {
            // Fixed-stack exhaustion is a hard refusal, never a torn-tail signal.
            return Err(RecoveryError::Quota {
                operation_id: self.l.operation_id,
                resource: "journal JSON nesting depth exceeds 32; journal left unchanged; no repair or unfence attempted",
            });
        }
        self.ws()?;
        match self.peek()? {
            Some(b'{') => {
                self.take()?;
                self.ws()?;
                if self.peek()? == Some(b'}') {
                    self.take()?;
                    return Ok(());
                }
                loop {
                    let (key, n) = self.string()?;
                    self.ws()?;
                    self.expect(b':')?;
                    self.ws()?;
                    if context == 0 && n == 6 && &key[..6] == b"budget" {
                        if self.budget_seen {
                            self.ambiguous_budget = true;
                        }
                        self.budget_seen = true;
                        if self.peek()? == Some(b'{') {
                            self.value(depth + 1, 1)?;
                        } else {
                            self.ambiguous_budget = true;
                            self.value(depth + 1, 2)?;
                        }
                    } else if context == 1 && n == 14 && &key == b"memory_ceiling" {
                        if self.memory_seen {
                            self.ambiguous_budget = true;
                        }
                        self.memory_seen = true;
                        if matches!(self.peek()?, Some(b'-' | b'0'..=b'9')) {
                            self.memory = self.number()?.filter(|n| *n != 0);
                            if self.memory.is_none() {
                                self.ambiguous_budget = true;
                            }
                        } else {
                            self.ambiguous_budget = true;
                            self.value(depth + 1, 2)?;
                        }
                    } else {
                        self.value(depth + 1, 2)?;
                    }
                    self.ws()?;
                    match self.take()? {
                        b'}' => break,
                        b',' => {
                            self.ws()?;
                        }
                        _ => return Err(self.l.verification()),
                    }
                }
            }
            Some(b'[') => {
                self.take()?;
                self.ws()?;
                if self.peek()? == Some(b']') {
                    self.take()?;
                    return Ok(());
                }
                loop {
                    self.value(depth + 1, 2)?;
                    self.ws()?;
                    match self.take()? {
                        b']' => break,
                        b',' => {}
                        _ => return Err(self.l.verification()),
                    }
                }
            }
            Some(b'"') => {
                self.string()?;
            }
            Some(b't') => {
                for b in b"true" {
                    self.expect(*b)?;
                }
            }
            Some(b'f') => {
                for b in b"false" {
                    self.expect(*b)?;
                }
            }
            Some(b'n') => {
                for b in b"null" {
                    self.expect(*b)?;
                }
            }
            Some(b'-' | b'0'..=b'9') => {
                self.number()?;
            }
            _ => return Err(self.l.verification()),
        }
        Ok(())
    }
}
fn saved_memory_ceiling(
    file: &std::fs::File,
    start: u64,
    end: u64,
    l: &OperationLimits,
) -> Result<Option<u64>, RecoveryError> {
    // Fixed bootstrap scratch is admitted under current M before any frame Vec.
    // Once known, the SAME reservation is checked against min(current M,saved M).
    l.admit_memory(JOURNAL_BOOTSTRAP_BYTES)?;
    let mut json = JournalJson {
        file,
        l,
        position: start,
        end,
        used: 0,
        available: 0,
        scratch: [0; 4096],
        budget_seen: false,
        memory_seen: false,
        ambiguous_budget: false,
        memory: None,
    };
    json.ws()?;
    // Syntax-valid nonobject roots still reach authenticated cap/count refusal.
    // Only a root object can expose the real journal budget; nested decoys cannot.
    let context = if json.peek()? == Some(b'{') { 0 } else { 2 };
    json.value(0, context)?;
    json.ws()?;
    if json.peek()?.is_some() {
        return Err(l.verification());
    }
    Ok(if json.ambiguous_budget {
        None
    } else {
        json.memory
    })
}

#[cfg(test)]
thread_local! { static FRAME_OWNED_ALLOCATIONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) }; }

fn journal_memory_refusal(error: RecoveryError) -> RecoveryError {
    match error {
        RecoveryError::Quota {
            operation_id,
            resource: "working memory",
        } => RecoveryError::Quota {
            operation_id,
            resource: "working memory; journal left unchanged; no repair or unfence attempted",
        },
        error => error,
    }
}

// INERT unit fixture/tests: apply AFTER bounds/evidence/immutable-view and
// separately reviewed streaming append+terminal integration. No filesystem I/O.
#[cfg(test)]
mod metadata_tests {
    use super::*;
    use crate::recovery::CancelToken;
    use crate::recovery::{readiness::*, restore_metadata as metadata};
    use std::time::{Duration, Instant};
    fn limits() -> OperationLimits {
        OperationLimits::new(
            Instant::now() + Duration::from_secs(30),
            128 << 20,
            512 << 20,
            CancelToken::new(),
        )
    }
    fn fixture() -> RestoreJournal {
        let l = limits();
        let mut j: RestoreJournal = serde_json::from_str(FIXTURE).unwrap();
        j.binding.profile_sha256 = verify::profile_digest(&j.selected_profile, &l).unwrap();
        j.verification.as_mut().unwrap().profile_sha256 = j.binding.profile_sha256.clone();
        j.original_verification.as_mut().unwrap().profile_sha256 =
            verify::profile_digest(&j.original_profile, &l).unwrap();
        let caps = metadata::Caps::new(j.journal_capacity, &l).unwrap();
        j.generation_metadata_sha256 =
            metadata::measure(&generation_metadata(&j), caps.payload, &l)
                .unwrap()
                .1;
        j
    }
    #[test]
    fn terminal_reservation_persisted_candidate_build_must_equal_admission() {
        let l = limits();
        let mut j = fixture();
        check_generation_metadata(&j, &l).unwrap();
        j.verification
            .as_mut()
            .unwrap()
            .verifier_build_identity
            .package_version
            .push_str("different producer");
        assert!(check_generation_metadata(&j, &l).is_err());
    }
    #[test]
    fn terminal_reservation_original_report_keeps_its_independent_build() {
        let l = limits();
        let mut j = fixture();
        j.original_verification
            .as_mut()
            .unwrap()
            .verifier_build_identity
            .package_version = "old producer \"\n".repeat(128);
        j.generation_metadata_sha256 = metadata::measure(
            &generation_metadata(&j),
            metadata::Caps::new(j.journal_capacity, &l).unwrap().payload,
            &l,
        )
        .unwrap()
        .1;
        check_generation_metadata(&j, &l).unwrap();
    }
    #[test]
    fn terminal_reservation_report_schema_has_a_complete_encoded_build_bound() {
        let l = limits();
        let mut j = fixture();
        let expected = TerminalCaps {
            operator: 128,
            lease: 1452,
            evidence: 1613,
        };
        assert_eq!(terminal_caps(&j, &l).unwrap(), expected);
        j.phase = RestorePhase::OriginalAbsenceFinalized;
        j.resume_finalize_attempts = u32::MAX;
        j.rollback_attempts = u32::MAX;
        j.current_attempt = u32::MAX;
        j.attempt_deadline_unix_ms = u128::MAX;
        j.verification = None;
        assert_eq!(terminal_caps(&j, &l).unwrap(), expected);
        let old_build_bytes = metadata::measure(&j.binding.build, 10168, &l).unwrap().0;
        j.binding.build.package_version = "escaped \"\n".repeat(8);
        let build_growth =
            metadata::measure(&j.binding.build, 10168, &l).unwrap().0 - old_build_bytes;
        let expanded = terminal_caps(&j, &l).unwrap();
        assert_eq!(expanded.lease, expected.lease + build_growth);
        assert_eq!(expanded.evidence, expected.evidence - 3 * build_growth);
        let mut report = fixture().verification.unwrap();
        report.report_version = u32::MAX;
        report.profile_version = u32::MAX;
        report.canonical_encoding_version = u32::MAX;
        report.image_bytes = u64::MAX;
        report.published_sequence = u64::MAX;
        report.verified_collection_count = u64::MAX;
        report.unknown_collection_count = u64::MAX;
        report.query_probe_count = u64::MAX;
        report.exception_count = u64::MAX;
        report.dependency_count = u64::MAX;
        report.readiness = super::super::manifest::Readiness::DependenciesRequireReconciliation;
        let copy = report.copy_evidence.as_mut().unwrap();
        copy.verified_file_identity = (u64::MAX, u64::MAX);
        copy.source_image_identity = Some((u64::MAX, u64::MAX));
        report.verifier_build_identity.package_version = "escaped \"\n".repeat(64);
        let build = metadata::measure(&report.verifier_build_identity, 10168, &l)
            .unwrap()
            .0;
        assert_eq!(
            metadata::measure(&report, 10168, &l).unwrap().0,
            1265 + build
        );
        assert_eq!(
            metadata::measure(&maximum_report(&report.verifier_build_identity), 10168, &l)
                .unwrap()
                .0,
            1265 + build
        );
        let context = TerminalEvidenceContext {
            mode: TerminalMode::OriginalAbsence,
            accepting_lease: j.binding.clone(),
            accepting_lease_sha256: [255; 32],
            admission_generation_sha256: [255; 32],
            prepared_report_sha256: Some([255; 32]),
            reconciliation_sha256: Some([255; 32]),
            accepted_at_unix_ms: u128::MAX,
        };
        let envelope = metadata::measure(&context, 10168, &l).unwrap().0
            - metadata::measure(&context.accepting_lease, 10168, &l)
                .unwrap()
                .0;
        assert_eq!(envelope, 732);
        assert!(envelope <= TERMINAL_CONTEXT_ENVELOPE_MAX);
    }
    fn encoded_string(bytes: u64) -> String {
        let mut value = "\u{1}".repeat(((bytes - 2) / 6) as usize);
        value.push_str(&"x".repeat(((bytes - 2) % 6) as usize));
        assert_eq!(serde_json::to_vec(&value).unwrap().len() as u64, bytes);
        value
    }
    #[test]
    fn terminal_reservation_accepting_fields_are_independent_of_admission_lengths() {
        let l = limits();
        let mut j = fixture();
        let initial = terminal_caps(&j, &l).unwrap();
        j.binding.operator = encoded_string(129);
        j.binding.exclusion.evidence = encoded_string(257);
        let expanded = terminal_caps(&j, &l).unwrap();
        assert_eq!(expanded.operator, initial.operator);
        assert_eq!(expanded.lease, initial.lease);
        assert!(expanded.evidence < initial.evidence); // Immutable original binding remains.
        check_terminal_reservation(&j, &l).unwrap();
    }
    #[test]
    fn terminal_reservation_admission_reserves_reconciliation_operator_occurrence() {
        let l = limits();
        let mut j = fixture();
        // At E=1,150 the 1,095-byte admission-operator record fits, but
        // the 1,205-byte maximum-accepting-operator record cannot fit.
        j.journal_capacity -= (1613 - 1150) * metadata::MAX_FRAMES;
        j.budget.metadata_payload_ceiling = j.journal_capacity + GUARD_ALLOWANCE;
        let caps = terminal_caps(&j, &l).unwrap();
        assert_eq!(caps.evidence, 1150);
        let mut record = selected_record(&j);
        record.recorded_at_unix_ms = u128::MAX;
        record.affected_scopes = vec![j.selected_profile.catalog_identity.clone()];
        record.index = IndexReadiness::Degraded;
        let admission_bytes = serde_json::to_vec(&record).unwrap().len() as u64;
        assert_eq!(admission_bytes, 1095);
        assert!(admission_bytes <= caps.evidence);
        record.operator = encoded_string(ACCEPTING_OPERATOR_ENCODED_MAX);
        let accepting_bytes = serde_json::to_vec(&record).unwrap().len() as u64;
        assert_eq!(accepting_bytes, 1205);
        assert!(accepting_bytes > caps.evidence);
        let error = check_terminal_reservation(&j, &l).unwrap_err();
        let RecoveryError::RestoreMetadataReservationQuota {
            operation_id,
            offered_bytes,
            required_bytes,
            ..
        } = &error
        else {
            panic!("fixed accepting operator must exceed the reservation: {error}");
        };
        assert_eq!(*operation_id, l.operation_id);
        assert_eq!(*offered_bytes, j.budget.metadata_payload_ceiling);
        assert_eq!(
            *required_bytes,
            offered_bytes + (accepting_bytes - caps.evidence) * metadata::MAX_FRAMES
        );
        assert!(required_bytes > offered_bytes);
        let diagnostic = error.to_string();
        assert!(diagnostic.contains("metadata_byte_limit"));
        assert!(diagnostic.contains(&offered_bytes.to_string()));
        assert!(diagnostic.contains(&required_bytes.to_string()));
        assert!(diagnostic.contains("at least") && diagnostic.contains("headroom"));
        eprintln!(
            "operator reservation: admission={admission_bytes} accepting={accepting_bytes} E={} offered={offered_bytes} required={required_bytes}",
            caps.evidence
        );
    }
    #[test]
    fn terminal_reservation_maximum_accepting_fields_fit_every_terminal_mode() {
        let l = limits();
        for mode in [
            TerminalMode::Selected,
            TerminalMode::Original,
            TerminalMode::OriginalAbsence,
            TerminalMode::Abandon,
        ] {
            let mut j = fixture();
            let cap = metadata::Caps::new(j.journal_capacity, &l).unwrap().payload;
            let reserved = terminal_caps(&j, &l).unwrap();
            let mut authority = j.binding.clone();
            authority.operator = encoded_string(128);
            authority.exclusion.evidence = encoded_string(256);
            authority.issued_at_unix_ms = u128::MAX;
            authority.expires_at_unix_ms = u128::MAX;
            authority.expected_file = FileBinding::Readable {
                identity: (u64::MAX, u64::MAX),
                bytes: u64::MAX,
                sha256: RESERVATION_SHA.into(),
            };
            authority.exclusion.legacy_and_cached_handles_excluded = false;
            authority.exclusion.external_dispatch_excluded = false;
            authority.exclusion.active_owner_excluded = false;
            assert_eq!(
                serde_json::to_vec(&authority).unwrap().len() as u64,
                reserved.lease
            );
            // Deliberately impossible combined maxima dominate every valid
            // report/lease. This is a size proof, not an accepted context.
            j.verification = Some(
                serde_json::from_slice(
                    &serde_json::to_vec(&maximum_report(&j.binding.build)).unwrap(),
                )
                .unwrap(),
            );
            j.phase = match mode {
                TerminalMode::Selected => RestorePhase::Finalized,
                TerminalMode::Original => RestorePhase::OriginalFinalized,
                TerminalMode::OriginalAbsence => RestorePhase::OriginalAbsenceFinalized,
                TerminalMode::Abandon => RestorePhase::Abandoned,
            };
            j.resume_finalize_attempts = u32::MAX;
            j.rollback_attempts = u32::MAX;
            j.current_attempt = u32::MAX;
            j.attempt_deadline_unix_ms = u128::MAX;
            j.generation_metadata_sha256 = [255; 32];
            if matches!(mode, TerminalMode::Selected | TerminalMode::Original) {
                let mut record = selected_record(&j);
                record.operator = authority.operator.clone();
                record.recorded_at_unix_ms = u128::MAX;
                let n = serde_json::to_vec(&record).unwrap().len() as u64;
                record.affected_scopes[0].push_str(&"x".repeat((reserved.evidence - n) as usize));
                assert_eq!(
                    serde_json::to_vec(&record).unwrap().len() as u64,
                    reserved.evidence
                );
                j.reconciliation = Some(record);
            } else if matches!(mode, TerminalMode::OriginalAbsence) {
                j.absence_reconciliation = Some(AbsenceReconciliationRecord {
                    version: u32::MAX,
                    operation_id: j.operation_id,
                    candidate_sha256: j.candidate_sha256.clone(),
                    operator: authority.operator.clone(),
                    recorded_at_unix_ms: u128::MAX,
                    no_client_opened_path_while_installed: false,
                    evidence_sha256: RESERVATION_SHA.into(),
                });
            }
            j.terminal_evidence_context = Some(TerminalEvidenceContext {
                mode,
                accepting_lease: authority,
                accepting_lease_sha256: [255; 32],
                admission_generation_sha256: [255; 32],
                prepared_report_sha256: Some([255; 32]),
                reconciliation_sha256: Some([255; 32]),
                accepted_at_unix_ms: u128::MAX,
            });
            let bytes = serde_json::to_vec(&j).unwrap().len() as u64;
            assert!(bytes <= cap, "{mode:?}: {bytes}>{cap}");
        }
    }
    fn selected_record(j: &RestoreJournal) -> ReconciliationRecord {
        let dep = &j.selected_dependencies.references[0];
        ReconciliationRecord {
            version: 1,
            operation_id: j.operation_id,
            image_sha256: j.candidate_sha256.clone(),
            published_sequence: j.selected_sequence,
            operator: j.binding.operator.clone(),
            recorded_at_unix_ms: 1400,
            affected_scopes: vec!["fixture store runtime".into()],
            identity_confirmed: true,
            reference_closure_confirmed: true,
            external_effect_coverage_confirmed: true,
            source_publication_resolution_sha256: "1".repeat(64),
            clients: ClientReconciliation {
                clients_invalidated: true,
                rpc_sessions_restarted: true,
                transactions_drained: true,
                caches_invalidated: true,
                retry_assumptions_reconciled: true,
                evidence_sha256: "2".repeat(64),
            },
            dependencies: vec![DependencyResolution {
                reference_sha256: dep.reference_sha256.clone(),
                catalog_identity: dep.catalog_identity.clone(),
                observed_sequence: dep.required_sequence.unwrap(),
                evidence_sha256: "3".repeat(64),
            }],
            external_outcomes: vec![],
            unresolved_items: vec![],
            index: IndexReadiness::Lexical,
        }
    }
    fn admit(j: &mut RestoreJournal, mode: TerminalMode) {
        let l = limits();
        let caps = metadata::Caps::new(j.journal_capacity, &l).unwrap();
        let context = TerminalEvidenceContext {
            mode,
            accepting_lease: j.binding.clone(),
            accepting_lease_sha256: metadata::measure(&j.binding, caps.payload, &l).unwrap().1,
            admission_generation_sha256: j.generation_metadata_sha256,
            prepared_report_sha256: j
                .verification
                .as_ref()
                .map(|v| metadata::measure(v, caps.payload, &l).unwrap().1),
            reconciliation_sha256: evidence_digest(j, mode, caps.payload, &l).unwrap(),
            accepted_at_unix_ms: 1500,
        };
        j.terminal_evidence_context = Some(context);
    }
    #[test]
    fn every_terminal_intent_and_terminal_phase_requires_acceptance_context() {
        let l = limits();
        let old = fixture();
        for phase in [
            RestorePhase::FinalizeIntent,
            RestorePhase::Finalized,
            RestorePhase::FinalizeOriginalIntent,
            RestorePhase::OriginalFinalized,
            RestorePhase::OriginalAbsenceFinalized,
            RestorePhase::Abandoned,
        ] {
            let mut j = old.clone();
            j.phase = phase.clone();
            assert!(j.terminal_evidence_context.is_none());
            assert!(j.reconciliation.is_none());
            assert!(j.absence_reconciliation.is_none());
            // No previous frame and a previous frame with no context must both
            // refuse. Abandoned intentionally has no reconciliation to trigger
            // a different rejection branch.
            for previous in [None, Some(&old)] {
                assert!(
                    matches!(
                        check_terminal_metadata(&j, previous, &l),
                        Err(RecoveryError::Verification { .. })
                    ),
                    "missing context for {phase:?}"
                );
            }
        }
        // Metadata acceptance remains optional in every nonterminal phase.
        for phase in [
            RestorePhase::Admitted,
            RestorePhase::Prepared,
            RestorePhase::ExchangeIntent,
            RestorePhase::RelocateIntent,
            RestorePhase::Installed,
            RestorePhase::ReverseIntent,
            RestorePhase::RolledBack,
            RestorePhase::OriginalAbsenceRestored,
        ] {
            let mut j = old.clone();
            j.phase = phase.clone();
            for previous in [None, Some(&old)] {
                check_terminal_metadata(&j, previous, &l)
                    .unwrap_or_else(|error| panic!("nonterminal {phase:?}: {error}"));
            }
        }
    }
    #[test]
    fn selected_and_original_evidence_have_immutable_distinct_generation_bindings() {
        let l = limits();
        let original = fixture();
        check_generation_metadata(&original, &l).unwrap();
        let mut j = original.clone();
        j.selected_dependencies.references[0]
            .catalog_identity
            .push('x');
        assert!(check_generation_metadata(&j, &l).is_err());
        let mut j = original.clone();
        j.original_profile = j.selected_profile.clone();
        assert!(check_generation_metadata(&j, &l).is_err());
        let mut j = original.clone();
        j.probe_identities[0].1 += 1;
        assert_ne!(
            immutable_digest(&original, &l).unwrap(),
            immutable_digest(&j, &l).unwrap()
        );
        let mut j = original.clone();
        j.original_verification.as_mut().unwrap().profile_sha256 = j.binding.profile_sha256.clone();
        assert!(check_generation_metadata(&j, &l).is_err());
    }
    #[test]
    fn terminal_evidence_acceptance_is_immutable_across_phase_and_current_lease_changes() {
        let l = limits();
        let old = fixture();
        let mut intent = old.clone();
        intent.phase = RestorePhase::FinalizeIntent;
        intent.reconciliation = Some(selected_record(&intent));
        admit(&mut intent, TerminalMode::Selected);
        check_terminal_metadata(&intent, Some(&old), &l).unwrap();
        let mut terminal = intent.clone();
        terminal.phase = RestorePhase::Finalized;
        check_terminal_metadata(&terminal, Some(&intent), &l).unwrap();
        let mut renewed = terminal.binding.clone();
        renewed.issued_at_unix_ms = 2100;
        renewed.expires_at_unix_ms = 3100;
        assert!(lease_binding_matches(&terminal.binding, &renewed));
        // Stored evidence predates current lease B. This immutable check takes
        // ONLY stored context and cannot accidentally compare against B's time.
        assert!(
            terminal
                .reconciliation
                .as_ref()
                .unwrap()
                .recorded_at_unix_ms
                < renewed.issued_at_unix_ms
        );
        check_terminal_metadata(&terminal, Some(&intent), &l).unwrap();
        for variant in 0..5 {
            let mut bad = terminal.clone();
            match variant {
                0 => {
                    bad.terminal_evidence_context
                        .as_mut()
                        .unwrap()
                        .accepting_lease
                        .issued_at_unix_ms = 2100
                }
                1 => {
                    bad.terminal_evidence_context
                        .as_mut()
                        .unwrap()
                        .admission_generation_sha256[0] ^= 1
                }
                2 => bad.reconciliation.as_mut().unwrap().operator.push('x'),
                3 => bad.terminal_evidence_context = None,
                _ => bad.phase = RestorePhase::RolledBack,
            }
            assert!(
                check_terminal_metadata(&bad, Some(&intent), &l).is_err(),
                "mutation{variant}"
            );
        }
        let mut bad = intent.clone();
        bad.phase = RestorePhase::FinalizeIntent;
        let mut wrong_old = old.clone();
        wrong_old.phase = RestorePhase::ReverseIntent;
        assert!(check_terminal_metadata(&bad, Some(&wrong_old), &l).is_err());
    }
    #[test]
    fn rollback_finalize_uses_original_dependencies_without_selected_fallback() {
        let l = limits();
        let mut old = fixture();
        old.phase = RestorePhase::RolledBack;
        let report = old.original_verification.as_ref().unwrap();
        let mut record = selected_record(&old);
        record.image_sha256 = report.image_sha256.clone();
        record.published_sequence = report.published_sequence;
        record.dependencies.clear();
        let mut intent = old.clone();
        intent.phase = RestorePhase::FinalizeOriginalIntent;
        intent.reconciliation = Some(record);
        admit(&mut intent, TerminalMode::Original);
        check_terminal_metadata(&intent, Some(&old), &l).unwrap();
        // Selected generation declares one different dependency; it is not
        // evidence for this original generation which declared none.
        let mut bad = intent.clone();
        bad.original_dependencies = bad.selected_dependencies.clone();
        assert!(check_generation_metadata(&bad, &l).is_err());
        assert!(check_terminal_metadata(&bad, Some(&old), &l).is_err());
    }
    #[test]
    fn representative_terminal_frames_fit_the_default_one_mib_reservation() {
        let l = limits();
        let old = fixture();
        let mut intent = old.clone();
        intent.phase = RestorePhase::FinalizeIntent;
        intent.reconciliation = Some(selected_record(&intent));
        admit(&mut intent, TerminalMode::Selected);
        let caps = metadata::Caps::new(intent.journal_capacity, &l).unwrap();
        let bytes = metadata::measure(&intent, caps.payload, &l).unwrap().0;
        assert!(
            bytes <= caps.payload,
            "representative selected terminal frame bytes{bytes} cap{}",
            caps.payload
        );
        assert!((bytes + metadata::FRAME_HEADER) * metadata::MAX_FRAMES <= intent.journal_capacity);
        let mut original = old.clone();
        original.phase = RestorePhase::FinalizeOriginalIntent;
        let report = original.original_verification.as_ref().unwrap();
        let mut record = selected_record(&old);
        record.image_sha256 = report.image_sha256.clone();
        record.published_sequence = report.published_sequence;
        record.dependencies.clear();
        original.reconciliation = Some(record);
        admit(&mut original, TerminalMode::Original);
        metadata::measure(&original, caps.payload, &l).unwrap();
    }
    const FIXTURE: &str = r###"{"version":1,"operation_id":"01a1ce11-0000-7000-8000-000000000001","destination":"/fixture/native-store.fls","staging":"/fixture/.restore-01a1ce11-0000-7000-8000-000000000001-01a1ce11-0000-7000-8000-000000000002","rollback":"/fixture/.restore-01a1ce11-0000-7000-8000-000000000001-01a1ce11-0000-7000-8000-000000000002/original.fls","original":{"state":"readable","identity":[11,22],"bytes":23068672,"sha256":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"},"original_verification":{"report_version":1,"copy_evidence":{"verified_file_identity":[11,22],"source_image_identity":null,"creation":"caller_supplied_existing","image_sha256":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"},"operation_id":"01a1ce11-0000-7000-8000-000000000001","image_sha256":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","image_bytes":23068672,"published_sequence":7,"logical_sha256":"eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee","profile_version":1,"profile_sha256":"4444444444444444444444444444444444444444444444444444444444444444","canonical_encoding_version":1,"intrinsic_lineage_available":false,"external_publish_status":"exclusion_unproven_before_single_ownership","verified_collection_count":2,"unknown_collection_count":0,"opaque_runtime_references":false,"query_probe_count":4,"exception_count":0,"dependency_count":0,"transient_rpc_dedupe_persisted":false,"durable_rpc_receipts_claimed":false,"readiness":"verified_local_contents","verifier_build_identity":{"package_version":"0.1.0","source_revision":{"availability":"unavailable"},"executable_sha256":{"availability":"unavailable"}}},"original_read_error":null,"selected_profile":{"version":1,"kind":"small_empty","catalog_identity":"fixture/catalog/store","dependencies":[{"reference_sha256":"dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd","catalog_identity":"original-or-selected-external-store","required_sequence":9,"policy":"reconcile_before_admission"}],"preexisting_exceptions":[],"reference_rules":[],"probes":[]},"selected_dependencies":{"references":[{"reference_sha256":"dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd","catalog_identity":"original-or-selected-external-store","required_sequence":9,"policy":"reconcile_before_admission"}]},"selected_sequence":11,"selected_logical_sha256":"eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee","generation_metadata_sha256":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],"terminal_evidence_context":null,"original_dependencies":{"references":[]},"original_profile":{"version":1,"kind":"small_empty","catalog_identity":"fixture/catalog/store","dependencies":[],"preexisting_exceptions":[],"reference_rules":[],"probes":[]},"candidate_bytes":23068672,"probe_identities":[[11,25],[11,26]],"candidate_identity":[11,23],"candidate_sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","staging_identity":[11,21],"journal_identity":[11,24],"operation_lock_identity":[11,27],"journal_capacity":983040,"selected_bundle_operation":"01a1ce11-0000-7000-8000-000000000002","selected_bundle_sha256":"ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff","binding":{"version":1,"operation_id":"01a1ce11-0000-7000-8000-000000000001","action":{"action":"restore","bundle_operation":"01a1ce11-0000-7000-8000-000000000002","bundle_sha256":"ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"},"catalog_identity":"fixture/catalog/store","approved_root_identity":[11,10],"physical_path":"/fixture/native-store.fls","parent_identity":[11,20],"expected_file":{"state":"readable","identity":[11,22],"bytes":23068672,"sha256":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"},"profile_sha256":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","build":{"package_version":"0.1.0","source_revision":{"availability":"unavailable"},"executable_sha256":{"availability":"unavailable"}},"issued_at_unix_ms":1000,"expires_at_unix_ms":2000,"operator":"fixture operator","exclusion":{"legacy_and_cached_handles_excluded":true,"external_dispatch_excluded":true,"active_owner_excluded":true,"evidence":"fixture administrative exclusion"}},"verification":{"report_version":1,"copy_evidence":{"verified_file_identity":[11,23],"source_image_identity":null,"creation":"caller_supplied_existing","image_sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},"operation_id":"01a1ce11-0000-7000-8000-000000000001","image_sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","image_bytes":23068672,"published_sequence":11,"logical_sha256":"eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee","profile_version":1,"profile_sha256":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","canonical_encoding_version":1,"intrinsic_lineage_available":false,"external_publish_status":"exclusion_unproven_before_single_ownership","verified_collection_count":2,"unknown_collection_count":0,"opaque_runtime_references":false,"query_probe_count":4,"exception_count":0,"dependency_count":1,"transient_rpc_dedupe_persisted":false,"durable_rpc_receipts_claimed":false,"readiness":"verified_local_contents","verifier_build_identity":{"package_version":"0.1.0","source_revision":{"availability":"unavailable"},"executable_sha256":{"availability":"unavailable"}}},"budget":{"baseline_bytes":23068672,"backup_bytes":23134208,"destination_bytes":23068672,"candidate_bytes":23068672,"rollback_new_bytes":0,"verifier_scratch_bytes":0,"metadata_bytes":18874368,"allocation_ceiling":46137344,"read_byte_ceiling":134217728,"memory_ceiling":536870912,"metadata_payload_ceiling":1048576,"space_margin_bytes":4096},"phase":"installed","resume_finalize_attempts":0,"rollback_attempts":0,"current_attempt":0,"attempt_deadline_unix_ms":1800,"forensic_handling_required":false,"reconciliation":null,"absence_reconciliation":null}"###;
}

#[cfg(all(test, target_os = "linux"))]
mod forensic_binding_tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    #[test]
    fn forensic_original_binding_rejects_a_different_read_error_code() {
        let root = crate::recovery::test_support::fixture_root();
        let path = root.path().join("unreadable.fls");
        std::fs::write(&path, b"forensic original").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let m = std::fs::metadata(&path).unwrap();
        let l = OperationLimits::new(
            std::time::Instant::now() + std::time::Duration::from_secs(30),
            1 << 20,
            1 << 20,
            super::super::CancelToken::new(),
        );
        let mut binding = FileBinding::Forensic {
            identity: (m.dev(), m.ino()),
            bytes: m.len(),
            read_error: libc::EACCES,
        };
        matches_binding(&path, &binding, &l).unwrap();
        if let FileBinding::Forensic { read_error, .. } = &mut binding {
            *read_error = libc::EIO;
        }
        assert!(matches_binding(&path, &binding, &l).is_err());
    }
}

#[cfg(test)]
mod restore_lease_deadline_tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    struct Clock {
        now: u128,
        sampled: Mutex<Option<Instant>>,
        cancel: Option<super::super::CancelToken>,
    }
    impl fs::RecoveryIo for Clock {
        fn unix_ms(&self) -> std::io::Result<u128> {
            *self.sampled.lock().unwrap() = Some(Instant::now());
            if let Some(cancel) = &self.cancel {
                cancel.cancel();
            }
            Ok(self.now)
        }
    }
    fn clock(now: u128) -> Clock {
        Clock {
            now,
            sampled: Mutex::new(None),
            cancel: None,
        }
    }
    fn limits() -> OperationLimits {
        OperationLimits::new(
            Instant::now() + Duration::from_secs(30),
            1234,
            5678,
            super::super::CancelToken::new(),
        )
    }
    #[test]
    fn initial_cap_uses_preclock_instant_and_preserves_budgets() {
        let mut l = limits();
        l.charge_query_work(7).unwrap();
        let shared = l.clone();
        let io = clock(1000);
        cap_lease_deadline(2000, &io, &mut l).unwrap();
        assert!(l.deadline <= io.sampled.lock().unwrap().unwrap() + Duration::from_secs(1));
        assert_eq!(
            (l.byte_limit, l.memory_byte_limit, l.query_work_limit),
            (
                shared.byte_limit,
                shared.memory_byte_limit,
                shared.query_work_limit
            )
        );
        assert_eq!(
            l.query_work_used.load(std::sync::atomic::Ordering::Acquire),
            7
        );
        assert!(std::sync::Arc::ptr_eq(
            &l.query_work_used,
            &shared.query_work_used
        ));
        let caller_deadline = Instant::now() + Duration::from_millis(100);
        l.deadline = caller_deadline;
        cap_lease_deadline(2000, &io, &mut l).unwrap();
        assert_eq!(l.deadline, caller_deadline);
    }
    #[test]
    fn expired_wallclock_and_monotonic_limits_return_deadline() {
        let mut l = limits();
        for now in [2000, 2001] {
            assert!(matches!(
                cap_lease_deadline(2000, &clock(now), &mut l),
                Err(RecoveryError::Deadline { .. })
            ));
            assert!(matches!(
                lease_time_remaining(2000, &clock(now), &l),
                Err(RecoveryError::Deadline { .. })
            ));
        }
        l.deadline = Instant::now();
        assert!(matches!(
            lease_time_remaining(2000, &clock(1000), &l),
            Err(RecoveryError::Deadline { .. })
        ));
    }
    #[test]
    fn final_clock_callback_cannot_bypass_limits() {
        let l = limits();
        let io = Clock {
            cancel: Some(l.cancel.clone()),
            ..clock(1000)
        };
        assert!(matches!(
            lease_time_remaining(2000, &io, &l),
            Err(RecoveryError::Cancelled { .. })
        ));
    }
}
