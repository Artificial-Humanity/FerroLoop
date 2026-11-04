//! Authority boundaries for future routine admission. No implementation here admits work.

use crate::routine::RoutineTarget;
use crate::slice_proof::{AdmittedSliceBound, SliceProofInput};

/// Failure to establish an upstream authority is always a refusal, never an empty success.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuthorityError {
    #[error("required authority could not be verified")]
    UnverifiedAuthority,
}

/// Exact reviewed record revision and installed execution identity to verify.
#[derive(Debug, Clone, Copy)]
pub struct ProofVerificationRequest<'a> {
    pub proof_record_id: &'a str,
    pub proof_revision: u64,
    pub identity: SliceProofInput<'a>,
}

/// Data returned by a trusted verifier. It is not a bearer credential or an admission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedProof {
    pub proof_record_id: String,
    pub proof_revision: u64,
    pub bound: AdmittedSliceBound,
}

/// An implementation must load an authenticated reviewed record, match all installed
/// identity fields, and calculate the complete bound. Mere possession of a record ID is
/// insufficient. This contract performs no I/O or admission by itself.
pub trait ProofRegistryVerifier {
    fn verify_proof(
        &self,
        request: &ProofVerificationRequest<'_>,
    ) -> Result<VerifiedProof, AuthorityError>;
}

/// The precise endpoint selected, including the fencing information the host must prove.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleTarget {
    BoundIncarnation {
        incarnation_id: String,
        thread_epoch: u64,
        controller_lease_id: String,
        controller_lease_epoch: u64,
    },
    FreshSession {
        template_id: String,
        scoped_session_id: String,
        host_spawn_token: String,
    },
}

/// The exact logical work whose scoped session and spawn token are being verified.
/// Values are identifiers, not authority; a trusted verifier must authenticate them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogicalWorkIdentity {
    Routine {
        routine_id: String,
        revision: u64,
        firing_key: String,
    },
    Intent {
        intent_id: String,
        revision: u64,
        member_slot: u32,
    },
}

/// The approval, resolved identity, and immutable host/readings revisions to recheck.
#[derive(Debug, Clone)]
pub struct LifecycleVerificationRequest<'a> {
    pub work: &'a LogicalWorkIdentity,
    pub approved_target: &'a RoutineTarget,
    pub resolved_target: &'a LifecycleTarget,
    pub lifecycle_revision: u64,
    pub now_utc: &'a str,
    pub reading_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedLifecycle {
    pub work: LogicalWorkIdentity,
    pub target: LifecycleTarget,
    pub lifecycle_revision: u64,
    pub reading_revision: u64,
}

/// The verifier is responsible for authenticated issuer provenance, ownership, live
/// telemetry, reading freshness and the exact approved incarnation or template. For a fresh
/// session it must bind the scoped session and spawn token to the authenticated work identity
/// and approved template; matching a template alone is insufficient.
pub trait LifecycleAuthority {
    fn verify_lifecycle(
        &self,
        request: &LifecycleVerificationRequest<'_>,
    ) -> Result<VerifiedLifecycle, AuthorityError>;
}

/// The specific effect and controller attempt whose outcome is being reconciled.
#[derive(Debug, Clone)]
pub struct TerminalVerificationRequest<'a> {
    pub outbox_id: &'a str,
    pub effect_id: &'a str,
    pub attempt: u32,
    pub incarnation_id: &'a str,
    pub controller_lease_epoch: u64,
    pub receipt_id: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifiedTerminalOutcome {
    Completed { tokens: u64, cost_micros: u64 },
    TombstonedUnstarted,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedTerminal {
    pub outbox_id: String,
    pub effect_id: String,
    pub attempt: u32,
    pub incarnation_id: String,
    pub controller_lease_epoch: u64,
    pub receipt_id: String,
    pub receipt_revision: u64,
    pub outcome: VerifiedTerminalOutcome,
}

/// An implementation must verify an authenticated terminal or tombstone receipt for
/// this exact effect and attempt; partial observations and unknown outcomes do not qualify.
pub trait TerminalReceiptVerifier {
    fn verify_terminal(
        &self,
        request: &TerminalVerificationRequest<'_>,
    ) -> Result<VerifiedTerminal, AuthorityError>;
}

/// Production default until trusted upstream verifiers are available.
#[derive(Debug, Default, Clone, Copy)]
pub struct UnavailableAuthority;
impl ProofRegistryVerifier for UnavailableAuthority {
    fn verify_proof(
        &self,
        _: &ProofVerificationRequest<'_>,
    ) -> Result<VerifiedProof, AuthorityError> {
        Err(AuthorityError::UnverifiedAuthority)
    }
}
impl LifecycleAuthority for UnavailableAuthority {
    fn verify_lifecycle(
        &self,
        _: &LifecycleVerificationRequest<'_>,
    ) -> Result<VerifiedLifecycle, AuthorityError> {
        Err(AuthorityError::UnverifiedAuthority)
    }
}
impl TerminalReceiptVerifier for UnavailableAuthority {
    fn verify_terminal(
        &self,
        _: &TerminalVerificationRequest<'_>,
    ) -> Result<VerifiedTerminal, AuthorityError> {
        Err(AuthorityError::UnverifiedAuthority)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A deterministic local fixture implements the verifier contracts. It is not a
    // production authority or a substitute for a transactionally versioned source.
    struct TestAuthority {
        proof: crate::slice_proof::SliceBoundProof,
        lifecycle: LifecycleTarget,
        work: LogicalWorkIdentity,
        terminal: VerifiedTerminal,
    }
    impl ProofRegistryVerifier for TestAuthority {
        fn verify_proof(
            &self,
            request: &ProofVerificationRequest<'_>,
        ) -> Result<VerifiedProof, AuthorityError> {
            if request.proof_record_id != self.proof.proof_record_id || request.proof_revision != 7
            {
                return Err(AuthorityError::UnverifiedAuthority);
            }
            let bound = crate::slice_proof::evaluate_slice_bounds(
                std::slice::from_ref(&self.proof),
                &request.identity,
            )
            .map_err(|_| AuthorityError::UnverifiedAuthority)?;
            Ok(VerifiedProof {
                proof_record_id: self.proof.proof_record_id.clone(),
                proof_revision: 7,
                bound,
            })
        }
    }
    impl LifecycleAuthority for TestAuthority {
        fn verify_lifecycle(
            &self,
            request: &LifecycleVerificationRequest<'_>,
        ) -> Result<VerifiedLifecycle, AuthorityError> {
            let approved_matches = match (request.approved_target, request.resolved_target) {
                (
                    RoutineTarget::BoundIncarnation {
                        incarnation_id: approved,
                    },
                    LifecycleTarget::BoundIncarnation { incarnation_id, .. },
                ) => approved == incarnation_id,
                (
                    RoutineTarget::FreshSession {
                        template_id: approved,
                    },
                    LifecycleTarget::FreshSession { template_id, .. },
                ) => approved == template_id,
                _ => false,
            };
            if request.work != &self.work
                || !approved_matches
                || request.resolved_target != &self.lifecycle
                || request.lifecycle_revision != 4
                || request.reading_revision != 5
                || request.now_utc != "2026-01-01T00:00:00Z"
            {
                return Err(AuthorityError::UnverifiedAuthority);
            }
            Ok(VerifiedLifecycle {
                work: self.work.clone(),
                target: self.lifecycle.clone(),
                lifecycle_revision: 4,
                reading_revision: 5,
            })
        }
    }
    impl TerminalReceiptVerifier for TestAuthority {
        fn verify_terminal(
            &self,
            request: &TerminalVerificationRequest<'_>,
        ) -> Result<VerifiedTerminal, AuthorityError> {
            let t = &self.terminal;
            if request.outbox_id != t.outbox_id
                || request.effect_id != t.effect_id
                || request.attempt != t.attempt
                || request.incarnation_id != t.incarnation_id
                || request.controller_lease_epoch != t.controller_lease_epoch
                || request.receipt_id != t.receipt_id
            {
                return Err(AuthorityError::UnverifiedAuthority);
            }
            Ok(t.clone())
        }
    }
    fn fixture() -> TestAuthority {
        use crate::slice_proof::{AccountingUnit, SliceBoundKind, SliceBoundProof, Usd};
        TestAuthority {
            proof: SliceBoundProof {
                proof_record_id: "reviewed-1".into(),
                adapter: "example".into(),
                mode: "stdio".into(),
                cli_version_range: "=1.0.0".into(),
                binary_hash: format!("sha256:{}", "a".repeat(64)),
                model: "model".into(),
                auth_billing_mode: "subscription".into(),
                config_digest: format!("sha256:{}", "b".repeat(64)),
                kind: SliceBoundKind::PerRequest {
                    max_requests: 2,
                    max_tokens_per_request: 20,
                    max_usd_per_request: Usd::parse("0").unwrap(),
                },
                accounting_unit: AccountingUnit::SubscriptionIncrementalUsd {
                    billing_contract: "contract".into(),
                    zero_cost_proven: true,
                },
                transcript_ref: "reviewed-transcript".into(),
            },
            work: LogicalWorkIdentity::Routine {
                routine_id: "routine".into(),
                revision: 1,
                firing_key: "firing".into(),
            },
            lifecycle: LifecycleTarget::BoundIncarnation {
                incarnation_id: "inc-1".into(),
                thread_epoch: 2,
                controller_lease_id: "lease".into(),
                controller_lease_epoch: 3,
            },
            terminal: VerifiedTerminal {
                outbox_id: "outbox".into(),
                effect_id: "effect".into(),
                attempt: 1,
                incarnation_id: "inc-1".into(),
                controller_lease_epoch: 3,
                receipt_id: "receipt".into(),
                receipt_revision: 6,
                outcome: VerifiedTerminalOutcome::Completed {
                    tokens: 10,
                    cost_micros: 0,
                },
            },
        }
    }

    #[test]
    fn verified_fixture_checks_proof_identity_and_calculates_bounds() {
        let auth = fixture();
        let p = &auth.proof;
        let request = ProofVerificationRequest {
            proof_record_id: &p.proof_record_id,
            proof_revision: 7,
            identity: SliceProofInput {
                adapter: &p.adapter,
                mode: &p.mode,
                installed_cli_version: "1.0.0",
                binary_hash: &p.binary_hash,
                model: &p.model,
                auth_billing_mode: &p.auth_billing_mode,
                config_digest: &p.config_digest,
            },
        };
        let proof = auth.verify_proof(&request).unwrap();
        assert_eq!(proof.bound.max_tokens, 40);
        assert_eq!(proof.bound.max_cost_micros, 0);
        let mut drift = request;
        drift.identity.model = "other";
        assert_eq!(
            auth.verify_proof(&drift).unwrap_err(),
            AuthorityError::UnverifiedAuthority
        );
        drift = request;
        drift.proof_revision = 8;
        assert_eq!(
            auth.verify_proof(&drift).unwrap_err(),
            AuthorityError::UnverifiedAuthority
        );
    }

    #[test]
    fn verified_fixture_binds_approved_target_fencing_and_freshness() {
        let auth = fixture();
        let target = auth.lifecycle.clone();
        let approved = RoutineTarget::BoundIncarnation {
            incarnation_id: "inc-1".into(),
        };
        let mut req = LifecycleVerificationRequest {
            work: &auth.work,
            approved_target: &approved,
            resolved_target: &target,
            lifecycle_revision: 4,
            now_utc: "2026-01-01T00:00:00Z",
            reading_revision: 5,
        };
        assert_eq!(auth.verify_lifecycle(&req).unwrap().target, target);
        let wrong = RoutineTarget::BoundIncarnation {
            incarnation_id: "inc-2".into(),
        };
        req.approved_target = &wrong;
        assert_eq!(
            auth.verify_lifecycle(&req).unwrap_err(),
            AuthorityError::UnverifiedAuthority
        );
        req.approved_target = &approved;
        req.reading_revision = 6;
        assert_eq!(
            auth.verify_lifecycle(&req).unwrap_err(),
            AuthorityError::UnverifiedAuthority
        );
        req.reading_revision = 5;
        req.now_utc = "2026-01-02T00:00:00Z";
        assert_eq!(
            auth.verify_lifecycle(&req).unwrap_err(),
            AuthorityError::UnverifiedAuthority
        );
        let wrong_lease = LifecycleTarget::BoundIncarnation {
            incarnation_id: "inc-1".into(),
            thread_epoch: 2,
            controller_lease_id: "lease".into(),
            controller_lease_epoch: 4,
        };
        req.resolved_target = &wrong_lease;
        req.now_utc = "2026-01-01T00:00:00Z";
        assert_eq!(
            auth.verify_lifecycle(&req).unwrap_err(),
            AuthorityError::UnverifiedAuthority
        );
    }

    #[test]
    fn verified_fixture_binds_terminal_receipt_to_exact_effect_and_attempt() {
        let auth = fixture();
        let mut req = TerminalVerificationRequest {
            outbox_id: "outbox",
            effect_id: "effect",
            attempt: 1,
            incarnation_id: "inc-1",
            controller_lease_epoch: 3,
            receipt_id: "receipt",
        };
        assert_eq!(auth.verify_terminal(&req).unwrap().receipt_revision, 6);
        req.attempt = 2;
        assert_eq!(
            auth.verify_terminal(&req).unwrap_err(),
            AuthorityError::UnverifiedAuthority
        );
        req.attempt = 1;
        req.outbox_id = "other";
        assert_eq!(
            auth.verify_terminal(&req).unwrap_err(),
            AuthorityError::UnverifiedAuthority
        );
        req.outbox_id = "outbox";
        req.receipt_id = "forged";
        assert_eq!(
            auth.verify_terminal(&req).unwrap_err(),
            AuthorityError::UnverifiedAuthority
        );
    }

    #[test]
    fn unavailable_authority_refuses_every_verification() {
        let unavailable = UnavailableAuthority;
        let proof = ProofVerificationRequest {
            proof_record_id: "reviewed-1",
            proof_revision: 1,
            identity: crate::slice_proof::SliceProofInput {
                adapter: "example",
                mode: "stdio",
                installed_cli_version: "1.0.0",
                binary_hash: "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                model: "model",
                auth_billing_mode: "subscription",
                config_digest: "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            },
        };
        let approved = crate::routine::RoutineTarget::BoundIncarnation {
            incarnation_id: "inc-1".into(),
        };
        let target = LifecycleTarget::BoundIncarnation {
            incarnation_id: "inc-1".into(),
            thread_epoch: 2,
            controller_lease_id: "lease".into(),
            controller_lease_epoch: 3,
        };
        let work = LogicalWorkIdentity::Routine {
            routine_id: "routine".into(),
            revision: 1,
            firing_key: "firing".into(),
        };
        let lifecycle = LifecycleVerificationRequest {
            work: &work,
            approved_target: &approved,
            resolved_target: &target,
            lifecycle_revision: 4,
            now_utc: "2026-01-01T00:00:00Z",
            reading_revision: 5,
        };
        let terminal = TerminalVerificationRequest {
            outbox_id: "outbox",
            effect_id: "effect",
            attempt: 1,
            incarnation_id: "inc-1",
            controller_lease_epoch: 3,
            receipt_id: "receipt",
        };
        assert_eq!(
            unavailable.verify_proof(&proof).unwrap_err(),
            AuthorityError::UnverifiedAuthority
        );
        assert_eq!(
            unavailable.verify_lifecycle(&lifecycle).unwrap_err(),
            AuthorityError::UnverifiedAuthority
        );
        assert_eq!(
            unavailable.verify_terminal(&terminal).unwrap_err(),
            AuthorityError::UnverifiedAuthority
        );
    }
    #[test]
    fn fresh_session_verification_rejects_cross_work_reuse() {
        let mut auth = fixture();
        let routine = LogicalWorkIdentity::Routine {
            routine_id: "routine-a".into(),
            revision: 3,
            firing_key: "firing-a".into(),
        };
        let other_firing = LogicalWorkIdentity::Routine {
            routine_id: "routine-a".into(),
            revision: 3,
            firing_key: "firing-b".into(),
        };
        let intent = LogicalWorkIdentity::Intent {
            intent_id: "intent-a".into(),
            revision: 2,
            member_slot: 1,
        };
        let other_slot = LogicalWorkIdentity::Intent {
            intent_id: "intent-a".into(),
            revision: 2,
            member_slot: 2,
        };
        let approved = RoutineTarget::FreshSession {
            template_id: "template".into(),
        };
        auth.lifecycle = LifecycleTarget::FreshSession {
            template_id: "template".into(),
            scoped_session_id: "scoped-a".into(),
            host_spawn_token: "token-a".into(),
        };
        auth.work = routine.clone();
        let mut req = LifecycleVerificationRequest {
            work: &routine,
            approved_target: &approved,
            resolved_target: &auth.lifecycle,
            lifecycle_revision: 4,
            now_utc: "2026-01-01T00:00:00Z",
            reading_revision: 5,
        };
        assert_eq!(auth.verify_lifecycle(&req).unwrap().work, routine);
        req.work = &other_firing;
        assert_eq!(
            auth.verify_lifecycle(&req),
            Err(AuthorityError::UnverifiedAuthority)
        );
        let other_revision = LogicalWorkIdentity::Routine {
            routine_id: "routine-a".into(),
            revision: 4,
            firing_key: "firing-a".into(),
        };
        req.work = &other_revision;
        assert_eq!(
            auth.verify_lifecycle(&req),
            Err(AuthorityError::UnverifiedAuthority)
        );
        req.work = &routine;
        let bad_token = LifecycleTarget::FreshSession {
            template_id: "template".into(),
            scoped_session_id: "scoped-a".into(),
            host_spawn_token: "other-token".into(),
        };
        req.resolved_target = &bad_token;
        assert_eq!(
            auth.verify_lifecycle(&req),
            Err(AuthorityError::UnverifiedAuthority)
        );
        req.resolved_target = &auth.lifecycle;
        let bad_template = RoutineTarget::FreshSession {
            template_id: "other-template".into(),
        };
        req.approved_target = &bad_template;
        assert_eq!(
            auth.verify_lifecycle(&req),
            Err(AuthorityError::UnverifiedAuthority)
        );
        req.approved_target = &approved;
        auth.work = intent.clone();
        req.work = &intent;
        assert_eq!(auth.verify_lifecycle(&req).unwrap().work, intent);
        req.work = &other_slot;
        assert_eq!(
            auth.verify_lifecycle(&req),
            Err(AuthorityError::UnverifiedAuthority)
        );
    }
}
