//! Bounded backup admission and durable bundle publication.

use super::fs::RecoveryFs;
use super::manifest::{Availability, EffectiveCodeIndexConfig, MemberKind};
use super::{
    CancelToken, CaptureBudget, OperationLimits, RecoveryError, StoreIdentity, ValidationProfile,
};
use std::path::PathBuf;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovedConfig {
    pub kind: MemberKind,
    pub path: PathBuf,
    pub identity: (u64, u64),
    pub byte_limit: u64,
}
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExcludedVaultKey {
    pub path: PathBuf,
    pub identity: (u64, u64),
}
pub struct BackupRequest {
    pub store: StoreIdentity,
    pub source_map_sha256: String,
    pub operation_id: uuid::Uuid,
    pub batch_id: Option<uuid::Uuid>,
    pub destination: PathBuf,
    pub capture: CaptureBudget,
    pub limits: OperationLimits,
    pub profile: ValidationProfile,
    pub image_byte_limit: Option<u64>,
    /// Payload budget for manifest/report/config/catalog metadata; derived
    /// directory growth and rounding are added, with total R capped at512MiB.
    pub metadata_byte_limit: u64,
    pub space_margin_bytes: u64,
    pub configs: Vec<ApprovedConfig>,
    pub excluded_vault_key: Option<ExcludedVaultKey>,
    pub code_index_configuration: Availability<EffectiveCodeIndexConfig>,
}
#[derive(Debug)]
pub struct VerifiedBundle {
    pub(crate) path: PathBuf,
}
impl VerifiedBundle {
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}
#[derive(Debug)]
pub enum PublicationOutcome {
    Durable(VerifiedBundle),
    OutcomeUnknown { operation_id: uuid::Uuid },
}
use super::fs::{
    self, Dir, Directory, ImageWriter, Member, PublicationIo, RecoveryEvent, StoreLock,
};
use super::manifest::{BuildIdentity, BundleManifestV1, ManifestContext, MemberChecksum};
use super::verify::{self, PrivateFile};
use std::{os::unix::fs::MetadataExt, sync::Arc};

fn admit_bytes(
    source: u64,
    image: u64,
    metadata: u64,
    available: u64,
    operation: u64,
) -> Result<u64, ()> {
    let twice_source = source
        .checked_mul(2)
        .and_then(|n| n.checked_add(metadata))
        .ok_or(())?;
    let required = image
        .checked_mul(2)
        .and_then(|n| n.checked_add(metadata))
        .ok_or(())?;
    if image == 0
        || image > source
        || metadata == 0
        || metadata > 512 * 1024 * 1024
        || required > twice_source
        || required > available
        || required > operation
    {
        return Err(());
    }
    Ok(required)
}
fn quota(l: &OperationLimits) -> RecoveryError {
    RecoveryError::Quota {
        operation_id: l.operation_id,
        resource: "two images and all staging",
    }
}
fn member(kind: &MemberKind) -> Option<(&'static str, Member)> {
    match kind {
        MemberKind::ProtectedConfig => Some(("config.toml", Member::Config)),
        MemberKind::Warrants => Some(("warrants.json", Member::Warrants)),
        MemberKind::VaultCiphertext => Some(("vault.enc", Member::VaultCiphertext)),
        _ => None,
    }
}
fn json_member<T: serde::Serialize>(
    dir: &Dir,
    name: &str,
    value: &T,
    kind: MemberKind,
    io_kind: Member,
    control: &Arc<PublicationIo>,
    l: &OperationLimits,
) -> Result<MemberChecksum, RecoveryError> {
    let (bytes, sha256) = fs::write_json(dir, name, value, io_kind, control, l)?;
    Ok(MemberChecksum {
        kind,
        relative_name: name.into(),
        bytes,
        sha256,
    })
}
struct ConfigSource {
    held: PrivateFile,
    name: &'static str,
    kind: MemberKind,
    io_kind: Member,
    len: u64,
}
fn configs(
    request: &BackupRequest,
    l: &OperationLimits,
) -> Result<Vec<ConfigSource>, RecoveryError> {
    let mut seen = std::collections::BTreeSet::new();
    let mut result = Vec::new();
    let mut total = 0u64;
    if request.configs.len() > 3 {
        return Err(fs::refused(l, "only approved batch members permitted"));
    }
    for config in &request.configs {
        let Some((name, io_kind)) = member(&config.kind) else {
            return Err(fs::refused(l, "only approved batch members permitted"));
        };
        let key = request
            .excluded_vault_key
            .as_ref()
            .ok_or_else(|| fs::refused(l, "explicit vault key exclusion required"))?;
        if !seen.insert(name)
            || config.path == key.path
            || config.identity == key.identity
            || config
                .path
                .components()
                .any(|c| c.as_os_str() == "vault.key")
        {
            return Err(fs::refused(l, "secret key or member alias refused"));
        }
        let held = PrivateFile::open(&config.path, false, l)?;
        if held.identity() != config.identity || held.identity() == key.identity {
            return Err(fs::refused(l, "configuration identity changed"));
        }
        let len = held.file.metadata().map_err(|_| fs::destination(l))?.len();
        if len > config.byte_limit {
            return Err(quota(l));
        }
        total = total.checked_add(len).ok_or_else(|| quota(l))?;
        l.quota(total, request.metadata_byte_limit, "configuration bytes")?;
        result.push(ConfigSource {
            held,
            name,
            kind: config.kind.clone(),
            io_kind,
            len,
        });
    }
    Ok(result)
}
fn copy_config(
    source: &ConfigSource,
    stage: &Dir,
    control: &Arc<PublicationIo>,
    l: &OperationLimits,
) -> Result<MemberChecksum, RecoveryError> {
    use std::os::unix::fs::FileExt;
    source.held.unchanged(l)?;
    let source_checksum = verify::hash_file(&source.held.file, l)?;
    fs::event(&*control.io, RecoveryEvent::Create(source.io_kind), l)?;
    let target = PrivateFile::open(&stage.path.join(source.name), true, l)?;
    control.allocate(&target.file, source.len, true, l)?;
    {
        l.admit_memory(65536)?;
        let mut offset = 0;
        let mut buffer = [0u8; 65536];
        while offset < source.len {
            l.check()?;
            let n = (source.len - offset).min(buffer.len() as u64) as usize;
            source
                .held
                .file
                .read_exact_at(&mut buffer[..n], offset)
                .map_err(|_| l.verification())?;
            control.write_all(&target.file, &buffer[..n], offset, source.io_kind, l)?;
            offset += n as u64;
        }
    }
    control.sync(&target.file, source.io_kind, l)?;
    source.held.unchanged(l)?;
    target.same_name(l)?;
    let (bytes, sha256) = verify::hash_file(&target.file, l)?;
    if (bytes, sha256.clone()) != source_checksum {
        return Err(l.verification());
    }
    Ok(MemberChecksum {
        kind: source.kind.clone(),
        relative_name: source.name.into(),
        bytes,
        sha256,
    })
}
/// Admit before capture, retain one immutable pin, verify a separately created
/// copy, then publish. All source I/O remains inside the existing capture API.
pub fn backup(
    owner: &crate::FerroStore,
    request: &BackupRequest,
    filesystem: &dyn RecoveryFs,
    cancel: &CancelToken,
) -> Result<PublicationOutcome, RecoveryError> {
    backup_with_shared_config(owner, request, None, filesystem, cancel)
}
/// A checksum-bound config reference never substitutes for ordinary local member verification.
pub fn backup_with_shared_config(
    owner: &crate::FerroStore,
    request: &BackupRequest,
    shared_config: Option<&super::manifest::SharedConfigReference>,
    filesystem: &dyn RecoveryFs,
    cancel: &CancelToken,
) -> Result<PublicationOutcome, RecoveryError> {
    let mut l = request.limits.clone();
    l.operation_id = request.operation_id;
    l.cancel = CancelToken::combined(&l.cancel, cancel);
    l.check()?;
    if request.destination != filesystem.root()
        || request.operation_id.is_nil()
        || request.store.alias_status != super::inventory::AliasStatus::NoAliasObserved
    {
        return Err(fs::refused(
            &l,
            "approved root and unambiguous inventory identity required",
        ));
    }
    let source = request
        .store
        .canonical_path
        .as_deref()
        .ok_or_else(|| fs::refused(&l, "physical source binding required"))?;
    if owner.recovery_source_path(l.operation_id)? != source
        || !request.store.exists
        || request.profile.catalog_identity != request.store.catalog_key
    {
        return Err(fs::refused(&l, "request does not bind owner and profile"));
    }
    if request.source_map_sha256.len() != 64
        || !request
            .source_map_sha256
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(fs::refused(&l, "source map digest required"));
    }
    let source_handle = fs::source_file(source, &l)?;
    let source_meta = source_handle.metadata().map_err(|_| fs::destination(&l))?;
    let physical = (source_meta.dev(), source_meta.ino());
    if request.store.filesystem_identity.as_deref()
        != Some(format!("{}:{}", physical.0, physical.1).as_str())
    {
        return Err(fs::refused(&l, "inventory physical identity changed"));
    }
    let root = Dir::open(filesystem.root(), &l)?;
    let shared_held = if let Some(reference) = shared_config {
        if !request.configs.is_empty() {
            return Err(fs::refused(
                &l,
                "shared configs must not be duplicated in leaf",
            ));
        }
        super::catalog::check_shared_config(
            &root,
            reference,
            request.batch_id.map(|id| id.to_string()).as_deref(),
            &l,
        )?
    } else {
        Vec::new()
    };
    let io = filesystem.io();
    let available = io
        .available_bytes(&root.file)
        .map_err(|_| fs::destination(&l))?
        .checked_sub(request.space_margin_bytes)
        .ok_or_else(|| quota(&l))?;
    let ceiling = request.image_byte_limit.unwrap_or(source_meta.len());
    let unit = fs::allocation_geometry(&root, &l)?;
    let reserve = fs::overhead_bytes(unit, &l)?
        .checked_add(request.metadata_byte_limit)
        .filter(|n| *n <= 512 * 1024 * 1024)
        .ok_or_else(|| quota(&l))?;
    let cap = admit_bytes(source_meta.len(), ceiling, reserve, available, l.byte_limit)
        .map_err(|_| quota(&l))?;
    l.quota(65536, l.memory_byte_limit, "copy working memory")?;
    let config_sources = configs(request, &l)?;
    verify::profile_digest(&request.profile, &l)?;
    let control = Arc::new(PublicationIo::new(io.clone(), cap, reserve, ceiling, unit));
    control.charge(fs::overhead_bytes(unit, &l)?, true, &l)?;
    l.publication_io = Some(control.clone());
    let locks = root.child(".locks", false, &l)?;
    let _lock = StoreLock::acquire(&locks, physical, &l)?;
    let _operation_lock = StoreLock::operation(&locks, request.operation_id, &l)?;
    let catalog_access =
        fs::CatalogLock::acquire(&root, fs::CatalogLockMode::Shared, true, io.clone(), &l)?;
    let staging = root.child(".staging", false, &l)?;
    let catalog = root.child(".catalog", false, &l)?;
    let quarantine = root.child(".quarantine", false, &l)?;
    let operation = request.operation_id.to_string();
    if root.exists(&operation, &l)?
        || staging.exists(&operation, &l)?
        || quarantine.exists(&operation, &l)?
        || catalog.exists(&format!("{operation}.json"), &l)?
    {
        return Err(fs::refused(&l, "operation name already exists"));
    }
    root.sync(&*io, Directory::Initialization, &l)?;
    // Directory entries and allocation rounding have a separate conservative
    // reservation within the metadata budget, never outside the shared cap.
    for directory in [&locks, &staging, &catalog, &quarantine] {
        fs::directory_geometry(directory, unit, &l)?;
    }
    let stage = staging.child(&operation, true, &l)?;
    let mut renamed = false;
    let result = (|| {
        fs::event(&*io, RecoveryEvent::Capture, &l)?;
        let mut budget = request.capture.clone();
        budget.deadline = budget.deadline.min(l.deadline);
        budget.pin_byte_limit = budget.pin_byte_limit.min(l.memory_byte_limit);
        budget.preparation_byte_limit = budget.preparation_byte_limit.min(l.memory_byte_limit);
        let pin = owner.capture_recovery(&budget, &l.cancel)?;
        let expected = pin.expectations(&request.profile, &l)?;
        if expected.source_file_identity != Some(physical)
            || fs::identity(&fs::source_file(source, &l)?, &l)? != physical
        {
            return Err(fs::refused(&l, "capture physical identity changed"));
        }
        let mut writer = ImageWriter::new(&stage.path.join("image.fls"), control.clone(), &l)?;
        let image = pin.write_image(&mut writer, &l);
        if let Some(error) = writer.failure {
            return Err(error);
        }
        let image = image?;
        writer.held.same_name(&l)?;
        drop(pin);
        let mut members = vec![
            json_member(
                &stage,
                "validation.json",
                &expected,
                MemberKind::ValidationExpectations,
                Member::Validation,
                &control,
                &l,
            )?,
            json_member(
                &stage,
                "profile.json",
                &request.profile,
                MemberKind::ValidationProfile,
                Member::Profile,
                &control,
                &l,
            )?,
        ];
        for source in &config_sources {
            members.push(copy_config(source, &stage, &control, &l)?)
        }
        if matches!(request.code_index_configuration, Availability::Available(_)) {
            members.push(json_member(
                &stage,
                "index-config.json",
                &request.code_index_configuration,
                MemberKind::IndexConfiguration,
                Member::IndexConfig,
                &control,
                &l,
            )?)
        }
        stage.sync(&*io, Directory::StagingBeforeVerify, &l)?;
        // Exactly Task 3's wrapper. It creates the separate inode, copies all
        // bytes through the shared quota, flushes it, and performs both reopens.
        let report = super::copy_and_verify(
            &stage.path.join("image.fls"),
            &stage.path.join("verification.fls"),
            &expected,
            &request.profile,
            &l,
        )?;
        if report.image_bytes != image.bytes {
            return Err(l.verification());
        }
        members.push(json_member(
            &stage,
            "report.json",
            &report,
            MemberKind::VerificationReport,
            Member::Report,
            &control,
            &l,
        )?);
        // The durable report must exist before the independently verified copy
        // is removed. Its file and name are both made durable first.
        stage.sync(&*io, Directory::StagingBeforePublish, &l)?;
        let evidence = report
            .copy_evidence
            .as_ref()
            .ok_or_else(|| l.verification())?;
        fs::event(&*io, RecoveryEvent::RemoveVerifier, &l)?;
        stage.remove_file("verification.fls", evidence.verified_file_identity, &l)?;
        for (_, file) in &shared_held {
            file.unchanged(&l)?;
        }
        let manifest = BundleManifestV1::verified_unpublished(
            &expected,
            &request.profile,
            &report,
            ManifestContext {
                batch_id: request.batch_id.map(|id| id.to_string()),
                shared_config: shared_config.cloned(),
                source_map_sha256: request.source_map_sha256.clone(),
                creator_build_identity: BuildIdentity::current(),
                retained_members: members,
                code_index_configuration: request.code_index_configuration.clone(),
            },
            &l,
        )?;
        fs::write_json(
            &stage,
            "manifest.json",
            &manifest,
            Member::Manifest,
            &control,
            &l,
        )?;
        // Recheck all final members against their held checksums immediately
        // before the exclusive rename, including rejection of unexpected files.
        let checked = super::catalog::check_bundle(&stage, &operation, &l)?;
        let checked_limits = checked.retained_limits(&l)?;
        checked.flush(&control, &checked_limits)?;
        stage.sync(&*io, Directory::StagingBeforePublish, &l)?;
        staging.rename_checked(
            &operation,
            &root,
            &operation,
            &*io,
            RecoveryEvent::Publish,
            &l,
            || {
                stage.same_name(&l)?;
                writer.held.same_name(&l)?;
                let latest = super::catalog::check_bundle(&stage, &operation, &checked_limits)?;
                if !checked.same_manifest(&latest) {
                    return Err(l.verification());
                }
                Ok(())
            },
        )?;
        renamed = true;
        root.sync(&*io, Directory::PublicationParent, &l)?;
        staging.sync(&*io, Directory::StagingParent, &l)?;
        super::catalog::record(&root, &catalog, checked, &control, &l, &catalog_access)
    })();
    match result {
        Ok(bundle) => Ok(PublicationOutcome::Durable(bundle)),
        Err(error @ RecoveryError::Quarantined { .. }) => Err(error),
        Err(_) if renamed => Ok(PublicationOutcome::OutcomeUnknown {
            operation_id: request.operation_id,
        }),
        Err(error) => {
            // Never sweep or unlink untrusted names. If cancellation/deadline
            // prevents bounded cleanup, leave only this private operation for
            // explicit reconciliation; nothing has occupied a final name.
            if staging
                .rename(
                    &operation,
                    &quarantine,
                    &operation,
                    &*io,
                    RecoveryEvent::Publish,
                    &l,
                )
                .is_ok()
            {
                let _ = quarantine.sync(&*io, Directory::QuarantineParent, &l);
                let _ = staging.sync(&*io, Directory::StagingParent, &l);
            }
            Err(error)
        }
    }
}
pub fn reconcile_publication(
    operation_id: &str,
    filesystem: &dyn RecoveryFs,
) -> Result<PublicationOutcome, RecoveryError> {
    super::catalog::reconcile(operation_id, filesystem)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_verified_handle_does_not_make_unpublished_directory_completed() {
        let root = crate::recovery::test_support::fixture_root();
        let operation = uuid::Uuid::now_v7();
        let path = root.path().join(operation.to_string());
        std::fs::create_dir(&path).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let limits = OperationLimits::new(
            std::time::Instant::now() + std::time::Duration::from_secs(10),
            1 << 20,
            64 << 20,
            CancelToken::new(),
        );
        let false_handle = VerifiedBundle { path: path.clone() };
        assert!(crate::recovery::catalog::retention_manifest(&false_handle, &limits).is_err());
        assert_eq!(std::fs::read_dir(&path).unwrap().count(), 0);
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }

    // Omitting the second full image or metadata from pre-capture admission
    // admits each of these impossible operations.
    #[test]
    fn two_images_and_metadata_must_fit_before_capture() {
        assert_eq!(
            admit_bytes(4096, 4096, 1, u64::MAX, u64::MAX).unwrap(),
            8193
        );
        assert_eq!(
            admit_bytes(4096, 4095, 3, u64::MAX, u64::MAX).unwrap(),
            8193
        );
        assert_eq!(
            admit_bytes(4096, 3000, 2193, u64::MAX, u64::MAX).unwrap(),
            8193
        );
        assert_eq!(admit_bytes(4096, 3000, 2192, 8192, 8192).unwrap(), 8192);
    }

    #[test]
    fn admission_refuses_overflow_space_and_operation_limits() {
        assert!(admit_bytes(u64::MAX, 1, 1, u64::MAX, u64::MAX).is_err());
        assert!(admit_bytes(4096, u64::MAX, 1, u64::MAX, u64::MAX).is_err());
        assert!(admit_bytes(4096, 1, u64::MAX, u64::MAX, u64::MAX).is_err());
        assert!(admit_bytes(4096, 3000, 1000, 6999, 8192).is_err());
        assert!(admit_bytes(4096, 3000, 1000, 8192, 6999).is_err());
        assert!(admit_bytes(4096, 0, 1000, 8192, 8192).is_err());
        assert!(admit_bytes(4096, 3000, 0, 8192, 8192).is_err());
    }
}

/// Read-only metadata status. Errors mean the scan is incomplete; no partial
/// count is reported as complete. Reserve is specific to the supplied request.
#[derive(Debug)]
pub struct PublicationStatus {
    pub requested_metadata_payload_bytes: u64,
    pub derived_reserve_bytes: u64,
    pub directory_and_rounding_reserve_bytes: u64,
    pub quarantined_count: u64,
    pub quarantined_allocated_bytes: u64,
}
pub fn publication_status(
    request: &BackupRequest,
    filesystem: &dyn RecoveryFs,
) -> Result<PublicationStatus, RecoveryError> {
    let l = &request.limits;
    if request.destination != filesystem.root() {
        return Err(fs::refused(l, "approved root required"));
    }
    let root = Dir::open(filesystem.root(), l)?;
    let unit = fs::allocation_geometry(&root, l)?;
    let mut result = PublicationStatus {
        requested_metadata_payload_bytes: request.metadata_byte_limit,
        derived_reserve_bytes: fs::overhead_bytes(unit, l)?
            .checked_add(request.metadata_byte_limit)
            .filter(|n| *n <= 512 * 1024 * 1024)
            .ok_or_else(|| quota(l))?,
        directory_and_rounding_reserve_bytes: fs::overhead_bytes(unit, l)?,
        quarantined_count: 0,
        quarantined_allocated_bytes: 0,
    };
    let quarantine = Dir::open(&root.path.join(".quarantine"), l)?;
    fn allocated(dir: &Dir, l: &OperationLimits, depth: u32) -> Result<u64, RecoveryError> {
        if depth > 4 {
            return Err(l.verification());
        }
        l.admit_memory(4096 * (depth as u64 + 1))?;
        let mut total = dir
            .file
            .metadata()
            .map_err(|_| fs::destination(l))?
            .blocks()
            .checked_mul(512)
            .ok_or_else(|| l.verification())?;
        dir.visit_names(l, |name| {
            let m =
                std::fs::symlink_metadata(dir.path.join(name)).map_err(|_| fs::destination(l))?;
            let bytes = if m.is_dir() {
                let child = Dir::open(&dir.path.join(name), l)?;
                if fs::identity(&child.file, l)? != (m.dev(), m.ino()) {
                    return Err(l.verification());
                }
                allocated(&child, l, depth + 1)?
            } else if m.is_file() && m.nlink() == 1 && m.uid() == unsafe { libc::geteuid() } {
                m.blocks()
                    .checked_mul(512)
                    .ok_or_else(|| l.verification())?
            } else {
                return Err(l.verification());
            };
            total = total.checked_add(bytes).ok_or_else(|| l.verification())?;
            Ok(())
        })?;
        Ok(total)
    }
    quarantine.visit_names(l, |_| {
        result.quarantined_count = result
            .quarantined_count
            .checked_add(1)
            .ok_or_else(|| l.verification())?;
        Ok(())
    })?;
    result.quarantined_allocated_bytes = allocated(&quarantine, l, 0)?;
    Ok(result)
}

/// Publish approved secret-bearing configs once, before any batch leaf. No
/// vault key path is opened. Allocation, source identities and all names retain
/// Task 4's private-file checks and bounded PublicationIo accounting.
pub fn publish_batch_configs(
    request: &BackupRequest,
    filesystem: &dyn RecoveryFs,
) -> Result<super::manifest::SharedConfigReference, RecoveryError> {
    use super::manifest::{BatchConfigV1, SharedConfigReference};
    let mut l = request.limits.clone();
    let batch = request
        .batch_id
        .filter(|b| !b.is_nil())
        .ok_or_else(|| fs::refused(&l, "batch config needs batch ID"))?;
    l.operation_id = batch;
    if request.destination != filesystem.root() {
        return Err(fs::refused(&l, "configured root required"));
    }
    let root = Dir::open(filesystem.root(), &l)?;
    let unit = fs::allocation_geometry(&root, &l)?;
    let reserve = request
        .metadata_byte_limit
        .checked_add(fs::overhead_bytes(unit, &l)?)
        .and_then(|n| n.checked_add(unit.checked_mul(16)?))
        .ok_or_else(|| quota(&l))?;
    if reserve > 512 << 20
        || reserve.checked_mul(2).is_none_or(|n| n > l.byte_limit)
        || filesystem
            .io()
            .available_bytes(&root.file)
            .map_err(|_| fs::destination(&l))?
            .checked_sub(request.space_margin_bytes)
            .is_none_or(|n| reserve.checked_mul(2).is_none_or(|twice| twice > n))
    {
        return Err(quota(&l));
    }
    let control = Arc::new(PublicationIo::new(
        filesystem.io(),
        reserve,
        reserve,
        0,
        unit,
    ));
    l.publication_io = Some(control.clone());
    control.charge(fs::overhead_bytes(unit, &l)?, true, &l)?;
    let locks = root.child(".locks", false, &l)?;
    let _lock = StoreLock::operation(&locks, batch, &l)?;
    let _catalog_access = fs::CatalogLock::acquire(
        &root,
        fs::CatalogLockMode::Shared,
        true,
        control.io.clone(),
        &l,
    )?;
    let parent = root.child(".batch-config", false, &l)?;
    let name = batch.to_string();
    let stage_name = format!("{batch}.staging");
    if parent.exists(&name, &l)? {
        let dir = Dir::open(&parent.path.join(&name), &l)?;
        let file = PrivateFile::open(&dir.path.join("config.json"), false, &l)?;
        let (_, digest) = verify::hash_file(&file.file, &l)?;
        let reference = SharedConfigReference {
            batch_id: batch,
            descriptor_sha256: digest,
        };
        let held = super::catalog::check_shared_config(&root, &reference, Some(&name), &l)?;
        for (kind, file) in &held {
            control.sync(&file.file, *kind, &l)?;
            file.unchanged(&l)?;
        }
        dir.sync(&*control.io, Directory::PublicationParent, &l)?;
        parent.sync(&*control.io, Directory::PublicationParent, &l)?;
        root.sync(&*control.io, Directory::PublicationParent, &l)?;
        return Ok(reference);
    }
    l.admit_memory(65536)?;
    let stage = parent.child(&stage_name, true, &l)?;
    let members = configs(request, &l)?
        .iter()
        .map(|source| copy_config(source, &stage, &control, &l))
        .collect::<Result<Vec<_>, _>>()?;
    let descriptor = BatchConfigV1 {
        version: 1,
        batch_id: batch,
        members,
    };
    let (_, digest) = fs::write_json(
        &stage,
        "config.json",
        &descriptor,
        Member::Manifest,
        &control,
        &l,
    )?;
    stage.sync(&*control.io, Directory::StagingBeforePublish, &l)?;
    parent.rename(
        &stage_name,
        &parent,
        &name,
        &*control.io,
        RecoveryEvent::Publish,
        &l,
    )?;
    parent.sync(&*control.io, Directory::PublicationParent, &l)?;
    root.sync(&*control.io, Directory::PublicationParent, &l)?;
    Ok(SharedConfigReference {
        batch_id: batch,
        descriptor_sha256: digest,
    })
}

/// Explicit administrative offline action. Daemon availability never selects it.
pub fn backup_offline(
    request: &BackupRequest,
    lease: &super::admission::OfflineLease,
    filesystem: &dyn RecoveryFs,
    cancel: &CancelToken,
) -> Result<PublicationOutcome, RecoveryError> {
    use super::admission::{OfflineAction, OperationOwnership};
    let mut l = request.limits.clone();
    l.operation_id = request.operation_id;
    lease.validate(filesystem, &l)?;
    let source = request
        .store
        .canonical_path
        .as_deref()
        .ok_or_else(|| fs::refused(&l, "existing physical source required"))?;
    if lease.request.action != OfflineAction::Backup
        || lease.request.operation_id != request.operation_id
        || lease.request.physical_path != source
        || lease.request.catalog_identity != request.store.catalog_key
        || lease.request.profile_sha256 != verify::profile_digest(&request.profile, &l)?
    {
        return Err(fs::refused(
            &l,
            "offline backup source, action, profile or operation binding mismatch",
        ));
    }
    let _ownership = OperationOwnership::acquire(source, &l)?;
    let held = fs::source_file(source, &l)?;
    let identity = fs::identity(&held, &l)?;
    // This private engine entry never creates, initializes, migrates or routes.
    let owner = crate::FerroStore::open_existing_for_backup(source, &l).map_err(|_| {
        RecoveryError::UnhealthySource {
            operation_id: l.operation_id,
        }
    })?;
    if fs::identity(&fs::source_file(source, &l)?, &l)? != identity {
        return Err(l.verification());
    }
    lease.validate(filesystem, &l)?;
    let outcome = backup(&owner, request, filesystem, cancel)?;
    lease.validate(filesystem, &l)?;
    Ok(outcome)
}
