//! Synthetic controllers and real host binaries. No vendor or running service.
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use fl_host_proto::{
    AsyncHostClient, BlockingHostClient, ChildRecord, ChildState, ControllerLease, Cursor,
    ExtensionFrame, ExtensionRefusal as Refusal, ExtensionRequest as Req,
    ExtensionResponse as Resp, HostEndpoint, HostRequest, HostResponse, ReliableKind, RequestId,
    SendOutcome, SessionId, SessionMode, SpawnKind, SpawnSpec, StreamId,
};

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}
fn record(state: &ChildState) -> &ChildRecord {
    match state {
        ChildState::Running(r) | ChildState::LeaderExited(r) | ChildState::CleanupComplete(r) => r,
    }
}
struct Rpc {
    client: BlockingHostClient,
    next: u64,
}
impl Rpc {
    fn connect(endpoint: &HostEndpoint) -> Self {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(client) = BlockingHostClient::connect(endpoint) {
                return Self { client, next: 1 };
            }
            assert!(Instant::now() < deadline, "host did not become ready");
            thread::sleep(Duration::from_millis(5));
        }
    }
    fn call(&mut self, request: Req) -> Resp {
        let id = RequestId(self.next);
        self.next += 1;
        match self
            .client
            .request(HostRequest::Extended { id, request })
            .unwrap()
        {
            HostResponse::Extended { response, .. } => response,
            other => panic!("wrong response {other:?}"),
        }
    }
    fn spawn(&mut self, name: &str, args: &[&str]) -> (StreamId, ChildRecord) {
        let response = self.call(Req::SpawnSession {
            spec: spec(name, args),
            mode: SessionMode::ReliablePipe,
            pty: None,
        });
        match response {
            Resp::Session { stream, state } => (stream, record(&state).clone()),
            other => panic!("spawn {other:?}"),
        }
    }
    fn claim(&mut self, stream: &StreamId) -> ControllerLease {
        match self.call(Req::ClaimController {
            stream: stream.clone(),
        }) {
            Resp::Controller { lease, .. } => lease,
            other => panic!("claim {other:?}"),
        }
    }
    fn summary(&mut self, stream: &StreamId) -> fl_host_proto::SessionSummary {
        match self.call(Req::ListSessions) {
            Resp::Sessions { records } => records
                .into_iter()
                .find(|r| r.stream == *stream)
                .expect("retained stream"),
            other => panic!("list {other:?}"),
        }
    }
    fn ticket(
        &mut self,
        stream: &StreamId,
        lease: ControllerLease,
        after: u64,
    ) -> (fl_host_proto::Ticket, u64) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match self.call(Req::SubscribeReliable {
                stream: stream.clone(),
                lease,
                after_seq: Some(after),
            }) {
                Resp::Subscription {
                    ticket, watermark, ..
                } => return (ticket, watermark),
                Resp::Refused {
                    code: Refusal::Busy,
                } if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
                other => panic!("subscription {other:?}"),
            }
        }
    }
}
// Keep the ticket on the same authenticated connection that owns the lease.
async fn reliable_ticket(
    client: &mut AsyncHostClient,
    stream: &StreamId,
    lease: ControllerLease,
    after: u64,
) -> fl_host_proto::Ticket {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match client
            .extension_request(Req::SubscribeReliable {
                stream: stream.clone(),
                lease,
                after_seq: Some(after),
            })
            .await
            .unwrap()
        {
            Resp::Subscription { ticket, .. } => return ticket,
            Resp::Refused {
                code: Refusal::Busy,
            } if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            other => panic!("subscription {other:?}"),
        }
    }
}
// Reassemble only complete events after a structured subscription has succeeded.
async fn next_complete_event(
    events: &mut fl_host_proto::ExtensionEventStream,
    reassembly: &mut fl_host_proto::ReliableReassembler,
) -> fl_host_proto::ReliableEvent {
    loop {
        let frame = events.next_frame().await.unwrap();
        if let Some(event) = reassembly.push(&frame).unwrap() {
            return event;
        }
    }
}
fn spec(name: &str, args: &[&str]) -> SpawnSpec {
    SpawnSpec {
        session: SessionId::new(name).unwrap(),
        kind: SpawnKind::Pipe,
        argv: std::iter::once(env!("CARGO_BIN_EXE_fl-host-testchild").to_owned())
            .chain(args.iter().map(|v| (*v).to_owned()))
            .collect(),
        cwd: std::env::temp_dir(),
        env: Default::default(),
        env_allow: Default::default(),
    }
}
struct Fixture {
    root: tempfile::TempDir,
    endpoint: HostEndpoint,
    hold: PathBuf,
    daemon: Child,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        let endpoint = {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
            HostEndpoint::unix(root.path().join("h.sock")).unwrap()
        };
        #[cfg(windows)]
        let endpoint = {
            use std::sync::atomic::{AtomicU64, Ordering};
            static NEXT: AtomicU64 = AtomicU64::new(0);
            HostEndpoint::windows(
                &format!(
                    "replay-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ),
                &fl_host::current_user_sid().unwrap(),
            )
            .unwrap()
        };
        let hold = root.path().join("hold");
        std::fs::create_dir(&hold).unwrap();
        let daemon = Self::launch(&endpoint, &hold);
        let f = Self {
            root,
            endpoint,
            hold,
            daemon,
        };
        let _ = f.rpc();
        f
    }
    fn launch(endpoint: &HostEndpoint, hold: &Path) -> Child {
        Command::new(env!("CARGO_BIN_EXE_fl-host"))
            .arg("--socket")
            .arg(endpoint.socket_path())
            .env("FL_HOST_SUBSCRIBER_HOLD", hold)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap()
    }
    fn rpc(&self) -> Rpc {
        Rpc::connect(&self.endpoint)
    }
    fn arm(&self, phase: &str) {
        std::fs::write(self.hold.join(format!("arm-{phase}")), b"1").unwrap();
    }
    fn release(&self, phase: &str) {
        let _ = std::fs::remove_file(self.hold.join(format!("arm-{phase}")));
        std::fs::write(self.hold.join(format!("go-{phase}")), b"1").unwrap();
    }
    fn at(&self, phase: &str) {
        wait_file(&self.hold.join(format!("at-{phase}")));
    }
}
fn stop(child: &mut Child) {
    if child.try_wait().unwrap().is_some() {
        return;
    }
    #[cfg(unix)]
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    #[cfg(windows)]
    let _ = child.kill();
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if child.try_wait().unwrap().is_some() {
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
    let _ = child.kill();
    let _ = child.wait();
}
impl Drop for Fixture {
    fn drop(&mut self) {
        stop(&mut self.daemon);
    }
}
struct Worker(Child);
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn wait_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() {
        assert!(Instant::now() < deadline, "missing {}", path.display());
        thread::sleep(Duration::from_millis(5));
    }
}
fn worker(f: &Fixture, stream: &StreamId, point: &str) -> Worker {
    let config = f.root.path().join("controller.json");
    let configuration = serde_json::json!({
        "endpoint": f.endpoint.socket_path(),
        "stream": stream,
        "state": f.root.path().join("durable.json"),
        "marker": f.root.path().join("controller-ready"),
        "point": point,
        "through": 2
    });
    std::fs::write(&config, serde_json::to_vec(&configuration).unwrap()).unwrap();
    Worker(
        Command::new(env!("CARGO_BIN_EXE_fl-host-testchild"))
            .arg("reliable-controller")
            .arg(config)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    )
}
fn durable_crash(point: &str) {
    let f = Fixture::new();
    let mut rpc = f.rpc();
    let (stream, original) = rpc.spawn("durable", &["echo"]);
    if point == "after-ack" {
        f.arm("reliable-ack-reply");
    }
    let mut crashed = worker(&f, &stream, point);
    if point == "after-ack" {
        f.at("reliable-ack-reply");
        assert_eq!(rpc.summary(&stream).acked, 1);
    } else {
        wait_file(&f.root.path().join("controller-ready"));
    }
    crashed.0.kill().unwrap();
    crashed.0.wait().unwrap();
    drop(crashed);
    if point == "after-ack" {
        f.release("reliable-ack-reply");
    }
    let mut recovered = worker(&f, &stream, "finish");
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = recovered.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "controller restart did not finish"
        );
        thread::sleep(Duration::from_millis(5));
    };
    assert!(status.success());
    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(f.root.path().join("durable.json")).unwrap())
            .unwrap();
    assert_eq!(state["stream"], serde_json::to_value(&stream).unwrap());
    assert_eq!(state["cursor"], 2);
    assert_eq!(state["status"], "active");
    let rows = state["applied"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["seq"], 1);
    assert_eq!(rows[1]["seq"], 2);
    let summary = rpc.summary(&stream);
    assert_eq!(record(&summary.state).pid, original.pid);
    assert_eq!(record(&summary.state).session_epoch, original.session_epoch);
    assert_eq!(summary.acked, 2);
}
#[test]
fn controller_crash_before_commit_replays_without_duplicate_application() {
    durable_crash("before-commit");
}
#[test]
fn controller_crash_after_commit_before_ack_replays_without_duplicate_application() {
    durable_crash("after-commit");
}
#[test]
fn controller_crash_after_host_ack_before_reply_retries_without_duplicate_application() {
    durable_crash("after-ack");
}

#[test]
fn fragmented_disconnect_and_watermark_recovery_apply_complete_events_once() {
    let f = Fixture::new();
    let mut rpc = f.rpc();
    let first = f.root.path().join("first");
    let second = f.root.path().join("second");
    let (stream, original) = rpc.spawn(
        "fragments",
        &[
            "reliable-gated-maxline",
            first.to_str().unwrap(),
            second.to_str().unwrap(),
        ],
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while rpc.summary(&stream).latest < 2 {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    let lease = rpc.claim(&stream);
    let (ticket, watermark) = rpc.ticket(&stream, lease, 0);
    assert_eq!(watermark, 2, "capture before gated publication");
    let rt = runtime();
    let mut events = rt.block_on(async {
        AsyncHostClient::connect(&f.endpoint)
            .await
            .unwrap()
            .open_extension_stream(ticket)
            .await
            .unwrap()
    });
    let mut partial = fl_host_proto::ReliableReassembler::for_stream(stream.clone(), 0);
    for expected in 1..=2 {
        let frame = rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(5), events.next_frame())
                .await
                .unwrap()
                .unwrap()
        });
        assert_eq!(partial.push(&frame).unwrap().unwrap().seq, expected);
    }
    f.arm("reliable-after-part");
    std::fs::write(&first, b"go").unwrap();
    let frame = rt.block_on(async {
        tokio::time::timeout(Duration::from_secs(5), events.next_frame())
            .await
            .unwrap()
            .unwrap()
    });
    assert!(
        matches!(&frame,ExtensionFrame::ReliablePart{seq:3,kind:ReliableKind::Line,more:true,bytes} if bytes.as_slice().len()==32*1024)
    );
    assert!(partial.push(&frame).unwrap().is_none());
    f.at("reliable-after-part");
    assert_eq!(
        rpc.call(Req::Ack {
            cursor: Cursor {
                stream: stream.clone(),
                seq: 3
            },
            lease
        }),
        Resp::Refused {
            code: Refusal::FutureCursor
        },
        "an earlier fragment has no final-write eligibility"
    );
    assert_eq!(rpc.summary(&stream).acked, 0);
    f.arm("reliable-settle");
    drop(events);
    drop(rpc); // real authenticated event socket closes inside event3
    f.at("reliable-settle");
    f.release("reliable-after-part");
    let mut recovered = rt.block_on(AsyncHostClient::connect(&f.endpoint)).unwrap();
    let grant = rt
        .block_on(recovered.claim_controller(stream.clone()))
        .unwrap();
    assert_eq!(grant.acked, 0);
    assert_eq!(
        rt.block_on(recovered.extension_request(Req::SubscribeReliable {
            stream: stream.clone(),
            lease: grant.lease,
            after_seq: Some(0),
        }))
        .unwrap(),
        Resp::Refused {
            code: Refusal::Busy
        },
        "takeover must not retire an old stream still held at settlement"
    );
    f.release("reliable-settle");
    let ticket = rt.block_on(reliable_ticket(&mut recovered, &stream, grant.lease, 0));
    let mut replay = rt
        .block_on(async {
            AsyncHostClient::connect(&f.endpoint)
                .await
                .unwrap()
                .open_extension_stream(ticket)
                .await
        })
        .unwrap();
    let mut reassembly = fl_host_proto::ReliableReassembler::for_stream(stream.clone(), 0);
    let mut seen = Vec::new();
    for expected in 1..=4 {
        if expected == 4 {
            std::fs::write(&second, b"go").unwrap();
        }
        let event = rt.block_on(async {
            tokio::time::timeout(
                Duration::from_secs(5),
                next_complete_event(&mut replay, &mut reassembly),
            )
            .await
            .unwrap()
        });
        assert_eq!(event.seq, expected);
        seen.push(event.seq);
        if expected == 3 {
            assert_eq!(event.bytes.len(), 1 << 20);
            assert!(event.bytes.iter().all(|b| *b == b'y'));
        }
        if expected == 4 {
            assert_eq!(event.bytes, b"TAIL");
        }
        rt.block_on(recovered.ack(
            Cursor {
                stream: stream.clone(),
                seq: event.seq,
            },
            grant.lease,
        ))
        .unwrap();
    }
    assert_eq!(seen, [1, 2, 3, 4]);
    let mut inspector = f.rpc();
    let current = inspector.summary(&stream);
    assert_eq!(record(&current.state).pid, original.pid);
    assert_eq!(current.stream, stream);
    assert_eq!(current.acked, 4);
}

#[test]
fn saturated_old_epoch_replays_every_line_after_kill_and_same_name_replacement() {
    let f = Fixture::new();
    let mut rpc = f.rpc();
    let flushed = f.root.path().join("flushed");
    let (stream, _) = rpc.spawn("saturated", &["lines", "4500", flushed.to_str().unwrap()]);
    let lease = rpc.claim(&stream);
    f.arm("reliable-before-part");
    let (ticket, _) = rpc.ticket(&stream, lease, 0);
    let rt = runtime();
    let held = rt.block_on(async {
        AsyncHostClient::connect(&f.endpoint)
            .await
            .unwrap()
            .open_extension_stream(ticket)
            .await
            .unwrap()
    });
    f.at("reliable-before-part");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !record(&rpc.summary(&stream).state).consumer_stalled {
        assert!(Instant::now() < deadline, "real data-count saturation");
        thread::sleep(Duration::from_millis(5));
    }
    wait_file(&flushed); // all4500 numbered lines were actually flushed, no guessed backlog
    assert_eq!(rpc.summary(&stream).acked, 0);
    assert_eq!(
        rpc.call(Req::KillSession {
            stream: stream.clone(),
            lease
        }),
        Resp::Controlled
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while !matches!(rpc.summary(&stream).state, ChildState::CleanupComplete(_)) {
        assert!(Instant::now() < deadline, "cleanup at saturated stdout");
        thread::sleep(Duration::from_millis(5));
    }
    let (new, new_record) = rpc.spawn("saturated", &["echo"]);
    assert!(new.epoch.0 > stream.epoch.0);
    assert!(matches!(
        rpc.call(Req::SendLine {
            stream: stream.clone(),
            lease,
            line: "OLD-MUST-NOT-WRITE".into()
        }),
        Resp::Refused {
            code: Refusal::SessionFenced
        }
    ));
    f.release("reliable-before-part");
    drop(held);
    let mut recovered = rt.block_on(AsyncHostClient::connect(&f.endpoint)).unwrap();
    let grant = rt
        .block_on(recovered.claim_controller(stream.clone()))
        .unwrap();
    let ticket = rt.block_on(reliable_ticket(&mut recovered, &stream, grant.lease, 0));
    let mut events = rt
        .block_on(async {
            AsyncHostClient::connect(&f.endpoint)
                .await
                .unwrap()
                .open_extension_stream(ticket)
                .await
        })
        .unwrap();
    let mut reassembly = fl_host_proto::ReliableReassembler::for_stream(stream.clone(), 0);
    let mut next_line = 0;
    let mut saw_ready = false;
    let mut exits = 0;
    let mut cleanups = 0;
    let mut exit_seq = 0;
    let mut cleanup_seq = 0;
    for seq in 1..=4504 {
        let event = rt.block_on(async {
            tokio::time::timeout(
                Duration::from_secs(5),
                next_complete_event(&mut events, &mut reassembly),
            )
            .await
            .unwrap()
        });
        assert_eq!(event.seq, seq);
        match event.kind {
            ReliableKind::Line if event.bytes == b"READY" => {
                assert!(!saw_ready);
                saw_ready = true;
            }
            ReliableKind::Line => {
                assert_eq!(event.bytes, format!("L{next_line}").as_bytes());
                next_line += 1;
            }
            ReliableKind::Lifecycle => {
                match serde_json::from_slice::<fl_host_proto::ControlEvent>(&event.bytes).unwrap() {
                    fl_host_proto::ControlEvent::Exited { .. } => {
                        exits += 1;
                        exit_seq = seq;
                    }
                    fl_host_proto::ControlEvent::CleanupComplete { .. } => {
                        cleanups += 1;
                        cleanup_seq = seq;
                    }
                    fl_host_proto::ControlEvent::Spawned { .. } => assert_eq!(seq, 1),
                    other => panic!("unexpected lifecycle {other:?}"),
                }
            }
        }
        rt.block_on(recovered.ack(
            Cursor {
                stream: stream.clone(),
                seq: event.seq,
            },
            grant.lease,
        ))
        .unwrap();
    }
    assert!(saw_ready);
    assert_eq!(next_line, 4500);
    assert_eq!((exits, cleanups), (1, 1));
    assert!(exit_seq < cleanup_seq);
    let current = rpc.summary(&new);
    assert_eq!(record(&current.state).pid, new_record.pid);
    assert!(matches!(current.state, ChildState::Running(_)));
    let current_lease = rpc.claim(&new);
    assert_eq!(
        rpc.call(Req::SendLine {
            stream: new.clone(),
            lease: current_lease,
            line: "NEW".into()
        }),
        Resp::Input {
            outcome: SendOutcome::Written
        }
    );
}

#[test]
fn retained_reliable_capacity_refuses_before_spawn_side_effects() {
    let f = Fixture::new();
    let mut rpc = f.rpc();
    for index in 0..64 {
        let (stream, _) = rpc.spawn(&format!("retained-{index}"), &["stall"]);
        let lease = rpc.claim(&stream);
        assert_eq!(
            rpc.call(Req::KillSession {
                stream: stream.clone(),
                lease
            }),
            Resp::Controlled
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while !matches!(rpc.summary(&stream).state, ChildState::CleanupComplete(_)) {
            assert!(Instant::now() < deadline, "retained cleanup {index}");
            thread::sleep(Duration::from_millis(5));
        }
    }
    let marker = f.root.path().join("denied-child-marker");
    assert_eq!(
        rpc.call(Req::SpawnSession {
            spec: spec("denied", &["marker", marker.to_str().unwrap()]),
            mode: SessionMode::ReliablePipe,
            pty: None
        }),
        Resp::Refused {
            code: Refusal::ResourceLimit
        }
    );
    assert!(
        !marker.exists(),
        "admission refusal must precede child code"
    );
    match rpc.call(Req::ListSessions) {
        Resp::Sessions { records } => {
            assert_eq!(records.len(), 64);
            assert!(records.iter().all(|r| r.acked == 0));
        }
        other => panic!("retention {other:?}"),
    }
}

#[test]
fn host_crash_rejects_old_instance_and_requires_explicit_uncertain_recovery() {
    let mut f = Fixture::new();
    let mut old = f.rpc();
    let (stream, _original) = old.spawn("crashed", &["echo"]);
    let lease = old.claim(&stream);
    let cursor = Cursor {
        stream: stream.clone(),
        seq: 0,
    };
    let mut previous = worker(&f, &stream, "after-commit");
    wait_file(&f.root.path().join("controller-ready"));
    previous.0.kill().unwrap();
    previous.0.wait().unwrap();
    drop(previous);
    // Abrupt daemon death is distinct from a controller-only restart. The echo
    // fixture owns no detached descendants and exits on stdin EOF. This test
    // makes no claim that an arbitrary former process is dead or adopted.
    f.daemon.kill().unwrap();
    f.daemon.wait().unwrap();
    drop(old);
    f.daemon = Fixture::launch(&f.endpoint, &f.hold);
    let mut new = f.rpc();
    let instance = match new.call(Req::Capabilities) {
        Resp::Capabilities { host, .. } => host,
        other => panic!("capabilities {other:?}"),
    };
    assert_ne!(instance, stream.host);
    assert_eq!(
        new.call(Req::ClaimController {
            stream: stream.clone()
        }),
        Resp::Refused {
            code: Refusal::WrongInstance
        }
    );
    assert_eq!(
        new.call(Req::Ack { cursor, lease }),
        Resp::Refused {
            code: Refusal::WrongInstance
        }
    );
    assert_eq!(
        new.call(Req::SendLine {
            stream: stream.clone(),
            lease,
            line: "DO-NOT-REPLAY".into()
        }),
        Resp::Refused {
            code: Refusal::WrongInstance
        }
    );
    let rows = match new.call(Req::ListSessions) {
        Resp::Sessions { records } => records,
        other => panic!("sessions {other:?}"),
    };
    assert!(
        rows.is_empty(),
        "saved process labels must not manufacture adopted sessions"
    );
    // The application must durably expose uncertainty rather than treating a
    // refused old cursor as empty history, a death observation, or respawn GO.
    let mut restarted = worker(&f, &stream, "finish");
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = restarted.0.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "uncertain recovery deadline");
        thread::sleep(Duration::from_millis(5));
    };
    assert!(status.success());
    let observed: serde_json::Value =
        serde_json::from_slice(&std::fs::read(f.root.path().join("durable.json")).unwrap())
            .unwrap();
    assert_eq!(observed["status"], "interrupted_uncertain");
    assert!(observed["replacement"].is_null());
    assert_eq!(observed["stream"], serde_json::to_value(stream).unwrap());
    assert_eq!(observed["cursor"], 1);
    assert_eq!(observed["applied"].as_array().unwrap().len(), 1);
}

// Catches rejecting one legal Ack after full emission but before ceiling publication.
#[test]
fn complete_event_ack_waits_for_publication_without_client_retry() {
    let f = Fixture::new();
    let mut rpc = f.rpc();
    let (stream, _) = rpc.spawn("ack-publication", &["echo"]);
    let lease = rpc.claim(&stream);
    f.arm("reliable-before-complete");
    let (ticket, _) = rpc.ticket(&stream, lease, 0);
    let rt = runtime();
    let mut events = rt.block_on(async {
        AsyncHostClient::connect(&f.endpoint)
            .await
            .unwrap()
            .open_extension_stream(ticket)
            .await
            .unwrap()
    });
    let mut reassembly = fl_host_proto::ReliableReassembler::for_stream(stream.clone(), 0);
    let event = rt.block_on(next_complete_event(&mut events, &mut reassembly));
    assert_eq!(event.seq, 1);
    f.at("reliable-before-complete");
    let summary_stream = stream.clone();
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let ack = thread::spawn(move || {
        let result = rpc.call(Req::Ack {
            cursor: Cursor {
                stream,
                seq: event.seq,
            },
            lease,
        });
        tx.send(result).unwrap();
        rpc
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    // The server marker is emitted only after the actual Ack wait was polled.
    // An early response is an error even if it reports FutureCursor.
    while !f.hold.join("at-reliable-ack-wait").exists() {
        if let Ok(response) = rx.try_recv() {
            panic!("single complete-event Ack returned before publication: {response:?}");
        }
        assert!(
            Instant::now() < deadline,
            "Ack did not enter bounded publication wait"
        );
        thread::sleep(Duration::from_millis(1));
    }
    assert!(matches!(
        rx.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    ));
    assert_eq!(f.rpc().summary(&summary_stream).acked, 0);
    f.release("reliable-before-complete");
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(2)).unwrap(),
        Resp::Acked { through: 1 }
    );
    let _rpc = ack.join().unwrap();
}

// Catches final-write cancellation leaving an Ack asleep or crediting a successor lease.
#[test]
fn pending_complete_event_ack_is_fenced_by_takeover_or_event_disconnect() {
    for cause in ["takeover", "event-disconnect"] {
        let f = Fixture::new();
        let mut rpc = f.rpc();
        let (stream, _) = rpc.spawn("ack-cancel", &["echo"]);
        let lease = rpc.claim(&stream);
        f.arm("reliable-before-complete");
        let (ticket, _) = rpc.ticket(&stream, lease, 0);
        let rt = runtime();
        let mut events = rt.block_on(async {
            AsyncHostClient::connect(&f.endpoint)
                .await
                .unwrap()
                .open_extension_stream(ticket)
                .await
                .unwrap()
        });
        let mut reassembly = fl_host_proto::ReliableReassembler::for_stream(stream.clone(), 0);
        assert_eq!(
            rt.block_on(next_complete_event(&mut events, &mut reassembly))
                .seq,
            1
        );
        f.at("reliable-before-complete");
        let ack_stream = stream.clone();
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let ack = thread::spawn(move || {
            let result = rpc.call(Req::Ack {
                cursor: Cursor {
                    stream: ack_stream,
                    seq: 1,
                },
                lease,
            });
            tx.send(result).unwrap();
            rpc
        });
        f.at("reliable-ack-wait");
        assert!(matches!(
            rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        let mut other = f.rpc();
        let next_lease = if cause == "takeover" {
            Some(other.claim(&stream))
        } else {
            None
        };
        drop(events);
        let expected = if cause == "takeover" {
            Refusal::StaleLease
        } else {
            Refusal::FutureCursor
        };
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            Resp::Refused { code: expected }
        );
        let _rpc = ack.join().unwrap();
        assert_eq!(other.summary(&stream).acked, 0);
        if let Some(lease) = next_lease {
            assert_eq!(
                other.call(Req::Ack {
                    cursor: Cursor {
                        stream: stream.clone(),
                        seq: 1
                    },
                    lease
                }),
                Resp::Refused {
                    code: Refusal::FutureCursor
                }
            );
        }
    }
}

// Catches guessing the exact pending seq granting credit before even the final write begins.
#[test]
fn guessed_pending_final_write_cancellation_retires_no_output() {
    let f = Fixture::new();
    let mut rpc = f.rpc();
    let (stream, _) = rpc.spawn("ack-no-precredit", &["echo"]);
    let lease = rpc.claim(&stream);
    f.arm("reliable-final-write");
    let (ticket, _) = rpc.ticket(&stream, lease, 0);
    let rt = runtime();
    let events = rt.block_on(async {
        AsyncHostClient::connect(&f.endpoint)
            .await
            .unwrap()
            .open_extension_stream(ticket)
            .await
            .unwrap()
    });
    f.at("reliable-final-write"); // exact token exists, no event bytes have been written
    let check_stream = stream.clone();
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let ack = thread::spawn(move || {
        let result = rpc.call(Req::Ack {
            cursor: Cursor { stream, seq: 1 },
            lease,
        });
        tx.send(result).unwrap();
        rpc
    });
    f.at("reliable-ack-wait");
    assert!(matches!(
        rx.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    ));
    assert_eq!(f.rpc().summary(&check_stream).acked, 0);
    drop(events);
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(2)).unwrap(),
        Resp::Refused {
            code: Refusal::FutureCursor
        }
    );
    let _rpc = ack.join().unwrap();
    assert_eq!(f.rpc().summary(&check_stream).acked, 0);
}
