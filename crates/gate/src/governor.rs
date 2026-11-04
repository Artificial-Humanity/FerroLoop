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
    ReplayRecordAction, ReplayRequest, SettleRequest, SettledOutcome, resolve_canonical_endpoint,
};
use serde_json::Value;

use crate::GateError;
use crate::adapter::SecretProviderAdapter;
use crate::connector::{ConnectorManifest, ToolOutcome};
use crate::proxy::DispatchPhaseError;
use crate::proxy::{LEGACY_MAX_RESPONSE_BYTES, ProxyResponse, parse_url_scheme_host_port};
use crate::vault::Vault;

pub const ADMISSION_UNAVAILABLE: &str = "admission_unavailable";

/// Owner policy channel for failover approval: environment variable
/// `FL_GATE_APPROVED_POLICY_DIGESTS`, a comma-separated list of
/// Owner-approved computed policy digests. Hub and CLI governor builders
/// read it (via [`approved_policy_digests_from_env`]) into
/// `GateGovernor::approved_policy_digests`. Empty/absent = no failover.
pub const APPROVED_POLICY_DIGESTS_ENV: &str = "FL_GATE_APPROVED_POLICY_DIGESTS";

/// Parse the Owner policy channel value: comma-separated, each entry
/// trimmed, validated as hex (non-empty, ASCII hex digits; normalized to
/// lowercase). Invalid entries are dropped, never trusted.
pub fn approved_policy_digests_from_value(
    value: Option<&str>,
) -> std::collections::HashSet<String> {
    let Some(value) = value else {
        return std::collections::HashSet::new();
    };
    value
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_hexdigit()))
        .map(str::to_ascii_lowercase)
        .collect()
}

/// Read [`APPROVED_POLICY_DIGESTS_ENV`] from the process environment.
pub fn approved_policy_digests_from_env() -> std::collections::HashSet<String> {
    approved_policy_digests_from_value(std::env::var(APPROVED_POLICY_DIGESTS_ENV).ok().as_deref())
}

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

/// Path component of a URL (after the authority, before `?`/`#`); empty if none.
pub fn url_path(url: &str) -> String {
    let Some((_, after)) = url.split_once("://") else {
        return String::new();
    };
    match after.find('/') {
        Some(i) => after[i..]
            .split(['?', '#'])
            .next()
            .unwrap_or("")
            .to_string(),
        None => String::new(),
    }
}

/// Join a base-url path prefix and a tool path into one `/`-rooted path.
pub fn join_url_paths(prefix: &str, path: &str) -> String {
    let p = prefix.trim_end_matches('/');
    if p.is_empty() {
        if path.starts_with('/') {
            path.to_string()
        } else {
            format!("/{path}")
        }
    } else if path.is_empty() {
        p.to_string()
    } else if path.starts_with('/') {
        format!("{p}{path}")
    } else {
        format!("{p}/{path}")
    }
}

/// Contract digest: SHA-256 over canonical (key-sorted) schema JSON.
/// `None` contract digests the JSON `null`.
pub fn contract_digest(contract: Option<&crate::contract::ResponseContract>) -> String {
    let value = match contract {
        Some(c) => serde_json::to_value(&c.schema).unwrap_or(serde_json::Value::Null),
        None => serde_json::Value::Null,
    };
    sha256_hex(fl_core::canonical_json(&value).as_bytes())
}

/// Credential scope digest: SHA-256 over `allowed_hosts` joined by `,`.
pub fn credential_scope_digest(allowed_hosts: &[String]) -> String {
    sha256_hex(allowed_hosts.join(",").as_bytes())
}

/// Drift fingerprint wired into the failover engine before first dispatch.
pub fn policy_fingerprint(
    manifest: &ConnectorManifest,
    tool_def: &crate::connector::ConnectorToolDef,
    set: &fl_core::EndpointSet,
) -> fl_core::PolicyFingerprint {
    fl_core::PolicyFingerprint {
        contract_digest: contract_digest(tool_def.response_contract.as_ref()),
        policy_digest: set.computed_policy_digest(),
        credential_scope: credential_scope_digest(&manifest.allowed_hosts),
    }
}

pub fn policy_fingerprint_for_tool(
    manifest: &ConnectorManifest,
    tool: &str,
    set: &fl_core::EndpointSet,
) -> Option<fl_core::PolicyFingerprint> {
    manifest
        .tools
        .iter()
        .find(|t| t.name == tool)
        .map(|td| policy_fingerprint(manifest, td, set))
}

/// Fold live approval into `policy_digest` as an approval marker, keeping
/// the `PolicyFingerprint` API unchanged: two fingerprints that differ only
/// in approval compare unequal, so `drift_invalidates` fires.
pub fn approval_marker_digest(computed_policy_digest: &str, approved: bool) -> String {
    format!("{computed_policy_digest}#approved={approved}")
}

/// Live approval for a computed digest. When
/// `FL_GATE_APPROVED_POLICY_DIGESTS` is present in the environment it is
/// the live, authoritative revocation channel (re-read on every call; an
/// operator revokes by removing the digest or emptying the variable).
/// When it is absent, the governor's current approved set — built from
/// that variable at construction — decides.
pub fn is_live_approved_with_env(
    computed_policy_digest: &str,
    env_digests: Option<&std::collections::HashSet<String>>,
    governor_approved: &std::collections::HashSet<String>,
) -> bool {
    match env_digests {
        Some(env) => env.contains(computed_policy_digest),
        None => governor_approved.contains(computed_policy_digest),
    }
}

/// [`is_live_approved_with_env`] reading the process environment live.
pub fn is_live_approved(
    computed_policy_digest: &str,
    governor_approved: &std::collections::HashSet<String>,
) -> bool {
    if std::env::var_os(APPROVED_POLICY_DIGESTS_ENV).is_some() {
        let env = approved_policy_digests_from_env();
        is_live_approved_with_env(computed_policy_digest, Some(&env), governor_approved)
    } else {
        is_live_approved_with_env(computed_policy_digest, None, governor_approved)
    }
}

/// Live fingerprint: like [`policy_fingerprint`], but `policy_digest`
/// carries the live approval marker for the set's computed digest.
///
/// Drift semantics: contract/manifest drift is per-operation,
/// admission-time only — the manifest is immutable within one
/// `governed_invoke` call, so its contract/credential digests cannot flip
/// mid-operation. Approval revocation is the live mid-operation drift
/// channel: this is recomputed on every call, so revoking approval makes
/// the next pre-dispatch drift check see `DriftInvalidated`.
pub fn policy_fingerprint_live(
    manifest: &ConnectorManifest,
    tool_def: &crate::connector::ConnectorToolDef,
    set: &fl_core::EndpointSet,
    governor_approved: &std::collections::HashSet<String>,
) -> fl_core::PolicyFingerprint {
    let mut fp = policy_fingerprint(manifest, tool_def, set);
    fp.policy_digest = approval_marker_digest(
        &set.computed_policy_digest(),
        is_live_approved(&set.computed_policy_digest(), governor_approved),
    );
    fp
}

pub fn policy_fingerprint_for_tool_live(
    manifest: &ConnectorManifest,
    tool: &str,
    set: &fl_core::EndpointSet,
    governor_approved: &std::collections::HashSet<String>,
) -> Option<fl_core::PolicyFingerprint> {
    manifest
        .tools
        .iter()
        .find(|t| t.name == tool)
        .map(|td| policy_fingerprint_live(manifest, td, set, governor_approved))
}

/// Discovered connector aliases: one per manifest tool, with
/// `path_prefix` = manifest `base_url` path prefix joined with the tool
/// path, so raw and connector calls canonicalise identically.
pub fn connector_endpoint_aliases(manifests: &[ConnectorManifest]) -> Vec<EndpointAlias> {
    let mut out = Vec::new();
    for m in manifests {
        let Ok((_, host, _)) = parse_url_scheme_host_port(&m.base_url) else {
            continue;
        };
        let base_path = url_path(&m.base_url);
        for t in &m.tools {
            out.push(EndpointAlias {
                connector_id: m.id.clone(),
                tool: t.name.clone(),
                host: host.clone(),
                path_prefix: join_url_paths(&base_path, &t.path),
            });
        }
    }
    out
}

pub struct GateGovernor {
    pub store: Option<Arc<dyn GovernorStore>>,
    pub policy: GovernorPolicy,
    pub aliases: Vec<EndpointAlias>,
    /// Owner-approved computed policy digests (from config; default empty
    /// = no failover authorized, fail-closed).
    pub approved_policy_digests: std::collections::HashSet<String>,
}

impl GateGovernor {
    pub fn new(store: Arc<dyn GovernorStore>) -> Self {
        Self {
            store: Some(store),
            policy: GovernorPolicy::default(),
            aliases: Vec::new(),
            approved_policy_digests: std::collections::HashSet::new(),
        }
    }
    pub fn none() -> Self {
        Self {
            store: None,
            policy: GovernorPolicy::default(),
            aliases: Vec::new(),
            approved_policy_digests: std::collections::HashSet::new(),
        }
    }
    pub fn with_aliases(mut self, aliases: Vec<EndpointAlias>) -> Self {
        self.aliases = aliases;
        self
    }
    pub fn with_approved_policy_digests(
        mut self,
        digests: std::collections::HashSet<String>,
    ) -> Self {
        self.approved_policy_digests = digests;
        self
    }
    /// Failover authorization: only the COMPUTED digest counts; the
    /// manifest-stored digest and `owner_approved` bool are advisory.
    pub fn is_policy_approved(&self, set: &fl_core::EndpointSet) -> bool {
        self.approved_policy_digests
            .contains(&set.computed_policy_digest())
    }
    /// Live approval (re-reads the env channel on every call); see
    /// [`is_live_approved`].
    pub fn is_policy_live_approved(&self, set: &fl_core::EndpointSet) -> bool {
        is_live_approved(&set.computed_policy_digest(), &self.approved_policy_digests)
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

    /// Admit with `requested_units` overridden to `reserved_override`
    /// (one up-front parent reservation sized by a failover envelope).
    pub fn admit_with_reserved(
        &self,
        req: &AdmissionRequest,
        now_ms: u64,
        reserved_override: u64,
    ) -> AdmissionOutcome {
        let mut req = req.clone();
        req.requested_units = reserved_override;
        self.admit(&req, now_ms)
    }

    /// Settle a permit obtained via [`Self::admit_with_reserved`].
    pub fn settle_permit(
        &self,
        permit: &Permit,
        actual: u64,
        outcome: SettledOutcome,
        complete: bool,
    ) {
        self.settle(permit, actual, outcome, complete);
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
    /// contract `max_response_bytes` when contracted, else
    /// `LEGACY_MAX_RESPONSE_BYTES`), dispatch, settle capped at the reserve.
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
            .unwrap_or(LEGACY_MAX_RESPONSE_BYTES);
        // Connector path includes any path prefix in `manifest.base_url`
        // (e.g. `https://host/v1` + tool `/chat` => `/v1/chat`).
        let (host, path) = match parse_url_scheme_host_port(&manifest.base_url) {
            Ok((_, h, _)) => (
                h,
                join_url_paths(&url_path(&manifest.base_url), &tool_def.path),
            ),
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
        // Caller JSON must not self-declare the side-effect class: carrying
        // an idempotency key does not promote this call to
        // `IdempotentWithKey`. `req.side_effect_class` stays the default
        // `SideEffectful` unless trusted (non-args) code sets it.
        // Replay identity is explicit: only a caller-supplied operation id
        // identifies a logical operation. Identical tool arguments alone
        // are legitimate repeated calls (for example repeated bridge
        // events), so no digest is derived from them. The concrete target
        // endpoint is NOT hashed into the digest; the logical endpoint-set
        // id below is what groups attempts, and an endpoint switch within
        // the set is refused by the replay guard.
        if let Some(op) = args
            .get("operation_id")
            .or_else(|| args.get("operation_digest"))
            .and_then(Value::as_str)
        {
            let digest_input = format!("{}|{}|{}", manifest.id, tool, op);
            req.operation_digest = Some(sha256_hex(digest_input.as_bytes()));
            req.endpoint_set_digest =
                sha256_hex(format!("endpoint-set|{}|{}", manifest.id, tool).as_bytes());
        }
        // Failover path: an Owner-approved endpoint set on the tool routes
        // through the failover engine (one parent reservation up front).
        if let Some(set) = tool_def.endpoint_set.clone() {
            if set.connector_id != manifest.id || set.tool != tool {
                return Err(GateError::PolicyDenied {
                    secret: format!("connector:{}", manifest.id),
                    reason: "endpoint_set connector_id/tool does not match manifest/tool".into(),
                });
            }
            if let Err(e) = set.validate() {
                return Err(GateError::PolicyDenied {
                    secret: format!("connector:{}", manifest.id),
                    reason: format!("endpoint_set: {e}"),
                });
            }
            // Failover runs ONLY when the COMPUTED digest is Owner-approved
            // (config-supplied set on this governor). Otherwise fail closed:
            // fall through to the single-attempt SideEffectful path below;
            // the failover engine itself returns a typed
            // `unapproved_policy_digest` refusal when invoked directly.
            if self.is_policy_approved(&set) && self.is_policy_live_approved(&set) {
                // Single clock: admission and envelope share this one
                // `now_fn()` value; the caller's `now_ms` is not used here.
                let admission_now = now_fn();
                let envelope = fl_core::ParentEnvelope::build(
                    u32::try_from(set.endpoints.len()).unwrap_or(u32::MAX),
                    set.endpoints.len(),
                    admission_now.saturating_add(self.policy.deadline_window_ms),
                    reserved,
                )
                .map_err(|e| GateError::PolicyDenied {
                    secret: format!("connector:{}", manifest.id),
                    reason: format!("endpoint_set envelope: {e}"),
                })?;
                let logical_operation_id = req
                    .operation_digest
                    .clone()
                    .unwrap_or_else(|| format!("{}:{}:{}", manifest.id, tool, admission_now));
                let idem_key = req.original_key.clone();
                let fingerprint_at_admission = policy_fingerprint_live(
                    manifest,
                    tool_def,
                    &set,
                    &self.approved_policy_digests,
                );
                let mut freq = req.clone();
                freq.requested_units = envelope.reserved_units;
                freq.side_effect_class = set.side_effect_class;
                let Some(store) = self.store.clone() else {
                    return Ok(GovernedOutcome::Refused(
                        AdmissionOutcome::AdmissionUnavailable,
                    ));
                };
                let mut journal =
                    crate::failover::PersistentFailoverJournal::new(store).map_err(|e| {
                        GateError::PolicyDenied {
                            secret: format!("connector:{}", manifest.id),
                            reason: format!("failover journal: {e}"),
                        }
                    })?;
                if journal.has_uncertain_for(&logical_operation_id) {
                    return Err(GateError::PolicyDenied {
                        secret: format!("connector:{}", manifest.id),
                        reason: format!("uncertain_prior_attempt:{logical_operation_id}"),
                    });
                }
                let engine = crate::failover::FailoverEngine::new(self);
                let caller_for_dispatch = caller.clone();
                let set_for_dispatch = set.clone();
                // Failover runs INSIDE governed_dispatch: replay check
                // before the engine, outcome recorded after it
                // (Executed->Executed, Unknown->UnknownAfterSend,
                // all-NoExecutionProven->ClearDispatching/NeverDispatched).
                // Governor approved set at admission; the env channel is
                // re-read live inside the source on every call (the set is
                // immutable for the duration of one `&self` call).
                let governor_approved_for_fp = self.approved_policy_digests.clone();
                let result = self.governed_dispatch(&freq, admission_now, |_permit| {
                    // Live fingerprint source: re-reads LIVE approval state
                    // on every call (before every dispatch) via
                    // `approved_policy_digests_from_env()` plus the
                    // governor's approved set, folded in as an approval
                    // marker. Contract/manifest digests are admission-time
                    // only (manifest immutable within this call); approval
                    // revocation is the live mid-operation drift channel —
                    // if it flips, the next attempt sees drift and is
                    // never dispatched.
                    let admitted = fingerprint_at_admission.clone();
                    let set_for_fp = set_for_dispatch.clone();
                    let approved_for_fp = governor_approved_for_fp.clone();
                    let fingerprint_source = move || {
                        policy_fingerprint_for_tool_live(
                            manifest,
                            tool,
                            &set_for_fp,
                            &approved_for_fp,
                        )
                        .unwrap_or_else(|| admitted.clone())
                    };
                    match engine.run_dispatch_only(
                        &set_for_dispatch,
                        &envelope,
                        &logical_operation_id,
                        idem_key.as_deref(),
                        Some(&fingerprint_at_admission),
                        &fingerprint_source,
                        now_fn,
                        &mut journal,
                        |endpoint, per_try_ms, key| {
                            manifest.execute_tool_outcome_failover_attempt(
                                vault,
                                tool,
                                args,
                                caller_for_dispatch.clone(),
                                Some((per_try_ms / 1000).max(1)),
                                secret_provider,
                                Some(envelope.deadline_mono_ms),
                                now_fn,
                                endpoint,
                                key,
                            )
                        },
                    ) {
                        crate::failover::DispatchOnlyOutcome::Executed {
                            response: Some((resp, outcome)),
                            bytes,
                            ..
                        } => DispatchResult::Executed((resp, outcome), bytes),
                        crate::failover::DispatchOnlyOutcome::Executed { .. } => {
                            DispatchResult::RefusedBeforeDispatch(GateError::UpstreamHttp {
                                cause: "failover completed without a response".into(),
                            })
                        }
                        crate::failover::DispatchOnlyOutcome::Unknown { .. } => {
                            DispatchResult::FailedAfterSend(GateError::UpstreamHttp {
                                cause: crate::failover::UNKNOWN_AFTER_SEND_TEXT.into(),
                            })
                        }
                        crate::failover::DispatchOnlyOutcome::AllNoExecutionProven {
                            error,
                            reason,
                            ..
                        } => DispatchResult::RefusedBeforeDispatch(error.unwrap_or_else(|| {
                            GateError::PolicyDenied {
                                secret: format!("connector:{}", manifest.id),
                                reason: format!("failover stopped: {reason:?}"),
                            }
                        })),
                    }
                });
                return match result {
                    Ok(((response, outcome), permit)) => Ok(GovernedOutcome::Completed {
                        response,
                        outcome,
                        permit,
                    }),
                    Err(GovernedOrGateError::Refused(outcome)) => {
                        Ok(GovernedOutcome::Refused(outcome))
                    }
                    Err(GovernedOrGateError::Gate(e)) => Err(e),
                };
            }
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
    fn approved_policy_digests_env_parsing() {
        assert!(approved_policy_digests_from_value(None).is_empty());
        assert!(approved_policy_digests_from_value(Some("")).is_empty());
        assert!(approved_policy_digests_from_value(Some(" , ,")).is_empty());
        let got = approved_policy_digests_from_value(Some(" AB12 , cd34,, not-hex!, zz, 00ff "));
        assert_eq!(
            got,
            std::collections::HashSet::from([
                "ab12".to_string(),
                "cd34".to_string(),
                "00ff".to_string()
            ])
        );
    }

    #[test]
    fn live_approval_removal_flips_marker() {
        // Governor-level (env-independent): removing approval from the
        // live env channel, or from the governor set when env is absent,
        // flips the approval marker and thus the fingerprint.
        let governor = std::collections::HashSet::from(["ab12".to_string()]);
        let empty = std::collections::HashSet::new();
        assert!(is_live_approved_with_env("ab12", None, &governor));
        assert!(!is_live_approved_with_env("ab12", Some(&empty), &governor));
        assert!(is_live_approved_with_env("ab12", Some(&governor), &empty));
        assert!(!is_live_approved_with_env("ab12", None, &empty));
        assert_ne!(
            approval_marker_digest("ab12", true),
            approval_marker_digest("ab12", false)
        );
        assert_eq!(approval_marker_digest("ab12", true), "ab12#approved=true");
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
            approved_policy_digests: Default::default(),
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
    fn governed_dispatch_refuses_endpoint_switch_in_same_set() {
        use fl_core::governor::SideEffectClass;
        let store = Arc::new(MemGovernorStore::new(GovernorPolicy::default()));
        let gov = GateGovernor {
            store: Some(store),
            policy: GovernorPolicy::default(),
            aliases: vec![],
            approved_policy_digests: Default::default(),
        };
        let mut r = req(5);
        r.operation_digest = Some("op-1".into());
        r.endpoint_set_digest = "logical-set".into();
        r.original_key = Some("k1".into());
        r.side_effect_class = SideEffectClass::IdempotentWithKey;
        gov.governed_dispatch(&r, 1_000, |_| DispatchResult::Executed((), 1))
            .unwrap();
        // Same digest and logical set, different concrete endpoint: refused
        // without dispatching, even with the original key.
        let mut switched = r.clone();
        switched.endpoint = "h/b".into();
        let called = std::cell::Cell::new(false);
        let out = gov.governed_dispatch(&switched, 2_000, |_| {
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
            approved_policy_digests: Default::default(),
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
            approved_policy_digests: Default::default(),
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
                endpoint_set: None,
            }],
            bridges: vec![],
        };
        let policy = GovernorPolicy {
            budget_units_per_scope: LEGACY_MAX_RESPONSE_BYTES,
            ..Default::default()
        };
        let store = Arc::new(MemGovernorStore::new(policy));
        let gov = GateGovernor {
            store: Some(store.clone()),
            policy: GovernorPolicy::default(),
            aliases: vec![],
            approved_policy_digests: Default::default(),
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
                endpoint_set: None,
            }],
            bridges: vec![],
        };
        let policy = GovernorPolicy {
            budget_units_per_scope: LEGACY_MAX_RESPONSE_BYTES,
            ..Default::default()
        };
        let store = Arc::new(MemGovernorStore::new(policy));
        let gov = GateGovernor {
            store: Some(store.clone()),
            policy: GovernorPolicy::default(),
            aliases: vec![],
            approved_policy_digests: Default::default(),
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
                endpoint_set: None,
            }],
            bridges: vec![],
        };
        let policy = GovernorPolicy {
            budget_units_per_scope: LEGACY_MAX_RESPONSE_BYTES,
            ..Default::default()
        };
        let store = Arc::new(MemGovernorStore::new(policy));
        let gov = GateGovernor {
            store: Some(store.clone()),
            policy: GovernorPolicy::default(),
            aliases: vec![],
            approved_policy_digests: Default::default(),
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
                endpoint_set: None,
            }],
            bridges: vec![],
        };
        // Deadline 1500 ms after admission at 1000; injected fresh now
        // (read only after prepare) is past it, simulating slow prepare.
        let policy = GovernorPolicy {
            budget_units_per_scope: LEGACY_MAX_RESPONSE_BYTES,
            deadline_window_ms: 1_500,
            ..Default::default()
        };
        let store = Arc::new(MemGovernorStore::new(policy));
        let gov = GateGovernor {
            store: Some(store.clone()),
            policy: GovernorPolicy::default(),
            aliases: vec![],
            approved_policy_digests: Default::default(),
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
    fn uncontracted_invoke_reserves_legacy_worst_case() {
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
                path: "/thing".into(),
                path_params: vec![],
                query_params: vec![],
                body_params: vec![],
                headers: BTreeMap::new(),
                response_fields: vec![],
                input_schema: serde_json::json!({"type":"object"}),
                response_contract: None,
                endpoint_set: None,
            }],
            bridges: vec![],
        };
        // One byte under the legacy worst case: an uncontracted invoke
        // reserving only 1 (old behaviour) would be admitted and reach
        // dispatch; reserving LEGACY_MAX_RESPONSE_BYTES is denied here.
        let policy = GovernorPolicy {
            budget_units_per_scope: LEGACY_MAX_RESPONSE_BYTES - 1,
            ..Default::default()
        };
        let store = Arc::new(MemGovernorStore::new(policy));
        let gov = GateGovernor {
            store: Some(store),
            policy: GovernorPolicy::default(),
            aliases: vec![],
            approved_policy_digests: Default::default(),
        };
        let out = gov
            .governed_invoke(
                &manifest,
                &vault,
                "get_thing",
                &serde_json::json!({"idempotency_key": "k1", "operation_id": "op1"}),
                None,
                Principal::Owner,
                "proj",
                1_000,
                Some(1_000),
            )
            .expect("budget denial is a refusal, not a dispatch error");
        assert!(
            matches!(
                out,
                GovernedOutcome::Refused(AdmissionOutcome::Denied { .. })
            ),
            "{out:?}"
        );
    }

    #[test]
    fn governed_invoke_failover_to_second_endpoint() {
        use crate::connector::ConnectorToolDef;
        use fl_core::{EndpointSet, governor::SideEffectClass};
        use std::collections::BTreeMap;
        // Closed first endpoint: bind then drop so the port refuses.
        let closed_port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        let handle = std::thread::spawn(move || {
            // Bounded receive: this thread must never block the test forever.
            let req = server
                .recv_timeout(std::time::Duration::from_secs(10))
                .expect("server receive failed")
                .expect("no request reached second endpoint within 10s");
            let _ = req.respond(tiny_http::Response::from_string(
                serde_json::json!({"id":"ok-1"}).to_string(),
            ));
        });
        let tmp = tempfile::TempDir::new().unwrap();
        let key = crate::load_or_init_master_key(&tmp.path().join("k")).unwrap();
        let mut vault = Vault::open(tmp.path().join("v.enc"), key).unwrap();
        vault
            .set(crate::SetSecretArgs {
                name: "API_TOKEN".into(),
                value: "tok-123456".into(),
                allowed_hosts: vec!["127.0.0.1".into()],
                allowed_methods: vec!["GET".into()],
                allow_private_network: true,
                ..Default::default()
            })
            .unwrap();
        let manifest = ConnectorManifest {
            id: "demo".into(),
            title: "Demo".into(),
            version: "0.1.0".into(),
            description: String::new(),
            base_url: format!("http://127.0.0.1:{port}/"),
            allowed_hosts: vec![],
            default_headers: BTreeMap::from([(
                "Authorization".into(),
                "Bearer {{FERROGATE:API_TOKEN}}".into(),
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
                endpoint_set: Some(EndpointSet {
                    connector_id: "demo".into(),
                    tool: "get_thing".into(),
                    endpoints: vec![
                        format!("http://127.0.0.1:{closed_port}"),
                        format!("http://127.0.0.1:{port}"),
                    ],
                    side_effect_class: SideEffectClass::SideEffectful,
                    idempotency_conformance_test: None,
                    owner_approved: true,
                    policy_digest: "pd1".into(),
                }),
            }],
            bridges: vec![],
        };
        manifest
            .validate(std::path::Path::new("demo.toml"))
            .unwrap();
        let policy = GovernorPolicy {
            budget_units_per_scope: LEGACY_MAX_RESPONSE_BYTES * 4,
            ..Default::default()
        };
        let store = Arc::new(MemGovernorStore::new(policy));
        // Small envelope deadline -> small per-try timeout (5 s).
        let gov = GateGovernor {
            store: Some(store),
            policy: GovernorPolicy {
                deadline_window_ms: 5_000,
                ..Default::default()
            },
            aliases: vec![],
            approved_policy_digests: std::collections::HashSet::from([manifest.tools[0]
                .endpoint_set
                .as_ref()
                .unwrap()
                .computed_policy_digest()]),
        };
        // The failover path admits on the real clock (`now_fn`), so the
        // quota observation must be fresh on that same clock.
        let now = crate::vault::now_unix_millis();
        let out = gov
            .governed_invoke(
                &manifest,
                &vault,
                "get_thing",
                &serde_json::json!({}),
                None,
                Principal::Owner,
                "proj",
                now,
                Some(now),
            )
            .expect("failover invoke must succeed via second endpoint");
        // Assert before joining: a refused invoke must fail fast here, not
        // block on the server thread.
        match out {
            GovernedOutcome::Completed { response, .. } => {
                assert!(response.body.contains("ok-1"), "{}", response.body)
            }
            other => panic!("expected Completed, got {other:?}"),
        }
        handle.join().unwrap();
    }

    fn failover_fixture(
        base_url: String,
        endpoints: Vec<String>,
        class: fl_core::governor::SideEffectClass,
        approved: bool,
    ) -> (GateGovernor, Vault, ConnectorManifest, tempfile::TempDir) {
        use crate::connector::ConnectorToolDef;
        use fl_core::EndpointSet;
        use std::collections::BTreeMap;
        let tmp = tempfile::TempDir::new().unwrap();
        let key = crate::load_or_init_master_key(&tmp.path().join("k")).unwrap();
        let mut vault = Vault::open(tmp.path().join("v.enc"), key).unwrap();
        vault
            .set(crate::SetSecretArgs {
                name: "API_TOKEN".into(),
                value: "tok-123456".into(),
                allowed_hosts: vec!["127.0.0.1".into()],
                allowed_methods: vec!["GET".into()],
                allow_private_network: true,
                ..Default::default()
            })
            .unwrap();
        let set = EndpointSet {
            connector_id: "demo".into(),
            tool: "get_thing".into(),
            endpoints,
            side_effect_class: class,
            idempotency_conformance_test: if class
                == fl_core::governor::SideEffectClass::IdempotentWithKey
            {
                Some("idem_conformance_v1".into())
            } else {
                None
            },
            owner_approved: true,
            policy_digest: "forged".into(),
        };
        let digest = set.computed_policy_digest();
        let manifest = ConnectorManifest {
            id: "demo".into(),
            title: "Demo".into(),
            version: "0.1.0".into(),
            description: String::new(),
            base_url,
            allowed_hosts: vec![],
            default_headers: BTreeMap::from([(
                "Authorization".into(),
                "Bearer {{FERROGATE:API_TOKEN}}".into(),
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
                endpoint_set: Some(set),
            }],
            bridges: vec![],
        };
        let policy = GovernorPolicy {
            budget_units_per_scope: LEGACY_MAX_RESPONSE_BYTES * 4,
            ..Default::default()
        };
        let store = Arc::new(MemGovernorStore::new(policy));
        let gov = GateGovernor {
            store: Some(store),
            policy: GovernorPolicy {
                deadline_window_ms: 5_000,
                ..Default::default()
            },
            aliases: vec![],
            approved_policy_digests: if approved {
                std::collections::HashSet::from([digest])
            } else {
                Default::default()
            },
        };
        (gov, vault, manifest, tmp)
    }

    #[test]
    fn governed_failover_replay_header_and_single_clock() {
        use fl_core::governor::SideEffectClass;
        let closed_port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        drop(std::net::TcpListener::bind(format!("127.0.0.1:{closed_port}")).ok());
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        let handle = std::thread::spawn(move || {
            let req = server
                .recv_timeout(std::time::Duration::from_secs(10))
                .expect("recv")
                .expect("no request");
            let header = req
                .headers()
                .iter()
                .find(|h| h.field.as_str().to_ascii_lowercase() == "idempotency-key")
                .map(|h| h.value.as_str().to_string());
            let _ = req.respond(tiny_http::Response::from_string(
                serde_json::json!({"id":"ok-2"}).to_string(),
            ));
            header
        });
        let (gov, vault, manifest, _tmp) = failover_fixture(
            format!("http://127.0.0.1:{port}/"),
            vec![
                format!("http://127.0.0.1:{closed_port}"),
                format!("http://127.0.0.1:{port}"),
            ],
            SideEffectClass::IdempotentWithKey,
            true,
        );
        let real_now = crate::vault::now_unix_millis();
        // Caller clock (now_ms=0) must be ignored: admission+envelope use now_fn.
        let out = gov
            .governed_invoke(
                &manifest,
                &vault,
                "get_thing",
                &serde_json::json!({"operation_id":"op-replay","idempotency_key":"key-9"}),
                None,
                Principal::Owner,
                "proj",
                0,
                Some(real_now),
            )
            .expect("single-clock failover must succeed");
        assert!(matches!(out, GovernedOutcome::Completed { .. }), "{out:?}");
        assert_eq!(handle.join().unwrap(), Some("key-9".to_string()));
        // Replay guard inside governed_dispatch: same operation refused, no dispatch.
        let out2 = gov.governed_invoke(
            &manifest,
            &vault,
            "get_thing",
            &serde_json::json!({"operation_id":"op-replay","idempotency_key":"key-9"}),
            None,
            Principal::Owner,
            "proj",
            0,
            Some(crate::vault::now_unix_millis()),
        );
        match out2 {
            Ok(GovernedOutcome::Refused(AdmissionOutcome::Denied { .. })) => {}
            other => panic!("expected replay refusal, got {other:?}"),
        }
    }

    #[test]
    fn unapproved_governed_failover_is_single_attempt_only() {
        use fl_core::governor::SideEffectClass;
        let closed_port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        let handle = std::thread::spawn(move || {
            server
                .recv_timeout(std::time::Duration::from_secs(2))
                .map(|r| r.is_some())
                .unwrap_or(false)
        });
        // Base (single attempt target) is the closed port; failover would
        // reach the live second endpoint and succeed if it ran.
        let (gov, vault, manifest, _tmp) = failover_fixture(
            format!("http://127.0.0.1:{closed_port}/"),
            vec![
                format!("http://127.0.0.1:{closed_port}"),
                format!("http://127.0.0.1:{port}"),
            ],
            SideEffectClass::SideEffectful,
            false,
        );
        let now = crate::vault::now_unix_millis();
        let out = gov.governed_invoke(
            &manifest,
            &vault,
            "get_thing",
            &serde_json::json!({}),
            None,
            Principal::Owner,
            "proj",
            now,
            Some(now),
        );
        assert!(
            out.is_err(),
            "single attempt to closed base must fail: {out:?}"
        );
        assert!(!handle.join().unwrap(), "failover endpoint must not be hit");
    }

    #[test]
    fn connector_aliases_include_base_url_path_prefix() {
        use crate::connector::ConnectorToolDef;
        use std::collections::BTreeMap;
        let manifest = ConnectorManifest {
            id: "demo".into(),
            title: "Demo".into(),
            version: "0.1.0".into(),
            description: String::new(),
            base_url: "https://api.example.com:443/v1/".into(),
            allowed_hosts: vec![],
            default_headers: BTreeMap::new(),
            enabled: true,
            tools: vec![ConnectorToolDef {
                name: "chat".into(),
                description: "Chat".into(),
                method: "POST".into(),
                path: "/chat".into(),
                path_params: vec![],
                query_params: vec![],
                body_params: vec![],
                headers: BTreeMap::new(),
                response_fields: vec![],
                input_schema: serde_json::json!({"type":"object"}),
                response_contract: None,
                endpoint_set: None,
            }],
            bridges: vec![],
        };
        let aliases = connector_endpoint_aliases(std::slice::from_ref(&manifest));
        assert_eq!(aliases.len(), 1);
        assert_eq!(aliases[0].path_prefix, "/v1/chat");
        assert_eq!(
            resolve_canonical_endpoint(&aliases, Some("demo"), Some("chat"), "", ""),
            "api.example.com/v1/chat"
        );
        assert_eq!(
            resolve_canonical_endpoint(
                &aliases,
                None,
                None,
                "api.example.com:443",
                "/v1/chat/extra"
            ),
            "api.example.com/v1/chat"
        );
    }

    #[test]
    fn executed_settles_actual_and_unknown_stays_counted() {
        let store = Arc::new(MemGovernorStore::new(GovernorPolicy::default()));
        let gov = GateGovernor {
            store: Some(store.clone()),
            policy: GovernorPolicy::default(),
            aliases: vec![],
            approved_policy_digests: Default::default(),
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
