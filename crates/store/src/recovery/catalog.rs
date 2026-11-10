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
    manifest: BundleManifestV1,
    manifest_sha256: String,
    bytes: u64,
    metadata_bytes: u64,
    held: Vec<(Member, PrivateFile)>,
    directory_identity: (u64, u64),
}
impl CheckedBundle {
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
        for (member, file) in &self.held {
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
    let manifest: BundleManifestV1 = fs::read_json(dir, "manifest.json", l)?;
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
    l.admit_memory(
        metadata_bytes
            .checked_mul(64)
            .and_then(|n| n.checked_add(65536))
            .ok_or_else(|| l.verification())?,
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
    let expected: ValidationExpectations = fs::read_json(dir, "validation.json", l)?;
    let profile: ValidationProfile = fs::read_json(dir, "profile.json", l)?;
    let report: VerificationReport = fs::read_json(dir, "report.json", l)?;
    if report.operation_id != operation {
        return Err(l.verification());
    }
    let rebuilt = BundleManifestV1::verified_unpublished(
        &expected,
        &profile,
        &report,
        ManifestContext {
            batch_id: manifest.batch_id.clone(),
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
        metadata_bytes,
        held,
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
fn checked_receipt(
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
) -> Result<VerifiedBundle, RecoveryError> {
    let operation = &checked.manifest.operation_id;
    let result = record_checked(root, catalog, &checked, control, l);
    match result {
        Err(RecoveryError::Verification { .. } | RecoveryError::RefusedInput { .. }) => {
            mark_quarantine(root, operation, control, l)?;
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
            fs::write_json(catalog, &temporary, &receipt, Member::Receipt, control, l)?;
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

fn mark_quarantine(
    root: &Dir,
    operation: &str,
    control: &Arc<PublicationIo>,
    l: &OperationLimits,
) -> Result<(), RecoveryError> {
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
        mark_quarantine(&root, operation, &control, &l)?;
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
                mark_quarantine(&root, operation, &control, &l)?;
                return Err(RecoveryError::Quarantined { operation_id: id });
            }
            return Err(error);
        }
    };
    // Include every existing byte in this reconciliation allocation, including
    // a preexisting receipt/temp. Nothing is charged to a fresh unlimited pot.
    let mut bytes = checked.bytes;
    let mut metadata_bytes = checked.metadata_bytes;
    for name in [format!("{operation}.json"), format!("{operation}.tmp")] {
        if catalog.exists(&name, &l)? {
            let file = PrivateFile::open(&catalog.path.join(&name), false, &l)?;
            let len = file.file.metadata().map_err(|_| fs::destination(&l))?.len();
            bytes = bytes.checked_add(len).ok_or_else(|| l.verification())?;
            metadata_bytes = metadata_bytes
                .checked_add(len)
                .ok_or_else(|| l.verification())?;
        }
    }
    l.admit_memory(
        metadata_bytes
            .checked_mul(64)
            .and_then(|n| n.checked_add(65536))
            .ok_or_else(|| l.verification())?,
    )?;
    control.charge(bytes, false, &l)?;
    let result = (|| {
        checked.flush(&control, &l)?;
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
                    let latest = check_bundle(&bundle, operation, &l)?;
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
        record(&root, &catalog, checked, &control, &l)
    })();
    match result {
        Ok(bundle) => Ok(PublicationOutcome::Durable(bundle)),
        Err(error @ RecoveryError::Quarantined { .. }) => Err(error),
        Err(_) if published => Ok(PublicationOutcome::OutcomeUnknown { operation_id: id }),
        Err(error) => Err(error),
    }
}
