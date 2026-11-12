//! Owner-only, read-only access to persisted FerroWatch usage.
use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(test)]
use fl_store::AnyStore;
#[cfg(test)]
use fl_watch::usage::aggregate_usage;
use fl_watch::usage::{
    UsageError, UsageGroupBy, UsageReportQuery, aggregate_snapshot, parse_since,
    read_usage_documents,
};
use serde_json::json;

use crate::auth::HubAuthority;
use crate::{HttpRequest, HttpResponse, HubState};

const ROUTE: &str = "/api/v1/usage";

pub(crate) fn route(
    state: &HubState,
    authority: &HubAuthority,
    req: &HttpRequest,
) -> Option<HttpResponse> {
    if req.path != ROUTE {
        return None;
    }
    if !authority.is_owner() {
        return Some(HttpResponse::json(
            403,
            json!({"error": "owner authority required"}),
        ));
    }
    if req.method != "GET" {
        return Some(
            HttpResponse::json(405, json!({"error": "usage takes only GET"}))
                .with_header("Allow", "GET"),
        );
    }
    let now = match SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_millis()).ok())
    {
        Some(now) => now,
        None => {
            return Some(HttpResponse::json(
                503,
                json!({"error": "usage clock unavailable"}),
            ));
        }
    };
    let query = match validate(req, now) {
        Ok(query) => query,
        Err(err) => return Some(HttpResponse::json(400, json!({"error": err.to_string()}))),
    };
    Some(report_response(snapshot_for_hub(state).and_then(
        |snapshot| aggregate_snapshot(snapshot, &query, now),
    )))
}

fn snapshot_for_hub(
    state: &HubState,
) -> Result<fl_watch::usage::UsageDocumentSnapshot, UsageError> {
    let store = state
        .open_store()
        .map_err(|e| UsageError::Store(e.to_string()))?;
    read_usage_documents(&store)
}

fn validate(req: &HttpRequest, now: u64) -> Result<UsageReportQuery, UsageError> {
    let mut keys = HashSet::new();
    if let Some((_, raw_query)) = req.raw_target.split_once('?') {
        for pair in raw_query.split('&').filter(|pair| !pair.is_empty()) {
            let raw_key = pair.split_once('=').map_or(pair, |(key, _)| key);
            let key = crate::api::percent_decode(raw_key);
            if !matches!(key.as_str(), "since" | "by" | "token") || !keys.insert(key) {
                return Err(UsageError::Invalid(
                    "unknown or duplicate query parameter".into(),
                ));
            }
        }
    }
    let since_ms = parse_since(
        req.query.get("since").map(String::as_str).unwrap_or("24h"),
        now,
    )?;
    let by = req
        .query
        .get("by")
        .map(String::as_str)
        .unwrap_or("persona")
        .parse::<UsageGroupBy>()?;
    Ok(UsageReportQuery { since_ms, by })
}

#[cfg(test)]
fn read_report(store: &AnyStore, query: &UsageReportQuery, now: u64) -> HttpResponse {
    report_response(aggregate_usage(store, query, now))
}

fn report_response(report: Result<fl_watch::usage::UsageReport, UsageError>) -> HttpResponse {
    match report {
        Ok(report) => HttpResponse::json(200, json!(report)),
        Err(UsageError::Invalid(message)) => HttpResponse::json(400, json!({"error": message})),
        Err(UsageError::Unsupported) => HttpResponse::json(
            503,
            json!({"error": "usage unsupported on redb backend", "code": "usage_unsupported"}),
        ),
        Err(UsageError::Store(_)) => {
            HttpResponse::json(503, json!({"error": "usage store unavailable"}))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn persisted_collection_reopens_and_reports_late_parent_without_double_counting() {
        use crate::usage::collector::{collect_once, read_sources};
        use crate::usage::scan::{ScanLimits, UsageSource};
        use fl_watch::usage::{IdentityMappingDocument, UsageVendor};

        let root = tempfile::tempdir().unwrap();
        let source_path = root.path().join("transcript.jsonl");
        let parent = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        std::fs::write(
            &source_path,
            format!(
                "{{\"type\":\"GENERIC\",\"content\":\"Message sent to \\\"{parent}\\\"\"}}\n\
                 {{\"type\":\"PLANNER_RESPONSE\",\"created_at\":\"2026-11-08T00:30:00Z\",\"input_tokens\":42,\"output_tokens\":7,\"cache_read_tokens\":13,\"content\":\"private fixture text\"}}\n"
            ),
        ).unwrap();
        let source = UsageSource {
            path: source_path,
            source_id: "isolated-source".into(),
            session_id: "child".into(),
            vendor: UsageVendor::Antigravity,
        };
        let opts = fl_store::StoreOptions {
            backend: Some(fl_store::BackendKind::Ferro),
            ..Default::default()
        };
        let db = root.path().join("test.fls");
        let query = UsageReportQuery {
            since_ms: 1_794_096_000_000,
            by: UsageGroupBy::Persona,
        };
        let now = 1_794_099_600_000;
        {
            let store = fl_store::open_store_with(&db, &opts).unwrap();
            collect_once(
                &store,
                std::slice::from_ref(&source),
                &[],
                ScanLimits::default(),
            )
            .unwrap();
            let initial = aggregate_usage(&store, &query, now).unwrap();
            assert_eq!(initial.rows[0].key, "[unattributed]");
            assert_eq!(initial.rows[0].input, Some(42));
        }
        {
            let store = fl_store::open_store_with(&db, &opts).unwrap();
            let original = read_sources(&store).unwrap();
            let identity = IdentityMappingDocument {
                session_id: parent.into(),
                persona: Some("Felix".into()),
                parent_id: None,
                launch_time_ms: None,
                provenance: "fixture-route".into(),
            };
            collect_once(
                &store,
                std::slice::from_ref(&source),
                &[identity],
                ScanLimits::default(),
            )
            .unwrap();
            collect_once(&store, &[source], &[], ScanLimits::default()).unwrap();
            assert_eq!(read_sources(&store).unwrap(), original);
            let report = aggregate_usage(&store, &query, now).unwrap();
            assert_eq!(report.rows.len(), 1);
            assert_eq!(report.rows[0].key, "Felix");
            assert_eq!(report.rows[0].input, Some(42));
            assert_eq!(report.rows[0].output, Some(7));
            let response = read_report(&store, &query, now);
            assert_eq!(response.status, 200);
            let body = response.json_value().unwrap();
            assert_eq!(body["rows"][0]["input"], 42);
            assert_eq!(body["rows"][0]["key"], "Felix");
            assert!(!body.to_string().contains("private fixture text"));
        }
    }
    #[test]
    fn snapshot_releases_hub_guard_before_aggregation() {
        let tmp = tempfile::tempdir().unwrap();
        let state = HubState::new(crate::HubServerConfig {
            store_path: tmp.path().join("test.fls"),
            ..Default::default()
        });
        let snapshot = snapshot_for_hub(&state).unwrap();
        let guard = state
            .store_lock
            .try_lock()
            .expect("snapshot released store guard");
        let query = UsageReportQuery {
            since_ms: 1,
            by: UsageGroupBy::Persona,
        };
        assert!(aggregate_snapshot(snapshot, &query, 3_600_001).is_ok());
        drop(guard);
    }
    #[test]
    fn same_store_and_clock_produce_identical_json() {
        let store = AnyStore::in_memory();
        let query = UsageReportQuery {
            since_ms: 1,
            by: UsageGroupBy::Persona,
        };
        let first = read_report(&store, &query, 3_600_001);
        let second = read_report(&store, &query, 3_600_001);
        assert_eq!(first.status, 200);
        assert_eq!(first.body, second.body);
    }
    #[test]
    fn malformed_documents_keep_healthy_usage_and_mark_incomplete() {
        use crate::usage::collector::collect_once;
        use crate::usage::scan::{ScanLimits, UsageSource};
        use fl_watch::usage::UsageVendor;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transcript.jsonl");
        std::fs::write(&path, "{\"type\":\"PLANNER_RESPONSE\",\"created_at\":\"2026-11-08T00:30:00Z\",\"input_tokens\":42}\n").unwrap();
        let store = AnyStore::in_memory();
        let source = UsageSource {
            path,
            source_id: "healthy".into(),
            session_id: "healthy".into(),
            vendor: UsageVendor::Antigravity,
        };
        collect_once(&store, &[source], &[], ScanLimits::default()).unwrap();
        let ferro = store.as_ferro().unwrap();
        ferro
            .put_doc(
                "watch_usage_v1",
                "bad-source",
                &json!({"kind":"source", "document":{}}),
            )
            .unwrap();
        ferro
            .put_doc(
                "watch_usage_v1",
                "bad-identity",
                &json!({"kind":"identity", "document":{}}),
            )
            .unwrap();
        let query = UsageReportQuery {
            since_ms: 1_794_096_000_000,
            by: UsageGroupBy::Vendor,
        };
        let response = read_report(&store, &query, 1_794_099_600_000);
        assert_eq!(response.status, 200);
        let body = response.json_value().unwrap();
        assert_eq!(body["rows"][0]["key"], "antigravity");
        assert_eq!(body["rows"][0]["input"], 42);
        assert_eq!(body["coverage"]["incomplete"], true);
    }
    #[test]
    fn malformed_status_still_returns_service_unavailable() {
        let store = AnyStore::in_memory();
        store
            .as_ferro()
            .unwrap()
            .put_doc("watch_usage_v1", "collector-status", &json!({"bad":true}))
            .unwrap();
        let query = UsageReportQuery {
            since_ms: 1,
            by: UsageGroupBy::Persona,
        };
        assert_eq!(read_report(&store, &query, 3_600_001).status, 503);
    }
    #[test]
    fn redb_is_explicitly_unsupported() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = fl_store::open_redb(&tmp.path().join("old.redb")).unwrap();
        let query = UsageReportQuery {
            since_ms: 1,
            by: UsageGroupBy::Persona,
        };
        let result = read_report(&store, &query, 3_600_001);
        assert_eq!(result.status, 503);
        assert_eq!(result.json_value().unwrap()["code"], "usage_unsupported");
    }
}
