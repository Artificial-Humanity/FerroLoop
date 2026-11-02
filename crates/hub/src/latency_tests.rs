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
        let first = cache.get_or_load("k", t0, || counted_load(&loads, "a"));
        assert_eq!(first, Ok(vec!["a".to_string()]));
        let hit = cache.get_or_load("k", t0 + TTL - Duration::from_millis(1), || {
            counted_load(&loads, "b")
        });
        assert_eq!(hit, Ok(vec!["a".to_string()]), "inside the TTL: cached");
        assert_eq!(loads.load(Ordering::SeqCst), 1);
        let expired = cache.get_or_load("k", t0 + TTL, || counted_load(&loads, "c"));
        assert_eq!(
            expired,
            Ok(vec!["c".to_string()]),
            "at the TTL: loaded again"
        );
        assert_eq!(loads.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_caller_changing_its_copy_does_not_change_the_cache() {
        let cache = TtlCache::new(TTL);
        let t0 = Instant::now();
        let mut mine = cache
            .get_or_load("k", t0, || Ok::<_, ()>(vec!["a".to_string()]))
            .unwrap();
        mine.push("leak".to_string());
        mine[0] = "changed".to_string();
        let again = cache
            .get_or_load("k", t0, || Ok::<_, ()>(vec!["reloaded".to_string()]))
            .unwrap();
        assert_eq!(again, vec!["a".to_string()]);
    }

    #[test]
    fn invalidate_forces_the_next_read_to_load() {
        let cache = TtlCache::new(TTL);
        let loads = AtomicUsize::new(0);
        let t0 = Instant::now();
        cache
            .get_or_load("k", t0, || counted_load(&loads, "a"))
            .unwrap();
        cache.invalidate();
        let after = cache.get_or_load("k", t0, || counted_load(&loads, "b"));
        assert_eq!(after, Ok(vec!["b".to_string()]));
        assert_eq!(loads.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn another_key_or_a_failed_load_is_not_served_from_the_cache() {
        let cache = TtlCache::new(TTL);
        let t0 = Instant::now();
        cache
            .get_or_load("k", t0, || Ok::<_, String>(vec!["a".into()]))
            .unwrap();
        let other = cache.get_or_load("j", t0, || Ok::<_, String>(vec!["j".into()]));
        assert_eq!(other, Ok(vec!["j".to_string()]), "a different key loads");
        let failed = cache.get_or_load("x", t0, || Err::<Vec<String>, _>("down".to_string()));
        assert_eq!(failed, Err("down".to_string()));
        let next = cache.get_or_load("x", t0, || Ok::<_, String>(vec!["x".into()]));
        assert_eq!(next, Ok(vec!["x".to_string()]), "an error is not cached");
    }

    #[test]
    fn an_invalidation_during_a_load_is_not_lost() {
        let cache = TtlCache::new(TTL);
        let t0 = Instant::now();
        // The load overlaps a lifecycle change: its result predates the change.
        let during = cache.get_or_load("k", t0, || {
            cache.invalidate();
            Ok::<_, ()>(vec!["before-change".to_string()])
        });
        assert_eq!(during, Ok(vec!["before-change".to_string()]));
        let next = cache.get_or_load("k", t0, || Ok::<_, ()>(vec!["after".to_string()]));
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
                        .get_or_load("k", t0, || {
                            loads.fetch_add(1, Ordering::SeqCst);
                            std::thread::sleep(Duration::from_millis(20));
                            Ok::<_, ()>(vec!["scan".to_string()])
                        })
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
fn send_now_skips_the_busy_parse_and_a_prompt_takes_one_store_snapshot() {
    let tmp = TempDir::new().unwrap();
    let state = hub_with_ferris(&tmp, 3);
    let (queued_check, checked) = post_prompt(&state, false);
    assert_eq!(queued_check.json_value().unwrap()["status"], "sent");
    // Busy check, then the in-flight mark after the dispatch.
    assert_eq!(checked.transcript_parses, 2);
    assert_eq!(checked.store_snapshots, 1);

    let (sent, skipped) = post_prompt(&state, true);
    let body = sent.json_value().unwrap();
    assert_eq!(body["status"], "sent");
    assert_eq!(body["session_id"], "conv-ferris-1");
    assert_eq!(
        skipped.transcript_parses, 1,
        "only the in-flight mark parses"
    );
    assert_eq!(skipped.store_snapshots, 1);

    // The in-flight mark still took: a prompt without send_now now queues.
    let (queued, _) = post_prompt(&state, false);
    assert_eq!(queued.json_value().unwrap()["status"], "queued");
}

// ---- item 3: lazy route lookup ----

mod route_lookup {
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};

    use fl_core::at::At;
    use fl_wire::identity::{RouteTable, VendorRoute};
    use tempfile::TempDir;

    use crate::discovery::cost_counters::measure;
    use crate::discovery::route_table_path;
    use crate::lifecycle::{RouteLookup, dispatch_with_rediscovery, lookup_route};

    fn routes_file(home: &Path) -> PathBuf {
        route_table_path(Some(home)).unwrap()
    }

    fn save_route(home: &Path, persona: &str, route: VendorRoute) {
        let path = routes_file(home);
        let mut routes = RouteTable::load(Some(&path));
        routes.upsert(persona, route, At::from_unix_millis(1));
        routes.save(Some(&path)).unwrap();
    }

    fn webhook(home: &Path, name: &str, live: bool) -> VendorRoute {
        let socket_path = home.join(name);
        if live {
            std::fs::write(&socket_path, b"").unwrap();
        }
        VendorRoute::Webhook { socket_path }
    }

    fn agy(conversation_id: &str) -> VendorRoute {
        VendorRoute::Agy {
            conversation_id: conversation_id.into(),
            ls_pid: None,
            ls_address: None,
            csrf_token: None,
            agentapi_exe: None,
        }
    }

    #[test]
    fn a_live_saved_route_is_used_without_a_scan() {
        let home = TempDir::new().unwrap();
        let route = webhook(home.path(), "ada.sock", true);
        save_route(home.path(), "ada", route.clone());
        let (found, costs) = measure(|| lookup_route(Some(home.path()), Some("ada"), "ada"));
        assert_eq!(found, Some(RouteLookup::Saved(route.clone())));
        assert_eq!(costs.route_scans, 0);
        // By session id too, when the persona has no entry.
        let (by_id, costs) = measure(|| lookup_route(Some(home.path()), None, "conv-x"));
        assert_eq!(by_id, None, "no route matches: scanned");
        assert_eq!(costs.route_scans, 1);
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

    #[test]
    fn a_miss_scans_once_and_saves_so_the_next_lookup_needs_no_scan() {
        let home = TempDir::new().unwrap();
        super::write_ferris_transcript(home.path(), 2);
        let (first, cold) = measure(|| lookup_route(Some(home.path()), Some("ferris"), "ferris"));
        assert_eq!(conversation(&first), Some((true, "conv-ferris-1".into())));
        assert_eq!(cold.route_scans, 1);
        let saved = RouteTable::load(Some(&routes_file(home.path())));
        assert!(saved.get("ferris").is_some(), "the scan was saved");
        let (second, warm) = measure(|| lookup_route(Some(home.path()), Some("ferris"), "ferris"));
        assert_eq!(conversation(&second), Some((false, "conv-ferris-1".into())));
        assert_eq!(warm.route_scans, 0);
    }

    #[test]
    fn a_dead_saved_route_is_rescanned() {
        let home = TempDir::new().unwrap();
        let dead = webhook(home.path(), "gone.sock", false);
        save_route(home.path(), "ada", dead.clone());
        let (found, costs) = measure(|| lookup_route(Some(home.path()), Some("ada"), "ada"));
        // Discovery keeps a webhook route, so the scan finds the same one.
        assert_eq!(found, Some(RouteLookup::Scanned(dead)));
        assert_eq!(costs.route_scans, 1);
    }

    fn claude_session_id(route: &VendorRoute) -> &str {
        match route {
            VendorRoute::Claude { session_id, .. } => session_id,
            _ => "",
        }
    }

    #[test]
    fn a_saved_route_that_refuses_is_rescanned_and_retried_once_on_the_new_route() {
        let home = TempDir::new().unwrap();
        // The session restarted: the host now runs `new-sid`, the saved route names `old-sid`.
        let sessions = home.path().join(".claude").join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(
            sessions.join("4242.json"),
            serde_json::json!({
                "name": "ada",
                "sessionId": "new-sid",
                "messagingSocketPath": home.path().join("new.sock"),
            })
            .to_string(),
        )
        .unwrap();
        save_route(
            home.path(),
            "ada",
            VendorRoute::Claude {
                pid: 7,
                session_id: "old-sid".into(),
                socket_path: home.path().join("old.sock"),
                key_path: None,
            },
        );
        let tried = RefCell::new(Vec::new());
        let (outcome, costs) = measure(|| {
            dispatch_with_rediscovery(Some(home.path()), Some("ada"), "ada", |route| {
                let sid = claude_session_id(route).to_string();
                tried.borrow_mut().push(sid.clone());
                if sid == "old-sid" {
                    Err("stale".to_string())
                } else {
                    Ok("delivered")
                }
            })
        });
        assert_eq!(outcome, Some(Ok("delivered")));
        assert_eq!(
            *tried.borrow(),
            vec!["old-sid".to_string(), "new-sid".to_string()]
        );
        assert_eq!(costs.route_scans, 1);
        let saved = RouteTable::load(Some(&routes_file(home.path())));
        assert_eq!(
            saved.get("ada").map(|e| claude_session_id(&e.route)),
            Some("new-sid")
        );
    }

    #[test]
    fn a_refusal_is_retried_at_most_once_and_never_on_the_same_route() {
        let home = TempDir::new().unwrap();
        let route = webhook(home.path(), "ada.sock", true);
        save_route(home.path(), "ada", route.clone());
        let sends = RefCell::new(0);
        let (outcome, costs) = measure(|| {
            dispatch_with_rediscovery(Some(home.path()), Some("ada"), "ada", |_| {
                *sends.borrow_mut() += 1;
                Err::<(), _>("refused".to_string())
            })
        });
        assert_eq!(outcome, Some(Err("refused".to_string())));
        assert_eq!(
            *sends.borrow(),
            1,
            "the scan found the same route: no second push"
        );
        assert_eq!(costs.route_scans, 1);

        // A route that a scan just found is not rescanned when it refuses.
        let fresh = TempDir::new().unwrap();
        super::write_ferris_transcript(fresh.path(), 2);
        let sends = RefCell::new(0);
        let (_, costs) = measure(|| {
            dispatch_with_rediscovery(Some(fresh.path()), Some("ferris"), "ferris", |_| {
                *sends.borrow_mut() += 1;
                Err::<(), _>("refused".to_string())
            })
        });
        assert_eq!((*sends.borrow(), costs.route_scans), (1, 1));
    }

    #[test]
    fn saving_a_scan_keeps_a_route_registered_while_it_ran() {
        let home = TempDir::new().unwrap();
        let path = routes_file(home.path());
        save_route(home.path(), "old", webhook(home.path(), "old.sock", true));
        let base = RouteTable::load(Some(&path));
        let mut scanned = base.clone();
        scanned.upsert("found", agy("conv-found"), At::from_unix_millis(5));
        // The daemon registers a route after the scan started.
        save_route(
            home.path(),
            "registered",
            webhook(home.path(), "reg.sock", true),
        );
        crate::discovery::save_route_scan(Some(home.path()), &base, &scanned);
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

    #[test]
    fn a_saved_route_delivers_without_a_scan() {
        let tmp = TempDir::new().unwrap();
        let state = hub_with_ferris(&tmp, 1);
        let socket = tmp.path().join("ada.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let reader = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            std::io::BufReader::new(stream)
                .read_line(&mut line)
                .unwrap();
            line
        });
        save_webhook(&state, "ada", &socket);
        let (body, costs) = send(&state, "ada");
        assert!(reader.join().unwrap().contains("\"b\""));
        assert_eq!(body["receipts"][0]["status"], "acked");
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
        let (body, costs) = send(&state, "ada");
        assert_eq!(body["receipts"][0]["status"], "pending");
        assert_eq!(body["receipts"][0]["attempts"], 1);
        assert_eq!(
            costs.route_scans, 1,
            "one rescan, no retry on the same route"
        );
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
