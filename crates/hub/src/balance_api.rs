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
//!   Answers the [`fl_watch::BalancePlan`].
//! - `POST /api/v1/watch/balance/apply`: the Owner, or a peer that presents
//!   `{ "warrant": "<grant id or flw1. token>" }` for an Owner grant that names the peer's agent
//!   name (`X-Fl-Agent`) or `*` and has scope `watch.balance.apply` or `*`. A single-use grant is
//!   consumed when the apply runs a switch. The plan is made again from the stored policy under
//!   the store guard; a `blocked` plan is a 409. Each switch is a remodel
//!   ([`crate::api::run_remodel`], the remodel endpoint's own body) run as the caller: the Owner
//!   as Owner, a peer as Peer, never more. A failed switch is recorded and the next one runs,
//!   unless the failure left its persona down: then the apply stops. Answers
//!   `{ plan, results, audit_slug, warnings }` and broadcasts `watch.balance.applied` on the
//!   `watch` channel. An apply with nothing to switch writes nothing and spends no grant.
//!
//! Any other method on these paths is a 405 with an `Allow` header, and any query key but
//! `token` a 400.
//!
//! **Quota.** The planner reads the discovered sessions' quota
//! ([`crate::watch_api::discovered_quotas`]), an account probe for every registry vendor no
//! discovered session reports ([`account_quota`]; `codex` and `grok` have none without a
//! transcript), and the stored `watch-quota-*` windows. Discovery and the probes run before the
//! store guard is taken: a discovery snapshot miss opens the store itself.
//!
//! **Agents.** Every hosted-session record that is not decommissioned. One that is not running,
//! or (when the hub starts runtimes) whose runtime the hub did not start, is listed as not
//! remodelable: [`lifecycle::remodel_session`] would refuse it with a 409.

use std::collections::BTreeSet;
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
    WatchQuotaWindow, plan_balance, read_runtime,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::api::{
    RemodelRun, load_discovery_snapshot, resolve_session_target_in, run_remodel, store_open_failure,
};
use crate::auth::HubAuthority;
use crate::discovery::{
    SessionQuotaUsage, discover_route_sessions_without_model_info, quota_for_resolved_route,
};
use crate::lifecycle::{
    self, Caller, HostedSessionRecord, RemodelRequest, STATUS_DECOMMISSIONED, STATUS_RUNNING,
    SessionTarget,
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
        PLAN => handle_plan(state, req),
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

/// The one account-quota probe a balance plan makes for a vendor no discovered session reports.
/// Every account probe goes through here.
fn account_quota(vendor: &str, home: Option<&Path>) -> Option<SessionQuotaUsage> {
    quota_for_resolved_route(vendor, None, home)
}

/// The live quota readings: every discovered route session's, then an account probe for each
/// registry vendor none of them reports. Must run with no store guard held.
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

/// Why a remodel of `rec` would be refused before it starts, so the plan does not recommend it.
/// It mirrors [`lifecycle::remodel_session`]: a session that is not running, or (when the hub
/// starts runtimes) a hosted one whose runtime the hub did not start (409).
fn remodel_blocker(rec: &HostedSessionRecord, spawn_runtime: bool) -> Option<String> {
    if rec.status != STATUS_RUNNING {
        return Some(format!(
            "its hosted session `{}` is {}, not {STATUS_RUNNING}",
            rec.session_id, rec.status
        ));
    }
    if spawn_runtime && rec.hosted && !rec.runtime_spawned {
        return Some(format!(
            "the hub did not start the runtime of `{}`, so a remodel would be refused",
            rec.session_id
        ));
    }
    None
}

/// What the plan reads from the store, under the guard.
struct StoreInputs {
    policy: BalancePolicy,
    history: BalanceHistory,
    agents: Vec<BalanceAgent>,
    stored_quotas: Vec<WatchQuotaWindow>,
}

fn read_store_inputs(store: &AnyStore, spawn_runtime: bool) -> Result<StoreInputs, HttpResponse> {
    let policy = read_policy(store).map_err(balance_error)?;
    let history = read_history(store).map_err(balance_error)?;
    let agents = lifecycle::list_hosted_sessions(store)
        .map_err(|e| error(e.status(), e.to_string()))?
        .into_iter()
        .filter(|r| r.status != STATUS_DECOMMISSIONED)
        .map(|r| BalanceAgent {
            remodel_blocker: remodel_blocker(&r, spawn_runtime),
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

fn evaluate(
    policy: &BalancePolicy,
    inputs: &StoreInputs,
    live: &[WatchQuotaWindow],
    vendors: &[String],
    now_ms: u64,
) -> Result<BalancePlan, HttpResponse> {
    let mut readings = live.to_vec();
    readings.extend(inputs.stored_quotas.iter().cloned());
    plan_balance(&BalanceInputs {
        policy,
        now_ms,
        agents: &inputs.agents,
        readings: &readings,
        candidate_vendors: vendors,
        history: &inputs.history,
    })
    .map_err(balance_error)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanBody {
    #[serde(default)]
    policy: Option<Map<String, Value>>,
}

fn handle_plan(state: &HubState, req: &HttpRequest) -> HttpResponse {
    let body: PlanBody = match body_object(req).and_then(|m| {
        serde_json::from_value(Value::Object(m)).map_err(|e| error(400, e.to_string()))
    }) {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    let vendors = lifecycle::spin_up_vendor_ids();
    let live = live_readings(state, &vendors);
    let spawn = crate::api::lifecycle_env(state).spawn_runtime;
    let Ok(store) = state.open_store() else {
        return store_open_failure();
    };
    let inputs = match read_store_inputs(&store, spawn) {
        Ok(i) => i,
        Err(resp) => return resp,
    };
    drop(store);
    let policy = match &body.policy {
        Some(patch) => match overlay(&inputs.policy, patch) {
            Ok(p) => p,
            Err(e) => return error(400, e),
        },
        None => inputs.policy.clone(),
    };
    match evaluate(
        &policy,
        &inputs,
        &live,
        &vendors,
        stamp::now().unix_millis(),
    ) {
        Ok(plan) => HttpResponse::json(200, json!(plan)),
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

/// The Owner, or a named peer with a verified warrant of scope `watch.balance.apply` (or `*`)
/// that names it. Nothing is consumed here.
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
    let scope = verified.grant.scope.trim();
    if scope != APPLY_SCOPE && scope != "*" {
        return Err(warrant_refused(format!(
            "its scope is `{scope}`, not `{APPLY_SCOPE}`"
        )));
    }
    Ok(ApplyAuthority::Warrant {
        grant_id: verified.grant.grant_id,
        id_or_token: warrant.to_string(),
        single_use: verified.grant.single_use,
    })
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
    let vendors = lifecycle::spin_up_vendor_ids();
    let live = live_readings(state, &vendors);
    let sessions =
        discover_route_sessions_without_model_info(&load_discovery_snapshot(state), home);
    let spawn = crate::api::lifecycle_env(state).spawn_runtime;

    let Ok(store) = state.open_store() else {
        return store_open_failure();
    };
    let inputs = match read_store_inputs(&store, spawn) {
        Ok(i) => i,
        Err(resp) => return resp,
    };
    let now_ms = stamp::now().unix_millis();
    let plan = match evaluate(&inputs.policy, &inputs, &live, &vendors, now_ms) {
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
    if let ApplyAuthority::Warrant {
        id_or_token,
        single_use: true,
        ..
    } = &auth
        && let Err(e) = fl_gate::verify_owner_grant(home, id_or_token, Some(by), true)
    {
        return warrant_refused(e);
    }

    let mut results: Vec<SwitchResult> = Vec::new();
    let mut down = false;
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
        let discovered = resolve_session_target_in(&sessions, &sw.session_id);
        let target = match SessionTarget::resolve(&*store, &sw.session_id, Some(discovered)) {
            Ok(t) => t,
            Err(e) => {
                result.status = SwitchStatus::Refused;
                result.error = Some(e.to_string());
                results.push(result);
                continue;
            }
        };
        let req = RemodelRequest {
            vendor: sw.to_vendor.clone(),
            ..RemodelRequest::default()
        };
        match run_remodel(state, &store, &target, &req, by, caller) {
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
    history.record(
        now_ms,
        results
            .iter()
            .filter(|r| r.status != SwitchStatus::NotRun)
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
    if let Err(e) = write_history(&*store, &history, stamp::now(), by) {
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

    fn record(status: &str, hosted: bool, spawned: bool) -> HostedSessionRecord {
        serde_json::from_value(json!({
            "session_id": "claude:ada", "persona": "ada", "project": "p", "role": "assistant",
            "vendor": "claude", "model": null, "node_id": "n", "hosted": hosted,
            "status": status, "restart_policy": "never", "restart_count": 0,
            "tmux_target": null, "cwd": null, "hydration_dispatched": false,
            "runtime_spawned": spawned, "created_at_ms": 0, "updated_at_ms": 0,
            "decommission_prepared_at_ms": null
        }))
        .unwrap()
    }

    #[test]
    fn the_remodel_blocker_mirrors_what_a_remodel_refuses() {
        assert_eq!(remodel_blocker(&record("running", true, true), true), None);
        let why = remodel_blocker(&record("running", true, false), true).unwrap();
        assert!(why.contains("did not start"), "{why}");
        // With runtimes off (tests, FL_TEST_SPINUP_NO_SPAWN) nothing checks the old runtime.
        assert_eq!(
            remodel_blocker(&record("running", true, false), false),
            None
        );
        // An unhosted session has no runtime to stop.
        assert_eq!(
            remodel_blocker(&record("running", false, false), true),
            None
        );
        let why = remodel_blocker(&record("decommissioning", true, true), false).unwrap();
        assert!(why.contains("decommissioning"), "{why}");
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
}
