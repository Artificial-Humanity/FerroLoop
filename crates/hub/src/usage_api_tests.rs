use crate::{HttpRequest, HubServerConfig, HubState, handle_http_request};
use fl_core::pr_policy::ReviewerApps;
use tempfile::TempDir;

const OWNER: &str = "owner-usage-test";
const PEER: &str = "peer-usage-test";

fn hub(tmp: &TempDir) -> HubState {
    HubState::new(HubServerConfig {
        listen: "127.0.0.1:0".into(),
        store_path: tmp.path().join("store.fls"),
        repo_root: tmp.path().to_path_buf(),
        ui_dir: None,
        owner_login: "owner".into(),
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
fn get(state: &HubState, target: &str, token: Option<&str>) -> crate::HttpResponse {
    let mut req = HttpRequest::new("GET", target);
    if let Some(token) = token {
        req = req.with_header("Authorization", format!("Bearer {token}"));
    }
    handle_http_request(state, &req)
}
#[test]
fn owner_report_is_readable_and_peer_and_unauthenticated_are_refused() {
    let tmp = TempDir::new().unwrap();
    let state = hub(&tmp);
    assert_eq!(get(&state, "/api/v1/usage", None).status, 401);
    assert_eq!(get(&state, "/api/v1/usage", Some(PEER)).status, 403);
    let response = get(&state, "/api/v1/usage?since=48h&by=vendor", Some(OWNER));
    assert_eq!(
        response.status,
        200,
        "{}",
        String::from_utf8_lossy(&response.body)
    );
    let data: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
    assert_eq!(data["by"], "vendor");
    assert_eq!(data["unit"], "tokens");
    assert!(!String::from_utf8_lossy(&response.body).contains("transcript"));
    let start = data["effective_start_ms"].as_u64().unwrap();
    let end = data["effective_end_ms"].as_u64().unwrap();
    assert_eq!(start % 3_600_000, 0);
    assert_eq!(end % 3_600_000, 0);
    assert!(start <= data["requested_start_ms"].as_u64().unwrap());
    assert!(end >= data["requested_end_ms"].as_u64().unwrap());
}
#[test]
fn invalid_and_duplicate_query_keys_are_bad_requests() {
    let tmp = TempDir::new().unwrap();
    let state = hub(&tmp);
    for target in [
        "/api/v1/usage?since=bad",
        "/api/v1/usage?by=nope",
        "/api/v1/usage?since=1h&since=2h",
        "/api/v1/usage?by=persona&%62y=model",
        "/api/v1/usage?unexpected=1",
    ] {
        assert_eq!(get(&state, target, Some(OWNER)).status, 400, "{target}");
    }
}
#[test]
fn store_failure_is_service_unavailable_after_authorization() {
    let tmp = TempDir::new().unwrap();
    std::fs::create_dir(tmp.path().join("store.fls")).unwrap();
    let state = hub(&tmp);
    assert_eq!(get(&state, "/api/v1/usage", Some(PEER)).status, 403);
    assert_eq!(get(&state, "/api/v1/usage", Some(OWNER)).status, 503);
}

#[test]
fn report_uses_persisted_buckets_without_scanning_and_keeps_unavailable_data() {
    use fl_watch::usage::{
        CategoryUnavailable, ParserState, SourceUsageDocument, TokenCounts, UsageBucket,
        UsageCoverage, UsageVendor,
    };
    let tmp = TempDir::new().unwrap();
    let state = hub(&tmp);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let hour = now / 3_600_000 * 3_600_000;
    let doc = SourceUsageDocument {
        source_id: "private-source".into(),
        session_id: "session".into(),
        vendor: UsageVendor::Grok,
        offset: 0,
        generation: 0,
        file_identity: None,
        anchor: String::new(),
        discarding_line: false,
        parser: ParserState::default(),
        buckets: vec![UsageBucket {
            model: None,
            hour_ms: Some(hour),
            counts: TokenCounts {
                input: None,
                output: None,
                cache_read: None,
                cache_write: None,
            },
            turns: 0,
            turns_unavailable: true,
            unavailable: CategoryUnavailable {
                input: true,
                output: true,
                cache_read: true,
                cache_write: true,
            },
            missing: Default::default(),
            observations: 1,
        }],
        coverage: UsageCoverage {
            unavailable: true,
            ..Default::default()
        },
    };
    {
        let store = state.open_store().unwrap();
        store
            .as_ferro()
            .unwrap()
            .put_doc(
                "watch_usage_v1",
                "fixture",
                &serde_json::json!({"kind":"source", "document":doc}),
            )
            .unwrap();
    }
    let response = get(&state, "/api/v1/usage?since=48h&by=vendor", Some(OWNER));
    assert_eq!(response.status, 200);
    let body: serde_json::Value = response.json_value().unwrap();
    let row = &body["rows"][0];
    assert_eq!(row["key"], "grok");
    assert!(row["input"].is_null());
    assert_eq!(row["unavailable"]["input"], true);
    assert!(row["turns"].is_null());
    assert_eq!(body["coverage"]["unavailable_sources"], 1);
    assert!(!String::from_utf8_lossy(&response.body).contains("private-source"));
}

#[test]
fn endpoint_does_not_scan_retained_transcripts() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join(".gemini/antigravity/brain/example");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("transcript.jsonl"),
        "{\"type\":\"PLANNER_RESPONSE\",\"input_tokens\":42}\n",
    )
    .unwrap();
    let state = hub(&tmp);
    let result = get(&state, "/api/v1/usage", Some(OWNER));
    assert_eq!(result.status, 200);
    assert_eq!(
        result.json_value().unwrap()["rows"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
}
#[test]
fn method_and_utc_parameter_validation() {
    let tmp = TempDir::new().unwrap();
    let state = hub(&tmp);
    let req = HttpRequest::new("POST", "/api/v1/usage")
        .with_header("Authorization", format!("Bearer {OWNER}"));
    let response = handle_http_request(&state, &req);
    assert_eq!(response.status, 405);
    assert!(
        response
            .headers
            .iter()
            .any(|(k, v)| k == "Allow" && v == "GET")
    );
    let response = get(
        &state,
        "/api/v1/usage?since=2020-01-01T01%3A15%3A00Z",
        Some(OWNER),
    );
    assert_eq!(response.status, 200);
    let body = response.json_value().unwrap();
    assert_eq!(body["requested_start_ms"], 1_577_841_300_000u64);
    assert_eq!(body["effective_start_ms"], 1_577_840_400_000u64);
}
