//! Versioned private bundle records. Publication/durability is a later operation.
use super::profiles::ReferenceDependency;
use crate::ferrostore::{ExternalPublishStatus, ImageDescriptor};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "availability", content = "value", rename_all = "snake_case")]
pub enum Availability<T> {
    Available(T),
    Unavailable,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalPublicationStatus {
    NotPublished,
    OutcomeUnknown,
    Durable,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndependentCopyStatus {
    NotAttempted,
    Verified,
    OutcomeUnknown,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberKind {
    ProtectedConfig,
    Warrants,
    VaultCiphertext,
    ValidationExpectations,
    ValidationProfile,
    VerificationReport,
    IndexConfiguration,
}
/// Config object lives only at `.batch-config/<batch_id>` under the configured
/// backup root. Old strict readers reject this additive V1 field; new readers
/// preserve old V1 encoding when the reference is absent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SharedConfigReference {
    pub batch_id: uuid::Uuid,
    pub descriptor_sha256: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchConfigV1 {
    pub version: u32,
    pub batch_id: uuid::Uuid,
    pub members: Vec<MemberChecksum>,
}
/// Typed members deliberately have no vault-key or arbitrary secret-path variant.
/// The later publisher must allowlist paths/identities before copying anything.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemberChecksum {
    pub kind: MemberKind,
    pub relative_name: String,
    pub bytes: u64,
    pub sha256: String,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectiveCodeIndexConfig {
    pub configuration_version: u32,
    pub effective_configuration_sha256: String,
    pub source_root_identity: Availability<String>,
    pub repository: Availability<String>,
    pub source_revision: Availability<String>,
    pub scan_rules: Vec<String>,
    pub ignore_rules: Vec<String>,
    pub languages_and_parser_versions: Vec<String>,
    pub embedding_space: String,
    pub embedding_dimensions: u32,
    pub embedding_configuration: serde_json::Value,
    pub ranking_version: String,
    pub rebuild_vector_budget: u64,
    pub rebuild_time_budget_ms: u64,
    /// All values must be the effective versioned defaults/overrides, not raw
    /// config keys. The producer supplies these without opening source roots.
    pub defaults_build_identity: String,
    pub recovery_dependencies: Vec<String>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleManifestV1 {
    pub manifest_version: u32,
    pub operation_id: String,
    pub batch_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared_config: Option<SharedConfigReference>,
    pub catalog_store_identity: String,
    pub intrinsic_lineage: Availability<String>,
    pub source_map_sha256: String,
    pub container_version: u32,
    pub snapshot_version: u32,
    pub application_schema_version: Availability<u64>,
    pub engine: u32,
    pub incompatible_features: u64,
    pub published_sequence: u64,
    pub external_publish_status: ExternalPublishStatus,
    pub captured_at_unix_ms: u128,
    pub published_at_unix_ms: Option<u128>,
    pub creator_build_identity: BuildIdentity,
    pub verifier_build_identity: BuildIdentity,
    pub image_bytes: u64,
    pub image_sha256: String,
    pub retained_members: Vec<MemberChecksum>,
    pub logical_sha256: String,
    pub canonical_encoding_version: u32,
    pub validation_profile_version: u32,
    pub validation_profile_sha256: String,
    pub reference_dependencies: Vec<ReferenceDependency>,
    pub exclusions: Vec<Exclusion>,
    pub code_index_configuration: Availability<EffectiveCodeIndexConfig>,
    pub local_status: LocalPublicationStatus,
    pub independent_copy_status: IndependentCopyStatus,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Exclusion {
    EphemeralMq,
    PendingAdapterDeliveries,
    TransientRpcDedupe,
    DerivedCodeSidecarContents,
    VaultKey,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Readiness {
    VerifiedLocalContents,
    DependenciesRequireReconciliation,
    UnknownDomainCoverage,
}
/// Volatile handle evidence from this verification operation, never durable
/// identity, intrinsic lineage, publication, or a caller provenance attestation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CopyCreation {
    WrapperCreated,
    CallerSuppliedExisting,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CopyEvidence {
    pub verified_file_identity: (u64, u64),
    pub source_image_identity: Option<(u64, u64)>,
    pub creation: CopyCreation,
    pub image_sha256: String,
}

/// Diagnostics contain counts, hashes, volatile file identities and typed outcomes only. No payload,
/// collection names, document IDs, catalog identifiers, paths or query text.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationReport {
    pub report_version: u32,
    pub copy_evidence: Option<CopyEvidence>,
    pub operation_id: String,
    pub image_sha256: String,
    pub image_bytes: u64,
    pub published_sequence: u64,
    pub logical_sha256: String,
    pub profile_version: u32,
    pub profile_sha256: String,
    pub canonical_encoding_version: u32,
    pub intrinsic_lineage_available: bool,
    pub external_publish_status: ExternalPublishStatus,
    pub verified_collection_count: u64,
    pub unknown_collection_count: u64,
    /// Runtime JSON may embed application-specific references the generic
    /// store cannot infer, even when its envelope and history are verified.
    pub opaque_runtime_references: bool,
    pub query_probe_count: u64,
    pub exception_count: u64,
    pub dependency_count: u64,
    pub transient_rpc_dedupe_persisted: bool,
    pub durable_rpc_receipts_claimed: bool,
    pub readiness: Readiness,
    pub verifier_build_identity: BuildIdentity,
}
impl VerificationReport {
    pub(crate) fn independent_copy_status(
        &self,
        l: &super::OperationLimits,
    ) -> Result<IndependentCopyStatus, super::RecoveryError> {
        let evidence = self
            .copy_evidence
            .as_ref()
            .ok_or_else(|| l.verification())?;
        if evidence.image_sha256 != self.image_sha256 || !sha256(&evidence.image_sha256) {
            return Err(l.verification());
        }
        match (&evidence.creation, evidence.source_image_identity) {
            (CopyCreation::WrapperCreated, Some(source))
                if source != evidence.verified_file_identity =>
            {
                Ok(IndependentCopyStatus::Verified)
            }
            (CopyCreation::CallerSuppliedExisting, None) => {
                Ok(IndependentCopyStatus::OutcomeUnknown)
            }
            _ => Err(l.verification()),
        }
    }
}
/// Expectations carry only hashes/results, not copies of sensitive source rows.
/// Fields are private: the sole producer is an immutable CapturedStore pin.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidationExpectations {
    pub(crate) version: u32,
    /// Volatile exclusion evidence, never intrinsic lineage or provenance.
    pub(crate) source_file_identity: Option<(u64, u64)>,
    pub(crate) captured_at_unix_ms: Option<u128>,
    pub(crate) descriptor: ImageDescriptor,
    pub(crate) catalog_identity: String,
    pub(crate) profile_sha256: String,
    pub(crate) logical_sha256: String,
    pub(crate) collection_digests: Vec<String>,
    pub(crate) query_digests: Vec<String>,
    pub(crate) vector_results: Vec<Vec<(String, f64)>>,
    pub(crate) known: u64,
    pub(crate) unknown: u64,
    pub(crate) opaque_runtime_references: bool,
    pub(crate) exceptions: u64,
    pub(crate) dependencies: u64,
}
impl std::fmt::Debug for ValidationExpectations {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ValidationExpectations(<private>)")
    }
}
impl ValidationExpectations {
    pub fn logical_sha256(&self) -> &str {
        &self.logical_sha256
    }
    /// Format metadata for the pin. `bytes` is zero here: only the report
    /// authenticates an actual image's physical length and checksum.
    pub fn descriptor(&self) -> &ImageDescriptor {
        &self.descriptor
    }
}

/// A package version is not a unique binary identity. Unavailable evidence is
/// explicit, and creator declarations remain distinct from verifier evidence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildIdentity {
    pub package_version: String,
    pub source_revision: Availability<String>,
    pub executable_sha256: Availability<String>,
}
impl BuildIdentity {
    pub(crate) fn current() -> Self {
        Self {
            package_version: env!("CARGO_PKG_VERSION").into(),
            source_revision: Availability::Unavailable,
            executable_sha256: Availability::Unavailable,
        }
    }
}
#[derive(Clone, Debug)]
pub struct ManifestContext {
    pub batch_id: Option<String>,
    pub shared_config: Option<SharedConfigReference>,
    pub source_map_sha256: String,
    pub creator_build_identity: BuildIdentity,
    pub retained_members: Vec<MemberChecksum>,
    pub code_index_configuration: Availability<EffectiveCodeIndexConfig>,
}
impl BundleManifestV1 {
    /// Prepare a reviewable metadata record. This does not publish, copy
    /// configuration, verify an external dependency, or attest directory durability.
    pub fn verified_unpublished(
        expected: &ValidationExpectations,
        profile: &super::ValidationProfile,
        report: &VerificationReport,
        context: ManifestContext,
        limits: &super::OperationLimits,
    ) -> Result<Self, super::RecoveryError> {
        limits.check()?;
        let independent_copy_status = report.independent_copy_status(limits)?;
        let profile_sha = super::verify::profile_digest(profile, limits)?;
        if expected.version != 1
            || profile.version != super::profiles::PROFILE_VERSION
            || expected.catalog_identity != profile.catalog_identity
            || expected.profile_sha256 != profile_sha
            || report.report_version != 1
            || report.profile_sha256 != expected.profile_sha256
            || report.profile_version != profile.version
            || report.logical_sha256 != expected.logical_sha256
            || report.published_sequence != expected.descriptor.sequence
            || report.canonical_encoding_version != 1
            || report.external_publish_status != expected.descriptor.external_publish_status
            || !sha256(&report.image_sha256)
            || !sha256(&context.source_map_sha256)
            || report.image_bytes == 0
            || report.transient_rpc_dedupe_persisted
            || report.durable_rpc_receipts_claimed
            || report.unknown_collection_count != expected.unknown
            || report.verified_collection_count != expected.known
            || report.opaque_runtime_references != expected.opaque_runtime_references
            || report.dependency_count != expected.dependencies
            || report.exception_count != expected.exceptions
        {
            return Err(limits.verification());
        }
        let mut names = std::collections::BTreeSet::new();
        let mut validation = false;
        let mut verification = false;
        for member in &context.retained_members {
            limits.check()?;
            let allowed = match member.kind {
                MemberKind::ProtectedConfig => "config.toml",
                MemberKind::Warrants => "warrants.json",
                MemberKind::VaultCiphertext => "vault.enc",
                MemberKind::ValidationExpectations => "validation.json",
                MemberKind::ValidationProfile => "profile.json",
                MemberKind::VerificationReport => "report.json",
                MemberKind::IndexConfiguration => "index-config.json",
            };
            if member.relative_name != allowed
                || !names.insert(&member.relative_name)
                || !sha256(&member.sha256)
            {
                return Err(limits.verification());
            }
            let checksum = match member.kind {
                MemberKind::ValidationProfile => {
                    Some(super::verify::serialized_checksum(profile, limits)?)
                }
                MemberKind::ValidationExpectations => {
                    validation = true;
                    Some(super::verify::serialized_checksum(expected, limits)?)
                }
                MemberKind::VerificationReport => {
                    verification = true;
                    Some(super::verify::serialized_checksum(report, limits)?)
                }
                MemberKind::IndexConfiguration => Some(super::verify::serialized_checksum(
                    &context.code_index_configuration,
                    limits,
                )?),
                _ => None,
            };
            if checksum.is_some_and(|(n, hash)| n != member.bytes || hash != member.sha256) {
                return Err(limits.verification());
            }
        }
        if !validation || !verification {
            return Err(limits.verification());
        }
        if let Availability::Available(config) = &context.code_index_configuration
            && (config.configuration_version != 1
                || config.defaults_build_identity.is_empty()
                || config.embedding_dimensions == 0
                || config.embedding_space.is_empty()
                || config.ranking_version.is_empty()
                || config.rebuild_time_budget_ms == 0
                || config.rebuild_vector_budget == 0
                || !sha256(&config.effective_configuration_sha256)
                || !context
                    .retained_members
                    .iter()
                    .any(|m| m.kind == MemberKind::IndexConfiguration))
        {
            return Err(limits.verification());
        }
        let d = &expected.descriptor;
        Ok(Self {
            manifest_version: 1,
            operation_id: report.operation_id.clone(),
            batch_id: context.batch_id,
            shared_config: context.shared_config,
            catalog_store_identity: expected.catalog_identity.clone(),
            intrinsic_lineage: d
                .lineage
                .clone()
                .map_or(Availability::Unavailable, Availability::Available),
            source_map_sha256: context.source_map_sha256,
            container_version: d.container_format,
            snapshot_version: d.snapshot_format,
            application_schema_version: d
                .schema
                .map_or(Availability::Unavailable, Availability::Available),
            engine: d.engine,
            incompatible_features: d.incompatible_features,
            published_sequence: d.sequence,
            external_publish_status: d.external_publish_status,
            captured_at_unix_ms: expected
                .captured_at_unix_ms
                .ok_or_else(|| limits.verification())?,
            published_at_unix_ms: None,
            creator_build_identity: context.creator_build_identity,
            verifier_build_identity: report.verifier_build_identity.clone(),
            image_bytes: report.image_bytes,
            image_sha256: report.image_sha256.clone(),
            retained_members: context.retained_members,
            logical_sha256: expected.logical_sha256.clone(),
            canonical_encoding_version: 1,
            validation_profile_version: profile.version,
            validation_profile_sha256: expected.profile_sha256.clone(),
            reference_dependencies: profile.dependencies.clone(),
            exclusions: vec![
                Exclusion::EphemeralMq,
                Exclusion::PendingAdapterDeliveries,
                Exclusion::TransientRpcDedupe,
                Exclusion::DerivedCodeSidecarContents,
                Exclusion::VaultKey,
            ],
            code_index_configuration: context.code_index_configuration,
            local_status: LocalPublicationStatus::NotPublished,
            independent_copy_status,
        })
    }
}
fn sha256(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn effective_configuration_keeps_defaults_sources_and_rebuild_inputs() {
        let config = EffectiveCodeIndexConfig {
            configuration_version: 1,
            effective_configuration_sha256: "a".repeat(64),
            source_root_identity: Availability::Available("fixture-root".into()),
            repository: Availability::Unavailable,
            source_revision: Availability::Unavailable,
            scan_rules: vec![
                "max_file_bytes=1048576".into(),
                "extensions=rs,ts,tsx,js,py,toml,md".into(),
            ],
            ignore_rules: vec![
                ".git".into(),
                "target".into(),
                "node_modules".into(),
                "dist".into(),
                ".next".into(),
            ],
            languages_and_parser_versions: vec!["fixture-line-parser-v1".into()],
            embedding_space: "fixture-hashing-v1".into(),
            embedding_dimensions: 256,
            embedding_configuration: serde_json::json!({"ngram_min":3,"ngram_max":5,"word_weight":1.0,"bigram_weight":0.5,"ngram_weight":0.35}),
            ranking_version: "fixture-ranking-v1".into(),
            rebuild_vector_budget: 4096,
            rebuild_time_budget_ms: 5000,
            defaults_build_identity: "fixture-build".into(),
            recovery_dependencies: vec!["fixture-source-root".into()],
        };
        let roundtrip: EffectiveCodeIndexConfig =
            serde_json::from_slice(&serde_json::to_vec(&config).unwrap()).unwrap();
        assert_eq!(config, roundtrip);
        assert_eq!(roundtrip.source_revision, Availability::Unavailable);
        // This schema records supplied effective inputs, never reads a root or
        // silently fills missing configuration keys from the local environment.
        let mut absent = serde_json::to_value(config).unwrap();
        absent.as_object_mut().unwrap().remove("ignore_rules");
        assert!(serde_json::from_value::<EffectiveCodeIndexConfig>(absent).is_err());
    }
}
