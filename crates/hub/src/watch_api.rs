//! `/api/v1/watch/*`: FerroWatch (`fl-watch`) over the hub's store.
//!
//! - `GET /api/v1/watch/summary`: `{ telemetry, health, quotas, alerts, thresholds }`.
//! - `GET /api/v1/watch/alerts`: `{ alerts, thresholds }`.
//! - `GET /api/v1/watch/rollups[?project=<slug_or_id>]`: `{ telemetry }`.
//!
//! `summary` and `alerts` take the threshold overrides `warn_quota_pct`, `critical_quota_pct`,
//! `warn_cost_usd_micros`, `critical_cost_usd_micros` and `stale_after_ms`; a value that is not
//! a number, is negative, is a percentage over 100, puts a warning above its critical level, or
//! is a zero `stale_after_ms` is a 400. A query key an endpoint does not take is a 400 that
//! lists the keys it does take (`token`, the query form of the bearer token, is always taken).
//! Any method but `GET` on one of these paths is a 405 with `Allow: GET`.
//!
//! **Auth.** These report spend, so they take the rule `/api/v1/stats` and the quota-bearing
//! session endpoints (`/api/v1/sessions`, `/api/v1/sessions/:id/transcript`) already take: any
//! authenticated caller (owner or peer); the general `/api/v1` gate refuses everyone else
//! before [`route`] runs, so a 405 is only ever sent to an authenticated caller.
//!
//! **Cache.** The threshold- and clock-independent part of a response is read once and shared
//! for [`WATCH_SNAPSHOT_TTL`] (2 s, the discovery snapshot's TTL): for `summary` and `alerts`
//! the store read ([`fl_watch::WatchStoreSnapshot`]: telemetry, heartbeats, hosted sessions,
//! stored quotas, project labels) and the discovered quotas, keyed by the store path; for
//! `rollups` the telemetry, keyed by the store path and `?project`. Health and alerts are
//! evaluated from it per request ([`fl_watch::summarize`], pure but for a `/proc` check), so
//! each request's thresholds and clock apply. One load at a time; every caller gets a shared,
//! read-only copy. Any request other than a `GET` or `HEAD` drops both caches, as it drops the
//! discovery snapshot ([`crate::api::handle_http_request`]). A cold `summary` makes `3P + G + 2`
//! store reads (P projects, G gates) and a warm one none.
//!
//! **Quota.** The vendor quota comes from the discovered sessions, read through the cached
//! discovery path: the shared store snapshot ([`HubState::cached_discovery_snapshot`]) and the
//! shared route scan (`discover_route_sessions_without_model_info`, served from the 2.5 s route
//! cache), then each route session's quota (`quota_for_resolved_route`, whose account probes
//! are cached for 60 s). That is what `/api/v1/sessions` reports as `quota_usage`, without the
//! model lookups. It runs only when the watch cache is cold, so the transcript reads behind it
//! happen at most once per TTL. A vendor that reports its allowance exhausted or rate limited
//! is a critical `quota:<vendor>:exhausted` alert whatever its windows read
//! ([`fl_watch::evaluate_watch_alerts`]).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use fl_watch::{
    QuotaWindowKind, WatchError, WatchQuotaWindow, WatchStoreSnapshot, WatchTelemetryReport,
    WatchThresholds, build_telemetry, read_store_snapshot, resolve_project, summarize,
};
use serde_json::json;

use crate::auth::HubAuthority;
use crate::discovery::{
    SessionQuotaUsage, TtlCache, discover_route_sessions_without_model_info,
    quota_for_resolved_route,
};
use crate::{DISCOVERY_SNAPSHOT_TTL, HttpRequest, HttpResponse, HubState};

const SUMMARY: &str = "/api/v1/watch/summary";
const ALERTS: &str = "/api/v1/watch/alerts";
const ROLLUPS: &str = "/api/v1/watch/rollups";

/// The query keys `summary` and `alerts` take.
const THRESHOLD_KEYS: &[&str] = &[
    "warn_quota_pct",
    "critical_quota_pct",
    "warn_cost_usd_micros",
    "critical_cost_usd_micros",
    "stale_after_ms",
];
/// The query keys `rollups` takes.
const ROLLUPS_KEYS: &[&str] = &["project"];
/// The query form of the bearer token, which the auth gate reads on every endpoint.
const TOKEN_KEY: &str = "token";

/// How long the watch endpoints reuse one store read: the discovery snapshot's TTL.
pub(crate) const WATCH_SNAPSHOT_TTL: Duration = DISCOVERY_SNAPSHOT_TTL;

/// What `summary` and `alerts` share for [`WATCH_SNAPSHOT_TTL`].
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct WatchSnapshot {
    pub(crate) store: WatchStoreSnapshot,
    /// The discovered sessions' quota windows.
    pub(crate) quotas: Vec<WatchQuotaWindow>,
}

/// The `rollups` cache key: the store path and the trimmed `?project` (`None` when absent or
/// empty).
type RollupsKey = (PathBuf, Option<String>);

/// The watch endpoints' caches (see the module docs). One entry each: a different key replaces
/// the entry.
pub(crate) struct WatchCaches {
    summary: TtlCache<PathBuf, Arc<WatchSnapshot>>,
    rollups: TtlCache<RollupsKey, Arc<WatchTelemetryReport>>,
}

impl std::fmt::Debug for WatchCaches {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WatchCaches")
            .field("summary", &self.summary)
            .field("rollups", &self.rollups)
            .finish()
    }
}

impl Default for WatchCaches {
    fn default() -> Self {
        Self::new(WATCH_SNAPSHOT_TTL)
    }
}

impl WatchCaches {
    pub(crate) const fn new(ttl: Duration) -> Self {
        Self {
            summary: TtlCache::new(ttl),
            rollups: TtlCache::new(ttl),
        }
    }

    /// Drop both entries; a load already running is not served either.
    pub(crate) fn invalidate(&self) {
        self.summary.invalidate();
        self.rollups.invalidate();
    }
}

/// The response for a `/api/v1/watch/*` request, or `None` for any other request.
pub(crate) fn route(
    state: &HubState,
    _authority: &HubAuthority,
    req: &HttpRequest,
) -> Option<HttpResponse> {
    let path = req.path.trim_end_matches('/');
    if ![SUMMARY, ALERTS, ROLLUPS].contains(&path) {
        return None;
    }
    if req.method != "GET" {
        return Some(error(405, format!("{path} takes only GET")).with_header("Allow", "GET"));
    }
    Some(match path {
        SUMMARY => handle_summary(state, req, true, Instant::now),
        ALERTS => handle_summary(state, req, false, Instant::now),
        _ => handle_rollups(state, req, Instant::now),
    })
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

/// Refuse a query key outside `allowed` (and [`TOKEN_KEY`]), naming the keys that are taken.
pub(crate) fn refuse_unknown_keys(
    query: &BTreeMap<String, String>,
    allowed: &[&str],
) -> Result<(), String> {
    let unknown: Vec<&str> = query
        .keys()
        .map(String::as_str)
        .filter(|k| *k != TOKEN_KEY && !allowed.contains(k))
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    let list = |keys: &[&str]| {
        keys.iter()
            .map(|k| format!("`{k}`"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let taken = if allowed.is_empty() {
        "none".to_string()
    } else {
        list(allowed)
    };
    Err(format!(
        "unknown query parameter{} {}; this endpoint takes: {taken}",
        if unknown.len() == 1 { "" } else { "s" },
        list(&unknown)
    ))
}

/// The defaults, with each query override given replacing one, validated as the CLI flags are
/// ([`WatchThresholds::validate`]: a zero `stale_after_ms` is refused, as `fl watch` refuses it).
pub(crate) fn thresholds_from_query(
    query: &BTreeMap<String, String>,
) -> Result<WatchThresholds, String> {
    refuse_unknown_keys(query, THRESHOLD_KEYS)?;
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
    let whole = |key: &str, unit: &str| -> Result<Option<u64>, String> {
        query
            .get(key)
            .map(|raw| {
                raw.trim().parse::<u64>().map_err(|_| {
                    format!("`{key}` must be a non-negative whole number of {unit}, not `{raw}`")
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
    if let Some(v) = whole("warn_cost_usd_micros", "micro-USD")? {
        t.warn_cost_usd_micros = v;
    }
    if let Some(v) = whole("critical_cost_usd_micros", "micro-USD")? {
        t.critical_cost_usd_micros = v;
    }
    if let Some(v) = whole("stale_after_ms", "milliseconds")? {
        t.stale_after_ms = v;
    }
    t.validate().map_err(|e| e.to_string())?;
    Ok(t)
}

/// The windows of one discovered session's quota. A vendor that reports its allowance exhausted
/// without a window is a primary window at 100%, carrying the vendor's reason; every window
/// carries the reason, and the alert rule makes any reading with a reason critical
/// (`quota:<vendor>:exhausted`) whatever its percent.
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
pub(crate) fn discovered_quotas(state: &HubState) -> Vec<WatchQuotaWindow> {
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

/// The shared `summary`/`alerts` snapshot: cached for [`WATCH_SNAPSHOT_TTL`] on `clock`, else
/// the discovered quotas (first: discovery may open the store for its own snapshot) and one
/// store read.
pub(crate) fn watch_snapshot(
    state: &HubState,
    clock: impl Fn() -> Instant,
) -> Result<Arc<WatchSnapshot>, HttpResponse> {
    state
        .watch_caches
        .summary
        .get_or_load(state.config.store_path.clone(), clock, || {
            let quotas = discovered_quotas(state);
            let store = state
                .open_store()
                .map_err(|_| error(500, "failed to open store: store unavailable"))?;
            let read = read_store_snapshot(&store, None).map_err(watch_error)?;
            drop(store);
            Ok(Arc::new(WatchSnapshot {
                store: read,
                quotas,
            }))
        })
}

/// The `rollups` telemetry for `project` (the raw `?project`), cached for
/// [`WATCH_SNAPSHOT_TTL`] on `clock`. An unknown project is a 404 and is not cached.
pub(crate) fn rollups_report(
    state: &HubState,
    project: Option<&str>,
    clock: impl Fn() -> Instant,
) -> Result<Arc<WatchTelemetryReport>, HttpResponse> {
    let key = (state.config.store_path.clone(), project.map(str::to_string));
    state.watch_caches.rollups.get_or_load(key, clock, || {
        let store = state
            .open_store()
            .map_err(|_| error(500, "failed to open store: store unavailable"))?;
        let project = project
            .map(|raw| resolve_project(&*store, raw))
            .transpose()
            .map_err(watch_error)?;
        build_telemetry(&store, project.as_ref())
            .map(Arc::new)
            .map_err(watch_error)
    })
}

/// `summary` (`full`) or `alerts`.
fn handle_summary(
    state: &HubState,
    req: &HttpRequest,
    full: bool,
    clock: impl Fn() -> Instant,
) -> HttpResponse {
    let thresholds = match thresholds_from_query(&req.query) {
        Ok(t) => t,
        Err(e) => return error(400, e),
    };
    let snap = match watch_snapshot(state, clock) {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let now_ms = fl_exec::stamp::now().unix_millis();
    let host = fl_exec::adapters::harness::detect_host();
    let summary = match summarize(
        &snap.store,
        &thresholds,
        snap.quotas.clone(),
        now_ms,
        &host,
        &pid_is_alive,
    ) {
        Ok(s) => s,
        Err(e) => return watch_error(e),
    };
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

fn handle_rollups(
    state: &HubState,
    req: &HttpRequest,
    clock: impl Fn() -> Instant,
) -> HttpResponse {
    if let Err(e) = refuse_unknown_keys(&req.query, ROLLUPS_KEYS) {
        return error(400, e);
    }
    let project = req
        .query
        .get("project")
        .map(|p| p.trim())
        .filter(|p| !p.is_empty());
    match rollups_report(state, project, clock) {
        Ok(t) => HttpResponse::json(200, json!({ "telemetry": &*t })),
        Err(resp) => resp,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::QuotaWindowUsage;
    use crate::discovery::cost_counters::{Costs, measure};
    use crate::handle_http_request;
    use crate::latency_tests::hub_with_ferris;
    use fl_core::store::{Catalog, Ledger};
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
        // Only GET is served here: any other method on a known path is a 405 naming GET,
        // after the same auth gate (no token is a 401, not a 405).
        for method in ["POST", "PUT", "DELETE", "PATCH", "HEAD"] {
            for path in [SUMMARY, ALERTS, ROLLUPS, "/api/v1/watch/summary/"] {
                let anon = handle_http_request(&state, &HttpRequest::new(method, path));
                assert_eq!(anon.status, 401, "{method} {path} without auth");
                for auth in [OWNER, PEER] {
                    let resp = handle_http_request(
                        &state,
                        &HttpRequest::new(method, path).with_header("Authorization", auth),
                    );
                    assert_eq!(resp.status, 405, "{method} {path}");
                    let allow: Vec<&(String, String)> = resp
                        .headers
                        .iter()
                        .filter(|(k, _)| k.eq_ignore_ascii_case("allow"))
                        .collect();
                    assert_eq!(allow.len(), 1, "{method} {path}: {:?}", resp.headers);
                    assert_eq!(allow[0].1, "GET");
                }
            }
        }
        // An unknown watch path is not claimed here.
        let resp = get_as(&state, "/api/v1/watch/nope", Some(OWNER));
        assert_eq!(resp.status, 404);
    }

    #[test]
    fn unknown_query_keys_are_400_and_stale_after_ms_is_taken() {
        let tmp = TempDir::new().unwrap();
        let state = hub_with_ferris(&tmp, 2);
        for (path, bad, allowed) in [
            (SUMMARY, "warn_pct=5", "stale_after_ms"),
            (ALERTS, "project=one", "warn_quota_pct"),
            (ROLLUPS, "warn_quota_pct=5", "project"),
            (ROLLUPS, "projct=one&x=1", "project"),
        ] {
            let (status, v) = get(&state, &format!("{path}?{bad}"));
            assert_eq!(status, 400, "{path}?{bad}: {v}");
            let msg = v["error"].as_str().unwrap();
            assert!(msg.contains("unknown query parameter"), "{msg}");
            assert!(msg.contains(allowed), "{msg}");
        }
        // The query form of the bearer token is always taken.
        let (status, _) = get(&state, &format!("{ROLLUPS}?token=x"));
        assert_eq!(status, 200);
        // `stale_after_ms`: a whole number of at least 1, as `fl watch` takes it.
        let (status, v) = get(&state, &format!("{SUMMARY}?stale_after_ms=1234"));
        assert_eq!(status, 200, "{v}");
        assert_eq!(v["thresholds"]["stale_after_ms"], 1234);
        assert_eq!(v["health"]["stale_after_ms"], 1234);
        for bad in ["0", "-1", "1.5", "abc"] {
            for path in [SUMMARY, ALERTS] {
                let (status, v) = get(&state, &format!("{path}?stale_after_ms={bad}"));
                assert_eq!(status, 400, "{path} {bad}: {v}");
                assert!(
                    v["error"].as_str().unwrap().contains("stale_after_ms"),
                    "{v}"
                );
            }
        }
    }

    fn long_watch_ttl(state: &mut HubState) {
        state.watch_caches = std::sync::Arc::new(WatchCaches::new(Duration::from_secs(3600)));
    }

    #[test]
    fn quota_reads_the_cached_discovery_path_and_rollups_skip_discovery() {
        let tmp = TempDir::new().unwrap();
        let mut state = hub_with_ferris(&tmp, 3);
        long_ttl(&mut state);
        long_watch_ttl(&mut state);
        // Warm the shared store snapshot the way any discovery endpoint does.
        let (resp, cold) = measure(|| get_as(&state, "/api/v1/sessions", Some(OWNER)));
        assert_eq!(resp.status, 200);
        assert_eq!(cold.store_snapshots, 1);
        // A cold watch call: one store open, the discovery snapshot reused, one route-table read
        // (with a `Some(home)` override a scan of that temp home; in production the shared
        // 2.5 s route cache), no model lookup.
        let (resp, cold) = measure(|| get_as(&state, SUMMARY, Some(OWNER)));
        assert_eq!(resp.status, 200);
        assert_eq!(cold.store_snapshots, 0, "reuses the discovery snapshot");
        assert_eq!(cold.store_opens, 1);
        assert_eq!(cold.route_scans, 1);
        assert_eq!(cold.transcript_parses, 0);
        // Warm: summary and alerts share the read, so neither reads the store, the routes or a
        // transcript again.
        for path in [SUMMARY, ALERTS, SUMMARY] {
            let (resp, warm) = measure(|| get_as(&state, path, Some(OWNER)));
            assert_eq!(resp.status, 200, "{path}");
            assert_eq!(warm, Costs::default(), "{path} warm");
        }
        let (resp, costs) = measure(|| get_as(&state, ROLLUPS, Some(OWNER)));
        assert_eq!(resp.status, 200);
        assert_eq!(costs.route_scans, 0, "rollups never touch discovery");
        assert_eq!(costs.store_snapshots, 0);
        assert_eq!(costs.transcript_reads, 0);
        assert_eq!(costs.store_opens, 1);
        let (_, warm) = measure(|| get_as(&state, ROLLUPS, Some(OWNER)));
        assert_eq!(warm, Costs::default(), "rollups warm");
    }

    /// A warm cache reads nothing; a write through the hub (any non-GET/HEAD request) drops it;
    /// the thresholds apply per request on the cached read.
    #[test]
    fn a_warm_watch_cache_reads_no_store_and_a_write_drops_it() {
        for _ in 0..10 {
            let tmp = TempDir::new().unwrap();
            let mut state = hub_with_ferris(&tmp, 2);
            long_ttl(&mut state);
            long_watch_ttl(&mut state);
            // Cold: the discovery snapshot's open and the watch read's.
            let (_, cold) = measure(|| get(&state, SUMMARY));
            assert_eq!((cold.store_opens, cold.store_snapshots), (2, 1));
            // A store write the hub did not make is not seen while the read is warm ...
            {
                let store = state.open_store().unwrap();
                let p = store.list_projects().unwrap()[0].id.clone();
                store
                    .append_attempt(fl_core::log::Attempt {
                        id: None,
                        at: None,
                        project: p,
                        record: fl_core::ids::RecordId(fl_core::ids::seq_iri(1)),
                        adapter: "codex".into(),
                        status: fl_core::log::AttemptStatus::Completed,
                        duration_ms: 1,
                        tokens_in: 0,
                        tokens_out: 0,
                        cost_usd_micros: 7,
                        paths_touched: fl_core::log::PathsTouched::Counted(0),
                        output_excerpt: None,
                    })
                    .unwrap();
            }
            let ((status, v), warm) = measure(|| get(&state, SUMMARY));
            assert_eq!(status, 200);
            assert_eq!(warm, Costs::default());
            assert_eq!(v["telemetry"]["totals"]["attempts"], 3);
            // ... but the thresholds still apply per request.
            let ((_, v), warm) = measure(|| {
                get(
                    &state,
                    &format!("{ALERTS}?critical_cost_usd_micros=0&warn_cost_usd_micros=0"),
                )
            });
            assert_eq!(warm, Costs::default());
            assert_eq!(v["alerts"].as_array().unwrap().len(), 3);
            // Any non-GET request drops the cache (a 405 here, after the auth gate).
            let resp = handle_http_request(
                &state,
                &HttpRequest::new("POST", SUMMARY).with_header("Authorization", OWNER),
            );
            assert_eq!(resp.status, 405);
            let ((_, v), cold) = measure(|| get(&state, SUMMARY));
            assert_eq!((cold.store_opens, cold.store_snapshots), (2, 1));
            assert_eq!(v["telemetry"]["totals"]["attempts"], 4);
            assert_eq!(v["telemetry"]["totals"]["cost_usd_micros"], 7);
        }
    }

    /// The TTL runs on the injected clock: served until it is up, read again once it is.
    #[test]
    fn the_watch_cache_ttl_expires_on_the_injected_clock() {
        for _ in 0..10 {
            let tmp = TempDir::new().unwrap();
            let mut state = hub_with_ferris(&tmp, 2);
            long_ttl(&mut state);
            let base = Instant::now();
            let offset = std::cell::Cell::new(Duration::ZERO);
            let clock = || base + offset.get();

            let (first, cold) = measure(|| watch_snapshot(&state, clock).unwrap());
            assert_eq!(
                (cold.store_opens, cold.store_snapshots),
                (2, 1),
                "with discovery's"
            );
            offset.set(WATCH_SNAPSHOT_TTL - Duration::from_millis(1));
            let (warm_snap, warm) = measure(|| watch_snapshot(&state, clock).unwrap());
            assert_eq!(warm, Costs::default(), "inside the TTL");
            assert!(Arc::ptr_eq(&first, &warm_snap), "the same shared read");
            offset.set(WATCH_SNAPSHOT_TTL);
            let (again, cold) = measure(|| watch_snapshot(&state, clock).unwrap());
            assert_eq!(
                (cold.store_opens, cold.store_snapshots),
                (1, 0),
                "expired at the TTL; discovery's snapshot is still warm"
            );
            assert_eq!(*again, *first);

            offset.set(Duration::ZERO);
            let (_, cold) = measure(|| rollups_report(&state, Some("one"), clock).unwrap());
            assert_eq!(cold.store_opens, 1);
            let (_, warm) = measure(|| rollups_report(&state, Some("one"), clock).unwrap());
            assert_eq!(warm, Costs::default());
            // Another `?project` is another key.
            let (two, cold) = measure(|| rollups_report(&state, Some("two"), clock).unwrap());
            assert_eq!(cold.store_opens, 1);
            assert_eq!(two.totals.attempts, 1);
            offset.set(WATCH_SNAPSHOT_TTL);
            let (_, cold) = measure(|| rollups_report(&state, Some("two"), clock).unwrap());
            assert_eq!(cold.store_opens, 1, "expired at the TTL");
            // An unknown project is a 404 and is not cached.
            for _ in 0..2 {
                let (err, cold) = measure(|| rollups_report(&state, Some("nope"), clock));
                assert_eq!(err.unwrap_err().status, 404);
                assert_eq!(cold.store_opens, 1);
            }
        }
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

        // Windows at 40% with the transcript saying rate limited: still a critical alert.
        let mut limited = both.clone();
        limited.primary_window.as_mut().unwrap().used_percent = 40.0;
        limited.secondary_window.as_mut().unwrap().used_percent = 40.0;
        limited.exhausted_reason = Some("rate limited".into());
        let w = quota_windows("codex", "t", &limited);
        assert_eq!(w.len(), 2);
        let alerts = fl_watch::evaluate_watch_alerts(
            &fl_watch::WatchTelemetryReport::default(),
            &fl_watch::WatchHealthReport::default(),
            &w,
            &WatchThresholds::default(),
            0,
        );
        let ids: Vec<(&str, fl_watch::Severity)> =
            alerts.iter().map(|a| (a.id.as_str(), a.severity)).collect();
        assert_eq!(
            ids,
            vec![("quota:codex:exhausted", fl_watch::Severity::Critical)]
        );

        let empty = SessionQuotaUsage {
            tier: None,
            primary_window: None,
            secondary_window: None,
            exhausted_reason: None,
        };
        assert!(quota_windows("grok", "s", &empty).is_empty());
    }
}
