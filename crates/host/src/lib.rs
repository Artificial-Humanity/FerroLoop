//! Pipe-child supervision.
//!
//! Linux without a delegated cgroup is best effort. The per-session supervisor is a
//! subreaper; a descendant that installs its own subreaper, and anything still running
//! after both that supervisor and the daemon are SIGKILLed, is a residual. macOS has no
//! subreaper and no cgroup. A `setsid` descendant reparented to launchd stays a residual.
//! This crate does not claim macOS adoption. The daemon reaps only the supervisor.

mod child;
mod env;
mod identity;
mod limits;
mod protocol;
mod refusal;
mod session;
#[cfg(unix)]
mod supervisor;
mod survivor_registry;

use std::collections::HashMap;
use std::fs::File;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use fl_host_proto::{
    ChildExit, HostConfig, HostEndpoint, HostError, HostListener, SessionEpoch, SessionId,
    SpawnSpec,
};

pub use child::SessionHandle;
use identity::{RecordedPid, SignalHook, cleanup_recorded};
pub use refusal::ContainmentRefusal;
use session::{Session, SessionParts};
use survivor_registry::{load_registry, publish, registry_path};

pub use child::{CleanupReport, DescendantExit};
pub use identity::{
    LeaderAnchor, ProcessIdentity, check_pidfd_support, process_identity,
    set_pidfd_kill_unavailable, set_pidfd_probe_unavailable, signal_anchored_group,
};
pub use limits::{CLEANUP_DEADLINE, HANDOFF_TIMEOUT, STARTUP_CLEANUP_DEADLINE, TERM_GRACE};
pub use refusal::refusal;
pub use survivor_registry::{
    CrashPoint, Registry, SessionPhase, SurvivorRecord, load_registry as read_registry,
    registry_path as state_file, write_raw_state, write_registry_fixture,
};

pub struct OpenOptions {
    pub config: HostConfig,
    pub supervisor_bin: PathBuf,
    pub crash_at: Option<CrashPoint>,
    pub recover_hold: Option<PathBuf>,
    /// Test seam. Publish succeeds, then the handshake fails so abort runs at once.
    pub fail_handshake_after_publish: bool,
    pub before_signal: Option<SignalHook>,
}

impl OpenOptions {
    pub fn new(config: HostConfig) -> Self {
        Self {
            config,
            supervisor_bin: std::env::current_exe().unwrap_or_default(),
            crash_at: None,
            recover_hold: None,
            fail_handshake_after_publish: false,
            before_signal: None,
        }
    }
}

pub struct Host {
    inner: Arc<HostInner>,
}

impl std::fmt::Debug for Host {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Host")
    }
}

struct HostInner {
    endpoint: HostEndpoint,
    path: PathBuf,
    registry: Mutex<Registry>,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    joins: Mutex<Vec<JoinHandle<()>>>,
    spawn_gate: Mutex<()>,
    listener: Mutex<Option<HostListener>>,
    /// Keeps the listener task polling after `bind` returns.
    runtime: tokio::runtime::Runtime,
    supervisor_bin: PathBuf,
    fail_handshake_after_publish: bool,
    crash: Mutex<Option<CrashPoint>>,
}

pub fn serve(config: HostConfig) -> Result<(), HostError> {
    try_backstop();
    let host = Host::open(OpenOptions::new(config))?;
    wait_for_stop()?;
    host.shutdown()
}

#[cfg(windows)]
pub fn current_user_sid() -> Result<String, HostError> {
    survivor_registry::current_sid_string()
}

pub fn session_supervisor() -> Result<(), HostError> {
    #[cfg(unix)]
    {
        supervisor::run()
    }
    #[cfg(not(unix))]
    {
        Err(HostError::Unsupported {
            subject: "session supervisor",
        })
    }
}

pub fn containment_worker() -> Result<(), HostError> {
    try_backstop();
    let path =
        std::env::var("FL_HOST_SOCK").map_err(|_| HostError::Protocol("FL_HOST_SOCK".into()))?;
    #[cfg(unix)]
    let endpoint = HostEndpoint::unix(path)?;
    #[cfg(windows)]
    let endpoint = worker_endpoint(&path)?;
    #[cfg(not(any(unix, windows)))]
    let endpoint = {
        let _ = path;
        return Err(HostError::Unsupported {
            subject: "containment worker",
        });
    };
    let mut opts = OpenOptions::new(HostConfig::new(endpoint));
    if let Some(bin) = std::env::var_os("FL_HOST_SUPERVISOR") {
        opts.supervisor_bin = PathBuf::from(bin);
    }
    if let Ok(text) = std::env::var("FL_HOST_CRASH_AT") {
        opts.crash_at = CrashPoint::parse(&text);
    }
    if let Some(dir) = std::env::var_os("FL_HOST_RECOVER_HOLD") {
        opts.recover_hold = Some(PathBuf::from(dir));
    }
    let host = Host::open(opts)?;
    writeln_out("ready")?;
    let stdin = std::io::stdin();
    let mut line = String::new();
    loop {
        line.clear();
        let n = std::io::BufRead::read_line(&mut stdin.lock(), &mut line)?;
        if n == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        match worker_command(&host, line) {
            Ok(text) => writeln_out(&text)?,
            Err(err) => writeln_out(&format!("err {err}"))?,
        }
    }
    host.shutdown()
}

#[cfg(windows)]
fn worker_endpoint(path: &str) -> Result<HostEndpoint, HostError> {
    let sid = survivor_registry::current_sid_string()?;
    let prefix = format!(r"\\.\pipe\fl-host-{sid}-");
    let suffix = path.strip_prefix(&prefix).unwrap_or(path);
    HostEndpoint::windows(suffix, &sid)
}

fn writeln_out(text: &str) -> Result<(), HostError> {
    use std::io::Write;
    let mut out = std::io::stdout();
    writeln!(out, "{text}")?;
    out.flush()?;
    Ok(())
}

fn worker_command(host: &Host, line: &str) -> Result<String, HostError> {
    let mut parts = line.splitn(2, ' ');
    let op = parts.next().unwrap_or("");
    let rest = parts.next().unwrap_or("");
    match op {
        "spawn" => {
            let spec: SpawnSpec =
                serde_json::from_str(rest).map_err(|err| HostError::Protocol(err.to_string()))?;
            let session = host.spawn(&spec)?;
            Ok(format!(
                "ok {} {} {} {} {} {} {}",
                session.session().as_str(),
                session.epoch().0,
                session.vendor_pid(),
                session.vendor_start(),
                session.supervisor_pid(),
                session.inner.supervisor_start,
                session.pgid()
            ))
        }
        "kill" => {
            let mut fields = rest.split_whitespace();
            let session = SessionId::new(fields.next().unwrap_or(""))?;
            let epoch: u64 = fields
                .next()
                .unwrap_or("0")
                .parse()
                .map_err(|_| HostError::Protocol("epoch".into()))?;
            host.kill(&session, SessionEpoch(epoch))?;
            Ok("ok".into())
        }
        "leader" => {
            let (session, timeout) = split_session_timeout(rest)?;
            let handle = host.session(&session)?;
            let exit = handle.wait_leader(timeout)?;
            Ok(format!("ok {:?} {:?}", exit.code, exit.signal))
        }
        "cleanup" => {
            let (session, timeout) = split_session_timeout(rest)?;
            let handle = host.session(&session)?;
            let report = handle.wait_cleanup(timeout)?;
            Ok(format!("ok {}", report.complete))
        }
        "stdout" => {
            let mut fields = rest.splitn(3, ' ');
            let session = SessionId::new(fields.next().unwrap_or(""))?;
            let millis: u64 = fields
                .next()
                .unwrap_or("5000")
                .parse()
                .map_err(|_| HostError::Protocol("timeout".into()))?;
            let needle = fields.next().unwrap_or("");
            let handle = host.session(&session)?;
            let bytes =
                handle.read_stdout_until(needle.as_bytes(), Duration::from_millis(millis))?;
            let text = String::from_utf8_lossy(&bytes).replace('\n', "\\n");
            Ok(format!("ok {text}"))
        }
        "epoch" => Ok(format!("ok {}", host.epoch_high_water())),
        "phase" => {
            let session = SessionId::new(rest.trim())?;
            Ok(format!("ok {}", phase_name(host.session_phase(&session))))
        }
        "alerts" => Ok(format!(
            "ok {}",
            serde_json::to_string(&host.alerts()).unwrap_or_else(|_| "[]".into())
        )),
        "pid" => Ok(format!("ok {}", std::process::id())),
        _ => Err(HostError::Protocol("worker command".into())),
    }
}

fn split_session_timeout(rest: &str) -> Result<(SessionId, Duration), HostError> {
    let mut fields = rest.split_whitespace();
    let session = SessionId::new(fields.next().unwrap_or(""))?;
    let millis: u64 = fields
        .next()
        .unwrap_or("5000")
        .parse()
        .map_err(|_| HostError::Protocol("timeout".into()))?;
    Ok((session, Duration::from_millis(millis)))
}

fn phase_name(phase: Option<SessionPhase>) -> &'static str {
    match phase {
        Some(SessionPhase::Running) => "running",
        Some(SessionPhase::LeaderExited) => "leader_exited",
        None => "absent",
    }
}

impl Host {
    pub fn open(opts: OpenOptions) -> Result<Self, HostError> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|err| HostError::Io(err.to_string()))?;
        let listener = runtime.block_on(HostListener::bind(opts.config))?;
        let endpoint = listener.endpoint().clone();
        let path = registry_path(&endpoint)?;
        let mut registry = load_registry(&endpoint)?;
        let deadline = Instant::now() + STARTUP_CLEANUP_DEADLINE;
        if let Err(err) = recover(
            &mut registry,
            &path,
            opts.before_signal.as_ref(),
            opts.recover_hold.as_deref(),
            deadline,
        ) {
            let _ = publish(&path, &registry, None);
            return Err(err);
        }
        Ok(Self {
            inner: Arc::new(HostInner {
                endpoint,
                path,
                registry: Mutex::new(registry),
                sessions: Mutex::new(HashMap::new()),
                joins: Mutex::new(Vec::new()),
                spawn_gate: Mutex::new(()),
                listener: Mutex::new(Some(listener)),
                runtime,
                supervisor_bin: opts.supervisor_bin,
                fail_handshake_after_publish: opts.fail_handshake_after_publish,
                crash: Mutex::new(opts.crash_at),
            }),
        })
    }

    pub fn endpoint(&self) -> &HostEndpoint {
        &self.inner.endpoint
    }

    pub fn epoch_high_water(&self) -> u64 {
        self.inner
            .registry
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .high_water
    }

    pub fn alerts(&self) -> Vec<String> {
        self.inner
            .registry
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .alerts
            .clone()
    }

    pub fn session_phase(&self, session: &SessionId) -> Option<SessionPhase> {
        self.inner
            .registry
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .sessions
            .iter()
            .find(|record| record.session == session.as_str())
            .map(|record| record.phase)
    }

    pub fn spawn(&self, spec: &SpawnSpec) -> Result<SessionHandle, HostError> {
        let _gate = self
            .inner
            .spawn_gate
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        spec.validate()?;
        if spec.kind == fl_host_proto::SpawnKind::Pty {
            return Err(HostError::Unsupported { subject: "pty" });
        }
        self.require_pidfd()?;
        let env = env::scrubbed_env(spec)?;
        let env = env
            .into_iter()
            .map(|(key, value)| {
                let key = key
                    .into_string()
                    .map_err(|_| HostError::Protocol("env name".into()))?;
                let value = value
                    .into_string()
                    .map_err(|_| HostError::Protocol("env value".into()))?;
                Ok((key, value))
            })
            .collect::<Result<Vec<_>, HostError>>()?;
        let epoch = {
            let registry = self
                .inner
                .registry
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            if registry
                .sessions
                .iter()
                .any(|record| record.session == spec.session.as_str())
            {
                return Err(ContainmentRefusal::SpawnRejected.into());
            }
            registry.alloc_epoch()?
        };
        #[cfg(unix)]
        {
            self.spawn_unix(spec, env, epoch)
        }
        #[cfg(windows)]
        {
            self.spawn_windows(spec, env, epoch)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (spec, env, epoch);
            Err(HostError::Unsupported { subject: "spawn" })
        }
    }

    pub fn kill(&self, session: &SessionId, epoch: SessionEpoch) -> Result<(), HostError> {
        let handle = self.session(session)?;
        if handle.epoch() != epoch {
            return Err(HostError::Protocol("stale epoch".into()));
        }
        handle.request_cleanup()
    }

    pub fn session(&self, session: &SessionId) -> Result<SessionHandle, HostError> {
        self.inner
            .sessions
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .get(session.as_str())
            .map(|inner| SessionHandle {
                inner: Arc::clone(inner),
            })
            .ok_or_else(|| HostError::Protocol("unknown session".into()))
    }

    pub fn shutdown(self) -> Result<(), HostError> {
        self.shutdown_inner();
        Ok(())
    }

    fn shutdown_inner(&self) {
        let sessions: Vec<_> = self
            .inner
            .sessions
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .values()
            .cloned()
            .collect();
        for session in &sessions {
            let _ = session.request_cleanup();
        }
        let deadline = Instant::now() + CLEANUP_DEADLINE;
        for session in &sessions {
            let _ = session.wait_cleanup(deadline.saturating_duration_since(Instant::now()));
        }
        let _runtime = &self.inner.runtime;
        let joins = std::mem::take(
            &mut *self
                .inner
                .joins
                .lock()
                .unwrap_or_else(|err| err.into_inner()),
        );
        for join in joins {
            let _ = join.join();
        }
        self.inner
            .listener
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .take();
    }

    fn require_pidfd(&self) -> Result<(), HostError> {
        let _ = self.inner.endpoint.pipe_name();
        #[cfg(target_os = "linux")]
        {
            check_pidfd_support()?;
        }
        Ok(())
    }

    #[cfg(unix)]
    fn spawn_unix(
        &self,
        spec: &SpawnSpec,
        env: Vec<(String, String)>,
        epoch: u64,
    ) -> Result<SessionHandle, HostError> {
        let host = Arc::clone(&self.inner);
        let crash = host
            .crash
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .take();
        let path = host.path.clone();
        let fail_after_publish = host.fail_handshake_after_publish;
        let supervised = child::unix::spawn_supervised(
            &host.supervisor_bin,
            spec.argv.clone(),
            &spec.cwd,
            env,
            spec.session.as_str(),
            epoch,
            |vendor_pid, vendor_start, supervisor_pid, supervisor_start, pgid| {
                let record = SurvivorRecord {
                    session: spec.session.as_str().to_owned(),
                    epoch,
                    phase: SessionPhase::Running,
                    supervisor_pid,
                    supervisor_start,
                    vendor_pid,
                    vendor_start,
                    pgid,
                    group_leader_pid: vendor_pid,
                    group_leader_start: vendor_start,
                    group_leader_reaped: false,
                    leader_code: None,
                    leader_signal: None,
                };
                let mut registry = host.registry.lock().unwrap_or_else(|err| err.into_inner());
                registry.high_water = epoch;
                registry.sessions.push(record);
                publish(&path, &registry, crash)
            },
            fail_after_publish,
        );
        let supervised = match supervised {
            Ok(spawned) => spawned,
            Err(err) => {
                // Pidfd abort could not prove both processes. Keep the durable record.
                if refusal(&err) != Some(ContainmentRefusal::CleanupUnresolved) {
                    self.retire_epoch_keep(spec.session.as_str(), epoch);
                }
                return Err(err);
            }
        };
        let session = Arc::new(Session::new(SessionParts {
            session: spec.session.clone(),
            epoch,
            vendor_pid: supervised.vendor_pid,
            vendor_start: supervised.vendor_start,
            supervisor_pid: supervised.supervisor_pid,
            supervisor_start: supervised.supervisor_start,
            pgid: supervised.pgid,
            control_inode: supervised.control_inode,
            control: Some(supervised.control),
            stdin: Some(supervised.stdin),
        }));
        session.set_supervisor(supervised.child);
        self.arm_unix(
            Arc::clone(&session),
            supervised.status,
            supervised.stdout,
            supervised.stderr,
        );
        self.inner
            .sessions
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .insert(spec.session.as_str().to_owned(), Arc::clone(&session));
        Ok(SessionHandle { inner: session })
    }

    #[cfg(unix)]
    fn arm_unix(&self, session: Arc<Session>, status: File, stdout: File, stderr: File) {
        let host = Arc::clone(&self.inner);
        let watched = Arc::clone(&session);
        self.spawn_thread(move || unix_status(status, watched, host));
        let pumped = Arc::clone(&session);
        self.spawn_thread(move || session::pump_stdout(stdout, pumped));
        let pumped = Arc::clone(&session);
        self.spawn_thread(move || session::pump_stderr(stderr, pumped));
        let waited = Arc::clone(&session);
        // The status thread owns cleanup reports. This thread only reaps the supervisor.
        self.spawn_thread(move || {
            if let Some(mut child) = waited.take_supervisor() {
                let _ = child.wait();
            }
        });
    }

    #[cfg(windows)]
    fn spawn_windows(
        &self,
        spec: &SpawnSpec,
        env: Vec<(String, String)>,
        epoch: u64,
    ) -> Result<SessionHandle, HostError> {
        let host = Arc::clone(&self.inner);
        let crash = host
            .crash
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .take();
        let path = host.path.clone();
        let spawned = child::windows::spawn_in_job(
            &spec.argv,
            &spec.cwd,
            &env,
            crash,
            |vendor_pid, vendor_start| {
                let record = SurvivorRecord {
                    session: spec.session.as_str().to_owned(),
                    epoch,
                    phase: SessionPhase::Running,
                    supervisor_pid: 0,
                    supervisor_start: 0,
                    vendor_pid,
                    vendor_start,
                    pgid: vendor_pid,
                    group_leader_pid: vendor_pid,
                    group_leader_start: vendor_start,
                    group_leader_reaped: false,
                    leader_code: None,
                    leader_signal: None,
                };
                let mut registry = host.registry.lock().unwrap_or_else(|err| err.into_inner());
                registry.high_water = epoch;
                registry.sessions.push(record);
                publish(&path, &registry, None)
            },
        );
        let spawned = match spawned {
            Ok(spawned) => spawned,
            Err(err) => {
                self.retire_epoch_keep(spec.session.as_str(), epoch);
                return Err(err);
            }
        };
        let session = Arc::new(Session::new(SessionParts {
            session: spec.session.clone(),
            epoch,
            vendor_pid: spawned.vendor_pid,
            vendor_start: spawned.vendor_start,
            supervisor_pid: 0,
            supervisor_start: 0,
            pgid: spawned.vendor_pid,
            control_inode: 0,
            control: None,
            stdin: Some(spawned.stdin),
        }));
        *session.job.lock().unwrap_or_else(|err| err.into_inner()) = Some(spawned.job);
        *session
            .process
            .lock()
            .unwrap_or_else(|err| err.into_inner()) = Some(spawned.process);
        self.arm_windows(Arc::clone(&session), spawned.stdout, spawned.stderr);
        self.inner
            .sessions
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .insert(spec.session.as_str().to_owned(), Arc::clone(&session));
        Ok(SessionHandle { inner: session })
    }

    #[cfg(windows)]
    fn arm_windows(&self, session: Arc<Session>, stdout: File, stderr: File) {
        let host = Arc::clone(&self.inner);
        let watched = Arc::clone(&session);
        self.spawn_thread(move || windows_watch(watched, host));
        let pumped = Arc::clone(&session);
        self.spawn_thread(move || session::pump_stdout(stdout, pumped));
        let pumped = Arc::clone(&session);
        self.spawn_thread(move || session::pump_stderr(stderr, pumped));
    }

    fn spawn_thread(&self, work: impl FnOnce() + Send + 'static) {
        self.inner
            .joins
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .push(std::thread::spawn(work));
    }

    fn retire_epoch_keep(&self, session: &str, epoch: u64) {
        let mut registry = self
            .inner
            .registry
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let had = registry
            .sessions
            .iter()
            .any(|record| record.session == session);
        registry.sessions.retain(|record| record.session != session);
        if registry.high_water < epoch {
            registry.high_water = epoch;
        }
        if had || registry.high_water == epoch {
            let _ = publish(&self.inner.path, &registry, None);
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.shutdown_inner();
    }
}

impl HostInner {
    fn alert(&self, text: &str) {
        let mut registry = self.registry.lock().unwrap_or_else(|err| err.into_inner());
        registry.alerts.push(text.to_owned());
        let _ = publish(&self.path, &registry, None);
    }

    fn mark_leader(&self, session: &str, exit: &ChildExit) {
        let mut registry = self.registry.lock().unwrap_or_else(|err| err.into_inner());
        if let Some(record) = registry
            .sessions
            .iter_mut()
            .find(|record| record.session == session)
        {
            record.phase = SessionPhase::LeaderExited;
            record.group_leader_reaped = true;
            record.leader_code = exit.code;
            record.leader_signal = exit.signal;
        }
        let _ = publish(&self.path, &registry, None);
    }

    fn retire(&self, session: &str) {
        let mut registry = self.registry.lock().unwrap_or_else(|err| err.into_inner());
        registry.sessions.retain(|record| record.session != session);
        let _ = publish(&self.path, &registry, None);
    }
}

#[cfg(unix)]
fn unix_status(mut status: File, session: Arc<Session>, host: Arc<HostInner>) {
    while let Ok(Some(line)) = child::unix::read_line_blocking(&mut status) {
        let Ok(message) = protocol::from_line::<protocol::SupervisorMsg>(&line) else {
            continue;
        };
        match message {
            protocol::SupervisorMsg::Leader { code, signal } => {
                let exit = ChildExit { code, signal };
                host.mark_leader(session.session.as_str(), &exit);
                session.note_leader(exit);
            }
            protocol::SupervisorMsg::Descendant {
                pid,
                code,
                signal,
                adopted,
            } => {
                session.note_descendant(DescendantExit {
                    pid,
                    code,
                    signal,
                    adopted,
                });
            }
            protocol::SupervisorMsg::Cleanup { complete } => {
                if complete {
                    host.retire(session.session.as_str());
                } else {
                    host.alert("cleanup unresolved");
                }
                session.note_cleanup(CleanupReport { complete });
                break;
            }
            _ => {}
        }
    }
    if session.cleanup_report().is_none() {
        host.alert("cleanup unresolved");
        session.note_cleanup(CleanupReport { complete: false });
    }
}

#[cfg(windows)]
fn windows_watch(session: Arc<Session>, host: Arc<HostInner>) {
    // The observation drops its guards before the sleep, so TerminateJobObject can lock the Job.
    windows_watch_until(session, host, observe_windows_job);
}

#[cfg(windows)]
fn observe_windows_job(session: &Session) -> WatchTick {
    let process = session
        .process
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    let job = session.job.lock().unwrap_or_else(|err| err.into_inner());
    let Some(process) = process.as_ref() else {
        return WatchTick::Stop;
    };
    let Some(job) = job.as_ref() else {
        return WatchTick::Stop;
    };
    if session.leader_exit().is_none() && child::windows::leader_signaled(process) {
        return WatchTick::Leader(child::windows::leader_exit_code(process));
    }
    let requested = session.cleanup_requested();
    // Hold forces accounting to look live. A non-zero Job is not cleanup.
    let quiet = !job_hold_active() && child::windows::stable_job_quiescent(job);
    WatchTick::Poll { requested, quiet }
}

/// Poll until cleanup is resolved or the post-request deadline expires.
///
/// Unresolved cleanup leaves this loop so `shutdown` can join the thread. The
/// `Session` keeps the Job until the host drops that session; this loop does not.
#[cfg(any(windows, test))]
fn windows_watch_until(
    session: Arc<Session>,
    host: Arc<HostInner>,
    mut observe: impl FnMut(&Session) -> WatchTick,
) {
    let mut cleanup_started: Option<Instant> = None;
    loop {
        match observe(&session) {
            WatchTick::Stop => break,
            WatchTick::Leader(code) => {
                let exit = ChildExit { code, signal: None };
                host.mark_leader(session.session.as_str(), &exit);
                session.note_leader(exit);
                continue;
            }
            WatchTick::Poll { requested, quiet } => {
                if requested && cleanup_started.is_none() {
                    cleanup_started = Some(Instant::now());
                }
                if quiet && (requested || session.leader_exit().is_some()) {
                    host.retire(session.session.as_str());
                    session.note_cleanup(CleanupReport { complete: true });
                    break;
                }
                if requested && !quiet && cleanup_deadline_expired(cleanup_started, Instant::now())
                {
                    host.alert("cleanup unresolved");
                    session.note_cleanup(CleanupReport { complete: false });
                    break;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(any(windows, test))]
enum WatchTick {
    #[cfg_attr(not(windows), allow(dead_code))]
    Leader(Option<i32>),
    Poll {
        requested: bool,
        quiet: bool,
    },
    #[cfg_attr(not(windows), allow(dead_code))]
    Stop,
}

#[cfg(windows)]
fn job_hold_active() -> bool {
    std::env::var_os("FL_HOST_JOB_HOLD")
        .is_some_and(|path| !path.is_empty() && std::path::Path::new(&path).exists())
}

/// The cleanup clock is absent until cleanup is requested. A process that has already
/// run longer than [`CLEANUP_DEADLINE`] has not used up that budget.
#[cfg(any(windows, test))]
fn cleanup_deadline_expired(started: Option<Instant>, now: Instant) -> bool {
    started.is_some_and(|started| now.saturating_duration_since(started) > CLEANUP_DEADLINE)
}

fn recover(
    registry: &mut Registry,
    path: &std::path::Path,
    hook: Option<&SignalHook>,
    hold: Option<&std::path::Path>,
    deadline: Instant,
) -> Result<(), HostError> {
    let original = registry.sessions.clone();
    let mut kept = Vec::new();
    let mut failure: Option<ContainmentRefusal> = None;
    for session in original {
        if let Some(dir) = hold {
            let hold_path = dir.join(format!("hold-{}", session.session));
            let go_path = dir.join(format!("go-{}", session.session));
            std::fs::write(&hold_path, b"1").map_err(|err| HostError::Io(err.to_string()))?;
            while !go_path.exists() {
                if Instant::now() >= deadline {
                    failure = Some(ContainmentRefusal::CleanupUnresolved);
                    kept.push(session.clone());
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            if Instant::now() >= deadline && !go_path.exists() {
                continue;
            }
        }
        let targets = [
            RecordedPid {
                pid: session.supervisor_pid,
                start: session.supervisor_start,
            },
            RecordedPid {
                pid: session.vendor_pid,
                start: session.vendor_start,
            },
        ];
        match cleanup_recorded(&targets, hook, deadline) {
            Ok(()) => {}
            Err(err) => {
                registry.alerts.push(err.as_str().to_owned());
                kept.push(session);
                failure = Some(err);
            }
        }
    }
    registry.sessions = kept;
    publish(path, registry, None)?;
    match failure {
        Some(err) => Err(err.into()),
        None => Ok(()),
    }
}

fn try_backstop() {
    #[cfg(windows)]
    child::windows::backstop();
}

fn wait_for_stop() -> Result<(), HostError> {
    #[cfg(unix)]
    {
        unsafe {
            let mut set = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, libc::SIGINT);
            libc::sigaddset(&mut set, libc::SIGTERM);
            libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
            let mut sig = 0;
            if libc::sigwait(&set, &mut sig) != 0 {
                return Err(HostError::Io("sigwait".into()));
            }
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::thread::park();
        Ok(())
    }
}

#[cfg(test)]
mod deadline_tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use super::{
        CLEANUP_DEADLINE, CleanupReport, Host, OpenOptions, Session, SessionParts, SessionPhase,
        SurvivorRecord, WatchTick, cleanup_deadline_expired, load_registry, windows_watch_until,
    };
    use fl_host_proto::{HostConfig, HostEndpoint, SessionId};

    #[test]
    fn cleanup_deadline_starts_only_when_requested() {
        let now = Instant::now();
        let running_since = now
            .checked_sub(CLEANUP_DEADLINE + Duration::from_secs(30))
            .expect("instant");
        assert!(!cleanup_deadline_expired(None, now));
        assert!(!cleanup_deadline_expired(Some(now), now));
        assert!(cleanup_deadline_expired(Some(running_since), now));
    }

    #[test]
    fn shutdown_returns_after_unresolved_windows_cleanup() {
        let dir = tempfile::tempdir().expect("tempdir");
        let endpoint = unresolved_endpoint(dir.path());
        let host = Host::open(OpenOptions::new(HostConfig::new(endpoint.clone()))).expect("open");
        let session = Arc::new(Session::new(SessionParts {
            session: SessionId::new("held").expect("session"),
            epoch: 1,
            vendor_pid: 1,
            vendor_start: 1,
            supervisor_pid: 0,
            supervisor_start: 0,
            pgid: 1,
            control_inode: 0,
            control: None,
            stdin: None,
        }));
        {
            let mut registry = host
                .inner
                .registry
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            registry.high_water = 1;
            registry.sessions.push(SurvivorRecord {
                session: "held".to_owned(),
                epoch: 1,
                phase: SessionPhase::Running,
                supervisor_pid: 0,
                supervisor_start: 0,
                vendor_pid: 1,
                vendor_start: 1,
                pgid: 1,
                group_leader_pid: 1,
                group_leader_start: 1,
                group_leader_reaped: false,
                leader_code: None,
                leader_signal: None,
            });
        }
        host.inner
            .sessions
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .insert("held".to_owned(), Arc::clone(&session));
        let watched = Arc::clone(&session);
        let inner = Arc::clone(&host.inner);
        host.spawn_thread(move || {
            // Never quiet: accounting stays non-zero, so this must not become cleanup.
            windows_watch_until(watched, inner, |session| WatchTick::Poll {
                requested: session.cleanup_requested(),
                quiet: false,
            });
        });
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let started = Instant::now();
            let result = host.shutdown();
            let _ = tx.send((started.elapsed(), result.is_ok()));
        });
        let (elapsed, ok) = rx
            .recv_timeout(CLEANUP_DEADLINE + Duration::from_secs(3))
            .expect("shutdown joined unresolved windows cleanup forever");
        assert!(ok);
        assert!(
            elapsed >= CLEANUP_DEADLINE,
            "cleanup clock was already running"
        );
        assert_eq!(
            session.cleanup_report(),
            Some(CleanupReport { complete: false })
        );
        assert_eq!(Arc::strong_count(&session), 1);
        let registry = load_registry(&endpoint).expect("registry");
        assert!(
            registry
                .alerts
                .iter()
                .any(|alert| alert == "cleanup unresolved")
        );
        assert!(
            registry
                .sessions
                .iter()
                .any(|record| record.session == "held")
        );
    }

    fn unresolved_endpoint(dir: &std::path::Path) -> HostEndpoint {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).expect("mode");
            HostEndpoint::unix(dir.join("h.sock")).expect("endpoint")
        }
        #[cfg(windows)]
        {
            let _ = dir;
            let suffix = format!("unres-{}", std::process::id());
            HostEndpoint::windows(&suffix, &super::current_user_sid().expect("sid"))
                .expect("endpoint")
        }
        #[cfg(not(any(unix, windows)))]
        {
            HostEndpoint::unix(dir.join("h.sock")).expect("endpoint")
        }
    }
}
