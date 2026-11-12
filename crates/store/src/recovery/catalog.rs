//! Immutable bundle checks and atomic, recoverable publication receipts. There
//! is intentionally no selector that turns a bare directory into VerifiedBundle.
use super::backup::{PublicationOutcome, VerifiedBundle};
use super::fs::{
    self, Dir, Directory, Member, PublicationIo, RecoveryEvent, RecoveryFs, StoreLock,
};
use super::manifest::{
    BundleManifestV1, IndependentCopyStatus, LocalPublicationStatus, ManifestContext,
    MemberChecksum, MemberKind,
};
use super::verify::{self, PrivateFile};
use super::{
    OperationLimits, RecoveryError, ValidationExpectations, ValidationProfile, VerificationReport,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicationReceiptV1 {
    pub receipt_version: u32,
    pub operation_id: String,
    pub manifest_sha256: String,
    pub image_sha256: String,
    pub members: Vec<MemberChecksum>,
    pub published_at_unix_ms: u128,
}
pub(crate) struct CheckedBundle {
    pub(crate) manifest: BundleManifestV1,
    pub(crate) manifest_sha256: String,
    pub(crate) bytes: u64,
    pub(crate) manifest_memory_bytes: u64,
    held: Vec<(Member, PrivateFile)>,
    shared_held: Vec<(Member, PrivateFile)>,
    directory_identity: (u64, u64),
}
// Fixed names bound this set to nine bundle and four shared-config handles.
// Their bounded handle/path/name storage has a separate fixed reservation;
// opaque payload lengths never enter it.
const CHECKED_HANDLES_MEMORY: u64 = 128 << 10;
impl CheckedBundle {
    pub(crate) fn memory_bytes(&self, l: &OperationLimits) -> Result<u64, RecoveryError> {
        self.manifest_memory_bytes
            .checked_add(CHECKED_HANDLES_MEMORY)
            .ok_or_else(|| l.verification())
    }
    pub(crate) fn retained_limits(
        &self,
        l: &OperationLimits,
    ) -> Result<OperationLimits, RecoveryError> {
        l.with_retained_memory(self.memory_bytes(l)?)
    }
    pub(crate) fn same_manifest(&self, other: &Self) -> bool {
        self.manifest_sha256 == other.manifest_sha256
    }
    fn validate_at(&self, dir: &Dir, l: &OperationLimits) -> Result<(), RecoveryError> {
        if fs::identity(&dir.file, l)? != self.directory_identity {
            return Err(l.verification());
        }
        for (_, file) in &self.held {
            file.unchanged_in(&dir.path, l)?;
        }
        for (_, file) in &self.shared_held {
            file.unchanged(l)?;
        }
        let latest = check_bundle(dir, &self.manifest.operation_id, l)?;
        if !self.same_manifest(&latest) {
            return Err(l.verification());
        }
        for (_, file) in &self.held {
            file.unchanged_in(&dir.path, l)?;
        }
        dir.same_name(l)
    }
    pub(crate) fn flush(
        &self,
        control: &PublicationIo,
        l: &OperationLimits,
    ) -> Result<(), RecoveryError> {
        for (member, file) in self.held.iter().chain(self.shared_held.iter()) {
            file.unchanged(l)?;
            control.sync(&file.file, *member, l)?;
            file.unchanged(l)?;
        }
        Ok(())
    }
}
pub(crate) fn check_bundle(
    dir: &Dir,
    operation: &str,
    l: &OperationLimits,
) -> Result<CheckedBundle, RecoveryError> {
    let mut models = l.clone();
    let (manifest, manifest_memory_bytes): (BundleManifestV1, _) =
        fs::read_json_retained(dir, "manifest.json", &mut models)?;
    models = models.with_retained_memory(CHECKED_HANDLES_MEMORY)?;
    let l = &models;
    if manifest.operation_id != operation
        || manifest.manifest_version != 1
        || manifest.local_status != LocalPublicationStatus::NotPublished
        || manifest.published_at_unix_ms.is_some()
        || manifest.independent_copy_status != IndependentCopyStatus::Verified
    {
        return Err(l.verification());
    }
    let mut names = vec!["image.fls".to_owned(), "manifest.json".to_owned()];
    let mut metadata_bytes = 0u64;
    for member in &manifest.retained_members {
        let expected_name = match member.kind {
            MemberKind::ProtectedConfig => "config.toml",
            MemberKind::Warrants => "warrants.json",
            MemberKind::VaultCiphertext => "vault.enc",
            MemberKind::ValidationExpectations => "validation.json",
            MemberKind::ValidationProfile => "profile.json",
            MemberKind::VerificationReport => "report.json",
            MemberKind::IndexConfiguration => "index-config.json",
        };
        if member.relative_name != expected_name || names.iter().any(|n| n == expected_name) {
            return Err(l.verification());
        }
        names.push(expected_name.to_owned());
        metadata_bytes = metadata_bytes
            .checked_add(member.bytes)
            .ok_or_else(|| l.verification())?;
    }
    names.sort();
    if names != dir.members(l)? {
        return Err(l.verification());
    }
    let manifest_file = PrivateFile::open(&dir.path.join("manifest.json"), false, l)?;
    let (manifest_bytes, manifest_sha256) = verify::hash_file(&manifest_file.file, l)?;
    metadata_bytes = metadata_bytes
        .checked_add(manifest_bytes)
        .ok_or_else(|| l.verification())?;
    l.quota(
        metadata_bytes
            .checked_add(manifest.image_bytes)
            .ok_or_else(|| l.verification())?,
        l.byte_limit,
        "bundle bytes",
    )?;
    let mut held = vec![(Member::Manifest, manifest_file)];
    let image = PrivateFile::open(&dir.path.join("image.fls"), false, l)?;
    if verify::hash_file(&image.file, l)? != (manifest.image_bytes, manifest.image_sha256.clone()) {
        return Err(l.verification());
    }
    held.push((Member::Image, image));
    for member in &manifest.retained_members {
        let file = PrivateFile::open(&dir.path.join(&member.relative_name), false, l)?;
        if verify::hash_file(&file.file, l)? != (member.bytes, member.sha256.clone()) {
            return Err(l.verification());
        }
        let kind = match member.kind {
            MemberKind::ProtectedConfig => Member::Config,
            MemberKind::Warrants => Member::Warrants,
            MemberKind::VaultCiphertext => Member::VaultCiphertext,
            MemberKind::ValidationExpectations => Member::Validation,
            MemberKind::ValidationProfile => Member::Profile,
            MemberKind::VerificationReport => Member::Report,
            MemberKind::IndexConfiguration => Member::IndexConfig,
        };
        held.push((kind, file));
    }
    let shared_held = if let Some(reference) = &manifest.shared_config {
        if manifest.retained_members.iter().any(|m| {
            matches!(
                m.kind,
                MemberKind::ProtectedConfig | MemberKind::Warrants | MemberKind::VaultCiphertext
            )
        }) {
            return Err(l.verification());
        }
        let parent = dir.path.parent().ok_or_else(|| l.verification())?;
        let root = if parent.file_name().is_some_and(|n| n == ".staging") {
            parent.parent().ok_or_else(|| l.verification())?
        } else {
            parent
        };
        check_shared_config(
            &Dir::open(root, l)?,
            reference,
            manifest.batch_id.as_deref(),
            l,
        )?
    } else {
        Vec::new()
    };
    let (expected, _): (ValidationExpectations, _) =
        fs::read_json_retained(dir, "validation.json", &mut models)?;
    let (profile, _): (ValidationProfile, _) =
        fs::read_json_retained(dir, "profile.json", &mut models)?;
    let (report, _): (VerificationReport, _) =
        fs::read_json_retained(dir, "report.json", &mut models)?;
    // The rebuilt manifest owns a distinct copy while all four parsed models
    // still live. This is coexistence, not a second charge for opaque members.
    let rebuilt_models = models.with_retained_memory(manifest_memory_bytes)?;
    let l = &rebuilt_models;
    if report.operation_id != operation {
        return Err(l.verification());
    }
    let rebuilt = BundleManifestV1::verified_unpublished(
        &expected,
        &profile,
        &report,
        ManifestContext {
            batch_id: manifest.batch_id.clone(),
            shared_config: manifest.shared_config.clone(),
            source_map_sha256: manifest.source_map_sha256.clone(),
            creator_build_identity: manifest.creator_build_identity.clone(),
            retained_members: manifest.retained_members.clone(),
            code_index_configuration: manifest.code_index_configuration.clone(),
        },
        l,
    )?;
    if rebuilt != manifest {
        return Err(l.verification());
    }
    // All file identity/length/timestamp bindings must still match after reading
    // the final member, not just at each file's individual hash.
    for (_, file) in &held {
        file.unchanged(l)?
    }
    dir.same_name(l)?;
    let bytes = metadata_bytes
        .checked_add(manifest.image_bytes)
        .ok_or_else(|| l.verification())?;
    Ok(CheckedBundle {
        manifest,
        manifest_sha256,
        bytes,
        manifest_memory_bytes,
        held,
        shared_held,
        directory_identity: fs::identity(&dir.file, l)?,
    })
}
fn receipt_matches(receipt: &PublicationReceiptV1, checked: &CheckedBundle) -> bool {
    receipt.receipt_version == 1
        && receipt.operation_id == checked.manifest.operation_id
        && receipt.manifest_sha256 == checked.manifest_sha256
        && receipt.image_sha256 == checked.manifest.image_sha256
        && receipt.members == checked.manifest.retained_members
}
// A receipt is bound to an open handle while its contents are checked. Keep
// that handle through the final directory sync so a replacement cannot inherit
// an earlier receipt's attestation, even if its bytes are identical.
pub(crate) fn checked_receipt(
    catalog: &Dir,
    name: &str,
    checked: &CheckedBundle,
    l: &OperationLimits,
) -> Result<PrivateFile, RecoveryError> {
    let held = PrivateFile::open(&catalog.path.join(name), false, l)?;
    let receipt: PublicationReceiptV1 = fs::read_json(catalog, name, l)?;
    if !receipt_matches(&receipt, checked) {
        return Err(l.verification());
    }
    held.unchanged(l)?;
    Ok(held)
}
pub(crate) fn record(
    root: &Dir,
    catalog: &Dir,
    checked: CheckedBundle,
    control: &Arc<PublicationIo>,
    l: &OperationLimits,
    access: &fs::CatalogLock,
) -> Result<VerifiedBundle, RecoveryError> {
    let retained = checked.retained_limits(l)?;
    let l = &retained;
    access.validate(root, l)?;
    let operation = &checked.manifest.operation_id;
    let result = record_checked(root, catalog, &checked, control, l);
    match result {
        Err(RecoveryError::Verification { .. } | RecoveryError::RefusedInput { .. }) => {
            mark_quarantine(root, operation, control, l, access)?;
            Err(RecoveryError::Quarantined {
                operation_id: l.operation_id,
            })
        }
        result => result,
    }
}
fn record_checked(
    root: &Dir,
    catalog: &Dir,
    checked: &CheckedBundle,
    control: &Arc<PublicationIo>,
    l: &OperationLimits,
) -> Result<VerifiedBundle, RecoveryError> {
    let operation = &checked.manifest.operation_id;
    let bundle = Dir::open(&root.path.join(operation), l)?;
    checked.validate_at(&bundle, l)?;
    let name = format!("{operation}.json");
    let temporary = format!("{operation}.tmp");
    let receipt = if catalog.exists(&name, l)? {
        checked_receipt(catalog, &name, checked, l)?
    } else {
        let mut prepared = false;
        if catalog.exists(&temporary, l)? {
            match fs::read_json::<PublicationReceiptV1>(catalog, &temporary, l) {
                Ok(receipt) if receipt_matches(&receipt, checked) => {
                    let held = PrivateFile::open(&catalog.path.join(&temporary), false, l)?;
                    control.sync(&held.file, Member::Receipt, l)?;
                    held.unchanged(l)?;
                    prepared = true;
                }
                Err(RecoveryError::Verification { .. }) => {
                    // Preserve an interrupted temporary receipt under this
                    // operation's exact name before deriving a fresh one.
                    let held = PrivateFile::open(&catalog.path.join(&temporary), false, l)?;
                    let quarantine = root.child(".quarantine", false, l)?;
                    catalog.rename_checked(
                        &temporary,
                        &quarantine,
                        &format!("{operation}-catalog-temp"),
                        &*control.io,
                        RecoveryEvent::CatalogPublish,
                        l,
                        || {
                            checked.validate_at(&bundle, l)?;
                            held.unchanged(l)
                        },
                    )?;
                    quarantine.sync(&*control.io, Directory::QuarantineParent, l)?;
                    catalog.sync(&*control.io, Directory::CatalogParent, l)?;
                }
                Ok(_) | Err(RecoveryError::RefusedInput { .. }) => return Err(l.verification()),
                Err(error) => return Err(error),
            }
        }
        if !prepared {
            let receipt_models = l.with_retained_memory(checked.manifest_memory_bytes)?;
            let receipt = PublicationReceiptV1 {
                receipt_version: 1,
                operation_id: operation.clone(),
                manifest_sha256: checked.manifest_sha256.clone(),
                image_sha256: checked.manifest.image_sha256.clone(),
                members: checked.manifest.retained_members.clone(),
                published_at_unix_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(|_| l.verification())?
                    .as_millis(),
            };
            fs::write_json(
                catalog,
                &temporary,
                &receipt,
                Member::Receipt,
                control,
                &receipt_models,
            )?;
        }
        let prepared_receipt = checked_receipt(catalog, &temporary, checked, l)?;
        let prepared_hash = verify::hash_file(&prepared_receipt.file, l)?;
        catalog.rename_checked(
            &temporary,
            catalog,
            &name,
            &*control.io,
            RecoveryEvent::CatalogPublish,
            l,
            || {
                checked.validate_at(&bundle, l)?;
                prepared_receipt.unchanged(l)
            },
        )?;
        let committed = checked_receipt(catalog, &name, checked, l)?;
        // Renaming the receipt changes its ctime. Compare its held inode and
        // complete digest across that rename, then hold the new timestamp bind.
        if committed.identity() != prepared_receipt.identity()
            || verify::hash_file(&committed.file, l)? != prepared_hash
        {
            return Err(l.verification());
        }
        committed
    };
    // Also covers an existing receipt whose previous catalog sync failed.
    catalog.sync(&*control.io, Directory::CatalogParent, l)?;
    checked.validate_at(&bundle, l)?;
    receipt.unchanged(l)?;
    checked_receipt(catalog, &name, checked, l)?;
    receipt.unchanged(l)?;
    catalog.same_name(l)?;
    root.same_name(l)?;
    // These checks attest this bounded operation, not immunity to later writes
    // by another process with the same user's access to the backup root.
    Ok(VerifiedBundle { path: bundle.path })
}

pub(crate) fn mark_quarantine(
    root: &Dir,
    operation: &str,
    control: &Arc<PublicationIo>,
    l: &OperationLimits,
    access: &fs::CatalogLock,
) -> Result<(), RecoveryError> {
    access.validate(root, l)?;
    let quarantine = root.child(".quarantine", false, l)?;
    let entry = quarantine.child(operation, false, l)?;
    let catalog = root.child(".catalog", false, l)?;
    let name = format!("{operation}.json");
    if catalog.exists(&name, l)? {
        let held = PrivateFile::open(&catalog.path.join(&name), false, l)?;
        catalog.rename_checked(
            &name,
            &entry,
            "receipt.json",
            &*control.io,
            RecoveryEvent::CatalogPublish,
            l,
            || held.unchanged(l),
        )?;
    }
    entry.sync(&*control.io, Directory::QuarantineParent, l)?;
    catalog.sync(&*control.io, Directory::CatalogParent, l)?;
    quarantine.sync(&*control.io, Directory::QuarantineParent, l)?;
    root.sync(&*control.io, Directory::PublicationParent, l)
}
pub(crate) fn reconcile(
    operation: &str,
    filesystem: &dyn RecoveryFs,
) -> Result<PublicationOutcome, RecoveryError> {
    let mut l = filesystem.reconciliation_limits().clone();
    l.publication_io = None;
    let id = uuid::Uuid::parse_str(operation)
        .map_err(|_| fs::refused(&l, "canonical operation UUID required"))?;
    l.operation_id = id;
    l.check()?;
    if id.is_nil() || operation != id.to_string() {
        return Err(fs::refused(&l, "canonical operation UUID required"));
    }
    let root = Dir::open(filesystem.root(), &l)?;
    let io = filesystem.io();
    let unit = fs::allocation_geometry(&root, &l)?;
    let control = Arc::new(PublicationIo::new(
        io.clone(),
        l.byte_limit,
        l.byte_limit,
        l.byte_limit,
        unit,
    ));
    control.charge(fs::overhead_bytes(unit, &l)?, true, &l)?;
    l.publication_io = Some(control.clone());
    let locks = root.child(".locks", false, &l)?;
    let _lock = StoreLock::operation(&locks, id, &l)?;
    let catalog_access =
        fs::CatalogLock::acquire(&root, fs::CatalogLockMode::Shared, true, io.clone(), &l)?;
    let staging = root.child(".staging", false, &l)?;
    let catalog = root.child(".catalog", false, &l)?;
    let quarantine = root.child(".quarantine", false, &l)?;
    for directory in [&locks, &staging, &catalog, &quarantine] {
        fs::directory_geometry(directory, unit, &l)?;
    }
    if quarantine.exists(operation, &l)? {
        // A previous attempt can create the quarantine entry before receipt
        // withdrawal or its directory barriers fail. Finish that same bounded
        // operation before reporting quarantine, including an already moved receipt.
        mark_quarantine(&root, operation, &control, &l, &catalog_access)?;
        return Err(RecoveryError::Quarantined { operation_id: id });
    }
    let mut published = root.exists(operation, &l)?;
    let path = if published {
        root.path.join(operation)
    } else {
        staging.path.join(operation)
    };
    let bundle = Dir::open(&path, &l)?;
    let checked = match check_bundle(&bundle, operation, &l) {
        Ok(checked) => checked,
        Err(error) => {
            // Cancellation/deadlines are not corruption; never turn transient
            // failure into an irreversible quarantine decision.
            if matches!(
                error,
                RecoveryError::Verification { .. } | RecoveryError::RefusedInput { .. }
            ) {
                mark_quarantine(&root, operation, &control, &l, &catalog_access)?;
                return Err(RecoveryError::Quarantined { operation_id: id });
            }
            return Err(error);
        }
    };
    // Include every existing byte in this reconciliation allocation, including
    // a preexisting receipt/temp. Nothing is charged to a fresh unlimited pot.
    let mut bytes = checked.bytes;
    for name in [format!("{operation}.json"), format!("{operation}.tmp")] {
        if catalog.exists(&name, &l)? {
            let file = PrivateFile::open(&catalog.path.join(&name), false, &l)?;
            let len = file.file.metadata().map_err(|_| fs::destination(&l))?.len();
            bytes = bytes.checked_add(len).ok_or_else(|| l.verification())?;
        }
    }
    control.charge(bytes, false, &l)?;
    let result = (|| {
        let retained = checked.retained_limits(&l)?;
        let held_limits = &retained;
        checked.flush(&control, held_limits)?;
        bundle.sync(&*io, Directory::StagingBeforePublish, &l)?;
        if !published {
            bundle.sync(&*io, Directory::StagingBeforePublish, &l)?;
            staging.rename_checked(
                operation,
                &root,
                operation,
                &*io,
                RecoveryEvent::Publish,
                &l,
                || {
                    bundle.same_name(&l)?;
                    let latest = check_bundle(&bundle, operation, held_limits)?;
                    if !checked.same_manifest(&latest) {
                        return Err(l.verification());
                    }
                    Ok(())
                },
            )?;
            published = true;
        }
        root.sync(&*io, Directory::PublicationParent, &l)?;
        staging.sync(&*io, Directory::StagingParent, &l)?;
        record(&root, &catalog, checked, &control, &l, &catalog_access)
    })();
    match result {
        Ok(bundle) => Ok(PublicationOutcome::Durable(bundle)),
        Err(error @ RecoveryError::Quarantined { .. }) => Err(error),
        Err(_) if published => Ok(PublicationOutcome::OutcomeUnknown { operation_id: id }),
        Err(error) => Err(error),
    }
}

/// Verify the independent immutable config object under the configured root.
/// Canonical batch IDs and fixed member names provide the entire path grammar.
pub(crate) fn check_shared_config(
    root: &Dir,
    reference: &super::manifest::SharedConfigReference,
    expected_batch: Option<&str>,
    l: &OperationLimits,
) -> Result<Vec<(Member, PrivateFile)>, RecoveryError> {
    use super::manifest::BatchConfigV1;
    if reference.batch_id.is_nil()
        || expected_batch != Some(reference.batch_id.to_string().as_str())
    {
        return Err(l.verification());
    }
    let parent = Dir::open(&root.path.join(".batch-config"), l)?;
    let dir = Dir::open(&parent.path.join(reference.batch_id.to_string()), l)?;
    let descriptor = PrivateFile::open(&dir.path.join("config.json"), false, l)?;
    let (descriptor_bytes, digest) = verify::hash_file(&descriptor.file, l)?;
    if digest != reference.descriptor_sha256 {
        return Err(l.verification());
    }
    let mut models = l.clone();
    let (value, _): (BatchConfigV1, _) = fs::read_json_retained(&dir, "config.json", &mut models)?;
    let l = &models;
    if value.version != 1 || value.batch_id != reference.batch_id || value.members.len() > 3 {
        return Err(l.verification());
    }
    let mut names = vec!["config.json".to_owned()];
    let mut held = vec![(Member::Manifest, descriptor)];
    let mut bytes = descriptor_bytes;
    for member in value.members {
        let (name, kind) = match member.kind {
            MemberKind::ProtectedConfig => ("config.toml", Member::Config),
            MemberKind::Warrants => ("warrants.json", Member::Warrants),
            MemberKind::VaultCiphertext => ("vault.enc", Member::VaultCiphertext),
            _ => return Err(l.verification()),
        };
        if member.relative_name != name || names.iter().any(|n| n == name) {
            return Err(l.verification());
        }
        names.push(name.into());
        bytes = bytes
            .checked_add(member.bytes)
            .ok_or_else(|| l.verification())?;
        l.quota(bytes, l.byte_limit.min(512 << 20), "batch configuration")?;
        let file = PrivateFile::open(&dir.path.join(name), false, l)?;
        if verify::hash_file(&file.file, l)? != (member.bytes, member.sha256) {
            return Err(l.verification());
        }
        held.push((kind, file));
    }
    names.sort();
    if dir.members(l)? != names {
        return Err(l.verification());
    }
    for (_, file) in &held {
        file.unchanged(l)?;
    }
    dir.same_name(l)?;
    parent.same_name(l)?;
    root.same_name(l)?;
    Ok(held)
}

pub(crate) fn retention_manifest(
    bundle: &VerifiedBundle,
    l: &OperationLimits,
) -> Result<BundleManifestV1, RecoveryError> {
    Ok(retention_checked(bundle, l)?.manifest)
}
pub(crate) fn retention_checked(
    bundle: &VerifiedBundle,
    l: &OperationLimits,
) -> Result<CheckedBundle, RecoveryError> {
    let dir = Dir::open(bundle.path(), l)?;
    let operation = dir
        .path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| l.verification())?;
    let root = Dir::open(dir.path.parent().ok_or_else(|| l.verification())?, l)?;
    let catalog = Dir::open(&root.path.join(".catalog"), l)?;
    let checked = check_bundle(&dir, operation, l)?;
    let retained = checked.retained_limits(l)?;
    let receipt = checked_receipt(&catalog, &format!("{operation}.json"), &checked, &retained)?;
    checked.validate_at(&dir, &retained)?;
    receipt.unchanged(&retained)?;
    Ok(checked)
}

/// Read-only completed selection. A directory alone is never proof of publication.
pub fn select_completed_bundle(
    operation: uuid::Uuid,
    filesystem: &dyn RecoveryFs,
    l: &OperationLimits,
) -> Result<VerifiedBundle, RecoveryError> {
    if operation.is_nil() {
        return Err(l.verification());
    }
    let root = Dir::open(filesystem.root(), l)?;
    let quarantine = Dir::open(&root.path.join(".quarantine"), l)?;
    if quarantine.exists(&operation.to_string(), l)? {
        return Err(RecoveryError::Quarantined {
            operation_id: operation,
        });
    }
    let bundle = VerifiedBundle {
        path: root.path.join(operation.to_string()),
    };
    let manifest = retention_manifest(&bundle, l)?;
    if manifest.operation_id != operation.to_string() {
        return Err(l.verification());
    }
    root.same_name(l)?;
    Ok(bundle)
}

#[cfg(all(test, target_os = "linux"))]
mod memory_tests {
    use super::*;
    use crate::recovery::{
        BindingInput, CancelToken, CaptureBudget, InventoryInputs, ProfileKind,
        backup::{BackupRequest, backup},
        collect_inventory,
        fs::NativeRecoveryFs,
        manifest::Availability,
    };
    use std::{
        fs as disk,
        os::unix::fs::PermissionsExt,
        time::{Duration, Instant},
    };

    #[test]
    fn nested_catalog_recheck_retains_outer_and_fresh_json_models() {
        let root = crate::recovery::test_support::fixture_root();
        let source = root.path().join("source.fls");
        let backups = root.path().join("backups");
        disk::create_dir(&backups).unwrap();
        disk::set_permissions(&backups, disk::Permissions::from_mode(0o700)).unwrap();
        let owner = crate::FerroStore::open(&source).unwrap();
        let inventory = collect_inventory(&InventoryInputs {
            roots: vec![root.path().to_path_buf()],
            bindings: vec![BindingInput::config(
                "fixture",
                source.clone(),
                Some("ferro".into()),
                "fixture",
            )],
            ..Default::default()
        })
        .unwrap();
        let store = inventory
            .stores
            .iter()
            .find(|s| s.identity.canonical_path.as_deref() == Some(source.as_path()))
            .unwrap()
            .identity
            .clone();
        let generous = OperationLimits::new(
            Instant::now() + Duration::from_secs(180),
            256 << 20,
            1 << 30,
            CancelToken::new(),
        );
        let request = BackupRequest {
            profile: ValidationProfile::new(ProfileKind::SmallEmpty, store.catalog_key.clone()),
            store,
            source_map_sha256: "a".repeat(64),
            operation_id: generous.operation_id,
            batch_id: None,
            destination: backups.clone(),
            capture: CaptureBudget::new(
                generous.deadline,
                256 << 20,
                512 << 20,
                Duration::from_secs(1),
            ),
            limits: generous.clone(),
            image_byte_limit: None,
            metadata_byte_limit: 1 << 20,
            space_margin_bytes: 4096,
            configs: vec![],
            excluded_vault_key: None,
            code_index_configuration: Availability::Unavailable,
        };
        let filesystem = NativeRecoveryFs::new(backups, generous.clone());
        let PublicationOutcome::Durable(bundle) =
            backup(&owner, &request, &filesystem, &CancelToken::new()).unwrap()
        else {
            panic!("fixture publication not durable")
        };
        drop(owner);
        let operation = request.operation_id.to_string();
        let sizes = [
            "manifest.json",
            "validation.json",
            "profile.json",
            "report.json",
        ]
        .map(|name| disk::metadata(bundle.path().join(name)).unwrap().len() * 32);
        let [manifest, expected, profile, report] = sizes;
        let json = expected + profile + report;
        // Contract bounds derive only from fixture lengths and fixed allowances,
        // never the implementation's admission peak or CheckedBundle::memory_bytes.
        let handles = 128 << 10;
        let scratch = 65536;
        let standalone = (manifest + handles + json + scratch).max(2 * manifest + handles + json);
        // Also permit every isolated decode with an outer manifest/handle set;
        // refusal must require the simultaneously live freshly decoded models.
        let isolated_nested =
            manifest + 2 * handles + (sizes.into_iter().max().unwrap() + scratch).max(manifest);
        let cap = standalone.max(isolated_nested);
        let nested = standalone + manifest + handles;
        assert!(standalone <= cap && cap < nested);
        let mut l = OperationLimits::new(
            Instant::now() + Duration::from_secs(180),
            256 << 20,
            cap,
            CancelToken::new(),
        );
        l.operation_id = request.operation_id;
        let dir = Dir::open(bundle.path(), &l).unwrap();
        let checked = check_bundle(&dir, &operation, &l)
            .expect("standalone catalog check must fit the fixture-derived cap");
        let nested_limits = checked.retained_limits(&l).unwrap();
        assert!(
            matches!(
                check_bundle(&dir, &operation, &nested_limits),
                Err(RecoveryError::Quota {
                    resource: "working memory",
                    ..
                })
            ),
            "nested catalog recheck must retain outer and fresh owned JSON models"
        );
        // The outer manifest and bound handles remain owned after refusal.
        assert_eq!(checked.manifest.operation_id, operation);
        checked.validate_at(&dir, &l).unwrap();
    }
}
