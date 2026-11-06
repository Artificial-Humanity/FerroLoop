//! Daemon inventory, pipe bounds, and epoch/lease fencing. Real binaries, isolated endpoints.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use fl_host_proto::{
    BlockingHostClient, ChildExit, ChildRecord, ChildState, ControlEvent, ControllerLease,
    HostEndpoint, HostError, HostEvent, HostRequest, HostResponse, HostedChild, InputState,
    RequestId, SendBlock, SendOutcome, SessionEpoch, SessionId, SignalKind, SpawnKind, SpawnSpec,
};

fn child_bin() -> String {
    env!("CARGO_BIN_EXE_fl-host-testchild").to_owned()
}

fn host_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_fl-host"))
}

struct Paths {
    /// Owns the endpoint directory. Taken out when a second daemon reuses it.
    root: Option<tempfile::TempDir>,
    endpoint: HostEndpoint,
    barrier: PathBuf,
    subscriber: PathBuf,
    status: PathBuf,
    transcript: PathBuf,
}

fn paths() -> Paths {
    let root = tempfile::tempdir().expect("tempdir");
    #[cfg(unix)]
    let endpoint = {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700))
            .expect("mode");
        HostEndpoint::unix(root.path().join("h.sock")).expect("endpoint")
    };
    #[cfg(windows)]
    let endpoint = {
        use std::sync::atomic::AtomicU64;
        static SUFFIX: AtomicU64 = AtomicU64::new(0);
        let n = SUFFIX.fetch_add(1, Ordering::Relaxed);
        let suffix = format!("ipc-{}-{n}", std::process::id());
        HostEndpoint::windows(&suffix, &fl_host::current_user_sid().expect("sid"))
            .expect("endpoint")
    };
    #[cfg(not(any(unix, windows)))]
    let endpoint = HostEndpoint::unix(root.path().join("h.sock")).expect("endpoint");
    let barrier = root.path().join("barrier");
    let subscriber = root.path().join("subscriber");
    let status = root.path().join("backlog");
    std::fs::create_dir_all(&barrier).expect("barrier");
    std::fs::create_dir_all(&subscriber).expect("subscriber");
    Paths {
        transcript: root.path().join("stdin.bin"),
        endpoint,
        barrier,
        subscriber,
        status,
        root: Some(root),
    }
}

struct Daemon {
    child: Child,
    paths: Paths,
    stderr: Arc<Mutex<String>>,
    stderr_thread: Option<JoinHandle<()>>,
}

struct DaemonOpts {
    chunk: Option<usize>,
}

impl Daemon {
    fn start(opts: DaemonOpts) -> Self {
        let paths = paths();
        let mut cmd = Command::new(host_bin());
        cmd.arg("--socket")
            .arg(paths.endpoint.socket_path())
            .env("FL_HOST_WRITER_BARRIER", &paths.barrier)
            .env("FL_HOST_SUBSCRIBER_HOLD", &paths.subscriber)
            .env("FL_HOST_BACKLOG_STATUS", &paths.status)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        if let Some(chunk) = opts.chunk {
            cmd.env("FL_HOST_WRITE_CHUNK", chunk.to_string());
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        let mut child = cmd.spawn().expect("daemon");
        let stderr = Arc::new(Mutex::new(String::new()));
        let stderr_thread = child.stderr.take().map(|pipe| {
            let slot = Arc::clone(&stderr);
            thread::spawn(move || {
                let mut text = String::new();
                let _ =
                    std::io::Read::read_to_string(&mut std::io::BufReader::new(pipe), &mut text);
                *slot.lock().expect("stderr") = text;
            })
        });
        Self {
            child,
            paths,
            stderr,
            stderr_thread,
        }
    }

    fn stop(&mut self) {
        let _ = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if self.child.try_wait().ok().flatten().is_some() {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            self.stop();
        }
        if let Some(thread) = self.stderr_thread.take() {
            let _ = thread.join();
        }
        let err = self.stderr.lock().expect("stderr").clone();
        if !err.is_empty() {
            eprintln!("daemon stderr: {err}");
        }
    }
}

struct Rpc {
    inner: BlockingHostClient,
    next: u64,
}

impl Rpc {
    fn connect(endpoint: &HostEndpoint) -> Self {
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut last = String::new();
        while Instant::now() < deadline {
            match BlockingHostClient::connect(endpoint) {
                Ok(inner) => {
                    return Self { inner, next: 1 };
                }
                Err(err) => last = err.to_string(),
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!("daemon did not accept connections: {last}");
    }

    fn call(
        &mut self,
        build: impl FnOnce(RequestId) -> HostRequest,
    ) -> Result<HostResponse, HostError> {
        let id = RequestId(self.next);
        self.next = self.next.checked_add(1).expect("request id");
        self.inner.request(build(id))
    }

    /// One frame on this connection, without waiting for its response.
    fn push(&mut self, build: impl FnOnce(RequestId) -> HostRequest) -> RequestId {
        let id = RequestId(self.next);
        self.next = self.next.checked_add(1).expect("request id");
        self.inner.write_request(&build(id)).expect("write request");
        id
    }

    fn pull(&mut self) -> HostResponse {
        self.inner.read_response().expect("read response")
    }

    fn spawn(&mut self, session: &str, args: &[&str]) -> HostedChild {
        let spec = spec(session, args);
        match self.call(|id| HostRequest::Spawn { id, spec }) {
            Ok(HostResponse::Spawned { child, .. }) => child,
            other => panic!("spawn {session}: {other:?}"),
        }
    }

    fn list(&mut self) -> Vec<HostedChild> {
        match self.call(|id| HostRequest::List { id }) {
            Ok(HostResponse::Listed { children, .. }) => children,
            other => panic!("list: {other:?}"),
        }
    }

    fn claim(&mut self, session: &str, epoch: u64) -> Result<ControllerLease, HostResponse> {
        match self.call(|id| HostRequest::ClaimLease {
            id,
            session: sid(session),
            expected_epoch: SessionEpoch(epoch),
        }) {
            Ok(HostResponse::Lease { lease, .. }) => Ok(lease),
            Ok(other) => Err(other),
            Err(err) => panic!("claim transport: {err}"),
        }
    }

    fn send(
        &mut self,
        session: &str,
        epoch: u64,
        lease: ControllerLease,
        line: &str,
    ) -> SendOutcome {
        let line = line.to_owned();
        match self.call(|id| HostRequest::Send {
            id,
            session: sid(session),
            expected_epoch: SessionEpoch(epoch),
            lease,
            line,
        }) {
            Ok(HostResponse::Sent { outcome, .. }) => outcome,
            other => panic!("send: {other:?}"),
        }
    }

    fn kill(
        &mut self,
        session: &str,
        epoch: u64,
        lease: Option<ControllerLease>,
    ) -> Result<(), HostResponse> {
        match self.call(|id| HostRequest::Kill {
            id,
            session: sid(session),
            expected_epoch: SessionEpoch(epoch),
            lease,
        }) {
            Ok(HostResponse::Killed { .. }) => Ok(()),
            Ok(other) => Err(other),
            Err(err) => panic!("kill transport: {err}"),
        }
    }

    fn signal(
        &mut self,
        session: &str,
        epoch: u64,
        lease: ControllerLease,
        signal: SignalKind,
    ) -> Result<(), HostResponse> {
        match self.call(|id| HostRequest::Signal {
            id,
            session: sid(session),
            expected_epoch: SessionEpoch(epoch),
            lease,
            signal,
        }) {
            Ok(HostResponse::Signaled { .. }) => Ok(()),
            Ok(other) => Err(other),
            Err(err) => panic!("signal transport: {err}"),
        }
    }
}

fn sid(name: &str) -> SessionId {
    SessionId::new(name).expect("session")
}

fn spec(session: &str, args: &[&str]) -> SpawnSpec {
    SpawnSpec {
        session: sid(session),
        kind: SpawnKind::Pipe,
        argv: std::iter::once(child_bin())
            .chain(args.iter().copied().map(str::to_owned))
            .collect(),
        cwd: std::env::temp_dir(),
        env: std::collections::BTreeMap::new(),
        env_allow: std::collections::BTreeSet::new(),
    }
}

fn record(state: &ChildState) -> &ChildRecord {
    match state {
        ChildState::Running(record)
        | ChildState::LeaderExited(record)
        | ChildState::CleanupComplete(record) => record,
    }
}

fn epoch_of(child: &HostedChild) -> u64 {
    record(&child.state).session_epoch.0
}

fn find<'a>(children: &'a [HostedChild], name: &str) -> &'a HostedChild {
    children
        .iter()
        .find(|child| record(&child.state).session.as_str() == name)
        .unwrap_or_else(|| panic!("missing {name}"))
}

fn arm(dir: &Path, name: &str) {
    std::fs::write(dir.join(format!("arm-{name}")), b"1").expect("arm");
}

fn release(dir: &Path, name: &str) {
    let _ = std::fs::remove_file(dir.join(format!("arm-{name}")));
    std::fs::write(dir.join(format!("go-{name}")), b"1").expect("go");
}

fn wait_at(dir: &Path, name: &str) {
    let at = dir.join(format!("at-{name}"));
    let deadline = Instant::now() + Duration::from_secs(3);
    while !at.exists() {
        if Instant::now() >= deadline {
            panic!("barrier {name} was not reached");
        }
        thread::sleep(Duration::from_millis(5));
    }
}

fn wait_depth(dir: &Path, min: usize) {
    let path = dir.join("depth");
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(&path)
            && text.trim().parse::<usize>().unwrap_or(0) >= min
        {
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
    panic!("queue depth never reached {min}");
}

fn wait_file(path: &Path, pred: impl Fn(&[u8]) -> bool) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let bytes = std::fs::read(path).unwrap_or_default();
        if pred(&bytes) {
            return bytes;
        }
        if Instant::now() >= deadline {
            panic!("transcript {:?}", String::from_utf8_lossy(&bytes));
        }
        thread::sleep(Duration::from_millis(5));
    }
}

struct EventLog {
    events: Arc<Mutex<Vec<HostEvent>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl EventLog {
    fn subscribe(endpoint: &HostEndpoint, filter: Option<&str>) -> Self {
        let events = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let endpoint = endpoint.clone();
        let filter = filter.map(sid);
        let sink = Arc::clone(&events);
        let flag = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            let mut stream = None;
            let deadline = Instant::now() + Duration::from_secs(3);
            while stream.is_none() && Instant::now() < deadline {
                stream = runtime
                    .block_on(fl_host_proto::events(&endpoint, filter.clone()))
                    .ok();
                if stream.is_none() {
                    thread::sleep(Duration::from_millis(20));
                }
            }
            let Some(mut stream) = stream else {
                return;
            };
            while !flag.load(Ordering::SeqCst) {
                match runtime.block_on(async {
                    tokio::time::timeout(Duration::from_millis(100), stream.next()).await
                }) {
                    Ok(Ok(event)) => sink.lock().expect("events").push(event),
                    Ok(Err(_)) => break,
                    Err(_) => {}
                }
            }
        });
        Self {
            events,
            stop,
            thread: Some(thread),
        }
    }

    fn snapshot(&self) -> Vec<HostEvent> {
        self.events.lock().expect("events").clone()
    }

    fn wait_until(&self, timeout: Duration, pred: impl Fn(&[HostEvent]) -> bool) -> Vec<HostEvent> {
        let deadline = Instant::now() + timeout;
        loop {
            let events = self.snapshot();
            if pred(&events) {
                return events;
            }
            if Instant::now() >= deadline {
                panic!("events {events:?}");
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for EventLog {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn line_bytes(events: &[HostEvent], session: &str) -> Vec<Vec<u8>> {
    events
        .iter()
        .filter_map(|event| match event {
            HostEvent::Line {
                session: id, bytes, ..
            } if id.as_str() == session => Some(bytes.clone()),
            _ => None,
        })
        .collect()
}

fn controls(events: &[HostEvent]) -> Vec<ControlEvent> {
    events
        .iter()
        .filter_map(|event| match event {
            HostEvent::Control(control) => Some(control.clone()),
            _ => None,
        })
        .collect()
}

fn failed_reason(response: HostResponse) -> String {
    match response {
        HostResponse::Failed { reason, .. } => reason,
        other => panic!("expected failure, got {other:?}"),
    }
}

fn not_written(outcome: &SendOutcome) -> &SendBlock {
    match outcome {
        SendOutcome::NotWritten { reason } => reason,
        other => panic!("expected NotWritten, got {other:?}"),
    }
}

#[test]
fn serves_spawn_list_lease_send_echo_events_kill_and_exit() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let endpoint = daemon.paths.endpoint.clone();
    let log = EventLog::subscribe(&endpoint, None);
    let mut rpc = Rpc::connect(&endpoint);
    let echo = rpc.spawn("echo", &["echo"]);
    let echo_epoch = epoch_of(&echo);
    assert!(matches!(echo.state, ChildState::Running(_)));
    let listed = rpc.list();
    let row = record(&find(&listed, "echo").state);
    assert_eq!(row.session_epoch.0, echo_epoch);
    assert_ne!(row.pid, 0);
    assert_ne!(row.started_at_ms, 0);
    assert_eq!(row.input, InputState::Writable);
    assert!(!row.consumer_stalled);
    assert!(row.exit.is_none());
    let lease = rpc.claim("echo", echo_epoch).expect("lease");
    assert_ne!(lease.0, 0);
    log.wait_until(Duration::from_secs(3), |events| {
        line_bytes(events, "echo")
            .iter()
            .any(|line| line == b"READY")
    });
    assert_eq!(
        rpc.send("echo", echo_epoch, lease, "ping"),
        SendOutcome::Written
    );
    log.wait_until(Duration::from_secs(3), |events| {
        line_bytes(events, "echo")
            .iter()
            .any(|line| line == b"ping")
    });
    let exited = rpc.spawn("exit", &["exit", "7"]);
    let exit_epoch = epoch_of(&exited);
    log.wait_until(Duration::from_secs(3), |events| {
        controls(events).iter().any(|event| {
            matches!(
                event,
                ControlEvent::Exited {
                    session,
                    session_epoch,
                    exit: ChildExit { code: Some(7), signal: None },
                    ..
                } if session.as_str() == "exit" && session_epoch.0 == exit_epoch
            )
        })
    });
    let listed = rpc.list();
    assert!(matches!(
        find(&listed, "exit").state,
        ChildState::LeaderExited(ref record)
            if record.exit.as_ref().is_some_and(|exit| exit.code == Some(7))
    ));
    let exits = controls(&log.snapshot())
        .into_iter()
        .filter(|event| {
            matches!(
                event,
                ControlEvent::Exited { session, .. } if session.as_str() == "exit"
            )
        })
        .count();
    assert_eq!(exits, 1);
    rpc.kill("echo", echo_epoch, Some(lease)).expect("kill");
    log.wait_until(Duration::from_secs(3), |events| {
        controls(events).iter().any(|event| {
            matches!(
                event,
                ControlEvent::Exited { session, .. } if session.as_str() == "echo"
            )
        }) && controls(events).iter().any(|event| {
            matches!(
                event,
                ControlEvent::CleanupComplete { session, session_epoch, .. }
                    if session.as_str() == "echo" && session_epoch.0 == echo_epoch
            )
        })
    });
    drop(daemon.stderr.lock().expect("stderr"));
}

#[test]
fn duplicate_session_is_refused() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let mut rpc = Rpc::connect(&daemon.paths.endpoint);
    rpc.spawn("same", &["stall"]);
    let spec = spec("same", &["stall"]);
    let reason = failed_reason(
        rpc.call(|id| HostRequest::Spawn { id, spec })
            .expect("response"),
    );
    assert!(reason.contains("duplicate"), "{reason}");
    assert_eq!(rpc.list().len(), 1);
}

#[test]
fn second_daemon_is_refused() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let _ = Rpc::connect(&daemon.paths.endpoint);
    let mut cmd = Command::new(host_bin());
    cmd.arg("--socket")
        .arg(daemon.paths.endpoint.socket_path())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("second");
    let deadline = Instant::now() + Duration::from_secs(3);
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("second daemon stayed up");
        }
        thread::sleep(Duration::from_millis(20));
    };
    assert!(!status.success());
}

#[test]
fn reclaim_lease_refuses_stale_send() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let mut owner = Rpc::connect(&daemon.paths.endpoint);
    let child = owner.spawn(
        "rec",
        &["record", daemon.paths.transcript.to_str().unwrap()],
    );
    let epoch = epoch_of(&child);
    let first = owner.claim("rec", epoch).expect("lease");
    let mut next = Rpc::connect(&daemon.paths.endpoint);
    let second = next.claim("rec", epoch).expect("reclaim");
    assert!(second.0 > first.0);
    let outcome = owner.send("rec", epoch, first, "STALE");
    assert_eq!(not_written(&outcome), &SendBlock::StaleLease);
    thread::sleep(Duration::from_millis(50));
    let text = std::fs::read(&daemon.paths.transcript).unwrap_or_default();
    assert!(!text.windows(5).any(|window| window == b"STALE"));
}

#[test]
fn reclaim_lease_fences_already_queued_write() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    arm(&daemon.paths.barrier, "before-first");
    let mut owner = Rpc::connect(&daemon.paths.endpoint);
    let child = owner.spawn("q", &["record", daemon.paths.transcript.to_str().unwrap()]);
    let epoch = epoch_of(&child);
    let lease = owner.claim("q", epoch).expect("lease");
    let queued = thread::spawn(move || owner.send("q", epoch, lease, "QUEUED"));
    wait_at(&daemon.paths.barrier, "before-first");
    let mut next = Rpc::connect(&daemon.paths.endpoint);
    let renewed = next.claim("q", epoch).expect("reclaim");
    assert!(renewed.0 > lease.0);
    release(&daemon.paths.barrier, "before-first");
    let outcome = queued.join().expect("join");
    assert_eq!(not_written(&outcome), &SendBlock::StaleLease);
    thread::sleep(Duration::from_millis(50));
    let text = std::fs::read(&daemon.paths.transcript).unwrap_or_default();
    assert!(!text.windows(6).any(|window| window == b"QUEUED"));
}

#[test]
fn stale_epoch_cannot_write_to_respawn() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let mut rpc = Rpc::connect(&daemon.paths.endpoint);
    let path = daemon.paths.transcript.clone();
    let child = rpc.spawn("again", &["record", path.to_str().unwrap()]);
    let old_epoch = epoch_of(&child);
    let lease = rpc.claim("again", old_epoch).expect("lease");
    rpc.kill("again", old_epoch, Some(lease)).expect("kill");
    let log = EventLog::subscribe(&daemon.paths.endpoint, Some("again"));
    log.wait_until(Duration::from_secs(3), |events| {
        controls(events).iter().any(|event| {
            matches!(event, ControlEvent::CleanupComplete { session, .. } if session.as_str() == "again")
        })
    });
    let path2 = daemon
        .paths
        .root
        .as_ref()
        .expect("root")
        .path()
        .join("stdin2.bin");
    let spawned = rpc.spawn("again", &["record", path2.to_str().unwrap()]);
    let new_epoch = epoch_of(&spawned);
    assert!(new_epoch > old_epoch);
    let outcome = rpc.send("again", old_epoch, lease, "OLD");
    assert_eq!(not_written(&outcome), &SendBlock::StaleEpoch);
    thread::sleep(Duration::from_millis(50));
    let text = std::fs::read(&path2).unwrap_or_default();
    assert!(!text.windows(3).any(|window| window == b"OLD"));
}

#[test]
fn daemon_restart_never_reuses_session_epoch() {
    let mut daemon = Daemon::start(DaemonOpts { chunk: None });
    let endpoint = daemon.paths.endpoint.clone();
    let root = daemon
        .paths
        .root
        .as_ref()
        .expect("root")
        .path()
        .to_path_buf();
    let mut rpc = Rpc::connect(&endpoint);
    let child = rpc.spawn("persist", &["stall"]);
    let old_epoch = epoch_of(&child);
    let lease = rpc.claim("persist", old_epoch).expect("lease");
    rpc.kill("persist", old_epoch, None).expect("kill");
    let log = EventLog::subscribe(&endpoint, Some("persist"));
    log.wait_until(Duration::from_secs(3), |events| {
        controls(events)
            .iter()
            .any(|event| matches!(event, ControlEvent::CleanupComplete { .. }))
    });
    drop(log);
    let registry = fl_host::read_registry(&endpoint).expect("registry");
    assert!(
        registry
            .sessions
            .iter()
            .all(|session| session.session != "persist"),
        "retired cleanup record must leave the session list"
    );
    assert!(registry.high_water >= old_epoch);
    drop(rpc);
    daemon.stop();
    let restarted = Daemon {
        child: {
            let mut cmd = Command::new(host_bin());
            cmd.arg("--socket")
                .arg(endpoint.socket_path())
                .env("FL_HOST_WRITER_BARRIER", root.join("barrier"))
                .env("FL_HOST_SUBSCRIBER_HOLD", root.join("subscriber"))
                .env("FL_HOST_BACKLOG_STATUS", root.join("backlog"))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            cmd.spawn().expect("restart")
        },
        paths: Paths {
            endpoint: endpoint.clone(),
            barrier: root.join("barrier"),
            subscriber: root.join("subscriber"),
            status: root.join("backlog"),
            transcript: root.join("stdin.bin"),
            root: daemon.paths.root.take(),
        },
        stderr: Arc::new(Mutex::new(String::new())),
        stderr_thread: None,
    };
    // The first daemon is stopped and no longer owns the directory. Drop must
    // not delete the endpoint the replacement is serving.
    let mut rpc = Rpc::connect(&restarted.paths.endpoint);
    let path = restarted.paths.transcript.clone();
    let spawned = rpc.spawn("persist", &["record", path.to_str().unwrap()]);
    let new_epoch = epoch_of(&spawned);
    assert!(new_epoch > old_epoch);
    let outcome = rpc.send("persist", old_epoch, lease, "REUSED");
    assert_eq!(not_written(&outcome), &SendBlock::StaleEpoch);
    let text = std::fs::read(&path).unwrap_or_default();
    assert!(!text.windows(6).any(|window| window == b"REUSED"));
    drop(restarted);
}

#[test]
fn stdout_line_overflow_fails_session() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let log = EventLog::subscribe(&daemon.paths.endpoint, Some("big"));
    let mut rpc = Rpc::connect(&daemon.paths.endpoint);
    let child = rpc.spawn("big", &["overflow"]);
    let epoch = epoch_of(&child);
    log.wait_until(Duration::from_secs(3), |events| {
        controls(events).iter().any(|event| {
            matches!(
                event,
                ControlEvent::ProtocolFailure { session, session_epoch, .. }
                    if session.as_str() == "big" && session_epoch.0 == epoch
            )
        })
    });
    assert!(
        line_bytes(&log.snapshot(), "big")
            .iter()
            .all(|line| line.len() <= 1024 * 1024 && line != &vec![b'z'; 1024 * 1024])
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let listed = rpc.list();
        if let Some(child) = listed
            .iter()
            .find(|child| record(&child.state).session.as_str() == "big")
            && !matches!(child.state, ChildState::Running(_))
        {
            break;
        }
        if Instant::now() >= deadline {
            panic!("overflow did not fail the session");
        }
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn stdin_queue_refuses_over_capacity() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    arm(&daemon.paths.barrier, "before-first");
    let mut rpc = Rpc::connect(&daemon.paths.endpoint);
    let path = daemon.paths.transcript.clone();
    let child = rpc.spawn("cap", &["record", path.to_str().unwrap()]);
    let epoch = epoch_of(&child);
    let lease = rpc.claim("cap", epoch).expect("lease");
    for index in 0..64 {
        let _ = rpc.push(|id| HostRequest::Send {
            id,
            session: sid("cap"),
            expected_epoch: SessionEpoch(epoch),
            lease,
            line: "Q".to_owned(),
        });
        if index == 0 {
            wait_at(&daemon.paths.barrier, "before-first");
        }
    }
    wait_depth(&daemon.paths.barrier, 63);
    let refused = rpc.send("cap", epoch, lease, "NO");
    assert_eq!(not_written(&refused), &SendBlock::QueueFull);
    release(&daemon.paths.barrier, "before-first");
    for _ in 0..64 {
        match rpc.pull() {
            HostResponse::Sent {
                outcome: SendOutcome::Written,
                ..
            } => {}
            other => panic!("queued send: {other:?}"),
        }
    }
    let text = wait_file(&path, |bytes| {
        bytes.iter().filter(|byte| **byte == b'Q').count() >= 64
    });
    assert!(!text.windows(2).any(|window| window == b"NO"));

    arm(&daemon.paths.barrier, "before-first");
    let path_b = daemon
        .paths
        .root
        .as_ref()
        .expect("root")
        .path()
        .join("bytes.bin");
    let child = rpc.spawn("bytes", &["record", path_b.to_str().unwrap()]);
    let epoch = epoch_of(&child);
    let lease = rpc.claim("bytes", epoch).expect("lease");
    let fat = "B".repeat(60_000);
    for index in 0..4 {
        let line = fat.clone();
        let _ = rpc.push(|id| HostRequest::Send {
            id,
            session: sid("bytes"),
            expected_epoch: SessionEpoch(epoch),
            lease,
            line,
        });
        if index == 0 {
            wait_at(&daemon.paths.barrier, "before-first");
        }
    }
    wait_depth(&daemon.paths.barrier, 3);
    let overflow = format!("BYTECAP{}", "B".repeat(30_000));
    let refused = rpc.send("bytes", epoch, lease, &overflow);
    assert_eq!(not_written(&refused), &SendBlock::QueueFull);
    release(&daemon.paths.barrier, "before-first");
    for _ in 0..4 {
        match rpc.pull() {
            HostResponse::Sent {
                outcome: SendOutcome::Written,
                ..
            } => {}
            other => panic!("byte send: {other:?}"),
        }
    }
    let text = wait_file(&path_b, |bytes| bytes.len() >= 60_001 * 4);
    assert!(!text.windows(7).any(|window| window == b"BYTECAP"));
    assert!(text.len() < 256 * 1024 + 64);
}

#[test]
fn stdin_progress_timeout_is_bounded() {
    let daemon = Daemon::start(DaemonOpts { chunk: Some(1) });
    arm(&daemon.paths.barrier, "after-chunk");
    let mut owner = Rpc::connect(&daemon.paths.endpoint);
    let child = owner.spawn("slow", &["stall"]);
    let epoch = epoch_of(&child);
    let lease = owner.claim("slow", epoch).expect("lease");
    let barrier = daemon.paths.barrier.clone();
    let started = Instant::now();
    let send = thread::spawn(move || owner.send("slow", epoch, lease, &"P".repeat(80)));
    wait_at(&barrier, "after-chunk");
    let pulser = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            let at = barrier.join("at-after-chunk");
            if at.exists() {
                let _ = std::fs::write(barrier.join("go-after-chunk"), b"1");
            }
            thread::sleep(Duration::from_millis(100));
        }
    });
    let outcome = send.join().expect("join");
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(4500) && elapsed < Duration::from_secs(8),
        "elapsed {elapsed:?}"
    );
    assert!(matches!(
        outcome,
        SendOutcome::Ambiguous {
            reason: SendBlock::Timeout
        }
    ));
    let mut rpc = Rpc::connect(&daemon.paths.endpoint);
    let listed = rpc.list();
    assert_eq!(
        record(&find(&listed, "slow").state).input,
        InputState::Fenced
    );
    let _ = pulser.join();
}

#[test]
fn stderr_flood_does_not_stall_stdout() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let log = EventLog::subscribe(&daemon.paths.endpoint, Some("mix"));
    let mut rpc = Rpc::connect(&daemon.paths.endpoint);
    let started = Instant::now();
    rpc.spawn("mix", &["stderr-and-stdout"]);
    log.wait_until(Duration::from_secs(2), |events| {
        line_bytes(events, "mix")
            .iter()
            .any(|line| line == b"STDOUT_OK")
    });
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn maximum_line_is_fragmented_losslessly() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let log = EventLog::subscribe(&daemon.paths.endpoint, Some("max"));
    let mut rpc = Rpc::connect(&daemon.paths.endpoint);
    rpc.spawn("max", &["maxline"]);
    let lines = log.wait_until(Duration::from_secs(5), |events| {
        line_bytes(events, "max")
            .iter()
            .any(|line| line.len() == 1024 * 1024)
    });
    let line = line_bytes(&lines, "max")
        .into_iter()
        .find(|line| line.len() == 1024 * 1024)
        .expect("max line");
    assert!(line.iter().all(|byte| *byte == b'y'));
}

#[test]
fn no_consumer_sets_stalled_without_unbounded_growth() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let mut rpc = Rpc::connect(&daemon.paths.endpoint);
    rpc.spawn("empty", &["flood-empty"]);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut saw = false;
    while Instant::now() < deadline {
        let listed = rpc.list();
        if record(&find(&listed, "empty").state).consumer_stalled {
            saw = true;
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(saw, "empty-line flood did not stall");
    let status = std::fs::read_to_string(&daemon.paths.status).unwrap_or_default();
    let events = status_field(&status, "events");
    let charge = status_field(&status, "charge");
    assert!(events <= 4096, "{status}");
    assert!(events >= 100, "{status}");
    assert!(charge <= 8 * 1024 * 1024, "{status}");

    rpc.spawn("bytes", &["flood-stdout"]);
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let listed = rpc.list();
        if record(&find(&listed, "bytes").state).consumer_stalled {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    let status = std::fs::read_to_string(&daemon.paths.status).unwrap_or_default();
    let charge = status_field(&status, "charge");
    let raw = status_field(&status, "raw");
    assert!(raw > 1024 * 1024, "{status}");
    assert!(charge <= 8 * 1024 * 1024, "{status}");
    assert!(raw <= 8 * 1024 * 1024, "{status}");
    #[cfg(target_os = "linux")]
    {
        let text =
            std::fs::read_to_string(format!("/proc/{}/status", daemon.child.id())).expect("status");
        let rss = text
            .lines()
            .find_map(|line| line.strip_prefix("VmRSS:"))
            .expect("rss");
        let kb: u64 = rss.split_whitespace().next().unwrap().parse().unwrap();
        assert!(kb < 128 * 1024, "rss {kb}");
    }
}

fn status_field(text: &str, key: &str) -> usize {
    text.lines()
        .rev()
        .find_map(|line| {
            line.split_whitespace().find_map(|field| {
                let (name, value) = field.split_once('=')?;
                if name == key {
                    value.parse().ok()
                } else {
                    None
                }
            })
        })
        .unwrap_or_else(|| panic!("missing {key} in {text}"))
}

#[test]
fn slow_subscriber_dropped_not_blocking() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let endpoint = daemon.paths.endpoint.clone();
    let healthy = EventLog::subscribe(&endpoint, Some("out"));
    let slow_stop = Arc::new(AtomicBool::new(false));
    let slow_events = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::clone(&slow_stop);
    let sink = Arc::clone(&slow_events);
    let endpoint2 = endpoint.clone();
    let slow = thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let mut stream = None;
        let connect_deadline = Instant::now() + Duration::from_secs(3);
        let mut last = String::new();
        while stream.is_none() && Instant::now() < connect_deadline {
            match runtime.block_on(fl_host_proto::events(&endpoint2, Some(sid("out")))) {
                Ok(events) => stream = Some(events),
                Err(err) => {
                    last = err.to_string();
                    thread::sleep(Duration::from_millis(20));
                }
            }
        }
        let mut stream = stream.unwrap_or_else(|| panic!("slow subscribe: {last}"));
        while !stop.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(20));
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            match runtime.block_on(async {
                tokio::time::timeout(Duration::from_millis(100), stream.next()).await
            }) {
                Ok(Ok(event)) => sink.lock().expect("slow").push(event),
                Ok(Err(_)) => break,
                Err(_) => break,
            }
        }
    });
    thread::sleep(Duration::from_millis(100));
    let mut rpc = Rpc::connect(&endpoint);
    let started = Instant::now();
    rpc.spawn("out", &["lines", "200"]);
    healthy.wait_until(Duration::from_secs(3), |events| {
        line_bytes(events, "out")
            .iter()
            .filter(|line| line.starts_with(b"L"))
            .count()
            >= 200
    });
    assert!(started.elapsed() < Duration::from_secs(3));
    let lines = line_bytes(&healthy.snapshot(), "out");
    let data: Vec<_> = lines
        .iter()
        .filter(|line| line.starts_with(b"L"))
        .cloned()
        .collect();
    assert_eq!(data.len(), 200);
    for (index, line) in data.iter().enumerate() {
        assert_eq!(line, &format!("L{index}").into_bytes());
    }
    slow_stop.store(true, Ordering::SeqCst);
    slow.join().expect("slow");
    let slow_lines = line_bytes(&slow_events.lock().expect("slow").clone(), "out");
    let slow_count = slow_lines
        .iter()
        .filter(|line| line.starts_with(b"L"))
        .count();
    assert!(
        slow_count < 200,
        "slow subscriber was not dropped: {slow_count}"
    );
    let saw_slow = slow_events.lock().expect("slow").iter().any(|event| {
        matches!(
            event,
            HostEvent::Control(ControlEvent::SlowSubscriber { .. })
        )
    });
    assert!(
        saw_slow,
        "missing SlowSubscriber after the slow subscriber was dropped; lines {slow_count}"
    );
}

#[test]
fn subscriber_count_queue_and_terminal_slot_are_bounded() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let endpoint = daemon.paths.endpoint.clone();
    let mut parked = Vec::new();
    for _ in 0..16 {
        parked.push(EventLog::subscribe(&endpoint, None));
    }
    thread::sleep(Duration::from_millis(200));
    let mut rpc = Rpc::connect(&endpoint);
    let reason = failed_reason(
        rpc.call(|id| HostRequest::Events { id, session: None })
            .expect("response"),
    );
    assert!(reason.contains("subscriber"), "{reason}");
    drop(parked);

    arm(&daemon.paths.subscriber, "before-exited");
    let log = EventLog::subscribe(&endpoint, Some("slot"));
    thread::sleep(Duration::from_millis(100));
    let child = rpc.spawn("slot", &["stall"]);
    let epoch = epoch_of(&child);
    rpc.kill("slot", epoch, None).expect("kill");
    wait_at(&daemon.paths.subscriber, "before-exited");
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        let listed = rpc.list();
        if matches!(find(&listed, "slot").state, ChildState::CleanupComplete(_)) {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    let child = rpc.spawn("slot", &["stall"]);
    let new_epoch = epoch_of(&child);
    assert!(new_epoch > epoch);
    rpc.kill("slot", new_epoch, None).expect("second kill");
    let held = log.wait_until(Duration::from_secs(3), |events| {
        !events.is_empty()
            && controls(events)
                .iter()
                .any(|event| matches!(event, ControlEvent::SlowSubscriber { .. }))
    });
    let replaced = controls(&held).iter().any(|event| {
        matches!(
            event,
            ControlEvent::Exited { session, session_epoch, .. }
                if session.as_str() == "slot" && session_epoch.0 == new_epoch
        )
    });
    assert!(
        !replaced,
        "occupied terminal slot was overwritten by the respawned exit"
    );
    release(&daemon.paths.subscriber, "before-exited");
    let mut fresh = Rpc::connect(&endpoint);
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let listed = fresh.list();
        let row = record(&find(&listed, "slot").state);
        if row.session_epoch.0 == new_epoch && row.exit.is_some() {
            break;
        }
        if Instant::now() >= deadline {
            panic!("inventory lost the respawned exit");
        }
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn events_connection_is_stream_only() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let mut rpc = Rpc::connect(&daemon.paths.endpoint);
    rpc.spawn("stream", &["stall"]);
    let mut events = Rpc::connect(&daemon.paths.endpoint);
    let subscribed = events
        .call(|id| HostRequest::Events {
            id,
            session: Some(sid("stream")),
        })
        .expect("subscribe");
    assert!(matches!(subscribed, HostResponse::Subscribed { .. }));
    let err = events
        .call(|id| HostRequest::List { id })
        .expect_err("stream connection accepted another rpc");
    assert!(matches!(
        err,
        HostError::Protocol(_) | HostError::Io(_) | HostError::FrameTimeout
    ));
}

#[test]
fn pending_output_survives_last_subscriber_drop() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    arm(&daemon.paths.subscriber, "before-send");
    let endpoint = daemon.paths.endpoint.clone();
    let log = EventLog::subscribe(&endpoint, Some("keep"));
    let mut rpc = Rpc::connect(&endpoint);
    rpc.spawn("keep", &["lines", "3"]);
    wait_at(&daemon.paths.subscriber, "before-send");
    drop(log);
    release(&daemon.paths.subscriber, "before-send");
    let again = EventLog::subscribe(&endpoint, Some("keep"));
    again.wait_until(Duration::from_secs(3), |events| {
        let lines = line_bytes(events, "keep");
        lines.iter().any(|line| line == b"L0")
            && lines.iter().any(|line| line == b"L1")
            && lines.iter().any(|line| line == b"L2")
    });
}

#[test]
fn partial_write_then_takeover_fences_and_refuses_new_lease() {
    let daemon = Daemon::start(DaemonOpts { chunk: Some(4) });
    arm(&daemon.paths.barrier, "after-chunk");
    let mut owner = Rpc::connect(&daemon.paths.endpoint);
    let path = daemon.paths.transcript.clone();
    let child = owner.spawn("part", &["record", path.to_str().unwrap()]);
    let epoch = epoch_of(&child);
    let lease = owner.claim("part", epoch).expect("lease");
    let send = thread::spawn(move || owner.send("part", epoch, lease, "HELLO-WORLD"));
    wait_at(&daemon.paths.barrier, "after-chunk");
    let _ = wait_file(&path, |bytes| bytes.starts_with(b"HELL"));
    let mut next = Rpc::connect(&daemon.paths.endpoint);
    let renewed = next.claim("part", epoch).expect("new lease");
    assert!(renewed.0 > lease.0);
    let outcome = send.join().expect("join");
    assert!(matches!(
        outcome,
        SendOutcome::Ambiguous { .. } | SendOutcome::NotWritten { .. }
    ));
    let refused = next.send("part", epoch, renewed, "NEW");
    assert_eq!(not_written(&refused), &SendBlock::SessionFenced);
    let listed = next.list();
    assert_eq!(
        record(&find(&listed, "part").state).input,
        InputState::Fenced
    );
    let text = std::fs::read(&path).unwrap_or_default();
    assert!(text.starts_with(b"HELL"));
    assert!(!text.windows(12).any(|window| window == b"HELLO-WORLD\n"));
    assert!(!text.windows(3).any(|window| window == b"NEW"));
}

#[test]
fn fenced_session_allows_epoch_guarded_signal_and_kill() {
    let daemon = Daemon::start(DaemonOpts { chunk: Some(4) });
    arm(&daemon.paths.barrier, "after-chunk");
    let mut owner = Rpc::connect(&daemon.paths.endpoint);
    let child = owner.spawn(
        "fence",
        &["record", daemon.paths.transcript.to_str().unwrap()],
    );
    let epoch = epoch_of(&child);
    let lease = owner.claim("fence", epoch).expect("lease");
    let send = thread::spawn(move || owner.send("fence", epoch, lease, "HELLO-WORLD"));
    wait_at(&daemon.paths.barrier, "after-chunk");
    let mut next = Rpc::connect(&daemon.paths.endpoint);
    let renewed = next.claim("fence", epoch).expect("lease");
    let _ = send.join();
    next.signal("fence", epoch, renewed, SignalKind::Interrupt)
        .expect("signal");
    next.kill("fence", epoch, Some(renewed)).expect("kill");
    let log = EventLog::subscribe(&daemon.paths.endpoint, Some("fence"));
    log.wait_until(Duration::from_secs(3), |events| {
        controls(events).iter().any(|event| {
            matches!(event, ControlEvent::CleanupComplete { session, .. } if session.as_str() == "fence")
                || matches!(event, ControlEvent::Exited { session, .. } if session.as_str() == "fence")
        })
    });
}

#[test]
fn claim_ack_is_boundary_for_pending_os_write() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    arm(&daemon.paths.barrier, "pending");
    let mut owner = Rpc::connect(&daemon.paths.endpoint);
    let path = daemon.paths.transcript.clone();
    let child = owner.spawn("ack", &["record", path.to_str().unwrap()]);
    let epoch = epoch_of(&child);
    let lease = owner.claim("ack", epoch).expect("lease");
    let id_one = owner.push(|id| HostRequest::Send {
        id,
        session: sid("ack"),
        expected_epoch: SessionEpoch(epoch),
        lease,
        line: "ONE".to_owned(),
    });
    wait_at(&daemon.paths.barrier, "pending");
    let _ = wait_file(&path, |bytes| {
        bytes.windows(4).any(|window| window == b"ONE\n")
    });
    let id_two = owner.push(|id| HostRequest::Send {
        id,
        session: sid("ack"),
        expected_epoch: SessionEpoch(epoch),
        lease,
        line: "TWO".to_owned(),
    });
    let endpoint = daemon.paths.endpoint.clone();
    let claim = thread::spawn(move || {
        let mut rpc = Rpc::connect(&endpoint);
        let renewed = rpc.claim("ack", epoch).expect("claim");
        (renewed, rpc)
    });
    thread::sleep(Duration::from_millis(200));
    assert!(!claim.is_finished());
    release(&daemon.paths.barrier, "pending");
    let (renewed, mut rpc) = claim.join().expect("join");
    assert!(renewed.0 > lease.0);
    let mut outcomes = std::collections::BTreeMap::new();
    for _ in 0..2 {
        match owner.pull() {
            HostResponse::Sent { id, outcome } => {
                outcomes.insert(id.0, outcome);
            }
            other => panic!("send result: {other:?}"),
        }
    }
    assert_eq!(outcomes.get(&id_one.0), Some(&SendOutcome::Written));
    assert_eq!(
        not_written(outcomes.get(&id_two.0).expect("two")),
        &SendBlock::StaleLease
    );
    assert_eq!(
        rpc.send("ack", epoch, renewed, "THREE"),
        SendOutcome::Written
    );
    let text = wait_file(&path, |bytes| {
        bytes.windows(6).any(|window| window == b"THREE\n")
    });
    assert!(!text.windows(4).any(|window| window == b"TWO\n"));
}

#[test]
fn claim_deadline_cannot_be_extended_by_input_progress() {
    let daemon = Daemon::start(DaemonOpts { chunk: Some(1) });
    arm(&daemon.paths.barrier, "pending");
    let mut owner = Rpc::connect(&daemon.paths.endpoint);
    let child = owner.spawn("deadline", &["stall"]);
    let epoch = epoch_of(&child);
    let lease = owner.claim("deadline", epoch).expect("lease");
    let send = thread::spawn(move || owner.send("deadline", epoch, lease, &"D".repeat(80)));
    wait_at(&daemon.paths.barrier, "pending");
    let endpoint = daemon.paths.endpoint.clone();
    let started = Instant::now();
    let claim = thread::spawn(move || {
        let mut rpc = Rpc::connect(&endpoint);
        rpc.claim("deadline", epoch)
    });
    let barrier = daemon.paths.barrier.clone();
    let pulser = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            let at = barrier.join("at-pending");
            if at.exists() {
                let _ = std::fs::write(barrier.join("go-pending"), b"1");
            }
            thread::sleep(Duration::from_millis(100));
        }
    });
    let result = claim.join().expect("join");
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(4500) && elapsed < Duration::from_secs(8),
        "elapsed {elapsed:?} result {result:?}"
    );
    assert!(result.is_err(), "{result:?}");
    let outcome = send.join().expect("join");
    assert!(matches!(outcome, SendOutcome::Ambiguous { .. }));
    let mut rpc = Rpc::connect(&daemon.paths.endpoint);
    let listed = rpc.list();
    assert_eq!(
        record(&find(&listed, "deadline").state).input,
        InputState::Fenced
    );
    let _ = pulser.join();
}

#[test]
fn caller_cancellation_does_not_drop_writer() {
    let daemon = Daemon::start(DaemonOpts { chunk: Some(4) });
    arm(&daemon.paths.barrier, "before-first");
    let endpoint = daemon.paths.endpoint.clone();
    let path = daemon.paths.transcript.clone();
    let mut setup = Rpc::connect(&endpoint);
    let child = setup.spawn("cancel", &["record", path.to_str().unwrap()]);
    let epoch = epoch_of(&child);
    drop(setup);
    let (cancel_tx, cancel_rx) = std::sync::mpsc::channel::<()>();
    let (aborted_tx, aborted_rx) = std::sync::mpsc::channel::<()>();
    let endpoint_task = endpoint.clone();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let task = runtime.spawn(async move {
        let mut client = fl_host_proto::AsyncHostClient::connect(&endpoint_task)
            .await
            .expect("connect");
        let response = client
            .request(HostRequest::ClaimLease {
                id: RequestId(1),
                session: sid("cancel"),
                expected_epoch: SessionEpoch(epoch),
            })
            .await
            .expect("claim");
        let HostResponse::Lease { lease, .. } = response else {
            panic!("claim {response:?}");
        };
        let cancel = tokio::task::spawn_blocking(move || {
            let _ = cancel_rx.recv();
        });
        {
            let send = client.request(HostRequest::Send {
                id: RequestId(2),
                session: sid("cancel"),
                expected_epoch: SessionEpoch(epoch),
                lease,
                line: "KEEP".into(),
            });
            tokio::pin!(send);
            tokio::select! {
                _ = &mut send => {}
                _ = cancel => {}
            }
        }
        // The request future is gone. The socket stays open until the sleep ends.
        let _ = aborted_tx.send(());
        tokio::time::sleep(Duration::from_secs(3)).await;
        drop(client);
    });
    wait_at(&daemon.paths.barrier, "before-first");
    let early = std::fs::read(&path).unwrap_or_default();
    assert!(
        !early.windows(4).any(|window| window == b"KEEP"),
        "writer was not blocked before the first byte: {early:?}"
    );
    cancel_tx.send(()).expect("cancel");
    aborted_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("request future was not aborted while the writer was blocked");
    release(&daemon.paths.barrier, "before-first");
    let text = wait_file(&path, |bytes| {
        bytes.windows(5).any(|window| window == b"KEEP\n")
    });
    assert!(text.windows(5).any(|window| window == b"KEEP\n"));
    let mut rpc = Rpc::connect(&endpoint);
    let renewed = rpc
        .claim("cancel", epoch)
        .expect("writer still accepts claims");
    assert_eq!(
        rpc.send("cancel", epoch, renewed, "NEXT"),
        SendOutcome::Written
    );
    let text = wait_file(&path, |bytes| {
        bytes.windows(5).any(|window| window == b"NEXT\n")
    });
    assert!(text.windows(5).any(|window| window == b"KEEP\n"));
    task.abort();
    let _ = runtime.block_on(task);
}

#[test]
fn control_disconnect_revokes_queued_input() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    arm(&daemon.paths.barrier, "before-first");
    let mut rpc = Rpc::connect(&daemon.paths.endpoint);
    let path = daemon.paths.transcript.clone();
    let child = rpc.spawn("drop", &["record", path.to_str().unwrap()]);
    let epoch = epoch_of(&child);
    let endpoint = daemon.paths.endpoint.clone();
    let barrier = daemon.paths.barrier.clone();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let task = runtime.spawn(async move {
        let mut client = fl_host_proto::AsyncHostClient::connect(&endpoint)
            .await
            .expect("async connect");
        let response = client
            .request(HostRequest::ClaimLease {
                id: RequestId(1),
                session: sid("drop"),
                expected_epoch: SessionEpoch(epoch),
            })
            .await
            .expect("claim");
        let HostResponse::Lease { lease, .. } = response else {
            panic!("claim {response:?}");
        };
        let _ = client
            .request(HostRequest::Send {
                id: RequestId(2),
                session: sid("drop"),
                expected_epoch: SessionEpoch(epoch),
                lease,
                line: "SECRET".into(),
            })
            .await;
    });
    wait_at(&daemon.paths.barrier, "before-first");
    task.abort();
    let abort_deadline = Instant::now() + Duration::from_secs(2);
    while !task.is_finished() {
        if Instant::now() >= abort_deadline {
            panic!("aborted send did not drop");
        }
        thread::sleep(Duration::from_millis(5));
    }
    let revoked = barrier.join("revoked");
    let deadline = Instant::now() + Duration::from_secs(3);
    while !revoked.exists() {
        if Instant::now() >= deadline {
            panic!("disconnect did not revoke queued input");
        }
        thread::sleep(Duration::from_millis(5));
    }
    release(&daemon.paths.barrier, "before-first");
    let mut rpc = Rpc::connect(&daemon.paths.endpoint);
    let renewed = rpc.claim("drop", epoch).expect("lease");
    assert_eq!(
        rpc.send("drop", epoch, renewed, "FRESH"),
        SendOutcome::Written
    );
    let text = wait_file(&path, |bytes| {
        bytes.windows(6).any(|window| window == b"FRESH\n")
    });
    assert!(!text.windows(6).any(|window| window == b"SECRET"));
}

#[test]
fn send_reports_not_written_vs_ambiguous() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let mut owner = Rpc::connect(&daemon.paths.endpoint);
    let path = daemon.paths.transcript.clone();
    let child = owner.spawn("kind", &["record", path.to_str().unwrap()]);
    let epoch = epoch_of(&child);
    let lease = owner.claim("kind", epoch).expect("lease");
    let mut other = Rpc::connect(&daemon.paths.endpoint);
    let _ = other.claim("kind", epoch).expect("reclaim");
    let outcome = owner.send("kind", epoch, lease, "ZERO");
    assert_eq!(not_written(&outcome), &SendBlock::StaleLease);
    let text = std::fs::read(&path).unwrap_or_default();
    assert!(!text.windows(4).any(|window| window == b"ZERO"));

    let child = owner.spawn("full", &["stall"]);
    let epoch = epoch_of(&child);
    let lease = owner.claim("full", epoch).expect("lease");
    let started = Instant::now();
    let outcome = owner.send("full", epoch, lease, &"Z".repeat(200_000));
    assert!(started.elapsed() < Duration::from_secs(8));
    assert!(matches!(
        outcome,
        SendOutcome::Ambiguous {
            reason: SendBlock::Timeout
        }
    ));
}

#[test]
fn send_reply_waits_for_newline_write_result() {
    let daemon = Daemon::start(DaemonOpts { chunk: Some(3) });
    arm(&daemon.paths.barrier, "after-chunk");
    let mut owner = Rpc::connect(&daemon.paths.endpoint);
    let path = daemon.paths.transcript.clone();
    let child = owner.spawn("nl", &["record", path.to_str().unwrap()]);
    let epoch = epoch_of(&child);
    let lease = owner.claim("nl", epoch).expect("lease");
    let send = thread::spawn(move || owner.send("nl", epoch, lease, "ABC"));
    wait_at(&daemon.paths.barrier, "after-chunk");
    let partial = wait_file(&path, |bytes| bytes.starts_with(b"ABC"));
    assert!(!partial.windows(4).any(|window| window == b"ABC\n"));
    assert!(!send.is_finished());
    release(&daemon.paths.barrier, "after-chunk");
    assert_eq!(send.join().expect("join"), SendOutcome::Written);
    let text = wait_file(&path, |bytes| {
        bytes.windows(4).any(|window| window == b"ABC\n")
    });
    assert!(text.windows(4).any(|window| window == b"ABC\n"));
}

#[test]
fn lease_is_bound_to_session_epoch_and_control_connection() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let mut owner = Rpc::connect(&daemon.paths.endpoint);
    let path = daemon.paths.transcript.clone();
    let child = owner.spawn("bound", &["record", path.to_str().unwrap()]);
    let epoch = epoch_of(&child);
    owner.spawn("other", &["stall"]);
    let lease = owner.claim("bound", epoch).expect("lease");
    let mut stranger = Rpc::connect(&daemon.paths.endpoint);
    let wrong_conn = stranger.send("bound", epoch, lease, "CONN");
    assert_eq!(not_written(&wrong_conn), &SendBlock::WrongConnection);
    let wrong_epoch = owner.send("bound", epoch + 9, lease, "EPOCH");
    assert_eq!(not_written(&wrong_epoch), &SendBlock::StaleEpoch);
    let wrong_lease = owner.send("bound", epoch, ControllerLease(lease.0 + 10), "LEASE");
    assert_eq!(not_written(&wrong_lease), &SendBlock::StaleLease);
    assert_eq!(
        owner.send("bound", epoch, lease, "OK"),
        SendOutcome::Written
    );
    let text = wait_file(&path, |bytes| {
        bytes.windows(3).any(|window| window == b"OK\n")
    });
    assert!(!text.windows(4).any(|window| window == b"CONN"));
    assert!(!text.windows(5).any(|window| window == b"EPOCH"));
}

#[test]
fn concurrent_claims_are_ordered() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    arm(&daemon.paths.barrier, "before-claim");
    let mut rpc = Rpc::connect(&daemon.paths.endpoint);
    let child = rpc.spawn("race", &["stall"]);
    let epoch = epoch_of(&child);
    let endpoint = daemon.paths.endpoint.clone();
    let first = thread::spawn(move || {
        let mut rpc = Rpc::connect(&endpoint);
        let lease = rpc.claim("race", epoch).expect("claim");
        (lease, rpc)
    });
    wait_at(&daemon.paths.barrier, "before-claim");
    let endpoint = daemon.paths.endpoint.clone();
    let second = thread::spawn(move || {
        let mut rpc = Rpc::connect(&endpoint);
        let lease = rpc.claim("race", epoch).expect("claim");
        (lease, rpc)
    });
    thread::sleep(Duration::from_millis(100));
    release(&daemon.paths.barrier, "before-claim");
    let (left, mut left_rpc) = first.join().expect("join");
    let (right, mut right_rpc) = second.join().expect("join");
    assert_ne!(left.0, right.0);
    let (low, high, winner) = if left.0 < right.0 {
        (left, right, &mut right_rpc)
    } else {
        (right, left, &mut left_rpc)
    };
    assert_eq!(high.0, low.0 + 1);
    let stale = winner.send("race", epoch, low, "LOW");
    assert_eq!(not_written(&stale), &SendBlock::StaleLease);
    assert_eq!(
        winner.send("race", epoch, high, "HIGH"),
        SendOutcome::Written
    );
}

#[test]
fn stale_epoch_kill_refused() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let mut rpc = Rpc::connect(&daemon.paths.endpoint);
    let child = rpc.spawn("live", &["stall"]);
    let epoch = epoch_of(&child);
    let err = rpc.kill("live", epoch + 3, None).expect_err("stale kill");
    assert!(failed_reason(err).contains("stale epoch"));
    let listed = rpc.list();
    assert!(matches!(
        find(&listed, "live").state,
        ChildState::Running(_)
    ));
    rpc.kill("live", epoch, None).expect("kill");
}

#[test]
fn stale_lease_signal_refused() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let mut rpc = Rpc::connect(&daemon.paths.endpoint);
    let child = rpc.spawn("sig", &["stall"]);
    let epoch = epoch_of(&child);
    let lease = rpc.claim("sig", epoch).expect("lease");
    let err = rpc
        .signal(
            "sig",
            epoch,
            ControllerLease(lease.0 + 4),
            SignalKind::Interrupt,
        )
        .expect_err("stale signal");
    assert!(failed_reason(err).contains("stale lease"));
    let listed = rpc.list();
    assert!(matches!(find(&listed, "sig").state, ChildState::Running(_)));
    rpc.signal("sig", epoch, lease, SignalKind::Interrupt)
        .expect("signal");
}

#[test]
fn epoch_guard_serializes_signal_kill_and_respawn() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    arm(&daemon.paths.barrier, "before-signal");
    let mut rpc = Rpc::connect(&daemon.paths.endpoint);
    let child = rpc.spawn("guard", &["stall"]);
    let epoch = epoch_of(&child);
    let lease = rpc.claim("guard", epoch).expect("lease");
    let barrier = daemon.paths.barrier.clone();
    let (queued, queued_rx) = std::sync::mpsc::channel();
    let worker = thread::spawn(move || {
        let _ = rpc.push(|id| HostRequest::Signal {
            id,
            session: sid("guard"),
            expected_epoch: SessionEpoch(epoch),
            lease,
            signal: SignalKind::Hangup,
        });
        wait_at(&barrier, "before-signal");
        let _ = rpc.push(|id| HostRequest::Kill {
            id,
            session: sid("guard"),
            expected_epoch: SessionEpoch(epoch),
            lease: Some(lease),
        });
        queued.send(()).expect("queued");
        (rpc.pull(), rpc.pull())
    });
    wait_at(&daemon.paths.barrier, "before-signal");
    queued_rx
        .recv_timeout(Duration::from_secs(3))
        .expect("kill was not admitted behind the signal");
    let mut rpc = Rpc::connect(&daemon.paths.endpoint);
    let spec = spec("guard", &["stall"]);
    let reason = failed_reason(
        rpc.call(|id| HostRequest::Spawn { id, spec })
            .expect("response"),
    );
    assert!(reason.contains("duplicate"), "{reason}");
    release(&daemon.paths.barrier, "before-signal");
    let (first, second) = worker.join().expect("join");
    assert!(
        matches!(
            (&first, &second),
            (HostResponse::Signaled { .. }, HostResponse::Killed { .. })
                | (HostResponse::Killed { .. }, HostResponse::Signaled { .. })
        ),
        "{first:?} {second:?}"
    );
    let log = EventLog::subscribe(&daemon.paths.endpoint, Some("guard"));
    log.wait_until(Duration::from_secs(3), |events| {
        controls(events)
            .iter()
            .any(|event| matches!(event, ControlEvent::CleanupComplete { .. }))
    });
    let spawned = rpc.spawn("guard", &["stall"]);
    let new_epoch = epoch_of(&spawned);
    assert!(new_epoch > epoch);
    let err = rpc.kill("guard", epoch, None).expect_err("old epoch");
    assert!(failed_reason(err).contains("stale epoch"));
    let listed = rpc.list();
    assert!(matches!(
        find(&listed, "guard").state,
        ChildState::Running(_)
    ));
    assert_eq!(
        record(&find(&listed, "guard").state).session_epoch.0,
        new_epoch
    );
}

#[cfg(unix)]
#[test]
fn terminate_signal_reaches_child_from_real_daemon() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let mut rpc = Rpc::connect(&daemon.paths.endpoint);
    let child = rpc.spawn("term", &["stall"]);
    let epoch = epoch_of(&child);
    let lease = rpc.claim("term", epoch).expect("lease");
    rpc.signal("term", epoch, lease, SignalKind::Terminate)
        .expect("signal");
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let children = rpc.list();
        let row = record(&find(&children, "term").state);
        if let Some(exit) = &row.exit {
            assert_eq!(exit.signal, Some(libc::SIGTERM));
            break;
        }
        assert!(Instant::now() < deadline, "child did not exit from SIGTERM");
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
#[test]
fn respawned_session_allows_bounded_daemon_shutdown() {
    let mut daemon = Daemon::start(DaemonOpts { chunk: None });
    let mut rpc = Rpc::connect(&daemon.paths.endpoint);
    let child = rpc.spawn("respawn-shutdown", &["stall"]);
    let epoch = epoch_of(&child);
    rpc.kill("respawn-shutdown", epoch, None).expect("kill");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let children = rpc.list();
        if matches!(
            find(&children, "respawn-shutdown").state,
            ChildState::CleanupComplete(_)
        ) {
            break;
        }
        assert!(Instant::now() < deadline, "cleanup did not finish");
        thread::sleep(Duration::from_millis(10));
    }
    let replacement = rpc.spawn("respawn-shutdown", &["stall"]);
    assert!(epoch_of(&replacement) > epoch);
    drop(rpc);
    let rc = unsafe { libc::kill(daemon.child.id() as libc::pid_t, libc::SIGTERM) };
    assert_eq!(rc, 0);
    let deadline = Instant::now() + fl_host::CLEANUP_DEADLINE + Duration::from_secs(3);
    loop {
        if let Some(status) = daemon.child.try_wait().expect("wait") {
            assert!(status.success(), "daemon exit: {status}");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "daemon shutdown exceeded deadline"
        );
        thread::sleep(Duration::from_millis(10));
    }
}
