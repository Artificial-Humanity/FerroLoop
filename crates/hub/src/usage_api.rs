//! Owner-only, read-only access to persisted FerroWatch usage.
use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

use fl_store::AnyStore;
use fl_watch::usage::{UsageError, UsageGroupBy, UsageReportQuery, aggregate_usage, parse_since};
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
    let store = match state.open_store() {
        Ok(store) => store,
        Err(_) => {
            return Some(HttpResponse::json(
                503,
                json!({"error": "usage store unavailable"}),
            ));
        }
    };
    Some(read_report(&store, &query, now))
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

fn read_report(store: &AnyStore, query: &UsageReportQuery, now: u64) -> HttpResponse {
    match aggregate_usage(store, query, now) {
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
    fn malformed_document_is_service_unavailable() {
        let store = AnyStore::in_memory();
        store
            .as_ferro()
            .unwrap()
            .put_doc(
                "watch_usage_v1",
                "bad",
                &json!({"kind":"source", "document":{}}),
            )
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
