//! Pure types for hosted session protocol profiles and ACP proof evidence.
//!
//! These types do not verify evidence, launch manifests, or permission authority. The proof
//! predicate is a necessary structural gate, not authorization to dispatch production work.

use serde::{Deserialize, Serialize};

/// Protocol selected underneath a vendor's hosted session.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionProtocolKind {
    #[default]
    Native,
    AcpV1Stdio,
}

/// Strength of evidence for one protocol operation.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConformanceState {
    #[default]
    Unproven,
    Advertised,
    RuntimePassed,
    RuntimeFailed,
    Unsupported,
}

/// Charter operations required for an ACP v1 production profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcpOperation {
    Approvals,
    Interrupt,
    Reset,
    Compact,
    ThreadIdentity,
    Usage,
}

impl AcpOperation {
    pub const ALL: [Self; 6] = [
        Self::Approvals,
        Self::Interrupt,
        Self::Reset,
        Self::Compact,
        Self::ThreadIdentity,
        Self::Usage,
    ];
}

/// Nonsecret identity and isolation bindings for a particular ACP profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcpProfileFingerprint {
    pub vendor_id: String,
    pub bridge_artifact_digest: String,
    pub bridge_version: String,
    pub native_artifact_digest: String,
    pub native_version: String,
    pub protocol_major: u32,
    pub schema_digest: String,
    pub extension_manifest_digest: String,
    pub platform: String,
    pub auth_billing_binding: String,
    pub auth_provisioning_state: String,
    pub auth_authority_receipt: String,
    pub auth_config_home_binding: String,
    pub config_digest: String,
    pub permission_policy_digest: String,
}

/// Conformance evidence for an individual ACP operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationEvidence {
    pub operation: AcpOperation,
    pub state: ConformanceState,
    pub required_capabilities: Vec<String>,
    pub mechanism: String,
    pub semantic_constraints: Vec<String>,
    pub test_evidence_refs: Vec<String>,
}

impl OperationEvidence {
    pub fn unproven(operation: AcpOperation) -> Self {
        Self {
            operation,
            state: ConformanceState::Unproven,
            required_capabilities: Vec::new(),
            mechanism: String::new(),
            semantic_constraints: Vec::new(),
            test_evidence_refs: Vec::new(),
        }
    }
}

/// Reviewed evidence and the exact profile identity for which it applies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcpProfileProofRecord {
    pub fingerprint: AcpProfileFingerprint,
    pub test_suite_revision: String,
    pub source_references: Vec<String>,
    pub redacted_transcripts: Vec<String>,
    pub operations: Vec<OperationEvidence>,
}

impl AcpProfileProofRecord {
    /// Whether this record meets the structural v1 proof gate. This does not verify the
    /// proof's provenance, match the active launch, or authorize a production dispatch.
    pub fn production_admitted(&self) -> bool {
        let f = &self.fingerprint;
        let bindings = [
            &f.vendor_id,
            &f.bridge_artifact_digest,
            &f.bridge_version,
            &f.native_artifact_digest,
            &f.native_version,
            &f.schema_digest,
            &f.extension_manifest_digest,
            &f.platform,
            &f.auth_billing_binding,
            &f.auth_provisioning_state,
            &f.auth_authority_receipt,
            &f.auth_config_home_binding,
            &f.config_digest,
            &f.permission_policy_digest,
        ];
        f.protocol_major == 1
            && bindings.iter().all(|binding| !binding.trim().is_empty())
            && self.operations.len() == AcpOperation::ALL.len()
            && AcpOperation::ALL.into_iter().all(|operation| {
                let mut rows = self
                    .operations
                    .iter()
                    .filter(|row| row.operation == operation);
                rows.next()
                    .is_some_and(|row| row.state == ConformanceState::RuntimePassed)
                    && rows.next().is_none()
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_proof() -> AcpProfileProofRecord {
        AcpProfileProofRecord {
            fingerprint: AcpProfileFingerprint {
                vendor_id: "vendor".into(),
                bridge_artifact_digest: "bridge-digest".into(),
                bridge_version: "1.0".into(),
                native_artifact_digest: "native-digest".into(),
                native_version: "2.0".into(),
                protocol_major: 1,
                schema_digest: "schema".into(),
                extension_manifest_digest: "extensions".into(),
                platform: "platform".into(),
                auth_billing_binding: "billing".into(),
                auth_provisioning_state: "provisioned".into(),
                auth_authority_receipt: "receipt".into(),
                auth_config_home_binding: "isolated-home".into(),
                config_digest: "config".into(),
                permission_policy_digest: "policy".into(),
            },
            test_suite_revision: "revision".into(),
            source_references: vec!["source".into()],
            redacted_transcripts: vec!["redacted".into()],
            operations: AcpOperation::ALL
                .into_iter()
                .map(|operation| OperationEvidence {
                    operation,
                    state: ConformanceState::RuntimePassed,
                    required_capabilities: vec!["capability".into()],
                    mechanism: "mechanism".into(),
                    semantic_constraints: vec!["constraint".into()],
                    test_evidence_refs: vec!["test".into()],
                })
                .collect(),
        }
    }

    #[test]
    fn complete_v1_proof_passes_independent_of_row_order() {
        let mut proof = valid_proof();
        assert!(proof.production_admitted());
        proof.operations.reverse();
        assert!(proof.production_admitted());
        for major in [0, 2, u32::MAX] {
            proof.fingerprint.protocol_major = major;
            assert!(!proof.production_admitted(), "version {major}");
        }
    }

    #[test]
    fn each_binding_is_required_and_whitespace_is_not_a_binding() {
        for index in 0..14 {
            let mut proof = valid_proof();
            let fields: [&mut String; 14] = [
                &mut proof.fingerprint.vendor_id,
                &mut proof.fingerprint.bridge_artifact_digest,
                &mut proof.fingerprint.bridge_version,
                &mut proof.fingerprint.native_artifact_digest,
                &mut proof.fingerprint.native_version,
                &mut proof.fingerprint.schema_digest,
                &mut proof.fingerprint.extension_manifest_digest,
                &mut proof.fingerprint.platform,
                &mut proof.fingerprint.auth_billing_binding,
                &mut proof.fingerprint.auth_provisioning_state,
                &mut proof.fingerprint.auth_authority_receipt,
                &mut proof.fingerprint.auth_config_home_binding,
                &mut proof.fingerprint.config_digest,
                &mut proof.fingerprint.permission_policy_digest,
            ];
            *fields.into_iter().nth(index).unwrap() = " \t\n".into();
            assert!(!proof.production_admitted(), "field {index}");
        }
    }

    #[test]
    fn every_operation_requires_exactly_one_runtime_passed_row() {
        for index in 0..AcpOperation::ALL.len() {
            let mut missing = valid_proof();
            missing.operations.remove(index);
            assert!(!missing.production_admitted(), "missing {index}");
            for state in [
                ConformanceState::Unproven,
                ConformanceState::Advertised,
                ConformanceState::RuntimeFailed,
                ConformanceState::Unsupported,
            ] {
                let mut proof = valid_proof();
                proof.operations[index].state = state;
                assert!(!proof.production_admitted(), "{index} {state:?}");
            }
            let mut duplicate = valid_proof();
            duplicate
                .operations
                .push(duplicate.operations[index].clone());
            assert!(!duplicate.production_admitted(), "duplicate {index}");
            duplicate.operations.last_mut().unwrap().state = ConformanceState::RuntimeFailed;
            assert!(!duplicate.production_admitted(), "conflicting {index}");
        }
    }

    #[test]
    fn evidence_round_trips_and_native_is_default() {
        assert_eq!(SessionProtocolKind::default(), SessionProtocolKind::Native);
        assert_eq!(
            AcpOperation::ALL,
            [
                AcpOperation::Approvals,
                AcpOperation::Interrupt,
                AcpOperation::Reset,
                AcpOperation::Compact,
                AcpOperation::ThreadIdentity,
                AcpOperation::Usage
            ]
        );
        let proof = valid_proof();
        let json = serde_json::to_value(&proof).unwrap();
        assert_eq!(json["operations"][0]["operation"], "approvals");
        assert_eq!(json["operations"][0]["state"], "runtime_passed");
        assert_eq!(
            serde_json::from_value::<AcpProfileProofRecord>(json).unwrap(),
            proof
        );
        for op in AcpOperation::ALL {
            let evidence = OperationEvidence::unproven(op);
            assert_eq!(evidence.operation, op);
            assert_eq!(evidence.state, ConformanceState::Unproven);
            assert!(evidence.required_capabilities.is_empty());
            assert!(evidence.mechanism.is_empty());
            assert!(evidence.semantic_constraints.is_empty());
            assert!(evidence.test_evidence_refs.is_empty());
        }
    }
}
