//! The hub's connection caps (`ws::ConnectionLimits`) and the rejected-session cache
//! (`auth::SessionMissCache`) as the HTTP and WebSocket layers see them.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use fl_core::pr_policy::ReviewerApps;
use serde_json::json;
use tempfile::TempDir;

use crate::discovery::cost_counters::measure;
use crate::{
    ConnectionLimits, HttpRequest, HttpResponse, HubServer, HubServerConfig, HubState, HubWsClient,
    handle_http_request,
};

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

// ---- P1-7: the rejected-session cache ----------------------------------------------------

fn session_probe(state: &HubState, bearer: &str) -> HttpResponse {
    handle_http_request(
        state,
        &HttpRequest::new("GET", "/api/v1/auth/session")
            .with_header("Authorization", format!("Bearer {bearer}")),
    )
}

/// Store opens `probe` costs, with its response.
fn opens(state: &HubState, bearer: &str) -> (HttpResponse, u64) {
    let (resp, costs) = measure(|| session_probe(state, bearer));
    (resp, costs.store_opens)
}

#[test]
fn a_repeated_bogus_bearer_opens_the_store_once_and_the_answer_does_not_change() {
    let tmp = TempDir::new().unwrap();
    let state = hub(&tmp);
    let (first, first_opens) = opens(&state, "bogus-token-one");
    assert_eq!((first.status, first_opens), (401, 1));
    for _ in 0..20 {
        let (again, again_opens) = opens(&state, "bogus-token-one");
        assert_eq!(
            again_opens, 0,
            "a remembered rejection must not open the store"
        );
        // Same bytes on the wire: nothing tells a remembered rejection from a fresh one.
        assert_eq!(again.to_http_bytes(false), first.to_http_bytes(false));
    }
    assert_eq!(
        first.json_value().unwrap(),
        json!({ "error": "unauthorized" })
    );
    // A different bogus token is a new rejection, not a hit.
    let (other, other_opens) = opens(&state, "bogus-token-two");
    assert_eq!((other.status, other_opens), (401, 1));
}

#[test]
fn a_request_with_no_session_credential_never_opens_the_store() {
    let tmp = TempDir::new().unwrap();
    let state = hub(&tmp);
    let (resp, costs) =
        measure(|| handle_http_request(&state, &HttpRequest::new("GET", "/api/v1/auth/session")));
    assert_eq!(resp.status, 401);
    assert_eq!(costs.store_opens, 0);
}

#[test]
fn owner_and_peer_tokens_are_never_cached_as_rejections() {
    let tmp = TempDir::new().unwrap();
    let state = hub(&tmp);
    for token in [OWNER, PEER] {
        let (resp, n) = opens(&state, token);
        assert_eq!((resp.status, n), (200, 0));
    }
    assert_eq!(state.auth.session_miss_len(), 0);
}

#[test]
fn a_rejection_is_checked_again_after_the_ttl() {
    let tmp = TempDir::new().unwrap();
    let mut state = hub(&tmp);
    state.auth = state
        .auth
        .clone()
        .with_session_miss_limits(Duration::from_millis(80), 16);
    assert_eq!(opens(&state, "stale-token").1, 1);
    assert_eq!(opens(&state, "stale-token").1, 0);
    std::thread::sleep(Duration::from_millis(150));
    let (resp, n) = opens(&state, "stale-token");
    assert_eq!(
        (resp.status, n),
        (401, 1),
        "the entry expired: ask the store again"
    );
    assert_eq!(opens(&state, "stale-token").1, 0);
}

#[test]
fn the_cache_holds_at_most_its_capacity_and_evicts_the_oldest() {
    let tmp = TempDir::new().unwrap();
    let mut state = hub(&tmp);
    state.auth = state
        .auth
        .clone()
        .with_session_miss_limits(Duration::from_secs(60), 3);
    for i in 1..=5 {
        assert_eq!(opens(&state, &format!("bogus-{i}")).1, 1);
        assert!(state.auth.session_miss_len() <= 3);
    }
    assert_eq!(state.auth.session_miss_len(), 3);
    // 3, 4 and 5 are remembered; 1 and 2 were pushed out.
    for i in 3..=5 {
        assert_eq!(opens(&state, &format!("bogus-{i}")).1, 0, "bogus-{i}");
    }
    assert_eq!(opens(&state, "bogus-1").1, 1);
    assert_eq!(state.auth.session_miss_len(), 3);
}

#[test]
fn a_session_minted_after_a_cached_rejection_is_accepted_at_once() {
    let tmp = TempDir::new().unwrap();
    let state = hub(&tmp);
    let token = "late-session-token-0123456789abcdef";
    assert_eq!(opens(&state, token).0.status, 401);
    assert_eq!(state.auth.session_miss_len(), 1);

    let store = state.open_store().unwrap();
    state
        .auth
        .mint_session_for_token(Some(&store), "lmcfarlin", token)
        .unwrap();
    drop(store);
    // Minting dropped the rejection itself, apart from the in-memory session also shadowing it.
    assert_eq!(state.auth.session_miss_len(), 0);

    let (resp, n) = opens(&state, token);
    assert_eq!((resp.status, n), (200, 0));
    assert_eq!(resp.json_value().unwrap()["authority"]["tier"], "owner");
}

#[test]
fn a_session_minted_by_pairing_after_bogus_traffic_works_at_once() {
    let tmp = TempDir::new().unwrap();
    let state = hub(&tmp);
    for i in 0..10 {
        assert_eq!(opens(&state, &format!("noise-{i}")).0.status, 401);
    }
    let pair = handle_http_request(
        &state,
        &HttpRequest::new("POST", "/api/v1/auth/pair").with_json_body(&json!({ "token": OWNER })),
    );
    assert_eq!(pair.status, 200);
    let session = pair.json_value().unwrap()["session_token"]
        .as_str()
        .unwrap()
        .to_string();
    let (resp, n) = opens(&state, &session);
    assert_eq!((resp.status, n), (200, 0));
    // Also by cookie, the way a browser sends it.
    let by_cookie = handle_http_request(
        &state,
        &HttpRequest::new("GET", "/api/v1/auth/session")
            .with_header("Cookie", format!("fl_hub_session={session}")),
    );
    assert_eq!(by_cookie.status, 200);
}

#[test]
fn a_logged_out_session_is_rejected_and_stays_rejected() {
    let tmp = TempDir::new().unwrap();
    let state = hub(&tmp);
    let session = {
        let store = state.open_store().unwrap();
        state.auth.mint_session(Some(&store), "lmcfarlin").unwrap()
    };
    assert_eq!(opens(&state, &session).0.status, 200);
    let logout = handle_http_request(
        &state,
        &HttpRequest::new("POST", "/api/v1/auth/logout")
            .with_header("Authorization", format!("Bearer {session}")),
    );
    assert_eq!(logout.status, 200);
    assert_eq!(opens(&state, &session).0.status, 401);
    assert_eq!(opens(&state, &session).1, 0);
}

// ---- P1-5: the connection caps -----------------------------------------------------------

struct Running {
    addr: SocketAddr,
    shutdown: tokio::sync::watch::Sender<bool>,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
    _tmp: TempDir,
}

impl Running {
    async fn stop(self) {
        let _ = self.shutdown.send(true);
        let _ = self.task.await;
    }
}

async fn serve(limits: ConnectionLimits) -> Running {
    let tmp = TempDir::new().unwrap();
    let state = hub(&tmp);
    let server = HubServer::bind(state.config.clone())
        .await
        .unwrap()
        .with_limits(limits);
    let addr = server.local_addr();
    let shutdown = server.shutdown_handle();
    let task = tokio::spawn(server.serve());
    Running {
        addr,
        shutdown,
        task,
        _tmp: tmp,
    }
}

fn limits(connections: usize, ws_sessions: usize, peer_ws_sessions: usize) -> ConnectionLimits {
    ConnectionLimits {
        connections,
        ws_sessions,
        peer_ws_sessions,
    }
}

fn connect(addr: SocketAddr) -> TcpStream {
    let tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(3)).unwrap();
    tcp.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    tcp
}

fn read_all(tcp: &mut TcpStream) -> String {
    let mut out = Vec::new();
    let _ = tcp.read_to_end(&mut out);
    String::from_utf8_lossy(&out).into_owned()
}

fn health_on(tcp: &mut TcpStream) -> String {
    tcp.write_all(b"GET /api/v1/health HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .unwrap();
    read_all(tcp)
}

/// Keep asking until `attempt` returns `Some`, for up to five seconds.
fn eventually<T>(mut attempt: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(v) = attempt() {
            return v;
        }
        assert!(Instant::now() < deadline, "condition never held");
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connections_past_the_cap_get_503_and_the_rest_keep_working() {
    let srv = serve(limits(3, 8, 4)).await;
    // Three idle connections hold the three permits (the server waits for their requests).
    let mut held: Vec<TcpStream> = (0..3).map(|_| connect(srv.addr)).collect();

    // The fourth is refused at accept: no request needed, and the loop is not stuck behind the
    // idle ones.
    let started = Instant::now();
    let mut fourth = connect(srv.addr);
    let reply = read_all(&mut fourth);
    assert!(
        reply.starts_with("HTTP/1.1 503 Service Unavailable"),
        "{reply}"
    );
    assert!(reply.contains("Retry-After: 1"), "{reply}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the accept loop stalled"
    );

    // The first three still work.
    for tcp in &mut held {
        let reply = health_on(tcp);
        assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
    }
    srv.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_closed_connection_gives_its_permit_back() {
    let srv = serve(limits(2, 8, 4)).await;
    let first = connect(srv.addr);
    let _second = connect(srv.addr);
    assert!(read_all(&mut connect(srv.addr)).starts_with("HTTP/1.1 503"));

    drop(first);
    let reply = eventually(|| {
        let mut tcp = connect(srv.addr);
        let reply = health_on(&mut tcp);
        reply.starts_with("HTTP/1.1 200").then_some(reply)
    });
    assert!(reply.contains("\"status\""), "{reply}");
    srv.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slow_request_holds_one_permit_and_blocks_nobody_under_the_cap() {
    let srv = serve(limits(4, 8, 4)).await;
    let mut slow = connect(srv.addr);
    slow.write_all(b"POST /api/v1/auth/pair HTTP/1.1\r\nContent-Length: 1000000\r\n\r\n{")
        .unwrap();
    let started = Instant::now();
    for _ in 0..3 {
        let reply = health_on(&mut connect(srv.addr));
        assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
    }
    assert!(started.elapsed() < Duration::from_secs(5));
    drop(slow);
    srv.stop().await;
}

fn ws_url(addr: SocketAddr) -> String {
    format!("ws://{addr}/ws/v1")
}

const T: Duration = Duration::from_secs(3);

fn ws_as_owner(addr: SocketAddr) -> Result<HubWsClient, String> {
    HubWsClient::connect(
        &ws_url(addr),
        &[("Authorization", &format!("Bearer {OWNER}"))],
        T,
    )
    .map_err(|e| e.to_string())
}

fn ws_as_peer(addr: SocketAddr, agent: &str) -> Result<HubWsClient, String> {
    HubWsClient::connect(
        &ws_url(addr),
        &[
            ("Authorization", &format!("Bearer {PEER}")),
            ("X-Fl-Agent", agent),
        ],
        T,
    )
    .map_err(|e| e.to_string())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn websocket_sessions_past_the_cap_get_503_and_closing_frees_a_slot() {
    let srv = serve(limits(32, 2, 2)).await;
    let mut a = ws_as_owner(srv.addr).unwrap();
    let _b = ws_as_owner(srv.addr).unwrap();
    assert_eq!(a.recv_frame().unwrap().event, "connected");

    let refused = ws_as_owner(srv.addr).err().expect("third session refused");
    assert!(refused.contains("503"), "{refused}");
    // Refusing a session did not cost the others theirs, nor plain HTTP its way in.
    assert!(health_on(&mut connect(srv.addr)).starts_with("HTTP/1.1 200"));

    a.close().unwrap();
    drop(a);
    let _c = eventually(|| ws_as_owner(srv.addr).ok());
    srv.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn peers_cannot_take_the_owners_websocket_slots() {
    let srv = serve(limits(32, 4, 1)).await;
    let _p1 = ws_as_peer(srv.addr, "ferris").unwrap();
    // A different agent name is the same tier: still over the peer cap.
    let refused = ws_as_peer(srv.addr, "someone-else").err().expect("refused");
    assert!(refused.contains("503"), "{refused}");
    // The Owner has the rest.
    let _o1 = ws_as_owner(srv.addr).unwrap();
    let _o2 = ws_as_owner(srv.addr).unwrap();
    let _o3 = ws_as_owner(srv.addr).unwrap();
    srv.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_peer_upgrade_does_not_leak_a_total_slot() {
    // Total 2, peer 1: the second peer is refused on the peer cap and must give nothing back
    // or keep anything, so the Owner can still take the second total slot.
    let srv = serve(limits(32, 2, 1)).await;
    let _p1 = ws_as_peer(srv.addr, "ferris").unwrap();
    assert!(ws_as_peer(srv.addr, "ferris").is_err());
    let _o1 = ws_as_owner(srv.addr).unwrap();
    assert!(ws_as_owner(srv.addr).is_err());
    srv.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unauthenticated_upgrades_get_401_not_a_slot() {
    let srv = serve(limits(32, 1, 1)).await;
    let err = HubWsClient::connect(&ws_url(srv.addr), &[], T)
        .err()
        .unwrap()
        .to_string();
    assert!(err.contains("401"), "{err}");
    let _o = ws_as_owner(srv.addr).unwrap();
    srv.stop().await;
}
