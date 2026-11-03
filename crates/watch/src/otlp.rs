//! OTLP/JSON exporter (traces + metrics) per OTLP spec JSON encoding (hex trace/span ids as
//! strings, int64 values as strings where spec requires, unix-nano timestamps as strings).
//!
//! Hand-rolled with `serde_json`: no OTLP crate and no network client. Delivery goes through
//! the [`OtlpTransport`] trait; callers supply an HTTP transport if they want one.
//!
//! # Multi-count series: do not aggregate across dimensions
//!
//! Each exported series set contains a total series (no dimension attributes) plus
//! alternative per-project, per-adapter and per-agent breakdown series of the same
//! underlying attempts. These series overlap: the total is a partition sum, and the
//! project, adapter and agent breakdowns are three different partitions of it. A consumer
//! must therefore pick exactly one dimension (for example only the total, only
//! `project.slug`, only `adapter`, or only `agent`) and must not sum the total with any
//! breakdown series, or one breakdown dimension with another. Doing so double- or
//! triple-counts the same attempts, tokens, cost and duration. The same rule applies to
//! the regret gauge, which carries a total series plus per-project series.

use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::BTreeMap;

use crate::rollups::{WatchAttemptCounts, WatchTelemetryReport};

/// Default service name.
pub const DEFAULT_SERVICE_NAME: &str = "ferroloop";

/// Exporter configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OtlpConfig {
    pub endpoint: String,
    pub service_name: String,
}

impl Default for OtlpConfig {
    fn default() -> Self {
        Self {
            endpoint: "http://localhost:4318".to_string(),
            service_name: DEFAULT_SERVICE_NAME.to_string(),
        }
    }
}

impl OtlpConfig {
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            ..Self::default()
        }
    }

    pub fn validate(&self) -> Result<(), OtlpError> {
        validate_endpoint(&self.endpoint)?;
        if self.service_name.is_empty() {
            return Err(OtlpError::InvalidEndpoint(
                "service_name must not be empty".to_string(),
            ));
        }
        if self.service_name.chars().any(char::is_control) {
            return Err(OtlpError::InvalidEndpoint(
                "service_name must not contain control characters".to_string(),
            ));
        }
        Ok(())
    }
}

/// Exporter errors.
#[derive(Debug, thiserror::Error)]
pub enum OtlpError {
    /// Endpoint (or service name) invalid; any embedded userinfo is redacted.
    #[error("invalid OTLP endpoint: {0}")]
    InvalidEndpoint(String),
    #[error("OTLP transport failed: {0}")]
    Transport(String),
}

fn redact_userinfo(endpoint: &str) -> String {
    // Authority is the part before the first path/query/fragment delimiter,
    // with or without a `scheme://` prefix. Redact userinfo in both forms so
    // schemeless inputs such as `user:secret@host` never echo credentials.
    let (prefix, rest) = match endpoint.find("://") {
        Some(scheme_end) => (&endpoint[..scheme_end + 3], &endpoint[scheme_end + 3..]),
        None => ("", endpoint),
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let Some(at) = authority.rfind('@') else {
        return endpoint.to_string();
    };
    format!(
        "{prefix}***@{}{}",
        &authority[at + 1..],
        &rest[authority_end..]
    )
}

fn validate_endpoint(endpoint: &str) -> Result<(), OtlpError> {
    let redacted = redact_userinfo(endpoint);
    let invalid = |msg: &str| OtlpError::InvalidEndpoint(format!("{msg}: {redacted}"));
    if endpoint.chars().any(char::is_control) || endpoint.contains(char::is_whitespace) {
        return Err(invalid(
            "endpoint must not contain whitespace or control characters",
        ));
    }
    let Some(rest) = endpoint
        .strip_prefix("http://")
        .or_else(|| endpoint.strip_prefix("https://"))
    else {
        return Err(invalid("endpoint must start with http:// or https://"));
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.contains('@') {
        return Err(invalid("endpoint must not contain embedded credentials"));
    }
    // Host: strip port / IPv6 brackets. A bracketed host must have a closing
    // bracket, and only an optional numeric `:port` may follow it.
    let host = if let Some(inner) = authority.strip_prefix('[') {
        let Some(close) = inner.find(']') else {
            return Err(invalid("endpoint IPv6 host must have a closing bracket"));
        };
        let after = &inner[close + 1..];
        if !after.is_empty() {
            let Some(port) = after.strip_prefix(':') else {
                return Err(invalid("endpoint port must be numeric"));
            };
            if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
                return Err(invalid("endpoint port must be numeric"));
            }
        }
        &inner[..close]
    } else {
        if authority.contains(['[', ']']) {
            return Err(invalid("endpoint host contains a stray bracket"));
        }
        authority.split(':').next().unwrap_or("")
    };
    if host.is_empty() {
        return Err(invalid("endpoint must have a non-empty host"));
    }
    // If a port is present (non-IPv6), it must be numeric.
    if !authority.starts_with('[')
        && let Some((_, port)) = authority.rsplit_once(':')
        && (port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(invalid("endpoint port must be numeric"));
    }
    Ok(())
}

// --- deterministic ids (FNV-1a, hand-rolled; stable across runs/processes) ---

fn fnv1a_update(mut hash: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// Hash fields unambiguously: every field is length-prefixed, so distinct
/// field splits (`["ab", "c"]` vs `["a", "bc"]`) can never collide by joining.
fn hash_fields(seed: u64, fields: &[&str]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325 ^ seed;
    for field in fields {
        hash = fnv1a_update(hash, &(field.len() as u64).to_le_bytes());
        hash = fnv1a_update(hash, field.as_bytes());
    }
    hash
}

fn trace_id(
    report: &WatchTelemetryReport,
    window_start_unix_nano: u64,
    window_end_unix_nano: u64,
) -> String {
    let start = window_start_unix_nano.to_string();
    let end = window_end_unix_nano.to_string();
    let mut fields: Vec<&str> = vec!["ferroloop.trace", &start, &end];
    for p in &report.by_project {
        fields.push(&p.project);
        fields.push(&p.slug);
    }
    let hi = hash_fields(0, &fields);
    let lo = hash_fields(0x9e37_79b9_7f4a_7c15, &fields);
    let (hi, lo) = if hi == 0 && lo == 0 {
        (hi, 1)
    } else {
        (hi, lo)
    };
    format!("{hi:016x}{lo:016x}")
}

fn span_id(fields: &[&str]) -> String {
    let h = hash_fields(0x51_7c_c1_b7_27_22_0a_95, fields);
    format!("{:016x}", if h == 0 { 1 } else { h })
}

/// Span id for `fields`, deduped against `seen`: the first occurrence uses the
/// base id, later duplicates deterministically re-hash with their occurrence
/// index, so repeated projects (same IRI and slug) never share a span id.
fn unique_span_id(seen: &mut BTreeMap<String, u64>, fields: &[&str]) -> String {
    let base = span_id(fields);
    let occurrence = seen.entry(base.clone()).or_insert(0);
    let n = *occurrence;
    *occurrence += 1;
    if n == 0 {
        return base;
    }
    let n = n.to_string();
    let mut extended: Vec<&str> = fields.to_vec();
    extended.push(&n);
    let id = span_id(&extended);
    // The re-hash could itself collide with an earlier id; it is recorded so
    // a further duplicate of either base gets a fresh occurrence count.
    *seen.entry(id.clone()).or_insert(0) += 1;
    id
}

// --- attribute / point helpers ---

fn attr_str(key: &str, value: &str) -> Value {
    json!({"key": key, "value": {"stringValue": value}})
}
fn attr_int(key: &str, value: u64) -> Value {
    json!({"key": key, "value": {"intValue": value.to_string()}})
}

fn resource(service_name: &str) -> Value {
    json!({"attributes": [attr_str("service.name", service_name)]})
}

fn data_point(attrs: Vec<Value>, value: u64, start_nano: u64, time_nano: u64) -> Value {
    json!({
        "attributes": attrs,
        "startTimeUnixNano": start_nano.to_string(),
        "timeUnixNano": time_nano.to_string(),
        "asInt": value.to_string(),
    })
}

/// Description shared by metrics that carry overlapping total and breakdown
/// series; see the module docs on multi-count series.
const MULTI_COUNT_NOTE: &str = "Series overlap: a total series (no dimension attributes) is \
    exported alongside per-project, per-adapter and per-agent breakdowns of the same values. \
    Do not aggregate the total and dimension series together; pick one dimension.";

fn sum_metric(name: &str, description: &str, unit: &str, points: Vec<Value>) -> Value {
    json!({
        "name": name,
        "description": description,
        "unit": unit,
        "sum": {"dataPoints": points, "aggregationTemporality": 2, "isMonotonic": true},
    })
}

fn gauge_metric(name: &str, description: &str, unit: &str, points: Vec<Value>) -> Value {
    json!({
        "name": name,
        "description": description,
        "unit": unit,
        "gauge": {"dataPoints": points},
    })
}

struct Series {
    attrs: Vec<Value>,
    counts: WatchAttemptCounts,
}

fn series(report: &WatchTelemetryReport) -> Vec<Series> {
    let mut out = vec![Series {
        attrs: vec![],
        counts: report.totals,
    }];
    for p in &report.by_project {
        out.push(Series {
            attrs: vec![attr_str("project.slug", &p.slug)],
            counts: p.counts,
        });
    }
    for b in &report.by_adapter {
        out.push(Series {
            attrs: vec![attr_str("adapter", &b.key)],
            counts: b.counts,
        });
    }
    for b in &report.by_agent {
        out.push(Series {
            attrs: vec![attr_str("agent", &b.key)],
            counts: b.counts,
        });
    }
    out
}

fn with_attr(base: &[Value], extra: Value) -> Vec<Value> {
    let mut v = base.to_vec();
    v.push(extra);
    v
}

fn build_metrics_payload_with(
    report: &WatchTelemetryReport,
    window_start_unix_nano: u64,
    now_unix_nano: u64,
    service_name: &str,
) -> Value {
    let s = series(report);
    let start = window_start_unix_nano;
    let t = now_unix_nano;
    let mut attempts_pts = Vec::new();
    let mut tin_pts = Vec::new();
    let mut tout_pts = Vec::new();
    let mut cost_pts = Vec::new();
    let mut dur_pts = Vec::new();
    for item in &s {
        for (status, v) in [
            ("succeeded", item.counts.succeeded),
            ("failed", item.counts.failed),
            ("crashed", item.counts.crashed),
            ("timed_out", item.counts.timed_out),
        ] {
            attempts_pts.push(data_point(
                with_attr(&item.attrs, attr_str("status", status)),
                v,
                start,
                t,
            ));
        }
        tin_pts.push(data_point(
            item.attrs.clone(),
            item.counts.tokens_in,
            start,
            t,
        ));
        tout_pts.push(data_point(
            item.attrs.clone(),
            item.counts.tokens_out,
            start,
            t,
        ));
        cost_pts.push(data_point(
            item.attrs.clone(),
            item.counts.cost_usd_micros,
            start,
            t,
        ));
        dur_pts.push(data_point(
            item.attrs.clone(),
            item.counts.duration_ms,
            start,
            t,
        ));
    }

    let gate_pts = vec![
        data_point(
            vec![attr_str("verdict", "pass")],
            report.gate_runs.pass,
            start,
            t,
        ),
        data_point(
            vec![attr_str("verdict", "fail")],
            report.gate_runs.fail,
            start,
            t,
        ),
        data_point(
            vec![attr_str("verdict", "error")],
            report.gate_runs.error,
            start,
            t,
        ),
    ];

    let mut regret_by_slug: BTreeMap<&str, u64> = BTreeMap::new();
    for c in &report.regret.challenges {
        *regret_by_slug.entry(c.slug.as_str()).or_default() += 1;
    }
    let mut regret_pts = vec![data_point(
        vec![],
        report.regret.challenges.len() as u64,
        start,
        t,
    )];
    for p in &report.by_project {
        regret_pts.push(data_point(
            vec![attr_str("project.slug", &p.slug)],
            regret_by_slug.get(p.slug.as_str()).copied().unwrap_or(0),
            start,
            t,
        ));
    }

    let metrics = vec![
        sum_metric(
            "ferroloop.attempts",
            MULTI_COUNT_NOTE,
            "{attempt}",
            attempts_pts,
        ),
        sum_metric(
            "ferroloop.tokens.input",
            MULTI_COUNT_NOTE,
            "{token}",
            tin_pts,
        ),
        sum_metric(
            "ferroloop.tokens.output",
            MULTI_COUNT_NOTE,
            "{token}",
            tout_pts,
        ),
        sum_metric(
            "ferroloop.cost.usd_micros",
            MULTI_COUNT_NOTE,
            "usd_micros",
            cost_pts,
        ),
        sum_metric("ferroloop.duration_ms", MULTI_COUNT_NOTE, "ms", dur_pts),
        sum_metric(
            "ferroloop.gate_runs",
            "Gate runs by verdict.",
            "{run}",
            gate_pts,
        ),
        gauge_metric(
            "ferroloop.regret.challenges",
            "Regret challenges: a total series plus per-project series. Do not aggregate \
             the total and per-project series together; pick one dimension.",
            "{challenge}",
            regret_pts,
        ),
    ];

    json!({
        "resourceMetrics": [{
            "resource": resource(service_name),
            "scopeMetrics": [{
                "scope": {"name": "ferroloop.watch", "version": "1"},
                "metrics": metrics,
            }],
        }],
    })
}

/// Metrics payload for `report` over the window `window_start_unix_nano` to
/// `now_unix_nano`: Sum points carry `startTimeUnixNano` = window start and
/// `timeUnixNano` = `now_unix_nano` (service name [`DEFAULT_SERVICE_NAME`]).
pub fn build_metrics_payload(
    report: &WatchTelemetryReport,
    window_start_unix_nano: u64,
    now_unix_nano: u64,
) -> Value {
    build_metrics_payload_with(
        report,
        window_start_unix_nano,
        now_unix_nano,
        DEFAULT_SERVICE_NAME,
    )
}

fn counts_attrs(prefix: &[Value], c: &WatchAttemptCounts, regret: u64) -> Vec<Value> {
    let mut attrs = prefix.to_vec();
    attrs.extend([
        attr_int("attempts", c.attempts),
        attr_int("attempts.succeeded", c.succeeded),
        attr_int("attempts.failed", c.failed),
        attr_int("attempts.crashed", c.crashed),
        attr_int("attempts.timed_out", c.timed_out),
        attr_int("tokens.input", c.tokens_in),
        attr_int("tokens.output", c.tokens_out),
        attr_int("cost.usd_micros", c.cost_usd_micros),
        attr_int("duration_ms", c.duration_ms),
        attr_int("regret.challenges", regret),
    ]);
    attrs
}

fn build_traces_payload_with(
    report: &WatchTelemetryReport,
    window_start_unix_nano: u64,
    window_end_unix_nano: u64,
    service_name: &str,
) -> Value {
    let tid = trace_id(report, window_start_unix_nano, window_end_unix_nano);
    let start = window_start_unix_nano.to_string();
    let end = window_end_unix_nano.to_string();
    let mut seen_span_ids: BTreeMap<String, u64> = BTreeMap::new();
    let root_id = unique_span_id(&mut seen_span_ids, &["root", &tid, &start, &end]);

    let mut regret_by_slug: BTreeMap<&str, u64> = BTreeMap::new();
    for c in &report.regret.challenges {
        *regret_by_slug.entry(c.slug.as_str()).or_default() += 1;
    }

    let mut spans = vec![json!({
        "traceId": tid,
        "spanId": root_id,
        "name": "ferroloop.watch.export",
        "kind": 1,
        "startTimeUnixNano": start,
        "endTimeUnixNano": end,
        "attributes": counts_attrs(&[], &report.totals, report.regret.challenges.len() as u64),
        "status": {"code": 1},
    })];
    for p in &report.by_project {
        let sid = unique_span_id(
            &mut seen_span_ids,
            &["project", &p.project, &p.slug, &start, &end],
        );
        let regret = regret_by_slug.get(p.slug.as_str()).copied().unwrap_or(0);
        let attrs = counts_attrs(
            &[
                attr_str("project.slug", &p.slug),
                attr_str("project", &p.project),
            ],
            &p.counts,
            regret,
        );
        spans.push(json!({
            "traceId": tid,
            "spanId": sid,
            "parentSpanId": root_id,
            "name": "ferroloop.attempts.rollup",
            "kind": 1,
            "startTimeUnixNano": start,
            "endTimeUnixNano": end,
            "attributes": attrs,
            "status": {"code": 1},
        }));
    }

    json!({
        "resourceSpans": [{
            "resource": resource(service_name),
            "scopeSpans": [{
                "scope": {"name": "ferroloop.watch", "version": "1"},
                "spans": spans,
            }],
        }],
    })
}

/// Traces payload: root `ferroloop.watch.export` span plus one
/// `ferroloop.attempts.rollup` span per project, all in one trace.
pub fn build_traces_payload(
    report: &WatchTelemetryReport,
    window_start_unix_nano: u64,
    window_end_unix_nano: u64,
) -> Value {
    build_traces_payload_with(
        report,
        window_start_unix_nano,
        window_end_unix_nano,
        DEFAULT_SERVICE_NAME,
    )
}

/// Delivery of one OTLP/JSON payload to `path` (`/v1/metrics` or `/v1/traces`).
pub trait OtlpTransport {
    fn send(&self, path: &str, payload: &Value) -> Result<(), OtlpError>;
}

/// Transport that records payloads in memory (for tests / embedding).
#[derive(Debug, Default)]
pub struct MemoryTransport {
    sent: RefCell<Vec<(String, Value)>>,
}

impl MemoryTransport {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn sent(&self) -> Vec<(String, Value)> {
        self.sent.borrow().clone()
    }
}

impl OtlpTransport for MemoryTransport {
    fn send(&self, path: &str, payload: &Value) -> Result<(), OtlpError> {
        self.sent
            .borrow_mut()
            .push((path.to_string(), payload.clone()));
        Ok(())
    }
}

/// Transport that discards payloads.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopTransport;

impl OtlpTransport for NoopTransport {
    fn send(&self, _path: &str, _payload: &Value) -> Result<(), OtlpError> {
        Ok(())
    }
}

/// Validate `config`, then send metrics to `/v1/metrics` and traces to `/v1/traces`.
pub fn export_report(
    transport: &dyn OtlpTransport,
    config: &OtlpConfig,
    report: &WatchTelemetryReport,
    start_nano: u64,
    end_nano: u64,
) -> Result<(), OtlpError> {
    config.validate()?;
    let metrics = build_metrics_payload_with(report, start_nano, end_nano, &config.service_name);
    transport.send("/v1/metrics", &metrics)?;
    let traces = build_traces_payload_with(report, start_nano, end_nano, &config.service_name);
    transport.send("/v1/traces", &traces)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rollups::{WatchBreakdown, WatchGateTotals, WatchProjectRollup};

    fn sample() -> WatchTelemetryReport {
        let counts = WatchAttemptCounts {
            attempts: 4,
            succeeded: 2,
            failed: 1,
            crashed: 0,
            timed_out: 1,
            tokens_in: 100,
            tokens_out: 200,
            cost_usd_micros: 1234,
            duration_ms: 500,
        };
        WatchTelemetryReport {
            source: "rows".into(),
            totals: counts,
            gate_runs: WatchGateTotals {
                pass: 3,
                fail: 1,
                error: 0,
                duration_ms: 30,
                cost_usd_micros: 10,
            },
            by_project: vec![WatchProjectRollup {
                project: "urn:fl:project:1".into(),
                slug: "alpha".into(),
                root: "/w/alpha".into(),
                counts,
            }],
            by_adapter: vec![WatchBreakdown {
                key: "claude".into(),
                counts,
            }],
            by_agent: vec![WatchBreakdown {
                key: "ada".into(),
                counts,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn payload_shape_and_int_as_string() {
        let p = build_metrics_payload(&sample(), 10, 42);
        let rm = &p["resourceMetrics"][0];
        assert_eq!(
            rm["resource"]["attributes"][0]["value"]["stringValue"],
            json!("ferroloop")
        );
        let metrics = rm["scopeMetrics"][0]["metrics"].as_array().unwrap();
        let names: Vec<&str> = metrics.iter().filter_map(|m| m["name"].as_str()).collect();
        for n in [
            "ferroloop.attempts",
            "ferroloop.tokens.input",
            "ferroloop.tokens.output",
            "ferroloop.cost.usd_micros",
            "ferroloop.duration_ms",
            "ferroloop.gate_runs",
            "ferroloop.regret.challenges",
        ] {
            assert!(names.contains(&n), "missing {n}: {names:?}");
        }
        let attempts = metrics
            .iter()
            .find(|m| m["name"] == "ferroloop.attempts")
            .unwrap();
        assert_eq!(attempts["sum"]["isMonotonic"], json!(true));
        assert_eq!(attempts["sum"]["aggregationTemporality"], json!(2));
        let pt = &attempts["sum"]["dataPoints"][0];
        assert!(pt["asInt"].is_string());
        assert_eq!(pt["startTimeUnixNano"], json!("10"));
        assert_eq!(pt["timeUnixNano"], json!("42"));
        let regret = metrics
            .iter()
            .find(|m| m["name"] == "ferroloop.regret.challenges")
            .unwrap();
        assert!(regret["gauge"]["dataPoints"].is_array());
        // traces
        let tr = build_traces_payload(&sample(), 10, 20);
        let spans = tr["resourceSpans"][0]["scopeSpans"][0]["spans"]
            .as_array()
            .unwrap();
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0]["name"], json!("ferroloop.watch.export"));
        assert_eq!(spans[1]["name"], json!("ferroloop.attempts.rollup"));
        assert_eq!(spans[1]["kind"], json!(1));
        assert_eq!(spans[0]["traceId"].as_str().unwrap().len(), 32);
        assert_eq!(spans[0]["spanId"].as_str().unwrap().len(), 16);
        assert_eq!(spans[1]["parentSpanId"], spans[0]["spanId"]);
        assert_eq!(spans[0]["startTimeUnixNano"], json!("10"));
    }

    #[test]
    fn invalid_endpoints_rejected_and_credentials_redacted() {
        for ep in [
            "",
            "ftp://host",
            "http://",
            "https:///path",
            "http://user:secret@host:4318",
            "https://user@host",
            "user:secret@host",
            "user:secret@host:4318/path",
            "http://[::1",
            "http://[::1]:abc",
            "http://[::1]:",
            "http://[::1]extra",
            "http://host:abc",
        ] {
            let cfg = OtlpConfig {
                endpoint: ep.into(),
                service_name: "ferroloop".into(),
            };
            let err = cfg.validate().unwrap_err().to_string();
            assert!(!err.contains("secret"), "{err}");
            assert!(!err.contains("user"), "{err}");
        }
        assert!(
            OtlpConfig {
                endpoint: "http://host".into(),
                service_name: "".into()
            }
            .validate()
            .is_err()
        );
        assert!(
            OtlpConfig {
                endpoint: "http://host".into(),
                service_name: "bad\u{1}name".into()
            }
            .validate()
            .is_err()
        );
        assert!(OtlpConfig::default().validate().is_ok());
        for ep in [
            "http://[::1]",
            "http://[::1]:4318",
            "https://[2001:db8::1]/v1",
        ] {
            assert!(
                OtlpConfig::new(ep).validate().is_ok(),
                "expected valid: {ep}"
            );
        }
    }

    #[test]
    fn deterministic_span_ids() {
        let a = build_traces_payload(&sample(), 10, 20);
        let b = build_traces_payload(&sample(), 10, 20);
        assert_eq!(a, b);
        let c = build_traces_payload(&sample(), 11, 20);
        assert_ne!(
            a["resourceSpans"][0]["scopeSpans"][0]["spans"][0]["traceId"],
            c["resourceSpans"][0]["scopeSpans"][0]["spans"][0]["traceId"]
        );
    }

    #[test]
    fn export_sends_both_paths_in_order() {
        let mem = MemoryTransport::new();
        export_report(&mem, &OtlpConfig::default(), &sample(), 10, 20).unwrap();
        let sent = mem.sent();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0].0, "/v1/metrics");
        assert_eq!(sent[1].0, "/v1/traces");
        assert!(sent[0].1.get("resourceMetrics").is_some());
        assert!(sent[1].1.get("resourceSpans").is_some());
        let first_point = &sent[0].1["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0]["sum"]
            ["dataPoints"][0];
        assert_eq!(first_point["startTimeUnixNano"], json!("10"));
        assert_eq!(first_point["timeUnixNano"], json!("20"));
        // invalid config sends nothing
        let mem2 = MemoryTransport::new();
        let bad = OtlpConfig {
            endpoint: "nope".into(),
            service_name: "x".into(),
        };
        assert!(export_report(&mem2, &bad, &sample(), 0, 1).is_err());
        assert!(mem2.sent().is_empty());
        assert!(export_report(&NoopTransport, &OtlpConfig::default(), &sample(), 0, 1).is_ok());
    }

    fn attrs_map(attrs: &Value) -> BTreeMap<String, String> {
        attrs
            .as_array()
            .unwrap()
            .iter()
            .map(|a| {
                let v = &a["value"];
                let val = v["stringValue"]
                    .as_str()
                    .or_else(|| v["intValue"].as_str())
                    .unwrap_or("")
                    .to_string();
                (a["key"].as_str().unwrap().to_string(), val)
            })
            .collect()
    }

    fn metric<'a>(payload: &'a Value, name: &str) -> &'a Value {
        payload["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["name"] == name)
            .unwrap_or_else(|| panic!("missing metric {name}"))
    }

    fn point<'a>(metric: &'a Value, kind: &str, want: &[(&str, &str)]) -> &'a Value {
        metric[kind]["dataPoints"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| {
                let attrs = attrs_map(&p["attributes"]);
                want.iter()
                    .all(|(k, v)| attrs.get(*k).map(String::as_str) == Some(*v))
                    && attrs.len() == want.len()
            })
            .unwrap_or_else(|| panic!("missing point {want:?} in {}", metric["name"]))
    }

    #[test]
    fn metric_values_window_and_multi_count_descriptions() {
        let p = build_metrics_payload(&sample(), 10, 42);
        let attempts = metric(&p, "ferroloop.attempts");
        assert!(
            attempts["description"]
                .as_str()
                .unwrap()
                .contains("Do not aggregate"),
            "{}",
            attempts["description"]
        );
        let total_succeeded = point(attempts, "sum", &[("status", "succeeded")]);
        assert_eq!(total_succeeded["asInt"], json!("2"));
        assert_eq!(total_succeeded["startTimeUnixNano"], json!("10"));
        assert_eq!(total_succeeded["timeUnixNano"], json!("42"));
        assert_eq!(
            point(attempts, "sum", &[("status", "failed")])["asInt"],
            json!("1")
        );
        assert_eq!(
            point(attempts, "sum", &[("status", "timed_out")])["asInt"],
            json!("1")
        );
        assert_eq!(
            point(
                attempts,
                "sum",
                &[("project.slug", "alpha"), ("status", "succeeded")]
            )["asInt"],
            json!("2")
        );
        assert_eq!(
            point(metric(&p, "ferroloop.tokens.input"), "sum", &[])["asInt"],
            json!("100")
        );
        assert_eq!(
            point(metric(&p, "ferroloop.tokens.output"), "sum", &[])["asInt"],
            json!("200")
        );
        assert_eq!(
            point(metric(&p, "ferroloop.cost.usd_micros"), "sum", &[])["asInt"],
            json!("1234")
        );
        assert_eq!(
            point(metric(&p, "ferroloop.duration_ms"), "sum", &[])["asInt"],
            json!("500")
        );
        let gates = metric(&p, "ferroloop.gate_runs");
        assert_eq!(
            point(gates, "sum", &[("verdict", "pass")])["asInt"],
            json!("3")
        );
        assert_eq!(
            point(gates, "sum", &[("verdict", "fail")])["asInt"],
            json!("1")
        );
        assert_eq!(
            point(gates, "sum", &[("verdict", "error")])["asInt"],
            json!("0")
        );
        assert_eq!(
            point(gates, "sum", &[("verdict", "pass")])["startTimeUnixNano"],
            json!("10")
        );
        // Every series-bearing Sum metric carries the multi-count warning.
        for name in [
            "ferroloop.tokens.input",
            "ferroloop.cost.usd_micros",
            "ferroloop.regret.challenges",
        ] {
            let kind = if name.contains("regret") {
                "gauge"
            } else {
                "sum"
            };
            let m = metric(&p, name);
            assert!(
                m["description"]
                    .as_str()
                    .unwrap()
                    .contains("Do not aggregate"),
                "{name} ({kind}) missing note: {}",
                m["description"]
            );
        }
    }

    #[test]
    fn span_attribute_values() {
        let tr = build_traces_payload(&sample(), 10, 20);
        let spans = tr["resourceSpans"][0]["scopeSpans"][0]["spans"]
            .as_array()
            .unwrap();
        let root = attrs_map(&spans[0]["attributes"]);
        assert_eq!(root["attempts"], "4");
        assert_eq!(root["attempts.succeeded"], "2");
        assert_eq!(root["attempts.failed"], "1");
        assert_eq!(root["attempts.timed_out"], "1");
        assert_eq!(root["tokens.input"], "100");
        assert_eq!(root["tokens.output"], "200");
        assert_eq!(root["cost.usd_micros"], "1234");
        assert_eq!(root["duration_ms"], "500");
        let proj = attrs_map(&spans[1]["attributes"]);
        assert_eq!(proj["project.slug"], "alpha");
        assert_eq!(proj["project"], "urn:fl:project:1");
        assert_eq!(proj["attempts.succeeded"], "2");
        assert_eq!(spans[1]["startTimeUnixNano"], json!("10"));
        assert_eq!(spans[1]["endTimeUnixNano"], json!("20"));
    }

    #[test]
    fn ids_distinguish_iri_window_end_and_duplicate_projects() {
        // Same slug, different project IRI -> different span ids.
        let mut two = sample();
        let mut other = two.by_project[0].clone();
        other.project = "urn:fl:project:2".into();
        two.by_project.push(other);
        let spans_of = |r: &WatchTelemetryReport| {
            build_traces_payload(r, 10, 20)["resourceSpans"][0]["scopeSpans"][0]["spans"]
                .as_array()
                .unwrap()
                .clone()
        };
        let spans = spans_of(&two);
        assert_ne!(spans[1]["spanId"], spans[2]["spanId"]);
        // Window end participates in trace and span ids.
        let later_end = build_traces_payload(&sample(), 10, 21);
        let base = build_traces_payload(&sample(), 10, 20);
        assert_ne!(
            later_end["resourceSpans"][0]["scopeSpans"][0]["spans"][0]["traceId"],
            base["resourceSpans"][0]["scopeSpans"][0]["spans"][0]["traceId"]
        );
        assert_ne!(
            later_end["resourceSpans"][0]["scopeSpans"][0]["spans"][1]["spanId"],
            base["resourceSpans"][0]["scopeSpans"][0]["spans"][1]["spanId"]
        );
        // Length-prefixed fields: different field splits hash differently.
        let mut a = sample();
        a.by_project[0].project = "ab".into();
        a.by_project[0].slug = "c".into();
        let mut b = sample();
        b.by_project[0].project = "a".into();
        b.by_project[0].slug = "bc".into();
        assert_ne!(spans_of(&a)[0]["traceId"], spans_of(&b)[0]["traceId"]);
        // Duplicate project entries are deduped deterministically.
        let mut dup = sample();
        let clone = dup.by_project[0].clone();
        dup.by_project.push(clone);
        let first = spans_of(&dup);
        let ids: std::collections::BTreeSet<&str> = first
            .iter()
            .map(|s| s["spanId"].as_str().unwrap())
            .collect();
        assert_eq!(ids.len(), first.len(), "span ids must be unique: {first:?}");
        assert_eq!(first, spans_of(&dup), "dedupe must be deterministic");
    }

    struct FailingTransport {
        fail_on: &'static str,
        attempted: RefCell<Vec<String>>,
    }

    impl OtlpTransport for FailingTransport {
        fn send(&self, path: &str, _payload: &Value) -> Result<(), OtlpError> {
            self.attempted.borrow_mut().push(path.to_string());
            if path == self.fail_on {
                return Err(OtlpError::Transport("boom".into()));
            }
            Ok(())
        }
    }

    #[test]
    fn transport_errors_propagate() {
        for fail_on in ["/v1/metrics", "/v1/traces"] {
            let stub = FailingTransport {
                fail_on,
                attempted: RefCell::new(Vec::new()),
            };
            let err = export_report(&stub, &OtlpConfig::default(), &sample(), 10, 20).unwrap_err();
            assert!(matches!(err, OtlpError::Transport(_)), "{err:?}");
            assert!(err.to_string().contains("boom"), "{err}");
            let attempted = stub.attempted.borrow().clone();
            if fail_on == "/v1/metrics" {
                assert_eq!(attempted, ["/v1/metrics"]);
            } else {
                assert_eq!(attempted, ["/v1/metrics", "/v1/traces"]);
            }
        }
    }

    #[test]
    fn empty_report_exports_valid_payload() {
        let empty = WatchTelemetryReport::default();
        let m = build_metrics_payload(&empty, 0, 0);
        let metrics = m["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]
            .as_array()
            .unwrap();
        assert!(!metrics.is_empty());
        let tr = build_traces_payload(&empty, 0, 0);
        let spans = tr["resourceSpans"][0]["scopeSpans"][0]["spans"]
            .as_array()
            .unwrap();
        assert_eq!(spans.len(), 1);
        let mem = MemoryTransport::new();
        export_report(&mem, &OtlpConfig::default(), &empty, 0, 0).unwrap();
        assert_eq!(mem.sent().len(), 2);
    }
}
