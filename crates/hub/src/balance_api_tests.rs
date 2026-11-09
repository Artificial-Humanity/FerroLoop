//! `/api/v1/watch/balance*` (Local Record 60): the dry run, the policy, and applies by the
//! Owner and by a peer holding an Owner warrant.

use fl_core::at::At;
use fl_core::pr_policy::ReviewerApps;
use fl_core::state::{
    MergeMode, RuntimeEntry, SensitivityPatch, StatePayload, StateScope, UpsertStateArgs,
};
use fl_core::store::StateStore;
use fl_watch::balance_store::{list_audits, read_history};
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::balance_api::{APPLY, BALANCE, PLAN, POLICY};
use crate::lifecycle::{self, HostedSessionRecord};
use crate::{HttpRequest, HttpResponse, HubServerConfig, HubState, handle_http_request};

const OWNER: &str = "owner-tok-secret";
const PEER: &str = "peer-tok-secret";

fn hub(tmp: &TempDir) -> HubState {
    HubState::new(HubServerConfig {
        listen: "127.0.0.1:0".into(),
        store_path: tmp.path().join("store.redb"),
        repo_root: tmp.path().to_path_buf(),
        ui_dir: None,
        owner_login: "lmcfarlin".into(),
        reviewer_apps: ReviewerApps::default(),
        owner_token: Some(OWNER.into()),
        peer_token: Some(PEER.into()),
        trust_tailscale_headers: false,
        enable_terminal: false,
        session_ttl_secs: 3600,
        home_override: Some(tmp.path().to_path_buf()),
        github: crate::github_pr::HubGithub::default(),
    })
}

fn call(
    state: &HubState,
    method: &str,
    path: &str,
    token: &str,
    agent: Option<&str>,
    body: Option<&Value>,
) -> (u16, Value) {
    let mut req =
        HttpRequest::new(method, path).with_header("Authorization", format!("Bearer {token}"));
    if let Some(a) = agent {
        req = req.with_header("X-Fl-Agent", a);
    }
    if let Some(b) = body {
        req = req.with_json_body(b);
    }
    let resp = handle_http_request(state, &req);
    let v = resp.json_value().unwrap_or(Value::Null);
    (resp.status, v)
}

fn post(state: &HubState, path: &str, token: &str, body: Value) -> (u16, Value) {
    call(state, "POST", path, token, None, Some(&body))
}

fn spin_up(state: &HubState, persona: &str, vendor: &str) {
    let resp = handle_http_request(
        state,
        &HttpRequest::new("POST", "/api/v1/sessions/spin-up")
            .with_header("Authorization", format!("Bearer {OWNER}"))
            .with_json_body(&json!({
                "persona": persona, "project": "FerroLoop", "role": "assistant", "vendor": vendor
            })),
    );
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
}

/// Store a primary-window quota reading for `vendor` (no reset time: always live).
fn quota(state: &HubState, vendor: &str, pct: f64) {
    quota_entry(
        state,
        vendor,
        json!({"vendor": vendor, "window": "primary", "used_percent": pct}),
    );
}

fn quota_entry(state: &HubState, vendor: &str, value: Value) {
    state
        .open_store()
        .unwrap()
        .upsert_state(UpsertStateArgs {
            scope: StateScope::Workspace,
            slug: format!("watch-quota-{vendor}"),
            sensitivity: SensitivityPatch::Inherit,
            payload: StatePayload::Runtime(RuntimeEntry {
                value,
                merge_mode: MergeMode::LastWriter,
            }),
            at: At::from_unix_millis(1),
            by: "test".into(),
        })
        .unwrap();
}

/// ada and bo on claude, which runs hot; codex warm, muse cool, the rest unread.
fn hot_claude(state: &HubState) {
    spin_up(state, "ada", "claude");
    spin_up(state, "bo", "codex");
    quota(state, "claude", 95.0);
    quota(state, "codex", 40.0);
    quota(state, "muse", 10.0);
}

fn records(state: &HubState) -> Vec<HostedSessionRecord> {
    lifecycle::list_hosted_sessions(&*state.open_store().unwrap()).unwrap()
}

fn live(state: &HubState) -> Vec<String> {
    records(state)
        .into_iter()
        .filter(|r| r.status == "running")
        .map(|r| r.session_id)
        .collect()
}

fn audits(state: &HubState) -> Vec<(String, fl_watch::BalanceAudit)> {
    list_audits(&*state.open_store().unwrap()).unwrap()
}

fn set_policy(state: &HubState, body: Value) {
    let (status, v) = post(state, POLICY, OWNER, body);
    assert_eq!(status, 200, "{v}");
}

fn issue(state: &HubState, target: &str, scope: &str, single_use: bool) -> Value {
    let (status, grant) = post(
        state,
        "/api/v1/auth/grants",
        OWNER,
        json!({
            "target_agent": target, "scope": scope,
            "statement": "balance the subscriptions", "single_use": single_use,
        }),
    );
    assert_eq!(status, 201, "{grant}");
    grant
}

fn keys(v: &Value) -> Vec<&str> {
    let mut k: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    k.sort_unstable();
    k
}

#[test]
fn a_dry_run_plans_the_hot_agent_onto_the_coolest_vendor_and_writes_nothing() {
    let tmp = TempDir::new().unwrap();
    let state = hub(&tmp);
    hot_claude(&state);
    let before = records(&state);
    let mut frames = state.subscribe();

    // Any authenticated caller may plan.
    let (status, plan) = call(&state, "POST", PLAN, PEER, None, None);
    assert_eq!(status, 200, "{plan}");
    assert_eq!(
        keys(&plan),
        vec![
            "evaluated_at_ms",
            "policy",
            "skipped",
            "switches",
            "vendors"
        ]
    );
    let sw = &plan["switches"].as_array().unwrap()[0];
    assert_eq!(plan["switches"].as_array().unwrap().len(), 1, "{plan}");
    assert_eq!(sw["persona"], "ada");
    assert_eq!(sw["session_id"], "claude:ada");
    assert_eq!(sw["from_vendor"], "claude");
    assert_eq!(sw["to_vendor"], "muse");
    assert_eq!(sw["trigger"], "primary");
    assert_eq!(sw["target_score"], 10.0);
    let skipped = plan["skipped"].as_array().unwrap();
    assert_eq!(skipped.len(), 1);
    assert_eq!(
        (
            skipped[0]["persona"].as_str(),
            skipped[0]["reason"].as_str()
        ),
        (Some("bo"), Some("healthy"))
    );
    let claude = plan["vendors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["vendor"] == "claude")
        .unwrap();
    assert_eq!(claude["primary"]["used_percent"], 95.0);
    assert_eq!(plan["policy"]["primary_trigger_pct"], 90.0);

    // A policy override applies to this evaluation only.
    let (status, plan) = post(
        &state,
        PLAN,
        OWNER,
        json!({ "policy": { "excluded_target_vendors": ["muse"] } }),
    );
    assert_eq!(status, 200, "{plan}");
    assert_eq!(plan["switches"][0]["to_vendor"], "codex");
    let (_, stored) = call(&state, "GET", BALANCE, PEER, None, None);
    assert_eq!(stored["policy"]["excluded_target_vendors"], json!([]));
    for bad in [
        json!({ "policy": { "nope": 1 } }),
        json!({ "policy": { "primary_trigger_pct": 50 } }),
        json!({ "policy": 5 }),
        json!({ "polcy": {} }),
        json!([1]),
    ] {
        let (status, v) = post(&state, PLAN, OWNER, bad.clone());
        assert_eq!(status, 400, "{bad}: {v}");
    }

    // Nothing changed.
    assert_eq!(records(&state), before);
    assert!(audits(&state).is_empty());
    assert_eq!(
        read_history(&*state.open_store().unwrap()).unwrap(),
        fl_watch::BalanceHistory::default()
    );
    assert!(frames.try_recv().is_err());
}

#[test]
fn the_policy_is_owner_only_partial_and_validated() {
    let tmp = TempDir::new().unwrap();
    let state = hub(&tmp);
    let (status, v) = call(&state, "GET", BALANCE, PEER, None, None);
    assert_eq!(status, 200);
    assert_eq!(keys(&v), vec!["enabled", "policy"]);
    assert_eq!(v["enabled"], false);
    assert_eq!(v["policy"]["cooldown_ms"], 1_800_000);

    let (status, _) = post(&state, POLICY, PEER, json!({ "enabled": true }));
    assert_eq!(status, 403);
    let (status, v) = post(
        &state,
        POLICY,
        OWNER,
        json!({ "enabled": true, "pinned_personas": ["ferris"] }),
    );
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["enabled"], true);
    assert_eq!(v["policy"]["pinned_personas"], json!(["ferris"]));
    // A partial body is laid over the stored policy, not the defaults.
    let (_, v) = post(&state, POLICY, OWNER, json!({ "cooldown_ms": 5 }));
    assert_eq!(v["policy"]["cooldown_ms"], 5);
    assert_eq!(v["enabled"], true);
    let (_, v) = call(&state, "GET", BALANCE, OWNER, None, None);
    assert_eq!(v["policy"]["pinned_personas"], json!(["ferris"]));
    assert_eq!(v["policy"]["cooldown_ms"], 5);

    for bad in [
        json!({ "cooldown": 5 }),
        json!({ "primary_trigger_pct": 60 }),
        json!({ "max_switches_per_run": 0 }),
        json!({ "secondary_trigger_pct": "high" }),
        json!({ "allowed_target_vendors": ["codex"], "excluded_target_vendors": ["codex"] }),
        json!("enabled"),
    ] {
        let (status, v) = post(&state, POLICY, OWNER, bad.clone());
        assert_eq!(status, 400, "{bad}: {v}");
    }
    let resp = handle_http_request(
        &state,
        &HttpRequest::new("POST", POLICY)
            .with_header("Authorization", format!("Bearer {OWNER}"))
            .with_body(b"{nope".to_vec()),
    );
    assert_eq!(resp.status, 400);
    let (_, v) = call(&state, "GET", BALANCE, OWNER, None, None);
    assert_eq!(
        v["policy"]["cooldown_ms"], 5,
        "a refused body changes nothing"
    );
}

#[test]
fn methods_auth_and_query_keys_are_checked() {
    let tmp = TempDir::new().unwrap();
    let state = hub(&tmp);
    for path in [BALANCE, PLAN, POLICY, APPLY] {
        let anon = handle_http_request(&state, &HttpRequest::new("GET", path));
        assert_eq!(anon.status, 401, "{path}");
    }
    for (path, wrong, allow) in [
        (BALANCE, "POST", "GET"),
        (BALANCE, "HEAD", "GET"),
        (PLAN, "GET", "POST"),
        (POLICY, "PUT", "POST"),
        (APPLY, "GET", "POST"),
        ("/api/v1/watch/balance/apply/", "DELETE", "POST"),
    ] {
        let resp: HttpResponse = handle_http_request(
            &state,
            &HttpRequest::new(wrong, path).with_header("Authorization", format!("Bearer {PEER}")),
        );
        assert_eq!(resp.status, 405, "{wrong} {path}");
        let allowed: Vec<&str> = resp
            .headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case("allow"))
            .map(|(_, v)| v.as_str())
            .collect();
        assert_eq!(allowed, vec![allow], "{wrong} {path}");
    }
    let (status, v) = call(&state, "GET", &format!("{BALANCE}?x=1"), OWNER, None, None);
    assert_eq!(status, 400);
    assert!(
        v["error"].as_str().unwrap().contains("unknown query"),
        "{v}"
    );
    let (status, _) = call(&state, "POST", &format!("{PLAN}?dry=1"), OWNER, None, None);
    assert_eq!(status, 400);
    let (status, _) = call(
        &state,
        "GET",
        &format!("{BALANCE}?token=x"),
        OWNER,
        None,
        None,
    );
    assert_eq!(status, 200);
    // Not a balance path.
    let (status, _) = call(
        &state,
        "GET",
        "/api/v1/watch/balance/nope",
        OWNER,
        None,
        None,
    );
    assert_eq!(status, 404);
}

#[test]
fn an_owner_apply_moves_the_hot_agent_writes_an_audit_and_then_blocks() {
    let tmp = TempDir::new().unwrap();
    let state = hub(&tmp);
    hot_claude(&state);
    let mut frames = state.subscribe();

    let (status, out) = post(&state, APPLY, OWNER, json!({}));
    assert_eq!(status, 200, "{out}");
    assert_eq!(
        keys(&out),
        vec!["audit_slug", "plan", "results", "warnings"]
    );
    let r = &out["results"][0];
    assert_eq!(out["results"].as_array().unwrap().len(), 1);
    assert_eq!(r["status"], "ok", "{r}");
    assert_eq!(r["persona"], "ada");
    assert_eq!(r["to_vendor"], "muse");
    assert_eq!(r["new_session_id"], "muse:ada");
    assert_eq!(r["outcome"], "remodeled");
    assert_eq!(out["warnings"], json!([]));
    let mut running = live(&state);
    running.sort();
    assert_eq!(running, vec!["codex:bo", "muse:ada"]);

    // One audit, at the slug the response names.
    let all = audits(&state);
    assert_eq!(all.len(), 1);
    assert_eq!(out["audit_slug"], all[0].0.as_str());
    assert!(all[0].0.starts_with("watch-balance-audit-"));
    let audit = &all[0].1;
    assert_eq!(audit.by, "lmcfarlin");
    assert_eq!(audit.authority, "owner");
    assert_eq!(audit.warrant_grant_id, None);
    assert_eq!(audit.results[0].new_session_id.as_deref(), Some("muse:ada"));
    assert_eq!(audit.plan.switches.len(), 1);
    let history = read_history(&*state.open_store().unwrap()).unwrap();
    assert_eq!(history.last_apply_ms, Some(audit.applied_at_ms));
    assert!(history.persona_last_switch_ms.contains_key("ada"));

    // The remodel broadcast, then the apply's own.
    let mut events = Vec::new();
    while let Ok(f) = frames.try_recv() {
        events.push((f.channel, f.event, f.data));
    }
    let names: Vec<(&str, &str)> = events
        .iter()
        .map(|(c, e, _)| (c.as_str(), e.as_str()))
        .collect();
    assert_eq!(
        names,
        vec![
            ("sessions", "session.remodeled"),
            ("watch", "watch.balance.applied")
        ]
    );
    assert_eq!(events[1].2["audit_slug"], out["audit_slug"]);
    assert_eq!(events[1].2["authority"], "owner");

    // Inside `min_interval_ms` the next apply is a 409 that changes nothing, even with a new
    // hot agent.
    quota(&state, "codex", 99.0);
    let (status, v) = post(&state, APPLY, OWNER, json!({}));
    assert_eq!(status, 409, "{v}");
    assert!(
        v["error"].as_str().unwrap().contains("min_interval_ms"),
        "{v}"
    );
    assert_eq!(v["plan"]["switches"][0]["persona"], "bo");
    assert_eq!(audits(&state).len(), 1);
    assert!(live(&state).contains(&"codex:bo".to_string()));
}

#[test]
fn the_cooldown_keeps_a_moved_persona_where_it_is() {
    let tmp = TempDir::new().unwrap();
    let state = hub(&tmp);
    hot_claude(&state);
    set_policy(&state, json!({ "min_interval_ms": 0 }));
    let (status, out) = post(&state, APPLY, OWNER, json!({}));
    assert_eq!(status, 200, "{out}");
    assert_eq!(out["results"][0]["new_session_id"], "muse:ada");
    // Now muse runs hot too, but ada was moved moments ago.
    quota(&state, "muse", 99.0);
    let (status, out) = post(&state, APPLY, OWNER, json!({}));
    assert_eq!(status, 200, "{out}");
    assert_eq!(out["results"], json!([]));
    assert_eq!(out["audit_slug"], Value::Null);
    let skipped = out["plan"]["skipped"].as_array().unwrap();
    let ada = skipped.iter().find(|s| s["persona"] == "ada").unwrap();
    assert_eq!(ada["reason"], "cooldown");
    assert_eq!(
        audits(&state).len(),
        1,
        "an apply with nothing to do is not audited"
    );
    assert!(live(&state).contains(&"muse:ada".to_string()));
}

#[test]
fn a_peer_needs_a_warrant_for_itself_and_this_scope() {
    let tmp = TempDir::new().unwrap();
    let state = hub(&tmp);
    hot_claude(&state);
    let apply_as =
        |agent: Option<&str>, body: Value| call(&state, "POST", APPLY, PEER, agent, Some(&body));
    // No warrant.
    let (status, v) = apply_as(Some("felicity"), json!({}));
    assert_eq!(status, 403, "{v}");
    assert!(
        v["error"]
            .as_str()
            .unwrap()
            .contains(fl_watch::balance::APPLY_SCOPE)
    );
    // A warrant for another scope, another agent, or a forged id.
    let wrong_scope = issue(&state, "felicity", "agent.replace", false);
    let other_agent = issue(&state, "ferris", "watch.balance.apply", false);
    for body in [
        json!({ "warrant": wrong_scope["grant_id"] }),
        json!({ "warrant": other_agent["grant_id"] }),
        json!({ "warrant": other_agent["compact_token"] }),
        json!({ "warrant": "fgw-0000000000000000" }),
        json!({ "warrant": "" }),
    ] {
        let (status, v) = apply_as(Some("felicity"), body.clone());
        assert_eq!(status, 403, "{body}: {v}");
    }
    let (status, v) = apply_as(
        Some("felicity"),
        json!({ "warrant": wrong_scope["grant_id"] }),
    );
    assert!(
        v["error"].as_str().unwrap().contains("agent.replace"),
        "{status} {v}"
    );
    // A valid warrant for any agent (`*`) still needs the peer to name itself.
    let any = issue(&state, "*", "watch.balance.apply", false);
    let (status, v) = apply_as(None, json!({ "warrant": any["grant_id"] }));
    assert_eq!(status, 403, "{v}");
    assert!(v["error"].as_str().unwrap().contains("X-Fl-Agent"), "{v}");
    // Unknown body fields are refused.
    let (status, _) = apply_as(Some("felicity"), json!({ "warant": "x" }));
    assert_eq!(status, 400);
    // Nothing moved, nothing audited.
    assert_eq!(live(&state), vec!["claude:ada", "codex:bo"]);
    assert!(audits(&state).is_empty());
}

#[test]
fn a_peer_with_a_single_use_warrant_applies_once() {
    let tmp = TempDir::new().unwrap();
    let state = hub(&tmp);
    hot_claude(&state);
    set_policy(&state, json!({ "min_interval_ms": 0 }));
    let grant = issue(&state, "felicity", "watch.balance.apply", true);
    let body = json!({ "warrant": grant["compact_token"] });
    let (status, out) = call(&state, "POST", APPLY, PEER, Some("Felicity"), Some(&body));
    assert_eq!(status, 200, "{out}");
    assert_eq!(out["results"][0]["status"], "ok", "{out}");
    assert_eq!(out["results"][0]["new_session_id"], "muse:ada");
    let all = audits(&state);
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].1.authority, "warrant");
    assert!(all[0].1.by.eq_ignore_ascii_case("felicity"));
    assert_eq!(
        all[0].1.warrant_grant_id.as_deref(),
        grant["grant_id"].as_str()
    );
    // Spent: the same warrant fails the second time, by token or by id.
    quota(&state, "codex", 99.0);
    for body in [
        json!({ "warrant": grant["compact_token"] }),
        json!({ "warrant": grant["grant_id"] }),
    ] {
        let (status, v) = call(&state, "POST", APPLY, PEER, Some("felicity"), Some(&body));
        assert_eq!(status, 403, "{v}");
        assert!(v["error"].as_str().unwrap().contains("consumed"), "{v}");
    }
    assert!(live(&state).contains(&"codex:bo".to_string()));
    // A reusable warrant is not spent by an apply.
    let reusable = issue(&state, "felicity", "watch.balance.apply", false);
    let body = json!({ "warrant": reusable["grant_id"] });
    let (status, out) = call(&state, "POST", APPLY, PEER, Some("felicity"), Some(&body));
    assert_eq!(status, 200, "{out}");
    assert_eq!(out["results"][0]["persona"], "bo");
    let (status, _) = call(&state, "POST", PLAN, PEER, Some("felicity"), None);
    assert_eq!(status, 200);
    let ok = fl_gate::verify_owner_grant(
        Some(tmp.path()),
        reusable["grant_id"].as_str().unwrap(),
        Some("felicity"),
        false,
    );
    assert!(ok.is_ok(), "{ok:?}");
}

#[test]
fn a_failed_switch_is_recorded_and_a_down_persona_stops_the_apply() {
    let tmp = TempDir::new().unwrap();
    let state = hub(&tmp);
    spin_up(&state, "ada", "claude");
    spin_up(&state, "bo", "claude");
    quota(&state, "claude", 95.0);
    quota(&state, "muse", 10.0);
    set_policy(
        &state,
        json!({ "min_interval_ms": 0, "cooldown_ms": 0, "max_switches_per_run": 2 }),
    );

    // muse cannot start: each remodel is undone and the old session restored.
    lifecycle::FAIL_SPIN_UP_VENDORS.with_borrow_mut(|v| v.push("muse".into()));
    let (status, out) = post(&state, APPLY, OWNER, json!({}));
    assert_eq!(status, 200, "{out}");
    let results = out["results"].as_array().unwrap();
    assert_eq!(results.len(), 2, "{out}");
    for (r, persona) in results.iter().zip(["ada", "bo"]) {
        assert_eq!(r["persona"], persona);
        assert_eq!(r["status"], "failed", "{r}");
        assert_eq!(r["outcome"], "restored");
        assert_eq!(r["restored_session_id"], format!("claude:{persona}"));
        assert!(
            r["error"]
                .as_str()
                .unwrap()
                .contains("muse binary is missing"),
            "{r}"
        );
    }
    let audit = &audits(&state)[0].1;
    assert_eq!(audit.results[0].status, fl_watch::SwitchStatus::Failed);
    assert_eq!(live(&state), vec!["claude:ada", "claude:bo"]);

    // The restore fails too: ada is down, so bo is not touched.
    lifecycle::FAIL_SPIN_UP_VENDORS.with_borrow_mut(|v| v.push("claude".into()));
    let (status, out) = post(&state, APPLY, OWNER, json!({}));
    assert_eq!(status, 200, "{out}");
    let results = out["results"].as_array().unwrap();
    assert_eq!(results[0]["status"], "failed");
    assert_eq!(results[0]["outcome"], "down");
    assert_eq!(results[1]["persona"], "bo");
    assert_eq!(results[1]["status"], "not_run");
    assert!(results[1]["error"].as_str().unwrap().contains("down"));
    assert_eq!(live(&state), vec!["claude:bo"]);
    let all = audits(&state);
    assert_eq!(all.len(), 2);
    assert_eq!(all[1].1.results[1].status, fl_watch::SwitchStatus::NotRun);
    // ada's failed attempt counts as a move for its cooldown.
    let history = read_history(&*state.open_store().unwrap()).unwrap();
    assert!(history.persona_last_switch_ms.contains_key("ada"));
    lifecycle::FAIL_SPIN_UP_VENDORS.with_borrow_mut(Vec::clear);
}

/// Whether a single-use grant can still be spent (checked without spending it).
fn unspent(tmp: &TempDir, grant: &Value) -> bool {
    fl_gate::verify_owner_grant(
        Some(tmp.path()),
        grant["grant_id"].as_str().unwrap(),
        Some("felicity"),
        false,
    )
    .is_ok()
}

fn skipped<'a>(plan: &'a Value, session_id: &str) -> &'a Value {
    plan["skipped"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["session_id"] == session_id)
        .unwrap_or_else(|| panic!("{session_id} not skipped: {plan}"))
}

#[test]
fn no_metered_api_vendor_is_ever_a_target() {
    let tmp = TempDir::new().unwrap();
    let state = hub(&tmp);
    for p in ["ada", "bo", "cy"] {
        spin_up(&state, p, "claude");
    }
    // Every known subscription vendor is over the target maximums; with unknown targets
    // allowed, only the unread subscription vendors (antigravity, grok) may take an agent. The
    // registry's `*-api` vendors (`anthropic-api` sorts before `antigravity`) never do.
    quota(&state, "claude", 95.0);
    quota(&state, "codex", 80.0);
    quota(&state, "muse", 80.0);
    let policy = json!({ "allow_unknown_quota_targets": true, "max_switches_per_run": 10 });
    let (status, plan) = post(&state, PLAN, OWNER, json!({ "policy": policy }));
    assert_eq!(status, 200, "{plan}");
    let to: Vec<&str> = plan["switches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["to_vendor"].as_str().unwrap())
        .collect();
    assert_eq!(to, vec!["antigravity", "grok"], "{plan}");
    assert_eq!(skipped(&plan, "claude:cy")["reason"], "no_eligible_target");
    for v in plan["vendors"].as_array().unwrap() {
        if v["vendor"].as_str().unwrap().ends_with("-api") {
            assert_eq!(v["eligible"], false, "{v}");
            assert_eq!(v["candidate"], false, "{v}");
        }
    }
    // The policy can only narrow the candidates: naming an API vendor allows nothing.
    let policy = json!({
        "allow_unknown_quota_targets": true, "max_switches_per_run": 10,
        "allowed_target_vendors": ["anthropic-api", "openai-api"],
    });
    let (status, plan) = post(&state, PLAN, OWNER, json!({ "policy": policy }));
    assert_eq!(status, 200, "{plan}");
    assert_eq!(plan["switches"], json!([]), "{plan}");
    // An apply with the same policy moves no one onto an API vendor either.
    set_policy(
        &state,
        json!({ "allow_unknown_quota_targets": true, "max_switches_per_run": 10 }),
    );
    let (status, out) = post(&state, APPLY, OWNER, json!({}));
    assert_eq!(status, 200, "{out}");
    let mut running = live(&state);
    running.sort();
    assert_eq!(running, vec!["antigravity:ada", "claude:cy", "grok:bo"]);
}

#[test]
fn a_switch_its_remodel_would_refuse_is_skipped_and_the_next_agent_moves() {
    use crate::lifecycle::{Caller, FakeRuntime, SpinUpRequest};
    let tmp = TempDir::new().unwrap();
    let _trusted =
        crate::lifecycle::trusted_codex_home(tmp.path(), &std::env::current_dir().unwrap());
    let state = hub(&tmp);
    quota(&state, "claude", 95.0);
    quota(&state, "codex", 40.0);
    quota(&state, "muse", 10.0);
    // Runtime calls on, against a fake tmux.
    let fake = FakeRuntime::default();
    let env = fake.env(tmp.path());
    let store = state.open_store().unwrap();
    let up = |persona: &str, vendor: &str| {
        let req = SpinUpRequest {
            persona: persona.into(),
            project: "FerroLoop".into(),
            role: "assistant".into(),
            vendor: vendor.into(),
            ..SpinUpRequest::default()
        };
        lifecycle::spin_up(&*store, &env, &req, "owner", Caller::Owner).unwrap();
    };
    // ada's claude and codex sessions share the tmux session `Ada`.
    up("ada", "claude");
    up("ada", "codex");
    // bo's record names another tmux session than its own `Bo`.
    up("bo", "claude");
    let mut bo = lifecycle::list_hosted_sessions(&*store)
        .unwrap()
        .into_iter()
        .find(|r| r.session_id == "claude:bo")
        .unwrap();
    bo.tmux_target = Some("Other:claude".into());
    lifecycle::persist_hosted_session(&*store, &bo, "owner").unwrap();
    up("cy", "claude");
    fake.calls.borrow_mut().clear();

    let inputs = crate::balance_api::read_store_inputs(&store).unwrap();
    let (plan, targets) = crate::balance_api::checked_plan(
        &store,
        &env,
        &[],
        Caller::Owner,
        &inputs.policy,
        &inputs,
        &[],
        1_000,
    )
    .unwrap_or_else(|_| panic!("the plan failed"));
    drop(store);
    // One run slot: ada and bo are worse off by order, but their remodels would be refused.
    let moves: Vec<(&str, &str)> = plan
        .switches
        .iter()
        .map(|s| (s.session_id.as_str(), s.to_vendor.as_str()))
        .collect();
    assert_eq!(moves, vec![("claude:cy", "muse")], "{plan:#?}");
    for (id, why) in [
        ("claude:ada", "it also runs codex:ada"),
        ("claude:bo", "started another tmux session"),
    ] {
        let s = plan.skipped.iter().find(|s| s.session_id == id).unwrap();
        assert_eq!(s.reason, fl_watch::SkipReason::NotRemodelable, "{id}");
        assert!(s.detail.contains(why), "{id}: {}", s.detail);
        assert!(s.detail.contains("(409)"), "{id}: {}", s.detail);
    }
    assert_eq!(targets.keys().collect::<Vec<_>>(), vec!["claude:cy"]);
    assert!(
        fake.calls.borrow().is_empty(),
        "the precheck starts and stops nothing"
    );
}

#[test]
fn an_agent_that_fails_its_precheck_spends_no_grant_and_frees_its_slot() {
    let tmp = TempDir::new().unwrap();
    let state = hub(&tmp);
    // ada's worktree is gone: a remodel would refuse it (400) before touching anything.
    let wt = tmp.path().join("wt-ada");
    std::fs::create_dir(&wt).unwrap();
    let (status, v) = post(
        &state,
        "/api/v1/sessions/spin-up",
        OWNER,
        json!({
            "persona": "ada", "project": "FerroLoop", "role": "assistant", "vendor": "claude",
            "cwd": wt.display().to_string(),
        }),
    );
    assert_eq!(status, 200, "{v}");
    std::fs::remove_dir(&wt).unwrap();
    spin_up(&state, "bo", "codex");
    quota(&state, "claude", 95.0);
    quota(&state, "codex", 40.0);
    quota(&state, "muse", 10.0);
    let grant = issue(&state, "felicity", "watch.balance.apply", true);
    let body = json!({ "warrant": grant["grant_id"] });

    // The plan already says so, and agrees with the apply.
    let (status, plan) = call(&state, "POST", PLAN, PEER, Some("felicity"), None);
    assert_eq!(status, 200, "{plan}");
    assert_eq!(plan["switches"], json!([]), "{plan}");
    let ada = skipped(&plan, "claude:ada");
    assert_eq!(ada["reason"], "not_remodelable");
    assert!(
        ada["detail"].as_str().unwrap().contains("not a directory"),
        "{ada}"
    );

    let (status, out) = call(&state, "POST", APPLY, PEER, Some("felicity"), Some(&body));
    assert_eq!(status, 200, "{out}");
    assert_eq!(out["results"], json!([]), "{out}");
    assert_eq!(
        skipped(&out["plan"], "claude:ada")["reason"],
        "not_remodelable"
    );
    assert!(
        unspent(&tmp, &grant),
        "no switch passed its precheck: the grant is kept"
    );
    assert!(audits(&state).is_empty());
    assert_eq!(
        read_history(&*state.open_store().unwrap()).unwrap(),
        fl_watch::BalanceHistory::default(),
        "no cooldown and no last apply for a switch that never ran"
    );

    // cy, also on hot claude, takes the run slot ada cannot use; the grant is spent now.
    spin_up(&state, "cy", "claude");
    let (status, out) = call(&state, "POST", APPLY, PEER, Some("felicity"), Some(&body));
    assert_eq!(status, 200, "{out}");
    let results = out["results"].as_array().unwrap();
    assert_eq!(results.len(), 1, "{out}");
    assert_eq!(results[0]["session_id"], "claude:cy");
    assert_eq!(results[0]["status"], "ok", "{out}");
    assert_eq!(
        skipped(&out["plan"], "claude:ada")["reason"],
        "not_remodelable"
    );
    assert!(!unspent(&tmp, &grant));
    let mut running = live(&state);
    running.sort();
    assert_eq!(running, vec!["claude:ada", "codex:bo", "muse:cy"]);
}

#[test]
fn a_blocked_or_empty_apply_leaves_a_single_use_grant_unspent() {
    let tmp = TempDir::new().unwrap();
    let state = hub(&tmp);
    spin_up(&state, "ada", "claude");
    spin_up(&state, "bo", "codex");
    quota(&state, "claude", 20.0);
    quota(&state, "codex", 40.0);
    quota(&state, "muse", 10.0);
    let grant = issue(&state, "felicity", "watch.balance.apply", true);
    let body = json!({ "warrant": grant["compact_token"] });
    let apply = || call(&state, "POST", APPLY, PEER, Some("felicity"), Some(&body));

    // Nobody runs hot: nothing to switch.
    let (status, out) = apply();
    assert_eq!(status, 200, "{out}");
    assert_eq!(out["results"], json!([]));
    assert!(unspent(&tmp, &grant), "an empty apply spends nothing");

    // The Owner moves ada; inside `min_interval_ms` the peer's apply is blocked.
    quota(&state, "claude", 95.0);
    let (status, out) = post(&state, APPLY, OWNER, json!({}));
    assert_eq!(status, 200, "{out}");
    quota(&state, "codex", 99.0);
    let (status, out) = apply();
    assert_eq!(status, 409, "{out}");
    assert!(unspent(&tmp, &grant), "a blocked apply spends nothing");

    // Once it may run, the grant is spent by the apply that switches.
    set_policy(&state, json!({ "min_interval_ms": 0 }));
    let (status, out) = apply();
    assert_eq!(status, 200, "{out}");
    assert_eq!(out["results"][0]["persona"], "bo", "{out}");
    assert!(!unspent(&tmp, &grant));
}
