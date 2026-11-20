//! Explicit operator evidence for admission. This module performs no external I/O.
use super::{VerificationReport, manifest::DependencyManifest};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexReadiness {
    Lexical,
    Degraded,
    Full,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientReconciliation {
    pub clients_invalidated: bool,
    pub rpc_sessions_restarted: bool,
    pub transactions_drained: bool,
    pub caches_invalidated: bool,
    pub retry_assumptions_reconciled: bool,
    pub evidence_sha256: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DependencyResolution {
    pub reference_sha256: String,
    pub catalog_identity: String,
    pub observed_sequence: u64,
    pub evidence_sha256: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum EffectResolution {
    Unknown,
    Verified {
        persistent_sequence: u64,
        evidence_sha256: String,
    },
    ReconciledBeyondHorizon {
        retry_suppressed: bool,
        evidence_sha256: String,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalOutcome {
    pub scope: String,
    pub resolution: EffectResolution,
}
/// No default: every operator must explicitly account for all affected state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReconciliationRecord {
    pub version: u32,
    pub operation_id: uuid::Uuid,
    pub image_sha256: String,
    pub published_sequence: u64,
    pub operator: String,
    pub recorded_at_unix_ms: u128,
    pub affected_scopes: Vec<String>,
    pub identity_confirmed: bool,
    pub reference_closure_confirmed: bool,
    pub external_effect_coverage_confirmed: bool,
    pub source_publication_resolution_sha256: String,
    pub clients: ClientReconciliation,
    pub dependencies: Vec<DependencyResolution>,
    pub external_outcomes: Vec<ExternalOutcome>,
    pub unresolved_items: Vec<String>,
    pub index: IndexReadiness,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadinessReport {
    pub ordinary_admission_ready: bool,
    pub index: IndexReadiness,
    pub blockers: Vec<String>,
    pub external_dispatch_enabled: bool,
}
pub fn assess_readiness(
    report: &VerificationReport,
    dependencies: &DependencyManifest,
    r: &ReconciliationRecord,
) -> ReadinessReport {
    use super::manifest::sha256;
    let mut blockers = Vec::new();
    if r.version != 1
        || r.operation_id.to_string() != report.operation_id
        || r.operation_id.is_nil()
        || r.image_sha256 != report.image_sha256
        || r.published_sequence != report.published_sequence
        || r.operator.trim().is_empty()
        || r.recorded_at_unix_ms == 0
        || r.affected_scopes.is_empty()
        || r.affected_scopes.iter().any(|s| s.trim().is_empty())
        || !r.identity_confirmed
        || !r.reference_closure_confirmed
        || !r.external_effect_coverage_confirmed
        || !sha256(&r.source_publication_resolution_sha256)
        || !r.unresolved_items.is_empty()
    {
        blockers.push("identity, reference or external-effect reconciliation incomplete".into());
    }
    let c = &r.clients;
    if !c.clients_invalidated
        || !c.rpc_sessions_restarted
        || !c.transactions_drained
        || !c.caches_invalidated
        || !c.retry_assumptions_reconciled
        || !sha256(&c.evidence_sha256)
    {
        blockers.push(
            "stale clients, sessions, transactions, caches or retry assumptions remain".into(),
        );
    }
    if dependencies.references.len() as u64 != report.dependency_count
        || r.dependencies.len() != dependencies.references.len()
        || dependencies.references.iter().any(|d| {
            let matched: Vec<_> = r
                .dependencies
                .iter()
                .filter(|e| {
                    e.reference_sha256 == d.reference_sha256
                        && e.catalog_identity == d.catalog_identity
                })
                .collect();
            matched.len() != 1
                || !sha256(&matched[0].evidence_sha256)
                || d.required_sequence
                    .is_some_and(|s| matched[0].observed_sequence != s)
        })
    {
        blockers.push("cross-store dependency sequence unresolved".into());
    }
    if r.external_outcomes.iter().any(|e| {
        !r.affected_scopes.contains(&e.scope)
            || match &e.resolution {
                EffectResolution::Unknown => true,
                EffectResolution::Verified {
                    persistent_sequence,
                    evidence_sha256,
                } => *persistent_sequence > report.published_sequence || !sha256(evidence_sha256),
                EffectResolution::ReconciledBeyondHorizon {
                    retry_suppressed,
                    evidence_sha256,
                } => !*retry_suppressed || !sha256(evidence_sha256),
            }
    }) {
        blockers.push("unknown or newer external effects require operator resolution".into());
    }
    ReadinessReport {
        ordinary_admission_ready: blockers.is_empty(),
        index: r.index.clone(),
        blockers,
        external_dispatch_enabled: false,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::recovery::manifest::{Availability, BuildIdentity, Readiness};
    fn report() -> VerificationReport {
        VerificationReport {
            report_version: 1,
            copy_evidence: None,
            operation_id: uuid::Uuid::now_v7().to_string(),
            image_sha256: "a".repeat(64),
            image_bytes: 4096,
            published_sequence: 7,
            logical_sha256: "b".repeat(64),
            profile_version: 1,
            profile_sha256: "c".repeat(64),
            canonical_encoding_version: 1,
            intrinsic_lineage_available: false,
            external_publish_status:
                crate::ferrostore::ExternalPublishStatus::ExclusionUnprovenBeforeSingleOwnership,
            verified_collection_count: 2,
            unknown_collection_count: 0,
            opaque_runtime_references: false,
            query_probe_count: 4,
            exception_count: 0,
            dependency_count: 0,
            transient_rpc_dedupe_persisted: false,
            durable_rpc_receipts_claimed: false,
            readiness: Readiness::VerifiedLocalContents,
            verifier_build_identity: BuildIdentity {
                package_version: "fixture".into(),
                source_revision: Availability::Unavailable,
                executable_sha256: Availability::Unavailable,
            },
        }
    }
    fn evidence(report: &VerificationReport) -> ReconciliationRecord {
        ReconciliationRecord {
            version: 1,
            operation_id: uuid::Uuid::parse_str(&report.operation_id).unwrap(),
            image_sha256: report.image_sha256.clone(),
            published_sequence: 7,
            operator: "fixture operator".into(),
            recorded_at_unix_ms: 1,
            affected_scopes: vec!["fixture".into()],
            identity_confirmed: true,
            reference_closure_confirmed: true,
            external_effect_coverage_confirmed: true,
            source_publication_resolution_sha256: "d".repeat(64),
            clients: ClientReconciliation {
                clients_invalidated: true,
                rpc_sessions_restarted: true,
                transactions_drained: true,
                caches_invalidated: true,
                retry_assumptions_reconciled: true,
                evidence_sha256: "e".repeat(64),
            },
            dependencies: vec![],
            external_outcomes: vec![],
            unresolved_items: vec![],
            index: IndexReadiness::Lexical,
        }
    }
    #[test]
    fn restore_finalize_requires_exact_dependency_sequence_and_unique_evidence() {
        let mut report = report();
        report.dependency_count = 1;
        let mut record = evidence(&report);
        let dependencies = DependencyManifest {
            references: vec![crate::recovery::profiles::ReferenceDependency {
                reference_sha256: "1".repeat(64),
                catalog_identity: "other fixture store".into(),
                required_sequence: Some(11),
                policy: crate::recovery::profiles::DependencyPolicy::CoordinatedRecoveryRequired,
            }],
        };
        assert!(!assess_readiness(&report, &dependencies, &record).ordinary_admission_ready);
        record.dependencies.push(DependencyResolution {
            reference_sha256: "1".repeat(64),
            catalog_identity: "other fixture store".into(),
            observed_sequence: 12,
            evidence_sha256: "2".repeat(64),
        });
        assert!(!assess_readiness(&report, &dependencies, &record).ordinary_admission_ready);
        record.dependencies[0].observed_sequence = 11;
        record.dependencies[0].reference_sha256 = "3".repeat(64);
        assert!(!assess_readiness(&report, &dependencies, &record).ordinary_admission_ready);
        record.dependencies[0].reference_sha256 = "1".repeat(64);
        for index in [
            IndexReadiness::Lexical,
            IndexReadiness::Degraded,
            IndexReadiness::Full,
        ] {
            record.index = index;
            let result = assess_readiness(&report, &dependencies, &record);
            assert!(result.ordinary_admission_ready);
            assert!(!result.external_dispatch_enabled);
        }
        record.dependencies.push(record.dependencies[0].clone());
        assert!(!assess_readiness(&report, &dependencies, &record).ordinary_admission_ready);
        record.dependencies.pop();
        record.dependencies[0].evidence_sha256.clear();
        assert!(!assess_readiness(&report, &dependencies, &record).ordinary_admission_ready);
    }
    #[test]
    fn restore_finalize_requires_reconciliation_but_not_full_index() {
        let report = report();
        let mut record = evidence(&report);
        let dependencies = DependencyManifest { references: vec![] };
        assert!(assess_readiness(&report, &dependencies, &record).ordinary_admission_ready);
        record.index = IndexReadiness::Degraded;
        assert!(assess_readiness(&report, &dependencies, &record).ordinary_admission_ready);
        record.clients.rpc_sessions_restarted = false;
        assert!(!assess_readiness(&report, &dependencies, &record).ordinary_admission_ready);
        record.clients.rpc_sessions_restarted = true;
        record.external_outcomes.push(ExternalOutcome {
            scope: "fixture".into(),
            resolution: EffectResolution::Unknown,
        });
        assert!(!assess_readiness(&report, &dependencies, &record).ordinary_admission_ready);
        record.external_outcomes[0].resolution = EffectResolution::Verified {
            persistent_sequence: 8,
            evidence_sha256: "f".repeat(64),
        };
        assert!(!assess_readiness(&report, &dependencies, &record).ordinary_admission_ready);
        record.external_outcomes[0].resolution = EffectResolution::ReconciledBeyondHorizon {
            retry_suppressed: true,
            evidence_sha256: "f".repeat(64),
        };
        let ready = assess_readiness(&report, &dependencies, &record);
        assert!(ready.ordinary_admission_ready);
        assert!(!ready.external_dispatch_enabled);
    }
}

pub fn finalize_restore(
    journal: &super::restore::RestoreJournal,
    lease: &super::admission::OfflineLease,
    record: &ReconciliationRecord,
    fs: &dyn super::fs::RecoveryFs,
) -> Result<super::restore::RestoreOutcome, super::RecoveryError> {
    super::restore::finalize(journal, lease, record, fs, false)
}
pub fn finalize_rollback(
    journal: &super::restore::RestoreJournal,
    lease: &super::admission::OfflineLease,
    record: &ReconciliationRecord,
    fs: &dyn super::fs::RecoveryFs,
) -> Result<super::restore::RestoreOutcome, super::RecoveryError> {
    super::restore::finalize(journal, lease, record, fs, true)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AbsenceReconciliationRecord {
    pub version: u32,
    pub operation_id: uuid::Uuid,
    pub candidate_sha256: String,
    pub operator: String,
    pub recorded_at_unix_ms: u128,
    pub no_client_opened_path_while_installed: bool,
    pub evidence_sha256: String,
}
pub fn finalize_original_absence(
    journal: &super::restore::RestoreJournal,
    lease: &super::admission::OfflineLease,
    record: &AbsenceReconciliationRecord,
    fs: &dyn super::fs::RecoveryFs,
) -> Result<super::restore::RestoreOutcome, super::RecoveryError> {
    super::restore::finalize_absence(journal, lease, record, fs)
}
