//! `/api/v1/watch/balance*`: automated subscription usage balancing (Local Record 60) over the
//! pure planner in [`fl_watch::balance`].
//!
//! - `GET  /api/v1/watch/balance`: `{ policy, enabled }`, the stored policy (the defaults when
//!   none is stored). Any authenticated caller.
//! - `POST /api/v1/watch/balance/policy`: Owner only (403 otherwise). The body is a full or
//!   partial policy, laid over the stored one; an unknown field or an invalid value is a 400.
//!   Answers `{ policy, enabled }` as stored.
//! - `POST /api/v1/watch/balance/plan`: a dry run, writing nothing. Any authenticated caller.
//!   An optional `{ "policy": { ... } }` is laid over the stored policy for this evaluation only.
//!   Each switch is prechecked as a remodel by this caller (see **Agents**), so the plan shows
//!   what an apply by the same caller would run. Answers the [`fl_watch::BalancePlan`].
//! - `POST /api/v1/watch/balance/apply`: the Owner, or a peer that presents
//!   `{ "warrant": "<grant id or flw1. token>" }` for an Owner grant that names the peer's agent
//!   name (`X-Fl-Agent`) or `*` and has scope `watch.balance.apply` (no other scope). The plan is
//!   made again from the stored policy under the store guard; a `blocked` plan is a 409. A
//!   single-use grant is consumed under the guard immediately before the first remodel runs, so a
//!   blocked apply, an apply with nothing to switch, and one whose every triggered agent fails its
//!   precheck spend nothing. Each switch is a remodel ([`crate::api::run_remodel`], the remodel
//!   endpoint's own body) run as the caller: the Owner as Owner, a peer as Peer, never more. A
//!   failed switch is recorded and the next one runs, unless the failure left its persona down:
//!   then the apply stops. Answers `{ plan, results, audit_slug, warnings }` and broadcasts
//!   `watch.balance.applied` on the `watch` channel. An apply with nothing to switch writes
//!   nothing. The history (`last_apply_ms` and each persona's cooldown) notes only the switches
//!   whose remodel ran (`ok` or `failed`): a `refused` one touched nothing.
//!
//! Any other method on these paths is a 405 with an `Allow` header, and any query key but
//! `token` a 400.
//!
//! **Peer identity.** A peer's agent name is its own `X-Fl-Agent` header: every peer shares the
//! one peer token, so the hub cannot tell peers apart. Any peer that holds a warrant's grant id
//! or token can therefore name that warrant's target agent and apply with it, as
//! `POST /api/v1/auth/grants/verify` lets it consume one (`warrants.rs`). Closing that needs
//! per-agent peer tokens. A peer never runs a switch as more than `Caller::Peer`.
//!
//! **Vendors.** The candidate targets are the subscription vendors
//! ([`lifecycle::SPIN_UP_VENDORS`]), never a metered API vendor of the registry; the policy's
//! `allowed_target_vendors` and `excluded_target_vendors` only narrow them.
//!
//! **Quota.** The planner reads the discovered sessions' quota
//! ([`crate::watch_api::discovered_quotas`]), an account probe for every subscription vendor no
//! discovered session reports ([`account_quota`]; `codex` and `grok` have none without a
//! transcript), and the stored `watch-quota-*` windows. Discovery and the probes run before the
//! store guard is taken: a discovery snapshot miss opens the store itself.
//!
//! **Agents.** Every hosted-session record that is not decommissioned. One that is not running
//! is listed as not remodelable. For every other switch the planner chooses, the hub runs the
//! remodel's own side-effect-free checks ([`lifecycle::remodel_precheck`]) under the store guard,
//! for the caller and the chosen vendor: one that a remodel would refuse (a runtime other
//! sessions share, a `tmux_target` naming another tmux session, a vendor that cannot be restored
//! for a peer, a missing directory, ...) is a `not_remodelable` skip with the refusal, and the
//! next triggered agent takes its run slot.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use fl_core::store::StoreError;
use fl_exec::stamp;
use fl_store::AnyStore;
use fl_watch::balance::APPLY_SCOPE;
use fl_watch::balance_store::{
    BalanceAudit, SwitchResult, SwitchStatus, read_history, read_policy, write_audit,
    write_history, write_policy,
};
use fl_watch::{
    BalanceAgent, BalanceError, BalanceHistory, BalanceInputs, BalancePlan, BalancePolicy,
    WatchQuotaWindow, plan_balance_checked, read_runtime,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::api::{
    RemodelRun, load_discovery_snapshot, resolve_session_target_in, run_remodel, store_open_failure,
};
use crate::auth::HubAuthority;
use crate::direct_quota::AgyQuotaTarget;
use crate::discovery::{
    DiscoveredSessionSummary, SessionQuotaUsage, discover_route_sessions_without_model_info,
    quota_for_resolved_route_at,
};
use crate::lifecycle::{
    self, Caller, HostedSessionRecord, LifecycleEnv, RemodelRequest, STATUS_DECOMMISSIONED,
    STATUS_RUNNING, SessionTarget,
};
use crate::watch_api::{discovered_quotas, quota_windows, refuse_unknown_keys};
use crate::{HttpRequest, HttpResponse, HubState};

pub(crate) const BALANCE: &str = "/api/v1/watch/balance";
pub(crate) const POLICY: &str = "/api/v1/watch/balance/policy";
pub(crate) const PLAN: &str = "/api/v1/watch/balance/plan";
pub(crate) const APPLY: &str = "/api/v1/watch/balance/apply";

/// The response for a `/api/v1/watch/balance*` request, or `None` for any other request.
pub(crate) fn route(
    state: &HubState,
    authority: &HubAuthority,
    req: &HttpRequest,
) -> Option<HttpResponse> {
    let path = req.path.trim_end_matches('/');
    let method = match path {
        BALANCE => "GET",
        POLICY | PLAN | APPLY => "POST",
        _ => return None,
    };
    if req.method != method {
        return Some(
            error(405, format!("{path} takes only {method}")).with_header("Allow", method),
        );
    }
    if let Err(e) = refuse_unknown_keys(&req.query, &[]) {
        return Some(error(400, e));
    }
    Some(match path {
        BALANCE => handle_get(state),
        POLICY => handle_post_policy(state, authority, req),
        PLAN => handle_plan(state, authority, req),
        _ => handle_apply(state, authority, req),
    })
}

fn error(status: u16, message: impl Into<String>) -> HttpResponse {
    HttpResponse::json(status, json!({ "error": message.into() }))
}

fn balance_error(e: BalanceError) -> HttpResponse {
    match e {
        BalanceError::InvalidPolicy(_) => error(400, e.to_string()),
        BalanceError::Store(_) | BalanceError::Undecodable(_) => error(500, e.to_string()),
    }
}

fn store_error(e: StoreError) -> HttpResponse {
    error(500, e.to_string())
}

/// The request body as a JSON object; an empty body is `{}`.
fn body_object(req: &HttpRequest) -> Result<Map<String, Value>, HttpResponse> {
    if req.body.iter().all(u8::is_ascii_whitespace) {
        return Ok(Map::new());
    }
    match serde_json::from_slice::<Value>(&req.body) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err(error(400, "the JSON body must be an object")),
        Err(e) => Err(error(400, format!("invalid JSON body: {e}"))),
    }
}

/// `patch` laid over `base`, key by key, then validated. An unknown key is refused.
fn overlay(base: &BalancePolicy, patch: &Map<String, Value>) -> Result<BalancePolicy, String> {
    let mut value = serde_json::to_value(base).map_err(|e| e.to_string())?;
    if let Some(obj) = value.as_object_mut() {
        for (k, v) in patch {
            obj.insert(k.clone(), v.clone());
        }
    }
    let policy: BalancePolicy =
        serde_json::from_value(value).map_err(|e| format!("invalid balance policy: {e}"))?;
    policy.validate().map_err(|e| e.to_string())?;
    Ok(policy)
}

fn policy_view(policy: &BalancePolicy) -> Value {
    json!({ "policy": policy, "enabled": policy.enabled })
}

fn handle_get(state: &HubState) -> HttpResponse {
    let Ok(store) = state.open_store() else {
        return store_open_failure();
    };
    match read_policy(&*store) {
        Ok(policy) => HttpResponse::json(200, policy_view(&policy)),
        Err(e) => balance_error(e),
    }
}

fn handle_post_policy(
    state: &HubState,
    authority: &HubAuthority,
    req: &HttpRequest,
) -> HttpResponse {
    if !authority.is_owner() {
        return error(403, "only Owner authority may change the balance policy");
    }
    let patch = match body_object(req) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let Ok(store) = state.open_store() else {
        return store_open_failure();
    };
    // A stored policy that no longer reads is replaced from the defaults, so the Owner can
    // always repair it here.
    let current = match read_policy(&*store) {
        Ok(p) => p,
        Err(BalanceError::Store(e)) => return store_error(e),
        Err(_) => BalancePolicy::default(),
    };
    let policy = match overlay(&current, &patch) {
        Ok(p) => p,
        Err(e) => return error(400, e),
    };
    match write_policy(&*store, &policy, stamp::now(), authority.actor_name()) {
        Ok(()) => HttpResponse::json(200, policy_view(&policy)),
        Err(e) => balance_error(e),
    }
}

/// The vendors an agent may be moved to: the subscription vendors, never a metered API vendor.
/// The policy only narrows this list.
pub(crate) fn candidate_vendors() -> Vec<String> {
    lifecycle::SPIN_UP_VENDORS.map(String::from).to_vec()
}

fn caller_of(authority: &HubAuthority) -> Caller {
    if authority.is_owner() {
        Caller::Owner
    } else {
        Caller::Peer
    }
}

/// The one account-quota probe a balance plan makes for a vendor no discovered session reports.
/// Every account probe goes through here. There is no session, so the Antigravity probe gets no
/// route, model or conversation id: it reads the account the environment's language server
/// serves, and only when the host may be probed (no `home_override`).
fn account_quota(vendor: &str, home: Option<&Path>) -> Option<SessionQuotaUsage> {
    if vendor == "grok" {
        return None;
    }
    quota_for_resolved_route_at(vendor, None, home, &AgyQuotaTarget::default())
}

/// The live quota readings: every discovered route session's, then an account probe for each
/// candidate vendor none of them reports. Must run with no store guard held.
fn live_readings(state: &HubState, vendors: &[String]) -> Vec<WatchQuotaWindow> {
    let home = state.config.home_override.as_deref();
    let mut readings = discovered_quotas(state);
    let seen: BTreeSet<String> = readings
        .iter()
        .map(|q| q.vendor.trim().to_ascii_lowercase())
        .collect();
    for vendor in vendors {
        if seen.contains(vendor) {
            continue;
        }
        if let Some(q) = account_quota(vendor, home) {
            readings.extend(
                quota_windows(vendor, "account", &q)
                    .into_iter()
                    .map(|mut w| {
                        w.session = None;
                        w
                    }),
            );
        }
    }
    readings
}

/// Why balancing leaves `rec` alone before any remodel check: a session that is not running
/// (one being decommissioned, for example) is not moved. Every other refusal comes from
/// [`lifecycle::remodel_precheck`].
fn status_blocker(rec: &HostedSessionRecord) -> Option<String> {
    (rec.status != STATUS_RUNNING).then(|| {
        format!(
            "its hosted session `{}` is {}, not {STATUS_RUNNING}",
            rec.session_id, rec.status
        )
    })
}

/// What the plan reads from the store, under the guard.
pub(crate) struct StoreInputs {
    pub(crate) policy: BalancePolicy,
    pub(crate) history: BalanceHistory,
    pub(crate) agents: Vec<BalanceAgent>,
    pub(crate) stored_quotas: Vec<WatchQuotaWindow>,
}

pub(crate) fn read_store_inputs(store: &AnyStore) -> Result<StoreInputs, HttpResponse> {
    let policy = read_policy(store).map_err(balance_error)?;
    let history = read_history(store).map_err(balance_error)?;
    let agents = lifecycle::list_hosted_sessions(store)
        .map_err(|e| error(e.status(), e.to_string()))?
        .into_iter()
        .filter(|r| r.status != STATUS_DECOMMISSIONED)
        .map(|r| BalanceAgent {
            remodel_blocker: status_blocker(&r),
            persona: r.persona,
            session_id: r.session_id,
            vendor: r.vendor,
            model: r.model,
            project: r.project,
        })
        .collect();
    let stored_quotas = read_runtime(store).map_err(store_error)?.quotas;
    Ok(StoreInputs {
        policy,
        history,
        agents,
        stored_quotas,
    })
}

/// The remodel a balance switch runs: the target vendor, everything else as the session has it.
fn switch_request(to_vendor: &str) -> RemodelRequest {
    RemodelRequest {
        vendor: to_vendor.to_string(),
        ..RemodelRequest::default()
    }
}

/// The plan for `policy`, each switch the planner chooses prechecked as a remodel by `caller`
/// ([`lifecycle::remodel_precheck`]), and the resolved target of every planned switch by
/// session id. Reads only; the caller holds the store guard, and has run discovery and the
/// quota probes before taking it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn checked_plan(
    store: &AnyStore,
    env: &LifecycleEnv<'_>,
    sessions: &[DiscoveredSessionSummary],
    caller: Caller,
    policy: &BalancePolicy,
    inputs: &StoreInputs,
    live: &[WatchQuotaWindow],
    now_ms: u64,
) -> Result<(BalancePlan, BTreeMap<String, SessionTarget>), HttpResponse> {
    let mut readings = live.to_vec();
    readings.extend(inputs.stored_quotas.iter().cloned());
    let vendors = candidate_vendors();
    let mut targets: BTreeMap<String, SessionTarget> = BTreeMap::new();
    let plan = plan_balance_checked(
        &BalanceInputs {
            policy,
            now_ms,
            agents: &inputs.agents,
            readings: &readings,
            candidate_vendors: &vendors,
            history: &inputs.history,
        },
        &mut |agent, to_vendor| {
            let discovered = resolve_session_target_in(sessions, &agent.session_id);
            let target = SessionTarget::resolve(store, &agent.session_id, Some(discovered))
                .map_err(|e| format!("{e} ({})", e.status()))?;
            lifecycle::remodel_precheck(store, env, &target, &switch_request(to_vendor), caller)
                .map_err(|e| format!("{e} ({})", e.status()))?;
            targets.insert(agent.session_id.clone(), target);
            Ok(())
        },
    )
    .map_err(balance_error)?;
    Ok((plan, targets))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanBody {
    #[serde(default)]
    policy: Option<Map<String, Value>>,
}

fn handle_plan(state: &HubState, authority: &HubAuthority, req: &HttpRequest) -> HttpResponse {
    let body: PlanBody = match body_object(req).and_then(|m| {
        serde_json::from_value(Value::Object(m)).map_err(|e| error(400, e.to_string()))
    }) {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    // Discovery and probes first: both may open the store, so no guard may be held yet.
    let home = state.config.home_override.as_deref();
    let vendors = candidate_vendors();
    let live = live_readings(state, &vendors);
    let sessions =
        discover_route_sessions_without_model_info(&load_discovery_snapshot(state), home);
    let env = crate::api::lifecycle_env(state);
    let Ok(store) = state.open_store() else {
        return store_open_failure();
    };
    let inputs = match read_store_inputs(&store) {
        Ok(i) => i,
        Err(resp) => return resp,
    };
    let policy = match &body.policy {
        Some(patch) => match overlay(&inputs.policy, patch) {
            Ok(p) => p,
            Err(e) => return error(400, e),
        },
        None => inputs.policy.clone(),
    };
    // The prechecks read the store, so the guard is held through the evaluation (and only that).
    let result = checked_plan(
        &store,
        &env,
        &sessions,
        caller_of(authority),
        &policy,
        &inputs,
        &live,
        stamp::now().unix_millis(),
    );
    drop(store);
    match result {
        Ok((plan, _)) => HttpResponse::json(200, json!(plan)),
        Err(resp) => resp,
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApplyBody {
    #[serde(default)]
    warrant: Option<String>,
}

/// On whose authority an apply runs.
enum ApplyAuthority {
    Owner,
    /// A peer holding an Owner warrant for itself and this scope.
    Warrant {
        grant_id: String,
        id_or_token: String,
        single_use: bool,
    },
}

fn warrant_refused(reason: impl std::fmt::Display) -> HttpResponse {
    error(
        403,
        format!("the warrant does not authorize a balance apply: {reason}"),
    )
}

/// A warrant's scope authorizes an apply only when it is `watch.balance.apply` itself.
fn check_scope(scope: &str) -> Result<(), HttpResponse> {
    let scope = scope.trim();
    if scope == APPLY_SCOPE {
        Ok(())
    } else {
        Err(warrant_refused(format!(
            "its scope is `{scope}`, not `{APPLY_SCOPE}`"
        )))
    }
}

/// The Owner, or a named peer with a verified warrant of scope `watch.balance.apply` that names
/// it. Nothing is consumed here.
fn authorize_apply(
    state: &HubState,
    authority: &HubAuthority,
    warrant: Option<&str>,
) -> Result<ApplyAuthority, HttpResponse> {
    if authority.is_owner() {
        return Ok(ApplyAuthority::Owner);
    }
    let Some(warrant) = warrant.map(str::trim).filter(|w| !w.is_empty()) else {
        return Err(error(
            403,
            format!(
                "a balance apply needs Owner authority, or an Owner warrant with scope \
                 `{APPLY_SCOPE}` in `warrant`"
            ),
        ));
    };
    // The name is the peer's own `X-Fl-Agent` (see the module doc on peer identity).
    let named = matches!(authority, HubAuthority::Peer { agent: Some(a) } if !a.trim().is_empty())
        && authority.actor_name() != "peer";
    if !named {
        return Err(error(
            403,
            "a peer presenting a warrant must name itself with `X-Fl-Agent`",
        ));
    }
    let me = authority.actor_name();
    let home = state.config.home_override.as_deref();
    let verified = match fl_gate::verify_owner_grant(home, warrant, Some(me), false) {
        Ok(v) => v,
        Err(fl_gate::GateError::WarrantRejected { reason })
        | Err(fl_gate::GateError::WarrantInput { reason }) => return Err(warrant_refused(reason)),
        Err(other) => {
            eprintln!("warrant store error: {other}");
            return Err(error(500, "warrant store error"));
        }
    };
    check_scope(&verified.grant.scope)?;
    Ok(ApplyAuthority::Warrant {
        grant_id: verified.grant.grant_id,
        id_or_token: warrant.to_string(),
        single_use: verified.grant.single_use,
    })
}

/// Whether a switch's remodel ran (and so counts for the history): it succeeded or failed. A
/// refused remodel touched nothing; a switch not run never started.
fn remodel_ran(status: SwitchStatus) -> bool {
    matches!(status, SwitchStatus::Ok | SwitchStatus::Failed)
}

fn handle_apply(state: &HubState, authority: &HubAuthority, req: &HttpRequest) -> HttpResponse {
    let body: ApplyBody = match body_object(req).and_then(|m| {
        serde_json::from_value(Value::Object(m)).map_err(|e| error(400, e.to_string()))
    }) {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    let auth = match authorize_apply(state, authority, body.warrant.as_deref()) {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let (caller, authority_kind, grant_id) = match &auth {
        ApplyAuthority::Owner => (Caller::Owner, "owner", None),
        ApplyAuthority::Warrant { grant_id, .. } => {
            (Caller::Peer, "warrant", Some(grant_id.clone()))
        }
    };
    let by = authority.actor_name();
    let home = state.config.home_override.as_deref();

    // Discovery and probes first: both may open the store, so no guard may be held yet. A
    // fresh discovery snapshot, as every request that acts on a session uses.
    let vendors = candidate_vendors();
    let live = live_readings(state, &vendors);
    let sessions =
        discover_route_sessions_without_model_info(&load_discovery_snapshot(state), home);
    let env = crate::api::lifecycle_env(state);

    let Ok(store) = state.open_store() else {
        return store_open_failure();
    };
    let inputs = match read_store_inputs(&store) {
        Ok(i) => i,
        Err(resp) => return resp,
    };
    let now_ms = stamp::now().unix_millis();
    let (plan, mut targets) = match checked_plan(
        &store,
        &env,
        &sessions,
        caller,
        &inputs.policy,
        &inputs,
        &live,
        now_ms,
    ) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    if let Some(why) = &plan.blocked {
        return HttpResponse::json(
            409,
            json!({ "error": format!("balance apply blocked: {why}"), "plan": plan }),
        );
    }
    if plan.switches.is_empty() {
        return HttpResponse::json(
            200,
            json!({ "plan": plan, "results": [], "audit_slug": null, "warnings": [] }),
        );
    }

    let mut results: Vec<SwitchResult> = Vec::new();
    let mut down = false;
    let mut grant_checked = false;
    for sw in &plan.switches {
        let mut result = SwitchResult {
            persona: sw.persona.clone(),
            session_id: sw.session_id.clone(),
            from_vendor: sw.from_vendor.clone(),
            to_vendor: sw.to_vendor.clone(),
            status: SwitchStatus::NotRun,
            new_session_id: None,
            outcome: None,
            restored_session_id: None,
            error: None,
        };
        if down {
            result.error = Some("not run: an earlier switch left its persona down".into());
            results.push(result);
            continue;
        }
        // Every planned switch passed its precheck, which resolved its target.
        let Some(target) = targets.remove(&sw.session_id) else {
            result.status = SwitchStatus::Refused;
            result.error = Some(format!("`{}` was not prechecked", sw.session_id));
            results.push(result);
            continue;
        };
        // A single-use grant is spent only now, under the guard, just before the first remodel.
        if !grant_checked {
            grant_checked = true;
            if let ApplyAuthority::Warrant {
                id_or_token,
                single_use: true,
                ..
            } = &auth
                && let Err(e) = fl_gate::verify_owner_grant(home, id_or_token, Some(by), true)
            {
                return warrant_refused(e);
            }
        }
        match run_remodel(
            state,
            &store,
            &target,
            &switch_request(&sw.to_vendor),
            by,
            caller,
        ) {
            RemodelRun::Done { out, .. } => {
                result.status = SwitchStatus::Ok;
                result.new_session_id = Some(out.session_id);
                result.outcome = Some("remodeled".into());
            }
            RemodelRun::Failed {
                message,
                restored_session_id,
                ..
            } => {
                result.status = SwitchStatus::Failed;
                down = restored_session_id.is_none();
                result.outcome = Some(if down { "down" } else { "restored" }.into());
                result.restored_session_id = restored_session_id;
                result.error = Some(message);
            }
            RemodelRun::Refused(e) => {
                result.status = SwitchStatus::Refused;
                result.error = Some(e.to_string());
            }
        }
        results.push(result);
    }

    let mut warnings = Vec::new();
    let mut history = inputs.history.clone();
    let history_changed = history.record(
        now_ms,
        results
            .iter()
            .filter(|r| remodel_ran(r.status))
            .map(|r| r.persona.as_str()),
    );
    let audit = BalanceAudit {
        applied_at_ms: now_ms,
        by: by.to_string(),
        authority: authority_kind.to_string(),
        warrant_grant_id: grant_id,
        plan: plan.clone(),
        results: results.clone(),
    };
    let audit_slug = match write_audit(&*store, &audit, stamp::now()) {
        Ok(slug) => Some(slug),
        Err(e) => {
            warnings.push(format!("the audit entry was not written: {e}"));
            None
        }
    };
    if history_changed && let Err(e) = write_history(&*store, &history, stamp::now(), by) {
        warnings.push(format!("the balance history was not updated: {e}"));
    }
    let data = json!({
        "plan": plan,
        "results": results,
        "audit_slug": audit_slug,
        "warnings": warnings,
    });
    state.broadcast(
        "watch",
        "watch.balance.applied",
        json!({
            "audit_slug": audit_slug,
            "by": by,
            "authority": authority_kind,
            "results": results,
        }),
    );
    // Every write above belongs to this apply; the next watch or discovery read starts fresh.
    drop(store);
    state.watch_caches.invalidate();
    state.discovery_snapshots.invalidate();
    crate::discovery::invalidate_route_discovery_cache();
    HttpResponse::json(200, data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(status: &str) -> HostedSessionRecord {
        serde_json::from_value(json!({
            "session_id": "claude:ada", "persona": "ada", "project": "p", "role": "assistant",
            "vendor": "claude", "model": null, "node_id": "n", "hosted": true,
            "status": status, "restart_policy": "never", "restart_count": 0,
            "tmux_target": null, "cwd": null, "hydration_dispatched": false,
            "runtime_spawned": true, "created_at_ms": 0, "updated_at_ms": 0,
            "decommission_prepared_at_ms": null
        }))
        .unwrap()
    }

    #[test]
    fn only_a_running_session_passes_the_status_blocker() {
        assert_eq!(status_blocker(&record("running")), None);
        let why = status_blocker(&record("decommissioning")).unwrap();
        assert!(why.contains("decommissioning"), "{why}");
    }

    #[test]
    fn the_candidates_are_the_subscription_vendors_only() {
        let c = candidate_vendors();
        assert_eq!(c, ["claude", "antigravity", "codex", "grok", "muse"]);
        assert!(c.iter().all(|v| !v.ends_with("-api")), "{c:?}");
        // The registry holds metered API vendors the candidates leave out.
        let registry = lifecycle::spin_up_vendor_ids();
        assert!(registry.iter().any(|v| v.ends_with("-api")), "{registry:?}");
    }

    #[test]
    fn only_the_apply_scope_itself_authorizes() {
        check_scope(APPLY_SCOPE).unwrap();
        check_scope(&format!(" {APPLY_SCOPE} ")).unwrap();
        for scope in [
            "*",
            "watch.balance",
            "watch.balance.apply.x",
            "agent.replace",
            "",
        ] {
            let resp = check_scope(scope).unwrap_err();
            assert_eq!(resp.status, 403, "{scope}");
            let v = resp.json_value().unwrap();
            assert!(
                v["error"].as_str().unwrap().contains(APPLY_SCOPE),
                "{scope}: {v}"
            );
        }
    }

    #[test]
    fn only_a_remodel_that_ran_counts_for_the_history() {
        assert!(remodel_ran(SwitchStatus::Ok));
        assert!(remodel_ran(SwitchStatus::Failed));
        assert!(!remodel_ran(SwitchStatus::Refused));
        assert!(!remodel_ran(SwitchStatus::NotRun));
    }

    #[test]
    fn overlay_lays_a_patch_over_the_base_and_refuses_unknown_or_invalid() {
        let base = BalancePolicy {
            cooldown_ms: 1,
            ..BalancePolicy::default()
        };
        let patch = |v: Value| v.as_object().unwrap().clone();
        let p = overlay(&base, &patch(json!({"enabled": true}))).unwrap();
        assert!(p.enabled);
        assert_eq!(
            p.cooldown_ms, 1,
            "the base is kept where the patch is silent"
        );
        assert!(overlay(&base, &patch(json!({"cooldown": 1}))).is_err());
        assert!(overlay(&base, &patch(json!({"cooldown_ms": -1}))).is_err());
        assert!(overlay(&base, &patch(json!({"primary_trigger_pct": 10}))).is_err());
    }

    #[test]
    fn grok_without_transcript_does_not_feed_balance_account_quota_or_watch_quotas() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path();
        let logs = home.join(".grok").join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        let line = json!({
            "time": "2026-11-08T18:00:00.000Z",
            "msg": "billing: fetched credits config",
            "ctx": {
                "subscriptionTier": "SuperGrok",
                "config": {
                    "creditUsagePercent": 24.0,
                    "billingPeriodEnd": "2099-12-01T00:00:00Z"
                }
            }
        });
        std::fs::write(logs.join("unified.jsonl"), format!("{line}\n")).unwrap();

        // Balance planner's account probe keeps Grok without a transcript unknown.
        assert!(account_quota("grok", Some(home)).is_none());

        // Roster/session route discovery still populates Grok quota without a transcript path.
        let direct = crate::discovery::quota_for_resolved_route("grok", None, Some(home), "grok-1")
            .expect("roster/session route discovery still reads unified.jsonl");
        assert_eq!(direct.tier.as_deref(), Some("SuperGrok"));
        assert_eq!(direct.secondary_window.unwrap().used_percent, 24.0);
    }
}
