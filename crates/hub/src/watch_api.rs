//! `/api/v1/watch/*`: FerroWatch (`fl-watch`) over the hub's store.
//!
//! - `GET /api/v1/watch/summary`: `{ telemetry, health, quotas, alerts, thresholds }`.
//! - `GET /api/v1/watch/alerts`: `{ alerts, thresholds }`.
//! - `GET /api/v1/watch/rollups[?project=<slug_or_id>]`: `{ telemetry }`.
//!
//! `summary` and `alerts` take the threshold overrides `warn_quota_pct`, `critical_quota_pct`,
//! `warn_cost_usd_micros` and `critical_cost_usd_micros`; a value that is not a number, is
//! negative, is a percentage over 100, or puts a warning above its critical level is a 400.
//!
//! **Auth.** These report spend, so they take the rule `/api/v1/stats` and the quota-bearing
//! session endpoints (`/api/v1/sessions`, `/api/v1/sessions/:id/transcript`) already take: any
//! authenticated caller (owner or peer); the general `/api/v1` gate refuses everyone else
//! before [`route`] runs. Those endpoints apply the same rule, so there is no stricter one to
//! pick.
//!
//! **Quota.** The vendor quota comes from the discovered sessions, read through the cached
//! discovery path: the shared store snapshot ([`HubState::cached_discovery_snapshot`]) and the
//! shared route scan (`discover_route_sessions_without_model_info`, served from the 2.5 s route
//! cache), then each route session's quota (`quota_for_resolved_route`, whose account probes
//! are cached for 60 s). That is what `/api/v1/sessions` reports as `quota_usage`, without the
//! model lookups. No request runs a host scan of its own.

use std::collections::BTreeMap;
use std::path::Path;

use fl_watch::{
    QuotaWindowKind, WatchError, WatchQuotaWindow, WatchThresholds, build_summary, build_telemetry,
    resolve_project,
};
use serde_json::json;

use crate::auth::HubAuthority;
use crate::discovery::{
    SessionQuotaUsage, discover_route_sessions_without_model_info, quota_for_resolved_route,
};
use crate::{HttpRequest, HttpResponse, HubState};

const SUMMARY: &str = "/api/v1/watch/summary";
const ALERTS: &str = "/api/v1/watch/alerts";
const ROLLUPS: &str = "/api/v1/watch/rollups";

/// The response for a `/api/v1/watch/*` request, or `None` for any other request.
pub(crate) fn route(
    state: &HubState,
    _authority: &HubAuthority,
    req: &HttpRequest,
) -> Option<HttpResponse> {
    if req.method != "GET" {
        return None;
    }
    match req.path.trim_end_matches('/') {
        SUMMARY => Some(handle_summary(state, req, true)),
        ALERTS => Some(handle_summary(state, req, false)),
        ROLLUPS => Some(handle_rollups(state, req)),
        _ => None,
    }
}

fn error(status: u16, message: impl Into<String>) -> HttpResponse {
    HttpResponse::json(status, json!({ "error": message.into() }))
}

fn watch_error(e: WatchError) -> HttpResponse {
    match e {
        WatchError::InvalidThreshold(_) => error(400, e.to_string()),
        WatchError::UnknownProject(_) => error(404, e.to_string()),
        WatchError::Store(_) => error(500, e.to_string()),
    }
}

/// The defaults, with each query override given replacing one, validated.
pub(crate) fn thresholds_from_query(
    query: &BTreeMap<String, String>,
) -> Result<WatchThresholds, String> {
    let mut t = WatchThresholds::default();
    let pct = |key: &str| -> Result<Option<f64>, String> {
        query
            .get(key)
            .map(|raw| {
                raw.trim()
                    .parse::<f64>()
                    .map_err(|_| format!("`{key}` must be a number, not `{raw}`"))
            })
            .transpose()
    };
    let micros = |key: &str| -> Result<Option<u64>, String> {
        query
            .get(key)
            .map(|raw| {
                raw.trim().parse::<u64>().map_err(|_| {
                    format!("`{key}` must be a non-negative whole number of micro-USD, not `{raw}`")
                })
            })
            .transpose()
    };
    if let Some(v) = pct("warn_quota_pct")? {
        t.warn_quota_pct = v;
    }
    if let Some(v) = pct("critical_quota_pct")? {
        t.critical_quota_pct = v;
    }
    if let Some(v) = micros("warn_cost_usd_micros")? {
        t.warn_cost_usd_micros = v;
    }
    if let Some(v) = micros("critical_cost_usd_micros")? {
        t.critical_cost_usd_micros = v;
    }
    t.validate().map_err(|e| e.to_string())?;
    Ok(t)
}

/// The windows of one discovered session's quota. A vendor that reports its allowance exhausted
/// without a window is a primary window at 100%, carrying the vendor's reason.
pub(crate) fn quota_windows(
    vendor: &str,
    session: &str,
    q: &SessionQuotaUsage,
) -> Vec<WatchQuotaWindow> {
    let mut out = Vec::new();
    for (kind, w) in [
        (QuotaWindowKind::Primary, &q.primary_window),
        (QuotaWindowKind::Secondary, &q.secondary_window),
    ] {
        if let Some(w) = w {
            out.push(WatchQuotaWindow {
                vendor: vendor.to_string(),
                window: kind,
                used_percent: w.used_percent,
                resets_at_ms: w.resets_at_ms,
                session: Some(session.to_string()),
                window_minutes: w.window_minutes,
                exhausted_reason: q.exhausted_reason.clone(),
            });
        }
    }
    if out.is_empty()
        && let Some(reason) = &q.exhausted_reason
    {
        out.push(WatchQuotaWindow {
            vendor: vendor.to_string(),
            window: QuotaWindowKind::Primary,
            used_percent: 100.0,
            resets_at_ms: None,
            session: Some(session.to_string()),
            window_minutes: None,
            exhausted_reason: Some(reason.clone()),
        });
    }
    out
}

/// The quota of every discovered route session, through the cached discovery path. Must run
/// with no store guard held: a snapshot miss opens the store itself.
fn discovered_quotas(state: &HubState) -> Vec<WatchQuotaWindow> {
    let Ok(snap) = state.cached_discovery_snapshot() else {
        return Vec::new();
    };
    let home = state.config.home_override.as_deref();
    let mut out = Vec::new();
    for s in discover_route_sessions_without_model_info(&snap, home) {
        if s.source != "route" {
            continue;
        }
        if let Some(q) = quota_for_resolved_route(&s.adapter, s.transcript_path.as_deref(), home) {
            out.extend(quota_windows(&s.adapter, &s.id, &q));
        }
    }
    out
}

fn pid_is_alive(pid: u32) -> bool {
    pid != 0 && Path::new(&format!("/proc/{pid}")).exists()
}

/// `summary` (`full`) or `alerts`.
fn handle_summary(state: &HubState, req: &HttpRequest, full: bool) -> HttpResponse {
    let thresholds = match thresholds_from_query(&req.query) {
        Ok(t) => t,
        Err(e) => return error(400, e),
    };
    // Before the store guard: discovery may open the store for its snapshot.
    let quotas = discovered_quotas(state);
    let now_ms = fl_exec::stamp::now().unix_millis();
    let host = fl_exec::adapters::harness::detect_host();
    let Ok(store) = state.open_store() else {
        return error(500, "failed to open store: store unavailable");
    };
    let summary = match build_summary(
        &store,
        None,
        &thresholds,
        quotas,
        now_ms,
        &host,
        &pid_is_alive,
    ) {
        Ok(s) => s,
        Err(e) => return watch_error(e),
    };
    drop(store);
    if full {
        HttpResponse::json(
            200,
            json!({
                "telemetry": summary.telemetry,
                "health": summary.health,
                "quotas": summary.quotas,
                "alerts": summary.alerts,
                "thresholds": summary.thresholds,
            }),
        )
    } else {
        HttpResponse::json(
            200,
            json!({ "alerts": summary.alerts, "thresholds": summary.thresholds }),
        )
    }
}

fn handle_rollups(state: &HubState, req: &HttpRequest) -> HttpResponse {
    let Ok(store) = state.open_store() else {
        return error(500, "failed to open store: store unavailable");
    };
    let project = match req.query.get("project").map(|p| p.trim()) {
        None | Some("") => None,
        Some(raw) => match resolve_project(&*store, raw) {
            Ok(p) => Some(p),
            Err(e) => return watch_error(e),
        },
    };
    match build_telemetry(&store, project.as_ref()) {
        Ok(t) => HttpResponse::json(200, json!({ "telemetry": t })),
        Err(e) => watch_error(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::QuotaWindowUsage;
    use crate::discovery::cost_counters::measure;
    use crate::handle_http_request;
    use crate::latency_tests::hub_with_ferris;
    use serde_json::Value;
    use tempfile::TempDir;

    const OWNER: &str = "Bearer owner-tok-secret";
    const PEER: &str = "Bearer peer-tok-secret";

    fn get_as(state: &HubState, path: &str, auth: Option<&str>) -> HttpResponse {
        let mut req = HttpRequest::new("GET", path);
        if let Some(a) = auth {
            req = req.with_header("Authorization", a);
        }
        handle_http_request(state, &req)
    }

    fn get(state: &HubState, path: &str) -> (u16, Value) {
        let resp = get_as(state, path, Some(OWNER));
        let v = resp.json_value().unwrap();
        (resp.status, v)
    }

    fn keys(v: &Value) -> Vec<&str> {
        let mut k: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        k.sort_unstable();
        k
    }

    fn long_ttl(state: &mut HubState) {
        state.discovery_snapshots = std::sync::Arc::new(crate::discovery::TtlCache::new(
            std::time::Duration::from_secs(3600),
        ));
    }

    #[test]
    fn each_endpoint_answers_with_its_shape() {
        let tmp = TempDir::new().unwrap();
        let state = hub_with_ferris(&tmp, 2);

        let (status, v) = get(&state, SUMMARY);
        assert_eq!(status, 200, "{v}");
        assert_eq!(
            keys(&v),
            vec!["alerts", "health", "quotas", "telemetry", "thresholds"]
        );
        assert_eq!(v["telemetry"]["totals"]["attempts"], 3);
        assert_eq!(v["telemetry"]["totals"]["crashed"], 1);
        assert_eq!(v["telemetry"]["by_project"].as_array().unwrap().len(), 2);
        assert!(v["health"]["agents"].is_array());
        assert!(v["quotas"].is_array());
        assert_eq!(v["thresholds"]["warn_quota_pct"], 80.0);
        assert_eq!(v["thresholds"]["critical_cost_usd_micros"], 500_000_000u64);

        let (status, v) = get(&state, ALERTS);
        assert_eq!(status, 200);
        assert_eq!(keys(&v), vec!["alerts", "thresholds"]);
        assert_eq!(
            v["alerts"],
            json!([]),
            "nothing in the fixture crosses a default"
        );

        let (status, v) = get(&state, ROLLUPS);
        assert_eq!(status, 200);
        assert_eq!(keys(&v), vec!["telemetry"]);
        assert_eq!(v["telemetry"]["project"], Value::Null);
        assert_eq!(v["telemetry"]["totals"]["attempts"], 3);
    }

    #[test]
    fn rollups_take_a_project_slug_or_id_and_404_an_unknown_one() {
        let tmp = TempDir::new().unwrap();
        let state = hub_with_ferris(&tmp, 2);
        let (status, v) = get(&state, "/api/v1/watch/rollups?project=one");
        assert_eq!(status, 200, "{v}");
        let id = v["telemetry"]["project"].as_str().unwrap().to_string();
        assert_eq!(v["telemetry"]["totals"]["attempts"], 2);
        assert_eq!(v["telemetry"]["totals"]["failed"], 1);
        let (status, by_id) = get(
            &state,
            &format!("/api/v1/watch/rollups?project={}", id.replace(':', "%3A")),
        );
        assert_eq!(status, 200);
        assert_eq!(by_id["telemetry"]["project"], id.as_str());
        let (status, v) = get(&state, "/api/v1/watch/rollups?project=nope");
        assert_eq!(status, 404, "{v}");
        assert!(v["error"].as_str().unwrap().contains("nope"));
    }

    #[test]
    fn query_overrides_move_the_thresholds() {
        let tmp = TempDir::new().unwrap();
        let state = hub_with_ferris(&tmp, 2);
        // The fixture spends nothing, so a zero critical line puts every project over it.
        let (status, v) = get(
            &state,
            "/api/v1/watch/alerts?warn_cost_usd_micros=0&critical_cost_usd_micros=0\
             &warn_quota_pct=10&critical_quota_pct=20.5",
        );
        assert_eq!(status, 200, "{v}");
        assert_eq!(v["thresholds"]["warn_cost_usd_micros"], 0);
        assert_eq!(v["thresholds"]["critical_quota_pct"], 20.5);
        let ids: Vec<&str> = v["alerts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids.len(), 3, "{ids:?}");
        assert!(ids[0].starts_with("spend:project:"));
        assert!(ids[1].starts_with("spend:project:"));
        assert_eq!(ids[2], "spend:workspace");
        assert!(
            v["alerts"]
                .as_array()
                .unwrap()
                .iter()
                .all(|a| a["severity"] == "critical")
        );
        // The summary takes the same overrides.
        let (status, v) = get(
            &state,
            "/api/v1/watch/summary?critical_cost_usd_micros=0&warn_cost_usd_micros=0",
        );
        assert_eq!(status, 200);
        assert_eq!(v["alerts"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn bad_threshold_values_are_400() {
        let tmp = TempDir::new().unwrap();
        let state = hub_with_ferris(&tmp, 2);
        for q in [
            "warn_quota_pct=NaN",
            "critical_quota_pct=nan",
            "warn_quota_pct=inf",
            "warn_quota_pct=-1",
            "critical_quota_pct=100.01",
            "warn_quota_pct=abc",
            "warn_quota_pct=96",
            "warn_quota_pct=50&critical_quota_pct=40",
            "warn_cost_usd_micros=-5",
            "critical_cost_usd_micros=1.5",
            "warn_cost_usd_micros=10&critical_cost_usd_micros=5",
        ] {
            for path in [ALERTS, SUMMARY] {
                let (status, v) = get(&state, &format!("{path}?{q}"));
                assert_eq!(status, 400, "{path}?{q}: {v}");
                assert!(v["error"].is_string(), "{path}?{q}");
            }
        }
    }

    #[test]
    fn auth_is_the_stats_rule_any_authenticated_caller() {
        let tmp = TempDir::new().unwrap();
        let state = hub_with_ferris(&tmp, 2);
        for path in [SUMMARY, ALERTS, ROLLUPS] {
            let stats = |auth| get_as(&state, "/api/v1/stats", auth).status;
            for auth in [None, Some("Bearer wrong"), Some(PEER), Some(OWNER)] {
                let status = get_as(&state, path, auth).status;
                assert_eq!(status, stats(auth), "{path} with {auth:?}");
            }
            assert_eq!(get_as(&state, path, None).status, 401);
            assert_eq!(get_as(&state, path, Some(PEER)).status, 200);
        }
        // Only GET is served here.
        let resp = handle_http_request(
            &state,
            &HttpRequest::new("POST", SUMMARY).with_header("Authorization", OWNER),
        );
        assert_ne!(resp.status, 200);
    }

    #[test]
    fn quota_reads_the_cached_discovery_path_and_rollups_skip_discovery() {
        let tmp = TempDir::new().unwrap();
        let mut state = hub_with_ferris(&tmp, 3);
        long_ttl(&mut state);
        // Warm the shared store snapshot the way any discovery endpoint does.
        let (resp, cold) = measure(|| get_as(&state, "/api/v1/sessions", Some(OWNER)));
        assert_eq!(resp.status, 200);
        assert_eq!(cold.store_snapshots, 1);
        for path in [SUMMARY, ALERTS] {
            let (resp, warm) = measure(|| get_as(&state, path, Some(OWNER)));
            assert_eq!(resp.status, 200, "{path}");
            assert_eq!(
                warm.store_snapshots, 0,
                "{path} reuses the discovery snapshot"
            );
            // One route-table read: with a `Some(home)` override it is a scan of that temp
            // home; in production the same call is served by the shared 2.5 s route cache.
            assert_eq!(warm.route_scans, 1, "{path}");
            // No model lookup: no transcript is parsed for turns.
            assert_eq!(warm.transcript_parses, 0, "{path}");
        }
        let (resp, costs) = measure(|| get_as(&state, ROLLUPS, Some(OWNER)));
        assert_eq!(resp.status, 200);
        assert_eq!(costs.route_scans, 0, "rollups never touch discovery");
        assert_eq!(costs.store_snapshots, 0);
    }

    #[test]
    fn quota_windows_map_both_windows_and_an_exhausted_vendor() {
        let both = SessionQuotaUsage {
            tier: Some("pro".into()),
            primary_window: Some(QuotaWindowUsage {
                used_percent: 85.5,
                window_minutes: Some(300),
                resets_at_ms: Some(10),
            }),
            secondary_window: Some(QuotaWindowUsage {
                used_percent: 40.0,
                window_minutes: Some(10_080),
                resets_at_ms: Some(20),
            }),
            exhausted_reason: None,
        };
        let w = quota_windows("codex", "thread-1", &both);
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].window, QuotaWindowKind::Primary);
        assert_eq!(w[0].used_percent, 85.5);
        assert_eq!(w[0].resets_at_ms, Some(10));
        assert_eq!(w[0].session.as_deref(), Some("thread-1"));
        assert_eq!(w[1].window, QuotaWindowKind::Secondary);
        assert_eq!(w[1].window_minutes, Some(10_080));

        let exhausted = SessionQuotaUsage {
            tier: None,
            primary_window: None,
            secondary_window: None,
            exhausted_reason: Some("rate limited".into()),
        };
        let w = quota_windows("claude", "s", &exhausted);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].used_percent, 100.0);
        assert_eq!(w[0].exhausted_reason.as_deref(), Some("rate limited"));

        let empty = SessionQuotaUsage {
            tier: None,
            primary_window: None,
            secondary_window: None,
            exhausted_reason: None,
        };
        assert!(quota_windows("grok", "s", &empty).is_empty());
    }
}
