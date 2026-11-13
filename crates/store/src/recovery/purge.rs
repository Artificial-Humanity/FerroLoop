//! Exact-operation quarantine deletion under administrative exclusion.
use super::{OperationLimits, RecoveryError, admission::OfflineLease, fs::RecoveryFs};
use serde::{Deserialize, Serialize};
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReferenceClosureEvidence {
    pub catalog_set_sha256: String,
    pub non_ferroloop_writers_excluded: bool,
    pub evidence_sha256: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PurgeRequest {
    pub operation_id: uuid::Uuid,
    pub confirm: Option<uuid::Uuid>,
    pub reference_closure: Option<ReferenceClosureEvidence>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PurgeMember {
    pub relative_name: String,
    pub identity: (u64, u64),
    pub bytes: u64,
    pub sha256: String,
    pub state: PurgeMemberState,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PurgeMemberState {
    Present,
    Removed,
    OutcomeUnknown,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PurgeReport {
    pub operation_id: uuid::Uuid,
    pub members: Vec<PurgeMember>,
    pub catalog_set_sha256: String,
    pub pending_receipt_withdrawal: bool,
    pub audit_durable: bool,
    pub complete: bool,
}
use super::{
    admission::{FileBinding, OfflineAction, OperationOwnership},
    catalog,
    control::DurableBatchV1,
    fs::{
        self, CatalogLock, CatalogLockMode, Dir, Directory, Member, PublicationIo, RecoveryEvent,
        StoreLock,
    },
    manifest::{self, BundleManifestV1},
    verify::{self, PrivateFile},
};
use std::{collections::BTreeSet, os::fd::AsRawFd, path::Path, sync::Arc};
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PurgeGeometryV1 {
    staged: bool,
    target_identity: (u64, u64),
    quarantine_identity: (u64, u64),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PurgeAuditV1 {
    version: u32,
    operation_id: uuid::Uuid,
    manifest_sha256: String,
    catalog_set_sha256: String,
    actor: String,
    at_unix_ms: u128,
    administrative_evidence: ReferenceClosureEvidence,
    // Old intents without directory evidence cannot safely authorize replay once
    // their files are gone. Decode them for diagnostics, but refuse replay.
    #[serde(default)]
    geometry: Option<PurgeGeometryV1>,
    members: Vec<PurgeMember>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PurgeCompletionV1 {
    version: u32,
    operation_id: uuid::Uuid,
    audit_sha256: String,
    member_index: Option<usize>,
}
fn canonical_op(text: &str, l: &OperationLimits) -> Result<uuid::Uuid, RecoveryError> {
    let op = uuid::Uuid::parse_str(text).map_err(|_| l.verification())?;
    if op.is_nil() || text != op.to_string() {
        return Err(l.verification());
    }
    Ok(op)
}
fn member(root: &Dir, path: &Path, l: &OperationLimits) -> Result<PurgeMember, RecoveryError> {
    let file = PrivateFile::open(path, false, l)?;
    let (bytes, sha256) = verify::hash_file(&file.file, l)?;
    file.unchanged(l)?;
    let relative_name = path
        .strip_prefix(&root.path)
        .map_err(|_| l.verification())?
        .to_str()
        .ok_or_else(|| l.verification())?
        .to_owned();
    Ok(PurgeMember {
        relative_name,
        identity: file.identity(),
        bytes,
        sha256,
        state: PurgeMemberState::Present,
    })
}
// The same exact-name geometry is used before initial admission and on replay.
// Keep both descriptors alive through all unlink/completion boundaries.
struct PurgeTarget {
    bundle: Dir,
    quarantine: Dir,
    staged: bool,
}
impl PurgeTarget {
    fn evidence(&self, l: &OperationLimits) -> Result<PurgeGeometryV1, RecoveryError> {
        Ok(PurgeGeometryV1 {
            staged: self.staged,
            target_identity: fs::identity(&self.bundle.file, l)?,
            quarantine_identity: fs::identity(&self.quarantine.file, l)?,
        })
    }
    fn validate(
        &self,
        root: &Dir,
        catalog: &Dir,
        op: uuid::Uuid,
        l: &OperationLimits,
    ) -> Result<(), RecoveryError> {
        root.same_name(l)?;
        catalog.same_name(l)?;
        self.bundle.same_name(l)?;
        self.quarantine.same_name(l)?;
        let staged = Dir::open(&root.path.join(".staging"), l)?;
        let text = op.to_string();
        if root.exists(&text, l)? == self.staged
            || staged.exists(&text, l)? != self.staged
            || catalog.exists(&format!("{op}.json"), l)?
        {
            return Err(l.verification());
        }
        Ok(())
    }
}
fn open_target(
    root: &Dir,
    op: uuid::Uuid,
    expected: Option<&PurgeGeometryV1>,
    l: &OperationLimits,
) -> Result<PurgeTarget, RecoveryError> {
    root.same_name(l)?;
    let staging = Dir::open(&root.path.join(".staging"), l)?;
    let text = op.to_string();
    let staged = match (root.exists(&text, l)?, staging.exists(&text, l)?) {
        (true, false) => false,
        (false, true) => true,
        _ => return Err(l.verification()),
    };
    let bundle = Dir::open(
        &if staged {
            staging.path.join(&text)
        } else {
            root.path.join(&text)
        },
        l,
    )?;
    let quarantine = Dir::open(&root.path.join(".quarantine").join(&text), l)?;
    let target = PurgeTarget {
        bundle,
        quarantine,
        staged,
    };
    if let Some(binding) = expected {
        let actual = target.evidence(l)?;
        if actual.staged != binding.staged
            || actual.target_identity != binding.target_identity
            || actual.quarantine_identity != binding.quarantine_identity
        {
            return Err(l.verification());
        }
    }
    staging.same_name(l)?;
    root.same_name(l)?;
    Ok(target)
}
// Unlike fs::read_json, this decodes the descriptor the caller already holds.
// Retain the decoded-model reservation for all later hashing/geometry scratch.
fn read_held_json<T: serde::de::DeserializeOwned>(
    held: &PrivateFile,
    l: &OperationLimits,
) -> Result<(T, OperationLimits), RecoveryError> {
    let len = held.file.metadata().map_err(|_| l.verification())?.len();
    l.quota(len, l.byte_limit, "metadata bytes")?;
    let retained = len.checked_mul(32).ok_or_else(|| l.verification())?;
    let scope = l.with_retained_memory(retained)?;
    scope.admit_memory(len.checked_add(65536).ok_or_else(|| l.verification())?)?;
    let mut bytes = vec![0; usize::try_from(len).map_err(|_| l.verification())?];
    for (index, chunk) in bytes.chunks_mut(65536).enumerate() {
        scope.check()?;
        fs::read_exact_at(&held.file, chunk, index as u64 * 65536, &scope)?;
    }
    let value = serde_json::from_slice(&bytes).map_err(|_| l.verification())?;
    held.unchanged(&scope)?;
    Ok((value, scope))
}
struct InspectedTarget {
    manifest: BundleManifestV1,
    hash: String,
    members: Vec<PurgeMember>,
    pending: bool,
    geometry: PurgeGeometryV1,
}
fn target(
    root: &Dir,
    operation: uuid::Uuid,
    l: &OperationLimits,
) -> Result<InspectedTarget, RecoveryError> {
    let op = operation.to_string();
    let target = open_target(root, operation, None, l)?;
    let dir = &target.bundle;
    let quarantine = &target.quarantine;
    let checked = catalog::check_bundle(dir, &op, l)?;
    let retained = checked.retained_limits(l)?;
    let l = &retained;
    let catalog = Dir::open(&root.path.join(".catalog"), l)?;
    let receipt_name = format!("{op}.json");
    let pending = catalog.exists(&receipt_name, l)?;
    let receipt = if pending {
        if quarantine.exists("receipt.json", l)? {
            return Err(l.verification());
        }
        catalog::checked_receipt(&catalog, &receipt_name, &checked, l)?
    } else {
        catalog::checked_receipt(quarantine, "receipt.json", &checked, l)?
    };
    if quarantine.members(l)?
        != if pending {
            vec![]
        } else {
            vec!["receipt.json".to_owned()]
        }
    {
        return Err(l.verification());
    }
    let mut members = Vec::new();
    for name in dir.members(l)? {
        members.push(member(root, &dir.path.join(name), l)?);
    }
    // Receipt is reported at its eventual, operation-owned quarantine name. The
    // dry run neither withdraws it nor claims that this pending move happened.
    let receipt_path = if pending {
        catalog.path.join(&receipt_name)
    } else {
        quarantine.path.join("receipt.json")
    };
    let mut receipt_member = member(root, &receipt_path, l)?;
    receipt_member.relative_name = format!(".quarantine/{op}/receipt.json");
    members.push(receipt_member);
    // Keep the manifest until all other files are accounted for, making ordinary
    // interrupted deletions inspectable. The durable audit handles the final cut.
    members.sort_by_key(|m| {
        (
            m.relative_name.ends_with("/manifest.json"),
            m.relative_name.clone(),
        )
    });
    receipt.unchanged(l)?;
    let geometry = target.evidence(l)?;
    target.bundle.same_name(l)?;
    target.quarantine.same_name(l)?;
    Ok(InspectedTarget {
        manifest: checked.manifest,
        hash: checked.manifest_sha256,
        members,
        pending,
        geometry,
    })
}
fn reference_hit(manifest: &BundleManifestV1, op: uuid::Uuid) -> bool {
    let name = op.to_string();
    manifest.operation_id == name
        || manifest
            .reference_dependencies
            .iter()
            .any(|r| r.catalog_identity == name)
        || manifest
            .shared_config
            .as_ref()
            .is_some_and(|r| r.batch_id == op)
}
/// Only known live receipt/manifests and completed recovery-set records are
/// reference grammar. Journals, scheduler history and arbitrary strings are not.
// Covers each retained tuple's strings, Vec capacity and allocator overhead.
// Canonical UUID paths and SHA-256 text are bounded well below this allowance.
const CLOSURE_ENTRY_MEMORY: u64 = 4096;
struct ClosureEvidence {
    entries: Vec<(String, String)>,
}
impl ClosureEvidence {
    fn reserve(
        &mut self,
        additional: usize,
        l: &OperationLimits,
    ) -> Result<OperationLimits, RecoveryError> {
        let needed = self
            .entries
            .len()
            .checked_add(additional)
            .ok_or_else(|| l.verification())?;
        let capacity = self.entries.capacity().max(needed);
        let bytes = (capacity as u64)
            .checked_mul(CLOSURE_ENTRY_MEMORY)
            .ok_or_else(|| l.verification())?;
        let scoped = l.with_retained_memory(
            bytes
                .checked_add(2 * 65536)
                .ok_or_else(|| l.verification())?,
        )?;
        // Includes streaming hash/serialization and the two directory iterators.
        // Admission happens before Vec growth, format!, checksum strings or clones.
        scoped.admit_memory(65536)?;
        if needed > self.entries.capacity() {
            self.entries
                .try_reserve_exact(needed - self.entries.len())
                .map_err(|_| l.verification())?;
        }
        Ok(scoped)
    }
}
fn retain_restore_pins(
    catalog: &Dir,
    target: uuid::Uuid,
    evidence: &mut ClosureEvidence,
    l: &OperationLimits,
) -> Result<(), RecoveryError> {
    super::restore::check_restore_pins(
        catalog,
        target,
        l,
        evidence,
        |evidence, additional, limits| evidence.reserve(additional, limits),
        |evidence, name, held, pin_limits| {
            // Capacity and iterator scratch were reserved before the pin read;
            // pin_limits also retains the decoded pin during hashing/string growth.
            let hash = verify::hash_file(&held.file, pin_limits)?.1;
            evidence.entries.push((format!("restore-pin/{name}"), hash));
            Ok(())
        },
    )
}
fn checked_memory(
    checked: &catalog::CheckedBundle,
    l: &OperationLimits,
) -> Result<u64, RecoveryError> {
    checked.memory_bytes(l)
}
// A completed set may outlive withdrawal of a member's receipt. Exactly one
// publication location is authority; retain its directory/receipt handles until
// all injectable receipt reads have finished, then recheck both locations.
fn check_batch_receipt(
    root: &Dir,
    catalog: &Dir,
    operation: &str,
    checked: &catalog::CheckedBundle,
    l: &OperationLimits,
) -> Result<(), RecoveryError> {
    // Covers the additional owned paths/descriptor bookkeeping and streaming
    // directory iteration, alongside the already-retained bundle/batch models.
    let l = l.with_retained_memory(6 * CLOSURE_ENTRY_MEMORY + 65536)?;
    canonical_op(operation, &l)?;
    let quarantine_root = Dir::open(&root.path.join(".quarantine"), &l)?;
    let quarantine = if quarantine_root.exists(operation, &l)? {
        Some(Dir::open(&quarantine_root.path.join(operation), &l)?)
    } else {
        None
    };
    let name = format!("{operation}.json");
    let live = catalog.exists(&name, &l)?;
    let quarantined = match &quarantine {
        Some(dir) => dir.exists("receipt.json", &l)?,
        None => false,
    };
    if live == quarantined {
        return Err(l.verification());
    }
    let validate_locations = || {
        root.same_name(&l)?;
        catalog.same_name(&l)?;
        quarantine_root.same_name(&l)?;
        if catalog.exists(&name, &l)? != live
            || quarantine_root.exists(operation, &l)? != quarantine.is_some()
        {
            return Err(l.verification());
        }
        if let Some(dir) = &quarantine {
            let mut count = 0;
            dir.visit_names(&l, |entry| {
                if live || entry != "receipt.json" {
                    return Err(l.verification());
                }
                count += 1;
                Ok(())
            })?;
            if count != usize::from(!live) {
                return Err(l.verification());
            }
        }
        Ok(())
    };
    validate_locations()?;
    let receipt = if live {
        catalog::checked_receipt(catalog, &name, checked, &l)?
    } else {
        catalog::checked_receipt(
            quarantine.as_ref().ok_or_else(|| l.verification())?,
            "receipt.json",
            checked,
            &l,
        )?
    };
    validate_locations()?;
    receipt.unchanged(&l)
}
fn closure(
    root: &Dir,
    op: uuid::Uuid,
    members: &[PurgeMember],
    _manifest_hash: &str,
    l: &OperationLimits,
) -> Result<String, RecoveryError> {
    // The input members coexist throughout both scans. Charge
    // before collect/clone, including a second caller-owned member list.
    let retained = (members.len() as u64)
        .checked_mul(3)
        .and_then(|n| n.checked_add(1))
        .and_then(|n| n.checked_mul(CLOSURE_ENTRY_MEMORY))
        .ok_or_else(|| l.verification())?;
    let l = l.with_retained_memory(retained)?;
    let catalog = Dir::open(&root.path.join(".catalog"), &l)?;
    let mut evidence = ClosureEvidence {
        entries: Vec::new(),
    };
    retain_restore_pins(&catalog, op, &mut evidence, &l)?;
    catalog.visit_names(&l, |name| {
        if name == "restore-pins" {
            return Ok(());
        }
        if name.starts_with("purge-") {
            if let Some(published) = name.strip_suffix(".tmp") {
                validate_audit_name(published, &l)
                    .map_err(|error| catalog_name_error(error, &catalog, name, &l))?;
                // Catalog EX excludes a live audit writer. A canonical private
                // regular temp is unpublished, so do not decode or retain it.
                let held = PrivateFile::open(&catalog.path.join(name), false, &l)?;
                return held.unchanged(&l);
            }
            validate_audit_name(name, &l)
                .map_err(|error| catalog_name_error(error, &catalog, name, &l))?;
            return Ok(());
        }
        let text = name
            .strip_suffix(".json")
            .ok_or_else(|| catalog_name_error(l.verification(), &catalog, name, &l))?;
        let other = canonical_op(text, &l)
            .map_err(|error| catalog_name_error(error, &catalog, name, &l))?;
        if other == op {
            return Ok(());
        }
        let scan = evidence.reserve(2, &l)?;
        let dir = Dir::open(&root.path.join(text), &scan)?;
        let checked = catalog::check_bundle(&dir, text, &scan)?;
        let receipt_limits = scan.with_retained_memory(checked_memory(&checked, &scan)?)?;
        let receipt = catalog::checked_receipt(&catalog, name, &checked, &receipt_limits)?;
        if reference_hit(&checked.manifest, op) {
            return Err(fs::refused(
                &scan,
                "administrative reference closure includes target operation or bundle name",
            ));
        }
        evidence.entries.push((
            format!("receipt/{name}"),
            verify::hash_file(&receipt.file, &receipt_limits)?.1,
        ));
        evidence
            .entries
            .push((format!("manifest/{text}"), checked.manifest_sha256));
        receipt.unchanged(&receipt_limits)?;
        Ok(())
    })?;
    root.visit_names(&l, |name| {
        let Some(text) = name
            .strip_prefix(".batch-")
            .and_then(|s| s.strip_suffix(".json"))
        else {
            return Ok(());
        };
        let batch_id = canonical_op(text, &l)?;
        let scan = evidence.reserve(1, &l)?;
        let held = PrivateFile::open(&root.path.join(name), false, &scan)?;
        let batch_bytes = held.file.metadata().map_err(|_| scan.verification())?.len();
        let batch: DurableBatchV1 = fs::read_json(root, name, &scan)?;
        // Keep the decoded batch charged while each member/receipt is checked.
        let batch_memory = batch_bytes
            .checked_mul(32)
            .and_then(|n| n.checked_add(65536))
            .ok_or_else(|| scan.verification())?;
        let batch_limits = scan.with_retained_memory(batch_memory)?;
        if batch.version != 1 || batch.batch_id != batch_id || batch.members.is_empty() {
            return Err(l.verification());
        }
        let mut seen = BTreeSet::new();
        for m in &batch.members {
            let seen_memory = (seen.len() as u64)
                .checked_add(1)
                .and_then(|n| n.checked_mul(CLOSURE_ENTRY_MEMORY))
                .ok_or_else(|| l.verification())?;
            let member_limits = batch_limits.with_retained_memory(seen_memory)?;
            member_limits.charge_query_work(1)?;
            if !seen.insert(m.operation_id)
                || m.operation_id == op
                || m.shared_config.as_ref().is_some_and(|r| r.batch_id == op)
            {
                return Err(fs::refused(
                    &member_limits,
                    "completed recovery set references quarantine material",
                ));
            }
            let text = m.operation_id.to_string();
            let dir = Dir::open(&root.path.join(&text), &member_limits)?;
            let checked = catalog::check_bundle(&dir, &text, &member_limits)
                .map_err(|error| error.at_metadata_entry(|| std::path::PathBuf::from(&text)))?;
            let receipt_limits =
                member_limits.with_retained_memory(checked_memory(&checked, &member_limits)?)?;
            if checked.manifest_sha256 != m.manifest_sha256
                || checked.manifest.catalog_store_identity != m.catalog_identity
                || checked.manifest.published_sequence != m.sequence
                || checked.manifest.shared_config != m.shared_config
                || reference_hit(&checked.manifest, op)
            {
                return Err(l.verification());
            }
            check_batch_receipt(root, &catalog, &text, &checked, &receipt_limits)?;
        }
        drop(seen);
        drop(batch);
        evidence
            .entries
            .push((name.to_owned(), verify::hash_file(&held.file, &scan)?.1));
        held.unchanged(&scan)?;
        Ok(())
    })?;
    let serialize = evidence.reserve(0, &l)?;
    // In-place sort needs no second evidence array; serialization streams.
    evidence.entries.sort_unstable();
    verify::serialized_checksum(&evidence.entries, &serialize).map(|(_, hash)| hash)
}

// Invalid name grammar used to refuse before opening. For diagnostics only,
// verify a private O_NOFOLLOW file before naming that lexical entry. Failure
// of this extra check preserves the original refusal and never exposes a target.
fn catalog_name_error(
    error: RecoveryError,
    catalog: &Dir,
    name: &str,
    l: &OperationLimits,
) -> RecoveryError {
    if matches!(error, RecoveryError::Verification { .. })
        && PrivateFile::open(&catalog.path.join(name), false, l).is_ok()
    {
        error.at_metadata_entry(|| Path::new(".catalog").join(name))
    } else {
        error
    }
}
fn validate_audit_name(name: &str, l: &OperationLimits) -> Result<(), RecoveryError> {
    let text = name
        .strip_prefix("purge-")
        .ok_or_else(|| l.verification())?;
    let (op, suffix) = text.split_once('.').ok_or_else(|| l.verification())?;
    canonical_op(op, l)?;
    if suffix == "intent.json" || suffix == "complete.json" {
        return Ok(());
    }
    let index = suffix
        .strip_prefix("member-")
        .and_then(|s| s.strip_suffix(".json"))
        .ok_or_else(|| l.verification())?;
    if index
        .parse::<usize>()
        .ok()
        .is_none_or(|n| n.to_string() != index)
    {
        return Err(l.verification());
    }
    Ok(())
}
// Called only by confirmed purge under physical, operation and catalog EX locks.
// A stopped writer's own exact temp is disposable without decoding; final names
// remain immutable, typed authority and are never overwritten or repaired.
fn publish_audit_row<T: Serialize>(
    catalog: &Dir,
    name: &str,
    value: &T,
    control: &Arc<PublicationIo>,
    l: &OperationLimits,
    authority: impl Fn() -> Result<(), RecoveryError>,
) -> Result<(), RecoveryError> {
    validate_audit_name(name, l)?;
    let temp = format!("{name}.tmp");
    if catalog.exists(&temp, l)? {
        let held = PrivateFile::open(&catalog.path.join(&temp), false, l)?;
        catalog.remove_file_checked(&temp, held.identity(), l, |last| {
            // remove_file_checked has completed its last injected open.
            authority()?;
            held.unchanged(l)?;
            last.unchanged(l)?;
            if catalog.exists(name, l)? {
                return Err(l.verification());
            }
            Ok(())
        })?;
    }
    let expected = fs::write_json(catalog, &temp, value, Member::PurgeAudit, control, l)?;
    let prepared = PrivateFile::open(&catalog.path.join(&temp), false, l)?;
    if verify::hash_file(&prepared.file, l)? != expected {
        return Err(l.verification());
    }
    prepared.unchanged(l)?;
    catalog.rename_checked(
        &temp,
        catalog,
        name,
        &*control.io,
        RecoveryEvent::Publish,
        l,
        || {
            // All injectable events/reads precede the clock and held authority.
            authority()?;
            prepared.unchanged(l)
        },
    )?;
    // Rename changes ctime; bind the final descriptor to the same inode/bytes.
    let published = PrivateFile::open(&catalog.path.join(name), false, l)?;
    if published.identity() != prepared.identity()
        || verify::hash_file(&published.file, l)? != expected
    {
        return Err(l.verification());
    }
    published.unchanged(l)?;
    catalog.sync(&*control.io, Directory::CatalogParent, l)?;
    authority()?;
    published.unchanged(l)?;
    if catalog.exists(&temp, l)? {
        return Err(l.verification());
    }
    Ok(())
}
fn active_operation(
    root: &Dir,
    op: uuid::Uuid,
    l: &OperationLimits,
) -> Result<Option<PrivateFile>, RecoveryError> {
    let locks = Dir::open(&root.path.join(".locks"), l)?;
    let name = format!("operation-{op}.lock");
    if !locks.exists(&name, l)? {
        return Ok(None);
    }
    let held = PrivateFile::open(&locks.path.join(name), false, l)?;
    // This is an activity refusal only. It establishes no administrative exclusion.
    if unsafe { libc::flock(held.file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(RecoveryError::ConcurrentBackup { operation_id: op });
    }
    held.unchanged(l)?;
    Ok(Some(held))
}
fn audit_members_valid(audit: &PurgeAuditV1, l: &OperationLimits) -> Result<(), RecoveryError> {
    let op = audit.operation_id.to_string();
    let geometry = audit.geometry.as_ref().ok_or_else(|| l.verification())?;
    let mut seen = BTreeSet::new();
    let mut receipt = 0;
    let mut manifest = 0;
    for m in &audit.members {
        let path = Path::new(&m.relative_name);
        let parts = path
            .components()
            .map(|p| p.as_os_str().to_str().ok_or_else(|| l.verification()))
            .collect::<Result<Vec<_>, _>>()?;
        let name = match parts.as_slice() {
            [p, n] if *p == op && !geometry.staged => *n,
            [".staging", p, n] if *p == op && geometry.staged => *n,
            [".quarantine", p, "receipt.json"] if *p == op => {
                receipt += 1;
                "receipt.json"
            }
            _ => return Err(l.verification()),
        };
        if !matches!(
            name,
            "image.fls"
                | "manifest.json"
                | "validation.json"
                | "profile.json"
                | "report.json"
                | "config.toml"
                | "warrants.json"
                | "vault.enc"
                | "index-config.json"
                | "receipt.json"
        ) || !seen.insert(m.relative_name.clone())
            || !manifest::sha256(&m.sha256)
            || m.state != PurgeMemberState::Present
        {
            return Err(l.verification());
        }
        if name == "manifest.json" {
            manifest += 1;
            if m.sha256 != audit.manifest_sha256 {
                return Err(l.verification());
            }
        }
    }
    if receipt != 1 || manifest != 1 || audit.members.len() > 10 {
        return Err(l.verification());
    }
    Ok(())
}
fn verify_present(
    root: &Dir,
    m: &PurgeMember,
    l: &OperationLimits,
) -> Result<PrivateFile, RecoveryError> {
    let held = PrivateFile::open(&root.path.join(&m.relative_name), false, l)?;
    if held.identity() != m.identity
        || verify::hash_file(&held.file, l)? != (m.bytes, m.sha256.clone())
    {
        return Err(l.verification());
    }
    held.unchanged(l)?;
    Ok(held)
}
fn replay_target(
    root: &Dir,
    catalog: &Dir,
    audit: &PurgeAuditV1,
    hash: &str,
    l: &OperationLimits,
) -> Result<PurgeTarget, RecoveryError> {
    audit_members_valid(audit, l)?;
    root.same_name(l)?;
    let binding = audit.geometry.as_ref().ok_or_else(|| l.verification())?;
    let target = open_target(root, audit.operation_id, Some(binding), l)?;
    target.validate(root, catalog, audit.operation_id, l)?;
    // Directory names must be the exact surviving subset of the admitted audit.
    // No unrelated entry is adopted, including after all audited files are gone.
    for dir in [&target.bundle, &target.quarantine] {
        dir.visit_names(l, |name| {
            if !audit
                .members
                .iter()
                .any(|m| root.path.join(&m.relative_name) == dir.path.join(name))
            {
                return Err(l.verification());
            }
            Ok(())
        })?;
    }
    let mut manifest_absent = false;
    let mut all_nonmanifest_complete = true;
    for (index, m) in audit.members.iter().enumerate() {
        l.check()?;
        let manifest = m.relative_name.ends_with("/manifest.json");
        let path = root.path.join(&m.relative_name);
        let parent = if path.parent() == Some(target.bundle.path.as_path()) {
            &target.bundle
        } else if path.parent() == Some(target.quarantine.path.as_path()) {
            &target.quarantine
        } else {
            return Err(l.verification());
        };
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| l.verification())?;
        let completion = format!("purge-{}.member-{index}.json", audit.operation_id);
        let done = catalog.exists(&completion, l)?;
        if done {
            check_completion(
                catalog,
                &completion,
                hash,
                Some(index),
                audit.operation_id,
                l,
            )?;
        }
        if parent.exists(name, l)? {
            verify_present(root, m, l)?;
            if done {
                return Err(l.verification());
            }
            if !manifest {
                all_nonmanifest_complete = false;
            }
        } else if manifest {
            manifest_absent = true;
        } else if !done {
            // A previous unlink may have succeeded before parent sync/completion.
            // Keep it OutcomeUnknown until the mutating path syncs and records it.
            all_nonmanifest_complete = false;
        }
    }
    if manifest_absent && !all_nonmanifest_complete {
        return Err(l.verification());
    }
    target.validate(root, catalog, audit.operation_id, l)?;
    Ok(target)
}
// Retain completion descriptors and original bindings through all injected reads.
fn hold_manifest_predecessors(
    catalog: &Dir,
    audit: &PurgeAuditV1,
    hash: &str,
    l: &OperationLimits,
) -> Result<(Vec<PrivateFile>, OperationLimits), RecoveryError> {
    // Charge full bounded vector capacity and owned path/name buffers before
    // allocation. Each temporary decode and final hash adds to this same scope.
    let per_entry = (catalog.path.as_os_str().len() as u64)
        .checked_add(128)
        .and_then(|n| n.checked_mul(2))
        .and_then(|n| n.checked_add(std::mem::size_of::<PrivateFile>() as u64))
        .ok_or_else(|| l.verification())?;
    let retained = per_entry
        .checked_mul(audit.members.len() as u64)
        .ok_or_else(|| l.verification())?;
    let scope = l.with_retained_memory(retained)?;
    let mut held = Vec::with_capacity(audit.members.len());
    for (index, member) in audit.members.iter().enumerate() {
        if member.relative_name.ends_with("/manifest.json") {
            continue;
        }
        let completion = format!("purge-{}.member-{index}.json", audit.operation_id);
        held.push(hold_completion(
            catalog,
            &completion,
            hash,
            Some(index),
            audit.operation_id,
            &scope,
        )?);
    }
    Ok((held, scope))
}
// Raw binding and absence checks only: no injected open or read after the hash.
fn manifest_predecessors_unchanged(
    root: &Dir,
    audit: &PurgeAuditV1,
    target: &PurgeTarget,
    held: &[PrivateFile],
    l: &OperationLimits,
) -> Result<(), RecoveryError> {
    for completion in held {
        completion.unchanged(l)?;
    }
    for member in &audit.members {
        if member.relative_name.ends_with("/manifest.json") {
            continue;
        }
        let path = root.path.join(&member.relative_name);
        let parent = if path.parent() == Some(target.bundle.path.as_path()) {
            &target.bundle
        } else if path.parent() == Some(target.quarantine.path.as_path()) {
            &target.quarantine
        } else {
            return Err(l.verification());
        };
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| l.verification())?;
        if parent.exists(name, l)? {
            return Err(l.verification());
        }
    }
    Ok(())
}
fn manifest_predecessors_complete(
    root: &Dir,
    catalog: &Dir,
    audit: &PurgeAuditV1,
    hash: &str,
    target: &PurgeTarget,
    l: &OperationLimits,
) -> Result<(), RecoveryError> {
    let (held, scope) = hold_manifest_predecessors(catalog, audit, hash, l)?;
    manifest_predecessors_unchanged(root, audit, target, &held, &scope)?;
    target.validate(root, catalog, audit.operation_id, &scope)
}
// Read the administrative clock before the final noninjectable authority checks.
// The monotonic deadline also bounds lock waits and work between these samples.
fn lease_time_remaining(
    lease: &OfflineLease,
    io: &dyn fs::RecoveryIo,
    l: &OperationLimits,
) -> Result<std::time::Duration, RecoveryError> {
    l.check()?;
    let now = io.unix_ms().map_err(|_| fs::destination(l))?;
    l.check()?;
    if now < lease.request.issued_at_unix_ms || now >= lease.request.expires_at_unix_ms {
        return Err(fs::refused(
            l,
            "valid bound lease and renewed administrative exclusion required",
        ));
    }
    let remaining =
        u64::try_from(lease.request.expires_at_unix_ms - now).map_err(|_| l.verification())?;
    Ok(std::time::Duration::from_millis(remaining))
}
pub fn purge_quarantined(
    request: &PurgeRequest,
    lease: &OfflineLease,
    filesystem: &dyn RecoveryFs,
    limits: &OperationLimits,
) -> Result<PurgeReport, RecoveryError> {
    let mut l = limits.clone();
    l.operation_id = request.operation_id;
    l.publication_io = None;
    // Keep the bounded canonical paths, held target directories and iterator
    // scratch charged alongside every audit decode/hash and completion write.
    l = l.with_retained_memory(65536)?;
    l.check()?;
    if request.operation_id.is_nil() || request.confirm.is_some_and(|op| op != request.operation_id)
    {
        return Err(fs::refused(
            &l,
            "confirmation must match exact quarantine operation",
        ));
    }
    // Sample Instant first so time spent in the injectable clock cannot extend
    // the administrative lease. Never widen the caller's deadline.
    let started = std::time::Instant::now();
    let io = filesystem.io();
    let remaining = lease_time_remaining(lease, &*io, &l)?;
    l.deadline = l.deadline.min(
        started
            .checked_add(remaining)
            .ok_or_else(|| l.verification())?,
    );
    lease.validate(filesystem, &l)?;
    if lease.request.operation_id != request.operation_id {
        return Err(l.verification());
    }
    let expected_hash = match &lease.request.action {
        OfflineAction::Purge {
            quarantined_operation,
            manifest_sha256,
        } if *quarantined_operation == request.operation_id => manifest_sha256.clone(),
        _ => return Err(l.verification()),
    };
    if request.confirm.is_some()
        && request.reference_closure.as_ref().is_none_or(|e| {
            !manifest::sha256(&e.catalog_set_sha256)
                || !manifest::sha256(&e.evidence_sha256)
                || !e.non_ferroloop_writers_excluded
        })
    {
        return Err(fs::refused(
            &l,
            "explicit administrative reference closure evidence required",
        ));
    }
    let _ownership = OperationOwnership::acquire(&lease.request.physical_path, &l)?;
    let root = Dir::open(filesystem.root(), &l)?;
    let locks = Dir::open(&root.path.join(".locks"), &l)?;
    // Fixed order: physical store, operation, catalog. Inspection creates no lock
    // inode and refuses an unbootstrapped root rather than silently initializing it.
    let control = if request.confirm.is_some() {
        let unit = fs::allocation_geometry(&root, &l)?;
        let control = Arc::new(PublicationIo::new(
            io.clone(),
            l.byte_limit,
            l.byte_limit,
            0,
            unit,
        ));
        let overhead = fs::overhead_bytes(unit, &l)?;
        control.charge(overhead, true, &l)?;
        if io
            .available_bytes(&root.file)
            .map_err(|_| fs::destination(&l))?
            < overhead.checked_add(unit).ok_or_else(|| l.verification())?
        {
            return Err(fs::destination(&l));
        }
        l.publication_io = Some(control.clone());
        Some(control)
    } else {
        None
    };
    let mut physical_lock = None;
    let mut operation_lock = None;
    let mut observed_operation = None;
    if request.confirm.is_some() {
        let physical = match lease.request.expected_file {
            FileBinding::Readable { identity, .. } | FileBinding::Forensic { identity, .. } => {
                identity
            }
            FileBinding::Absent => return Err(l.verification()),
        };
        physical_lock = Some(StoreLock::acquire(&locks, physical, &l)?);
        operation_lock = Some(StoreLock::operation(&locks, request.operation_id, &l)?);
    } else {
        observed_operation = active_operation(&root, request.operation_id, &l)?;
    }
    // Carry private name/inode witnesses for the already-held locks through
    // publication hooks. Binding itself has no injectable descriptor-open seam.
    let lock_witnesses = if request.confirm.is_some() {
        let mut binding_limits = l.clone();
        binding_limits.publication_io = None;
        let physical = match lease.request.expected_file {
            FileBinding::Readable { identity, .. } | FileBinding::Forensic { identity, .. } => {
                identity
            }
            FileBinding::Absent => return Err(l.verification()),
        };
        Some([
            PrivateFile::open(
                &locks
                    .path
                    .join(format!("{}:{}.lock", physical.0, physical.1)),
                false,
                &binding_limits,
            )?,
            PrivateFile::open(
                &locks
                    .path
                    .join(format!("operation-{}.lock", request.operation_id)),
                false,
                &binding_limits,
            )?,
        ])
    } else {
        None
    };
    let access = CatalogLock::acquire(&root, CatalogLockMode::Exclusive, false, io.clone(), &l)
        .map_err(|e| {
            if !locks.exists("catalog.lock", &l).unwrap_or(false) {
                fs::refused(
                    &l,
                    "catalog lock bootstrap required before read-only purge inspection",
                )
            } else {
                e
            }
        })?;
    if !access.exclusive() {
        return Err(l.verification());
    }
    let catalog = Dir::open(&root.path.join(".catalog"), &l)?;
    let intent_name = format!("purge-{}.intent.json", request.operation_id);
    let mut pending = false;
    let mut admitted_geometry = None;
    let mut observed_audit = None;
    let mut audit = if catalog.exists(&intent_name, &l)? {
        let held = PrivateFile::open(&catalog.path.join(&intent_name), false, &l)?;
        let (a, scope): (PurgeAuditV1, _) = read_held_json(&held, &l).map_err(|error| {
            error.at_metadata_entry(|| Path::new(".catalog").join(&intent_name))
        })?;
        l = scope;
        if a.version != 1
            || a.operation_id != request.operation_id
            || a.manifest_sha256 != expected_hash
            || a.actor.trim().is_empty()
            || !manifest::sha256(&a.catalog_set_sha256)
        {
            return Err(l
                .verification()
                .at_metadata_entry(|| Path::new(".catalog").join(&intent_name)));
        }
        audit_members_valid(&a, &l).map_err(|error| {
            error.at_metadata_entry(|| Path::new(".catalog").join(&intent_name))
        })?;
        root.same_name(&l)?;
        let hash = verify::hash_file(&held.file, &l)?.1;
        replay_target(&root, &catalog, &a, &hash, &l)?;
        held.unchanged(&l)?;
        observed_audit = Some(held);
        Some(a)
    } else {
        None
    };
    let (members, set_hash) = if let Some(a) = &audit {
        let audit_memory = verify::serialized_checksum(a, &l)?
            .0
            .checked_mul(32)
            .ok_or_else(|| l.verification())?;
        let scan = l.with_retained_memory(audit_memory)?;
        let hash = closure(
            &root,
            request.operation_id,
            &a.members,
            &expected_hash,
            &scan,
        )?;
        (a.members.clone(), hash)
    } else {
        let InspectedTarget {
            manifest,
            hash,
            members,
            pending: withdraw,
            geometry,
        } = target(&root, request.operation_id, &l)?;
        admitted_geometry = Some(geometry);
        if hash != expected_hash
            || manifest.catalog_store_identity != lease.request.catalog_identity
            || manifest.validation_profile_sha256 != lease.request.profile_sha256
        {
            return Err(l.verification());
        }
        pending = withdraw;
        drop(manifest);
        let set = closure(&root, request.operation_id, &members, &hash, &l)?;
        (members, set)
    };
    let mut report = PurgeReport {
        operation_id: request.operation_id,
        members,
        catalog_set_sha256: set_hash,
        pending_receipt_withdrawal: pending,
        audit_durable: audit.is_some(),
        complete: false,
    };
    if request.confirm.is_none() {
        if let Some(a) = &audit {
            let held = observed_audit.as_ref().ok_or_else(|| l.verification())?;
            inspect_audit(&root, &catalog, a, held, &mut report, &l)?;
        }
        return Ok(report);
    }
    let evidence = request.reference_closure.as_ref().ok_or_else(|| {
        fs::refused(
            &l,
            "dry-run closure digest and non-FerroLoop writer exclusion required",
        )
    })?;
    if evidence.catalog_set_sha256 != report.catalog_set_sha256
        || !evidence.non_ferroloop_writers_excluded
        || !manifest::sha256(&evidence.evidence_sha256)
    {
        return Err(fs::refused(
            &l,
            "reference closure changed or administrative evidence incomplete",
        ));
    }
    let control = control.ok_or_else(|| l.verification())?;
    if pending {
        catalog::mark_quarantine(
            &root,
            &request.operation_id.to_string(),
            &control,
            &l,
            &access,
        )?;
        let InspectedTarget {
            manifest: _,
            hash,
            members: latest,
            pending: still_pending,
            geometry,
        } = target(&root, request.operation_id, &l)?;
        let previous = admitted_geometry.as_ref().ok_or_else(|| l.verification())?;
        if still_pending
            || hash != expected_hash
            || latest != report.members
            || geometry.staged != previous.staged
            || geometry.target_identity != previous.target_identity
            || geometry.quarantine_identity != previous.quarantine_identity
        {
            return Err(l.verification());
        }
        if closure(&root, request.operation_id, &latest, &expected_hash, &l)?
            != report.catalog_set_sha256
        {
            return Err(fs::refused(
                &l,
                "reference closure changed after receipt withdrawal",
            ));
        }
        report.pending_receipt_withdrawal = false;
    }
    lease.validate(filesystem, &l)?;
    if audit.is_none() {
        let a = PurgeAuditV1 {
            version: 1,
            operation_id: request.operation_id,
            manifest_sha256: expected_hash,
            catalog_set_sha256: report.catalog_set_sha256.clone(),
            actor: lease.request.operator.clone(),
            at_unix_ms: io.unix_ms().map_err(|_| fs::destination(&l))?,
            administrative_evidence: evidence.clone(),
            geometry: admitted_geometry,
            members: report.members.clone(),
        };
        audit_members_valid(&a, &l)?;
        root.same_name(&l)?;
        let target = open_target(&root, request.operation_id, a.geometry.as_ref(), &l)?;
        publish_audit_row(&catalog, &intent_name, &a, &control, &l, || {
            lease_time_remaining(lease, &*io, &l)?;
            for held in lock_witnesses.as_ref().ok_or_else(|| l.verification())? {
                held.unchanged(&l)?;
            }
            access.validate(&root, &l)?;
            target.validate(&root, &catalog, request.operation_id, &l)
        })?;
        audit = Some(a);
    }
    let audit = audit.ok_or_else(|| l.verification())?;
    let held_audit = match observed_audit {
        Some(held) => held,
        None => {
            // Bind the freshly written model to the descriptor used thereafter.
            let retained = verify::serialized_checksum(&audit, &l)?
                .0
                .checked_mul(32)
                .ok_or_else(|| l.verification())?;
            l = l.with_retained_memory(retained)?;
            let held = PrivateFile::open(&catalog.path.join(&intent_name), false, &l)?;
            if verify::hash_file(&held.file, &l)? != verify::serialized_checksum(&audit, &l)? {
                return Err(l.verification());
            }
            held.unchanged(&l)?;
            held
        }
    };
    let audit_hash = verify::hash_file(&held_audit.file, &l)?.1;
    let target = replay_target(&root, &catalog, &audit, &audit_hash, &l)?;
    control.sync(&held_audit.file, Member::PurgeAudit, &l)?;
    catalog.sync(&*io, Directory::CatalogParent, &l)?;
    report.audit_durable = true;
    for (index, m) in audit.members.iter().enumerate() {
        lease_time_remaining(lease, &*io, &l)?;
        access.validate(&root, &l)?;
        held_audit.unchanged(&l)?;
        target.validate(&root, &catalog, request.operation_id, &l)?;
        let complete_name = format!("purge-{}.member-{index}.json", request.operation_id);
        let path = root.path.join(&m.relative_name);
        let parent = Dir::open(path.parent().ok_or_else(|| l.verification())?, &l)?;
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| l.verification())?;
        if catalog.exists(&complete_name, &l)? {
            check_completion(
                &catalog,
                &complete_name,
                &audit_hash,
                Some(index),
                request.operation_id,
                &l,
            )?;
            if parent.exists(name, &l)? {
                return Err(l.verification());
            }
            target.validate(&root, &catalog, request.operation_id, &l)?;
            report.members[index].state = PurgeMemberState::Removed;
            continue;
        }
        if parent.exists(name, &l)? {
            if name == "manifest.json" {
                manifest_predecessors_complete(&root, &catalog, &audit, &audit_hash, &target, &l)?;
            }
            let held = verify_present(&root, m, &l)?;
            fs::event(&*io, RecoveryEvent::PurgeUnlink, &l)?;
            access.validate(&root, &l)?;
            held_audit.unchanged(&l)?;
            target.validate(&root, &catalog, request.operation_id, &l)?;
            if name == "manifest.json" {
                manifest_predecessors_complete(&root, &catalog, &audit, &audit_hash, &target, &l)?;
            }
            parent.remove_file_checked(name, held.identity(), &l, |last| {
                // The helper has performed the final injectable descriptor open.
                // Complete any remaining injected reads first, then revalidate
                // all authority with noninjectable descriptor/name checks last.
                let (predecessors, final_scope) = if name == "manifest.json" {
                    hold_manifest_predecessors(&catalog, &audit, &audit_hash, &l)?
                } else {
                    (Vec::new(), l.clone())
                };
                final_scope.admit_memory(65536)?;
                if verify::hash_file(&last.file, &final_scope)? != (m.bytes, m.sha256.clone()) {
                    return Err(final_scope.verification());
                }
                // Sample expiry after the final injectable read, before held
                // authority checks. No injected clock/open/read follows them.
                lease_time_remaining(lease, &*io, &final_scope)?;
                // Never adopt new completion authority after this boundary;
                // check original bindings/absence.
                if name == "manifest.json" {
                    manifest_predecessors_unchanged(
                        &root,
                        &audit,
                        &target,
                        &predecessors,
                        &final_scope,
                    )?;
                }
                held.unchanged(&final_scope)?;
                access.validate(&root, &final_scope)?;
                held_audit.unchanged(&final_scope)?;
                target.validate(&root, &catalog, request.operation_id, &final_scope)
            })?;
        }
        // Missing after an interrupted unlink is only completed once its parent is
        // synced. No planned name is reported removed merely from audit intent.
        lease_time_remaining(lease, &*io, &l)?;
        parent.sync(&*io, Directory::PurgeParent, &l)?;
        lease_time_remaining(lease, &*io, &l)?;
        access.validate(&root, &l)?;
        held_audit.unchanged(&l)?;
        target.validate(&root, &catalog, request.operation_id, &l)?;
        let done = PurgeCompletionV1 {
            version: 1,
            operation_id: request.operation_id,
            audit_sha256: audit_hash.clone(),
            member_index: Some(index),
        };
        publish_audit_row(&catalog, &complete_name, &done, &control, &l, || {
            lease_time_remaining(lease, &*io, &l)?;
            for held in lock_witnesses.as_ref().ok_or_else(|| l.verification())? {
                held.unchanged(&l)?;
            }
            access.validate(&root, &l)?;
            held_audit.unchanged(&l)?;
            target.validate(&root, &catalog, request.operation_id, &l)?;
            if parent.exists(name, &l)? {
                return Err(l.verification());
            }
            Ok(())
        })?;
        lease_time_remaining(lease, &*io, &l)?;
        target.validate(&root, &catalog, request.operation_id, &l)?;
        if parent.exists(name, &l)? {
            return Err(l.verification());
        }
        report.members[index].state = PurgeMemberState::Removed;
    }
    // Recheck the full surviving-name/completion set before final completion.
    replay_target(&root, &catalog, &audit, &audit_hash, &l)?;
    lease_time_remaining(lease, &*io, &l)?;
    let complete_name = format!("purge-{}.complete.json", request.operation_id);
    if catalog.exists(&complete_name, &l)? {
        check_completion(
            &catalog,
            &complete_name,
            &audit_hash,
            None,
            request.operation_id,
            &l,
        )?;
    } else {
        let done = PurgeCompletionV1 {
            version: 1,
            operation_id: request.operation_id,
            audit_sha256: audit_hash.clone(),
            member_index: None,
        };
        // Retain the predecessor descriptors across the publication hooks;
        // re-opening them after the final clock sample could adopt replacements.
        let (mut predecessors, scope) =
            hold_manifest_predecessors(&catalog, &audit, &audit_hash, &l)?;
        let manifest_index = audit
            .members
            .iter()
            .position(|m| m.relative_name.ends_with("/manifest.json"))
            .ok_or_else(|| l.verification())?;
        predecessors.push(hold_completion(
            &catalog,
            &format!(
                "purge-{}.member-{manifest_index}.json",
                request.operation_id
            ),
            &audit_hash,
            Some(manifest_index),
            request.operation_id,
            &scope,
        )?);
        publish_audit_row(&catalog, &complete_name, &done, &control, &scope, || {
            lease_time_remaining(lease, &*io, &scope)?;
            for held in lock_witnesses
                .as_ref()
                .ok_or_else(|| scope.verification())?
            {
                held.unchanged(&scope)?;
            }
            access.validate(&root, &scope)?;
            held_audit.unchanged(&scope)?;
            manifest_predecessors_unchanged(&root, &audit, &target, &predecessors, &scope)?;
            target.validate(&root, &catalog, request.operation_id, &scope)?;
            for m in &audit.members {
                let path = root.path.join(&m.relative_name);
                if !matches!(path.symlink_metadata(), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
                {
                    return Err(scope.verification());
                }
            }
            Ok(())
        })?;
    }
    replay_target(&root, &catalog, &audit, &audit_hash, &l)?;
    lease_time_remaining(lease, &*io, &l)?;
    held_audit.unchanged(&l)?;
    report.complete = true;
    drop(observed_operation);
    drop(operation_lock);
    drop(physical_lock);
    Ok(report)
}
fn check_completion(
    catalog: &Dir,
    name: &str,
    hash: &str,
    index: Option<usize>,
    op: uuid::Uuid,
    l: &OperationLimits,
) -> Result<(), RecoveryError> {
    hold_completion(catalog, name, hash, index, op, l).map(|_| ())
}
fn hold_completion(
    catalog: &Dir,
    name: &str,
    hash: &str,
    index: Option<usize>,
    op: uuid::Uuid,
    l: &OperationLimits,
) -> Result<PrivateFile, RecoveryError> {
    let held = PrivateFile::open(&catalog.path.join(name), false, l)?;
    (|| {
        let (d, scope): (PurgeCompletionV1, _) = read_held_json(&held, l)?;
        if d.version != 1
            || d.operation_id != op
            || d.audit_sha256 != hash
            || d.member_index != index
        {
            return Err(l.verification());
        }
        held.unchanged(&scope)
    })()
    .map_err(|error| error.at_metadata_entry(|| Path::new(".catalog").join(name)))?;
    Ok(held)
}
fn inspect_audit(
    root: &Dir,
    catalog: &Dir,
    audit: &PurgeAuditV1,
    held: &PrivateFile,
    report: &mut PurgeReport,
    l: &OperationLimits,
) -> Result<(), RecoveryError> {
    held.unchanged(l)?;
    let hash = verify::hash_file(&held.file, l)?.1;
    let target = replay_target(root, catalog, audit, &hash, l)?;
    for (index, m) in audit.members.iter().enumerate() {
        let completion = format!("purge-{}.member-{index}.json", audit.operation_id);
        let path = root.path.join(&m.relative_name);
        let parent = Dir::open(path.parent().ok_or_else(|| l.verification())?, l)?;
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| l.verification())?;
        if parent.exists(name, l)? {
            verify_present(root, m, l)?;
            if catalog.exists(&completion, l)? {
                return Err(l.verification());
            }
        } else if catalog.exists(&completion, l)? {
            check_completion(
                catalog,
                &completion,
                &hash,
                Some(index),
                audit.operation_id,
                l,
            )?;
            report.members[index].state = PurgeMemberState::Removed;
        } else {
            report.members[index].state = PurgeMemberState::OutcomeUnknown;
        }
    }
    target.validate(root, catalog, audit.operation_id, l)?;
    let complete = format!("purge-{}.complete.json", audit.operation_id);
    if catalog.exists(&complete, l)? {
        check_completion(catalog, &complete, &hash, None, audit.operation_id, l)?;
        if report
            .members
            .iter()
            .any(|m| m.state != PurgeMemberState::Removed)
        {
            return Err(l.verification());
        }
        report.complete = true;
    }
    target.validate(root, catalog, audit.operation_id, l)?;
    held.unchanged(l)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "linux")]
    #[test]
    fn audit_temp_cleanup_refuses_replacement_nonregular_and_lost_authority() {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct ReplaceAtFinalOpen {
            path: std::path::PathBuf,
            opens: AtomicUsize,
            replace: bool,
        }
        impl fs::RecoveryIo for ReplaceAtFinalOpen {
            fn before(&self, event: RecoveryEvent) -> std::io::Result<()> {
                if event == RecoveryEvent::OpenFile
                    && self.opens.fetch_add(1, Ordering::AcqRel) == 1
                    && self.replace
                {
                    std::fs::rename(&self.path, self.path.with_extension("saved"))?;
                    std::fs::write(&self.path, b"foreign replacement")?;
                    std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600))?;
                }
                Ok(())
            }
        }
        for case in 0..6 {
            let root = super::super::test_support::fixture_root();
            let name = format!("purge-{}.intent.json", uuid::Uuid::now_v7());
            let path = root.path().join(format!("{name}.tmp"));
            if case == 2 {
                std::os::unix::fs::symlink("unowned", &path).unwrap();
            } else if case == 3 {
                std::fs::create_dir(&path).unwrap();
            } else {
                std::fs::write(&path, b"{").unwrap();
                std::fs::set_permissions(
                    &path,
                    std::fs::Permissions::from_mode(if case == 5 { 0o644 } else { 0o600 }),
                )
                .unwrap();
                if case == 4 {
                    std::fs::hard_link(&path, root.path().join("alias")).unwrap();
                }
            }
            let hook = Arc::new(ReplaceAtFinalOpen {
                path: path.clone(),
                opens: AtomicUsize::new(0),
                replace: case == 0,
            });
            let mut l = OperationLimits::new(
                std::time::Instant::now() + std::time::Duration::from_secs(30),
                1 << 20,
                1 << 20,
                super::super::CancelToken::new(),
            );
            let control = Arc::new(PublicationIo::new(hook.clone(), 1 << 20, 1 << 20, 0, 4096));
            l.publication_io = Some(control.clone());
            let catalog = Dir::open(root.path(), &l).unwrap();
            let result = publish_audit_row(
                &catalog,
                &name,
                &serde_json::json!({"fixture":"row"}),
                &control,
                &l,
                || {
                    if case == 1 {
                        return Err(RecoveryError::Deadline {
                            operation_id: l.operation_id,
                        });
                    }
                    Ok(())
                },
            );
            assert!(result.is_err(), "case {case} must retain the temp");
            assert!(!root.path().join(&name).exists());
            match case {
                0 => assert_eq!(std::fs::read(&path).unwrap(), b"foreign replacement"),
                2 => assert_eq!(
                    std::fs::read_link(&path).unwrap(),
                    std::path::Path::new("unowned")
                ),
                3 => assert!(path.is_dir()),
                _ => assert_eq!(std::fs::read(&path).unwrap(), b"{"),
            }
            if case == 1 {
                assert!(matches!(result, Err(RecoveryError::Deadline { .. })));
            }
        }
    }

    // RED plan after approved application: temporarily derive pin_limits from
    // l instead of scan (the original missing-retained-scope behavior), then run
    // this test: the last scan reads/succeeds, failing the Quota/no-read check.
    // GREEN plan: restore scan.with_retained_memory and rerun this exact test,
    // then required recovery checks. No tests have run for this inert proposal.
    #[cfg(target_os = "linux")]
    #[test]
    fn restore_pin_read_includes_prior_evidence_before_json_allocation() {
        use std::os::unix::fs::{FileExt, PermissionsExt};
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        struct CountReads(AtomicUsize);
        impl super::super::fs::RecoveryIo for CountReads {
            fn read_exact_at(
                &self,
                file: &std::fs::File,
                bytes: &mut [u8],
                offset: u64,
            ) -> std::io::Result<()> {
                self.0.fetch_add(1, Ordering::SeqCst);
                file.read_exact_at(bytes, offset)
            }
        }
        let fixture = super::super::test_support::fixture_root();
        let mut generous = OperationLimits::new(
            std::time::Instant::now() + std::time::Duration::from_secs(30),
            1 << 20,
            16 << 20,
            super::super::CancelToken::new(),
        );
        let reads = Arc::new(CountReads(AtomicUsize::new(0)));
        generous.publication_io = Some(Arc::new(super::super::fs::PublicationIo::new(
            reads.clone(),
            1 << 20,
            1 << 20,
            1 << 20,
            4096,
        )));
        let mut evidence = ClosureEvidence {
            entries: Vec::new(),
        };
        let target = uuid::Uuid::now_v7();
        // Single-pin fixture directories force small-before-large order without
        // depending on readdir order. One evidence collection spans every scan.
        let write_pin = |index: usize, padded: bool| {
            let catalog_path = fixture.path().join(format!("catalog-{index}"));
            let pins = catalog_path.join("restore-pins");
            std::fs::create_dir(&catalog_path).unwrap();
            std::fs::create_dir(&pins).unwrap();
            for path in [&catalog_path, &pins] {
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
            }
            let operation = uuid::Uuid::now_v7();
            let staging = fixture
                .path()
                .join(format!(".restore-{operation}-{}", uuid::Uuid::now_v7()));
            let value = serde_json::json!({
                "version": 1,
                "restore_operation": operation,
                "bundle_operation": uuid::Uuid::now_v7(),
                "bundle_sha256": "a".repeat(64),
                "destination": fixture.path().join("destination.fls"),
                "journal": staging.join("journal.bin"),
                "journal_identity": [1, 2],
                "operation_lock_identity": [1, 3],
            });
            let mut bytes = serde_json::to_vec(&value).unwrap();
            if padded {
                assert!(bytes.len() < 32768);
                bytes.resize(32768, b' '); // Valid JSON at the private pin ceiling.
            }
            let path = pins.join(format!("{operation}.json"));
            std::fs::write(&path, &bytes).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            (catalog_path, path, bytes)
        };
        for index in 0..256 {
            let (path, _, _) = write_pin(index, false);
            let catalog = Dir::open(&path, &generous).unwrap();
            retain_restore_pins(&catalog, target, &mut evidence, &generous).unwrap();
        }
        assert_eq!(evidence.entries.len(), 256);
        let (path, pin_path, pin_bytes) = write_pin(256, true);
        let catalog = Dir::open(&path, &generous).unwrap();
        let capacity = evidence.entries.capacity().max(evidence.entries.len() + 1) as u64;
        let ceiling = capacity * CLOSURE_ENTRY_MEMORY + 3 * 65536 + 2 * 32768 * 32;
        // Use fresh admission counters so prior generous scans cannot mask it.
        let bounded = |limit| {
            let mut limits = OperationLimits::new(
                generous.deadline,
                generous.byte_limit,
                limit,
                super::super::CancelToken::new(),
            );
            limits.publication_io = generous.publication_io.clone();
            limits
        };
        let limits = bounded(ceiling - 1);
        let before = reads.0.load(Ordering::SeqCst);
        assert!(matches!(
            retain_restore_pins(&catalog, target, &mut evidence, &limits),
            Err(RecoveryError::Quota {
                resource: "working memory",
                ..
            })
        ));
        assert_eq!(
            reads.0.load(Ordering::SeqCst),
            before,
            "must refuse before reading pin JSON"
        );
        assert_eq!(evidence.entries.len(), 256);
        assert!(limits.admitted_memory_bytes() <= limits.memory_byte_limit);
        assert_eq!(std::fs::read(&pin_path).unwrap(), pin_bytes);
        let sufficient = bounded(ceiling);
        retain_restore_pins(&catalog, target, &mut evidence, &sufficient).unwrap();
        assert_eq!(evidence.entries.len(), 257);
        assert!(reads.0.load(Ordering::SeqCst) > before);
        assert_eq!(sufficient.admitted_memory_bytes(), ceiling);
    }

    #[test]
    fn closure_evidence_refuses_before_vector_growth() {
        let generous = OperationLimits::new(
            std::time::Instant::now() + std::time::Duration::from_secs(10),
            1 << 20,
            1 << 20,
            super::super::CancelToken::new(),
        );
        let mut evidence = ClosureEvidence {
            entries: Vec::new(),
        };
        evidence.reserve(2, &generous).unwrap();
        evidence
            .entries
            .push(("receipt/fixture".into(), "a".repeat(64)));
        evidence
            .entries
            .push(("manifest/fixture".into(), "b".repeat(64)));
        let old_capacity = evidence.entries.capacity();
        let old_entries = evidence.entries.clone();
        let mut bounded = generous.clone();
        bounded.memory_byte_limit = 3 * 65536 + old_capacity as u64 * CLOSURE_ENTRY_MEMORY;
        evidence.reserve(0, &bounded).unwrap();
        assert!(matches!(
            evidence.reserve(1, &bounded),
            Err(RecoveryError::Quota {
                resource: "working memory",
                ..
            })
        ));
        assert_eq!(
            evidence.entries.capacity(),
            old_capacity,
            "refusal must precede allocation"
        );
        assert_eq!(evidence.entries, old_entries);
    }
    #[test]
    fn retained_memory_adds_nested_scratch_once() {
        let l = OperationLimits::new(
            std::time::Instant::now() + std::time::Duration::from_secs(10),
            1 << 20,
            1000,
            super::super::CancelToken::new(),
        );
        let first = l.with_retained_memory(100).unwrap();
        let second = first.with_retained_memory(200).unwrap();
        second.admit_memory(700).unwrap();
        assert_eq!(l.admitted_memory_bytes(), 1000);
        assert!(matches!(
            second.admit_memory(701),
            Err(RecoveryError::Quota {
                resource: "working memory",
                ..
            })
        ));
        // Scopes do not mutate their parent or share reservation consumption.
        first.admit_memory(900).unwrap();
        l.admit_memory(1000).unwrap();
        assert!(second.with_retained_memory(u64::MAX).is_err());
    }
}
