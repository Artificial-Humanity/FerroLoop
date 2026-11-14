//! Explicit offline recovery adapters. Every source and destination is operator
//! selected; absence of a daemon never selects an offline action.
use super::{Error, Result};
use crate::recovery::{
    self as recovery, CancelToken, CaptureBudget, OperationLimits, StoreIdentity,
    ValidationProfile,
    admission::{self, OfflineLease, OfflineLeaseRequest},
    backup::{self, BackupRequest, PublicationOutcome},
    catalog,
    fs::{self, Dir, NativeRecoveryFs, RecoveryFs},
    purge::{self, PurgeRequest},
    readiness::{self, AbsenceReconciliationRecord, ReconciliationRecord},
    restore::{self, RestoreRequest},
    verify::PrivateFile,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OfflineBackupRequestV1 {
    pub version: u32,
    pub operation_id: uuid::Uuid,
    pub source: PathBuf,
    pub store: StoreIdentity,
    pub source_map_sha256: String,
    pub profile: ValidationProfile,
    pub pin_byte_limit: u64,
    pub preparation_byte_limit: u64,
    pub writer_delay_limit_ms: u64,
    pub image_byte_limit: Option<u64>,
    pub metadata_byte_limit: u64,
    pub space_margin_bytes: u64,
    pub configs: Vec<backup::ApprovedConfig>,
    pub excluded_vault_key: Option<backup::ExcludedVaultKey>,
    pub code_index_configuration:
        recovery::manifest::Availability<recovery::manifest::EffectiveCodeIndexConfig>,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OfflineRestoreRequestV1 {
    pub version: u32,
    pub operation_id: uuid::Uuid,
    pub destination: PathBuf,
    pub store: StoreIdentity,
    pub staging_parent: PathBuf,
    pub bundle_operation: uuid::Uuid,
    pub metadata_byte_limit: u64,
    pub space_margin_bytes: u64,
    pub original_profile: ValidationProfile,
    pub available_dependencies: Vec<readiness::DependencyResolution>,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RestoreContinuationRequestV1 {
    pub version: u32,
    pub operation_id: uuid::Uuid,
    pub destination: PathBuf,
    pub journal: PathBuf,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RestoreFinalizeRequestV1 {
    pub version: u32,
    pub operation_id: uuid::Uuid,
    pub destination: PathBuf,
    pub journal: PathBuf,
    pub reconciliation: ReconciliationRecord,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AbsenceFinalizeRequestV1 {
    pub version: u32,
    pub operation_id: uuid::Uuid,
    pub destination: PathBuf,
    pub journal: PathBuf,
    pub reconciliation: AbsenceReconciliationRecord,
}
fn error(e: recovery::RecoveryError) -> Error {
    Error::Refused(e.to_string())
}
fn private_request<T: DeserializeOwned>(path: &Path, l: &OperationLimits) -> Result<T> {
    let parent = Dir::open(
        path.parent()
            .ok_or_else(|| Error::Refused("private request parent required".into()))?,
        l,
    )
    .map_err(error)?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| Error::Refused("request filename required".into()))?;
    let held = PrivateFile::open(path, false, l).map_err(error)?;
    let request = fs::read_json(&parent, name, l).map_err(error)?;
    held.unchanged(l).map_err(error)?;
    Ok(request)
}
fn bound(path: &Path, selected: Option<&Path>) -> Result<()> {
    if let Some(selected) = selected
        && admission::canonical_physical(path).map_err(error)?
            != admission::canonical_physical(selected).map_err(error)?
    {
        return Err(Error::Refused(
            "request must match the bound physical store".into(),
        ));
    }
    Ok(())
}
fn continuation(
    request: &RestoreContinuationRequestV1,
    lease: &OfflineLease,
    selected: Option<&Path>,
) -> Result<()> {
    if request.version != 1
        || request.operation_id != lease.request.operation_id
        || request.destination != lease.request.physical_path
    {
        return Err(Error::Refused(
            "matching versioned continuation and lease required".into(),
        ));
    }
    bound(&request.destination, selected)
}
#[cfg(feature = "cli")]
pub fn run(args: &super::args::RecoveryArgs, selected: Option<&Path>) -> Result<serde_json::Value> {
    use super::args::RecoveryCommand;
    if args.timeout_ms == 0
        || args.timeout_ms > 3_600_000
        || args.byte_limit == 0
        || args.memory_byte_limit == 0
    {
        return Err(Error::Refused(
            "finite nonzero recovery limits required (timeout at most one hour)".into(),
        ));
    }
    let l = OperationLimits::new(
        Instant::now() + Duration::from_millis(args.timeout_ms),
        args.byte_limit,
        args.memory_byte_limit,
        CancelToken::new(),
    );
    let filesystem = NativeRecoveryFs::new(
        args.root.clone().ok_or_else(|| {
            Error::Refused("explicit --root approved private path required".into())
        })?,
        l.clone(),
    );
    let request_path = || {
        args.request
            .as_deref()
            .ok_or_else(|| Error::Refused("explicit --request PATH required".into()))
    };
    if let RecoveryCommand::Lease = &args.action {
        let request: OfflineLeaseRequest = private_request(request_path()?, &l)?;
        bound(&request.physical_path, selected)?;
        let lease = admission::create_offline_lease(&request, &filesystem, &l).map_err(error)?;
        return Ok(serde_json::json!({"lease":lease.path(),"administrative_attestation":true}));
    }
    if let RecoveryCommand::Inspect { operation } | RecoveryCommand::Verify { operation } =
        &args.action
    {
        let bundle =
            catalog::select_completed_bundle(*operation, &filesystem, &l).map_err(error)?;
        let manifest = catalog::retention_manifest(&bundle, &l).map_err(error)?;
        if matches!(args.action, RecoveryCommand::Inspect { .. }) {
            return serde_json::to_value(manifest).map_err(Error::Json);
        }
        let dir = Dir::open(bundle.path(), &l).map_err(error)?;
        let expected = fs::read_json(&dir, "validation.json", &l).map_err(error)?;
        let profile = fs::read_json(&dir, "profile.json", &l).map_err(error)?;
        let report = recovery::verify_existing_copy(
            &bundle.path().join("image.fls"),
            &expected,
            &profile,
            &l,
        )
        .map_err(error)?;
        return serde_json::to_value(report).map_err(Error::Json);
    }
    let lease_path = args
        .lease
        .as_deref()
        .ok_or_else(|| Error::Refused("explicit --lease PATH required".into()))?;
    let lease = OfflineLease::load(lease_path, &filesystem, &l).map_err(error)?;
    bound(&lease.request.physical_path, selected)?;
    match &args.action {
        RecoveryCommand::BackupOffline => {
            let r: OfflineBackupRequestV1 = private_request(request_path()?, &l)?;
            if r.version != 1
                || r.operation_id != lease.request.operation_id
                || r.source != lease.request.physical_path
                || r.store.canonical_path.as_deref() != Some(r.source.as_path())
            {
                return Err(Error::Refused(
                    "matching existing physical source request and lease required".into(),
                ));
            }
            let request = BackupRequest {
                operation_id: r.operation_id,
                store: r.store,
                source_map_sha256: r.source_map_sha256,
                profile: r.profile,
                batch_id: None,
                destination: filesystem.root().to_path_buf(),
                capture: CaptureBudget::new(
                    l.deadline,
                    r.pin_byte_limit,
                    r.preparation_byte_limit,
                    Duration::from_millis(r.writer_delay_limit_ms),
                ),
                limits: l,
                image_byte_limit: r.image_byte_limit,
                metadata_byte_limit: r.metadata_byte_limit,
                space_margin_bytes: r.space_margin_bytes,
                configs: r.configs,
                excluded_vault_key: r.excluded_vault_key,
                code_index_configuration: r.code_index_configuration,
            };
            let outcome =
                backup::backup_offline(&request, &lease, &filesystem, &CancelToken::new())
                    .map_err(error)?;
            Ok(match outcome {
                PublicationOutcome::Durable(bundle) => {
                    serde_json::json!({"outcome":"durable","bundle":bundle.path()})
                }
                PublicationOutcome::OutcomeUnknown { operation_id } => {
                    serde_json::json!({"outcome":"outcome_unknown","operation_id":operation_id})
                }
            })
        }
        RecoveryCommand::Restore => {
            let r: OfflineRestoreRequestV1 = private_request(request_path()?, &l)?;
            if r.version != 1
                || r.operation_id != lease.request.operation_id
                || r.destination != lease.request.physical_path
            {
                return Err(Error::Refused(
                    "matching destination request and lease required".into(),
                ));
            }
            let bundle = catalog::select_completed_bundle(r.bundle_operation, &filesystem, &l)
                .map_err(error)?;
            let request = RestoreRequest {
                operation_id: r.operation_id,
                store: r.store,
                destination: r.destination,
                staging_parent: r.staging_parent,
                selected_bundle: bundle,
                limits: l,
                metadata_byte_limit: r.metadata_byte_limit,
                space_margin_bytes: r.space_margin_bytes,
                original_profile: r.original_profile,
                available_dependencies: r.available_dependencies,
            };
            serde_json::to_value(
                restore::restore_offline(&request, &lease, &filesystem).map_err(error)?,
            )
            .map_err(Error::Json)
        }
        RecoveryCommand::Resume
        | RecoveryCommand::Rollback
        | RecoveryCommand::Abandon
        | RecoveryCommand::Status => {
            let r: RestoreContinuationRequestV1 = private_request(request_path()?, &l)?;
            continuation(&r, &lease, selected)?;
            if matches!(args.action, RecoveryCommand::Status) {
                return serde_json::to_value(
                    restore::restore_status(&r.journal, &lease, &filesystem).map_err(error)?,
                )
                .map_err(Error::Json);
            }
            let journal =
                restore::load_restore_journal(&r.journal, &lease, &filesystem).map_err(error)?;
            let result = if matches!(args.action, RecoveryCommand::Resume) {
                restore::resume_restore(&journal, &lease, &filesystem)
            } else if matches!(args.action, RecoveryCommand::Abandon) {
                restore::abandon_restore(&journal, &lease, &filesystem)
            } else {
                restore::rollback_restore(&journal, &lease, &filesystem)
            };
            serde_json::to_value(result.map_err(error)?).map_err(Error::Json)
        }
        RecoveryCommand::Finalize | RecoveryCommand::FinalizeRollback => {
            let r: RestoreFinalizeRequestV1 = private_request(request_path()?, &l)?;
            continuation(
                &RestoreContinuationRequestV1 {
                    version: r.version,
                    operation_id: r.operation_id,
                    destination: r.destination,
                    journal: r.journal.clone(),
                },
                &lease,
                selected,
            )?;
            let journal =
                restore::load_restore_journal(&r.journal, &lease, &filesystem).map_err(error)?;
            let result = if matches!(args.action, RecoveryCommand::Finalize) {
                readiness::finalize_restore(&journal, &lease, &r.reconciliation, &filesystem)
            } else {
                readiness::finalize_rollback(&journal, &lease, &r.reconciliation, &filesystem)
            };
            serde_json::to_value(result.map_err(error)?).map_err(Error::Json)
        }
        RecoveryCommand::FinalizeAbsence => {
            let r: AbsenceFinalizeRequestV1 = private_request(request_path()?, &l)?;
            continuation(
                &RestoreContinuationRequestV1 {
                    version: r.version,
                    operation_id: r.operation_id,
                    destination: r.destination,
                    journal: r.journal.clone(),
                },
                &lease,
                selected,
            )?;
            let journal =
                restore::load_restore_journal(&r.journal, &lease, &filesystem).map_err(error)?;
            serde_json::to_value(
                readiness::finalize_original_absence(
                    &journal,
                    &lease,
                    &r.reconciliation,
                    &filesystem,
                )
                .map_err(error)?,
            )
            .map_err(Error::Json)
        }
        RecoveryCommand::Purge { confirm } => {
            let r: PurgeRequest = private_request(request_path()?, &l)?;
            if r.confirm != *confirm {
                return Err(Error::Refused("request confirmation must match explicit --confirm <op-id> (absent means dry-run)".into()));
            }
            serde_json::to_value(
                purge::purge_quarantined(&r, &lease, &filesystem, &l).map_err(error)?,
            )
            .map_err(Error::Json)
        }
        RecoveryCommand::Lease
        | RecoveryCommand::Inspect { .. }
        | RecoveryCommand::Verify { .. } => {
            unreachable!("read-only and lease commands returned above")
        }
    }
}
