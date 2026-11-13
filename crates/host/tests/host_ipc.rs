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

/// How long a connect helper waits for a (re)started daemon to accept.
/// The daemon binds in `Host::open` before serving, so there is no separate
/// ready signal to wait on; polling the connect is the ready wait. 15 s
/// covers a loaded CI runner where 3 s proved too short (#522).
const DAEMON_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

fn capture_stderr(child: &mut Child) -> (Arc<Mutex<String>>, Option<JoinHandle<()>>) {
    let stderr = Arc::new(Mutex::new(String::new()));
    let thread = child.stderr.take().map(|pipe| {
        let slot = Arc::clone(&stderr);
        thread::spawn(move || {
            let mut reader = std::io::BufReader::new(pipe);
            let mut line = String::new();
            while std::io::BufRead::read_line(&mut reader, &mut line).unwrap_or(0) > 0 {
                slot.lock().expect("stderr").push_str(&line);
                line.clear();
            }
        })
    });
    (stderr, thread)
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
        let (stderr, stderr_thread) = capture_stderr(&mut child);
        Self {
            child,
            paths,
            stderr,
            stderr_thread,
        }
    }

    fn stderr_snapshot(&self) -> String {
        self.stderr.lock().expect("stderr").clone()
    }

    fn rpc(&self) -> Rpc {
        Rpc::connect_with_stderr(&self.paths.endpoint, Some(&self.stderr))
    }

    fn stop(&mut self) {
        #[cfg(unix)]
        {
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
        let err = self.stderr_snapshot();
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
        Self::connect_with_stderr(endpoint, None)
    }

    fn connect_with_stderr(endpoint: &HostEndpoint, stderr: Option<&Arc<Mutex<String>>>) -> Self {
        let deadline = Instant::now() + DAEMON_CONNECT_TIMEOUT;
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
        let stderr = stderr
            .map(|slot| slot.lock().expect("stderr").clone())
            .unwrap_or_else(|| "<not captured>".to_owned());
        let stderr = if stderr.trim().is_empty() {
            "<empty>".to_owned()
        } else {
            stderr
        };
        panic!(
            "daemon did not accept connections within {DAEMON_CONNECT_TIMEOUT:?}: {last}; daemon stderr: {stderr}"
        );
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

    fn claim(&mut self, session: &str, epoch: u64) -> Result<ControllerLease, Box<HostResponse>> {
        match self.call(|id| HostRequest::ClaimLease {
            id,
            session: sid(session),
            expected_epoch: SessionEpoch(epoch),
        }) {
            Ok(HostResponse::Lease { lease, .. }) => Ok(lease),
            Ok(other) => Err(Box::new(other)),
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
    ) -> Result<(), Box<HostResponse>> {
        match self.call(|id| HostRequest::Kill {
            id,
            session: sid(session),
            expected_epoch: SessionEpoch(epoch),
            lease,
        }) {
            Ok(HostResponse::Killed { .. }) => Ok(()),
            Ok(other) => Err(Box::new(other)),
            Err(err) => panic!("kill transport: {err}"),
        }
    }

    fn signal(
        &mut self,
        session: &str,
        epoch: u64,
        lease: ControllerLease,
        signal: SignalKind,
    ) -> Result<(), Box<HostResponse>> {
        match self.call(|id| HostRequest::Signal {
            id,
            session: sid(session),
            expected_epoch: SessionEpoch(epoch),
            lease,
            signal,
        }) {
            Ok(HostResponse::Signaled { .. }) => Ok(()),
            Ok(other) => Err(Box::new(other)),
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
        let (subscribed_tx, subscribed_rx) = std::sync::mpsc::sync_channel(1);
        let endpoint = endpoint.clone();
        let session_filter = filter.map(sid);
        let sink = Arc::clone(&events);
        let flag = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            let mut stream = None;
            let mut last_err = None;
            let deadline = Instant::now() + DAEMON_CONNECT_TIMEOUT;
            while stream.is_none() && Instant::now() < deadline {
                match runtime.block_on(fl_host_proto::events(&endpoint, session_filter.clone())) {
                    Ok(s) => stream = Some(s),
                    Err(err) => {
                        last_err = Some(err);
                        thread::sleep(Duration::from_millis(20));
                    }
                }
            }
            let Some(mut stream) = stream else {
                let _ = subscribed_tx.send(Err(last_err));
                return;
            };
            let _ = subscribed_tx.send(Ok(()));
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
        match subscribed_rx
            .recv()
            .expect("EventLog subscriber thread exited before handshake")
        {
            Ok(()) => {}
            Err(last_err) => {
                let _ = thread.join();
                panic!(
                    "EventLog::subscribe timed out waiting for HostResponse::Subscribed (filter={filter:?}, last_err={last_err:?})"
                );
            }
        }
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
                panic!(
                    "timed out after {} events ({} line bytes)",
                    events.len(),
                    events
                        .iter()
                        .map(|e| match e {
                            HostEvent::Line { bytes, .. } => bytes.len(),
                            _ => 0,
                        })
                        .sum::<usize>()
                );
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
    let mut lines = Vec::new();
    let mut tail = Vec::new();
    for event in events {
        if let HostEvent::Line {
            session: id,
            bytes,
            terminated,
            ..
        } = event
            && id.as_str() == session
        {
            tail.extend_from_slice(bytes);
            if *terminated {
                lines.push(std::mem::take(&mut tail));
            }
        }
    }
    if !tail.is_empty() {
        lines.push(tail);
    }
    lines
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

fn failed_reason(response: impl Into<Box<HostResponse>>) -> String {
    match *response.into() {
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
    let mut rpc = daemon.rpc();
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
    let _ = daemon.rpc();
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
    let mut owner = daemon.rpc();
    let child = owner.spawn(
        "rec",
        &["record", daemon.paths.transcript.to_str().unwrap()],
    );
    let epoch = epoch_of(&child);
    let first = owner.claim("rec", epoch).expect("lease");
    let mut next = daemon.rpc();
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
    let mut owner = daemon.rpc();
    let child = owner.spawn("q", &["record", daemon.paths.transcript.to_str().unwrap()]);
    let epoch = epoch_of(&child);
    let lease = owner.claim("q", epoch).expect("lease");
    let queued = thread::spawn(move || owner.send("q", epoch, lease, "QUEUED"));
    wait_at(&daemon.paths.barrier, "before-first");
    let mut next = daemon.rpc();
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
    let mut rpc = daemon.rpc();
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
    let mut restarted_child = {
        let mut cmd = Command::new(host_bin());
        cmd.arg("--socket")
            .arg(endpoint.socket_path())
            .env("FL_HOST_WRITER_BARRIER", root.join("barrier"))
            .env("FL_HOST_SUBSCRIBER_HOLD", root.join("subscriber"))
            .env("FL_HOST_BACKLOG_STATUS", root.join("backlog"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        cmd.spawn().expect("restart")
    };
    let (restarted_stderr, restarted_stderr_thread) = capture_stderr(&mut restarted_child);
    let restarted = Daemon {
        child: restarted_child,
        paths: Paths {
            endpoint: endpoint.clone(),
            barrier: root.join("barrier"),
            subscriber: root.join("subscriber"),
            status: root.join("backlog"),
            transcript: root.join("stdin.bin"),
            root: daemon.paths.root.take(),
        },
        stderr: restarted_stderr,
        stderr_thread: restarted_stderr_thread,
    };
    // The first daemon is stopped and no longer owns the directory. Drop must
    // not delete the endpoint the replacement is serving.
    let mut rpc = restarted.rpc();
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
fn stdout_fragments_preserve_large_binary_stream_and_keep_child_alive() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let log = EventLog::subscribe(&daemon.paths.endpoint, Some("big"));
    let mut rpc = daemon.rpc();
    let done = daemon.paths.transcript.with_extension("done");
    rpc.spawn("big", &["stdout-fragments", "tail", done.to_str().unwrap()]);
    let mut expected = vec![b'z'; 513 * 4096];
    expected.extend_from_slice(b"\n\r\x1b[0m\0\xfftail");
    let reconstruct = |events: &[HostEvent]| {
        let mut bytes = Vec::new();
        for event in events {
            if let HostEvent::Line {
                bytes: raw,
                terminated,
                ..
            } = event
            {
                bytes.extend_from_slice(raw);
                if *terminated {
                    bytes.push(b'\n');
                }
            }
        }
        bytes
    };
    log.wait_until(Duration::from_secs(5), |events| {
        reconstruct(events).len() == expected.len()
    });
    let events = log.snapshot();
    assert_eq!(reconstruct(&events), expected);
    assert!(
        !controls(&events)
            .iter()
            .any(|e| matches!(e, ControlEvent::ProtocolFailure { .. }))
    );
    assert!(
        rpc.list()
            .iter()
            .any(|c| matches!(&c.state, ChildState::Running(r) if r.session.as_str()=="big"))
    );
}

#[test]
fn stdin_queue_refuses_over_capacity() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    arm(&daemon.paths.barrier, "before-first");
    let mut rpc = daemon.rpc();
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
    // Keep using the split API while earlier correlated sends are outstanding.
    let overflow_id = rpc.push(|id| HostRequest::Send {
        id,
        session: sid("cap"),
        expected_epoch: SessionEpoch(epoch),
        lease,
        line: "NO".into(),
    });
    match rpc.pull() {
        HostResponse::Sent { id, outcome } => {
            assert_eq!(id, overflow_id);
            assert_eq!(not_written(&outcome), &SendBlock::QueueFull);
        }
        other => panic!("overflow response: {other:?}"),
    }
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
    let overflow_id = rpc.push(|id| HostRequest::Send {
        id,
        session: sid("bytes"),
        expected_epoch: SessionEpoch(epoch),
        lease,
        line: overflow,
    });
    match rpc.pull() {
        HostResponse::Sent { id, outcome } => {
            assert_eq!(id, overflow_id);
            assert_eq!(not_written(&outcome), &SendBlock::QueueFull);
        }
        other => panic!("byte overflow response: {other:?}"),
    }
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
    let mut owner = daemon.rpc();
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
    let mut rpc = daemon.rpc();
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
    let mut rpc = daemon.rpc();
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
    let mut rpc = daemon.rpc();
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
    let mut rpc = daemon.rpc();
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
    let status = backlog_status::wait_record(&daemon.paths.status, 0, deadline)
        .unwrap_or_else(|err| panic!("{err}"));
    let events = status_field(&status, "events");
    let charge = status_field(&status, "charge");
    assert!(events <= 4096, "{status}");
    assert!(events >= 100, "{status}");
    assert!(charge <= 8 * 1024 * 1024, "{status}");

    rpc.spawn("bytes", &["flood-stdout"]);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut saw = false;
    while Instant::now() < deadline {
        let listed = rpc.list();
        if record(&find(&listed, "bytes").state).consumer_stalled {
            saw = true;
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(saw, "byte flood did not stall");
    let status = backlog_status::wait_record(&daemon.paths.status, 1, deadline)
        .unwrap_or_else(|err| panic!("{err}"));
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

mod backlog_status {
    use super::*;

    pub(super) fn wait_record(
        path: &Path,
        previous_records: usize,
        deadline: Instant,
    ) -> Result<String, String> {
        wait_with_poll(path, previous_records, deadline, || {
            thread::sleep(Duration::from_millis(20));
        })
    }

    fn wait_with_poll(
        path: &Path,
        previous_records: usize,
        deadline: Instant,
        mut poll: impl FnMut(),
    ) -> Result<String, String> {
        let mut last_status = String::new();
        let mut last_io_error = None;
        loop {
            match std::fs::read_to_string(path) {
                Ok(text) => last_status = text,
                Err(err) => last_io_error = Some(err.to_string()),
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "timed out waiting for status record {} in {}; last status: {last_status:?}; last I/O error: {last_io_error:?}",
                    previous_records + 1,
                    path.display(),
                ));
            }
            if let Some(row) = last_status
                .split_inclusive('\n')
                .filter(|row| row.ends_with('\n'))
                .nth(previous_records)
            {
                return Ok(row.to_owned());
            }
            poll();
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        const FIRST: &str = "events=200 charge=20000 raw=0\n";
        const SECOND: &str = "events=4 charge=2097408 raw=2097152\n";

        #[test]
        fn waits_through_missing_empty_partial_and_unterminated_status() {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("status");
            // Each pending read releases the next writer step. No scheduling
            // delay can skip the absent, empty, partial, or unterminated file.
            let writes = ["", "events=200 charge=", FIRST.trim_end(), FIRST];
            let mut step = 0;
            let row = wait_with_poll(&path, 0, Instant::now() + Duration::from_secs(5), || {
                std::fs::write(&path, writes[step]).unwrap();
                step += 1;
            })
            .unwrap();
            assert_eq!(row, FIRST);
            assert_eq!(step, writes.len());
        }

        #[test]
        fn waits_for_second_complete_record_instead_of_stale_first() {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("status");
            std::fs::write(&path, FIRST).unwrap();
            let writes = [
                format!("{FIRST}events=4 charge="),
                format!("{FIRST}{}", SECOND.trim_end()),
                format!("{FIRST}{SECOND}"),
            ];
            let mut step = 0;
            let row = wait_with_poll(&path, 1, Instant::now() + Duration::from_secs(5), || {
                std::fs::write(&path, &writes[step]).unwrap();
                step += 1;
            })
            .unwrap();
            assert_eq!(row, SECOND);
            assert_eq!(step, writes.len());
        }

        #[test]
        fn selects_next_record_without_waiting_for_numerical_limits() {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("status");
            let over_limit = "events=9999 charge=99999999 raw=99999999\n";
            std::fs::write(&path, format!("{FIRST}{over_limit}{SECOND}")).unwrap();
            let row = wait_with_poll(&path, 1, Instant::now() + Duration::from_secs(5), || {
                panic!("a complete new record must be returned immediately");
            })
            .unwrap();
            assert_eq!(row, over_limit);
        }

        #[test]
        fn deadline_reports_complete_and_partial_status() {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("status");
            let text = format!("{FIRST}events=4 charge=");
            std::fs::write(&path, &text).unwrap();
            let err = wait_with_poll(&path, 1, Instant::now(), || {
                panic!("must not poll beyond the supplied deadline");
            })
            .unwrap_err();
            assert!(err.contains(&format!("{text:?}")), "{err}");
        }

        #[test]
        fn deadline_reports_status_io_error() {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("missing-status");
            let io_error = std::fs::read_to_string(&path).unwrap_err().to_string();
            let err = wait_with_poll(&path, 0, Instant::now(), || {
                panic!("must not poll beyond the supplied deadline");
            })
            .unwrap_err();
            assert!(err.contains(&io_error), "{err}");
            assert!(err.contains("last status: \"\""), "{err}");
        }
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
fn healthy_subscriber_receives_single_buffered_line_burst() {
    healthy_subscribers_receive_buffered_line_burst(1);
}

#[test]
fn two_healthy_subscribers_receive_single_buffered_line_burst() {
    healthy_subscribers_receive_buffered_line_burst(2);
}

fn healthy_subscribers_receive_buffered_line_burst(subscriber_count: usize) {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let endpoint = &daemon.paths.endpoint;
    let subscribers: Vec<_> = (0..subscriber_count)
        .map(|_| EventLog::subscribe(endpoint, Some("burst")))
        .collect();
    let mut rpc = Rpc::connect(endpoint);
    let gate = daemon.paths.barrier.join("burst");
    let child = rpc.spawn("burst", &["gated-line-burst", gate.to_str().unwrap()]);
    for events in &subscribers {
        events.wait_until(Duration::from_secs(3), |events| {
            line_bytes(events, "burst").contains(&b"READY".to_vec())
        });
    }
    std::fs::write(gate, b"go").unwrap();
    for events in &subscribers {
        let received = events.wait_until(Duration::from_secs(3), |events| {
            let lines = line_bytes(events, "burst");
            lines.iter().any(|line| line == b"L255")
                || events
                    .iter()
                    .any(|e| matches!(e, HostEvent::Control(ControlEvent::SlowSubscriber { .. })))
        });
        assert!(
            !received
                .iter()
                .any(|e| matches!(e, HostEvent::Control(ControlEvent::SlowSubscriber { .. })))
        );
        let actual: Vec<_> = line_bytes(&received, "burst")
            .into_iter()
            .filter(|line| line.starts_with(b"L"))
            .collect();
        let expected: Vec<_> = (0..256).map(|i| format!("L{i:03}").into_bytes()).collect();
        assert_eq!(actual, expected);
    }
    // A subsequent echo proves every subscription remains attached.
    let epoch = epoch_of(&child);
    let lease = rpc.claim("burst", epoch).expect("lease");
    assert_eq!(
        rpc.send("burst", epoch, lease, "AFTER"),
        SendOutcome::Written
    );
    for events in &subscribers {
        let received = events.wait_until(Duration::from_secs(3), |events| {
            line_bytes(events, "burst").contains(&b"AFTER".to_vec())
        });
        assert!(
            !received
                .iter()
                .any(|e| matches!(e, HostEvent::Control(ControlEvent::SlowSubscriber { .. })))
        );
    }
}

#[test]
fn slow_subscriber_dropped_not_blocking() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let endpoint = &daemon.paths.endpoint;
    // The first subscriber's handshake and READY delivery establish its identity
    // and sent_any state before its next server write is deliberately held.
    let slow = EventLog::subscribe(endpoint, Some("out"));
    let mut rpc = Rpc::connect(endpoint);
    let first_gate = daemon.paths.barrier.join("first-line");
    let flood_gate = daemon.paths.barrier.join("flood-lines");
    let batches = daemon.paths.barrier.join("line-batches");
    std::fs::create_dir(&batches).unwrap();
    rpc.spawn(
        "out",
        &[
            "gated-lines",
            first_gate.to_str().unwrap(),
            flood_gate.to_str().unwrap(),
            batches.to_str().unwrap(),
        ],
    );
    slow.wait_until(Duration::from_secs(3), |events| {
        line_bytes(events, "out")
            .iter()
            .any(|line| line == b"READY")
    });
    let healthy = EventLog::subscribe(endpoint, Some("out"));
    wait_file(&daemon.paths.subscriber.join("subscribed-2"), |bytes| {
        bytes == b"1"
    });
    arm(&daemon.paths.subscriber, "before-send-1");
    std::fs::write(first_gate, b"go").unwrap();
    wait_at(&daemon.paths.subscriber, "before-send-1");
    healthy.wait_until(Duration::from_secs(3), |events| {
        line_bytes(events, "out").iter().any(|line| line == b"HOLD")
    });
    std::fs::write(flood_gate, b"go").unwrap();
    // Bound the healthy peer's pending batch below its queue cap. Its observed
    // progress admits the next batch, while the held peer accumulates all of them.
    for batch in 0..20 {
        std::fs::write(batches.join(batch.to_string()), b"go").unwrap();
        let last = format!("L{}", (batch + 1) * 10 - 1).into_bytes();
        healthy.wait_until(Duration::from_secs(3), |events| {
            line_bytes(events, "out").iter().any(|line| line == &last)
        });
    }
    let events = healthy.wait_until(Duration::from_secs(3), |events| {
        line_bytes(events, "out")
            .iter()
            .filter(|line| line.starts_with(b"L"))
            .count()
            == 200
    });
    // The child measures writes + flushes after each gate, excluding the twenty
    // harness/healthy-reader round trips. Each batch still has the progress
    // deadline above, so a fanout blocked by the held peer fails independently.
    let timing = wait_file(&batches.join("producer-ns"), |bytes| bytes.ends_with(b"\n"));
    let nanos = std::str::from_utf8(&timing)
        .expect("producer timing UTF-8")
        .trim()
        .parse::<u64>()
        .expect("producer timing nanoseconds");
    let producer_elapsed = Duration::from_nanos(nanos);
    assert!(
        producer_elapsed < Duration::from_secs(3),
        "producer writes and flushes took {producer_elapsed:?}"
    );
    let data: Vec<_> = line_bytes(&events, "out")
        .into_iter()
        .filter(|line| line.starts_with(b"L"))
        .collect();
    for (index, line) in data.iter().enumerate() {
        assert_eq!(line, &format!("L{index}").into_bytes());
    }
    let events = slow.wait_until(Duration::from_secs(3), |events| {
        events.iter().any(|event| {
            matches!(
                event,
                HostEvent::Control(ControlEvent::SlowSubscriber { .. })
            )
        })
    });
    assert!(
        line_bytes(&events, "out")
            .iter()
            .filter(|line| line.starts_with(b"L"))
            .count()
            < 200
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
    for id in 1..=16 {
        wait_file(
            &daemon.paths.subscriber.join(format!("subscribed-{id}")),
            |bytes| bytes == b"1",
        );
    }
    let mut rpc = Rpc::connect(&endpoint);
    let reason = failed_reason(
        rpc.call(|id| HostRequest::Events { id, session: None })
            .expect("response"),
    );
    assert!(reason.contains("subscriber"), "{reason}");
    drop(parked);

    arm(&daemon.paths.subscriber, "before-exited");
    let log = EventLog::subscribe(&endpoint, Some("slot"));
    wait_file(&daemon.paths.subscriber.join("subscribed-17"), |bytes| {
        bytes == b"1"
    });
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
    let mut rpc = daemon.rpc();
    rpc.spawn("stream", &["stall"]);
    let mut events = daemon.rpc();
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
    let mut owner = daemon.rpc();
    let path = daemon.paths.transcript.clone();
    let child = owner.spawn("part", &["record", path.to_str().unwrap()]);
    let epoch = epoch_of(&child);
    let lease = owner.claim("part", epoch).expect("lease");
    let send = thread::spawn(move || owner.send("part", epoch, lease, "HELLO-WORLD"));
    wait_at(&daemon.paths.barrier, "after-chunk");
    let _ = wait_file(&path, |bytes| bytes.starts_with(b"HELL"));
    let mut next = daemon.rpc();
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
    let mut owner = daemon.rpc();
    let eof = daemon.paths.barrier.join("record-eof");
    let child = owner.spawn(
        "fence",
        &[
            "record-survive-eof",
            daemon.paths.transcript.to_str().unwrap(),
            eof.to_str().unwrap(),
        ],
    );
    let epoch = epoch_of(&child);
    let lease = owner.claim("fence", epoch).expect("lease");
    let send = thread::spawn(move || owner.send("fence", epoch, lease, "HELLO-WORLD"));
    wait_at(&daemon.paths.barrier, "after-chunk");
    let mut next = daemon.rpc();
    let renewed = next.claim("fence", epoch).expect("lease");
    let _ = send.join();
    wait_file(&eof, |bytes| bytes == b"eof");
    assert_eq!(
        record(&find(&next.list(), "fence").state).input,
        InputState::Fenced
    );
    next.signal("fence", epoch, renewed, SignalKind::Terminate)
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
    let mut owner = daemon.rpc();
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
    let mut owner = daemon.rpc();
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
    let mut rpc = daemon.rpc();
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
    let mut rpc = daemon.rpc();
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
    let mut rpc = daemon.rpc();
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
    let mut owner = daemon.rpc();
    let path = daemon.paths.transcript.clone();
    let child = owner.spawn("kind", &["record", path.to_str().unwrap()]);
    let epoch = epoch_of(&child);
    let lease = owner.claim("kind", epoch).expect("lease");
    let mut other = daemon.rpc();
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
    let mut owner = daemon.rpc();
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
    let mut owner = daemon.rpc();
    let path = daemon.paths.transcript.clone();
    let child = owner.spawn("bound", &["record", path.to_str().unwrap()]);
    let epoch = epoch_of(&child);
    owner.spawn("other", &["stall"]);
    let lease = owner.claim("bound", epoch).expect("lease");
    let mut stranger = daemon.rpc();
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
    let mut rpc = daemon.rpc();
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
    let mut rpc = daemon.rpc();
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
    let mut rpc = daemon.rpc();
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
    rpc.signal("sig", epoch, lease, SignalKind::Terminate)
        .expect("signal");
}

#[test]
fn epoch_guard_serializes_signal_kill_and_respawn() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    arm(&daemon.paths.barrier, "before-signal");
    let mut rpc = daemon.rpc();
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
            signal: SignalKind::Terminate,
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
    let mut rpc = daemon.rpc();
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
    let mut rpc = daemon.rpc();
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
    let mut rpc = daemon.rpc();
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

/// No child is spawned and no output events are produced in this scenario.
#[cfg(windows)]
#[test]
fn disconnected_idle_subscribers_release_all_windows_slots() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let endpoint = &daemon.paths.endpoint;
    let mut subscribers = Vec::new();
    for _ in 0..fl_host_proto::MAX_SUBSCRIBERS {
        let mut rpc = Rpc::connect(endpoint);
        assert!(matches!(
            rpc.call(|id| HostRequest::Events { id, session: None })
                .unwrap(),
            HostResponse::Subscribed { .. }
        ));
        subscribers.push(rpc);
    }
    let mut probe = Rpc::connect(endpoint);
    assert!(matches!(
        probe
            .call(|id| HostRequest::Events { id, session: None })
            .unwrap(),
        HostResponse::Failed { .. }
    ));
    subscribers.clear();
    // Reclaim every slot, not only the first one. Refused probes stay RPC.
    let deadline = Instant::now() + Duration::from_secs(5);
    for _ in 0..fl_host_proto::MAX_SUBSCRIBERS {
        loop {
            match probe
                .call(|id| HostRequest::Events { id, session: None })
                .unwrap()
            {
                HostResponse::Subscribed { .. } => break,
                HostResponse::Failed { .. } if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(20))
                }
                other => panic!("idle disconnect failed to reclaim subscriber slot: {other:?}"),
            }
        }
        subscribers.push(probe);
        probe = Rpc::connect(endpoint);
    }
}

#[cfg(target_os = "linux")]
struct SignalFixturePin(std::os::fd::OwnedFd);

#[cfg(target_os = "linux")]
impl SignalFixturePin {
    fn new(pid: u32) -> Self {
        use std::os::fd::FromRawFd;
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as i32, 0) };
        assert!(fd >= 0, "pin fixture");
        Self(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd as i32) })
    }
    fn send(&self, signal: i32) {
        use std::os::fd::AsRawFd;
        unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.0.as_raw_fd(),
                signal,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            );
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for SignalFixturePin {
    fn drop(&mut self) {
        self.send(libc::SIGCONT);
        self.send(libc::SIGKILL);
    }
}

#[cfg(target_os = "linux")]
#[test]
fn signal_waits_for_sole_reaper_and_expired_command_cannot_signal() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let log = EventLog::subscribe(&daemon.paths.endpoint, Some("serialized"));
    let mut rpc = daemon.rpc();
    let child = rpc.spawn("serialized", &["stall"]);
    let epoch = epoch_of(&child);
    let lease = rpc.claim("serialized", epoch).expect("lease");
    log.wait_until(Duration::from_secs(3), |events| {
        line_bytes(events, "serialized")
            .iter()
            .any(|line| line == b"READY")
    });
    let registry = fl_host::read_registry(&daemon.paths.endpoint).expect("registry");
    let row = registry
        .sessions
        .iter()
        .find(|row| row.session == "serialized")
        .expect("row");
    let supervisor = SignalFixturePin::new(row.supervisor_pid);
    let vendor = SignalFixturePin::new(row.vendor_pid);
    supervisor.send(libc::SIGSTOP);
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let stat =
            std::fs::read_to_string(format!("/proc/{}/stat", row.supervisor_pid)).expect("stat");
        if stat
            .rsplit_once(')')
            .expect("stat fields")
            .1
            .trim_start()
            .starts_with('T')
        {
            break;
        }
        assert!(Instant::now() < deadline, "supervisor did not stop");
        thread::yield_now();
    }
    let started = Instant::now();
    let error = rpc
        .signal("serialized", epoch, lease, SignalKind::Kill)
        .expect_err("no acknowledgement while sole reaper is stopped");
    assert!(failed_reason(error).contains("signal outcome unknown"));
    assert!(started.elapsed() < Duration::from_secs(3));
    supervisor.send(libc::SIGCONT);
    // A subsequent RPC on the same FIFO proves the expired command was consumed.
    rpc.signal("serialized", epoch, lease, SignalKind::Hangup)
        .expect("next signal");
    log.wait_until(Duration::from_secs(3), |events| {
        controls(events).iter().any(|event| {
            matches!(
                event,
                ControlEvent::Exited {
                    exit: ChildExit {
                        signal: Some(libc::SIGHUP),
                        ..
                    },
                    ..
                }
            )
        })
    });
    let error = rpc
        .signal("serialized", epoch, lease, SignalKind::Kill)
        .expect_err("reaped leader cannot anchor a signal");
    assert!(failed_reason(error).contains("unanchored group"));
    drop(vendor);
}

#[cfg(windows)]
#[test]
fn signalled_windows_handle_preserves_exit_259_in_event_inventory_and_registry() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let log = EventLog::subscribe(&daemon.paths.endpoint, Some("exit259"));
    let mut rpc = daemon.rpc();
    let child = rpc.spawn("exit259", &["exit-with-survivor", "259"]);
    log.wait_until(Duration::from_secs(3), |events| {
        controls(events).iter().any(|event| {
            matches!(
                event,
                ControlEvent::Exited {
                    exit: ChildExit {
                        code: Some(259),
                        signal: None
                    },
                    ..
                }
            )
        })
    });
    let snapshot = log.snapshot();
    let exits: Vec<_> = controls(&snapshot)
        .into_iter()
        .filter(|event| matches!(event, ControlEvent::Exited { .. }))
        .collect();
    assert_eq!(exits.len(), 1);
    let children = rpc.list();
    assert!(
        matches!(&find(&children, "exit259").state, ChildState::LeaderExited(row) if row.exit.as_ref().is_some_and(|exit| exit.code == Some(259)))
    );
    let registry = fl_host::read_registry(&daemon.paths.endpoint).expect("registry");
    let row = registry
        .sessions
        .iter()
        .find(|row| row.session == "exit259")
        .expect("durable leader");
    assert_eq!(row.leader_code, Some(259));
    assert!(row.group_leader_reaped);
    rpc.kill("exit259", epoch_of(&child), None)
        .expect("cleanup");
}

#[cfg(windows)]
#[test]
fn unsupported_windows_signals_refuse_without_mutation() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let log = EventLog::subscribe(&daemon.paths.endpoint, Some("unsupported"));
    let mut rpc = daemon.rpc();
    let child = rpc.spawn("unsupported", &["echo"]);
    let epoch = epoch_of(&child);
    let lease = rpc.claim("unsupported", epoch).expect("lease");
    log.wait_until(Duration::from_secs(3), |events| {
        line_bytes(events, "unsupported")
            .iter()
            .any(|line| line == b"READY")
    });
    for signal in [SignalKind::Interrupt, SignalKind::Hangup] {
        let error = rpc
            .signal("unsupported", epoch, lease, signal)
            .expect_err("unsupported signal");
        assert!(failed_reason(error).contains("unsupported Windows signal"));
        assert!(matches!(
            find(&rpc.list(), "unsupported").state,
            ChildState::Running(_)
        ));
        let text = format!("survived-{signal:?}");
        assert_eq!(
            rpc.send("unsupported", epoch, lease, &text),
            SendOutcome::Written
        );
        log.wait_until(Duration::from_secs(3), |events| {
            line_bytes(events, "unsupported")
                .iter()
                .any(|line| line == text.as_bytes())
        });
    }
    rpc.signal("unsupported", epoch, lease, SignalKind::Kill)
        .expect("supported kill");
    log.wait_until(Duration::from_secs(3), |events| {
        controls(events)
            .iter()
            .any(|event| matches!(event, ControlEvent::Exited { .. }))
    });
}

#[cfg(target_os = "linux")]
#[test]
fn supervisor_disconnect_never_reports_signal_success() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let log = EventLog::subscribe(&daemon.paths.endpoint, Some("disconnect"));
    let mut rpc = daemon.rpc();
    let child = rpc.spawn("disconnect", &["stall"]);
    let epoch = epoch_of(&child);
    let lease = rpc.claim("disconnect", epoch).expect("lease");
    log.wait_until(Duration::from_secs(3), |events| {
        line_bytes(events, "disconnect")
            .iter()
            .any(|line| line == b"READY")
    });
    let registry = fl_host::read_registry(&daemon.paths.endpoint).expect("registry");
    let row = registry
        .sessions
        .iter()
        .find(|row| row.session == "disconnect")
        .expect("row");
    let vendor = SignalFixturePin::new(row.vendor_pid);
    let supervisor = SignalFixturePin::new(row.supervisor_pid);
    supervisor.send(libc::SIGKILL);
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if fl_host::read_registry(&daemon.paths.endpoint)
            .expect("registry")
            .alerts
            .iter()
            .any(|alert| alert == "cleanup unresolved")
        {
            break;
        }
        assert!(Instant::now() < deadline, "supervisor EOF was not observed");
        thread::sleep(Duration::from_millis(5));
    }
    let started = Instant::now();
    assert!(
        rpc.signal("disconnect", epoch, lease, SignalKind::Kill)
            .is_err()
    );
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(matches!(
        find(&rpc.list(), "disconnect").state,
        ChildState::Running(_)
    ));
    drop(vendor);
}

#[cfg(target_os = "linux")]
#[test]
fn signal_with_unreported_leader_exit_is_serialized_with_reaping() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let log = EventLog::subscribe(&daemon.paths.endpoint, Some("exit-race"));
    let mut rpc = daemon.rpc();
    let child = rpc.spawn("exit-race", &["stall"]);
    let epoch = epoch_of(&child);
    let lease = rpc.claim("exit-race", epoch).expect("lease");
    log.wait_until(Duration::from_secs(3), |events| {
        line_bytes(events, "exit-race")
            .iter()
            .any(|line| line == b"READY")
    });
    let registry = fl_host::read_registry(&daemon.paths.endpoint).expect("registry");
    let row = registry
        .sessions
        .iter()
        .find(|row| row.session == "exit-race")
        .expect("row");
    let supervisor = SignalFixturePin::new(row.supervisor_pid);
    let vendor = SignalFixturePin::new(row.vendor_pid);
    supervisor.send(libc::SIGSTOP);
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let stat =
            std::fs::read_to_string(format!("/proc/{}/stat", row.supervisor_pid)).expect("stat");
        if stat
            .rsplit_once(')')
            .expect("stat fields")
            .1
            .trim_start()
            .starts_with('T')
        {
            break;
        }
        assert!(Instant::now() < deadline, "supervisor did not stop");
        thread::yield_now();
    }
    vendor.send(libc::SIGTERM);
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let stat = std::fs::read_to_string(format!("/proc/{}/stat", row.vendor_pid))
            .expect("unreaped fixture");
        if stat
            .rsplit_once(')')
            .expect("stat fields")
            .1
            .trim_start()
            .starts_with('Z')
        {
            break;
        }
        assert!(Instant::now() < deadline, "leader did not exit");
        thread::yield_now();
    }
    assert!(
        matches!(find(&rpc.list(), "exit-race").state, ChildState::Running(_)),
        "leader exit must still be unreported"
    );
    let error = rpc
        .signal("exit-race", epoch, lease, SignalKind::Kill)
        .expect_err("sole reaper must decide");
    assert!(failed_reason(error).contains("signal outcome unknown"));
    supervisor.send(libc::SIGCONT);
    log.wait_until(Duration::from_secs(3), |events| {
        controls(events).iter().any(|event| {
            matches!(
                event,
                ControlEvent::Exited {
                    exit: ChildExit {
                        signal: Some(libc::SIGTERM),
                        ..
                    },
                    ..
                }
            )
        })
    });
    let error = rpc
        .signal("exit-race", epoch, lease, SignalKind::Kill)
        .expect_err("reaped anchor");
    assert!(failed_reason(error).contains("unanchored group"));
}

fn wait_complete(rpc: &mut Rpc, name: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if matches!(
            find(&rpc.list(), name).state,
            ChildState::CleanupComplete(_)
        ) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "cleanup never completed for {name}"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn unread_previous_epoch_survives_respawn() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let endpoint = &daemon.paths.endpoint;
    let mut rpc = Rpc::connect(endpoint);
    let marker = daemon.paths.barrier.join("lines-flushed");
    let old = epoch_of(&rpc.spawn("retained", &["lines", "3", marker.to_str().unwrap()]));
    wait_file(&marker, |bytes| bytes == b"flushed");
    arm(&daemon.paths.subscriber, "before-send");
    let held = EventLog::subscribe(endpoint, Some("retained"));
    wait_at(&daemon.paths.subscriber, "before-send");
    rpc.kill("retained", old, None).unwrap();
    wait_complete(&mut rpc, "retained");
    drop(held);
    // Old output is still unwritten when the same name is replaced.
    let new = epoch_of(&rpc.spawn("retained", &["stall"]));
    assert!(new > old);
    release(&daemon.paths.subscriber, "before-send");
    let log = EventLog::subscribe(endpoint, Some("retained"));
    let events = log.wait_until(Duration::from_secs(3), |events| {
        events.iter().any(|event| matches!(event, HostEvent::Control(ControlEvent::CleanupComplete {session_epoch, ..}) if session_epoch.0 == old))
        && events.iter().any(|event| matches!(event, HostEvent::Line {session_epoch, bytes, ..} if session_epoch.0 == new && bytes == b"READY"))
        && events.iter().any(|event| matches!(event, HostEvent::Line {session_epoch, bytes, ..} if session_epoch.0 == old && bytes == b"L2"))
    });
    assert!(events.iter().any(|event| matches!(event, HostEvent::Control(ControlEvent::Spawned {session_epoch, ..}) if session_epoch.0 == old)));
    assert!(events.iter().any(|event| matches!(event, HostEvent::Control(ControlEvent::Exited {session_epoch, ..}) if session_epoch.0 == old)));
    for expected in [b"READY".as_slice(), b"L0", b"L1", b"L2"] {
        assert!(events.iter().any(|event| matches!(event, HostEvent::Line {session_epoch, bytes, ..} if session_epoch.0 == old && bytes == expected)));
    }
}

#[test]
fn unread_epochs_refuse_admission_at_retained_capacity() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let mut rpc = daemon.rpc();
    for index in 0..fl_host_proto::DEFAULT_MAX_SESSIONS {
        let name = format!("retained-{index}");
        let epoch = epoch_of(&rpc.spawn(&name, &["stall"]));
        rpc.kill(&name, epoch, None).unwrap();
        wait_complete(&mut rpc, &name);
    }
    for index in 64..70 {
        let spec = spec(&format!("overflow-{index}"), &["stall"]);
        let response = rpc.call(|id| HostRequest::Spawn { id, spec }).unwrap();
        assert!(
            matches!(response, HostResponse::Failed { .. }),
            "unread output must consume admission capacity: {response:?}"
        );
    }
    #[cfg(target_os = "linux")]
    {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let threads = std::fs::read_dir(format!("/proc/{}/task", daemon.child.id()))
                .unwrap()
                .count();
            if threads <= 12 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "completed epochs retained {threads} daemon threads"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
    assert!(rpc.list().len() <= fl_host_proto::DEFAULT_MAX_SESSIONS);
    let log = EventLog::subscribe(&daemon.paths.endpoint, Some("retained-0"));
    log.wait_until(Duration::from_secs(3), |events| {
        controls(events)
            .iter()
            .any(|event| matches!(event, ControlEvent::CleanupComplete { .. }))
    });
}

// Reliable mode uses a distinct authenticated authority and explicit cumulative Ack.
use fl_host_proto::{
    AsyncHostClient, Cursor, ExtensionEventStream, ExtensionRefusal as Refuse,
    ExtensionRequest as XReq, ExtensionResponse as XResp, SessionMode as Mode, StreamId, Ticket,
};
fn extended(rpc: &mut Rpc, request: XReq) -> XResp {
    match rpc
        .call(|id| HostRequest::Extended { id, request })
        .expect("extension RPC")
    {
        HostResponse::Extended { response, .. } => response,
        other => panic!("extension response {other:?}"),
    }
}
fn reliable_spawn(rpc: &mut Rpc, name: &str, args: &[&str]) -> StreamId {
    match extended(
        rpc,
        XReq::SpawnSession {
            spec: spec(name, args),
            mode: Mode::ReliablePipe,
            pty: None,
        },
    ) {
        XResp::Session { stream, .. } => stream,
        other => panic!("reliable spawn {other:?}"),
    }
}
fn controller(rpc: &mut Rpc, stream: &StreamId) -> ControllerLease {
    match extended(
        rpc,
        XReq::ClaimController {
            stream: stream.clone(),
        },
    ) {
        XResp::Controller { lease, .. } => lease,
        other => panic!("controller {other:?}"),
    }
}
fn ticket(rpc: &mut Rpc, stream: &StreamId, lease: ControllerLease, after: u64) -> Ticket {
    match extended(
        rpc,
        XReq::SubscribeReliable {
            stream: stream.clone(),
            lease,
            after_seq: Some(after),
        },
    ) {
        XResp::Subscription { ticket, .. } => ticket,
        other => panic!("ticket {other:?}"),
    }
}
fn settled_ticket(rpc: &mut Rpc, stream: &StreamId, lease: ControllerLease, after: u64) -> Ticket {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match extended(
            rpc,
            XReq::SubscribeReliable {
                stream: stream.clone(),
                lease,
                after_seq: Some(after),
            },
        ) {
            XResp::Subscription { ticket, .. } => return ticket,
            XResp::Refused { code: Refuse::Busy } => {
                assert!(Instant::now() < deadline, "old authority did not settle");
                thread::sleep(Duration::from_millis(5));
            }
            other => panic!("ticket {other:?}"),
        }
    }
}
fn event_socket(
    endpoint: &HostEndpoint,
    ticket: Ticket,
) -> (tokio::runtime::Runtime, ExtensionEventStream) {
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let stream = rt.block_on(async {
        AsyncHostClient::connect(endpoint)
            .await
            .expect("event connect")
            .open_extension_stream(ticket)
            .await
            .expect("open")
    });
    (rt, stream)
}
fn event_seq(rt: &tokio::runtime::Runtime, stream: &mut ExtensionEventStream, after: u64) -> u64 {
    let mut reassembly = fl_host_proto::ReliableReassembler::new(after);
    rt.block_on(async {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Some(event) = reassembly
                    .push(&stream.next_frame().await.expect("frame"))
                    .expect("event")
                {
                    return event.seq;
                }
            }
        })
        .await
        .expect("event deadline")
    })
}
fn ack(rpc: &mut Rpc, stream: &StreamId, lease: ControllerLease, seq: u64) -> XResp {
    extended(
        rpc,
        XReq::Ack {
            cursor: Cursor {
                stream: stream.clone(),
                seq,
            },
            lease,
        },
    )
}
#[test]
fn second_authoritative_subscribe_is_busy() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let mut rpc = daemon.rpc();
    let stream = reliable_spawn(&mut rpc, "one-stream", &["echo"]);
    let lease = controller(&mut rpc, &stream);
    let t = ticket(&mut rpc, &stream, lease, 0);
    let (rt, mut events) = event_socket(&daemon.paths.endpoint, t);
    assert_eq!(
        extended(
            &mut rpc,
            XReq::SubscribeReliable {
                stream: stream.clone(),
                lease,
                after_seq: Some(u64::MAX)
            }
        ),
        XResp::Refused { code: Refuse::Busy }
    );
    assert_eq!(event_seq(&rt, &mut events, 0), 1);
    assert_eq!(
        ack(&mut rpc, &stream, lease, 1),
        XResp::Acked { through: 1 }
    );
    assert_eq!(
        extended(
            &mut rpc,
            XReq::SubscribeReliable {
                stream: stream.clone(),
                lease,
                after_seq: None
            }
        ),
        XResp::Refused { code: Refuse::Busy }
    );
}
#[test]
fn ticket_bound_to_live_rpc_lease_and_consumed_once() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let mut rpc = daemon.rpc();
    let stream = reliable_spawn(&mut rpc, "ticket", &["echo"]);
    let lease = controller(&mut rpc, &stream);
    let old = ticket(&mut rpc, &stream, lease, 0);
    let new = ticket(&mut rpc, &stream, lease, 0);
    let mut event = daemon.rpc();
    assert_eq!(
        extended(&mut event, XReq::OpenStream { ticket: old }),
        XResp::Refused {
            code: Refuse::TicketInvalid
        }
    );
    let (_rt, _events) = event_socket(&daemon.paths.endpoint, new.clone());
    assert_eq!(
        extended(&mut event, XReq::OpenStream { ticket: new }),
        XResp::Refused {
            code: Refuse::TicketInvalid
        }
    );
    let mut other = daemon.rpc();
    let lease2 = controller(&mut other, &stream);
    let pending = settled_ticket(&mut other, &stream, lease2, 0);
    drop(other);
    thread::sleep(Duration::from_millis(100));
    assert_eq!(
        extended(&mut event, XReq::OpenStream { ticket: pending }),
        XResp::Refused {
            code: Refuse::TicketInvalid
        }
    );
}
#[test]
fn ack_lost_reply_is_idempotent() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let mut rpc = daemon.rpc();
    let stream = reliable_spawn(&mut rpc, "ack-retry", &["echo"]);
    let lease = controller(&mut rpc, &stream);
    let t = ticket(&mut rpc, &stream, lease, 0);
    let (rt, mut events) = event_socket(&daemon.paths.endpoint, t);
    assert_eq!(event_seq(&rt, &mut events, 0), 1);
    arm(&daemon.paths.subscriber, "reliable-ack-reply");
    rpc.push(|id| HostRequest::Extended {
        id,
        request: XReq::Ack {
            cursor: Cursor {
                stream: stream.clone(),
                seq: 1,
            },
            lease,
        },
    });
    wait_at(&daemon.paths.subscriber, "reliable-ack-reply");
    let mut recovered = daemon.rpc();
    assert!(
        matches!(extended(&mut recovered,XReq::ListSessions),XResp::Sessions{records} if records[0].acked==1)
    );
    drop(rpc);
    drop(events);
    release(&daemon.paths.subscriber, "reliable-ack-reply");
    let lease = controller(&mut recovered, &stream);
    assert_eq!(
        ack(&mut recovered, &stream, lease, 1),
        XResp::Acked { through: 1 }
    );
    assert_eq!(
        ack(&mut recovered, &stream, lease, 1),
        XResp::Acked { through: 1 }
    );
    assert_eq!(
        ack(&mut recovered, &stream, lease, u64::MAX),
        XResp::Refused {
            code: Refuse::FutureCursor
        }
    );
}
#[test]
fn legacy_cannot_access_reliable_epoch() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let mut rpc = daemon.rpc();
    let stream = reliable_spawn(&mut rpc, "private-mode", &["echo"]);
    let lease = controller(&mut rpc, &stream);
    assert!(rpc.list().is_empty());
    assert!(rpc.claim("private-mode", stream.epoch.0).is_err());
    assert!(rpc.kill("private-mode", stream.epoch.0, None).is_err());
    assert!(
        rpc.signal("private-mode", stream.epoch.0, lease, SignalKind::Terminate)
            .is_err()
    );
    assert!(matches!(
        rpc.send("private-mode", stream.epoch.0, lease, "BAD"),
        SendOutcome::NotWritten { .. }
    ));
    assert!(
        matches!(extended(&mut rpc,XReq::ListSessions),XResp::Sessions{records} if records.len()==1 && records[0].mode==Mode::ReliablePipe)
    );
}
#[test]
fn observer_cannot_advance_ack_ceiling() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let mut rpc = daemon.rpc();
    let stream = reliable_spawn(&mut rpc, "observer", &["echo"]);
    let lease = controller(&mut rpc, &stream);
    let t = match extended(
        &mut rpc,
        XReq::Observe {
            stream: stream.clone(),
            after_seq: Some(0),
        },
    ) {
        XResp::Subscription { ticket, .. } => ticket,
        other => panic!("{other:?}"),
    };
    let (rt, mut events) = event_socket(&daemon.paths.endpoint, t);
    assert_eq!(event_seq(&rt, &mut events, 0), 1);
    assert_eq!(
        ack(&mut rpc, &stream, lease, 1),
        XResp::Refused {
            code: Refuse::FutureCursor
        }
    );
}
#[test]
fn takeover_revokes_old_stream_ceiling() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let mut old = daemon.rpc();
    let stream = reliable_spawn(&mut old, "takeover", &["echo"]);
    let lease = controller(&mut old, &stream);
    let t = ticket(&mut old, &stream, lease, 0);
    let (rt, mut events) = event_socket(&daemon.paths.endpoint, t);
    assert_eq!(event_seq(&rt, &mut events, 0), 1);
    let mut new = daemon.rpc();
    let fresh = controller(&mut new, &stream);
    assert_ne!(fresh, lease);
    assert_eq!(
        ack(&mut old, &stream, lease, 1),
        XResp::Refused {
            code: Refuse::StaleLease
        }
    );
    assert_eq!(
        ack(&mut new, &stream, fresh, 1),
        XResp::Refused {
            code: Refuse::FutureCursor
        }
    );
    let _ = settled_ticket(&mut new, &stream, fresh, 1);
    assert_eq!(
        ack(&mut new, &stream, fresh, 1),
        XResp::Acked { through: 1 }
    );
}
#[test]
fn historical_epoch_ack_survives_writer_retirement() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let mut rpc = daemon.rpc();
    let old = reliable_spawn(&mut rpc, "history", &["exit", "0"]);
    let cleanup_lease = controller(&mut rpc, &old);
    assert_eq!(
        extended(
            &mut rpc,
            XReq::KillSession {
                stream: old.clone(),
                lease: cleanup_lease
            }
        ),
        XResp::Controlled
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let XResp::Sessions { records } = extended(&mut rpc, XReq::ListSessions) else {
            panic!("list")
        };
        if records
            .iter()
            .any(|r| r.stream == old && matches!(r.state, ChildState::CleanupComplete(_)))
        {
            break;
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
    let new = reliable_spawn(&mut rpc, "history", &["echo"]);
    assert_ne!(old.epoch, new.epoch);
    let lease = controller(&mut rpc, &old);
    let t = ticket(&mut rpc, &old, lease, 0);
    let (rt, mut events) = event_socket(&daemon.paths.endpoint, t);
    assert_eq!(event_seq(&rt, &mut events, 0), 1);
    assert_eq!(ack(&mut rpc, &old, lease, 1), XResp::Acked { through: 1 });
    assert!(matches!(
        extended(
            &mut rpc,
            XReq::SendLine {
                stream: old,
                lease,
                line: "BAD".into()
            }
        ),
        XResp::Refused {
            code: Refuse::SessionFenced
        }
    ));
}

#[test]
fn stale_claim_continuation_cannot_publish_owner() {
    let daemon = Daemon::start(DaemonOpts { chunk: Some(1) });
    let mut owner = daemon.rpc();
    let eof = daemon.paths.barrier.join("reliable-eof");
    let stream = reliable_spawn(
        &mut owner,
        "stale-claim",
        &[
            "record-survive-eof",
            daemon.paths.transcript.to_str().unwrap(),
            eof.to_str().unwrap(),
        ],
    );
    let original = controller(&mut owner, &stream);
    arm(&daemon.paths.barrier, "after-chunk");
    owner.push(|id| HostRequest::Extended {
        id,
        request: XReq::SendLine {
            stream: stream.clone(),
            lease: original,
            line: "ACTUAL_WRITE".into(),
        },
    });
    wait_at(&daemon.paths.barrier, "after-chunk"); // first OS byte was written, completion is held.
    let _ = wait_file(&daemon.paths.transcript, |b| b.starts_with(b"A"));
    arm(&daemon.paths.barrier, "before-claim");
    let mut claimant = daemon.rpc();
    claimant.push(|id| HostRequest::Extended {
        id,
        request: XReq::ClaimController {
            stream: stream.clone(),
        },
    });
    wait_at(&daemon.paths.barrier, "before-claim");
    drop(claimant);
    let _ = wait_file(&daemon.paths.barrier.join("revoked"), |b| !b.is_empty());
    release(&daemon.paths.barrier, "before-claim");
    release(&daemon.paths.barrier, "after-chunk");
    assert!(matches!(
        owner.pull(),
        HostResponse::Extended {
            response: XResp::Input {
                outcome: SendOutcome::Ambiguous { .. }
            },
            ..
        }
    ));
    let lease = controller(&mut owner, &stream);
    assert_eq!(
        extended(
            &mut owner,
            XReq::SendLine {
                stream,
                lease,
                line: "NO_REOPEN".into()
            }
        ),
        XResp::Input {
            outcome: SendOutcome::NotWritten {
                reason: SendBlock::SessionFenced
            }
        }
    );
}
#[test]
fn partial_event_cannot_advance_emitted_ceiling() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let mut rpc = daemon.rpc();
    let stream = reliable_spawn(&mut rpc, "partial", &["echo"]);
    let lease = controller(&mut rpc, &stream);
    let t = ticket(&mut rpc, &stream, lease, 0);

    // Keep the event socket unopened so stdout reads can coalesce into one
    // unexposed record. An active replay could expose a short unterminated read.
    for line in ["x".repeat(60 * 1024), "sentinel".into()] {
        assert_eq!(
            extended(
                &mut rpc,
                XReq::SendLine {
                    stream: stream.clone(),
                    lease,
                    line
                }
            ),
            XResp::Input {
                outcome: SendOutcome::Written
            }
        );
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let XResp::Sessions { records } = extended(&mut rpc, XReq::ListSessions) else {
            panic!("list")
        };
        // Seq 1 is Spawned, seq 2 is READY. The sentinel at seq 4 proves
        // the newline ending the 60 KiB record at seq 3 has been consumed.
        if records.iter().any(|r| r.stream == stream && r.latest >= 4) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "large record and sentinel not retained for {stream:?}: {records:?}"
        );
        thread::sleep(Duration::from_millis(5));
    }
    arm(&daemon.paths.subscriber, "reliable-after-part");
    let (rt, mut events) = event_socket(&daemon.paths.endpoint, t);
    assert_eq!(event_seq(&rt, &mut events, 0), 1);
    assert_eq!(event_seq(&rt, &mut events, 1), 2);
    let frame = rt.block_on(async {
        tokio::time::timeout(Duration::from_secs(3), events.next_frame())
            .await
            .expect("first fragment deadline")
            .expect("first fragment")
    });
    assert!(
        matches!(
            &frame,
            fl_host_proto::ExtensionFrame::ReliablePart {
                terminated: true,
                seq: 3,
                more: true,
                ..
            }
        ),
        "expected first nonfinal part of terminated seq 3; actual frame: {frame:?}"
    );
    wait_at(&daemon.paths.subscriber, "reliable-after-part");
    assert_eq!(
        ack(&mut rpc, &stream, lease, 3),
        XResp::Refused {
            code: Refuse::FutureCursor
        }
    );
    drop(events);
    release(&daemon.paths.subscriber, "reliable-after-part");
}
#[test]
fn closed_stream_replacement_waits_for_authority_settlement() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let mut rpc = daemon.rpc();
    let stream = reliable_spawn(&mut rpc, "settlement", &["echo"]);
    let lease = controller(&mut rpc, &stream);
    let t = ticket(&mut rpc, &stream, lease, 0);
    let (rt, mut events) = event_socket(&daemon.paths.endpoint, t);
    assert_eq!(event_seq(&rt, &mut events, 0), 1);
    // The server completes seq 1 before it can emit seq 2. Receiving seq 1
    // alone can race peer-close against completion and legitimately lose credit.
    assert_eq!(event_seq(&rt, &mut events, 1), 2);
    arm(&daemon.paths.subscriber, "reliable-settle");
    drop(events);
    wait_at(&daemon.paths.subscriber, "reliable-settle");
    assert_eq!(
        ack(&mut rpc, &stream, lease, 1),
        XResp::Acked { through: 1 }
    );
    assert_eq!(
        extended(
            &mut rpc,
            XReq::SubscribeReliable {
                stream: stream.clone(),
                lease,
                after_seq: Some(1)
            }
        ),
        XResp::Refused { code: Refuse::Busy }
    );
    release(&daemon.paths.subscriber, "reliable-settle");
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match extended(
            &mut rpc,
            XReq::SubscribeReliable {
                stream: stream.clone(),
                lease,
                after_seq: Some(1),
            },
        ) {
            XResp::Subscription { ticket, .. } => {
                let (_rt, _events) = event_socket(&daemon.paths.endpoint, ticket);
                break;
            }
            XResp::Refused { code: Refuse::Busy } => {
                assert!(Instant::now() < deadline);
                thread::sleep(Duration::from_millis(10));
            }
            other => panic!("{other:?}"),
        }
    }
}
#[test]
fn cancelled_ticket_keeps_real_refs() {
    // Pending tickets own their cursors/slots until actual revocation; 16 is shared with legacy.
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let mut rpc = daemon.rpc();
    let stream = reliable_spawn(&mut rpc, "capacity", &["echo"]);
    let lease = controller(&mut rpc, &stream);
    let pending = ticket(&mut rpc, &stream, lease, 0);
    let mut observers = Vec::new();
    for _ in 0..15 {
        let mut observer = daemon.rpc();
        assert!(matches!(
            extended(
                &mut observer,
                XReq::Observe {
                    stream: stream.clone(),
                    after_seq: Some(0)
                }
            ),
            XResp::Subscription { .. }
        ));
        observers.push(observer);
    }
    let mut legacy = daemon.rpc();
    assert!(matches!(
        legacy.call(|id| HostRequest::Events { id, session: None }),
        Ok(HostResponse::Failed { .. })
    ));
    arm(&daemon.paths.subscriber, "reliable-before-part");
    arm(&daemon.paths.subscriber, "reliable-settle");
    let (_rt, events) = event_socket(&daemon.paths.endpoint, pending);
    wait_at(&daemon.paths.subscriber, "reliable-before-part");
    drop(events);
    wait_at(&daemon.paths.subscriber, "reliable-settle");
    assert_eq!(
        extended(
            &mut rpc,
            XReq::SubscribeReliable {
                stream: stream.clone(),
                lease,
                after_seq: Some(0)
            }
        ),
        XResp::Refused { code: Refuse::Busy }
    );
    assert_eq!(
        extended(
            &mut rpc,
            XReq::Observe {
                stream: stream.clone(),
                after_seq: Some(0)
            }
        ),
        XResp::Refused {
            code: Refuse::ResourceLimit
        }
    );
    release(&daemon.paths.subscriber, "reliable-before-part");
    release(&daemon.paths.subscriber, "reliable-settle");
    drop(observers);
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match extended(
            &mut legacy,
            XReq::Observe {
                stream: stream.clone(),
                after_seq: Some(0),
            },
        ) {
            XResp::Subscription { .. } => break,
            XResp::Refused {
                code: Refuse::ResourceLimit,
            } => {
                assert!(Instant::now() < deadline);
                thread::sleep(Duration::from_millis(10));
            }
            other => panic!("{other:?}"),
        }
    }
}
#[test]
fn reliable_client_yields_complete_events_without_automatic_ack() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let mut rpc = daemon.rpc();
    let stream = reliable_spawn(&mut rpc, "typed-client", &["echo"]);
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let mut client=AsyncHostClient::connect(&daemon.paths.endpoint).await.unwrap();
        let grant=client.claim_controller(stream.clone()).await.unwrap();
        let mut events=client.subscribe_reliable(stream.clone(),grant.lease,Some(0)).await.unwrap();
        let first=events.next_event().await.unwrap();assert_eq!(first.seq,1);
        assert_eq!(events.cursor().seq,1);
        assert!(matches!(client.extension_request(XReq::ListSessions).await.unwrap(),XResp::Sessions{records} if records[0].acked==0));
        assert_eq!(client.ack(events.cursor(),grant.lease).await.unwrap(),1);
    });
}

#[test]
fn reliable_scope_recovery_and_expiry_refusals() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let mut rpc = daemon.rpc();
    let stream = reliable_spawn(&mut rpc, "scope", &["echo"]);
    let lease = controller(&mut rpc, &stream);
    let mut other = daemon.rpc();
    assert_eq!(
        ack(&mut other, &stream, lease, 0),
        XResp::Refused {
            code: Refuse::WrongConnection
        }
    );
    assert_eq!(
        ack(&mut rpc, &stream, ControllerLease(lease.0 + 1), 0),
        XResp::Refused {
            code: Refuse::StaleLease
        }
    );
    let mut wrong = stream.clone();
    wrong.host = fl_host_proto::HostInstance::new([0; 16]);
    assert_eq!(
        ack(&mut rpc, &wrong, lease, 0),
        XResp::Refused {
            code: Refuse::WrongInstance
        }
    );
    wrong = stream.clone();
    wrong.epoch.0 += 1;
    assert_eq!(
        ack(&mut rpc, &wrong, lease, 0),
        XResp::Refused {
            code: Refuse::StaleEpoch
        }
    );
    assert_eq!(
        extended(
            &mut rpc,
            XReq::SubscribeReliable {
                stream: stream.clone(),
                lease,
                after_seq: Some(u64::MAX)
            }
        ),
        XResp::Refused {
            code: Refuse::FutureCursor
        }
    );
    let expiring = ticket(&mut rpc, &stream, lease, 0);
    thread::sleep(Duration::from_millis(5050));
    assert_eq!(
        extended(&mut other, XReq::OpenStream { ticket: expiring }),
        XResp::Refused {
            code: Refuse::TicketInvalid
        }
    );
    // A validated durable cursor may exceed host Ack; no stream emission is required for that baseline.
    let _pending = ticket(&mut rpc, &stream, lease, 1);
    assert_eq!(
        ack(&mut rpc, &stream, lease, 1),
        XResp::Acked { through: 1 }
    );
    assert_eq!(
        extended(
            &mut rpc,
            XReq::SubscribeReliable {
                stream: stream.clone(),
                lease,
                after_seq: None
            }
        ),
        XResp::Refused {
            code: Refuse::CursorExpired
        }
    );
    assert_eq!(
        extended(
            &mut rpc,
            XReq::SubscribeReliable {
                stream: stream.clone(),
                lease,
                after_seq: Some(0)
            }
        ),
        XResp::Refused {
            code: Refuse::CursorExpired
        }
    );
    assert_eq!(
        extended(
            &mut other,
            XReq::Observe {
                stream,
                after_seq: Some(0)
            }
        ),
        XResp::Refused {
            code: Refuse::CursorExpired
        }
    );
}

#[test]
fn reliable_minor_zero_has_no_extension_fallback() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let mut rpc = daemon.rpc();
    let _stream = reliable_spawn(&mut rpc, "minor", &["echo"]);
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let mut old=AsyncHostClient::connect_with(&daemon.paths.endpoint,fl_host_proto::ConnectOptions{minor:0,..Default::default()}).await.unwrap();
        assert_eq!(old.negotiated().1,0);
        assert!(old.extension_request(XReq::Capabilities).await.is_err());
        assert!(matches!(old.request(HostRequest::List{id:RequestId(1)}).await.unwrap(),HostResponse::Listed{children,..} if children.is_empty()));
    });
}

#[test]
fn reliable_raw_minor_zero_request_is_rejected_before_spawn() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let mut rpc = daemon.rpc();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let old = AsyncHostClient::connect_with(
            &daemon.paths.endpoint,
            fl_host_proto::ConnectOptions {
                minor: 0,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        #[cfg(unix)]
        let mut raw = old.into_stream();
        #[cfg(windows)]
        let mut raw = old.into_pipe();
        let frame = fl_host_proto::encode_request_for_minor(
            1,
            &HostRequest::Extended {
                id: RequestId(1),
                request: XReq::SpawnSession {
                    spec: spec("forged-minor", &["echo"]),
                    mode: Mode::ReliablePipe,
                    pty: None,
                },
            },
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        fl_host_proto::write_frame_async(&mut raw, &frame, deadline)
            .await
            .unwrap();
        assert!(
            fl_host_proto::read_frame_async(&mut raw, deadline, false)
                .await
                .is_err()
        );
    });
    assert!(
        matches!(extended(&mut rpc,XReq::ListSessions),XResp::Sessions{records} if records.is_empty())
    );
}

#[test]
fn reliable_observer_lag_is_explicit_after_controller_retirement() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let mut rpc = daemon.rpc();
    let stream = reliable_spawn(&mut rpc, "lag", &["echo"]);
    let lease = controller(&mut rpc, &stream);
    arm(&daemon.paths.subscriber, "reliable-before-part");
    let t = match extended(
        &mut rpc,
        XReq::Observe {
            stream: stream.clone(),
            after_seq: Some(0),
        },
    ) {
        XResp::Subscription { ticket, .. } => ticket,
        other => panic!("{other:?}"),
    };
    let (rt, mut events) = event_socket(&daemon.paths.endpoint, t);
    wait_at(&daemon.paths.subscriber, "reliable-before-part");
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let XResp::Sessions { records } = extended(&mut rpc, XReq::ListSessions) else {
            panic!("list")
        };
        if records[0].latest >= 2 {
            break;
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    let _pending = ticket(&mut rpc, &stream, lease, 2);
    assert_eq!(
        ack(&mut rpc, &stream, lease, 2),
        XResp::Acked { through: 2 }
    );
    release(&daemon.paths.subscriber, "reliable-before-part");
    assert_eq!(event_seq(&rt, &mut events, 0), 1);
    assert_eq!(
        rt.block_on(events.next_frame()).unwrap(),
        fl_host_proto::ExtensionFrame::Notice {
            code: Refuse::CursorExpired
        }
    );
}

#[test]
fn reliable_pending_operations_and_responses_have_finite_admission() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let mut rpc = daemon.rpc();
    arm(&daemon.paths.subscriber, "reliable-operation");
    for _ in 0..64 {
        rpc.push(|id| HostRequest::Extended {
            id,
            request: XReq::Capabilities,
        });
    }
    wait_at(&daemon.paths.subscriber, "reliable-operation");
    let marker = daemon.paths.barrier.join("must-not-spawn");
    let extra = rpc.push(|id| HostRequest::Extended {
        id,
        request: XReq::SpawnSession {
            spec: spec("over-cap", &["marker", marker.to_str().unwrap()]),
            mode: Mode::ReliablePipe,
            pty: None,
        },
    });
    assert!(
        matches!(rpc.pull(),HostResponse::Extended{id,response:XResp::Refused{code:Refuse::Busy}} if id==extra)
    );
    assert!(!marker.exists());
    arm(&daemon.paths.subscriber, "reliable-response-write");
    release(&daemon.paths.subscriber, "reliable-operation");
    wait_at(&daemon.paths.subscriber, "reliable-response-write");
    let count = wait_file(
        &daemon.paths.subscriber.join("reliable-pending-count"),
        |b| b == b"64",
    );
    assert_eq!(count, b"64"); // completed workers and queued/in-flight replies still own all permits.
    release(&daemon.paths.subscriber, "reliable-response-write");
    for _ in 0..64 {
        assert!(matches!(
            rpc.pull(),
            HostResponse::Extended {
                response: XResp::Capabilities { .. },
                ..
            }
        ));
    }
    assert!(matches!(
        extended(&mut rpc, XReq::Capabilities),
        XResp::Capabilities { .. }
    ));
}

#[test]
fn rpc_stream_legacy_conversion_revokes_pending_reliable_ticket() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let mut owner = daemon.rpc();
    let stream = reliable_spawn(&mut owner, "legacy-conversion", &["echo"]);
    let lease = controller(&mut owner, &stream);
    let pending = ticket(&mut owner, &stream, lease, 0);
    assert!(matches!(
        owner
            .call(|id| HostRequest::Events { id, session: None })
            .unwrap(),
        HostResponse::Subscribed { .. }
    ));
    let mut event = daemon.rpc();
    assert_eq!(
        extended(&mut event, XReq::OpenStream { ticket: pending }),
        XResp::Refused {
            code: Refuse::TicketInvalid
        }
    );
    let current = controller(&mut event, &stream);
    assert_eq!(
        extended(
            &mut event,
            XReq::SendLine {
                stream,
                lease: current,
                line: "still-running".into()
            }
        ),
        XResp::Input {
            outcome: SendOutcome::Written
        }
    );
}

#[test]
fn rpc_stream_extension_conversion_revokes_previous_rpc_ownership() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let mut owner = daemon.rpc();
    let first = reliable_spawn(&mut owner, "first-controller", &["echo"]);
    let lease = controller(&mut owner, &first);
    let first_ticket = ticket(&mut owner, &first, lease, 0);
    let mut converting = daemon.rpc();
    let second = reliable_spawn(&mut converting, "second-controller", &["echo"]);
    let second_lease = controller(&mut converting, &second);
    let second_ticket = ticket(&mut converting, &second, second_lease, 0);
    assert_eq!(
        extended(
            &mut converting,
            XReq::OpenStream {
                ticket: first_ticket
            }
        ),
        XResp::Opened
    );
    let mut event = daemon.rpc();
    assert_eq!(
        extended(
            &mut event,
            XReq::OpenStream {
                ticket: second_ticket
            }
        ),
        XResp::Refused {
            code: Refuse::TicketInvalid
        }
    );
    // Converting the event connection must not revoke the separate first RPC.
    assert_eq!(
        extended(
            &mut owner,
            XReq::SendLine {
                stream: first,
                lease,
                line: "separate-owner".into()
            }
        ),
        XResp::Input {
            outcome: SendOutcome::Written
        }
    );
}

#[test]
fn rpc_stream_own_ticket_requires_separate_live_rpc() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let mut owner = daemon.rpc();
    let stream = reliable_spawn(&mut owner, "own-ticket", &["echo"]);
    let lease = controller(&mut owner, &stream);
    let pending = ticket(&mut owner, &stream, lease, 0);
    assert_eq!(
        extended(
            &mut owner,
            XReq::OpenStream {
                ticket: pending.clone()
            }
        ),
        XResp::Refused {
            code: Refuse::TicketInvalid
        }
    );
    let (rt, mut events) = event_socket(&daemon.paths.endpoint, pending);
    assert_eq!(event_seq(&rt, &mut events, 0), 1);
}

#[test]
fn rpc_stream_conversion_refuses_outstanding_claim_response() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let mut owner = daemon.rpc();
    let stream = reliable_spawn(&mut owner, "pending-conversion", &["echo"]);
    let mut separate = daemon.rpc();
    let other = reliable_spawn(&mut separate, "pending-other", &["echo"]);
    let other_lease = controller(&mut separate, &other);
    let pending_ticket = ticket(&mut separate, &other, other_lease, 0);
    arm(&daemon.paths.subscriber, "reliable-operation");
    let claim = owner.push(|id| HostRequest::Extended {
        id,
        request: XReq::ClaimController {
            stream: stream.clone(),
        },
    });
    wait_at(&daemon.paths.subscriber, "reliable-operation");
    let events = owner.push(|id| HostRequest::Events { id, session: None });
    assert!(
        matches!(owner.pull(), HostResponse::Failed { id, reason } if id == events && reason == "busy")
    );
    let open = owner.push(|id| HostRequest::Extended {
        id,
        request: XReq::OpenStream {
            ticket: pending_ticket.clone(),
        },
    });
    assert!(
        matches!(owner.pull(), HostResponse::Extended { id, response: XResp::Refused { code: Refuse::Busy } } if id == open)
    );
    release(&daemon.paths.subscriber, "reliable-operation");
    assert!(
        matches!(owner.pull(), HostResponse::Extended { id, response: XResp::Controller { .. } } if id == claim)
    );
    assert!(matches!(
        owner
            .call(|id| HostRequest::Events { id, session: None })
            .unwrap(),
        HostResponse::Subscribed { .. }
    ));
    // Busy did not consume the separate owner's ticket.
    let (rt, mut events) = event_socket(&daemon.paths.endpoint, pending_ticket);
    assert_eq!(event_seq(&rt, &mut events, 0), 1);
}

#[test]
fn pre_enqueue_stale_claim_cannot_change_current_writer() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let mut older = daemon.rpc();
    let eof = daemon.paths.barrier.join("record-eof");
    let stream = reliable_spawn(
        &mut older,
        "claim-order",
        &[
            "record-survive-eof",
            daemon.paths.transcript.to_str().unwrap(),
            eof.to_str().unwrap(),
        ],
    );
    arm(&daemon.paths.subscriber, "reliable-pre-enqueue");
    older.push(|id| HostRequest::Extended {
        id,
        request: XReq::ClaimController {
            stream: stream.clone(),
        },
    });
    wait_at(&daemon.paths.subscriber, "reliable-pre-enqueue");
    let mut newer = daemon.rpc();
    let lease = controller(&mut newer, &stream);
    assert_eq!(
        extended(
            &mut newer,
            XReq::SendLine {
                stream: stream.clone(),
                lease,
                line: "BEFORE".into()
            }
        ),
        XResp::Input {
            outcome: SendOutcome::Written
        }
    );
    let _ = wait_file(&daemon.paths.transcript, |b| b == b"BEFORE\n");
    release(&daemon.paths.subscriber, "reliable-pre-enqueue");
    assert!(matches!(
        older.pull(),
        HostResponse::Extended {
            response: XResp::Refused {
                code: Refuse::StaleLease
            },
            ..
        }
    ));
    assert_eq!(
        extended(
            &mut newer,
            XReq::SendLine {
                stream: stream.clone(),
                lease,
                line: "AFTER".into()
            }
        ),
        XResp::Input {
            outcome: SendOutcome::Written
        }
    );
    let _ = wait_file(&daemon.paths.transcript, |b| b == b"BEFORE\nAFTER\n");
    assert!(matches!(
        extended(
            &mut older,
            XReq::SendLine {
                stream,
                lease,
                line: "STALE".into()
            }
        ),
        XResp::Refused { .. }
    ));
}

#[test]
fn legacy_lookup_replacement_cannot_select_reliable_writer() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let mut legacy = daemon.rpc();
    let old = legacy.spawn("lookup-race", &["exit", "0"]);
    let old_epoch = epoch_of(&old);
    legacy
        .kill("lookup-race", old_epoch, None)
        .expect("old cleanup");
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if matches!(
            find(&legacy.list(), "lookup-race").state,
            ChildState::CleanupComplete(_)
        ) {
            break;
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    arm(&daemon.paths.subscriber, "legacy-writer-lookup");
    let kill = legacy.push(|id| HostRequest::Kill {
        id,
        session: sid("lookup-race"),
        expected_epoch: SessionEpoch(old_epoch + 1),
        lease: None,
    });
    wait_at(&daemon.paths.subscriber, "legacy-writer-lookup");
    let mut owner = daemon.rpc();
    let stream = reliable_spawn(&mut owner, "lookup-race", &["echo"]);
    assert_eq!(stream.epoch.0, old_epoch + 1);
    let lease = controller(&mut owner, &stream);
    release(&daemon.paths.subscriber, "legacy-writer-lookup");
    let reply = legacy.pull();
    assert!(
        matches!(reply, HostResponse::Failed { id, .. } if id == kill),
        "legacy unleased Kill reached reliable successor: {reply:?}"
    );
    assert_eq!(
        extended(
            &mut owner,
            XReq::SendLine {
                stream: stream.clone(),
                lease,
                line: "SURVIVES".into()
            }
        ),
        XResp::Input {
            outcome: SendOutcome::Written
        }
    );
    let XResp::Sessions { records } = extended(&mut owner, XReq::ListSessions) else {
        panic!("list");
    };
    assert!(
        records
            .iter()
            .any(|r| r.stream == stream && matches!(r.state, ChildState::Running(_)))
    );
}

#[cfg(unix)]
#[test]
fn pty_reliable_session_supports_capture_output_and_write_raw() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let mut client = daemon.rpc().inner;
    let mut env = std::collections::BTreeMap::new();
    env.insert("FL_HOST_PTY".into(), "1".into());
    let mut env_allow = std::collections::BTreeSet::new();
    env_allow.insert("FL_HOST_PTY".into());
    let spawn_spec = SpawnSpec {
        session: sid("pty-prompt"),
        kind: SpawnKind::Pipe,
        argv: vec![
            "/bin/sh".into(),
            "-c".into(),
            "test -t 0 && printf \"PROMPT>\" && read ans && echo \"GOT:$ans\"".into(),
        ],
        cwd: std::env::temp_dir(),
        env,
        env_allow,
    };
    let (stream, _state) = client
        .spawn_reliable_session(spawn_spec)
        .expect("spawn reliable pty");
    let grant = client.claim_controller(stream.clone()).expect("claim");
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let out = client
            .capture_output(sid("pty-prompt"), 4096)
            .expect("capture");
        if out.contains("PROMPT>") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "did not observe PROMPT>: {out:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
    let outcome = client
        .write_raw(stream, grant.lease, b"yes\n".to_vec())
        .expect("write_raw");
    assert_eq!(outcome, SendOutcome::Written);
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let out = client
            .capture_output(sid("pty-prompt"), 4096)
            .expect("capture after write_raw");
        if out.contains("GOT:yes") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "did not observe GOT:yes: {out:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn non_reliable_eof_preserves_exact_unterminated_tail() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let log = EventLog::subscribe(&daemon.paths.endpoint, Some("eof-tail"));
    let mut rpc = daemon.rpc();
    let done = daemon.paths.transcript.with_extension("done");
    rpc.spawn(
        "eof-tail",
        &["stdout-fragments", "tail", done.to_str().unwrap(), "eof"],
    );
    let mut expected = vec![b'z'; 513 * 4096];
    expected.extend_from_slice(b"\n\r\x1b[0m\0\xfftail");
    let reconstruct = |events: &[HostEvent]| {
        let mut bytes = Vec::new();
        for event in events {
            if let HostEvent::Line {
                bytes: raw,
                terminated,
                ..
            } = event
            {
                bytes.extend_from_slice(raw);
                if *terminated {
                    bytes.push(b'\n');
                }
            }
        }
        bytes
    };
    let events = log.wait_until(Duration::from_secs(5), |events| {
        reconstruct(events).len() >= expected.len()
    });
    assert_eq!(reconstruct(&events), expected);
    assert!(
        !controls(&events)
            .iter()
            .any(|e| matches!(e, ControlEvent::ProtocolFailure { .. }))
    );
}

#[test]
fn host_build_identity_is_opt_in() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let mut rpc = daemon.rpc();
    // A strict old reader only knows these variants/fields. Advertising a bit is
    // compatible; adding build fields to either old response would fail here.
    #[derive(serde::Deserialize, serde::Serialize)]
    #[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
    enum LegacyResponse {
        Capabilities {
            host: fl_host_proto::HostInstance,
            features: u64,
            limits: fl_host_proto::ExtensionLimits,
        },
        Sessions {
            records: Vec<fl_host_proto::SessionSummary>,
        },
    }
    let caps = extended(&mut rpc, XReq::Capabilities);
    let value = serde_json::to_value(&caps).unwrap();
    let legacy: LegacyResponse = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(serde_json::to_value(legacy).unwrap(), value);
    let sessions = serde_json::to_value(extended(&mut rpc, XReq::ListSessions)).unwrap();
    let legacy: LegacyResponse = serde_json::from_value(sessions.clone()).unwrap();
    assert_eq!(serde_json::to_value(legacy).unwrap(), sessions);
    let XResp::Capabilities { features, .. } = caps else {
        panic!("capabilities")
    };
    assert_ne!(
        features & fl_host_proto::HOST_STATUS_FEATURE,
        0,
        "host must advertise opt-in build identity"
    );
    let expected = fl_host_proto::RELIABLE_FEATURE
        | fl_host_proto::HOST_STATUS_FEATURE
        | fl_host_proto::SESSION_OWNER_FEATURE
        | fl_host_proto::PTY_RETENTION_FEATURE;
    #[cfg(unix)]
    let expected = expected | fl_host_proto::PTY_FEATURE | fl_host_proto::RENDERED_PTY_FEATURE;
    assert_eq!(features, expected);
    let request = serde_json::from_str(r#"{"op":"host_status"}"#).unwrap();
    let status = serde_json::to_value(extended(&mut rpc, request)).unwrap();
    assert_eq!(status["op"], "host_status");
    assert_eq!(
        status["build"],
        serde_json::to_value(fl_host::build_info()).unwrap()
    );
    assert_eq!(
        rpc.inner.host_status().unwrap(),
        Some(fl_host::build_info())
    );
}

#[test]
fn pipe_modes_report_current_device_truth() {
    use fl_host_proto::SessionMode;
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let mut client = daemon.rpc().inner;
    for mode in [SessionMode::LegacyPipe, SessionMode::ReliablePipe] {
        for env_pty in [false, true] {
            let name = format!("device-{mode:?}-{env_pty}");
            let mut spec = spec(&name, &["report"]);
            if env_pty {
                spec.env.insert("FL_HOST_PTY".into(), "1".into());
                spec.env_allow.insert("FL_HOST_PTY".into());
            }
            let response = client
                .extension_request(XReq::SpawnSession {
                    spec,
                    mode,
                    pty: None,
                })
                .unwrap();
            let XResp::Session { stream, state } = response else {
                panic!("spawn: {response:?}")
            };
            assert_eq!(record(&state).kind, SpawnKind::Pipe);
            let deadline = Instant::now() + Duration::from_secs(5);
            let report = loop {
                let output = client.capture_output(stream.session.clone(), 8192).unwrap();
                if let Some(report) = output
                    .lines()
                    .find_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                {
                    break report;
                }
                assert!(
                    Instant::now() < deadline,
                    "missing terminal report: {output:?}"
                );
                thread::sleep(Duration::from_millis(10));
            };
            // Record 183 characterizes the mismatch: the Unix env opt-in uses a real
            // PTY while reporting Pipe. Explicit PTY sessions below report their device truth.
            assert_eq!(
                report["isatty"],
                cfg!(unix) && env_pty,
                "{mode:?}, env PTY={env_pty}"
            );
            let summary = client
                .list_sessions()
                .unwrap()
                .into_iter()
                .find(|s| s.stream == stream)
                .unwrap();
            assert_eq!(summary.mode, mode);
            assert_eq!(record(&summary.state).kind, SpawnKind::Pipe);
        }
    }
    let mut pty = spec("first-class-pty", &["report"]);
    pty.kind = SpawnKind::Pty;
    let response = client
        .extension_request(XReq::SpawnSession {
            spec: pty,
            mode: SessionMode::Pty,
            pty: None,
        })
        .unwrap();
    #[cfg(unix)]
    {
        let XResp::Session { stream, state } = response else {
            panic!("PTY spawn: {response:?}")
        };
        assert_eq!(record(&state).kind, SpawnKind::Pty);
        let deadline = Instant::now() + Duration::from_secs(5);
        let report = loop {
            let output = client.capture_output(stream.session.clone(), 8192).unwrap();
            if let Some(report) = output
                .lines()
                .find_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            {
                break report;
            }
            assert!(Instant::now() < deadline, "missing PTY report: {output:?}");
            thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(report["isatty"], true);
        let summary = client
            .list_sessions()
            .unwrap()
            .into_iter()
            .find(|s| s.stream == stream)
            .unwrap();
        assert_eq!(summary.mode, SessionMode::Pty);
        assert_eq!(record(&summary.state).kind, SpawnKind::Pty);
    }
    #[cfg(not(unix))]
    assert_eq!(
        response,
        XResp::Refused {
            code: Refuse::Unsupported
        }
    );
    let mut pty = spec("kind-pty", &["report"]);
    pty.kind = SpawnKind::Pty;
    assert!(
        client
            .extension_request(XReq::SpawnSession {
                spec: pty,
                mode: SessionMode::LegacyPipe,
                pty: None,
            })
            .is_err()
    );
}

#[path = "support/flood.rs"]
mod safety_flood;

fn safety_flood_bounds(daemon: &Daemon) {
    let status = backlog_status::wait_record(
        &daemon.paths.status,
        0,
        Instant::now() + Duration::from_secs(5),
    )
    .unwrap();
    assert!(status_field(&status, "events") <= 4096, "{status}");
    assert!(
        status_field(&status, "charge") <= 8 * 1024 * 1024,
        "{status}"
    );
    assert!(status_field(&status, "raw") > 7 * 1024 * 1024, "{status}");
    #[cfg(target_os = "linux")]
    {
        let status =
            std::fs::read_to_string(format!("/proc/{}/status", daemon.child.id())).unwrap();
        let kb: usize = status
            .lines()
            .find_map(|s| s.strip_prefix("VmRSS:"))
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert!(kb < 128 * 1024, "daemon RSS {kb} KiB");
    }
}

fn wait_safety_stall(rpc: &mut Rpc) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let children = rpc.list();
        let state = &find(&children, "safety-flood").state;
        assert!(matches!(state, ChildState::Running(_)));
        if record(state).consumer_stalled {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "producer never reached bounded stall"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn safety_flood_legacy_unattended_recovers_and_captures_final_tail() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let mut rpc = daemon.rpc();
    let gate = daemon.paths.barrier.join("produce");
    let done = daemon.paths.barrier.join("flushed");
    let fragment_gate = daemon.paths.barrier.join("finish-fragment");
    rpc.spawn(
        "safety-flood",
        &[
            "safety-flood",
            gate.to_str().unwrap(),
            done.to_str().unwrap(),
            fragment_gate.to_str().unwrap(),
        ],
    );
    std::fs::write(&gate, b"go").unwrap();
    wait_safety_stall(&mut rpc);
    safety_flood_bounds(&daemon);
    assert!(
        !done.exists(),
        "lossless unattended producer should be backpressured"
    );
    assert!(
        !BlockingHostClient::connect(&daemon.paths.endpoint)
            .unwrap()
            .capture_output(sid("safety-flood"), 4096)
            .unwrap()
            .contains(safety_flood::FINAL)
    );
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let mut events = fl_host_proto::events(&daemon.paths.endpoint, Some(sid("safety-flood")))
            .await
            .unwrap();
        let mut count = 0;
        let mut bytes = 0;
        let mut pending_line = Vec::new();
        let mut saw_ready = false;
        let mut saw_fragment = false;
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                match events.next().await.unwrap() {
                    HostEvent::Line {
                        bytes: fragment,
                        terminated,
                        ..
                    } => {
                        // Pipe reads can split a logical line into multiple events.
                        pending_line.extend_from_slice(&fragment);
                        if !terminated {
                            if count == safety_flood::LINES - 1 && pending_line.len() == 1174 {
                                // Release only after observing the gated prefix: no timing
                                // assumption can let this test skip fragmented delivery.
                                assert_eq!(pending_line, safety_flood::line(count)[..1174]);
                                assert!(!saw_fragment);
                                saw_fragment = true;
                                std::fs::write(&fragment_gate, b"go").unwrap();
                            }
                            continue;
                        }
                        let line = std::mem::take(&mut pending_line);
                        if !saw_ready {
                            assert_eq!(line, b"READY");
                            saw_ready = true;
                        } else if line == safety_flood::FINAL.as_bytes() {
                            break;
                        } else {
                            assert_eq!(line, safety_flood::line(count), "record {count}");
                            bytes += line.len();
                            count += 1;
                        }
                    }
                    HostEvent::Control(ControlEvent::SlowSubscriber { .. }) => {
                        panic!("draining subscriber was shed after {count} records ({bytes} bytes)")
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("drain final marker");
        assert!(saw_ready);
        assert!(saw_fragment);
        assert!(pending_line.is_empty());
        assert_eq!(count, safety_flood::LINES);
        assert_eq!(bytes, safety_flood::LINES * safety_flood::LINE_BYTES);
        assert!(bytes > 8 * 1024 * 1024);
    });
    wait_file(&done, |b| b == b"flushed");
    let tail = BlockingHostClient::connect(&daemon.paths.endpoint)
        .unwrap()
        .capture_output(sid("safety-flood"), 4096)
        .unwrap();
    assert!(tail.ends_with(&format!("{}\n", safety_flood::FINAL)));
    assert!(!record(&find(&rpc.list(), "safety-flood").state).consumer_stalled);
}

#[test]
fn safety_flood_legacy_held_subscriber_preserves_control_and_cleanup() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let mut rpc = daemon.rpc();
    arm(&daemon.paths.subscriber, "before-send-1");
    let events = EventLog::subscribe(&daemon.paths.endpoint, Some("safety-flood"));
    wait_file(&daemon.paths.subscriber.join("subscribed-1"), |b| b == b"1");
    let gate = daemon.paths.barrier.join("produce");
    let done = daemon.paths.barrier.join("flushed");
    let child = rpc.spawn(
        "safety-flood",
        &[
            "safety-flood",
            gate.to_str().unwrap(),
            done.to_str().unwrap(),
        ],
    );
    wait_at(&daemon.paths.subscriber, "before-send-1");
    std::fs::write(&gate, b"go").unwrap();
    wait_safety_stall(&mut rpc);
    safety_flood_bounds(&daemon);
    assert!(!done.exists());
    assert!(
        !BlockingHostClient::connect(&daemon.paths.endpoint)
            .unwrap()
            .capture_output(sid("safety-flood"), 4096)
            .unwrap()
            .contains(safety_flood::FINAL)
    );
    let lease = rpc.claim("safety-flood", epoch_of(&child)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    rpc.kill("safety-flood", epoch_of(&child), Some(lease))
        .unwrap();
    loop {
        let children = rpc.list();
        assert!(
            Instant::now() < deadline,
            "cleanup blocked behind held subscriber"
        );
        if matches!(
            find(&children, "safety-flood").state,
            ChildState::CleanupComplete(_)
        ) {
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    release(&daemon.paths.subscriber, "before-send-1");
    drop(events);
}

#[cfg(unix)]
fn rendered_spawn(rpc: &mut Rpc, name: &str) -> StreamId {
    let mut child = spec(name, &[]);
    child.kind = SpawnKind::Pty;
    child.argv = vec![
        "/bin/sh".into(),
        "-c".into(),
        r"stty -echo; printf '\033[2J\033[Hready'; while read line; do stty size; done".into(),
    ];
    match extended(
        rpc,
        XReq::SpawnSession {
            spec: child,
            mode: Mode::Pty,
            pty: None,
        },
    ) {
        XResp::Session { stream, .. } => stream,
        other => panic!("PTY spawn {other:?}"),
    }
}

// More than the former projection limit must coexist, without consumers or Ack.
#[cfg(unix)]
#[test]
fn default_pty_capacity_supports_sixteen_live_gated_children() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let mut rpc = daemon.rpc();
    let gate = daemon.paths.barrier.join("capacity-go");
    let mut streams = Vec::new();
    for index in 0..16 {
        let mut child = spec(
            &format!("capacity-{index}"),
            &["gated-line-burst", gate.to_str().unwrap()],
        );
        child.kind = SpawnKind::Pty;
        let stream = rpc.inner.spawn_pty_session(child, None).unwrap().0;
        streams.push(stream);
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    for stream in &streams {
        loop {
            let screen = rpc.inner.capture_rendered(stream.clone()).unwrap();
            if screen.contents.contains("READY") {
                assert_eq!((screen.rows, screen.cols), (40, 120));
                break;
            }
            assert!(Instant::now() < deadline, "child did not reach its gate");
            thread::sleep(Duration::from_millis(5));
        }
    }
    let records = rpc.inner.list_sessions().unwrap();
    assert_eq!(records.len(), 16);
    assert!(
        records
            .iter()
            .all(|r| matches!(r.state, ChildState::Running(_)))
    );
    std::fs::write(gate, b"go").unwrap();
    for stream in &streams {
        let lease = rpc.inner.claim_controller(stream.clone()).unwrap().lease;
        rpc.inner.kill_session(stream.clone(), lease).unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let records = rpc.inner.list_sessions().unwrap();
        if records
            .iter()
            .all(|r| matches!(r.state, ChildState::CleanupComplete(_)))
        {
            assert!(records.len() <= 16);
            assert!(records.iter().all(|r| r.acked == 0));
            break;
        }
        assert!(Instant::now() < deadline, "PTY cleanup incomplete");
        thread::sleep(Duration::from_millis(5));
    }
    for stream in streams {
        assert!(matches!(
            rpc.inner
                .extension_request(XReq::CaptureRendered { stream })
                .unwrap(),
            XResp::Refused {
                code: Refuse::SessionFenced | Refuse::StaleEpoch
            }
        ));
    }
}

// Catches missing opt-in host support, name-based identity fallback and ioctl/parser divergence.
#[cfg(unix)]
#[test]
fn rendered_capture_resize_identity_and_kernel_geometry() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let mut rpc = daemon.rpc();
    let stream = rendered_spawn(&mut rpc, "rendered");
    let lease = controller(&mut rpc, &stream);
    let deadline = Instant::now() + Duration::from_secs(5);
    let initial = loop {
        let screen = rpc
            .inner
            .capture_rendered(stream.clone())
            .expect("rendered capability and capture");
        if screen.contents == "ready" {
            break screen;
        }
        assert!(Instant::now() < deadline, "screen: {screen:?}");
        thread::yield_now();
    };
    assert_eq!(
        (
            initial.schema,
            initial.rows,
            initial.cols,
            initial.cursor_row,
            initial.cursor_col
        ),
        (1, 40, 120, 0, 5)
    );
    assert_eq!(initial.stream, stream);
    assert!(initial.output_bytes >= 12);
    let generation = rpc
        .inner
        .resize_session(stream.clone(), lease, 160, 50)
        .unwrap();
    let resized = rpc.inner.capture_rendered(stream.clone()).unwrap();
    assert_eq!(
        (resized.rows, resized.cols, resized.generation),
        (50, 160, generation)
    );
    assert!(generation > initial.generation);
    assert_eq!(
        (resized.output_bytes, resized.output_seq),
        (initial.output_bytes, initial.output_seq)
    );
    assert!(
        rpc.inner
            .resize_session(stream.clone(), lease, 0, 50)
            .is_err()
    );
    assert_eq!(rpc.inner.capture_rendered(stream.clone()).unwrap(), resized);
    rpc.inner
        .write_raw(stream.clone(), lease, b"size\n".to_vec())
        .unwrap();
    loop {
        let screen = rpc.inner.capture_rendered(stream.clone()).unwrap();
        if screen.contents.contains("50 160") {
            assert!(screen.generation > generation);
            assert!(screen.output_bytes > resized.output_bytes);
            assert!(screen.output_seq >= resized.output_seq);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "kernel size not observed: {screen:?}"
        );
        thread::yield_now();
    }
    let mut rpc = daemon.rpc();
    let mut wrong = stream.clone();
    wrong.host = fl_host_proto::HostInstance::new([99; 16]);
    assert_eq!(
        extended(&mut rpc, XReq::CaptureRendered { stream: wrong }),
        XResp::Refused {
            code: Refuse::WrongInstance
        }
    );
    let mut wrong = stream.clone();
    wrong.epoch.0 += 1;
    assert_eq!(
        extended(&mut rpc, XReq::CaptureRendered { stream: wrong }),
        XResp::Refused {
            code: Refuse::StaleEpoch
        }
    );
    let pipe = reliable_spawn(&mut rpc, "rendered-pipe", &["echo"]);
    let pipe_lease = controller(&mut rpc, &pipe);
    assert_eq!(
        extended(
            &mut rpc,
            XReq::CaptureRendered {
                stream: pipe.clone()
            }
        ),
        XResp::Refused {
            code: Refuse::WrongMode
        }
    );
    assert_eq!(
        extended(
            &mut rpc,
            XReq::Resize {
                stream: pipe,
                lease: pipe_lease,
                cols: 80,
                rows: 24
            }
        ),
        XResp::Refused {
            code: Refuse::WrongMode
        }
    );
    assert!(
        rpc.inner
            .extension_request(XReq::Attach {
                stream,
                resume: None
            })
            .is_err()
    );
}

#[cfg(unix)]
fn rendered_screen(rpc: &mut Rpc, stream: &StreamId) -> fl_host_proto::RenderedScreen {
    match extended(
        rpc,
        XReq::CaptureRendered {
            stream: stream.clone(),
        },
    ) {
        XResp::RenderedCapture { screen } => screen,
        other => panic!("snapshot {other:?}"),
    }
}

#[cfg(unix)]
fn rendered_ready(rpc: &mut Rpc, stream: &StreamId) -> fl_host_proto::RenderedScreen {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let screen = rendered_screen(rpc, stream);
        if screen.contents == "ready" {
            return screen;
        }
        assert!(Instant::now() < deadline, "not ready: {screen:?}");
        thread::yield_now();
    }
}

// Catches checking only the writer lease while a takeover has invalidated actual authority.
#[cfg(unix)]
#[test]
fn rendered_resize_takeover_before_claim_enqueue_refuses_without_mutation() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let mut owner = daemon.rpc();
    let stream = rendered_spawn(&mut owner, "resize-takeover");
    let lease = controller(&mut owner, &stream);
    let before = rendered_ready(&mut owner, &stream);
    arm(&daemon.paths.barrier, "before-resize");
    owner.push(|id| HostRequest::Extended {
        id,
        request: XReq::Resize {
            stream: stream.clone(),
            lease,
            cols: 160,
            rows: 50,
        },
    });
    wait_at(&daemon.paths.barrier, "before-resize");
    arm(&daemon.paths.subscriber, "reliable-pre-enqueue");
    let mut claimant = daemon.rpc();
    claimant.push(|id| HostRequest::Extended {
        id,
        request: XReq::ClaimController {
            stream: stream.clone(),
        },
    });
    wait_at(&daemon.paths.subscriber, "reliable-pre-enqueue");
    release(&daemon.paths.barrier, "before-resize");
    assert!(matches!(
        owner.pull(),
        HostResponse::Extended {
            response: XResp::Refused {
                code: Refuse::StaleLease
            },
            ..
        }
    ));
    let mut observer = daemon.rpc();
    assert_eq!(rendered_screen(&mut observer, &stream), before);
    release(&daemon.paths.subscriber, "reliable-pre-enqueue");
    assert!(matches!(
        claimant.pull(),
        HostResponse::Extended {
            response: XResp::Controller { .. },
            ..
        }
    ));
}

// Catches stale queued mutation after RPC disconnect, even before writer revocation settles.
#[cfg(unix)]
#[test]
fn rendered_resize_disconnect_refuses_without_mutation() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let mut owner = daemon.rpc();
    let stream = rendered_spawn(&mut owner, "resize-disconnect");
    let lease = controller(&mut owner, &stream);
    let before = rendered_ready(&mut owner, &stream);
    arm(&daemon.paths.barrier, "before-resize");
    owner.push(|id| HostRequest::Extended {
        id,
        request: XReq::Resize {
            stream: stream.clone(),
            lease,
            cols: 160,
            rows: 50,
        },
    });
    wait_at(&daemon.paths.barrier, "before-resize");
    drop(owner);
    wait_file(&daemon.paths.barrier.join("revoked"), |b| !b.is_empty());
    release(&daemon.paths.barrier, "before-resize");
    let mut observer = daemon.rpc();
    let fresh = controller(&mut observer, &stream); // worker passed the old job before this fence completes
    assert_eq!(rendered_screen(&mut observer, &stream), before);
    assert!(matches!(
        extended(
            &mut observer,
            XReq::Resize {
                stream: stream.clone(),
                lease: fresh,
                cols: 80,
                rows: 24
            }
        ),
        XResp::Resized {
            cols: 80,
            rows: 24,
            ..
        }
    ));
    let after = rendered_screen(&mut observer, &stream);
    assert_eq!(
        (after.rows, after.cols, after.generation),
        (24, 80, before.generation + 1)
    );
}

// Catches selecting the replacement by name for an old epoch's capture or resize.
#[cfg(unix)]
#[test]
fn rendered_old_epoch_never_selects_respawned_session() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let mut rpc = daemon.rpc();
    let old = rendered_spawn(&mut rpc, "rendered-respawn");
    let lease = controller(&mut rpc, &old);
    let before = rendered_ready(&mut rpc, &old);
    assert_eq!((before.rows, before.cols), (40, 120));
    assert_eq!(
        extended(
            &mut rpc,
            XReq::KillSession {
                stream: old.clone(),
                lease
            }
        ),
        XResp::Controlled
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let XResp::Sessions { records } = extended(&mut rpc, XReq::ListSessions) else {
            panic!("list")
        };
        if records
            .iter()
            .any(|r| r.stream == old && matches!(r.state, ChildState::CleanupComplete(_)))
        {
            break;
        }
        assert!(Instant::now() < deadline);
        thread::yield_now();
    }
    assert!(matches!(
        extended(&mut rpc, XReq::CaptureOutput { session: old.session.clone(), max_bytes: 64 * 1024 }),
        XResp::CapturedOutput { text } if text.ends_with("ready")
    ));
    let new = rendered_spawn(&mut rpc, "rendered-respawn");
    assert_ne!(old.epoch, new.epoch);
    assert_eq!(rendered_ready(&mut rpc, &new).stream, new);
    assert_eq!(
        extended(
            &mut rpc,
            XReq::CaptureRendered {
                stream: old.clone()
            }
        ),
        XResp::Refused {
            code: Refuse::SessionFenced
        }
    );
    assert!(matches!(
        extended(
            &mut rpc,
            XReq::Resize {
                stream: old,
                lease,
                cols: 80,
                rows: 24
            }
        ),
        XResp::Refused {
            code: Refuse::SessionFenced
        }
    ));
    assert_eq!(
        (
            rendered_screen(&mut rpc, &new).rows,
            rendered_screen(&mut rpc, &new).cols
        ),
        (40, 120)
    );
}

// Catches deadline renewal while a queued resize waits for execution.
#[cfg(unix)]
#[test]
fn rendered_resize_held_past_deadline_refuses_without_mutation() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let mut rpc = daemon.rpc();
    let stream = rendered_spawn(&mut rpc, "resize-deadline");
    let lease = controller(&mut rpc, &stream);
    let before = rendered_ready(&mut rpc, &stream);
    arm(&daemon.paths.barrier, "before-resize");
    rpc.push(|id| HostRequest::Extended {
        id,
        request: XReq::Resize {
            stream: stream.clone(),
            lease,
            cols: 160,
            rows: 50,
        },
    });
    wait_at(&daemon.paths.barrier, "before-resize");
    assert!(matches!(
        rpc.pull(),
        HostResponse::Extended {
            response: XResp::Refused { code: Refuse::Busy },
            ..
        }
    ));
    release(&daemon.paths.barrier, "before-resize");
    assert_eq!(rendered_screen(&mut rpc, &stream), before);
}

// Catches reordering of queued geometry changes or returning a later generation.
#[cfg(unix)]
#[test]
fn rendered_resize_queue_preserves_geometry_and_generation_order() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let mut rpc = daemon.rpc();
    let stream = rendered_spawn(&mut rpc, "resize-order");
    let lease = controller(&mut rpc, &stream);
    let before = rendered_ready(&mut rpc, &stream);
    arm(&daemon.paths.barrier, "before-resize");
    let first = rpc.push(|id| HostRequest::Extended {
        id,
        request: XReq::Resize {
            stream: stream.clone(),
            lease,
            cols: 160,
            rows: 50,
        },
    });
    wait_at(&daemon.paths.barrier, "before-resize");
    let second = rpc.push(|id| HostRequest::Extended {
        id,
        request: XReq::Resize {
            stream: stream.clone(),
            lease,
            cols: 80,
            rows: 24,
        },
    });
    wait_file(&daemon.paths.barrier.join("queued-resize-80-24"), |b| {
        !b.is_empty()
    });
    release(&daemon.paths.barrier, "before-resize");
    let mut revisions = std::collections::BTreeMap::new();
    for _ in 0..2 {
        let HostResponse::Extended {
            id,
            response:
                XResp::Resized {
                    cols,
                    rows,
                    projection_revision,
                },
        } = rpc.pull()
        else {
            panic!("resize")
        };
        revisions.insert(id.0, (cols, rows, projection_revision));
    }
    assert_eq!(revisions[&first.0], (160, 50, before.generation + 1));
    assert_eq!(revisions[&second.0], (80, 24, before.generation + 2));
    let after = rendered_screen(&mut rpc, &stream);
    assert_eq!(
        (after.cols, after.rows, after.generation),
        (80, 24, before.generation + 2)
    );
}

// Native Windows coverage: valid new requests explicitly refuse without advertising bit 8.
#[cfg(not(unix))]
#[test]
fn rendered_capture_and_resize_are_unsupported_without_unix_backend() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let mut rpc = daemon.rpc();
    let XResp::Capabilities { features, .. } = extended(&mut rpc, XReq::Capabilities) else {
        panic!("capabilities")
    };
    assert_eq!(features & fl_host_proto::RENDERED_PTY_FEATURE, 0);
    let stream = reliable_spawn(&mut rpc, "unsupported-screen", &["echo"]);
    assert_eq!(
        extended(
            &mut rpc,
            XReq::CaptureRendered {
                stream: stream.clone()
            }
        ),
        XResp::Refused {
            code: Refuse::Unsupported
        }
    );
    assert_eq!(
        extended(
            &mut rpc,
            XReq::Resize {
                stream: stream.clone(),
                lease: ControllerLease(1),
                cols: 80,
                rows: 24
            }
        ),
        XResp::Refused {
            code: Refuse::Unsupported
        }
    );
    assert!(matches!(
        rpc.inner.capture_rendered(stream.clone()),
        Err(HostError::Unsupported { .. })
    ));
    assert!(matches!(
        rpc.inner.resize_session(stream, ControllerLease(1), 80, 24),
        Err(HostError::Unsupported { .. })
    ));
}

#[cfg(unix)]
mod rendered_deadline {
    use super::*;

    pub(super) fn run<T>(deadline: Instant, context: &str, work: impl FnOnce() -> T) -> T {
        assert!(Instant::now() < deadline, "rendered deadline: {context}");
        let value = work();
        assert!(Instant::now() < deadline, "rendered deadline: {context}");
        value
    }

    pub(super) fn wait<T>(
        deadline: Instant,
        context: &str,
        mut poll: impl FnMut() -> Option<T>,
    ) -> T {
        loop {
            if let Some(value) = run(deadline, context, &mut poll) {
                return value;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::cell::Cell;
        use std::panic::{AssertUnwindSafe, catch_unwind};

        #[test]
        fn expired_deadline_refuses_to_start_work() {
            let called = Cell::new(false);
            let result = catch_unwind(AssertUnwindSafe(|| {
                run(Instant::now() - Duration::from_millis(1), "expired", || {
                    called.set(true);
                });
            }));
            assert!(result.is_err());
            assert!(!called.get());
        }

        #[test]
        fn wait_rejects_success_returned_after_deadline() {
            let deadline = Instant::now() + Duration::from_millis(50);
            let called = Cell::new(false);
            let result = catch_unwind(AssertUnwindSafe(|| {
                wait(deadline, "late successful checkpoint or RPC", || {
                    called.set(true);
                    thread::sleep(deadline.saturating_duration_since(Instant::now()));
                    Some(())
                });
            }));
            assert!(called.get(), "late-return scenario must run its callback");
            assert!(result.is_err(), "a late successful result must be refused");
        }
    }
}

#[cfg(unix)]
fn wait_rendered_fixture(path: &Path) -> u64 {
    wait_rendered_fixture_until(path, Instant::now() + Duration::from_secs(30))
}

#[cfg(unix)]
fn wait_rendered_fixture_until(path: &Path, deadline: Instant) -> u64 {
    rendered_deadline::wait(
        deadline,
        &format!("fixture checkpoint {}", path.display()),
        || std::fs::read_to_string(path).ok()?.parse().ok(),
    )
}

#[cfg(unix)]
fn rendered_through(rpc: &mut Rpc, stream: &StreamId, bytes: u64) -> fl_host_proto::RenderedScreen {
    rendered_through_until(rpc, stream, bytes, Instant::now() + Duration::from_secs(30))
}

#[cfg(unix)]
fn rendered_through_until(
    rpc: &mut Rpc,
    stream: &StreamId,
    bytes: u64,
    deadline: Instant,
) -> fl_host_proto::RenderedScreen {
    rendered_deadline::wait(
        deadline,
        &format!("projection drain through {bytes} bytes"),
        || {
            let screen = rpc.inner.capture_rendered(stream.clone()).unwrap();
            (screen.output_bytes >= bytes).then_some(screen)
        },
    )
}

#[cfg(unix)]
fn rendered_fixture_spawn(rpc: &mut Rpc, name: &str, mode: &str, root: &Path) -> StreamId {
    let mut child = spec(name, &[mode, root.to_str().unwrap()]);
    child.kind = SpawnKind::Pty;
    rpc.inner
        .spawn_pty_session(
            child,
            Some(fl_host_proto::PtyOptions {
                rows: 6,
                cols: 32,
                scrollback_rows: 0,
            }),
        )
        .unwrap()
        .0
}

// Catches reconstructing from the retained tail, parser starvation behind replay,
// loss of alternate-screen state and buffering control strings or split UTF-8.
#[cfg(unix)]
#[test]
fn rendered_tui_retains_early_state_after_eviction_with_unread_and_stalled_consumers() {
    use rendered_deadline::{run, wait};

    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let root = tempfile::tempdir().unwrap();
    let mut rpc = daemon.rpc();
    let stream = rendered_fixture_spawn(&mut rpc, "rendered-tui", "rendered-tui", root.path());
    let lease = rpc.inner.claim_controller(stream.clone()).unwrap().lease;
    let initial_bytes = wait_rendered_fixture(&root.path().join("ready"));
    let initial = rendered_through(&mut rpc, &stream, initial_bytes);
    assert_eq!(initial.contents, "ALTSEED");
    assert_eq!((initial.rows, initial.cols), (6, 32));

    // One ticket stays unopened. A second consumer pins a record before its
    // first write, without depending on socket-buffer capacity or scheduling.
    let mut unread = daemon.rpc();
    let unread_ticket = match extended(
        &mut unread,
        XReq::Observe {
            stream: stream.clone(),
            after_seq: Some(1),
        },
    ) {
        XResp::Subscription { ticket, .. } => ticket,
        other => panic!("unread observer {other:?}"),
    };
    arm(&daemon.paths.subscriber, "reliable-before-part");
    let stalled_ticket = match rpc
        .inner
        .extension_request(XReq::SubscribeReliable {
            stream: stream.clone(),
            lease,
            after_seq: Some(0),
        })
        .unwrap()
    {
        XResp::Subscription { ticket, .. } => ticket,
        other => panic!("stalled ticket {other:?}"),
    };
    // reliable_hold expires after five seconds. Start one shorter deadline
    // before opening the event socket can begin that hold, and keep every
    // checkpoint and blocking result inside it so expiry cannot create a pass.
    let deadline = Instant::now() + Duration::from_secs(4);
    let (_stalled_rt, stalled) = run(deadline, "open held subscriber", || {
        event_socket(&daemon.paths.endpoint, stalled_ticket)
    });
    wait(deadline, "subscriber reached reliable-before-part", || {
        daemon
            .paths
            .subscriber
            .join("at-reliable-before-part")
            .exists()
            .then_some(())
    });
    std::fs::write(root.path().join("run"), b"1").unwrap();

    let escape_bytes = wait_rendered_fixture_until(&root.path().join("escape-ready"), deadline);
    assert!(escape_bytes > 8 * 1024 * 1024);
    let escape = rendered_through_until(&mut rpc, &stream, escape_bytes, deadline);
    assert!(escape.contents.starts_with("ALTSEED"));
    assert!(!escape.contents.contains("hidden"));
    assert!(escape.generation > initial.generation);
    std::fs::write(root.path().join("escape-go"), b"1").unwrap();

    let utf8_bytes = wait_rendered_fixture_until(&root.path().join("utf8-ready"), deadline);
    let utf8 = rendered_through_until(&mut rpc, &stream, utf8_bytes, deadline);
    assert_eq!(utf8.contents, "PRIMARY\nFINAL ");
    assert_eq!((utf8.cursor_row, utf8.cursor_col), (1, 6));
    std::fs::write(root.path().join("utf8-go"), b"1").unwrap();
    let done_bytes = wait_rendered_fixture_until(&root.path().join("done"), deadline);
    let final_screen = rendered_through_until(&mut rpc, &stream, done_bytes, deadline);
    assert_eq!(final_screen.contents, "PRIMARY\nFINAL ✓");
    assert_eq!((final_screen.cursor_row, final_screen.cursor_col), (1, 7));
    assert_eq!((final_screen.rows, final_screen.cols), (6, 32));
    assert_eq!(final_screen.output_bytes, done_bytes);
    assert_eq!(final_screen.stream, stream);
    assert!(final_screen.generation > utf8.generation);
    assert!(final_screen.output_seq >= escape.output_seq);
    assert_eq!(
        run(deadline, "repeat coherent capture", || {
            rpc.inner.capture_rendered(stream.clone()).unwrap()
        }),
        final_screen
    );
    let raw = run(deadline, "raw retained tail", || {
        rpc.inner
            .capture_output(stream.session.clone(), 64 * 1024)
            .unwrap()
    });
    assert_eq!(raw.len(), 64 * 1024);
    assert!(
        !raw.contains("PRIMARY"),
        "primary cells must predate the retained raw tail"
    );
    assert!(raw.ends_with("FINAL ✓"));
    let summary = run(deadline, "session state while subscriber held", || {
        rpc.inner.list_sessions().unwrap()
    })
    .into_iter()
    .find(|s| s.stream == stream)
    .unwrap();
    assert!(!record(&summary.state).consumer_stalled);

    // The unopened ticket resumes with an explicit gap. This proves early
    // state really was evicted while its coherent screen remained available.
    let (rt, mut replay) = run(deadline, "open unread observer", || {
        event_socket(&daemon.paths.endpoint, unread_ticket)
    });
    let gap = run(deadline, "eviction gap while subscriber held", || {
        rt.block_on(async {
            tokio::time::timeout(
                deadline.saturating_duration_since(Instant::now()),
                replay.next_frame(),
            )
            .await
            .unwrap()
            .unwrap()
        })
    });
    assert!(
        matches!(gap, fl_host_proto::ExtensionFrame::Gap { from_seq: 2, next_seq } if next_seq > 2),
        "expected eviction gap, got {gap:?}"
    );
    run(deadline, "release subscriber before hold expires", || {
        release(&daemon.paths.subscriber, "reliable-before-part");
    });
    drop(replay);
    drop(stalled);
    rpc.inner.kill_session(stream, lease).unwrap();
}

// Catches applying parser resize before the ioctl, losing output/resize order,
// or returning a generation unrelated to the geometry linearization point.
#[cfg(unix)]
#[test]
fn rendered_child_winsize_matches_ordered_resize_generation_and_output() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let root = tempfile::tempdir().unwrap();
    let mut rpc = daemon.rpc();
    let stream = rendered_fixture_spawn(
        &mut rpc,
        "rendered-resize-child",
        "rendered-resize",
        root.path(),
    );
    let lease = rpc.inner.claim_controller(stream.clone()).unwrap().lease;
    let ready_bytes = wait_rendered_fixture(&root.path().join("ready"));
    let initial = rendered_through(&mut rpc, &stream, ready_bytes);
    assert_eq!(initial.contents, "SIZE 6 32");
    for (step, cols, rows) in [(1, 80, 24), (2, 40, 12)] {
        let before = rpc.inner.capture_rendered(stream.clone()).unwrap();
        let generation = rpc
            .inner
            .resize_session(stream.clone(), lease, cols, rows)
            .unwrap();
        let resized = rpc.inner.capture_rendered(stream.clone()).unwrap();
        assert_eq!(
            (resized.rows, resized.cols, resized.generation),
            (rows, cols, generation)
        );
        assert!(generation > before.generation);
        assert_eq!(
            (resized.output_bytes, resized.output_seq),
            (before.output_bytes, before.output_seq)
        );
        std::fs::write(root.path().join(format!("report-{step}")), b"1").unwrap();
        let bytes = wait_rendered_fixture(&root.path().join(format!("reported-{step}")));
        let after = rendered_through(&mut rpc, &stream, bytes);
        assert_eq!(after.contents, format!("SIZE {rows} {cols}"));
        assert_eq!((after.rows, after.cols), (rows, cols));
        assert!(after.generation > generation);
        assert!(after.output_bytes > resized.output_bytes);
        assert!(after.output_seq >= resized.output_seq);
    }
    rpc.inner.kill_session(stream, lease).unwrap();
}

// No consumer connection or Ack is created in this fixture.
#[cfg(unix)]
#[test]
fn unattended_pty_retention_survives_more_than_sixty_four_epochs() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let mut rpc = daemon.rpc();
    let baseline = rpc.inner.pty_retention_status().unwrap().unwrap();
    assert_eq!(baseline.projection_reserved_bytes, 0);
    let mut streams = Vec::new();
    for n in 0..74 {
        // Reusing the first name at the end tests full-epoch diagnostic identity.
        let name = if n == 73 {
            "tail-000".to_owned()
        } else {
            format!("tail-{n:03}")
        };
        let marker = daemon.paths.barrier.join("drained");
        let mut child = if n == 0 {
            spec(
                &name,
                &["stdout-fragments", "lines", marker.to_str().unwrap(), "eof"],
            )
        } else {
            spec(&name, &["exit", "0"])
        };
        child.kind = SpawnKind::Pty;
        let stream = rpc.inner.spawn_pty_session(child, None).unwrap().0;
        streams.push(stream.clone());
        let mut cleanup_requested = false;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let status = rpc.inner.pty_retention_status().unwrap().unwrap();
            let records = rpc.inner.list_sessions().unwrap();
            // A bare synthetic child is not a hosted owner: leader exit alone
            // deliberately does not request containment cleanup. Request that
            // boundary after natural exit, without subscribing or acknowledging.
            if !cleanup_requested
                && records
                    .iter()
                    .any(|r| r.stream == stream && matches!(r.state, ChildState::LeaderExited(_)))
            {
                let lease = rpc.inner.claim_controller(stream.clone()).unwrap().lease;
                rpc.inner.kill_session(stream.clone(), lease).unwrap();
                cleanup_requested = true;
            }
            if status.projection_reserved_bytes == 0
                && status.completed_pty_sessions == (n + 1).min(8)
                && status.evicted_total == u64::from((n + 1).saturating_sub(8))
                && records.iter().any(|r| {
                    r.stream == stream && matches!(r.state, ChildState::CleanupComplete(_))
                })
            {
                assert!(records.len() <= 8);
                assert!(records.iter().all(|r| r.acked == 0));
                let tail = rpc
                    .inner
                    .capture_output(stream.session.clone(), 65536)
                    .unwrap();
                if n == 0 {
                    assert_eq!(tail.len(), 65536);
                    assert!(tail.ends_with("99999\r\n"));
                } else {
                    assert_eq!(tail, "READY\r\n");
                }
                break;
            }
            assert!(
                Instant::now() < deadline,
                "epoch {n} did not finish: diagnostic={status:?} records={records:?} child_finished_output={} tail={:?}",
                marker.exists(),
                rpc.inner.capture_output(stream.session.clone(), 128)
            );
            thread::sleep(Duration::from_millis(5));
        }
    }
    let status = rpc.inner.pty_retention_status().unwrap().unwrap();
    assert_eq!(status.completed_pty_sessions, 8);
    assert_eq!(status.evicted_total, 66);
    assert_eq!(status.recent_evictions, streams[2..66]);
    assert_eq!(
        status.projection_reserved_bytes,
        baseline.projection_reserved_bytes
    );
    let evicted = &streams[2];
    let err = rpc.inner.claim_controller(evicted.clone()).unwrap_err();
    assert!(
        matches!(&err, HostError::PtyEpochEvicted { stream } if stream == evicted),
        "{err:?}"
    );
    let error = err.to_string();
    assert!(error.contains("PTY epoch evicted"), "{error}");
    assert!(
        error.contains(&serde_json::to_string(evicted).unwrap()),
        "{error}"
    );
    let old_same_name = &streams[0];
    let err = rpc
        .inner
        .claim_controller(old_same_name.clone())
        .unwrap_err();
    assert!(
        matches!(&err, HostError::StaleEpoch { stream } if stream == old_same_name),
        "{err:?}"
    );
    let error = err.to_string();
    assert!(error.contains("stale epoch (possibly evicted"), "{error}");
    assert!(
        !error.contains("PTY epoch evicted"),
        "history must not invent proof"
    );
    assert!(
        rpc.inner
            .capture_output(streams[73].session.clone(), 65536)
            .unwrap()
            .ends_with("READY\r\n")
    );
    // Old/raw clients continue receiving the existing refusal, never a new shape.
    assert_eq!(
        rpc.inner
            .extension_request(XReq::Observe {
                stream: evicted.clone(),
                after_seq: None
            })
            .unwrap(),
        XResp::Refused {
            code: Refuse::StaleEpoch
        }
    );
}

#[cfg(unix)]
#[test]
fn attached_completed_pty_reader_receives_session_fenced_notice_on_compaction() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let mut rpc = daemon.rpc();
    let mut old_rpc = daemon.rpc();

    let mut child = spec("fenced-reader", &["hold"]);
    child.kind = SpawnKind::Pty;
    let (stream, _) = rpc.inner.spawn_pty_session(child, None).unwrap();
    let lease = rpc.inner.claim_controller(stream.clone()).unwrap().lease;

    let mut old_child = spec("fenced-reader-v1", &["hold"]);
    old_child.kind = SpawnKind::Pty;
    let (old_stream, _) = old_rpc.inner.spawn_pty_session(old_child, None).unwrap();
    let old_lease = old_rpc
        .inner
        .claim_controller(old_stream.clone())
        .unwrap()
        .lease;

    let XResp::Subscription {
        ticket: sub_ticket, ..
    } = rpc
        .inner
        .extension_request(XReq::SubscribeReliable {
            stream: stream.clone(),
            lease,
            after_seq: Some(0),
        })
        .unwrap()
    else {
        panic!("subscribe ticket");
    };
    let (rt, mut events) = event_socket(&daemon.paths.endpoint, sub_ticket);
    assert_eq!(event_seq(&rt, &mut events, 0), 1);
    assert_eq!(event_seq(&rt, &mut events, 1), 2);

    let XResp::Subscription {
        ticket: old_sub_ticket,
        ..
    } = old_rpc
        .inner
        .extension_request(XReq::SubscribeReliable {
            stream: old_stream.clone(),
            lease: old_lease,
            after_seq: Some(0),
        })
        .unwrap()
    else {
        panic!("old subscribe ticket");
    };
    let mut old_events = rt.block_on(async {
        let client = AsyncHostClient::connect_with(
            &daemon.paths.endpoint,
            fl_host_proto::ConnectOptions {
                minor: 1,
                ..Default::default()
            },
        )
        .await
        .expect("old event connect");
        assert_eq!(client.negotiated().1, 1);
        client
            .open_extension_stream(old_sub_ticket)
            .await
            .expect("old open")
    });
    assert_eq!(event_seq(&rt, &mut old_events, 0), 1);
    assert_eq!(event_seq(&rt, &mut old_events, 1), 2);

    // Hold both readers before the next record's first part so unread records are
    // still pending when completed-PTY compaction fences their journals.
    arm(&daemon.paths.subscriber, "reliable-before-part");
    rpc.inner.kill_session(stream.clone(), lease).unwrap();
    old_rpc
        .inner
        .kill_session(old_stream.clone(), old_lease)
        .unwrap();
    wait_at(&daemon.paths.subscriber, "reliable-before-part");

    // Completing both PTYs triggers completed-PTY compaction (`compact_completed_pty`)
    // and cancels their reader streams without needing to churn 9 extra PTYs.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = rpc.inner.pty_retention_status().unwrap().unwrap();
        let records = rpc.inner.list_sessions().unwrap();
        if records
            .iter()
            .any(|r| r.stream == stream && matches!(r.state, ChildState::CleanupComplete(_)))
            && records.iter().any(|r| {
                r.stream == old_stream && matches!(r.state, ChildState::CleanupComplete(_))
            })
            && status.completed_pty_sessions == 2
        {
            break;
        }
        assert!(Instant::now() < deadline, "compaction PTY did not complete");
        thread::sleep(Duration::from_millis(5));
    }

    for mut reader in [events, old_events] {
        let notice = rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    match reader
                        .next_frame()
                        .await
                        .expect("fenced reader frame before EOF")
                    {
                        fl_host_proto::ExtensionFrame::Notice { code } => return code,
                        fl_host_proto::ExtensionFrame::ReliablePart { .. } => continue,
                        other => panic!("unexpected frame on fenced PTY stream: {other:?}"),
                    }
                }
            })
            .await
            .expect("fenced notice deadline")
        });
        assert_eq!(notice, Refuse::SessionFenced);
    }
    release(&daemon.paths.subscriber, "reliable-before-part");
}

#[cfg(unix)]
#[test]
fn pipes_survive_completed_pty_compaction_and_sixty_four_slot_pressure() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let mut rel_rpc = daemon.rpc();
    let mut leg_rpc = daemon.rpc();
    let mut pty_rpc = daemon.rpc();

    // 1. Spawn a ReliablePipe and verify initial replay + partial Ack.
    let rel_stream = reliable_spawn(&mut rel_rpc, "keep-reliable", &["echo"]);
    let rel_lease = controller(&mut rel_rpc, &rel_stream);
    let sub_ticket = ticket(&mut rel_rpc, &rel_stream, rel_lease, 0);
    let (rt, mut rel_events) = event_socket(&daemon.paths.endpoint, sub_ticket);
    assert_eq!(event_seq(&rt, &mut rel_events, 0), 1); // Spawned
    assert_eq!(event_seq(&rt, &mut rel_events, 1), 2); // READY
    assert_eq!(
        ack(&mut rel_rpc, &rel_stream, rel_lease, 2),
        XResp::Acked { through: 2 }
    );
    assert_eq!(
        extended(
            &mut rel_rpc,
            XReq::SendLine {
                stream: rel_stream.clone(),
                lease: rel_lease,
                line: "rel-before".into(),
            },
        ),
        XResp::Input {
            outcome: SendOutcome::Written,
        }
    );
    assert_eq!(event_seq(&rt, &mut rel_events, 2), 3); // rel-before (left unacked across churn)
    drop(rel_events);

    // 2. Spawn a LegacyPipe and verify initial CaptureOutput.
    let leg_child = leg_rpc.spawn("keep-legacy", &["echo"]);
    let leg_epoch = record(&leg_child.state).session_epoch.0;
    let leg_lease = leg_rpc
        .claim("keep-legacy", leg_epoch)
        .expect("legacy lease");
    assert_eq!(
        leg_rpc.send("keep-legacy", leg_epoch, leg_lease, "leg-before"),
        SendOutcome::Written
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let out = pty_rpc
            .inner
            .capture_output(sid("keep-legacy"), 4096)
            .unwrap();
        if out.contains("READY\n") && out.contains("leg-before\n") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "legacy output before churn: {out:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }

    // 3. Churn 10 completed PTYs (>8 completed-PTY retention limit).
    for n in 0..10 {
        let mut pty = spec(&format!("pty-churn-{n:02}"), &["exit", "0"]);
        pty.kind = SpawnKind::Pty;
        let (pty_stream, _) = pty_rpc.inner.spawn_pty_session(pty, None).unwrap();
        let mut killed = false;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let status = pty_rpc.inner.pty_retention_status().unwrap().unwrap();
            let records = pty_rpc.inner.list_sessions().unwrap();
            if !killed
                && records.iter().any(|r| {
                    r.stream == pty_stream && matches!(r.state, ChildState::LeaderExited(_))
                })
            {
                let l = pty_rpc
                    .inner
                    .claim_controller(pty_stream.clone())
                    .unwrap()
                    .lease;
                pty_rpc.inner.kill_session(pty_stream.clone(), l).unwrap();
                killed = true;
            }
            if status.completed_pty_sessions == (n + 1).min(8)
                && status.evicted_total == u64::from((n + 1).saturating_sub(8))
                && records.iter().any(|r| {
                    r.stream == pty_stream && matches!(r.state, ChildState::CleanupComplete(_))
                })
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "pty-churn-{n:02} did not complete"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    // 4. Fill to 64 total outputs (2 pipes + 8 completed PTYs + 54 live PTYs = 64)
    // and spawn 2 more PTYs to force 64-slot pressure eviction of completed PTYs.
    let mut live_ptys = Vec::new();
    for n in 0..54 {
        let mut pty = spec(&format!("pty-live-{n:02}"), &["stall"]);
        pty.kind = SpawnKind::Pty;
        let (pty_stream, _) = pty_rpc.inner.spawn_pty_session(pty, None).unwrap();
        live_ptys.push(pty_stream);
    }
    assert_eq!(pty_rpc.inner.list_sessions().unwrap().len(), 64);

    for n in 0..2 {
        let mut pty = spec(&format!("pty-pressure-{n}"), &["stall"]);
        pty.kind = SpawnKind::Pty;
        let (pty_stream, _) = pty_rpc.inner.spawn_pty_session(pty, None).unwrap();
        live_ptys.push(pty_stream);
    }
    let status = pty_rpc.inner.pty_retention_status().unwrap().unwrap();
    assert_eq!(status.completed_pty_sessions, 6);
    assert_eq!(status.evicted_total, 4);
    assert!(!status.recent_evictions.contains(&rel_stream));

    // 5. Verify ReliablePipe replay and Ack still work after PTY compaction and slot pressure.
    assert_eq!(
        extended(
            &mut rel_rpc,
            XReq::SendLine {
                stream: rel_stream.clone(),
                lease: rel_lease,
                line: "rel-after".into(),
            },
        ),
        XResp::Input {
            outcome: SendOutcome::Written,
        }
    );
    let sub_ticket = ticket(&mut rel_rpc, &rel_stream, rel_lease, 2);
    let (rt2, mut rel_events2) = event_socket(&daemon.paths.endpoint, sub_ticket);
    assert_eq!(event_seq(&rt2, &mut rel_events2, 2), 3); // unacked rel-before replayed!
    assert_eq!(event_seq(&rt2, &mut rel_events2, 3), 4); // rel-after
    assert_eq!(
        ack(&mut rel_rpc, &rel_stream, rel_lease, 4),
        XResp::Acked { through: 4 }
    );
    drop(rel_events2);

    // 6. Verify LegacyPipe send and CaptureOutput still work after PTY compaction and slot pressure.
    assert_eq!(
        leg_rpc.send("keep-legacy", leg_epoch, leg_lease, "leg-after"),
        SendOutcome::Written
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let out = pty_rpc
            .inner
            .capture_output(sid("keep-legacy"), 4096)
            .unwrap();
        if out.contains("READY\n") && out.contains("leg-before\n") && out.contains("leg-after\n") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "legacy output after churn: {out:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
#[test]
fn hosted_owner_pty_and_reliable_pipe_auto_cleanup_after_leader_exit() {
    let daemon = Daemon::start(DaemonOpts { chunk: None });
    let mut rpc = daemon.rpc();
    let baseline = rpc.inner.pty_retention_status().unwrap().unwrap();
    assert_eq!(baseline.projection_reserved_bytes, 0);

    // 1. Hosted-owner PTY session with a lingering descendant.
    let mut pty_spec = spec("hosted-pty", &["orphan-linger"]);
    pty_spec.kind = SpawnKind::Pty;
    pty_spec
        .env
        .insert("FL_HOSTED_ID".into(), "hosted-pty-owner".into());
    pty_spec.env_allow.insert("FL_HOSTED_ID".into());
    let (pty_stream, _) = rpc.inner.spawn_pty_session(pty_spec, None).unwrap();

    // 2. Hosted-owner ReliablePipe session with a lingering descendant.
    let mut pipe_spec = spec("hosted-relpipe", &["orphan-linger"]);
    pipe_spec
        .env
        .insert("FL_HOSTED_ID".into(), "hosted-pipe-owner".into());
    pipe_spec.env_allow.insert("FL_HOSTED_ID".into());
    let XResp::Session {
        stream: pipe_stream,
        ..
    } = rpc
        .inner
        .extension_request(XReq::SpawnSession {
            spec: pipe_spec,
            mode: fl_host_proto::SessionMode::ReliablePipe,
            pty: None,
        })
        .unwrap()
    else {
        panic!("spawn reliable pipe")
    };

    // 3. Hosted-owner session that exits cleanly with 0 lingering direct children.
    let mut clean_spec = spec("hosted-clean", &["exit", "0"]);
    clean_spec
        .env
        .insert("FL_HOSTED_ID".into(), "hosted-clean-owner".into());
    clean_spec.env_allow.insert("FL_HOSTED_ID".into());
    let XResp::Session {
        stream: clean_stream,
        ..
    } = rpc
        .inner
        .extension_request(XReq::SpawnSession {
            spec: clean_spec,
            mode: fl_host_proto::SessionMode::ReliablePipe,
            pty: None,
        })
        .unwrap()
    else {
        panic!("spawn clean reliable pipe")
    };

    let extract_child = |rpc: &mut Rpc, session_name: &str| -> (u32, u64) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let text = rpc.inner.capture_output(sid(session_name), 4096).unwrap();
            if let Some(pos) = text.find("CHILD ")
                && text[pos..].contains(" END")
            {
                let digits: String = text[pos + "CHILD ".len()..]
                    .chars()
                    .take_while(|ch| ch.is_ascii_digit())
                    .collect();
                let pid: u32 = digits.parse().expect("child pid");
                let start = fl_host::process_identity(pid).expect("child ident").start;
                return (pid, start);
            }
            assert!(Instant::now() < deadline, "missing CHILD line: {text:?}");
            thread::sleep(Duration::from_millis(10));
        }
    };

    let (pty_child_pid, pty_child_start) = extract_child(&mut rpc, "hosted-pty");
    let (pipe_child_pid, pipe_child_start) = extract_child(&mut rpc, "hosted-relpipe");

    let deadline = Instant::now()
        + fl_host::HOSTED_LEADER_EXIT_GRACE
        + fl_host::CLEANUP_DEADLINE
        + Duration::from_secs(2);
    loop {
        let status = rpc.inner.pty_retention_status().unwrap().unwrap();
        let records = rpc.inner.list_sessions().unwrap();
        let pty_done = records
            .iter()
            .any(|r| r.stream == pty_stream && matches!(r.state, ChildState::CleanupComplete(_)));
        // Unattended ReliablePipe session retires once its epoch threads finish after auto-cleanup.
        let pipe_done = records.iter().all(|r| r.stream != pipe_stream)
            || records.iter().any(|r| {
                r.stream == pipe_stream && matches!(r.state, ChildState::CleanupComplete(_))
            });
        let clean_done = records.iter().all(|r| r.stream != clean_stream)
            || records.iter().any(|r| {
                r.stream == clean_stream && matches!(r.state, ChildState::CleanupComplete(_))
            });
        let stderr = daemon.stderr.lock().expect("stderr").clone();
        #[cfg(target_os = "linux")]
        let logs_ready = {
            let pty_expected = format!(
                "fl-host: hosted session hosted-pty (epoch {}) armed post-leader-exit cleanup after 500ms (direct_children=1)",
                pty_stream.epoch.0
            );
            let pipe_expected = format!(
                "fl-host: hosted session hosted-relpipe (epoch {}) armed post-leader-exit cleanup after 500ms (direct_children=1)",
                pipe_stream.epoch.0
            );
            stderr.lines().any(|line| line == pty_expected)
                && stderr.lines().any(|line| line == pipe_expected)
        };
        #[cfg(not(target_os = "linux"))]
        let logs_ready = true;
        if pty_done
            && pipe_done
            && clean_done
            && status.projection_reserved_bytes == 0
            && status.completed_pty_sessions == 1
            && logs_ready
        {
            assert!(
                !stderr.contains("hosted-clean"),
                "clean exit with 0 direct_children logged unexpectedly: {stderr}"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "hosted sessions did not auto-clean: status={status:?} records={records:?} stderr={stderr:?}"
        );
        thread::sleep(Duration::from_millis(15));
    }

    assert!(
        fl_host::read_registry(&daemon.paths.endpoint)
            .expect("registry")
            .sessions
            .is_empty()
    );
    #[cfg(target_os = "linux")]
    {
        let wait_deadline = Instant::now() + Duration::from_secs(2);
        for (pid, start) in [
            (pty_child_pid, pty_child_start),
            (pipe_child_pid, pipe_child_start),
        ] {
            while matches!(fl_host::process_identity(pid), Ok(ident) if ident.pid == pid && ident.start == start)
            {
                assert!(
                    Instant::now() < wait_deadline,
                    "lingering child {pid} survived"
                );
                thread::sleep(Duration::from_millis(15));
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        for (pid, start) in [
            (pty_child_pid, pty_child_start),
            (pipe_child_pid, pipe_child_start),
        ] {
            if matches!(fl_host::process_identity(pid), Ok(ident) if ident.pid == pid && ident.start == start)
            {
                let _ = Command::new("kill")
                    .args(["-KILL", &pid.to_string()])
                    .status();
            }
        }
    }
}
