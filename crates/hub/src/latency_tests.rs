//! Request-cost tests: how many route scans, transcript reads and parses, and store reads one
//! hub request takes. They prove a costly step is skipped, not only that the answer is right.

use std::fs;
use std::path::Path;

use fl_core::ids::{RecordId, seq_iri};
use fl_core::log::{Attempt, AttemptStatus, PathsTouched};
use fl_core::pr_policy::ReviewerApps;
use fl_core::store::{Catalog, Ledger};
use serde_json::json;
use tempfile::TempDir;

use crate::discovery::cost_counters::{Costs, measure};
use crate::{HttpRequest, HttpResponse, HubServerConfig, HubState, handle_http_request};

const OWNER: &str = "Bearer owner-tok-secret";

/// A hub whose home is `tmp`: one Antigravity session for `ferris` with `turns` turns, and a
/// store with two projects and three attempts.
pub(crate) fn hub_with_ferris(tmp: &TempDir, turns: usize) -> HubState {
    let state = HubState::new(HubServerConfig {
        listen: "127.0.0.1:0".into(),
        store_path: tmp.path().join("store.redb"),
        repo_root: tmp.path().to_path_buf(),
        ui_dir: None,
        owner_login: "lmcfarlin".into(),
        reviewer_apps: ReviewerApps::default(),
        github: crate::github_pr::HubGithub::default(),
        owner_token: Some("owner-tok-secret".into()),
        peer_token: Some("peer-tok-secret".into()),
        trust_tailscale_headers: false,
        enable_terminal: false,
        session_ttl_secs: 3600,
        home_override: Some(tmp.path().to_path_buf()),
    });
    write_ferris_transcript(tmp.path(), turns);
    {
        let store = state.open_store().unwrap();
        let p1 = store.add_project("one").unwrap();
        let p2 = store.add_project("two").unwrap();
        for (project, status) in [
            (&p1, AttemptStatus::Completed),
            (&p1, AttemptStatus::Refused),
            (&p2, AttemptStatus::Crashed),
        ] {
            store
                .append_attempt(Attempt {
                    id: None,
                    at: None,
                    project: project.clone(),
                    record: RecordId(seq_iri(9)),
                    adapter: "claude".into(),
                    status,
                    duration_ms: 1,
                    tokens_in: 1,
                    tokens_out: 1,
                    cost_usd_micros: 0,
                    paths_touched: PathsTouched::Counted(0),
                    output_excerpt: Some("done".into()),
                })
                .unwrap();
        }
    }
    state
}

/// `turns` Antigravity turns for the `ferris` persona; the last one is an assistant reply.
pub(crate) fn write_ferris_transcript(home: &Path, turns: usize) {
    let logs = home
        .join(".gemini")
        .join("antigravity-cli")
        .join("brain")
        .join("conv-ferris-1")
        .join(".system_generated")
        .join("logs");
    fs::create_dir_all(&logs).unwrap();
    let mut body = String::new();
    for i in 0..turns.max(1) {
        let line = if i == 0 {
            json!({
                "step_index": 0,
                "type": "USER_INPUT",
                "content": "You will take the name of Ferris. Run cargo test.",
                "created_at": "2026-10-27T16:00:00Z"
            })
        } else {
            json!({
                "step_index": i,
                "type": "PLANNER_RESPONSE",
                "content": format!("reply {i}"),
                "created_at": "2026-10-27T16:01:00Z"
            })
        };
        body.push_str(&line.to_string());
        body.push('\n');
    }
    fs::write(logs.join("transcript.jsonl"), body).unwrap();
}

pub(crate) fn get(state: &HubState, path: &str) -> (HttpResponse, Costs) {
    measure(|| {
        handle_http_request(
            state,
            &HttpRequest::new("GET", path).with_header("Authorization", OWNER),
        )
    })
}

#[test]
fn read_only_discovery_endpoints_share_one_store_snapshot_until_a_write() {
    let tmp = TempDir::new().unwrap();
    let mut state = hub_with_ferris(&tmp, 3);
    // A TTL no slow test run can outlast, so only the write below can drop the snapshot.
    state.discovery_snapshots = std::sync::Arc::new(crate::discovery::TtlCache::new(
        std::time::Duration::from_secs(3600),
    ));
    let (first, cold) = get(&state, "/api/v1/sessions");
    assert_eq!(first.status, 200);
    assert_eq!(cold.store_snapshots, 1);
    let attempts = |resp: &HttpResponse| {
        resp.json_value().unwrap()["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|s| s["source"] == "attempt")
            .count()
    };
    assert_eq!(
        attempts(&first),
        3,
        "every attempt row is listed, terminal ones too"
    );
    for path in [
        "/api/v1/sessions",
        "/api/v1/agents",
        "/api/v1/agents/liveness",
        "/api/v1/sessions/ferris/transcript",
    ] {
        let (resp, warm) = get(&state, path);
        assert_eq!(resp.status, 200, "{path}");
        assert_eq!(warm.store_snapshots, 0, "{path} reuses the snapshot");
    }
    // A write through the hub drops the snapshot, so its own change shows at once.
    let write = handle_http_request(
        &state,
        &HttpRequest::new("POST", "/api/v1/sessions/ferris/queue/none/send-now")
            .with_header("Authorization", OWNER),
    );
    assert_eq!(write.status, 404);
    let (_, after_write) = get(&state, "/api/v1/sessions");
    assert_eq!(after_write.store_snapshots, 1);
}

#[test]
fn a_zero_ttl_snapshot_cache_reads_the_store_every_time() {
    let tmp = TempDir::new().unwrap();
    let mut state = hub_with_ferris(&tmp, 3);
    state.discovery_snapshots =
        std::sync::Arc::new(crate::discovery::TtlCache::new(std::time::Duration::ZERO));
    for _ in 0..2 {
        let (_, costs) = get(&state, "/api/v1/sessions");
        assert_eq!(costs.store_snapshots, 1);
    }
}

// ---- item 1: the route discovery TTL cache ----

mod ttl_cache {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};
    use std::time::{Duration, Instant};

    use crate::discovery::TtlCache;

    const TTL: Duration = Duration::from_millis(100);

    /// A load that counts itself and yields `[value]`.
    fn counted_load(loads: &AtomicUsize, value: &str) -> Result<Vec<String>, String> {
        loads.fetch_add(1, Ordering::SeqCst);
        Ok(vec![value.to_string()])
    }

    #[test]
    fn a_read_inside_the_ttl_hits_and_one_at_the_ttl_loads_again() {
        let cache = TtlCache::new(TTL);
        let loads = AtomicUsize::new(0);
        let t0 = Instant::now();
        let first = cache.get_or_load("k", || t0, || counted_load(&loads, "a"));
        assert_eq!(first, Ok(vec!["a".to_string()]));
        let hit = cache.get_or_load(
            "k",
            || t0 + TTL - Duration::from_millis(1),
            || counted_load(&loads, "b"),
        );
        assert_eq!(hit, Ok(vec!["a".to_string()]), "inside the TTL: cached");
        assert_eq!(loads.load(Ordering::SeqCst), 1);
        let expired = cache.get_or_load("k", || t0 + TTL, || counted_load(&loads, "c"));
        assert_eq!(
            expired,
            Ok(vec!["c".to_string()]),
            "at the TTL: loaded again"
        );
        assert_eq!(loads.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_value_ages_from_the_end_of_its_load_not_the_start() {
        let cache = TtlCache::new(TTL);
        let t0 = Instant::now();
        // An injected clock: the load "takes" most of a TTL, as a 1.4 s host scan does.
        let now = std::cell::Cell::new(t0);
        let load_time = TTL - Duration::from_millis(10);
        let first = cache.get_or_load(
            "k",
            || now.get(),
            || {
                now.set(t0 + load_time);
                Ok::<_, ()>(vec!["scan".to_string()])
            },
        );
        assert_eq!(first, Ok(vec!["scan".to_string()]));
        let loaded = t0 + load_time;
        // Stamped at the start, this read would be past the TTL and load again.
        now.set(loaded + TTL - Duration::from_millis(1));
        let hit = cache.get_or_load("k", || now.get(), || Ok::<_, ()>(vec!["again".to_string()]));
        assert_eq!(
            hit,
            Ok(vec!["scan".to_string()]),
            "a whole TTL after the load"
        );
        now.set(loaded + TTL);
        let expired =
            cache.get_or_load("k", || now.get(), || Ok::<_, ()>(vec!["again".to_string()]));
        assert_eq!(
            expired,
            Ok(vec!["again".to_string()]),
            "a TTL after the load"
        );
    }

    #[test]
    fn a_slow_load_that_an_invalidation_overtook_is_not_served() {
        let cache = TtlCache::new(TTL);
        let t0 = Instant::now();
        let now = std::cell::Cell::new(t0);
        let during = cache.get_or_load(
            "k",
            || now.get(),
            || {
                now.set(t0 + Duration::from_millis(50));
                cache.invalidate();
                Ok::<_, ()>(vec!["before-change".to_string()])
            },
        );
        assert_eq!(during, Ok(vec!["before-change".to_string()]));
        let next = cache.get_or_load("k", || now.get(), || Ok::<_, ()>(vec!["after".to_string()]));
        assert_eq!(
            next,
            Ok(vec!["after".to_string()]),
            "stamped late, still discarded"
        );
    }

    #[test]
    fn a_caller_changing_its_copy_does_not_change_the_cache() {
        let cache = TtlCache::new(TTL);
        let t0 = Instant::now();
        let mut mine = cache
            .get_or_load("k", || t0, || Ok::<_, ()>(vec!["a".to_string()]))
            .unwrap();
        mine.push("leak".to_string());
        mine[0] = "changed".to_string();
        let again = cache
            .get_or_load("k", || t0, || Ok::<_, ()>(vec!["reloaded".to_string()]))
            .unwrap();
        assert_eq!(again, vec!["a".to_string()]);
    }

    #[test]
    fn invalidate_forces_the_next_read_to_load() {
        let cache = TtlCache::new(TTL);
        let loads = AtomicUsize::new(0);
        let t0 = Instant::now();
        cache
            .get_or_load("k", || t0, || counted_load(&loads, "a"))
            .unwrap();
        cache.invalidate();
        let after = cache.get_or_load("k", || t0, || counted_load(&loads, "b"));
        assert_eq!(after, Ok(vec!["b".to_string()]));
        assert_eq!(loads.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn another_key_or_a_failed_load_is_not_served_from_the_cache() {
        let cache = TtlCache::new(TTL);
        let t0 = Instant::now();
        cache
            .get_or_load("k", || t0, || Ok::<_, String>(vec!["a".into()]))
            .unwrap();
        let other = cache.get_or_load("j", || t0, || Ok::<_, String>(vec!["j".into()]));
        assert_eq!(other, Ok(vec!["j".to_string()]), "a different key loads");
        let failed = cache.get_or_load("x", || t0, || Err::<Vec<String>, _>("down".to_string()));
        assert_eq!(failed, Err("down".to_string()));
        let next = cache.get_or_load("x", || t0, || Ok::<_, String>(vec!["x".into()]));
        assert_eq!(next, Ok(vec!["x".to_string()]), "an error is not cached");
    }

    #[test]
    fn an_invalidation_during_a_load_is_not_lost() {
        let cache = TtlCache::new(TTL);
        let t0 = Instant::now();
        // The load overlaps a lifecycle change: its result predates the change.
        let during = cache.get_or_load(
            "k",
            || t0,
            || {
                cache.invalidate();
                Ok::<_, ()>(vec!["before-change".to_string()])
            },
        );
        assert_eq!(during, Ok(vec!["before-change".to_string()]));
        let next = cache.get_or_load("k", || t0, || Ok::<_, ()>(vec!["after".to_string()]));
        assert_eq!(
            next,
            Ok(vec!["after".to_string()]),
            "the stale load was not cached"
        );
    }

    #[test]
    fn concurrent_misses_share_one_load() {
        let cache = Arc::new(TtlCache::new(Duration::from_secs(60)));
        let loads = Arc::new(AtomicUsize::new(0));
        let start = Arc::new(Barrier::new(8));
        let t0 = Instant::now();
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let (cache, loads, start) = (cache.clone(), loads.clone(), start.clone());
                std::thread::spawn(move || {
                    start.wait();
                    cache
                        .get_or_load(
                            "k",
                            || t0,
                            || {
                                loads.fetch_add(1, Ordering::SeqCst);
                                std::thread::sleep(Duration::from_millis(20));
                                Ok::<_, ()>(vec!["scan".to_string()])
                            },
                        )
                        .unwrap()
                })
            })
            .collect();
        for handle in handles {
            assert_eq!(handle.join().unwrap(), vec!["scan".to_string()]);
        }
        assert_eq!(
            loads.load(Ordering::SeqCst),
            1,
            "one scan for all eight callers"
        );
    }
}

#[test]
fn a_home_override_always_scans_fresh_and_never_shares_a_scan() {
    use fl_core::at::At;
    use fl_wire::identity::{RouteTable, VendorRoute};

    let tmp = TempDir::new().unwrap();
    let path = crate::discovery::route_table_path(Some(tmp.path())).unwrap();
    let write = |persona: &str| {
        let mut routes = RouteTable::default();
        routes.upsert(
            persona,
            VendorRoute::Webhook {
                socket_path: tmp.path().join("w.sock"),
            },
            At::from_unix_millis(1),
        );
        routes.save(Some(&path)).unwrap();
    };
    write("first");
    let (one, costs_one) = measure(|| crate::discovery::discovered_route_table(Some(tmp.path())));
    write("second");
    let (two, costs_two) = measure(|| crate::discovery::discovered_route_table(Some(tmp.path())));
    assert!(one.get("first").is_some());
    assert!(
        two.get("second").is_some() && two.get("first").is_none(),
        "no cached table"
    );
    assert_eq!((costs_one.route_scans, costs_two.route_scans), (1, 1));
}

// ---- item 2: the transcript and prompt endpoints ----

/// The transcript response as the handler built it before the shared lookup: each step ran its
/// own route lookup and read (and parsed) its file again.
fn transcript_response_reference(
    state: &HubState,
    session_id: &str,
    limit: Option<usize>,
) -> Vec<u8> {
    use crate::discovery::{
        discover_route_sessions_without_model_info, read_session_context_usage_from_snapshot,
        read_session_quota_usage_with_home, read_session_transcript_from_snapshot,
    };
    let snap = state.cached_discovery_snapshot().unwrap_or_default();
    let home = state.config.home_override.as_deref();
    let trimmed = session_id.trim();
    let (canonical_id, _) = discover_route_sessions_without_model_info(&snap, home)
        .iter()
        .find(|s| s.id == trimmed || s.persona.as_deref() == Some(trimmed))
        .map(|m| (m.id.clone(), m.persona.clone()))
        .unwrap_or_else(|| (trimmed.to_string(), Some(trimmed.to_ascii_lowercase())));
    let all_turns = read_session_transcript_from_snapshot(session_id, &snap, home);
    let total_turns = all_turns.len();
    let turns = match limit {
        Some(limit) if total_turns > limit => all_turns[total_turns - limit..].to_vec(),
        _ => all_turns,
    };
    let busy_turns = read_session_transcript_from_snapshot(&canonical_id, &snap, home);
    let is_busy = state.is_session_busy_from_turns(&canonical_id, &busy_turns);
    let queue = state.session_queue(&canonical_id);
    let context_usage = read_session_context_usage_from_snapshot(session_id, &snap, home);
    let quota_usage = read_session_quota_usage_with_home(&canonical_id, home).or_else(|| {
        if canonical_id == session_id {
            None
        } else {
            read_session_quota_usage_with_home(session_id, home)
        }
    });
    HttpResponse::json(
        200,
        json!({
            "session_id": session_id,
            "turns": turns,
            "total_turns": total_turns,
            "is_busy": is_busy,
            "in_flight_unconfirmed": state.session_in_flight_unconfirmed(&canonical_id),
            "queue": queue,
            "context_usage": context_usage,
            "quota_usage": quota_usage,
        }),
    )
    .body
}

#[test]
fn the_transcript_endpoint_scans_once_parses_once_and_answers_byte_for_byte_the_same() {
    let tmp = TempDir::new().unwrap();
    let state = hub_with_ferris(&tmp, 6);
    for (id, query, limit) in [
        // The persona resolves to the conversation id: two ids, one file.
        ("ferris", "", Some(250)),
        ("ferris", "?limit=2", Some(2)),
        ("ferris", "?limit=all", None),
        ("conv-ferris-1", "?limit=250", Some(250)),
        // No session at all: the attempt fallback and the empty answer.
        ("ghost", "", Some(250)),
    ] {
        let (resp, costs) = get(&state, &format!("/api/v1/sessions/{id}/transcript{query}"));
        assert_eq!(resp.status, 200);
        let expected = transcript_response_reference(&state, id, limit);
        assert_eq!(
            String::from_utf8_lossy(&resp.body),
            String::from_utf8_lossy(&expected),
            "{id}{query}"
        );
        assert_eq!(costs.route_scans, 1, "{id}{query}: one route lookup");
        let found = id != "ghost";
        assert_eq!(
            costs.transcript_parses,
            u64::from(found),
            "{id}{query}: parsed once"
        );
        assert_eq!(
            costs.transcript_reads,
            u64::from(found),
            "{id}{query}: read once"
        );
    }
    let ferris = get(&state, "/api/v1/sessions/ferris/transcript").0;
    assert_eq!(ferris.json_value().unwrap()["total_turns"], 6);
}

fn post_prompt(state: &HubState, send_now: bool) -> (HttpResponse, Costs) {
    measure(|| {
        handle_http_request(
            state,
            &HttpRequest::new("POST", "/api/v1/sessions/ferris/prompt")
                .with_header("Authorization", OWNER)
                .with_json_body(&json!({ "prompt": "hi", "send_now": send_now })),
        )
    })
}

#[test]
fn send_now_unstable_post_read_keeps_queue_until_later_completion() {
    let tmp = TempDir::new().unwrap();
    let state = hub_with_ferris(&tmp, 3);
    let home = tmp.path().to_path_buf();
    crate::discovery::after_next_transcript_read(move || write_ferris_transcript(&home, 4));
    let (sent, _) = post_prompt(&state, true);
    assert_eq!(sent.json_value().unwrap()["status"], "sent");
    let (queued, _) = post_prompt(&state, false);
    assert_eq!(queued.json_value().unwrap()["status"], "queued");
    assert_eq!(state.session_queue("conv-ferris-1").len(), 1);
    crate::api::drain_ready_session_queues(&state);
    assert_eq!(
        state.session_queue("conv-ferris-1").len(),
        1,
        "old idle turns cannot complete the new prompt"
    );
    write_ferris_transcript(tmp.path(), 5);
    crate::api::drain_ready_session_queues(&state);
    assert!(
        state.session_queue("conv-ferris-1").is_empty(),
        "a later assistant turn releases the queue"
    );
}

#[test]
fn removed_post_dispatch_transcript_keeps_queue_fenced_until_new_reply() {
    for send_now in [false, true] {
        let tmp = TempDir::new().unwrap();
        let state = hub_with_ferris(&tmp, 3);
        let path = tmp.path().join(
            ".gemini/antigravity-cli/brain/conv-ferris-1/.system_generated/logs/transcript.jsonl",
        );
        let old = fs::read(&path).unwrap();
        let target = path.clone();
        crate::api::after_next_prompt_dispatch(move || fs::remove_file(target).unwrap());
        let (sent, _) = post_prompt(&state, send_now);
        assert_eq!(sent.json_value().unwrap()["status"], "sent");
        assert_eq!(sent.json_value().unwrap()["in_flight_unconfirmed"], true);
        let (queued, _) = post_prompt(&state, false);
        assert_eq!(queued.json_value().unwrap()["status"], "queued");
        crate::api::drain_ready_session_queues(&state);
        assert_eq!(
            state.session_queue("conv-ferris-1").len(),
            1,
            "absence cannot set an unknown baseline to zero"
        );
        fs::write(&path, &old).unwrap();
        crate::api::drain_ready_session_queues(&state);
        assert_eq!(
            state.session_queue("conv-ferris-1").len(),
            1,
            "restored old turns do not complete a new prompt"
        );
        let request = HttpRequest::new("GET", "/api/v1/sessions/conv-ferris-1/transcript")
            .with_header("Authorization", OWNER);
        let observed = handle_http_request(&state, &request).json_value().unwrap();
        assert_eq!(observed["in_flight_unconfirmed"], true);
        assert_eq!(observed["queue"].as_array().unwrap().len(), 1);

        write_ferris_transcript(tmp.path(), 4);
        crate::api::drain_ready_session_queues(&state);
        assert!(
            state.session_queue("conv-ferris-1").is_empty(),
            "new reply releases queue"
        );
    }
}

#[test]
fn removed_queued_dispatch_transcript_keeps_following_prompt_fenced() {
    let tmp = TempDir::new().unwrap();
    let state = hub_with_ferris(&tmp, 3);
    let path = tmp.path().join(
        ".gemini/antigravity-cli/brain/conv-ferris-1/.system_generated/logs/transcript.jsonl",
    );
    let old = fs::read(&path).unwrap();
    state.enqueue_session_prompt(
        "conv-ferris-1",
        Some("ferris".into()),
        "first".into(),
        "first".into(),
        "owner".into(),
        vec![],
    );
    state.enqueue_session_prompt(
        "conv-ferris-1",
        Some("ferris".into()),
        "second".into(),
        "second".into(),
        "owner".into(),
        vec![],
    );
    let target = path.clone();
    crate::api::after_next_prompt_dispatch(move || fs::remove_file(target).unwrap());
    crate::api::drain_ready_session_queues(&state);
    assert_eq!(state.session_queue("conv-ferris-1").len(), 1);
    crate::api::drain_ready_session_queues(&state);
    assert_eq!(state.session_queue("conv-ferris-1").len(), 1);
    fs::write(&path, &old).unwrap();
    crate::api::drain_ready_session_queues(&state);
    assert_eq!(state.session_queue("conv-ferris-1").len(), 1);
    write_ferris_transcript(tmp.path(), 4);
    crate::api::drain_ready_session_queues(&state);
    assert!(state.session_queue("conv-ferris-1").is_empty());
}

#[test]
fn known_prompt_skips_attempt_snapshot_and_reuses_unchanged_transcript() {
    let tmp = TempDir::new().unwrap();
    let state = hub_with_ferris(&tmp, 3);
    let (queued_check, checked) = post_prompt(&state, false);
    assert_eq!(queued_check.json_value().unwrap()["status"], "sent");
    // Busy check and in-flight mark share the unchanged transcript.
    assert_eq!(checked.transcript_parses, 1);
    assert_eq!(checked.store_snapshots, 0);

    let (sent, skipped) = post_prompt(&state, true);
    let body = sent.json_value().unwrap();
    assert_eq!(body["status"], "sent");
    assert_eq!(body["session_id"], "conv-ferris-1");
    assert_eq!(
        skipped.transcript_parses, 1,
        "only the in-flight mark parses"
    );
    assert_eq!(skipped.store_snapshots, 0);

    // The in-flight mark still took: a prompt without send_now now queues.
    let (queued, _) = post_prompt(&state, false);
    assert_eq!(queued.json_value().unwrap()["status"], "queued");
}

#[test]
fn unknown_prompt_uses_full_attempt_fallback() {
    let tmp = TempDir::new().unwrap();
    let state = hub_with_ferris(&tmp, 3);
    let (response, costs) = measure(|| {
        handle_http_request(
            &state,
            &HttpRequest::new("POST", "/api/v1/sessions/not-a-route/prompt")
                .with_header("Authorization", OWNER)
                .with_json_body(&json!({ "prompt": "hi", "send_now": true })),
        )
    });
    assert_eq!(response.status, 200);
    assert_eq!(
        costs.store_snapshots, 1,
        "unknown targets retain attempt fallback"
    );
}

#[test]
fn prompt_to_recorded_attempt_still_resolves_via_full_fallback() {
    let tmp = TempDir::new().unwrap();
    let state = hub_with_ferris(&tmp, 3);
    let listed = get(&state, "/api/v1/sessions").0.json_value().unwrap();
    let attempt = listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["source"] == "attempt")
        .unwrap()["id"]
        .as_str()
        .unwrap();
    let (response, costs) = measure(|| {
        handle_http_request(
            &state,
            &HttpRequest::new("POST", format!("/api/v1/sessions/{attempt}/prompt"))
                .with_header("Authorization", OWNER)
                .with_json_body(&json!({ "prompt": "hi", "send_now": true })),
        )
    });
    assert_eq!(response.status, 200);
    assert_eq!(response.json_value().unwrap()["session_id"], attempt);
    assert_eq!(costs.store_snapshots, 1);
}

#[test]
fn single_route_lookup_does_not_search_unrelated_transcripts() {
    use fl_core::at::At;
    use fl_wire::identity::{RouteTable, VendorRoute};
    let tmp = TempDir::new().unwrap();
    let path = crate::discovery::route_table_path(Some(tmp.path())).unwrap();
    let mut routes = RouteTable::default();
    for i in 0..25 {
        routes.upsert(
            &format!("agent{i}"),
            VendorRoute::Codex {
                thread_id: format!("thread-{i}"),
                cwd: None,
            },
            At::from_unix_millis(1),
        );
    }
    routes.save(Some(&path)).unwrap();
    let snapshot = crate::discovery::DiscoveryStoreSnapshot::default();
    let (found, costs) = measure(|| {
        crate::discovery::lookup_one_route_session(&snapshot, Some(tmp.path()), "thread-12")
    });
    assert_eq!(found.unwrap().persona.as_deref(), Some("agent12"));
    assert_eq!(
        costs.transcript_finds, 1,
        "only the selected route is searched"
    );
    let full = crate::discovery::lookup_sessions(&snapshot, Some(tmp.path()));
    for requested in ["thread-12", "agent12", "missing"] {
        let fast =
            crate::discovery::lookup_one_route_session(&snapshot, Some(tmp.path()), requested);
        let full_match = full
            .iter()
            .find(|s| s.id == requested || s.persona.as_deref() == Some(requested));
        assert_eq!(fast.as_ref(), full_match, "{requested}");
    }
}

#[test]
fn fast_lookup_preserves_full_discoverys_deduplicated_route_order() {
    use fl_core::at::At;
    use fl_wire::identity::{RouteTable, VendorRoute};
    let tmp = TempDir::new().unwrap();
    let path = crate::discovery::route_table_path(Some(tmp.path())).unwrap();
    let mut routes = RouteTable::default();
    for persona in ["alice", "bob"] {
        routes.upsert(
            persona,
            VendorRoute::Codex {
                thread_id: "shared-thread".into(),
                cwd: None,
            },
            At::from_unix_millis(1),
        );
    }
    routes.save(Some(&path)).unwrap();
    let snapshot = crate::discovery::DiscoveryStoreSnapshot::default();
    let full = crate::discovery::lookup_sessions(&snapshot, Some(tmp.path()));
    for requested in ["alice", "bob", "shared-thread"] {
        let fast =
            crate::discovery::lookup_one_route_session(&snapshot, Some(tmp.path()), requested);
        let full_match = full
            .iter()
            .find(|s| s.id == requested || s.persona.as_deref() == Some(requested));
        assert_eq!(fast.as_ref(), full_match, "{requested}");
    }
}

// ---- item 3: lazy route lookup ----

mod route_lookup {
    use std::cell::RefCell;
    use std::os::unix::net::UnixListener;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use fl_core::at::At;
    use fl_wire::dispatcher::{
        PushError, ROUTE_REDISCOVERY_INTERVAL, push_operator_prompt_to_route,
    };
    use fl_wire::identity::{RouteTable, VendorRoute};
    use tempfile::TempDir;

    use crate::discovery::cost_counters::measure;
    use crate::discovery::{note_route_file_scan, route_table_path};
    use crate::lifecycle::{
        RouteLookup, RouteQuery, Undeliverable, dispatch_once, dispatch_with_rediscovery,
        lookup_route, undeliverable,
    };

    pub(super) fn routes_file(home: &Path) -> PathBuf {
        route_table_path(Some(home)).unwrap()
    }

    pub(super) fn save_route(home: &Path, persona: &str, route: VendorRoute) {
        let path = routes_file(home);
        let mut routes = RouteTable::load(Some(&path));
        routes.upsert(persona, route, At::from_unix_millis(1));
        routes.save(Some(&path)).unwrap();
    }

    /// The hub scanned the host and saved the scan to `home`'s route file just now.
    pub(super) fn scanned_just_now(home: &Path) {
        note_route_file_scan(Some(home), Instant::now());
    }

    /// A webhook route whose socket file exists (the cheap lookup check passes), with nothing
    /// listening on it: a connect is refused.
    fn refusing_webhook(home: &Path, name: &str) -> VendorRoute {
        let socket_path = home.join(name);
        drop(UnixListener::bind(&socket_path).unwrap());
        VendorRoute::Webhook { socket_path }
    }

    /// What each connection made to `listener` so far carried, in order. Every send under test
    /// has returned by then, so each of its connections waits in the listener's backlog.
    pub(super) fn connections(listener: &UnixListener) -> Vec<String> {
        use std::io::Read;
        listener.set_nonblocking(true).unwrap();
        let mut carried = Vec::new();
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    let mut text = String::new();
                    stream.read_to_string(&mut text).unwrap();
                    carried.push(text);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return carried,
                Err(e) => panic!("accept: {e}"),
            }
        }
    }

    /// A webhook route with a listener; the listener must outlive the test's dispatch.
    fn listening_webhook(home: &Path, name: &str) -> (VendorRoute, UnixListener) {
        let socket_path = home.join(name);
        let listener = UnixListener::bind(&socket_path).unwrap();
        (VendorRoute::Webhook { socket_path }, listener)
    }

    pub(super) fn agy(conversation_id: &str) -> VendorRoute {
        VendorRoute::Agy {
            conversation_id: conversation_id.into(),
            ls_pid: None,
            ls_address: None,
            csrf_token: None,
            agentapi_exe: None,
        }
    }

    fn claude(session_id: &str, pid: u32) -> VendorRoute {
        VendorRoute::Claude {
            pid,
            session_id: session_id.into(),
            socket_path: PathBuf::from("/nonexistent/claude.sock"),
            key_path: None,
        }
    }

    /// The host now runs Claude session `session_id` for `persona` (its session record).
    fn host_runs_claude(home: &Path, persona: &str, session_id: &str) {
        let sessions = home.join(".claude").join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(
            sessions.join("4242.json"),
            serde_json::json!({
                "name": persona,
                "sessionId": session_id,
                "messagingSocketPath": home.join("new.sock"),
            })
            .to_string(),
        )
        .unwrap();
    }

    fn by_persona(persona: &str) -> RouteQuery<'_> {
        RouteQuery {
            persona: Some(persona),
            fallback_session_id: persona,
            session_id: None,
        }
    }

    fn claude_session_id(route: &VendorRoute) -> &str {
        match route {
            VendorRoute::Claude { session_id, .. } => session_id,
            _ => "",
        }
    }

    fn conversation(found: &Option<RouteLookup>) -> Option<(bool, String)> {
        match found {
            Some(RouteLookup::Saved(VendorRoute::Agy {
                conversation_id, ..
            })) => Some((false, conversation_id.clone())),
            Some(RouteLookup::Scanned(VendorRoute::Agy {
                conversation_id, ..
            })) => Some((true, conversation_id.clone())),
            _ => None,
        }
    }

    // -- which saved routes a lookup trusts --

    #[test]
    fn a_fresh_saved_route_that_serves_the_session_is_used_without_a_scan() {
        let home = TempDir::new().unwrap();
        save_route(home.path(), "ada", agy("conv-ada"));
        scanned_just_now(home.path());
        let query = RouteQuery {
            persona: Some("ada"),
            fallback_session_id: "conv-ada",
            session_id: Some("conv-ada"),
        };
        let (found, costs) = measure(|| lookup_route(Some(home.path()), query));
        assert_eq!(found, Some(RouteLookup::Saved(agy("conv-ada"))));
        assert_eq!(costs.route_scans, 0);
        // By persona alone too.
        let (found, costs) = measure(|| lookup_route(Some(home.path()), by_persona("ada")));
        assert_eq!(found, Some(RouteLookup::Saved(agy("conv-ada"))));
        assert_eq!(costs.route_scans, 0);
        // A route that names no session (a webhook) serves any session id.
        let (hook, _listener) = listening_webhook(home.path(), "bob.sock");
        save_route(home.path(), "bob", hook.clone());
        let query = RouteQuery {
            persona: Some("bob"),
            fallback_session_id: "webhook-bob",
            session_id: Some("webhook-bob"),
        };
        let (found, costs) = measure(|| lookup_route(Some(home.path()), query));
        assert_eq!(
            (found, costs.route_scans),
            (Some(RouteLookup::Saved(hook)), 0)
        );
    }

    #[test]
    fn a_saved_route_to_the_old_conversation_rescans_for_the_new_session() {
        let home = TempDir::new().unwrap();
        // The saved table still names ada's old conversation; the host runs her new one.
        save_route(home.path(), "ada", claude("old-sid", 0));
        host_runs_claude(home.path(), "ada", "new-sid");
        scanned_just_now(home.path());
        let query = RouteQuery {
            persona: Some("ada"),
            fallback_session_id: "new-sid",
            session_id: Some("new-sid"),
        };
        let (found, costs) = measure(|| lookup_route(Some(home.path()), query));
        let Some(RouteLookup::Scanned(route)) = found else {
            panic!("expected a scanned route, got {found:?}");
        };
        assert_eq!(claude_session_id(&route), "new-sid");
        assert_eq!(costs.route_scans, 1);
        // The scan was saved: the next lookup for the new session needs none.
        let (again, costs) = measure(|| lookup_route(Some(home.path()), query));
        assert!(
            matches!(again, Some(RouteLookup::Saved(ref r)) if claude_session_id(r) == "new-sid")
        );
        assert_eq!(costs.route_scans, 0);
    }

    #[test]
    fn a_stale_live_webhook_is_rescanned_without_connecting() {
        let home = TempDir::new().unwrap();
        let (route, _listener) = listening_webhook(home.path(), "live.sock");
        save_route(home.path(), "ada", route.clone());
        let long_ago = Instant::now()
            .checked_sub(ROUTE_REDISCOVERY_INTERVAL + Duration::from_secs(1))
            .unwrap();
        note_route_file_scan(Some(home.path()), long_ago);
        let (found, costs) = measure(|| lookup_route(Some(home.path()), by_persona("ada")));
        assert_eq!(found, Some(RouteLookup::Scanned(route)));
        assert_eq!(
            costs.route_scans, 1,
            "a pathname cannot verify listener identity"
        );
        assert!(
            connections(&_listener).is_empty(),
            "lookup must not connect"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_rebound_non_listening_webhook_does_not_reuse_the_old_listener() {
        use std::os::fd::{FromRawFd, OwnedFd};
        use std::os::unix::ffi::OsStrExt;

        let home = TempDir::new().unwrap();
        let socket_path = home.path().join("rebound.sock");
        let old_listener = UnixListener::bind(&socket_path).unwrap();
        std::fs::remove_file(&socket_path).unwrap();
        // Bind another stream socket at the same pathname, but deliberately do not listen.
        // SAFETY: socket returns an owned descriptor; sockaddr_un is initialized before bind.
        let raw_fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
        assert!(raw_fd >= 0, "{}", std::io::Error::last_os_error());
        // SAFETY: socket returned a valid, uniquely owned descriptor.
        let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
        let path_bytes = socket_path.as_os_str().as_bytes();
        // SAFETY: zero is a valid initial representation for sockaddr_un.
        let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        assert!(path_bytes.len() < addr.sun_path.len());
        for (dest, src) in addr.sun_path.iter_mut().zip(path_bytes) {
            *dest = *src as libc::c_char;
        }
        // SAFETY: addr is a fully initialized sockaddr_un and remains alive for the call.
        let ret = unsafe {
            libc::bind(
                std::os::fd::AsRawFd::as_raw_fd(&fd),
                (&raw const addr).cast(),
                std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
            )
        };
        assert_eq!(ret, 0, "{}", std::io::Error::last_os_error());
        assert!(std::os::unix::net::UnixStream::connect(&socket_path).is_err());
        save_route(home.path(), "ada", VendorRoute::Webhook { socket_path });
        host_runs_claude(home.path(), "ada", "new-sid");
        let long_ago = Instant::now()
            .checked_sub(ROUTE_REDISCOVERY_INTERVAL + Duration::from_secs(1))
            .unwrap();
        note_route_file_scan(Some(home.path()), long_ago);
        let (found, costs) = measure(|| lookup_route(Some(home.path()), by_persona("ada")));
        assert!(
            matches!(found, Some(RouteLookup::Scanned(ref route)) if claude_session_id(route) == "new-sid"),
            "{found:?}"
        );
        assert_eq!(costs.route_scans, 1);
        assert!(
            connections(&old_listener).is_empty(),
            "liveness must not send a probe"
        );
    }

    #[test]
    fn a_stale_webhook_without_a_listener_rescans_the_live_route() {
        let home = TempDir::new().unwrap();
        let old = refusing_webhook(home.path(), "stale.sock");
        save_route(home.path(), "ada", old);
        host_runs_claude(home.path(), "ada", "new-sid");
        let replacement = UnixListener::bind(home.path().join("new.sock")).unwrap();
        let long_ago = Instant::now()
            .checked_sub(ROUTE_REDISCOVERY_INTERVAL + Duration::from_secs(1))
            .unwrap();
        note_route_file_scan(Some(home.path()), long_ago);
        let (found, costs) = measure(|| lookup_route(Some(home.path()), by_persona("ada")));
        let Some(RouteLookup::Scanned(route)) = found else {
            panic!("expected rescan: {found:?}");
        };
        assert_eq!(claude_session_id(&route), "new-sid");
        assert_eq!(costs.route_scans, 1);
        let chosen = dispatch_once(Some(home.path()), by_persona("ada"), |route| {
            Ok::<_, String>(route.clone())
        });
        assert!(matches!(chosen, Some(Ok(ref route)) if claude_session_id(route) == "new-sid"));
        assert!(
            connections(&replacement).is_empty(),
            "route verification must not connect"
        );
    }

    #[test]
    fn an_old_unverifiable_route_is_rescanned() {
        let home = TempDir::new().unwrap();
        save_route(home.path(), "ada", agy("conv-ada"));
        let long_ago = Instant::now()
            .checked_sub(ROUTE_REDISCOVERY_INTERVAL + Duration::from_secs(1))
            .unwrap();
        note_route_file_scan(Some(home.path()), long_ago);
        let (_, costs) = measure(|| lookup_route(Some(home.path()), by_persona("ada")));
        assert_eq!(costs.route_scans, 1);
    }

    #[test]
    fn a_rescan_cannot_return_a_different_session() {
        let home = TempDir::new().unwrap();
        save_route(home.path(), "ada", claude("old-sid", 0));
        host_runs_claude(home.path(), "ada", "old-sid");
        let query = RouteQuery {
            persona: Some("ada"),
            fallback_session_id: "new-sid",
            session_id: Some("new-sid"),
        };
        let (found, costs) = measure(|| lookup_route(Some(home.path()), query));
        assert_eq!(found, None);
        assert_eq!(costs.route_scans, 1);
    }

    #[test]
    fn a_miss_scans_once_and_saves_so_the_next_lookup_needs_no_scan() {
        let home = TempDir::new().unwrap();
        super::write_ferris_transcript(home.path(), 2);
        let (first, cold) = measure(|| lookup_route(Some(home.path()), by_persona("ferris")));
        assert_eq!(conversation(&first), Some((true, "conv-ferris-1".into())));
        assert_eq!(cold.route_scans, 1);
        let saved = RouteTable::load(Some(&routes_file(home.path())));
        assert!(saved.get("ferris").is_some(), "the scan was saved");
        let (second, warm) = measure(|| lookup_route(Some(home.path()), by_persona("ferris")));
        assert_eq!(conversation(&second), Some((false, "conv-ferris-1".into())));
        assert_eq!(warm.route_scans, 0);
        // By session id too, when no persona has a route.
        let (by_id, costs) = measure(|| {
            lookup_route(
                Some(home.path()),
                RouteQuery {
                    persona: None,
                    fallback_session_id: "conv-x",
                    session_id: None,
                },
            )
        });
        assert_eq!(by_id, None, "no route matches: scanned");
        assert_eq!(costs.route_scans, 1);
    }

    #[test]
    fn a_dead_saved_route_is_rescanned() {
        let home = TempDir::new().unwrap();
        let dead = VendorRoute::Webhook {
            socket_path: home.path().join("gone.sock"),
        };
        save_route(home.path(), "ada", dead.clone());
        scanned_just_now(home.path());
        let (found, costs) = measure(|| lookup_route(Some(home.path()), by_persona("ada")));
        // Discovery keeps a webhook route, so the scan finds the same one.
        assert_eq!(found, Some(RouteLookup::Scanned(dead)));
        assert_eq!(costs.route_scans, 1);
    }

    // -- the pre-delivery check and the single send --

    #[test]
    fn the_pre_delivery_check_names_each_refusal() {
        let home = TempDir::new().unwrap();
        assert_eq!(
            undeliverable(&VendorRoute::Webhook {
                socket_path: home.path().join("gone.sock")
            }),
            Some(Undeliverable::NoSocket)
        );
        // The check never connects, so a socket file that refuses passes it: the push finds the
        // refusal.
        assert_eq!(
            undeliverable(&refusing_webhook(home.path(), "dead.sock")),
            None
        );
        let (live, listener) = listening_webhook(home.path(), "live.sock");
        assert_eq!(undeliverable(&live), None);
        assert!(
            connections(&listener).is_empty(),
            "the check opened a connection to the receiver"
        );
        // No process can have this pid (it is above the kernel's pid limit).
        assert_eq!(
            undeliverable(&claude("sid", 4_000_000_000)),
            Some(Undeliverable::ProcessGone { pid: 4_000_000_000 })
        );
        assert_eq!(undeliverable(&claude("sid", std::process::id())), None);
        assert_eq!(undeliverable(&claude("sid", 0)), None, "no pid: no check");
        assert_eq!(undeliverable(&agy("conv")), None, "no cheap check");
    }

    #[test]
    fn a_prompt_to_a_missing_socket_goes_once_to_the_new_route() {
        let home = TempDir::new().unwrap();
        save_route(
            home.path(),
            "ada",
            VendorRoute::Webhook {
                socket_path: home.path().join("gone.sock"),
            },
        );
        host_runs_claude(home.path(), "ada", "new-sid");
        scanned_just_now(home.path());
        let tried = RefCell::new(Vec::new());
        let (outcome, costs) = measure(|| {
            dispatch_with_rediscovery(Some(home.path()), by_persona("ada"), |route| {
                tried.borrow_mut().push(route.clone());
                Ok("delivered")
            })
        });
        assert_eq!(outcome, Some(Ok("delivered")));
        let tried = tried.into_inner();
        assert_eq!(tried.len(), 1, "one send, and none to the missing socket");
        assert_eq!(claude_session_id(&tried[0]), "new-sid");
        assert_eq!(costs.route_scans, 1);
        let saved = RouteTable::load(Some(&routes_file(home.path())));
        assert_eq!(
            saved.get("ada").map(|e| claude_session_id(&e.route)),
            Some("new-sid")
        );
    }

    #[test]
    fn a_prompt_whose_connect_is_refused_goes_to_the_new_route() {
        let home = TempDir::new().unwrap();
        let old = refusing_webhook(home.path(), "old.sock");
        save_route(home.path(), "ada", old.clone());
        host_runs_claude(home.path(), "ada", "new-sid");
        scanned_just_now(home.path());
        let tried = RefCell::new(Vec::new());
        let (outcome, costs) = measure(|| {
            dispatch_with_rediscovery(Some(home.path()), by_persona("ada"), |route| {
                tried.borrow_mut().push(route.clone());
                match route {
                    // The real push: the refused connect delivered nothing.
                    VendorRoute::Webhook { .. } => {
                        push_operator_prompt_to_route(route, "owner", "hi").map(|()| "webhook")
                    }
                    _ => Ok("delivered"),
                }
            })
        });
        assert_eq!(outcome, Some(Ok("delivered")));
        let tried = tried.into_inner();
        assert_eq!(tried.len(), 2, "the refused push, then the new route");
        assert_eq!(tried[0], old);
        assert_eq!(claude_session_id(&tried[1]), "new-sid");
        assert_eq!(costs.route_scans, 1);
    }

    #[test]
    fn a_prompt_to_a_webhook_route_opens_exactly_one_connection() {
        let home = TempDir::new().unwrap();
        let (hook, listener) = listening_webhook(home.path(), "ada.sock");
        save_route(home.path(), "ada", hook);
        scanned_just_now(home.path());
        let outcome = dispatch_with_rediscovery(Some(home.path()), by_persona("ada"), |route| {
            push_operator_prompt_to_route(route, "owner", "hi")
        });
        assert_eq!(outcome, Some(Ok(())));
        let carried = connections(&listener);
        assert_eq!(carried.len(), 1, "one connection per push: {carried:?}");
        assert_eq!(carried[0].lines().count(), 1);
        assert!(carried[0].contains("\"operator_prompt\""));
    }

    #[test]
    fn a_failure_after_delivery_is_not_retried() {
        let home = TempDir::new().unwrap();
        let (hook, _listener) = listening_webhook(home.path(), "ada.sock");
        save_route(home.path(), "ada", hook.clone());
        // A newer route exists, so a retry would have somewhere to go.
        host_runs_claude(home.path(), "ada", "new-sid");
        scanned_just_now(home.path());
        let tried = RefCell::new(Vec::new());
        let (outcome, costs) = measure(|| {
            dispatch_with_rediscovery(Some(home.path()), by_persona("ada"), |route| {
                tried.borrow_mut().push(route.clone());
                Err::<(), _>(PushError::Failed(
                    "connection reset after the write".to_string(),
                ))
            })
        });
        assert_eq!(
            outcome,
            Some(Err("connection reset after the write".to_string()))
        );
        assert_eq!(tried.into_inner(), vec![hook]);
        assert_eq!(costs.route_scans, 0);
    }

    #[test]
    fn a_refused_route_with_no_newer_one_is_sent_once_as_it_is() {
        let home = TempDir::new().unwrap();
        let dead = refusing_webhook(home.path(), "dead.sock");
        save_route(home.path(), "ada", dead.clone());
        scanned_just_now(home.path());
        let tried = RefCell::new(Vec::new());
        let (outcome, costs) = measure(|| {
            dispatch_with_rediscovery(Some(home.path()), by_persona("ada"), |route| {
                tried.borrow_mut().push(route.clone());
                Err::<(), _>(PushError::NotConnected("refused".to_string()))
            })
        });
        assert_eq!(outcome, Some(Err("refused".to_string())));
        assert_eq!(
            tried.into_inner(),
            vec![dead],
            "the scan found the same route: fl-wire's refusal is returned"
        );
        assert_eq!(costs.route_scans, 1);

        // A route that a scan just found is sent once and not rescanned when it fails.
        let fresh = TempDir::new().unwrap();
        super::write_ferris_transcript(fresh.path(), 2);
        let sends = RefCell::new(0);
        let (_, costs) = measure(|| {
            dispatch_with_rediscovery(Some(fresh.path()), by_persona("ferris"), |_| {
                *sends.borrow_mut() += 1;
                Err::<(), _>(PushError::NotConnected("refused".to_string()))
            })
        });
        assert_eq!((*sends.borrow(), costs.route_scans), (1, 1));
    }

    #[test]
    fn a_reset_that_fails_after_its_slash_command_is_not_run_again() {
        let home = TempDir::new().unwrap();
        // The host has a newer route, so a retry would have somewhere to go: a reset still runs
        // once, on the route the lookup chose.
        save_route(home.path(), "ada", claude("old-sid", 0));
        host_runs_claude(home.path(), "ada", "new-sid");
        scanned_just_now(home.path());
        let tried = RefCell::new(Vec::new());
        let (outcome, costs) = measure(|| {
            dispatch_once(Some(home.path()), by_persona("ada"), |route| {
                tried
                    .borrow_mut()
                    .push(claude_session_id(route).to_string());
                Err::<(), _>("/clear landed; the reforge prompt failed".to_string())
            })
        });
        assert_eq!(
            outcome,
            Some(Err("/clear landed; the reforge prompt failed".to_string()))
        );
        assert_eq!(tried.into_inner(), vec!["old-sid".to_string()]);
        assert_eq!(costs.route_scans, 0);
    }

    #[test]
    fn saving_a_scan_keeps_a_route_registered_while_it_ran() {
        let home = TempDir::new().unwrap();
        let path = routes_file(home.path());
        save_route(home.path(), "old", agy("conv-old"));
        let base = RouteTable::load(Some(&path));
        let mut scanned = base.clone();
        scanned.upsert("found", agy("conv-found"), At::from_unix_millis(5));
        // The daemon registers a route after the scan started.
        save_route(home.path(), "registered", agy("conv-registered"));
        crate::discovery::save_route_scan(Some(home.path()), &base, &scanned, Instant::now());
        let saved = RouteTable::load(Some(&path));
        for persona in ["old", "found", "registered"] {
            assert!(saved.get(persona).is_some(), "{persona} kept");
        }
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "no temp files left");
    }

    #[test]
    fn a_route_a_teardown_removed_during_the_scan_stays_removed() {
        let home = TempDir::new().unwrap();
        let path = routes_file(home.path());
        save_route(home.path(), "ada", agy("conv-ada"));
        save_route(home.path(), "bob", agy("conv-bob"));
        let base = RouteTable::load(Some(&path));
        // The scan left `ada` as it was and learned `found`.
        let mut scanned = base.clone();
        scanned.upsert("found", agy("conv-found"), At::from_unix_millis(5));
        // A teardown removes `ada` while the scan runs.
        assert_eq!(
            crate::lifecycle::remove_routes(Some(home.path()), &["ada"], &[]),
            1
        );
        crate::discovery::save_route_scan(Some(home.path()), &base, &scanned, Instant::now());
        let saved = RouteTable::load(Some(&path));
        assert!(saved.get("ada").is_none(), "the removal is not undone");
        assert!(saved.get("bob").is_some() && saved.get("found").is_some());
    }

    #[test]
    fn a_route_the_scan_changed_is_saved_even_when_removed_meanwhile() {
        let home = TempDir::new().unwrap();
        let path = routes_file(home.path());
        save_route(home.path(), "ada", agy("conv-old"));
        let base = RouteTable::load(Some(&path));
        let mut scanned = base.clone();
        // The scan found ada's new conversation: that is news, not a stale copy.
        scanned.upsert("ada", agy("conv-new"), At::from_unix_millis(9));
        crate::lifecycle::remove_routes(Some(home.path()), &["ada"], &[]);
        crate::discovery::save_route_scan(Some(home.path()), &base, &scanned, Instant::now());
        let saved = RouteTable::load(Some(&path));
        assert_eq!(saved.get("ada").map(|e| &e.route), Some(&agy("conv-new")));
    }

    #[test]
    fn a_teardown_waits_for_a_hub_save_in_progress() {
        let home = TempDir::new().unwrap();
        let path = routes_file(home.path());
        save_route(home.path(), "ada", agy("conv-ada"));
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let remover = crate::discovery::with_route_file_lock(|| {
            let home = home.path().to_path_buf();
            let remover = std::thread::spawn(move || {
                started_tx.send(()).unwrap();
                crate::lifecycle::remove_routes(Some(&home), &["ada"], &[])
            });
            started_rx.recv().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(20));
            assert!(
                RouteTable::load(Some(&path)).get("ada").is_some(),
                "the removal waits while another hub writer holds the file"
            );
            remover
        });
        assert_eq!(remover.join().unwrap(), 1);
        assert!(RouteTable::load(Some(&path)).get("ada").is_none());
    }
}

mod wire_send {
    use std::io::BufRead;
    use std::os::unix::net::UnixListener;

    use fl_core::at::At;
    use fl_wire::identity::{RouteTable, VendorRoute};
    use serde_json::{Value, json};
    use tempfile::TempDir;

    use super::{OWNER, hub_with_ferris};
    use crate::discovery::cost_counters::{Costs, measure};
    use crate::discovery::route_table_path;
    use crate::{HttpRequest, HubState, handle_http_request};

    fn save_webhook(state: &HubState, persona: &str, socket: &std::path::Path) {
        let path = route_table_path(state.config.home_override.as_deref()).unwrap();
        let mut routes = RouteTable::load(Some(&path));
        routes.upsert(
            persona,
            VendorRoute::Webhook {
                socket_path: socket.to_path_buf(),
            },
            At::from_unix_millis(1),
        );
        routes.save(Some(&path)).unwrap();
    }

    fn send(state: &HubState, to: &str) -> (Value, Costs) {
        let (resp, costs) = measure(|| {
            handle_http_request(
                state,
                &HttpRequest::new("POST", "/api/v1/wire/send")
                    .with_header("Authorization", OWNER)
                    .with_json_body(&json!({ "to": to, "subject": "s", "body": "b" })),
            )
        });
        assert_eq!(resp.status, 200);
        (resp.json_value().unwrap(), costs)
    }

    /// The line the first connection to `listener` carries (fl-wire's webhook contract: one
    /// connection per JSON line, so it must carry one).
    fn first_line(listener: UnixListener) -> std::thread::JoinHandle<String> {
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            std::io::BufReader::new(stream)
                .read_line(&mut line)
                .unwrap();
            assert!(!line.is_empty(), "the first connection carried no line");
            line
        })
    }

    #[test]
    fn a_fresh_saved_route_delivers_without_a_scan() {
        let tmp = TempDir::new().unwrap();
        let state = hub_with_ferris(&tmp, 1);
        let socket = tmp.path().join("ada.sock");
        let reader = first_line(UnixListener::bind(&socket).unwrap());
        save_webhook(&state, "ada", &socket);
        super::route_lookup::scanned_just_now(tmp.path());
        let (body, costs) = send(&state, "ada");
        assert!(reader.join().unwrap().contains("\"b\""));
        assert_eq!(body["receipts"][0]["status"], "acked");
        assert_eq!(costs.route_scans, 0);
    }

    #[test]
    fn a_stale_saved_table_is_scanned_once_before_the_push() {
        let tmp = TempDir::new().unwrap();
        let state = hub_with_ferris(&tmp, 1);
        let socket = tmp.path().join("ada.sock");
        let reader = first_line(UnixListener::bind(&socket).unwrap());
        save_webhook(&state, "ada", &socket);
        let long_ago = std::time::Instant::now()
            .checked_sub(
                fl_wire::dispatcher::ROUTE_REDISCOVERY_INTERVAL + std::time::Duration::from_secs(1),
            )
            .unwrap();
        crate::discovery::note_route_file_scan(Some(tmp.path()), long_ago);
        let (body, costs) = send(&state, "ada");
        assert!(reader.join().unwrap().contains("\"b\""));
        assert_eq!(body["receipts"][0]["status"], "acked");
        assert_eq!(
            costs.route_scans, 1,
            "every recipient had a route, the table was old"
        );
        // The scan was saved: the next send needs none.
        let socket_two = tmp.path().join("ada-2.sock");
        let reader = first_line(UnixListener::bind(&socket_two).unwrap());
        save_webhook(&state, "ada", &socket_two);
        let (_, costs) = send(&state, "ada");
        assert!(reader.join().unwrap().contains("\"b\""));
        assert_eq!(costs.route_scans, 0);
    }

    #[test]
    fn a_recipient_with_no_saved_route_scans_once() {
        let tmp = TempDir::new().unwrap();
        let state = hub_with_ferris(&tmp, 1);
        let (body, costs) = send(&state, "nobody");
        assert_eq!(body["receipts"][0]["status"], "pending");
        assert_eq!(costs.route_scans, 1);
    }

    #[test]
    fn a_saved_route_that_refuses_scans_once_and_stays_pending() {
        let tmp = TempDir::new().unwrap();
        let state = hub_with_ferris(&tmp, 1);
        save_webhook(&state, "ada", &tmp.path().join("gone.sock"));
        super::route_lookup::scanned_just_now(tmp.path());
        let (body, costs) = send(&state, "ada");
        assert_eq!(body["receipts"][0]["status"], "pending");
        assert_eq!(
            body["receipts"][0]["attempts"], 1,
            "one push, never repeated"
        );
        assert_eq!(
            costs.route_scans, 1,
            "the pre-delivery check refused: one rescan"
        );

        // A socket file that refuses: the push itself finds the refusal, scans once, and the
        // scan finds the same route, so the receipt stays pending after one push.
        let socket = tmp.path().join("dead.sock");
        drop(UnixListener::bind(&socket).unwrap());
        save_webhook(&state, "ada", &socket);
        super::route_lookup::scanned_just_now(tmp.path());
        let (body, costs) = send(&state, "ada");
        assert_eq!(body["receipts"][0]["status"], "pending");
        assert_eq!(body["receipts"][0]["attempts"], 1);
        assert_eq!(costs.route_scans, 1, "the refused push: one rescan");
    }

    #[test]
    fn a_push_to_a_webhook_route_opens_exactly_one_connection() {
        let tmp = TempDir::new().unwrap();
        let state = hub_with_ferris(&tmp, 1);
        let socket = tmp.path().join("ada.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        save_webhook(&state, "ada", &socket);
        super::route_lookup::scanned_just_now(tmp.path());
        let (body, costs) = send(&state, "ada");
        assert_eq!(body["receipts"][0]["status"], "acked");
        assert_eq!(costs.route_scans, 0);
        let carried = super::route_lookup::connections(&listener);
        assert_eq!(carried.len(), 1, "one connection per push: {carried:?}");
        assert_eq!(carried[0].lines().count(), 1);
        assert!(carried[0].contains("\"b\""));
    }

    #[test]
    fn a_send_with_nothing_to_deliver_never_scans() {
        let tmp = TempDir::new().unwrap();
        let state = hub_with_ferris(&tmp, 1);
        // Never scanned in this process, so the saved table is not fresh: a send with a
        // recipient to deliver would scan. A send with no receipt to deliver must not.
        assert!(!crate::discovery::saved_routes_fresh(
            Some(tmp.path()),
            std::time::Instant::now()
        ));
        let (body, costs) = send(&state, "@area:no-such-area");
        assert_eq!(body["receipts"], json!([]));
        assert_eq!(costs.route_scans, 0);
        // The same stale table, with a recipient to deliver: one scan.
        let (_, costs) = send(&state, "nobody");
        assert_eq!(costs.route_scans, 1);
    }
}

// ---- item 4: one bulk receipt read for the wire message list ----

mod wire_messages {
    use fl_core::agent::{AgentRole, RegisterAgentArgs};
    use fl_core::at::At;
    use fl_core::ferrowire::{
        DeliveryStatus, MessageKind, MessageUrgency, SendWireArgs, WireAddress,
    };
    use fl_core::store::{AgentRegistry, MessageStore};
    use fl_store::{AnyStore, BackendKind};
    use serde_json::{Value, json};
    use tempfile::TempDir;

    use super::{get, hub_with_ferris};
    use crate::{HttpResponse, HubState};

    /// The list as the handler built it before: one `list_wire_receipts` per message.
    fn per_message_reference(store: &AnyStore, unread_only: bool) -> Vec<u8> {
        let mut out = Vec::new();
        for (_seq, msg) in store.wire_messages_after(0).unwrap() {
            let receipts = store.list_wire_receipts(&msg.id).unwrap_or_default();
            if unread_only && receipts.iter().all(|r| r.status == DeliveryStatus::Acked) {
                continue;
            }
            out.push(json!({ "message": msg, "receipts": receipts }));
        }
        HttpResponse::json(200, json!({ "messages": out })).body
    }

    fn send(store: &AnyStore, to: WireAddress, subject: &str, at: u64) -> fl_core::ids::MessageId {
        store
            .send_wire(SendWireArgs {
                id: None,
                sender_id: None,
                sender_name: "jarvis".into(),
                sender_host: None,
                to,
                kind: MessageKind::Mail,
                urgency: MessageUrgency::Normal,
                durable: true,
                subject: subject.into(),
                body: format!("body of {subject}"),
                reply_to: None,
                thread_id: None,
                deadline_at: None,
                at: At::from_unix_millis(at),
            })
            .unwrap()
            .0
            .id
    }

    fn seed(state: &HubState) -> usize {
        let store = state.open_store().unwrap();
        // Registered in an order unlike their ids, so receipts sort by `recipient_id`.
        for name in ["Cy", "Ada", "Bob"] {
            store
                .register_agent(RegisterAgentArgs {
                    display_name: name.into(),
                    email: None,
                    project: "FerroLoop".into(),
                    role: AgentRole::Assistant,
                    extra_areas: Vec::new(),
                    create_area: true,
                    replace: false,
                    at: At::from_unix_millis(1),
                    by: "owner".into(),
                })
                .unwrap();
        }
        let all = send(&store, WireAddress::All, "to everyone", 10);
        let one = send(&store, WireAddress::AgentName("ada".into()), "to ada", 20);
        let unregistered = send(&store, WireAddress::AgentName("zed".into()), "to zed", 30);
        let nobody = send(
            &store,
            WireAddress::Area("no-such-area".into()),
            "to nobody",
            40,
        );
        assert!(
            store.list_wire_receipts(&nobody).unwrap().is_empty(),
            "a message with no receipts"
        );
        let acked = DeliveryStatus::Acked;
        store
            .update_wire_receipt(&all, "ada", acked, false, At::from_unix_millis(50))
            .unwrap();
        store
            .update_wire_receipt(
                &all,
                "bob",
                DeliveryStatus::Expired,
                true,
                At::from_unix_millis(51),
            )
            .unwrap();
        store
            .update_wire_receipt(
                &all,
                "cy",
                DeliveryStatus::Delivered,
                true,
                At::from_unix_millis(52),
            )
            .unwrap();
        store
            .update_wire_receipt(&one, "ada", acked, false, At::from_unix_millis(53))
            .unwrap();
        assert_eq!(store.list_wire_receipts(&unregistered).unwrap().len(), 1);
        assert!(
            store.list_wire_receipts(&all).unwrap().len() >= 3,
            "several recipients"
        );
        store.wire_messages_after(0).unwrap().len()
    }

    #[test]
    fn the_bulk_join_answers_exactly_as_the_per_message_reads_on_every_engine() {
        for (kind, file) in [
            (BackendKind::Redb, "hub.redb"),
            (BackendKind::Ferro, "hub.fls"),
        ] {
            let tmp = TempDir::new().unwrap();
            let mut state = hub_with_ferris(&tmp, 1);
            state.store_backend = Some(kind);
            state.config.store_path = tmp.path().join(file);
            let messages = seed(&state);
            assert_eq!(messages, 4, "{kind:?}");
            for (query, unread) in [("", false), ("?unread=true", true)] {
                let (resp, _) = get(&state, &format!("/api/v1/wire/messages{query}"));
                assert_eq!(resp.status, 200);
                let expected = per_message_reference(&state.open_store().unwrap(), unread);
                assert_eq!(
                    String::from_utf8_lossy(&resp.body),
                    String::from_utf8_lossy(&expected),
                    "{kind:?} {query}"
                );
                let listed: Value = resp.json_value().unwrap();
                let subjects: Vec<_> = listed["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|m| m["message"]["subject"].as_str().unwrap().to_string())
                    .collect();
                let want: &[&str] = if unread {
                    // A message whose receipts are all acked, or that has none, is read.
                    &["to everyone", "to zed"]
                } else {
                    &["to everyone", "to ada", "to zed", "to nobody"]
                };
                assert_eq!(subjects, want, "{kind:?} {query}");
            }
        }
    }
}
