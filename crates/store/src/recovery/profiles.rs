//! Private, versioned validation policy. No profile authorizes external I/O.
use serde::{Deserialize, Serialize};

pub const PROFILE_VERSION: u32 = 1;
/// Version 1 requires exact vector IDs/order and absolute score error <= 1e-9.
pub const VECTOR_SCORE_TOLERANCE_V1: f64 = 1e-9;
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileKind {
    Workspace,
    ProjectLedgerRecords,
    CouncilPrivacyRevisions,
    SmallEmpty,
    MixedTablesVectors,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidationProfile {
    pub version: u32,
    pub kind: ProfileKind,
    /// Catalog assertion, not intrinsic provenance. The engine has no lineage UUID.
    pub catalog_identity: String,
    pub dependencies: Vec<ReferenceDependency>,
    pub preexisting_exceptions: Vec<ReferenceException>,
    /// Additional deployment-specific references, including opaque runtime state.
    pub reference_rules: Vec<ReferenceRule>,
    /// Additional fixed probes. Automatic point/absent/scan/query/lexical/vector
    /// probes also run over every applicable collection, regardless of class.
    pub probes: Vec<QueryProbe>,
}
impl ValidationProfile {
    pub fn new(kind: ProfileKind, catalog_identity: impl Into<String>) -> Self {
        Self {
            version: PROFILE_VERSION,
            kind,
            catalog_identity: catalog_identity.into(),
            dependencies: Vec::new(),
            preexisting_exceptions: Vec::new(),
            reference_rules: Vec::new(),
            probes: Vec::new(),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReferenceRule {
    pub source_collection: String,
    /// JSON pointer; a string, integer or array of those denotes target keys.
    pub pointer: String,
    pub target_collection: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReferenceException {
    pub reference_sha256: String,
    pub reason: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DependencyPolicy {
    CoordinatedRecoveryRequired,
    ReconcileBeforeAdmission,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReferenceDependency {
    pub reference_sha256: String,
    pub catalog_identity: String,
    pub required_sequence: Option<u64>,
    pub policy: DependencyPolicy,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum QueryProbe {
    Point {
        collection: String,
        id: String,
    },
    Absent {
        collection: String,
        id: String,
    },
    Scan {
        collection: String,
    },
    Query {
        collection: String,
        field: String,
        value: String,
    },
    Lexical {
        collection: String,
        query: String,
        limit: usize,
    },
    Vector {
        collection: String,
        vector: Vec<f32>,
        limit: usize,
    },
}
