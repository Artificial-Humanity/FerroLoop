//! Versioned metadata records and explicit deployment evidence.
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
/// Supplied observations remain distinct from this collector's filesystem observations.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Observation {
    pub source: String,
    pub observed_at: String,
    pub value: String,
}
impl Observation {
    pub(super) fn attributed(&self) -> bool {
        !self.source.trim().is_empty() && !self.observed_at.trim().is_empty()
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConsumerEdge {
    pub caller: String,
    pub scope: String,
    pub effects: Vec<String>,
    pub handle_lifetime: String,
    pub source_symbol: String,
    pub source_revision: String,
    pub declared_mode: String,
    pub provenance: String,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BindingInput {
    pub caller: String,
    pub scope: String,
    pub project_root: Option<PathBuf>,
    pub logical_path: PathBuf,
    pub backend_env: Option<String>,
    pub source: String,
    pub precedence: String,
    pub fallback_target: Option<PathBuf>,
    pub hub_slug: Option<String>,
    pub declared_mode: String,
}
impl BindingInput {
    pub fn config(scope: &str, path: PathBuf, backend_env: Option<String>, source: &str) -> Self {
        Self {
            caller: "config".into(),
            scope: scope.into(),
            logical_path: path,
            backend_env,
            source: source.into(),
            precedence: "configured binding".into(),
            declared_mode: "unspecified_legacy_auto".into(),
            ..Default::default()
        }
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UnitInput {
    pub name: String,
    pub file: PathBuf,
    pub store_path: PathBuf,
    pub socket_path: Option<PathBuf>,
    pub intended_owner: Option<String>,
    pub declared_mode: Option<String>,
    pub observation: Option<Observation>,
    pub observed_access_mode: Option<Observation>,
    pub installed_executable: Option<PathBuf>,
    pub installed_build: Option<Observation>,
    pub running_build: Option<Observation>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RecoveryReferences {
    pub store_path: PathBuf,
    pub backup_policy: Option<String>,
    pub last_verified_artifact: Option<String>,
    pub index_rebuild_config: Option<String>,
    pub provenance: Option<Observation>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DeploymentMetadata {
    pub units: Vec<UnitInput>,
    pub bindings: Vec<BindingInput>,
    pub consumers: Vec<ConsumerEdge>,
    pub recovery_references: Vec<RecoveryReferences>,
    /// Coverage of external scripts/library consumers, not a claim about observed traffic.
    pub external_coverage: Option<Observation>,
    pub unit_coverage: Option<Observation>,
}
#[derive(Clone, Debug, Default)]
pub struct InventoryInputs {
    pub roots: Vec<PathBuf>,
    pub home: Option<PathBuf>,
    pub host_identifier: Option<String>,
    pub config_path: Option<PathBuf>,
    pub config_digest: Option<String>,
    pub collector_build: serde_json::Value,
    pub bindings: Vec<BindingInput>,
    pub consumers: Vec<ConsumerEdge>,
    pub deployment: DeploymentMetadata,
    pub warnings: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct Link {
    pub path: PathBuf,
    pub target: PathBuf,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoreIdentity {
    /// Catalog identity derived from the canonical path; not an intrinsic store UUID.
    pub catalog_key: String,
    pub given_paths: Vec<PathBuf>,
    pub canonical_path: Option<PathBuf>,
    pub exists: bool,
    pub file_type: String,
    pub symlink_chain: Vec<Link>,
    pub filesystem_identity: Option<String>,
    pub identity_confidence: String,
    pub intrinsic_lineage: Option<String>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FileMetadata {
    pub size_bytes: Option<u64>,
    pub modified_unix_ms: Option<u128>,
    pub mode: Option<u32>,
    pub owner_uid: Option<u32>,
    pub hardlink_count: Option<u64>,
    pub filesystem_device: Option<u64>,
    pub filesystem_type: Option<String>,
    pub acl_summary: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UnitMetadata {
    pub declared: UnitInput,
    pub unit_digest: Option<String>,
    pub installed_executable_digest: Option<String>,
    pub file_binding_verified: bool,
    pub validation_warnings: Vec<String>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Ownership {
    pub declared_modes: Vec<String>,
    pub mode_sources: Vec<String>,
    pub units: Vec<UnitMetadata>,
    pub observed_access_mode: Option<String>,
    pub desired_mode: Option<String>,
    pub conflicts: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoreRecord {
    pub identity: StoreIdentity,
    pub classification: String,
    pub classification_evidence: Vec<String>,
    pub owner_decision: Option<String>,
    pub metadata: FileMetadata,
    pub ownership: Ownership,
    pub consumers: Vec<ConsumerEdge>,
    pub recovery: Vec<RecoveryReferences>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BindingEdge {
    pub caller: String,
    pub scope: String,
    pub project_root: Option<PathBuf>,
    pub logical_path: PathBuf,
    pub selected_backend: String,
    pub backend_source: String,
    pub physical_path: PathBuf,
    pub effective_path: Option<PathBuf>,
    pub source: String,
    pub precedence: String,
    pub fallback_target: Option<PathBuf>,
    pub fallback_used: bool,
    pub refusal_status: Option<u16>,
    pub declared_mode: String,
    pub mismatch: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoreMap {
    pub schema_version: u32,
    pub collected_at_unix_ms: u128,
    pub host_identifier: Option<String>,
    pub collector_build: serde_json::Value,
    pub scan_roots: Vec<PathBuf>,
    pub required_additional_roots: Vec<PathBuf>,
    pub config_path: Option<PathBuf>,
    pub config_digest: Option<String>,
    pub coverage: DeploymentMetadata,
    pub stores: Vec<StoreRecord>,
    pub bindings: Vec<BindingEdge>,
    pub warnings: Vec<String>,
    pub complete: bool,
}
