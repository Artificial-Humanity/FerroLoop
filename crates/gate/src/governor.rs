//! Gate-level governor: admission + settlement around connector dispatch (Record #97).
//!
//! A `GateGovernor` with no store refuses everything with
//! `admission_unavailable` — governed dispatch never falls back to
//! ungoverned execution.
//!
//! Track A broker composition is not yet landed: there is no broker
//! admit step in this code. A pre-byte refusal (for example a tool or
//! argument build error, classified `RefusedBeforeDispatch`) settling
//! as `NeverDispatched` — releasing budget and concurrency in full —
//! is today's equivalent of a broker refusal. No broker is fabricated
//! here.
//!
//! Deadline: a permit carries `deadline_mono_ms`; dispatch is refused
//! (settled `NeverDispatched`) when `now_ms` is past that deadline.

use std::sync::Arc;

use fl_core::governor::{
    AdmissionOutcome, AdmissionRequest, EndpointAlias, GOVERNANCE_CALLS, GovernorPolicy,
    GovernorStore, Permit, Principal, RecordedOutcome, ReplayAttempt, ReplayDecision,
    ReplayRecordAction, ReplayRequest, SettleRequest, SettledOutcome, SideEffectClass,
    resolve_canonical_endpoint,
};
use serde_json::Value;

use crate::GateError;
use crate::adapter::SecretProviderAdapter;
use crate::connector::{ConnectorManifest, ToolOutcome};
use crate::proxy::DispatchPhaseError;
use crate::proxy::{ProxyResponse, parse_url_scheme_host_port};
use crate::vault::Vault;

pub const ADMISSION_UNAVAILABLE: &str = "admission_unavailable";

#[derive(Debug)]
pub enum GovernedOutcome {
    Completed {
        response: ProxyResponse,
        outcome: ToolOutcome,
        permit: Permit,
    },
    Refused(AdmissionOutcome),
}

/// Classification a dispatch closure reports so settlement is testable without HTTP.
pub enum DispatchResult<T> {
    /// Executed; `actual_units` is the charged cost (response bytes).
    Executed(T, u64),
    /// Refused before any byte was sent (broker-style: no such tool / bad args).
    RefusedBeforeDispatch(GateError),
    /// Failed after a send was attempted; outcome unknown.
    FailedAfterSend(GateError),
}

pub struct GateGovernor {
    pub store: Option<Arc<dyn GovernorStore>>,
    pub policy: GovernorPolicy,
    pub aliases: Vec<EndpointAlias>,
}

impl GateGovernor {
    pub fn new(store: Arc<dyn GovernorStore>) -> Self {
        Self {
            store: Some(store),
            policy: GovernorPolicy::default(),
            aliases: Vec::new(),
        }
    }
    pub fn none() -> Self {
        Self {
            store: None,
            policy: GovernorPolicy::default(),
            aliases: Vec::new(),
        }
    }
    pub fn with_aliases(mut self, aliases: Vec<EndpointAlias>) -> Self {
        self.aliases = aliases;
        self
    }

    pub fn admit(&self, req: &AdmissionRequest, now_ms: u64) -> AdmissionOutcome {
        let Some(store) = &self.store else {
            return AdmissionOutcome::AdmissionUnavailable;
        };
        match store.reserve(req, now_ms) {
            Ok(outcome) => outcome,
            Err(_) => AdmissionOutcome::AdmissionUnavailable,
        }
    }

    fn replay(
        &self,
        attempt: &ReplayAttempt,
        now_ms: u64,
        record: Option<ReplayRecordAction>,
    ) -> Result<ReplayDecision, fl_core::governor::GovernorError> {
        let Some(store) = &self.store else {
            return Err(fl_core::governor::GovernorError::Unavailable);
        };
        store.record_and_check_replay(&ReplayRequest {
            attempt: attempt.clone(),
            now_ms,
            record,
        })
    }

    fn settle(&self, permit: &Permit, actual: u64, outcome: SettledOutcome, complete: bool) {
        if let Some(store) = &self.store {
            let _ = store.settle(&SettleRequest {
                permit_id: permit.id.clone(),
                actual_units: actual,
                outcome,
                complete,
            });
        }
    }

    /// Admit, run `dispatch` once if admitted, then settle by its classification.
    /// `Err` from dispatch classifications is returned after settlement.
    pub fn governed_dispatch<T>(
        &self,
        req: &AdmissionRequest,
        now_ms: u64,
        dispatch: impl FnOnce(&Permit) -> DispatchResult<T>,
    ) -> Result<(T, Permit), GovernedOrGateError> {
        // Replay guard (only for requests carrying a logical-operation
        // digest): check first, then begin the dispatch journal before
        // reserving. A refused replay never dispatches.
        let attempt = req.replay_attempt();
        if let Some(att) = &attempt {
            match self.replay(att, now_ms, None) {
                Ok(ReplayDecision::Refused { reason }) => {
                    return Err(GovernedOrGateError::Refused(AdmissionOutcome::Denied {
                        reason,
                    }));
                }
                Ok(_) => {}
                Err(_) => {
                    return Err(GovernedOrGateError::Refused(
                        AdmissionOutcome::AdmissionUnavailable,
                    ));
                }
            }
            if self
                .replay(att, now_ms, Some(ReplayRecordAction::Dispatching))
                .is_err()
            {
                return Err(GovernedOrGateError::Refused(
                    AdmissionOutcome::AdmissionUnavailable,
                ));
            }
        }
        let permit = match self.admit(req, now_ms) {
            AdmissionOutcome::Admitted(p) => p,
            other => {
                if let Some(att) = &attempt {
                    let _ = self.replay(att, now_ms, Some(ReplayRecordAction::ClearDispatching));
                }
                return Err(GovernedOrGateError::Refused(other));
            }
        };
        if now_ms >= permit.deadline_mono_ms {
            self.settle(&permit, 0, SettledOutcome::NeverDispatched, true);
            if let Some(att) = &attempt {
                let _ = self.replay(att, now_ms, Some(ReplayRecordAction::ClearDispatching));
            }
            return Err(GovernedOrGateError::Refused(AdmissionOutcome::Denied {
                reason: "permit_deadline_exceeded".into(),
            }));
        }
        match dispatch(&permit) {
            DispatchResult::Executed(value, actual) => {
                if let Some(att) = &attempt {
                    let _ = self.replay(
                        att,
                        now_ms,
                        Some(ReplayRecordAction::Outcome(RecordedOutcome::Executed)),
                    );
                }
                self.settle(&permit, actual, SettledOutcome::Executed, true);
                Ok((value, permit))
            }
            DispatchResult::RefusedBeforeDispatch(e) => {
                // Pre-byte refusal: no dispatch happened, so the journal
                // entry is cleared and settlement is NeverDispatched.
                if let Some(att) = &attempt {
                    let _ = self.replay(att, now_ms, Some(ReplayRecordAction::ClearDispatching));
                }
                self.settle(&permit, 0, SettledOutcome::NeverDispatched, true);
                Err(GovernedOrGateError::Gate(e))
            }
            DispatchResult::FailedAfterSend(e) => {
                if let Some(att) = &attempt {
                    let _ = self.replay(
                        att,
                        now_ms,
                        Some(ReplayRecordAction::Outcome(
                            RecordedOutcome::UnknownAfterSend,
                        )),
                    );
                }
                // Stays counted: charge the reservation, do not complete.
                self.settle(
                    &permit,
                    permit.reserved_units,
                    SettledOutcome::UnknownAfterSend,
                    false,
                );
                Err(GovernedOrGateError::Gate(e))
            }
        }
    }

    /// Governed connector invoke: admit (reserve worst-case units =
    /// contract `max_response_bytes`, else 1), dispatch, settle.
    #[allow(clippy::too_many_arguments)]
    pub fn governed_invoke(
        &self,
        manifest: &ConnectorManifest,
        vault: &Vault,
        tool: &str,
        args: &Value,
        caller: Option<String>,
        principal: Principal,
        project: &str,
        now_ms: u64,
        quota_observed_at: Option<u64>,
    ) -> Result<GovernedOutcome, GateError> {
        self.governed_invoke_with_provider_and_clock(
            manifest,
            vault,
            tool,
            args,
            caller,
            principal,
            project,
            now_ms,
            quota_observed_at,
            None,
            &crate::vault::now_unix_millis,
        )
    }

    /// [`Self::governed_invoke`] with an optional secret provider, threaded
    /// into request preparation so provider-backed secrets can resolve.
    #[allow(clippy::too_many_arguments)]
    pub fn governed_invoke_with_provider(
        &self,
        manifest: &ConnectorManifest,
        vault: &Vault,
        tool: &str,
        args: &Value,
        caller: Option<String>,
        principal: Principal,
        project: &str,
        now_ms: u64,
        quota_observed_at: Option<u64>,
        secret_provider: Option<&dyn SecretProviderAdapter>,
    ) -> Result<GovernedOutcome, GateError> {
        self.governed_invoke_with_provider_and_clock(
            manifest,
            vault,
            tool,
            args,
            caller,
            principal,
            project,
            now_ms,
            quota_observed_at,
            secret_provider,
            &crate::vault::now_unix_millis,
        )
    }

    /// [`Self::governed_invoke_with_provider`] with an injectable fresh-now
    /// clock, re-read after preparation (see proxy deadline helper).
    #[allow(clippy::too_many_arguments)]
    pub fn governed_invoke_with_provider_and_clock(
        &self,
        manifest: &ConnectorManifest,
        vault: &Vault,
        tool: &str,
        args: &Value,
        caller: Option<String>,
        principal: Principal,
        project: &str,
        now_ms: u64,
        quota_observed_at: Option<u64>,
        secret_provider: Option<&dyn SecretProviderAdapter>,
        now_fn: &dyn Fn() -> u64,
    ) -> Result<GovernedOutcome, GateError> {
        let tool_def = manifest
            .tools
            .iter()
            .find(|t| t.name == tool)
            .ok_or_else(|| GateError::NoSuchTool {
                tool: tool.to_string(),
            })?;
        let reserved = tool_def
            .response_contract
            .as_ref()
            .map(|c| c.max_response_bytes)
            .unwrap_or(1);
        let (host, path) = match parse_url_scheme_host_port(&manifest.base_url) {
            Ok((_, h, _)) => (h, tool_def.path.clone()),
            Err(_) => (String::new(), tool_def.path.clone()),
        };
        let endpoint =
            resolve_canonical_endpoint(&self.aliases, Some(&manifest.id), Some(tool), &host, &path);
        let mut req = AdmissionRequest::new(principal, project, &endpoint, reserved);
        req.caller_agent = caller.clone().unwrap_or_default();
        req.quota_observed_at_ms = quota_observed_at;
        req.call_name = Some(tool.to_string());
        // Governance lane is reachable only for named governance calls.
        if GOVERNANCE_CALLS.contains(&tool) {
            req.governance_call = Some(tool.to_string());
        }
        req.original_key = args
            .get("idempotency_key")
            .or_else(|| args.get("original_key"))
            .and_then(Value::as_str)
            .map(str::to_string);
        if req.original_key.is_some() {
            req.side_effect_class = SideEffectClass::IdempotentWithKey;
        }
        // Replay identity is explicit: only a caller-supplied operation id
        // identifies a logical operation. Identical tool arguments alone
        // are legitimate repeated calls (for example repeated bridge
        // events), so no digest is derived from them.
        if let Some(op) = args
            .get("operation_id")
            .or_else(|| args.get("operation_digest"))
            .and_then(Value::as_str)
        {
            let digest_input = format!("{}|{}|{}|{}", manifest.id, tool, endpoint, op);
            req.operation_digest = Some(sha256_hex(digest_input.as_bytes()));
        }
        match self.governed_dispatch(&req, now_ms, |permit| {
            // No stale timeout from `now_ms`: the shared phased helper
            // re-checks `permit.deadline` against a fresh `now_fn()` after
            // prepare and derives the send timeout from that remainder.
            match manifest.execute_tool_outcome_phased_with_deadline(
                vault,
                tool,
                args,
                caller,
                None,
                secret_provider,
                Some(permit.deadline_mono_ms),
                now_fn,
            ) {
                Ok((resp, outcome)) => {
                    let actual = match &outcome {
                        ToolOutcome::ContractFailedAfterExecution { body_bytes, .. } => *body_bytes,
                        _ => resp.body.len() as u64,
                    };
                    DispatchResult::Executed((resp, outcome), actual)
                }
                // Phase, not variant: preparation errors settle
                // NeverDispatched; only post-send errors settle
                // UnknownAfterSend.
                Err(DispatchPhaseError::BeforeSend(e)) => DispatchResult::RefusedBeforeDispatch(e),
                Err(DispatchPhaseError::AfterSend(e)) => DispatchResult::FailedAfterSend(e),
            }
        }) {
            Ok(((response, outcome), permit)) => Ok(GovernedOutcome::Completed {
                response,
                outcome,
                permit,
            }),
            Err(GovernedOrGateError::Refused(outcome)) => Ok(GovernedOutcome::Refused(outcome)),
            Err(GovernedOrGateError::Gate(e)) => Err(e),
        }
    }
}

/// Either a governor refusal (no dispatch happened) or a gate error after settlement.
#[derive(Debug)]
pub enum GovernedOrGateError {
    Refused(AdmissionOutcome),
    Gate(GateError),
}

/// Stable refusal text for an [`AdmissionOutcome`]; the unavailable case is
/// exactly `admission_unavailable`.
pub fn admission_refusal_text(outcome: &AdmissionOutcome) -> String {
    match outcome {
        AdmissionOutcome::AdmissionUnavailable => ADMISSION_UNAVAILABLE.to_string(),
        AdmissionOutcome::Deferred {
            reason,
            retry_after_ms,
        } => format!("deferred: {reason} (retry_after_ms {retry_after_ms})"),
        AdmissionOutcome::Denied { reason } => format!("denied: {reason}"),
        AdmissionOutcome::Admitted(p) => format!("admitted: {}", p.id),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, bytes);
    let mut out = String::with_capacity(digest.as_ref().len() * 2);
    for b in digest.as_ref() {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::governor::MemGovernorStore;

    fn req(units: u64) -> AdmissionRequest {
        AdmissionRequest::new(Principal::Owner, "proj", "h/a", units)
    }

    #[test]
    fn governor_unavailable_refuses_all_surfaces() {
        let gov = GateGovernor::none();
        // admit refuses
        assert_eq!(
            gov.admit(&req(1), 1_000),
            AdmissionOutcome::AdmissionUnavailable
        );
        assert_eq!(
            admission_refusal_text(&AdmissionOutcome::AdmissionUnavailable),
            "admission_unavailable"
        );
        // governed_dispatch refuses without calling dispatch (unreachable URL never hit)
        let called = std::cell::Cell::new(false);
        let out = gov.governed_dispatch(&req(1), 1_000, |_| {
            called.set(true);
            DispatchResult::Executed((), 1)
        });
        assert!(!called.get(), "dispatch must not run when unavailable");
        assert!(matches!(
            out,
            Err(GovernedOrGateError::Refused(
                AdmissionOutcome::AdmissionUnavailable
            ))
        ));
    }

    #[test]
    fn replay_refusal_through_governed_dispatch() {
        let store = Arc::new(MemGovernorStore::new(GovernorPolicy::default()));
        let gov = GateGovernor {
            store: Some(store),
            policy: GovernorPolicy::default(),
            aliases: vec![],
        };
        let mut r = req(5);
        r.operation_digest = Some("op-1".into());
        r.original_key = Some("k1".into());
        gov.governed_dispatch(&r, 1_000, |_| DispatchResult::Executed((), 1))
            .unwrap();
        // Second identical dispatch is refused without dispatching.
        let called = std::cell::Cell::new(false);
        let out = gov.governed_dispatch(&r, 2_000, |_| {
            called.set(true);
            DispatchResult::Executed((), 1)
        });
        assert!(!called.get());
        assert!(matches!(
            out,
            Err(GovernedOrGateError::Refused(
                AdmissionOutcome::Denied { .. }
            ))
        ));
        // A new idempotency key does not help.
        let mut r2 = r.clone();
        r2.original_key = Some("k2".into());
        assert!(matches!(
            gov.governed_dispatch(&r2, 2_000, |_| DispatchResult::Executed((), 1)),
            Err(GovernedOrGateError::Refused(
                AdmissionOutcome::Denied { .. }
            ))
        ));
        // Owner without fresh flag, same principal/record, is refused too:
        // (r is already Owner) — covered above. Fresh Owner is allowed.
        let mut fresh = r.clone();
        fresh.owner_fresh_action = true;
        gov.governed_dispatch(&fresh, 3_000, |_| DispatchResult::Executed((), 1))
            .expect("owner fresh action must be allowed");
    }

    #[test]
    fn broker_refusal_releases_budget_fully() {
        // Budget 10 on all scopes; reserve 10, broker-refuse, then 10 again must succeed.
        let policy = GovernorPolicy {
            budget_units_per_scope: 10,
            concurrency_per_scope: 10,
            ..Default::default()
        };
        let store = Arc::new(MemGovernorStore::new(policy));
        let gov = GateGovernor {
            store: Some(store.clone()),
            policy: GovernorPolicy::default(),
            aliases: vec![],
        };
        let err = gov.governed_dispatch(&req(10), 1_000, |_| {
            DispatchResult::<()>::RefusedBeforeDispatch(GateError::InvalidToolArgs {
                tool: "t".into(),
                reason: "bad".into(),
            })
        });
        assert!(matches!(err, Err(GovernedOrGateError::Gate(_))));
        // Budget fully restored: second admit of the full 10 succeeds via dispatch path.
        let ((), permit) = gov
            .governed_dispatch(&req(10), 2_000, |_| DispatchResult::Executed((), 10))
            .expect("second admit must succeed after release");
        assert_eq!(permit.reserved_units, 10);
        assert!(store.unconfirmed_permits().is_empty());
    }

    #[test]
    fn tiny_deadline_window_refuses_before_dispatch() {
        // Policy with a zero deadline window: the permit is already
        // expired at admission time, so dispatch is never called.
        let policy = GovernorPolicy {
            deadline_window_ms: 0,
            ..Default::default()
        };
        let store = Arc::new(MemGovernorStore::new(policy));
        let gov = GateGovernor {
            store: Some(store.clone()),
            policy: GovernorPolicy::default(),
            aliases: vec![],
        };
        let called = std::cell::Cell::new(false);
        let out = gov.governed_dispatch(&req(1), 1_000, |_| {
            called.set(true);
            DispatchResult::Executed((), 1)
        });
        assert!(!called.get(), "expired permit must not dispatch");
        assert!(matches!(
            out,
            Err(GovernedOrGateError::Refused(
                AdmissionOutcome::Denied { .. }
            ))
        ));
        assert!(store.unconfirmed_permits().is_empty());
    }

    #[test]
    fn ssrf_refusal_restores_budget_fully() {
        use crate::connector::ConnectorToolDef;
        use std::collections::BTreeMap;
        let tmp = tempfile::TempDir::new().unwrap();
        let key = crate::load_or_init_master_key(&tmp.path().join("k")).unwrap();
        let vault = Vault::open(tmp.path().join("v.enc"), key).unwrap();
        // Loopback base URL with no secret granting private-network
        // access: prepare refuses with SsrfBlocked before any byte.
        let manifest = ConnectorManifest {
            id: "demo".into(),
            title: "Demo".into(),
            version: "0.1.0".into(),
            description: String::new(),
            base_url: "http://127.0.0.1:1/".into(),
            allowed_hosts: vec![],
            default_headers: BTreeMap::new(),
            enabled: true,
            tools: vec![ConnectorToolDef {
                name: "get_thing".into(),
                description: "Get".into(),
                method: "GET".into(),
                path: "/things/{id}".into(),
                path_params: vec!["id".into()],
                query_params: vec![],
                body_params: vec![],
                headers: BTreeMap::new(),
                response_fields: vec![],
                input_schema: serde_json::json!({"type":"object"}),
                response_contract: None,
            }],
            bridges: vec![],
        };
        let policy = GovernorPolicy {
            budget_units_per_scope: 1,
            ..Default::default()
        };
        let store = Arc::new(MemGovernorStore::new(policy));
        let gov = GateGovernor {
            store: Some(store.clone()),
            policy: GovernorPolicy::default(),
            aliases: vec![],
        };
        let err = gov
            .governed_invoke(
                &manifest,
                &vault,
                "get_thing",
                &serde_json::json!({"id": "x"}),
                None,
                Principal::Owner,
                "proj",
                1_000,
                Some(1_000),
            )
            .unwrap_err();
        assert!(
            matches!(
                err,
                GateError::SsrfBlocked { .. } | GateError::PolicyDenied { .. }
            ),
            "{err}"
        );
        // NeverDispatched settlement: budget and concurrency fully restored.
        assert!(store.unconfirmed_permits().is_empty());
        assert!(matches!(
            gov.admit(&req(1), 2_000),
            AdmissionOutcome::Admitted(_)
        ));
    }

    #[test]
    fn secret_preparation_failure_restores_budget_fully() {
        use crate::connector::ConnectorToolDef;
        use std::collections::BTreeMap;
        let tmp = tempfile::TempDir::new().unwrap();
        let key = crate::load_or_init_master_key(&tmp.path().join("k")).unwrap();
        let vault = Vault::open(tmp.path().join("v.enc"), key).unwrap();
        // Invalid secret name in a manifest header: prepare fails with
        // InvalidSecret before any byte is sent.
        let manifest = ConnectorManifest {
            id: "demo".into(),
            title: "Demo".into(),
            version: "0.1.0".into(),
            description: String::new(),
            base_url: "http://127.0.0.1:1/".into(),
            allowed_hosts: vec![],
            default_headers: BTreeMap::from([(
                "Authorization".into(),
                "Bearer {{FERROGATE:bad-name}}".into(),
            )]),
            enabled: true,
            tools: vec![ConnectorToolDef {
                name: "get_thing".into(),
                description: "Get".into(),
                method: "GET".into(),
                path: "/thing".into(),
                path_params: vec![],
                query_params: vec![],
                body_params: vec![],
                headers: BTreeMap::new(),
                response_fields: vec![],
                input_schema: serde_json::json!({"type":"object"}),
                response_contract: None,
            }],
            bridges: vec![],
        };
        let policy = GovernorPolicy {
            budget_units_per_scope: 1,
            ..Default::default()
        };
        let store = Arc::new(MemGovernorStore::new(policy));
        let gov = GateGovernor {
            store: Some(store.clone()),
            policy: GovernorPolicy::default(),
            aliases: vec![],
        };
        let err = gov
            .governed_invoke(
                &manifest,
                &vault,
                "get_thing",
                &serde_json::json!({}),
                None,
                Principal::Owner,
                "proj",
                1_000,
                Some(1_000),
            )
            .unwrap_err();
        assert!(matches!(err, GateError::InvalidSecret { .. }), "{err}");
        assert!(store.unconfirmed_permits().is_empty());
        assert!(matches!(
            gov.admit(&req(1), 2_000),
            AdmissionOutcome::Admitted(_)
        ));
    }

    #[test]
    fn governed_invoke_bad_args_releases_via_never_dispatched() {
        use crate::connector::ConnectorToolDef;
        use std::collections::BTreeMap;
        let tmp = tempfile::TempDir::new().unwrap();
        let key = crate::load_or_init_master_key(&tmp.path().join("k")).unwrap();
        let vault = Vault::open(tmp.path().join("v.enc"), key).unwrap();
        let manifest = ConnectorManifest {
            id: "demo".into(),
            title: "Demo".into(),
            version: "0.1.0".into(),
            description: String::new(),
            base_url: "http://127.0.0.1:1/".into(),
            allowed_hosts: vec![],
            default_headers: BTreeMap::new(),
            enabled: true,
            tools: vec![ConnectorToolDef {
                name: "get_thing".into(),
                description: "Get".into(),
                method: "GET".into(),
                path: "/things/{id}".into(),
                path_params: vec!["id".into()],
                query_params: vec![],
                body_params: vec![],
                headers: BTreeMap::new(),
                response_fields: vec![],
                input_schema: serde_json::json!({"type":"object"}),
                response_contract: None,
            }],
            bridges: vec![],
        };
        let policy = GovernorPolicy {
            budget_units_per_scope: 1,
            ..Default::default()
        };
        let store = Arc::new(MemGovernorStore::new(policy));
        let gov = GateGovernor {
            store: Some(store.clone()),
            policy: GovernorPolicy::default(),
            aliases: vec![],
        };
        // Missing `id` => InvalidToolArgs before any byte => NeverDispatched release.
        let err = gov
            .governed_invoke(
                &manifest,
                &vault,
                "get_thing",
                &serde_json::json!({}),
                None,
                Principal::Owner,
                "proj",
                1_000,
                Some(1_000),
            )
            .unwrap_err();
        assert!(matches!(err, GateError::InvalidToolArgs { .. }), "{err}");
        assert!(store.unconfirmed_permits().is_empty());
        // Budget (cap 1, reserved 1) fully restored: admit succeeds again.
        assert!(matches!(
            gov.admit(&req(1), 2_000),
            AdmissionOutcome::Admitted(_)
        ));
    }

    #[test]
    fn prepare_crossing_deadline_yields_no_dispatch_full_release() {
        use crate::connector::ConnectorToolDef;
        use std::collections::BTreeMap;
        let tmp = tempfile::TempDir::new().unwrap();
        let key = crate::load_or_init_master_key(&tmp.path().join("k")).unwrap();
        let vault = Vault::open(tmp.path().join("v.enc"), key).unwrap();
        let manifest = ConnectorManifest {
            id: "demo".into(),
            title: "Demo".into(),
            version: "0.1.0".into(),
            description: String::new(),
            base_url: "http://example.com/".into(),
            allowed_hosts: vec![],
            default_headers: BTreeMap::new(),
            enabled: true,
            tools: vec![ConnectorToolDef {
                name: "get_thing".into(),
                description: "Get".into(),
                method: "GET".into(),
                path: "/thing".into(),
                path_params: vec![],
                query_params: vec![],
                body_params: vec![],
                headers: BTreeMap::new(),
                response_fields: vec![],
                input_schema: serde_json::json!({"type":"object"}),
                response_contract: None,
            }],
            bridges: vec![],
        };
        // Deadline 1500 ms after admission at 1000; injected fresh now
        // (read only after prepare) is past it, simulating slow prepare.
        let policy = GovernorPolicy {
            budget_units_per_scope: 1,
            deadline_window_ms: 1_500,
            ..Default::default()
        };
        let store = Arc::new(MemGovernorStore::new(policy));
        let gov = GateGovernor {
            store: Some(store.clone()),
            policy: GovernorPolicy::default(),
            aliases: vec![],
        };
        let err = gov
            .governed_invoke_with_provider_and_clock(
                &manifest,
                &vault,
                "get_thing",
                &serde_json::json!({}),
                None,
                Principal::Owner,
                "proj",
                1_000,
                Some(1_000),
                None,
                &|| 10_000,
            )
            .unwrap_err();
        assert!(
            matches!(err, GateError::InvalidUrl { ref reason, .. } if reason == "permit_deadline_exceeded"),
            "{err}"
        );
        // BeforeSend => NeverDispatched: full budget/concurrency release.
        assert!(store.unconfirmed_permits().is_empty());
        assert!(matches!(
            gov.admit(&req(1), 2_000),
            AdmissionOutcome::Admitted(_)
        ));
    }

    #[test]
    fn executed_settles_actual_and_unknown_stays_counted() {
        let store = Arc::new(MemGovernorStore::new(GovernorPolicy::default()));
        let gov = GateGovernor {
            store: Some(store.clone()),
            policy: GovernorPolicy::default(),
            aliases: vec![],
        };
        let ((), _) = gov
            .governed_dispatch(&req(5), 1_000, |_| DispatchResult::Executed((), 3))
            .unwrap();
        assert_eq!(store.budget_used_for("project:proj"), 3);
        // UnknownAfterSend: stays unconfirmed (counted).
        let _ = gov.governed_dispatch(&req(5), 2_000, |_| {
            DispatchResult::<()>::FailedAfterSend(GateError::UpstreamHttp {
                cause: "down".into(),
            })
        });
        assert_eq!(store.unconfirmed_permits().len(), 1);
        assert_eq!(store.budget_used_for("project:proj"), 3 + 5);
    }
}
