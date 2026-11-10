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
struct PurgeAuditV1 {
    version: u32,
    operation_id: uuid::Uuid,
    manifest_sha256: String,
    catalog_set_sha256: String,
    actor: String,
    at_unix_ms: u128,
    administrative_evidence: ReferenceClosureEvidence,
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
fn target(
    root: &Dir,
    operation: uuid::Uuid,
    l: &OperationLimits,
) -> Result<(BundleManifestV1, String, Vec<PurgeMember>, bool), RecoveryError> {
    let op = operation.to_string();
    let quarantine = Dir::open(&root.path.join(".quarantine").join(&op), l)?;
    let final_path = root.path.join(&op);
    let staged_path = root.path.join(".staging").join(&op);
    let exists = |p: &Path| {
        std::fs::symlink_metadata(p)
            .map(|_| true)
            .or_else(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    Ok(false)
                } else {
                    Err(e)
                }
            })
            .map_err(|_| l.verification())
    };
    let dir = match (exists(&final_path)?, exists(&staged_path)?) {
        (true, false) => Dir::open(&final_path, l)?,
        (false, true) => Dir::open(&staged_path, l)?,
        _ => return Err(l.verification()),
    };
    let checked = catalog::check_bundle(&dir, &op, l)?;
    let catalog = Dir::open(&root.path.join(".catalog"), l)?;
    let receipt_name = format!("{op}.json");
    let pending = catalog.exists(&receipt_name, l)?;
    let receipt = if pending {
        if quarantine.exists("receipt.json", l)? {
            return Err(l.verification());
        }
        catalog::checked_receipt(&catalog, &receipt_name, &checked, l)?
    } else {
        catalog::checked_receipt(&quarantine, "receipt.json", &checked, l)?
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
    Ok((checked.manifest, checked.manifest_sha256, members, pending))
}
fn reference_hit(manifest: &BundleManifestV1, op: uuid::Uuid, digests: &BTreeSet<String>) -> bool {
    let name = op.to_string();
    manifest.operation_id == name
        || manifest
            .reference_dependencies
            .iter()
            .any(|r| r.catalog_identity == name || digests.contains(&r.reference_sha256))
        || manifest
            .retained_members
            .iter()
            .any(|m| digests.contains(&m.sha256))
        || digests.contains(&manifest.image_sha256)
        || manifest
            .shared_config
            .as_ref()
            .is_some_and(|r| digests.contains(&r.descriptor_sha256))
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
fn checked_memory(
    checked: &catalog::CheckedBundle,
    l: &OperationLimits,
) -> Result<u64, RecoveryError> {
    checked
        .bytes
        .checked_sub(checked.manifest.image_bytes)
        .and_then(|n| n.checked_mul(64))
        .and_then(|n| n.checked_add(65536))
        .ok_or_else(|| l.verification())
}
fn closure(
    root: &Dir,
    op: uuid::Uuid,
    members: &[PurgeMember],
    manifest_hash: &str,
    l: &OperationLimits,
) -> Result<String, RecoveryError> {
    // The input members and digest tree coexist throughout both scans. Charge
    // before collect/clone, including a second caller-owned member list.
    let retained = (members.len() as u64)
        .checked_mul(3)
        .and_then(|n| n.checked_add(1))
        .and_then(|n| n.checked_mul(CLOSURE_ENTRY_MEMORY))
        .ok_or_else(|| l.verification())?;
    let l = l.with_retained_memory(retained)?;
    let mut digests: BTreeSet<String> = members.iter().map(|m| m.sha256.clone()).collect();
    digests.insert(manifest_hash.to_owned());
    let catalog = Dir::open(&root.path.join(".catalog"), &l)?;
    let mut evidence = ClosureEvidence {
        entries: Vec::new(),
    };
    catalog.visit_names(&l, |name| {
        if name.starts_with("purge-") {
            validate_audit_name(name, &l)?;
            return Ok(());
        }
        let text = name.strip_suffix(".json").ok_or_else(|| l.verification())?;
        let other = canonical_op(text, &l)?;
        if other == op {
            return Ok(());
        }
        let scan = evidence.reserve(2, &l)?;
        let dir = Dir::open(&root.path.join(text), &scan)?;
        let checked = catalog::check_bundle(&dir, text, &scan)?;
        let receipt_limits = scan.with_retained_memory(checked_memory(&checked, &scan)?)?;
        let receipt = catalog::checked_receipt(&catalog, name, &checked, &receipt_limits)?;
        if reference_hit(&checked.manifest, op, &digests) {
            return Err(fs::refused(
                &scan,
                "administrative reference closure includes target operation, name or member digest",
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
                || digests.contains(&m.manifest_sha256)
                || m.shared_config
                    .as_ref()
                    .is_some_and(|r| digests.contains(&r.descriptor_sha256))
            {
                return Err(fs::refused(
                    &member_limits,
                    "completed recovery set references quarantine material",
                ));
            }
            let text = m.operation_id.to_string();
            let dir = Dir::open(&root.path.join(&text), &member_limits)?;
            let checked = catalog::check_bundle(&dir, &text, &member_limits)?;
            let receipt_limits =
                member_limits.with_retained_memory(checked_memory(&checked, &member_limits)?)?;
            let receipt = catalog::checked_receipt(
                &catalog,
                &format!("{text}.json"),
                &checked,
                &receipt_limits,
            )?;
            if checked.manifest_sha256 != m.manifest_sha256
                || checked.manifest.catalog_store_identity != m.catalog_identity
                || checked.manifest.published_sequence != m.sequence
                || checked.manifest.shared_config != m.shared_config
                || reference_hit(&checked.manifest, op, &digests)
            {
                return Err(l.verification());
            }
            receipt.unchanged(&receipt_limits)?;
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
fn audit_members_valid(
    root: &Dir,
    audit: &PurgeAuditV1,
    l: &OperationLimits,
) -> Result<(), RecoveryError> {
    let op = audit.operation_id.to_string();
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
            [p, n] if *p == op => *n,
            [".staging", p, n] if *p == op => *n,
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
    root.same_name(l)
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
pub fn purge_quarantined(
    request: &PurgeRequest,
    lease: &OfflineLease,
    filesystem: &dyn RecoveryFs,
    limits: &OperationLimits,
) -> Result<PurgeReport, RecoveryError> {
    let mut l = limits.clone();
    l.operation_id = request.operation_id;
    l.publication_io = None;
    l.check()?;
    if request.operation_id.is_nil() || request.confirm.is_some_and(|op| op != request.operation_id)
    {
        return Err(fs::refused(
            &l,
            "confirmation must match exact quarantine operation",
        ));
    }
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
    let io = filesystem.io();
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
    let mut audit = if catalog.exists(&intent_name, &l)? {
        let a: PurgeAuditV1 = fs::read_json(&catalog, &intent_name, &l)?;
        if a.version != 1
            || a.operation_id != request.operation_id
            || a.manifest_sha256 != expected_hash
            || a.actor.trim().is_empty()
            || !manifest::sha256(&a.catalog_set_sha256)
        {
            return Err(l.verification());
        }
        audit_members_valid(&root, &a, &l)?;
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
        let (manifest, hash, members, withdraw) = target(&root, request.operation_id, &l)?;
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
            inspect_audit(&root, &catalog, a, &mut report, &l)?;
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
        let (_, hash, latest, still_pending) = target(&root, request.operation_id, &l)?;
        if still_pending || hash != expected_hash || latest != report.members {
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
            members: report.members.clone(),
        };
        audit_members_valid(&root, &a, &l)?;
        fs::write_json(&catalog, &intent_name, &a, Member::PurgeAudit, &control, &l)?;
        catalog.sync(&*io, Directory::CatalogParent, &l)?;
        audit = Some(a);
    }
    let audit = audit.ok_or_else(|| l.verification())?;
    let held_audit = PrivateFile::open(&catalog.path.join(&intent_name), false, &l)?;
    let audit_hash = verify::hash_file(&held_audit.file, &l)?.1;
    control.sync(&held_audit.file, Member::PurgeAudit, &l)?;
    catalog.sync(&*io, Directory::CatalogParent, &l)?;
    report.audit_durable = true;
    for (index, m) in audit.members.iter().enumerate() {
        l.check()?;
        access.validate(&root, &l)?;
        held_audit.unchanged(&l)?;
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
            report.members[index].state = PurgeMemberState::Removed;
            continue;
        }
        if parent.exists(name, &l)? {
            let held = verify_present(&root, m, &l)?;
            fs::event(&*io, RecoveryEvent::PurgeUnlink, &l)?;
            held.unchanged(&l)?;
            let again = verify_present(&root, m, &l)?;
            if again.identity() != held.identity() {
                return Err(l.verification());
            }
            parent.remove_file(name, held.identity(), &l)?;
        }
        // Missing after an interrupted unlink is only completed once its parent is
        // synced. No planned name is reported removed merely from audit intent.
        parent.sync(&*io, Directory::PurgeParent, &l)?;
        let done = PurgeCompletionV1 {
            version: 1,
            operation_id: request.operation_id,
            audit_sha256: audit_hash.clone(),
            member_index: Some(index),
        };
        fs::write_json(
            &catalog,
            &complete_name,
            &done,
            Member::PurgeAudit,
            &control,
            &l,
        )?;
        catalog.sync(&*io, Directory::CatalogParent, &l)?;
        report.members[index].state = PurgeMemberState::Removed;
    }
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
        fs::write_json(
            &catalog,
            &complete_name,
            &PurgeCompletionV1 {
                version: 1,
                operation_id: request.operation_id,
                audit_sha256: audit_hash,
                member_index: None,
            },
            Member::PurgeAudit,
            &control,
            &l,
        )?;
        catalog.sync(&*io, Directory::CatalogParent, &l)?;
    }
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
    let held = PrivateFile::open(&catalog.path.join(name), false, l)?;
    let d: PurgeCompletionV1 = fs::read_json(catalog, name, l)?;
    if d.version != 1 || d.operation_id != op || d.audit_sha256 != hash || d.member_index != index {
        return Err(l.verification());
    }
    held.unchanged(l)
}
fn inspect_audit(
    root: &Dir,
    catalog: &Dir,
    audit: &PurgeAuditV1,
    report: &mut PurgeReport,
    l: &OperationLimits,
) -> Result<(), RecoveryError> {
    let held = PrivateFile::open(
        &catalog
            .path
            .join(format!("purge-{}.intent.json", audit.operation_id)),
        false,
        l,
    )?;
    let hash = verify::hash_file(&held.file, l)?.1;
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
    held.unchanged(l)
}

#[cfg(test)]
mod tests {
    use super::*;
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
