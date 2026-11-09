//! Pipe-child supervision.
//!
//! Linux without a delegated cgroup is best effort. The per-session supervisor is a
//! subreaper; a descendant that installs its own subreaper, and anything still running
//! after both that supervisor and the daemon are SIGKILLed, is a residual. macOS has no
//! subreaper and no cgroup. A `setsid` descendant reparented to launchd stays a residual.
//! This crate does not claim macOS adoption. The daemon reaps only the supervisor.

mod authority;
mod child;
mod daemon;
mod env;
mod identity;
mod input;
mod limits;
mod output;
mod projection;
#[cfg(unix)]
mod protocol;
mod refusal;
mod reliable;
mod retention;
mod session;
mod slice_budget;
#[cfg(unix)]
mod supervisor;
mod survivor_registry;

use std::collections::HashMap;
use std::fs::File;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
pub use limits::{
    CLEANUP_DEADLINE, HANDOFF_TIMEOUT, HOSTED_LEADER_EXIT_GRACE, STARTUP_CLEANUP_DEADLINE,
    TERM_GRACE,
};
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
    runtime: Option<tokio::runtime::Runtime>,
}

impl std::fmt::Debug for Host {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Host")
    }
}

type ThreadJoin = (Option<(String, u64)>, JoinHandle<()>);

pub(crate) struct HostInner {
    build_info: fl_host_proto::HostBuildInfo,
    endpoint: HostEndpoint,
    path: PathBuf,
    registry: Mutex<Registry>,
    pub(crate) sessions: Mutex<HashMap<String, Arc<Session>>>,
    joins: Mutex<Vec<ThreadJoin>>,
    spawn_gate: Mutex<()>,
    retention: Mutex<retention::Retention>,
    pub(crate) listener: Mutex<Option<HostListener>>,
    pub(crate) handle: tokio::runtime::Handle,
    pub(crate) stop: AtomicBool,
    pub(crate) wake: tokio::sync::Notify,
    pub(crate) hub: Arc<output::Hub>,
    reliable_budget: Arc<slice_budget::SliceBudget>,
    pub(crate) authority: Arc<authority::AuthorityRegistry>,
    pub(crate) writers: Mutex<HashMap<String, Arc<input::WriterHandle>>>,
    pub(crate) outputs: Mutex<HashMap<(String, u64), Arc<output::OutputSession>>>,
    pub(crate) next_conn: AtomicU64,
    shutdown_started: AtomicBool,
    #[cfg(unix)]
    supervisor_bin: PathBuf,
    #[cfg(unix)]
    fail_handshake_after_publish: bool,
    crash: Mutex<Option<CrashPoint>>,
}

/// Standalone host build stamp. Embedding binaries must pass their own stamp to
/// `serve_with_build_info` so all of their compiled sources determine dirty state.
pub fn build_info() -> fl_host_proto::HostBuildInfo {
    fl_host_proto::HostBuildInfo {
        version: env!("CARGO_PKG_VERSION").into(),
        commit: env!("FL_BUILD_COMMIT").into(),
        dirty: match env!("FL_BUILD_DIRTY") {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        },
        build_time_unix: env!("FL_BUILD_TIME").parse().ok(),
    }
}

pub fn serve(config: HostConfig) -> Result<(), HostError> {
    serve_with_build_info(config, build_info())
}

pub fn serve_with_build_info(
    config: HostConfig,
    build_info: fl_host_proto::HostBuildInfo,
) -> Result<(), HostError> {
    try_backstop();
    block_stop_signals();
    let host = Host::open_with_build_info(OpenOptions::new(config), build_info)?;
    host.inner
        .handle
        .spawn(daemon::accept_loop(Arc::clone(&host.inner)));
    let stopped = wait_for_stop();
    host.shutdown()?;
    stopped
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
    let endpoint = socket_endpoint(&path)?;
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

/// Parse the same endpoint syntax for daemon and containment worker.
pub fn socket_endpoint(path: &str) -> Result<HostEndpoint, HostError> {
    #[cfg(windows)]
    {
        windows_socket_endpoint(path, &survivor_registry::current_sid_string()?)
    }
    #[cfg(not(windows))]
    {
        HostEndpoint::unix(path)
    }
}

#[cfg(any(windows, test))]
fn windows_socket_endpoint(path: &str, sid: &str) -> Result<HostEndpoint, HostError> {
    let prefix = format!(r"\\.\pipe\fl-host-{sid}-");
    HostEndpoint::windows(path.strip_prefix(&prefix).unwrap_or(path), sid)
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
        Self::open_with_build_info(opts, build_info())
    }

    fn open_with_build_info(
        opts: OpenOptions,
        build_info: fl_host_proto::HostBuildInfo,
    ) -> Result<Self, HostError> {
        build_info.validate()?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
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
        let handle = runtime.handle().clone();
        let reliable_budget = slice_budget::SliceBudget::new();
        let hub = Arc::new(output::Hub::new());
        let authority =
            authority::AuthorityRegistry::new(Arc::clone(&reliable_budget), Arc::clone(&hub))
                .map_err(|_| HostError::Protocol("authority admission".into()))?;
        let retention = retention::Retention::new(&reliable_budget)
            .map_err(|_| HostError::Protocol("retention admission".into()))?;
        let host = Self {
            runtime: Some(runtime),
            inner: Arc::new(HostInner {
                build_info,
                reliable_budget,
                authority,
                endpoint,
                path,
                registry: Mutex::new(registry),
                sessions: Mutex::new(HashMap::new()),
                joins: Mutex::new(Vec::new()),
                spawn_gate: Mutex::new(()),
                retention: Mutex::new(retention),
                listener: Mutex::new(Some(listener)),
                handle,
                stop: AtomicBool::new(false),
                wake: tokio::sync::Notify::new(),
                hub,
                writers: Mutex::new(HashMap::new()),
                outputs: Mutex::new(HashMap::new()),
                next_conn: AtomicU64::new(1),
                shutdown_started: AtomicBool::new(false),
                #[cfg(unix)]
                supervisor_bin: opts.supervisor_bin,
                #[cfg(unix)]
                fail_handshake_after_publish: opts.fail_handshake_after_publish,
                crash: Mutex::new(opts.crash_at),
            }),
        };
        host.inner.authority.bind(&host.inner);
        Ok(host)
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
        Self::spawn_session(&self.inner, spec)
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

    pub fn shutdown(mut self) -> Result<(), HostError> {
        self.shutdown_inner();
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_timeout(Duration::from_millis(500));
        }
        Ok(())
    }

    fn shutdown_inner(&self) {
        if self.inner.shutdown_started.swap(true, Ordering::SeqCst) {
            return;
        }
        // A spawn holds this gate through publication and thread registration.
        // Shutdown cannot snapshot until every admitted spawn has settled.
        let gate = self
            .inner
            .spawn_gate
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        self.inner.stop.store(true, Ordering::SeqCst);
        self.inner.authority.stop();
        drop(gate);
        self.inner.wake.notify_one();
        self.inner.hub.shutdown();
        let writers: Vec<_> = self
            .inner
            .writers
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .values()
            .cloned()
            .collect();
        for writer in &writers {
            writer.shutdown();
        }
        let outputs: Vec<_> = self
            .inner
            .outputs
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .values()
            .cloned()
            .collect();
        for output in &outputs {
            output.request_stop();
        }
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
        // Historical epochs can still own blocked pipe readers after same-name
        // replacement. Cancel every retained epoch before joining its threads.
        for output in &outputs {
            output.stop_pumps();
        }
        let joins = std::mem::take(
            &mut *self
                .inner
                .joins
                .lock()
                .unwrap_or_else(|err| err.into_inner()),
        );
        for (_, join) in joins {
            let _ = join.join();
        }
        self.inner
            .writers
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clear();
        self.inner
            .outputs
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clear();
        self.inner
            .listener
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .take();
    }

    pub(crate) fn spawn_session(
        this: &Arc<HostInner>,
        spec: &SpawnSpec,
    ) -> Result<SessionHandle, HostError> {
        Self::spawn_mode(this, spec, false)
    }

    pub(crate) fn spawn_mode(
        this: &Arc<HostInner>,
        spec: &SpawnSpec,
        reliable: bool,
    ) -> Result<SessionHandle, HostError> {
        let mode = if reliable {
            fl_host_proto::SessionMode::ReliablePipe
        } else {
            fl_host_proto::SessionMode::LegacyPipe
        };
        Self::spawn_session_mode(this, spec, mode, None)
    }

    pub(crate) fn spawn_session_mode(
        this: &Arc<HostInner>,
        spec: &SpawnSpec,
        mode: fl_host_proto::SessionMode,
        pty: Option<fl_host_proto::PtyOptions>,
    ) -> Result<SessionHandle, HostError> {
        Self::admit_spawn(this, |notices| {
            Self::spawn_admitted(this, spec, mode, pty, notices)
        })
    }

    fn admit_spawn<T>(
        this: &HostInner,
        spawn: impl FnOnce(&mut retention::EvictionNotices) -> Result<T, HostError>,
    ) -> Result<T, HostError> {
        this.with_spawn_gate(|notices| {
            if this.shutdown_started.load(Ordering::SeqCst) || this.stop.load(Ordering::SeqCst) {
                return Err(HostError::Protocol("host shutting down".into()));
            }
            spawn(notices)
        })
    }

    fn spawn_admitted(
        this: &Arc<HostInner>,
        spec: &SpawnSpec,
        mode: fl_host_proto::SessionMode,
        pty: Option<fl_host_proto::PtyOptions>,
        notices: &mut retention::EvictionNotices,
    ) -> Result<SessionHandle, HostError> {
        spec.validate_fields()?;
        match (mode, spec.kind, pty) {
            (
                fl_host_proto::SessionMode::LegacyPipe | fl_host_proto::SessionMode::ReliablePipe,
                fl_host_proto::SpawnKind::Pipe,
                None,
            ) => {}
            (
                fl_host_proto::SessionMode::LegacyPipe | fl_host_proto::SessionMode::ReliablePipe,
                fl_host_proto::SpawnKind::Pty,
                _,
            ) => {
                return Err(HostError::Unsupported { subject: "pty" });
            }
            (fl_host_proto::SessionMode::Pty, fl_host_proto::SpawnKind::Pty, _) => {
                #[cfg(not(unix))]
                {
                    return Err(HostError::Unsupported { subject: "pty" });
                }
                #[cfg(unix)]
                if let Some(opts) = pty {
                    opts.validate()?;
                }
            }
            _ => {
                return Err(HostError::Protocol(
                    "contradictory session mode/options".into(),
                ));
            }
        }
        let _ = this.endpoint.pipe_name();
        #[cfg(target_os = "linux")]
        {
            check_pidfd_support()?;
        }
        this.reclaim_completed(notices);
        this.evict_completed_ptys(true, notices);
        // Every retained epoch consumes capacity, including unread completed
        // pipe output and unresolved containment. Completed PTYs were bounded above.
        let retained = this
            .outputs
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .len();
        let unresolved = this
            .registry
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .sessions
            .iter()
            .filter(|record| {
                !this
                    .outputs
                    .lock()
                    .unwrap_or_else(|err| err.into_inner())
                    .contains_key(&(record.session.clone(), record.epoch))
            })
            .count();
        if retained.saturating_add(unresolved) >= fl_host_proto::DEFAULT_MAX_SESSIONS {
            return Err(HostError::Protocol("session limit".into()));
        }
        {
            let sessions = this.sessions.lock().unwrap_or_else(|err| err.into_inner());
            if sessions.get(spec.session.as_str()).is_some_and(|session| {
                !matches!(
                    session.state(),
                    fl_host_proto::ChildState::CleanupComplete(_)
                )
            }) {
                return Err(ContainmentRefusal::SpawnRejected.into());
            }
        }
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
            let registry = this.registry.lock().unwrap_or_else(|err| err.into_inner());
            if registry
                .sessions
                .iter()
                .any(|record| record.session == spec.session.as_str())
            {
                return Err(ContainmentRefusal::SpawnRejected.into());
            }
            registry.alloc_epoch()?
        };
        let admission = if mode != fl_host_proto::SessionMode::LegacyPipe {
            let stream = fl_host_proto::StreamId {
                host: this.authority.instance().clone(),
                session: spec.session.clone(),
                epoch: fl_host_proto::SessionEpoch(epoch),
            };
            let journal = this
                .reserve_reliable_journal_with_mode(stream, mode == fl_host_proto::SessionMode::Pty)
                .map_err(|_| HostError::Protocol("reliable capacity".into()))?;
            Some(
                this.authority
                    .prepare_epoch(journal)
                    .map_err(|_| HostError::Protocol("authority capacity".into()))?,
            )
        } else {
            None
        };
        #[cfg(unix)]
        {
            Self::spawn_unix(this, spec, pty, env, epoch, admission)
        }
        #[cfg(windows)]
        {
            let _ = pty;
            Self::spawn_windows(this, spec, env, epoch, admission)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (this, spec, pty, env, epoch);
            Err(HostError::Unsupported { subject: "spawn" })
        }
    }

    #[cfg(unix)]
    fn spawn_unix(
        this: &Arc<HostInner>,
        spec: &SpawnSpec,
        pty: Option<fl_host_proto::PtyOptions>,
        env: Vec<(String, String)>,
        epoch: u64,
        admission: Option<authority::PreparedEpoch>,
    ) -> Result<SessionHandle, HostError> {
        // The epoch owns all parser storage before any child exists. Failure
        // simply drops admission; it never needs post-spawn cleanup.
        let projection = match admission.as_ref().filter(|a| a.journal.pty_mode()) {
            Some(admission) => Some(
                projection::Projection::new(
                    Arc::clone(admission.journal.budget()),
                    pty.unwrap_or_default(),
                )
                .map_err(|_| HostError::Protocol("projection capacity".into()))?,
            ),
            None => None,
        };
        let host = Arc::clone(this);
        let crash = host
            .crash
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .take();
        let path = host.path.clone();
        let fail_after_publish = host.fail_handshake_after_publish;
        let hosted_owner = explicit_hosted_owner(spec, &env).map(str::to_owned);
        let supervised = child::unix::spawn_supervised(
            &host.supervisor_bin,
            spec.kind,
            pty,
            spec.argv.clone(),
            &spec.cwd,
            env,
            spec.session.as_str(),
            epoch,
            hosted_owner.is_some(),
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
                    Self::retire_epoch_keep(this, spec.session.as_str(), epoch);
                }
                return Err(err);
            }
        };
        let parts = SessionParts {
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
        };
        let session = Arc::new(
            (match (admission.as_ref(), projection) {
                (Some(admission), Some(projection)) => Session::with_projection(
                    parts,
                    Arc::clone(&admission.journal),
                    projection,
                    supervised.resize_master,
                ),
                (Some(admission), None) => {
                    Session::with_reliable(parts, Some(Arc::clone(&admission.journal)))
                }
                (None, None) => Session::new(parts),
                (None, Some(_)) => unreachable!("projection requires an epoch journal"),
            })
            .with_hosted_owner(hosted_owner.as_deref()),
        );
        session.set_supervisor(supervised.child);
        this.sessions
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .insert(spec.session.as_str().to_owned(), Arc::clone(&session));
        daemon::attach(this, &session, admission);
        Self::arm_unix(
            this,
            Arc::clone(&session),
            supervised.status,
            supervised.stdout,
            supervised.stderr,
        );
        Ok(SessionHandle { inner: session })
    }

    #[cfg(unix)]
    fn arm_unix(
        this: &Arc<HostInner>,
        session: Arc<Session>,
        status: File,
        stdout: File,
        stderr: File,
    ) {
        let host = Arc::clone(this);
        let watched = Arc::clone(&session);
        this.spawn_epoch_thread(&session, move || unix_status(status, watched, host));
        let pumped = Arc::clone(&session);
        this.spawn_epoch_thread(&session, move || session::pump_stdout(stdout, pumped));
        let pumped = Arc::clone(&session);
        this.spawn_epoch_thread(&session, move || session::pump_stderr(stderr, pumped));
        let waited = Arc::clone(&session);
        // The status thread owns cleanup reports. This thread only reaps the supervisor.
        this.spawn_epoch_thread(&session, move || {
            if let Some(mut child) = waited.take_supervisor() {
                let _ = child.wait();
            }
        });
    }

    #[cfg(windows)]
    fn spawn_windows(
        this: &Arc<HostInner>,
        spec: &SpawnSpec,
        env: Vec<(String, String)>,
        epoch: u64,
        admission: Option<authority::PreparedEpoch>,
    ) -> Result<SessionHandle, HostError> {
        let host = Arc::clone(this);
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
                Self::retire_epoch_keep(this, spec.session.as_str(), epoch);
                return Err(err);
            }
        };
        let parts = SessionParts {
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
        };
        let hosted_owner = explicit_hosted_owner(spec, &env);
        let session = Arc::new(
            (match admission.as_ref() {
                Some(admission) => {
                    Session::with_reliable(parts, Some(Arc::clone(&admission.journal)))
                }
                None => Session::new(parts),
            })
            .with_hosted_owner(hosted_owner),
        );
        *session.job.lock().unwrap_or_else(|err| err.into_inner()) = Some(spawned.job);
        *session
            .process
            .lock()
            .unwrap_or_else(|err| err.into_inner()) = Some(spawned.process);
        this.sessions
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .insert(spec.session.as_str().to_owned(), Arc::clone(&session));
        daemon::attach(this, &session, admission);
        Self::arm_windows(this, Arc::clone(&session), spawned.stdout, spawned.stderr);
        Ok(SessionHandle { inner: session })
    }

    #[cfg(windows)]
    fn arm_windows(this: &Arc<HostInner>, session: Arc<Session>, stdout: File, stderr: File) {
        let host = Arc::clone(this);
        let watched = Arc::clone(&session);
        this.spawn_epoch_thread(&session, move || windows_watch(watched, host));
        let pumped = Arc::clone(&session);
        this.spawn_epoch_thread(&session, move || session::pump_stdout(stdout, pumped));
        let pumped = Arc::clone(&session);
        this.spawn_epoch_thread(&session, move || session::pump_stderr(stderr, pumped));
    }

    #[cfg(test)]
    fn spawn_thread(&self, work: impl FnOnce() + Send + 'static) {
        self.inner.spawn_thread(work);
    }

    fn retire_epoch_keep(this: &Arc<HostInner>, session: &str, epoch: u64) {
        let mut registry = this.registry.lock().unwrap_or_else(|err| err.into_inner());
        let had = registry
            .sessions
            .iter()
            .any(|record| record.session == session);
        registry.sessions.retain(|record| record.session != session);
        if registry.high_water < epoch {
            registry.high_water = epoch;
        }
        if had || registry.high_water == epoch {
            let _ = publish(&this.path, &registry, None);
        }
    }
}

impl HostInner {
    #[cfg(test)]
    fn reserve_reliable_journal(
        &self,
        stream: fl_host_proto::StreamId,
    ) -> Result<Arc<reliable::ReliableJournal>, fl_host_proto::ExtensionRefusal> {
        self.reserve_reliable_journal_with_mode(stream, false)
    }

    fn reserve_reliable_journal_with_mode(
        &self,
        stream: fl_host_proto::StreamId,
        pty_mode: bool,
    ) -> Result<Arc<reliable::ReliableJournal>, fl_host_proto::ExtensionRefusal> {
        reliable::ReliableJournal::new_with_mode(
            Arc::clone(&self.reliable_budget),
            stream,
            pty_mode,
        )
    }

    fn reap_threads(&self) {
        let mut joins = self.joins.lock().unwrap_or_else(|err| err.into_inner());
        let mut running = Vec::new();
        for (key, join) in joins.drain(..) {
            if join.is_finished() {
                let _ = join.join();
            } else {
                running.push((key, join));
            }
        }
        *joins = running;
    }

    pub(crate) fn spawn_thread(&self, work: impl FnOnce() + Send + 'static) {
        self.reap_threads();
        self.joins
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .push((None, std::thread::spawn(work)));
    }

    pub(crate) fn spawn_epoch_thread(
        &self,
        session: &Session,
        work: impl FnOnce() + Send + 'static,
    ) {
        self.reap_threads();
        self.joins
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .push((
                Some((session.session.as_str().to_owned(), session.epoch)),
                std::thread::spawn(work),
            ));
    }

    // Called under spawn_gate. Output is reclaimable only after EOF, successful
    // cleanup, all pending/assigned output and all epoch threads have retired.
    fn reclaim_completed(&self, notices: &mut retention::EvictionNotices) {
        self.reclaim_completed_selected(notices, || {});
    }

    // Private fixture boundary after candidate selection, before registry retirement.
    fn reclaim_completed_selected(
        &self,
        notices: &mut retention::EvictionNotices,
        selected: impl FnOnce(),
    ) {
        self.reap_threads();
        let joins = self.joins.lock().unwrap_or_else(|err| err.into_inner());
        let mut outputs = self.outputs.lock().unwrap_or_else(|err| err.into_inner());
        let retired: Vec<_> = outputs
            .iter()
            .filter(|(key, output)| {
                output.session().mode() != fl_host_proto::SessionMode::Pty
                    && output.can_retire()
                    && self.authority.can_retire(key)
                    && !joins
                        .iter()
                        .any(|(running, _)| running.as_ref() == Some(*key))
            })
            .map(|(key, _)| key.clone())
            .collect();
        selected();
        for (key, output) in outputs.iter() {
            if output.completed_pty()
                && !joins
                    .iter()
                    .any(|(running, _)| running.as_ref() == Some(key))
            {
                self.authority.compact_completed_pty(key, || {
                    output.completed_pty()
                        && !joins
                            .iter()
                            .any(|(running, _)| running.as_ref() == Some(key))
                });
            }
        }
        for key in retired {
            if !self
                .authority
                .try_retire(&key, || outputs[&key].can_retire())
            {
                continue;
            }
            outputs.remove(&key);
            let mut sessions = self.sessions.lock().unwrap_or_else(|err| err.into_inner());
            if sessions
                .get(&key.0)
                .is_some_and(|session| session.epoch == key.1)
            {
                sessions.remove(&key.0);
                self.writers
                    .lock()
                    .unwrap_or_else(|err| err.into_inner())
                    .remove(&key.0);
            }
            self.hub.forget_epoch(&key);
        }
        drop(outputs);
        drop(joins);
        self.evict_completed_ptys(false, notices);
    }

    // spawn_gate serializes this with inventory publication and status maintenance.
    fn evict_completed_ptys(&self, for_spawn: bool, notices: &mut retention::EvictionNotices) {
        let registry = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        let mut outputs = self.outputs.lock().unwrap_or_else(|e| e.into_inner());
        let unresolved = registry
            .sessions
            .iter()
            .filter(|r| !outputs.contains_key(&(r.session.clone(), r.epoch)))
            .count();
        drop(registry);
        while !notices.is_full() {
            let mut completed = 0;
            let oldest = outputs
                .iter()
                .filter(|(_, output)| {
                    let eligible = output.session().reliable().is_some_and(|j| j.compacted());
                    if eligible {
                        completed += 1;
                    }
                    eligible
                })
                .min_by_key(|(_, output)| output.session().completion_order())
                .map(|(key, _)| key.clone());
            let pressure = for_spawn
                && outputs.len().saturating_add(unresolved) >= fl_host_proto::DEFAULT_MAX_SESSIONS;
            if completed <= retention::COMPLETED_PTY_LIMIT && !pressure {
                break;
            }
            let Some(key) = oldest else {
                break;
            };
            self.authority.evict_completed_pty(&key);
            let output = outputs.remove(&key).expect("selected completed PTY");
            let stream = output
                .session()
                .reliable()
                .expect("PTY journal")
                .stream()
                .clone();
            let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            if sessions.get(&key.0).is_some_and(|s| s.epoch == key.1) {
                sessions.remove(&key.0);
                self.writers
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&key.0);
            }
            drop(sessions);
            self.retention
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .evict(stream, notices);
            self.hub.forget_epoch(&key);
        }
    }

    // All maintenance/admission callers share this gate and log boundary.
    fn with_spawn_gate<T>(
        &self,
        operation: impl FnOnce(&mut retention::EvictionNotices) -> T,
    ) -> T {
        self.with_spawn_gate_log(&mut std::io::stderr(), operation)
    }

    fn with_spawn_gate_log<T>(
        &self,
        sink: &mut dyn std::io::Write,
        operation: impl FnOnce(&mut retention::EvictionNotices) -> T,
    ) -> T {
        let mut notices = retention::EvictionNotices::new();
        let gate = self.spawn_gate.lock().unwrap_or_else(|e| e.into_inner());
        let result = operation(&mut notices);
        drop(gate);
        // Inventory and diagnostic history are committed, and every host lock
        // is released. A blocked/failed stderr cannot fence admission or shutdown.
        let _ = notices.emit(sink);
        result
    }

    // The extension permit admits the bounded snapshot before this method runs.
    pub(crate) fn pty_retention_status(&self) -> fl_host_proto::PtyRetentionStatus {
        self.with_spawn_gate(|notices| {
            self.reclaim_completed(notices);
            let completed = self
                .outputs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .values()
                .filter(|output| output.session().reliable().is_some_and(|j| j.compacted()))
                .count();
            let retention = self.retention.lock().unwrap_or_else(|e| e.into_inner());
            fl_host_proto::PtyRetentionStatus {
                projection_reserved_bytes: self
                    .reliable_budget
                    .used(slice_budget::ChargeKind::Projection)
                    as u64,
                projection_limit_bytes: slice_budget::projection_limit() as u64,
                completed_pty_sessions: completed as u32,
                completed_pty_limit: retention::COMPLETED_PTY_LIMIT as u32,
                evicted_total: retention.evicted_total,
                recent_evictions: retention.recent(),
            }
        })
    }

    pub(crate) fn maintain_completed(&self) {
        self.with_spawn_gate(|notices| self.reclaim_completed(notices));
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.shutdown_inner();
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_timeout(Duration::from_millis(500));
        }
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

fn explicit_hosted_owner<'a>(spec: &SpawnSpec, env: &'a [(String, String)]) -> Option<&'a str> {
    env.iter()
        .find(|(key, _)| key == "FL_HOSTED_ID")
        .map(|(_, value)| value.as_str())
        .filter(|value| {
            spec.env.contains_key("FL_HOSTED_ID")
                && !value.is_empty()
                && value.len() <= fl_host_proto::MAX_HOSTED_OWNER_BYTES
        })
}

#[cfg(unix)]
fn write_if_pollout(fd: libc::c_int, bytes: &[u8]) -> bool {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLOUT,
        revents: 0,
    };
    // SAFETY: one pollfd with zero timeout so an unresponsive or full stderr pipe
    // never blocks status draining or epoch retirement.
    if unsafe { libc::poll(&mut pfd, 1, 0) } <= 0
        || (pfd.revents & libc::POLLOUT) == 0
        || (pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL)) != 0
    {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        use std::io::Write;
        let mut path = [0u8; 32];
        let mut cursor = std::io::Cursor::new(path.as_mut_slice());
        if write!(cursor, "/proc/self/fd/{fd}\0").is_ok() {
            // SAFETY: NUL-terminated stack path; opens an independent kernel file
            // description with O_NONBLOCK so `fd`'s own status flags are untouched.
            let opened = unsafe {
                libc::open(
                    path.as_ptr().cast(),
                    libc::O_WRONLY | libc::O_NONBLOCK | libc::O_CLOEXEC,
                )
            };
            if opened >= 0 {
                let n = unsafe { libc::write(opened, bytes.as_ptr().cast(), bytes.len()) };
                unsafe { libc::close(opened) };
                return n > 0;
            }
        }
    }
    // SAFETY: if `fd` is a socket, MSG_DONTWAIT performs a non-blocking write without
    // mutating file status flags; if `fd` is not a socket, send fails with ENOTSOCK.
    let sent = unsafe { libc::send(fd, bytes.as_ptr().cast(), bytes.len(), libc::MSG_DONTWAIT) };
    if sent >= 0 {
        return sent > 0;
    }
    if std::io::Error::last_os_error().raw_os_error() != Some(libc::ENOTSOCK) {
        return false;
    }
    static NONBLOCK_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = NONBLOCK_LOCK.lock().unwrap_or_else(|err| err.into_inner());
    let dup_fd = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    if dup_fd < 0 {
        return false;
    }
    let flags = unsafe { libc::fcntl(dup_fd, libc::F_GETFL) };
    if flags < 0 {
        unsafe { libc::close(dup_fd) };
        return false;
    }
    if (flags & libc::O_NONBLOCK) == 0
        && unsafe { libc::fcntl(dup_fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } != 0
    {
        unsafe { libc::close(dup_fd) };
        return false;
    }
    let n = unsafe { libc::write(dup_fd, bytes.as_ptr().cast(), bytes.len()) };
    if (flags & libc::O_NONBLOCK) == 0 {
        let _ = unsafe { libc::fcntl(dup_fd, libc::F_SETFL, flags) };
    }
    unsafe { libc::close(dup_fd) };
    n > 0
}

#[cfg(unix)]
fn emit_hosted_cleanup_notice_unix(session: &Session, direct_children: usize) {
    if direct_children == 0 {
        return;
    }
    use std::io::Write;
    let mut raw = [0u8; 256];
    let mut cursor = std::io::Cursor::new(raw.as_mut_slice());
    if writeln!(
        cursor,
        "fl-host: hosted session {} (epoch {}) armed post-leader-exit cleanup after {}ms (direct_children={direct_children})",
        session.session.as_str(),
        session.epoch,
        HOSTED_LEADER_EXIT_GRACE.as_millis(),
    )
    .is_err()
    {
        return;
    }
    let len = cursor.position() as usize;
    let _ = write_if_pollout(libc::STDERR_FILENO, &raw[..len]);
}

#[cfg(unix)]
fn unix_status(mut status: File, session: Arc<Session>, host: Arc<HostInner>) {
    while let Ok(Some(line)) = child::unix::read_line_blocking(&mut status) {
        let Ok(message) = protocol::from_line::<protocol::SupervisorMsg>(&line) else {
            continue;
        };
        match message {
            protocol::SupervisorMsg::SignalResult { id, error } => session.note_signal(id, error),
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
            protocol::SupervisorMsg::HostedLeaderExitCleanup { direct_children } => {
                if session.has_hosted_owner() && direct_children > 0 {
                    emit_hosted_cleanup_notice_unix(&session, direct_children);
                }
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
    observe_windows_job_after_poll(session, || {})
}

// The callback permits a native test to complete a real process exit precisely
// between the retained-handle poll and Job accounting. Production has no barrier.
#[cfg(windows)]
fn observe_windows_job_after_poll(
    session: &Session,
    after_initial_poll: impl FnOnce(),
) -> WatchTick {
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
    after_initial_poll();
    let requested = session.cleanup_requested();
    // Hold forces accounting to look live. A non-zero Job is not cleanup.
    let quiet = !job_hold_active() && child::windows::stable_job_quiescent(job);
    WatchTick::Poll { requested, quiet }
}

#[cfg(any(windows, test))]
struct NonBlockingStderrSink;

#[cfg(any(windows, test))]
impl std::io::Write for NonBlockingStderrSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        static STDERR_TX: std::sync::OnceLock<std::sync::mpsc::SyncSender<([u8; 256], usize)>> =
            std::sync::OnceLock::new();
        let tx = STDERR_TX.get_or_init(|| {
            let (tx, rx) = std::sync::mpsc::sync_channel::<([u8; 256], usize)>(16);
            let _ = std::thread::Builder::new()
                .name("fl-host-stderr".into())
                .spawn(move || {
                    while let Ok((raw, len)) = rx.recv() {
                        let _ = std::io::stderr().write_all(&raw[..len]);
                    }
                });
            tx
        });
        let len = buf.len().min(256);
        let mut raw = [0u8; 256];
        raw[..len].copy_from_slice(&buf[..len]);
        let _ = tx.try_send((raw, len));
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
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
    let mut sink = NonBlockingStderrSink;
    windows_watch_until_log(session, host, &mut sink, &mut observe);
}

#[cfg(any(windows, test))]
fn windows_watch_until_log(
    session: Arc<Session>,
    host: Arc<HostInner>,
    sink: &mut dyn std::io::Write,
    observe: &mut dyn FnMut(&Session) -> WatchTick,
) {
    let mut cleanup_started: Option<Instant> = None;
    let mut leader_exited_at: Option<Instant> = None;
    let mut emit_hosted_notice = false;
    loop {
        match observe(&session) {
            WatchTick::Stop => break,
            WatchTick::Leader(code) => {
                if session.has_hosted_owner() {
                    leader_exited_at = Some(Instant::now());
                }
                let exit = ChildExit { code, signal: None };
                host.mark_leader(session.session.as_str(), &exit);
                session.note_leader(exit);
                continue;
            }
            WatchTick::Poll {
                mut requested,
                quiet,
            } => {
                if requested && cleanup_started.is_none() {
                    cleanup_started = Some(Instant::now());
                }
                // Stable Job accounting proves containment cleanup, not that the
                // leader's retained handle has been observed and Exited published.
                if quiet && session.leader_exit().is_some() {
                    host.retire(session.session.as_str());
                    session.note_cleanup(CleanupReport { complete: true });
                    break;
                }
                if !requested
                    && cleanup_started.is_none()
                    && session.has_hosted_owner()
                    && leader_exited_at.is_some_and(|t| t.elapsed() >= HOSTED_LEADER_EXIT_GRACE)
                {
                    cleanup_started = Some(Instant::now());
                    let _ = session.request_cleanup();
                    requested = true;
                    emit_hosted_notice = true;
                }
                if (requested || cleanup_started.is_some())
                    && cleanup_deadline_expired(cleanup_started, Instant::now())
                {
                    host.alert("cleanup unresolved");
                    session.note_cleanup(CleanupReport { complete: false });
                    break;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    if emit_hosted_notice {
        let _ = emit_hosted_cleanup_notice_windows(&session, sink);
    }
}

#[cfg(any(windows, test))]
fn emit_hosted_cleanup_notice_windows(
    session: &Session,
    sink: &mut dyn std::io::Write,
) -> std::io::Result<()> {
    use std::io::Write;
    let mut raw = [0u8; 256];
    let mut cursor = std::io::Cursor::new(raw.as_mut_slice());
    writeln!(
        cursor,
        "fl-host: hosted session {} (epoch {}) armed post-leader-exit cleanup after {}ms",
        session.session.as_str(),
        session.epoch,
        HOSTED_LEADER_EXIT_GRACE.as_millis(),
    )?;
    let len = cursor.position() as usize;
    sink.write_all(&raw[..len])
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

fn block_stop_signals() {
    #[cfg(unix)]
    unsafe {
        let mut set = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
    }
}

fn wait_for_stop() -> Result<(), HostError> {
    #[cfg(unix)]
    {
        block_stop_signals();
        unsafe {
            let mut set = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, libc::SIGINT);
            libc::sigaddset(&mut set, libc::SIGTERM);
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
        CLEANUP_DEADLINE, CleanupReport, HOSTED_LEADER_EXIT_GRACE, Host, OpenOptions, Session,
        SessionParts, SessionPhase, SurvivorRecord, WatchTick, cleanup_deadline_expired,
        load_registry, windows_watch_until, windows_watch_until_log,
    };
    use fl_host_proto::{HostConfig, HostEndpoint, SessionId};

    #[test]
    fn reliable_reservation_uses_host_shared_budget() {
        let (_dir, host, _session, _events) = watch_fixture();
        let stream = fl_host_proto::StreamId {
            host: fl_host_proto::HostInstance::new([1; 16]),
            session: SessionId::new("a").unwrap(),
            epoch: fl_host_proto::SessionEpoch(1),
        };
        let journal = host.inner.reserve_reliable_journal(stream.clone()).unwrap();
        assert_eq!(journal.stream(), &stream);
        assert!(journal.can_retire());
        host.shutdown().unwrap();
    }

    #[derive(Default)]
    struct WatchEvents(std::sync::Mutex<Vec<String>>);

    impl super::session::StdoutSink for WatchEvents {
        fn room(&self) -> usize {
            usize::MAX
        }
        fn push(&self, _: &[u8]) -> bool {
            true
        }
        fn eof(&self) {}
        fn wait(&self, _: Duration) {}
        fn stopped(&self) -> bool {
            false
        }
        fn on_leader(&self, exit: fl_host_proto::ChildExit) {
            self.0.lock().unwrap().push(format!("exit:{:?}", exit.code));
        }
        fn on_cleanup(&self, complete: bool) {
            self.0.lock().unwrap().push(format!("cleanup:{complete}"));
        }
    }

    fn watch_fixture() -> (tempfile::TempDir, Host, Arc<Session>, Arc<WatchEvents>) {
        let dir = tempfile::tempdir().unwrap();
        let host = Host::open(OpenOptions::new(HostConfig::new(unresolved_endpoint(
            dir.path(),
        ))))
        .unwrap();
        let session = Arc::new(Session::new(SessionParts {
            session: SessionId::new("race").unwrap(),
            epoch: 1,
            vendor_pid: 0,
            vendor_start: 0,
            supervisor_pid: 0,
            supervisor_start: 0,
            pgid: 0,
            control_inode: 0,
            control: None,
            stdin: None,
        }));
        host.inner
            .registry
            .lock()
            .unwrap()
            .sessions
            .push(SurvivorRecord {
                session: "race".into(),
                epoch: 1,
                phase: SessionPhase::Running,
                supervisor_pid: 0,
                supervisor_start: 0,
                vendor_pid: 0,
                vendor_start: 0,
                pgid: 0,
                group_leader_pid: 0,
                group_leader_start: 0,
                group_leader_reaped: false,
                leader_code: None,
                leader_signal: None,
            });
        let events = Arc::new(WatchEvents::default());
        let sink: Arc<dyn super::session::StdoutSink> = events.clone();
        session.set_sink(Arc::downgrade(&sink));
        (dir, host, session, events)
    }

    #[test]
    fn windows_hosted_owner_arms_cleanup_after_leader_exit_grace_and_logs() {
        {
            let (_dir, host, _, _) = watch_fixture();
            let session = Arc::new(
                Session::new(SessionParts {
                    session: SessionId::new("race").unwrap(),
                    epoch: 1,
                    vendor_pid: 0,
                    vendor_start: 0,
                    supervisor_pid: 0,
                    supervisor_start: 0,
                    pgid: 0,
                    control_inode: 0,
                    control: None,
                    stdin: None,
                })
                .with_hosted_owner(Some("hosted-409")),
            );
            assert!(session.has_hosted_owner());
            let events = Arc::new(WatchEvents::default());
            let sink_events: Arc<dyn super::session::StdoutSink> = events.clone();
            session.set_sink(Arc::downgrade(&sink_events));
            let mut log = Vec::new();
            let started = Instant::now();
            let mut leader_sent = false;
            let mut armed_at = None;
            windows_watch_until_log(
                Arc::clone(&session),
                Arc::clone(&host.inner),
                &mut log,
                &mut |s| {
                    if !leader_sent {
                        leader_sent = true;
                        return WatchTick::Leader(Some(0));
                    }
                    if s.cleanup_requested() {
                        armed_at.get_or_insert_with(|| started.elapsed());
                        return WatchTick::Poll {
                            requested: true,
                            quiet: true,
                        };
                    }
                    if started.elapsed() > HOSTED_LEADER_EXIT_GRACE + Duration::from_secs(1) {
                        return WatchTick::Stop;
                    }
                    WatchTick::Poll {
                        requested: false,
                        quiet: false,
                    }
                },
            );
            let armed = armed_at.expect("hosted-owner session never armed cleanup");
            assert!(
                armed >= HOSTED_LEADER_EXIT_GRACE,
                "armed before grace: {armed:?}"
            );
            assert!(session.cleanup_requested());
            assert_eq!(
                session.cleanup_report(),
                Some(CleanupReport { complete: true })
            );
            assert!(host.inner.registry.lock().unwrap().sessions.is_empty());
            assert_eq!(*events.0.lock().unwrap(), ["exit:Some(0)", "cleanup:true"]);
            assert_eq!(
                String::from_utf8(log).unwrap(),
                "fl-host: hosted session race (epoch 1) armed post-leader-exit cleanup after 500ms\n"
            );
            host.shutdown().unwrap();
        }

        // A blocked/failing io::Write sink must run after retire + note_cleanup and outside all host locks.
        struct BlockedSink {
            entered: std::sync::mpsc::Sender<()>,
            release: std::sync::mpsc::Receiver<()>,
        }
        impl std::io::Write for BlockedSink {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                self.entered.send(()).unwrap();
                self.release.recv_timeout(Duration::from_secs(5)).unwrap();
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
        }
        let (_dir, host, _, _) = watch_fixture();
        let session = Arc::new(
            Session::new(SessionParts {
                session: SessionId::new("race").unwrap(),
                epoch: 1,
                vendor_pid: 0,
                vendor_start: 0,
                supervisor_pid: 0,
                supervisor_start: 0,
                pgid: 0,
                control_inode: 0,
                control: None,
                stdin: None,
            })
            .with_hosted_owner(Some("hosted-409")),
        );
        let (entered, observing) = std::sync::mpsc::channel();
        let (release, waiting) = std::sync::mpsc::channel();
        let watched = Arc::clone(&session);
        let inner = Arc::clone(&host.inner);
        let watcher = std::thread::spawn(move || {
            let started = Instant::now();
            let mut leader_sent = false;
            let mut sink = BlockedSink {
                entered,
                release: waiting,
            };
            windows_watch_until_log(watched, inner, &mut sink, &mut |s| {
                if !leader_sent {
                    leader_sent = true;
                    return WatchTick::Leader(Some(0));
                }
                if s.cleanup_requested() {
                    return WatchTick::Poll {
                        requested: true,
                        quiet: true,
                    };
                }
                if started.elapsed() > HOSTED_LEADER_EXIT_GRACE + Duration::from_secs(1) {
                    return WatchTick::Stop;
                }
                WatchTick::Poll {
                    requested: false,
                    quiet: false,
                }
            });
        });
        observing.recv_timeout(Duration::from_secs(5)).unwrap();
        let registry_retired = host
            .inner
            .registry
            .try_lock()
            .is_ok_and(|registry| registry.sessions.is_empty());
        let cleanup_published = session.cleanup_report() == Some(CleanupReport { complete: true });
        let locks_free = host.inner.sessions.try_lock().is_ok()
            && host.inner.outputs.try_lock().is_ok()
            && host.inner.spawn_gate.try_lock().is_ok();
        release.send(()).unwrap();
        watcher.join().unwrap();
        assert!(
            registry_retired,
            "blocked log ran before session retired from registry or held registry lock"
        );
        assert!(
            cleanup_published,
            "blocked log ran before session.note_cleanup completed"
        );
        assert!(locks_free, "blocked log held a host lock");
        assert!(session.cleanup_requested());
        host.shutdown().unwrap();
    }

    #[test]
    fn windows_unmarked_session_does_not_auto_cleanup_after_leader_exit_grace() {
        let make_spec = |value: Option<&str>, allow: bool| {
            let mut spec = fl_host_proto::SpawnSpec {
                session: SessionId::new("race").unwrap(),
                kind: fl_host_proto::SpawnKind::Pipe,
                argv: vec!["bin".into()],
                cwd: std::env::temp_dir(),
                env: Default::default(),
                env_allow: Default::default(),
            };
            if allow {
                spec.env_allow.insert("FL_HOSTED_ID".into());
            }
            if let Some(value) = value {
                spec.env.insert("FL_HOSTED_ID".into(), value.to_owned());
            }
            spec
        };
        let valid = "x".repeat(fl_host_proto::MAX_HOSTED_OWNER_BYTES);
        let oversized = "x".repeat(fl_host_proto::MAX_HOSTED_OWNER_BYTES + 1);
        assert_eq!(
            super::explicit_hosted_owner(
                &make_spec(Some(&valid), true),
                &[("FL_HOSTED_ID".into(), valid.clone())]
            ),
            Some(valid.as_str())
        );
        assert_eq!(
            super::explicit_hosted_owner(
                &make_spec(None, true),
                &[("FL_HOSTED_ID".into(), "inherited".into())]
            ),
            None
        );
        assert_eq!(
            super::explicit_hosted_owner(
                &make_spec(Some(""), true),
                &[("FL_HOSTED_ID".into(), String::new())]
            ),
            None
        );
        assert_eq!(
            super::explicit_hosted_owner(
                &make_spec(Some(&oversized), true),
                &[("FL_HOSTED_ID".into(), oversized)]
            ),
            None
        );

        let (_dir, host, session, events) = watch_fixture();
        assert!(!session.has_hosted_owner());
        let mut log = Vec::new();
        let started = Instant::now();
        let mut leader_sent = false;
        windows_watch_until_log(
            Arc::clone(&session),
            Arc::clone(&host.inner),
            &mut log,
            &mut |s| {
                if !leader_sent {
                    leader_sent = true;
                    return WatchTick::Leader(Some(0));
                }
                if started.elapsed() >= HOSTED_LEADER_EXIT_GRACE + Duration::from_millis(200) {
                    return WatchTick::Stop;
                }
                WatchTick::Poll {
                    requested: s.cleanup_requested(),
                    quiet: false,
                }
            },
        );
        assert!(!session.cleanup_requested());
        assert!(session.cleanup_report().is_none());
        assert_eq!(session.leader_exit().unwrap().code, Some(0));
        assert_eq!(*events.0.lock().unwrap(), ["exit:Some(0)"]);
        assert!(log.is_empty());
        let registry = host.inner.registry.lock().unwrap();
        assert_eq!(registry.sessions.len(), 1);
        assert_eq!(registry.sessions[0].phase, SessionPhase::LeaderExited);
        drop(registry);
        host.shutdown().unwrap();
    }

    #[test]
    fn windows_hosted_owner_unresolved_cleanup_expires_after_deadline() {
        let (_dir, host, _, _) = watch_fixture();
        let session = Arc::new(
            Session::new(SessionParts {
                session: SessionId::new("race").unwrap(),
                epoch: 1,
                vendor_pid: 0,
                vendor_start: 0,
                supervisor_pid: 0,
                supervisor_start: 0,
                pgid: 0,
                control_inode: 0,
                control: None,
                stdin: None,
            })
            .with_hosted_owner(Some("hosted-409")),
        );
        let events = Arc::new(WatchEvents::default());
        let sink_events: Arc<dyn super::session::StdoutSink> = events.clone();
        session.set_sink(Arc::downgrade(&sink_events));
        let mut log = Vec::new();
        let started = Instant::now();
        let mut leader_sent = false;
        windows_watch_until_log(
            Arc::clone(&session),
            Arc::clone(&host.inner),
            &mut log,
            &mut |_| {
                if !leader_sent {
                    leader_sent = true;
                    return WatchTick::Leader(Some(0));
                }
                if started.elapsed()
                    > HOSTED_LEADER_EXIT_GRACE + CLEANUP_DEADLINE + Duration::from_secs(2)
                {
                    return WatchTick::Stop;
                }
                // Even if requested stays false in the poll snapshot, cleanup_started must bound wait.
                WatchTick::Poll {
                    requested: false,
                    quiet: false,
                }
            },
        );
        assert_eq!(
            session.cleanup_report(),
            Some(CleanupReport { complete: false })
        );
        assert_eq!(*events.0.lock().unwrap(), ["exit:Some(0)", "cleanup:false"]);
        assert_eq!(log.iter().filter(|&&b| b == b'\n').count(), 1);
        assert!(
            host.inner
                .registry
                .lock()
                .unwrap()
                .alerts
                .iter()
                .any(|a| a == "cleanup unresolved")
        );
        host.shutdown().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn unix_hosted_cleanup_log_and_protocol_defaults_stay_bounded() {
        use std::io::Read;
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

        let legacy = br#"{"op":"spawn","argv":["a"],"cwd":[47],"env":[],"session":"s","epoch":1}"#;
        let parsed: super::protocol::DaemonMsg = super::protocol::from_line(legacy).unwrap();
        assert_eq!(
            parsed,
            super::protocol::DaemonMsg::Spawn {
                argv: vec!["a".into()],
                cwd: vec![b'/'],
                env: Vec::new(),
                session: "s".into(),
                epoch: 1,
                hosted_owner: false,
            }
        );
        let msg = super::protocol::SupervisorMsg::HostedLeaderExitCleanup { direct_children: 3 };
        let line = super::protocol::to_line(&msg).unwrap();
        assert_eq!(
            super::protocol::from_line::<super::protocol::SupervisorMsg>(&line[..line.len() - 1])
                .unwrap(),
            msg
        );
        let legacy_cleanup = br#"{"op":"hosted_leader_exit_cleanup","descendants":3}"#;
        assert_eq!(
            super::protocol::from_line::<super::protocol::SupervisorMsg>(legacy_cleanup).unwrap(),
            msg
        );

        let mut fds = [0; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let mut reader = unsafe { std::fs::File::from_raw_fd(fds[0]) };
        let writer = unsafe { OwnedFd::from_raw_fd(fds[1]) };
        assert!(super::write_if_pollout(writer.as_raw_fd(), b"hello\n"));
        assert_eq!(
            unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_GETFL) } & libc::O_NONBLOCK,
            0
        );
        let mut got = [0u8; 6];
        reader.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"hello\n");

        // Fill the pipe with O_NONBLOCK, then restore blocking flags like STDERR_FILENO.
        let flags = unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_GETFL) };
        assert!(flags >= 0);
        assert_eq!(
            unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) },
            0
        );
        let chunk = [b'x'; 4096];
        while unsafe { libc::write(writer.as_raw_fd(), chunk.as_ptr().cast(), chunk.len()) } > 0 {}
        assert_eq!(
            unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_SETFL, flags) },
            0
        );
        let started = Instant::now();
        assert!(!super::write_if_pollout(
            writer.as_raw_fd(),
            b"must-not-block\n"
        ));
        assert!(started.elapsed() < Duration::from_millis(100));
        assert_eq!(
            unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_GETFL) } & libc::O_NONBLOCK,
            0
        );

        // Also verify socket fallback path (`MSG_DONTWAIT` when `/proc/self/fd` returns ENXIO).
        let mut socks = [0; 2];
        assert_eq!(
            unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, socks.as_mut_ptr()) },
            0
        );
        let mut sock_reader = unsafe { std::fs::File::from_raw_fd(socks[0]) };
        let sock_writer = unsafe { OwnedFd::from_raw_fd(socks[1]) };
        assert!(super::write_if_pollout(sock_writer.as_raw_fd(), b"sock!\n"));
        assert_eq!(
            unsafe { libc::fcntl(sock_writer.as_raw_fd(), libc::F_GETFL) } & libc::O_NONBLOCK,
            0
        );
        let mut sock_got = [0u8; 6];
        sock_reader.read_exact(&mut sock_got).unwrap();
        assert_eq!(&sock_got, b"sock!\n");
    }

    #[test]
    fn windows_cleanup_records_exit_before_retiring_quiet_job() {
        for code in [0, 259] {
            let (_dir, host, session, events) = watch_fixture();
            let mut tick = 0;
            windows_watch_until(Arc::clone(&session), Arc::clone(&host.inner), |_| {
                tick += 1;
                match tick {
                    1 => WatchTick::Poll {
                        requested: true,
                        quiet: true,
                    },
                    2 => {
                        assert_eq!(host.inner.registry.lock().unwrap().sessions.len(), 1);
                        assert!(events.0.lock().unwrap().is_empty());
                        WatchTick::Leader(Some(code))
                    }
                    3 => {
                        let registry = host.inner.registry.lock().unwrap();
                        let record = &registry.sessions[0];
                        assert!(record.group_leader_reaped);
                        assert_eq!(record.leader_code, Some(code));
                        WatchTick::Poll {
                            requested: true,
                            quiet: false,
                        }
                    }
                    4 => {
                        assert_eq!(host.inner.registry.lock().unwrap().sessions.len(), 1);
                        assert!(session.cleanup_report().is_none());
                        WatchTick::Poll {
                            requested: true,
                            quiet: true,
                        }
                    }
                    _ => panic!("watcher failed to finish"),
                }
            });
            assert_eq!(tick, 4, "retired before observing leader exit");
            assert_eq!(
                *events.0.lock().unwrap(),
                [format!("exit:Some({code})"), "cleanup:true".into()]
            );
            assert!(host.inner.registry.lock().unwrap().sessions.is_empty());
            host.shutdown().unwrap();
        }
    }

    #[test]
    fn windows_quiet_without_leader_evidence_has_bounded_unresolved_cleanup() {
        let (_dir, host, session, events) = watch_fixture();
        let watched = Arc::clone(&session);
        let inner = Arc::clone(&host.inner);
        let (tx, rx) = std::sync::mpsc::channel();
        let started = Instant::now();
        let watcher = std::thread::spawn(move || {
            windows_watch_until(watched, inner, |_| {
                // A regressed driver must fail this test without leaving a
                // permanently running watcher behind in the test process.
                if started.elapsed() > CLEANUP_DEADLINE + Duration::from_secs(1) {
                    WatchTick::Stop
                } else {
                    WatchTick::Poll {
                        requested: true,
                        quiet: true,
                    }
                }
            });
            tx.send(()).unwrap();
        });
        rx.recv_timeout(CLEANUP_DEADLINE + Duration::from_secs(3))
            .expect("quiet without exit evidence hung cleanup");
        watcher.join().unwrap();
        assert!(started.elapsed() >= CLEANUP_DEADLINE);
        assert!(session.leader_exit().is_none());
        assert_eq!(
            session.cleanup_report(),
            Some(CleanupReport { complete: false })
        );
        assert_eq!(*events.0.lock().unwrap(), ["cleanup:false"]);
        assert_eq!(host.inner.registry.lock().unwrap().sessions.len(), 1);
        host.shutdown().unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_real_leader_exit_between_handle_poll_and_job_accounting() {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
        use windows_sys::Win32::System::Threading::{TerminateProcess, WaitForSingleObject};

        for code in [0, 259] {
            let (dir, host, session, events) = watch_fixture();
            // Request before installing the test handles: the worker below supplies
            // the asynchronous termination, independently of the observer's locks.
            session.request_cleanup().unwrap();
            let cmd = std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap())
                .join("System32")
                .join("cmd.exe");
            let child = super::child::windows::spawn_in_job(
                &[
                    cmd.to_string_lossy().into_owned(),
                    "/D".into(),
                    "/Q".into(),
                    "/C".into(),
                    "for /L %i in (1,0,2) do @rem".into(),
                ],
                dir.path(),
                &[],
                None,
                |_, _| Ok(()),
            )
            .unwrap();
            let retained = child.process.try_clone().unwrap();
            let retained_job = child.job.try_clone().unwrap();
            *session.process.lock().unwrap() = Some(child.process);
            *session.job.lock().unwrap() = Some(child.job);
            assert!(
                matches!(
                    super::observe_windows_job(&session),
                    WatchTick::Poll {
                        requested: true,
                        quiet: false
                    }
                ),
                "live Job member must not be cleanup proof"
            );
            let (release, hold) = std::sync::mpsc::channel();
            let (done, completed) = std::sync::mpsc::channel();
            let terminator = std::thread::spawn(move || {
                hold.recv_timeout(Duration::from_secs(3)).unwrap();
                // SAFETY: this independently owned process handle stays live across
                // both calls; only the fixture child is terminated.
                let exited = unsafe {
                    TerminateProcess(retained.as_raw_handle() as _, code) != 0
                        && WaitForSingleObject(retained.as_raw_handle() as _, 3000) == WAIT_OBJECT_0
                };
                // Process signaling need not coincide with the Job accounting
                // update. Establish that state before releasing the observer,
                // rather than assuming the kernel updates both atomically.
                let deadline = Instant::now() + Duration::from_secs(3);
                let mut quiet = false;
                while exited && Instant::now() < deadline {
                    if super::child::windows::stable_job_quiescent(&retained_job) {
                        quiet = true;
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                done.send(exited && quiet).unwrap();
            });
            let first = super::observe_windows_job_after_poll(&session, || {
                release.send(()).unwrap();
                assert!(completed.recv_timeout(Duration::from_secs(7)).unwrap());
            });
            terminator.join().unwrap();
            assert!(
                matches!(
                    first,
                    WatchTick::Poll {
                        requested: true,
                        quiet: true
                    }
                ),
                "real exit must occur after unsignaled poll and before stable zero accounting"
            );
            let mut first = Some(first);
            let mut ticks = 0;
            windows_watch_until(Arc::clone(&session), Arc::clone(&host.inner), |session| {
                ticks += 1;
                assert!(ticks <= 4, "real exit not observed");
                first
                    .take()
                    .unwrap_or_else(|| super::observe_windows_job(session))
            });
            assert!(
                ticks >= 3,
                "cleanup skipped the retained-handle exit observation"
            );
            assert_eq!(
                *events.0.lock().unwrap(),
                [format!("exit:Some({code})"), "cleanup:true".into()]
            );
            assert_eq!(session.leader_exit().unwrap().code, Some(code as i32));
            assert!(host.inner.registry.lock().unwrap().sessions.is_empty());
            host.shutdown().unwrap();
        }
    }

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

    #[test]
    fn replacement_releases_old_writer_before_host_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = unresolved_endpoint(dir.path());
        let host = Host::open(OpenOptions::new(HostConfig::new(endpoint))).unwrap();
        let make = |epoch| {
            Arc::new(Session::new(SessionParts {
                session: SessionId::new("same").unwrap(),
                epoch,
                vendor_pid: 0,
                vendor_start: 0,
                supervisor_pid: 0,
                supervisor_start: 0,
                pgid: 0,
                control_inode: 0,
                control: None,
                stdin: None,
            }))
        };
        let old = make(1);
        super::daemon::attach(&host.inner, &old, None);
        let weak = Arc::downgrade(host.inner.writers.lock().unwrap().get("same").unwrap());
        drop(old);
        super::daemon::attach(&host.inner, &make(2), None);
        let deadline = Instant::now() + Duration::from_secs(2);
        while weak.upgrade().is_some() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            weak.upgrade().is_none(),
            "old writer remains alive after replacement"
        );
        host.shutdown().unwrap();
    }

    #[test]
    fn retirement_rechecks_historical_claim_after_candidate_selection() {
        retirement_admission_race(false);
    }

    #[test]
    fn retirement_rechecks_observer_after_candidate_selection() {
        retirement_admission_race(true);
    }

    #[test]
    fn retirement_waits_for_unresolved_claim_and_releases_cancelled_admission() {
        use fl_host_proto::{ConnId, SessionEpoch, StreamId};
        use std::{
            future::Future,
            task::{Context, Poll, Waker},
        };
        let dir = tempfile::tempdir().unwrap();
        let host = Host::open(OpenOptions::new(HostConfig::new(unresolved_endpoint(
            dir.path(),
        ))))
        .unwrap();
        let stream = StreamId {
            host: host.inner.authority.instance().clone(),
            session: SessionId::new("claim-fence").unwrap(),
            epoch: SessionEpoch(1),
        };
        let journal = host.inner.reserve_reliable_journal(stream.clone()).unwrap();
        let session = Arc::new(Session::with_reliable(
            SessionParts {
                session: stream.session.clone(),
                epoch: 1,
                vendor_pid: 0,
                vendor_start: 0,
                supervisor_pid: 0,
                supervisor_start: 0,
                pgid: 0,
                control_inode: 0,
                control: None,
                stdin: None,
            },
            Some(Arc::clone(&journal)),
        ));
        let writer = super::input::WriterHandle::start(Arc::clone(&session), None);
        let reg = &host.inner.authority;
        reg.register(
            reg.prepare_epoch(Arc::clone(&journal)).unwrap(),
            &session,
            &writer,
        );
        let key = (stream.session.as_str().to_owned(), 1);
        host.inner
            .sessions
            .lock()
            .unwrap()
            .insert(key.0.clone(), Arc::clone(&session));
        host.inner.outputs.lock().unwrap().insert(
            key.clone(),
            super::output::OutputSession::open(Arc::clone(&session), Arc::clone(&host.inner.hub)),
        );
        reg.connect(ConnId(81)).unwrap();
        let mut claim = Box::pin(super::authority::claim(reg, ConnId(81), &stream));
        let mut cx = Context::from_waker(Waker::noop());
        assert!(matches!(claim.as_mut().poll(&mut cx), Poll::Pending));
        // Claim generation is assigned, owner is None, writer fence is unresolved.
        journal.eof();
        session.note_cleanup(CleanupReport { complete: true });
        host.inner.maintain_completed();
        assert!(host.inner.outputs.lock().unwrap().contains_key(&key));
        drop(claim);
        host.inner.maintain_completed();
        assert!(!host.inner.outputs.lock().unwrap().contains_key(&key));
        writer.shutdown();
        host.shutdown().unwrap();
    }

    fn retirement_admission_race(observer: bool) {
        use fl_host_proto::{ConnId, Cursor, SessionEpoch, StreamId};
        let dir = tempfile::tempdir().unwrap();
        let host = Host::open(OpenOptions::new(HostConfig::new(unresolved_endpoint(
            dir.path(),
        ))))
        .unwrap();
        let stream = StreamId {
            host: host.inner.authority.instance().clone(),
            session: SessionId::new("retirement-race").unwrap(),
            epoch: SessionEpoch(1),
        };
        let journal = host.inner.reserve_reliable_journal(stream.clone()).unwrap();
        let session = Arc::new(Session::with_reliable(
            SessionParts {
                session: stream.session.clone(),
                epoch: 1,
                vendor_pid: 0,
                vendor_start: 0,
                supervisor_pid: 0,
                supervisor_start: 0,
                pgid: 0,
                control_inode: 0,
                control: None,
                stdin: None,
            },
            Some(Arc::clone(&journal)),
        ));
        let writer = super::input::WriterHandle::start(Arc::clone(&session), None);
        let prepared = host
            .inner
            .authority
            .prepare_epoch(Arc::clone(&journal))
            .unwrap();
        host.inner.authority.register(prepared, &session, &writer);
        let output =
            super::output::OutputSession::open(Arc::clone(&session), Arc::clone(&host.inner.hub));
        journal.eof();
        session.note_cleanup(CleanupReport { complete: true });
        let key = (stream.session.as_str().to_owned(), 1);
        host.inner
            .outputs
            .lock()
            .unwrap()
            .insert(key.clone(), output);
        let reg = &host.inner.authority;
        reg.connect(ConnId(71)).unwrap();
        reg.connect(ConnId(72)).unwrap();
        let cursor = Cursor {
            stream: stream.clone(),
            seq: 0,
        };
        // Fill the remaining retained capacity with incomplete legacy epochs.
        // Reclamation cannot turn this into an available admission slot.
        for n in 1..64 {
            let filler = Arc::new(Session::new(SessionParts {
                session: SessionId::new(&format!("filler-{n}")).unwrap(),
                epoch: 1,
                vendor_pid: 0,
                vendor_start: 0,
                supervisor_pid: 0,
                supervisor_start: 0,
                pgid: 0,
                control_inode: 0,
                control: None,
                stdin: None,
            }));
            let output = super::output::OutputSession::open(
                Arc::clone(&filler),
                Arc::clone(&host.inner.hub),
            );
            host.inner
                .outputs
                .lock()
                .unwrap()
                .insert((filler.session.as_str().to_owned(), 1), output);
        }
        let mut ticket = None;
        let mut lease = None;
        // The callback is the deterministic scheduling barrier: admission completes
        // after the old eligibility checks and before either removal.
        host.inner.with_spawn_gate(|notices| {
            host.inner.reclaim_completed_selected(notices, || {
                std::thread::scope(|scope| {
                    scope
                        .spawn(|| {
                            if observer {
                                ticket = Some(
                                    super::authority::issue_ticket(
                                        reg,
                                        ConnId(71),
                                        cursor.clone(),
                                        None,
                                        true,
                                        Instant::now(),
                                    )
                                    .unwrap()
                                    .ticket,
                                );
                            } else {
                                lease = Some(
                                    host.inner
                                        .handle
                                        .block_on(super::authority::claim(reg, ConnId(71), &stream))
                                        .unwrap()
                                        .lease,
                                );
                            }
                        })
                        .join()
                        .unwrap()
                });
            })
        });
        assert!(
            host.inner.outputs.lock().unwrap().contains_key(&key),
            "accepted authority must retain its counted epoch"
        );
        assert_eq!(host.inner.outputs.lock().unwrap().len(), 64);
        let rows = host
            .inner
            .handle
            .block_on(super::daemon::extended(
                &host.inner,
                ConnId(71),
                fl_host_proto::ExtensionRequest::ListSessions,
                &mut None,
            ))
            .unwrap();
        let fl_host_proto::ExtensionResponse::Sessions { records } = rows else {
            panic!("missing listing")
        };
        assert_eq!(records.len(), 64);
        assert!(records.iter().any(|record| record.stream == stream));
        if let Some(lease) = lease {
            host.inner
                .handle
                .block_on(reg.acknowledge(ConnId(71), cursor, lease))
                .unwrap();
        }
        let replay = ticket.map(|ticket| {
            super::authority::consume_ticket(reg, ConnId(72), ticket, Instant::now()).unwrap()
        });
        super::authority::revoke_connection(reg, ConnId(71));
        host.inner.maintain_completed();
        assert_eq!(
            host.inner.outputs.lock().unwrap().contains_key(&key),
            replay.is_some(),
            "active observer cursor must remain counted"
        );
        drop(replay);
        host.inner.maintain_completed();
        assert!(!host.inner.outputs.lock().unwrap().contains_key(&key));
        host.shutdown().unwrap();
    }

    fn retained_pty_fixture(
        host: &Host,
        name: &str,
        epoch: u64,
    ) -> (
        Arc<Session>,
        Arc<super::reliable::ReliableJournal>,
        Arc<super::input::WriterHandle>,
    ) {
        let stream = fl_host_proto::StreamId {
            host: host.inner.authority.instance().clone(),
            session: SessionId::new(name).unwrap(),
            epoch: fl_host_proto::SessionEpoch(epoch),
        };
        let journal = host
            .inner
            .reserve_reliable_journal_with_mode(stream.clone(), true)
            .unwrap();
        let session = Arc::new(Session::with_reliable(
            SessionParts {
                session: stream.session.clone(),
                epoch,
                vendor_pid: 0,
                vendor_start: 0,
                supervisor_pid: 0,
                supervisor_start: 0,
                pgid: 0,
                control_inode: 0,
                control: None,
                stdin: None,
            },
            Some(Arc::clone(&journal)),
        ));
        let output =
            super::output::OutputSession::open(Arc::clone(&session), Arc::clone(&host.inner.hub));
        let writer = super::input::WriterHandle::start(Arc::clone(&session), None);
        host.inner.authority.register(
            host.inner
                .authority
                .prepare_epoch(Arc::clone(&journal))
                .unwrap(),
            &session,
            &writer,
        );
        host.inner
            .outputs
            .lock()
            .unwrap()
            .insert((name.to_owned(), epoch), output);
        host.inner
            .sessions
            .lock()
            .unwrap()
            .insert(name.to_owned(), Arc::clone(&session));
        (session, journal, writer)
    }

    #[test]
    fn blocked_eviction_log_cannot_hold_inventory_history_or_spawn_gate() {
        struct BlockedSink {
            entered: std::sync::mpsc::Sender<()>,
            release: std::sync::mpsc::Receiver<()>,
        }
        impl std::io::Write for BlockedSink {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                self.entered.send(()).unwrap();
                self.release.recv_timeout(Duration::from_secs(5)).unwrap();
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let host = Host::open(OpenOptions::new(HostConfig::new(unresolved_endpoint(
            dir.path(),
        ))))
        .unwrap();
        let mut oldest = None;
        for n in 0..9 {
            let (session, journal, writer) =
                retained_pty_fixture(&host, &format!("notice-{n}"), n + 1);
            oldest.get_or_insert_with(|| journal.stream().clone());
            journal.eof();
            session.note_cleanup(CleanupReport { complete: true });
            writer.shutdown();
        }
        let oldest = oldest.unwrap();
        let (entered, observing) = std::sync::mpsc::channel();
        let (release, waiting) = std::sync::mpsc::channel();
        let inner = Arc::clone(&host.inner);
        let logging = std::thread::spawn(move || {
            let mut sink = BlockedSink {
                entered,
                release: waiting,
            };
            inner.with_spawn_gate_log(&mut sink, |notices| {
                inner.reclaim_completed(notices);
                // Admission may fail after reclaiming: notices still must be emitted
                // outside the gate without replacing the operation's error.
                Err::<(), _>(fl_host_proto::HostError::Protocol(
                    "admission refused after maintenance".into(),
                ))
            })
        });
        observing.recv_timeout(Duration::from_secs(5)).unwrap();
        let inventory_committed = host.inner.outputs.try_lock().is_ok_and(|outputs| {
            outputs.len() == 8
                && !outputs.contains_key(&(oldest.session.as_str().to_owned(), oldest.epoch.0))
        });
        let history_committed =
            host.inner.retention.try_lock().is_ok_and(|retention| {
                retention.evicted_total == 1 && retention.recent() == [oldest]
            });
        let gate_available = host.inner.spawn_gate.try_lock().is_ok();
        let sessions_available = host.inner.sessions.try_lock().is_ok();
        let writers_available = host.inner.writers.try_lock().is_ok();
        release.send(()).unwrap();
        let result = logging.join();
        host.shutdown().unwrap();
        assert!(
            inventory_committed,
            "blocked log retained inventory lock or eviction"
        );
        assert!(
            history_committed,
            "blocked log retained history lock or eviction record"
        );
        assert!(
            gate_available,
            "blocked log retained admission/shutdown gate"
        );
        assert!(sessions_available && writers_available);
        assert!(
            matches!(result, Ok(Err(fl_host_proto::HostError::Protocol(message))) if message == "admission refused after maintenance"),
            "failed log panicked or replaced admission result"
        );
    }

    #[test]
    fn completed_pty_order_history_and_slot_pressure_are_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let host = Host::open(OpenOptions::new(HostConfig::new(unresolved_endpoint(
            dir.path(),
        ))))
        .unwrap();
        // Reverse names prove retirement does not follow lexical/map order.
        let mut streams = Vec::new();
        for n in 0..74 {
            let (session, journal, writer) =
                retained_pty_fixture(&host, &format!("z-{:03}", 74 - n), n + 1);
            streams.push(journal.stream().clone());
            journal.eof();
            session.note_cleanup(CleanupReport { complete: true });
            host.inner.maintain_completed();
            assert!(host.inner.outputs.lock().unwrap().len() <= 8);
            assert_eq!(journal.position().0, 0);
            writer.shutdown();
        }
        let status = host.inner.pty_retention_status();
        assert_eq!(status.completed_pty_sessions, 8);
        assert_eq!(status.evicted_total, 66);
        assert_eq!(status.recent_evictions, streams[2..66]);
        for n in 0..56 {
            // These PTYs never clean up and must survive capacity pressure.
            retained_pty_fixture(&host, &format!("live-{n}"), n + 100);
        }
        assert_eq!(host.inner.outputs.lock().unwrap().len(), 64);
        host.inner
            .with_spawn_gate(|notices| host.inner.evict_completed_ptys(true, notices));
        assert_eq!(host.inner.outputs.lock().unwrap().len(), 63);
        let status = host.inner.pty_retention_status();
        assert_eq!(status.completed_pty_sessions, 7);
        assert_eq!(status.evicted_total, 67);
        assert_eq!(status.recent_evictions.last(), Some(&streams[66]));
        assert!(
            host.inner
                .outputs
                .lock()
                .unwrap()
                .keys()
                .filter(|(name, _)| name.starts_with("live-"))
                .count()
                == 56
        );
        host.shutdown().unwrap();
    }

    #[test]
    fn pty_compaction_requires_cleanup_eof_and_retired_threads() {
        let dir = tempfile::tempdir().unwrap();
        let host = Host::open(OpenOptions::new(HostConfig::new(unresolved_endpoint(
            dir.path(),
        ))))
        .unwrap();
        let (session, journal, writer) = retained_pty_fixture(&host, "held-thread", 1);
        session.note_cleanup(CleanupReport { complete: true });
        host.inner.maintain_completed();
        assert!(!journal.compacted(), "cleanup alone is insufficient");
        let (release, hold) = std::sync::mpsc::channel();
        host.inner.spawn_epoch_thread(&session, move || {
            hold.recv_timeout(Duration::from_secs(5)).unwrap();
        });
        journal.eof();
        host.inner.maintain_completed();
        assert!(
            !journal.compacted(),
            "live epoch thread prevents compaction"
        );
        release.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !journal.compacted() {
            host.inner.maintain_completed();
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        let (incomplete, j2, w2) = retained_pty_fixture(&host, "incomplete", 2);
        j2.eof();
        incomplete.note_cleanup(CleanupReport { complete: false });
        host.inner.maintain_completed();
        assert!(!j2.compacted());
        writer.shutdown();
        w2.shutdown();
        host.shutdown().unwrap();
    }

    #[test]
    fn pty_compaction_cancels_racing_tickets_and_observers_without_releasing_held_bytes() {
        use fl_host_proto::{ConnId, Cursor, ExtensionRefusal as F};
        let dir = tempfile::tempdir().unwrap();
        let host = Host::open(OpenOptions::new(HostConfig::new(unresolved_endpoint(
            dir.path(),
        ))))
        .unwrap();
        let (session, journal, writer) = retained_pty_fixture(&host, "observer", 1);
        let bytes = journal.allocate(10).unwrap();
        super::reliable::append(&journal, super::reliable::ReliableBody::Line(bytes)).unwrap();
        journal.eof();
        session.note_cleanup(CleanupReport { complete: true });
        let reg = &host.inner.authority;
        reg.connect(ConnId(1)).unwrap();
        reg.connect(ConnId(2)).unwrap();
        let cursor = Cursor {
            stream: journal.stream().clone(),
            seq: 0,
        };
        let ticket = super::authority::issue_ticket(
            reg,
            ConnId(1),
            cursor.clone(),
            None,
            true,
            Instant::now(),
        )
        .unwrap()
        .ticket;
        let mut replay =
            super::authority::consume_ticket(reg, ConnId(2), ticket, Instant::now()).unwrap();
        let record = replay.next().unwrap().unwrap();
        let held_charge = journal
            .budget()
            .used(super::slice_budget::ChargeKind::ReliableData);
        assert!(held_charge >= 10); // includes the retained wire envelope
        let mut racing_ticket = None;
        host.inner.with_spawn_gate(|notices| {
            host.inner.reclaim_completed_selected(notices, || {
                racing_ticket = Some(
                    super::authority::issue_ticket(
                        reg,
                        ConnId(1),
                        cursor.clone(),
                        None,
                        true,
                        Instant::now(),
                    )
                    .unwrap()
                    .ticket,
                );
            })
        });
        assert_eq!(journal.position().0, 0);
        assert_eq!(
            journal
                .budget()
                .used(super::slice_budget::ChargeKind::ReliableData),
            held_charge
        );
        assert!(replay.authority.as_ref().unwrap().state.cancelled());
        assert!(matches!(replay.next(), Err(F::SessionFenced)));
        assert!(matches!(
            super::authority::consume_ticket(
                reg,
                ConnId(2),
                racing_ticket.unwrap(),
                Instant::now()
            ),
            Err(F::TicketInvalid)
        ));
        assert!(matches!(
            super::authority::issue_ticket(reg, ConnId(1), cursor, None, true, Instant::now()),
            Err(F::SessionFenced)
        ));
        assert!(matches!(
            super::reliable::begin_replay(&journal, 0),
            Err(F::SessionFenced)
        ));
        drop(replay);
        assert_eq!(
            journal
                .budget()
                .used(super::slice_budget::ChargeKind::ReliableData),
            held_charge
        );
        drop(record);
        assert_eq!(
            journal
                .budget()
                .used(super::slice_budget::ChargeKind::ReliableData),
            0
        );
        writer.shutdown();
        host.shutdown().unwrap();
    }

    #[test]
    fn pty_compaction_settles_final_write_and_unresolved_claim() {
        use fl_host_proto::{ConnId, Cursor, ExtensionRefusal as F};
        use std::{
            future::Future,
            task::{Context, Poll, Waker},
        };
        let dir = tempfile::tempdir().unwrap();
        let host = Host::open(OpenOptions::new(HostConfig::new(unresolved_endpoint(
            dir.path(),
        ))))
        .unwrap();
        let (session, journal, writer) = retained_pty_fixture(&host, "pending-claim", 1);
        let reg = &host.inner.authority;
        reg.connect(ConnId(1)).unwrap();
        reg.connect(ConnId(2)).unwrap();
        let mut claim = Box::pin(super::authority::claim(reg, ConnId(1), journal.stream()));
        let mut cx = Context::from_waker(Waker::noop());
        assert!(matches!(claim.as_mut().poll(&mut cx), Poll::Pending));
        journal.eof();
        session.note_cleanup(CleanupReport { complete: true });
        host.inner.maintain_completed();
        assert!(
            journal.compacted(),
            "unresolved claim must not retain completed PTY"
        );
        drop(claim); // ClaimAdmission reenters registry state; must not deadlock.
        let (s2, j2, w2) = retained_pty_fixture(&host, "pending-write", 2);
        super::reliable::append(
            &j2,
            super::reliable::ReliableBody::Line(j2.allocate(20).unwrap()),
        )
        .unwrap();
        j2.eof();
        s2.note_cleanup(CleanupReport { complete: true });
        let grant = host
            .inner
            .handle
            .block_on(super::authority::claim(reg, ConnId(1), j2.stream()))
            .unwrap();
        let ticket = super::authority::issue_ticket(
            reg,
            ConnId(1),
            Cursor {
                stream: j2.stream().clone(),
                seq: 0,
            },
            Some(grant.lease),
            false,
            Instant::now(),
        )
        .unwrap()
        .ticket;
        let replay =
            super::authority::consume_ticket(reg, ConnId(2), ticket, Instant::now()).unwrap();
        let final_write = reg
            .begin_final_write(&replay, 1, Instant::now() + Duration::from_secs(5))
            .unwrap()
            .unwrap();
        let mut ack = Box::pin(reg.acknowledge(
            ConnId(1),
            Cursor {
                stream: j2.stream().clone(),
                seq: 1,
            },
            grant.lease,
        ));
        let _enter = host.inner.handle.enter();
        assert!(matches!(ack.as_mut().poll(&mut cx), Poll::Pending));
        host.inner.maintain_completed();
        assert!(matches!(
            ack.as_mut().poll(&mut cx),
            Poll::Ready(Err(F::SessionFenced))
        ));
        assert_eq!(j2.position().0, 0);
        drop(ack);
        drop(final_write);
        drop(replay);
        drop(_enter);
        writer.shutdown();
        w2.shutdown();
        host.shutdown().unwrap();
    }

    #[test]
    fn completed_pty_compacts_unacked_journal_but_retains_tail_inventory() {
        let dir = tempfile::tempdir().unwrap();
        let host = Host::open(OpenOptions::new(HostConfig::new(unresolved_endpoint(
            dir.path(),
        ))))
        .unwrap();
        let stream = fl_host_proto::StreamId {
            host: host.inner.authority.instance().clone(),
            session: SessionId::new("completed-pty").unwrap(),
            epoch: fl_host_proto::SessionEpoch(1),
        };
        let journal = host
            .inner
            .reserve_reliable_journal_with_mode(stream.clone(), true)
            .unwrap();
        let session = Arc::new(Session::with_reliable(
            SessionParts {
                session: stream.session.clone(),
                epoch: 1,
                vendor_pid: 0,
                vendor_start: 0,
                supervisor_pid: 0,
                supervisor_start: 0,
                pgid: 0,
                control_inode: 0,
                control: None,
                stdin: None,
            },
            Some(Arc::clone(&journal)),
        ));
        let output =
            super::output::OutputSession::open(Arc::clone(&session), Arc::clone(&host.inner.hub));
        let writer = super::input::WriterHandle::start(Arc::clone(&session), None);
        host.inner.authority.register(
            host.inner
                .authority
                .prepare_epoch(Arc::clone(&journal))
                .unwrap(),
            &session,
            &writer,
        );
        let mut bytes = journal.allocate(10).unwrap();
        bytes.as_mut_slice().copy_from_slice(b"final-tail");
        journal.accept_read(bytes).unwrap();
        journal.eof();
        session.note_cleanup(CleanupReport { complete: true });
        let key = (stream.session.as_str().to_owned(), 1);
        host.inner
            .outputs
            .lock()
            .unwrap()
            .insert(key.clone(), output);
        host.inner
            .sessions
            .lock()
            .unwrap()
            .insert(key.0.clone(), Arc::clone(&session));
        assert!(
            journal
                .budget()
                .used(super::slice_budget::ChargeKind::ReliableData)
                > 0
        );
        host.inner.maintain_completed();
        assert_eq!(
            journal
                .budget()
                .used(super::slice_budget::ChargeKind::ReliableData),
            0
        );
        assert_eq!(journal.position().0, 0, "compaction must never invent Ack");
        assert!(host.inner.outputs.lock().unwrap().contains_key(&key));
        writer.shutdown();
        host.shutdown().unwrap();
    }

    #[test]
    fn completed_output_waits_for_epoch_thread_retirement_before_reclamation() {
        let dir = tempfile::tempdir().unwrap();
        let host = Host::open(OpenOptions::new(HostConfig::new(unresolved_endpoint(
            dir.path(),
        ))))
        .unwrap();
        let session = Arc::new(Session::new(SessionParts {
            session: SessionId::new("retire").unwrap(),
            epoch: 1,
            vendor_pid: 0,
            vendor_start: 0,
            supervisor_pid: 0,
            supervisor_start: 0,
            pgid: 0,
            control_inode: 0,
            control: None,
            stdin: None,
        }));
        let output =
            super::output::OutputSession::open(Arc::clone(&session), Arc::clone(&host.inner.hub));
        super::session::StdoutSink::eof(output.as_ref());
        session.note_cleanup(CleanupReport { complete: true });
        let key = ("retire".to_owned(), 1);
        host.inner
            .outputs
            .lock()
            .unwrap()
            .insert(key.clone(), output);
        host.inner
            .sessions
            .lock()
            .unwrap()
            .insert(key.0.clone(), Arc::clone(&session));
        let (release, hold) = std::sync::mpsc::channel();
        host.inner.spawn_epoch_thread(&session, move || {
            hold.recv_timeout(Duration::from_secs(2)).unwrap();
        });
        host.inner.maintain_completed();
        assert!(host.inner.outputs.lock().unwrap().contains_key(&key));
        assert!(host.inner.sessions.lock().unwrap().contains_key(&key.0));
        release.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !host.inner.outputs.lock().unwrap().is_empty() {
            assert!(Instant::now() < deadline, "retired epoch was not reclaimed");
            host.inner.maintain_completed();
            std::thread::yield_now();
        }
        assert!(host.inner.sessions.lock().unwrap().is_empty());
        assert!(host.inner.joins.lock().unwrap().is_empty());
    }

    #[test]
    fn shutdown_waits_for_admitted_spawn_gate_and_rejects_late_spawn() {
        use std::sync::atomic::Ordering;
        let dir = tempfile::tempdir().unwrap();
        let endpoint = unresolved_endpoint(dir.path());
        let host = Arc::new(Host::open(OpenOptions::new(HostConfig::new(endpoint))).unwrap());
        let admitted = Arc::new(std::sync::Barrier::new(2));
        let publish = Arc::new(std::sync::Barrier::new(2));
        let spawning = Arc::clone(&host);
        let at = Arc::clone(&admitted);
        let go = Arc::clone(&publish);
        let spawn = std::thread::spawn(move || {
            Host::admit_spawn(&spawning.inner, |_| {
                at.wait();
                go.wait();
                let session = Arc::new(Session::new(SessionParts {
                    session: SessionId::new("admitted").unwrap(),
                    epoch: 1,
                    vendor_pid: 0,
                    vendor_start: 0,
                    supervisor_pid: 0,
                    supervisor_start: 0,
                    pgid: 0,
                    control_inode: 0,
                    control: None,
                    stdin: None,
                }));
                spawning
                    .inner
                    .sessions
                    .lock()
                    .unwrap()
                    .insert("admitted".into(), Arc::clone(&session));
                let watched = Arc::clone(&session);
                spawning.inner.spawn_epoch_thread(&session, move || {
                    let deadline = Instant::now() + Duration::from_secs(2);
                    while !watched.cleanup_requested() {
                        assert!(Instant::now() < deadline, "admitted session missed cleanup");
                        std::thread::yield_now();
                    }
                    watched.note_cleanup(CleanupReport { complete: true });
                });
                Ok(session)
            })
        });
        admitted.wait();
        let other = Arc::clone(&host);
        let (finished, completion) = std::sync::mpsc::channel();
        let shutdown = std::thread::spawn(move || {
            other.shutdown_inner();
            finished.send(()).unwrap();
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while !host.inner.shutdown_started.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        // The admitted spawn is held at a publication barrier. Shutdown must
        // remain blocked until that admission publishes its session and joins.
        let finished_early = completion.recv_timeout(Duration::from_millis(100)).is_ok();
        let stopped_early = host.inner.stop.load(Ordering::SeqCst);
        publish.wait();
        let session = spawn.join().unwrap().unwrap();
        shutdown.join().unwrap();
        assert!(
            !stopped_early && !finished_early,
            "shutdown published stop or finished before settling admitted spawn"
        );
        assert!(
            session.cleanup_requested(),
            "admitted publication missed cleanup snapshot"
        );
        assert_eq!(
            session.cleanup_report(),
            Some(CleanupReport { complete: true })
        );
        assert!(host.inner.joins.lock().unwrap().is_empty());
        let spec = fl_host_proto::SpawnSpec {
            session: SessionId::new("late").unwrap(),
            kind: fl_host_proto::SpawnKind::Pty,
            argv: vec!["unused".into()],
            cwd: dir.path().to_owned(),
            env: Default::default(),
            env_allow: Default::default(),
        };
        assert!(
            matches!(host.spawn(&spec), Err(fl_host_proto::HostError::Protocol(reason)) if reason == "host shutting down")
        );
        assert!(!host.inner.sessions.lock().unwrap().contains_key("late"));
    }

    #[test]
    fn windows_deadline_endpoints_are_distinct_across_threads() {
        let endpoints = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..16)
                .map(|_| {
                    scope.spawn(|| {
                        unresolved_windows_endpoint("S-1-5-21-123")
                            .socket_path()
                            .to_owned()
                    })
                })
                .collect();
            workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .collect::<std::collections::HashSet<_>>()
        });
        assert_eq!(endpoints.len(), 16);
    }

    #[cfg(windows)]
    #[test]
    fn distinct_deadline_hosts_keep_per_endpoint_singleton() {
        let first_dir = tempfile::tempdir().unwrap();
        let second_dir = tempfile::tempdir().unwrap();
        let first = unresolved_endpoint(first_dir.path());
        let second = unresolved_endpoint(second_dir.path());
        let _first_host = Host::open(OpenOptions::new(HostConfig::new(first.clone()))).unwrap();
        let _second_host = Host::open(OpenOptions::new(HostConfig::new(second))).unwrap();
        assert!(matches!(
            Host::open(OpenOptions::new(HostConfig::new(first))),
            Err(fl_host_proto::HostError::AlreadyRunning)
        ));
    }

    fn unresolved_windows_endpoint(sid: &str) -> HostEndpoint {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let suffix = format!(
            "unres-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        HostEndpoint::windows(&suffix, sid).expect("endpoint")
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
            unresolved_windows_endpoint(&super::current_user_sid().expect("sid"))
        }
        #[cfg(not(any(unix, windows)))]
        {
            HostEndpoint::unix(dir.join("h.sock")).expect("endpoint")
        }
    }
}

#[cfg(test)]
mod endpoint_tests {
    #[test]
    fn windows_full_pipe_name_and_suffix_resolve_identically() {
        let sid = "S-1-5-21-123";
        let full = format!(r"\\.\pipe\fl-host-{sid}-example");
        let endpoint = super::windows_socket_endpoint(&full, sid).unwrap();
        assert_eq!(
            endpoint.socket_path(),
            super::windows_socket_endpoint("example", sid)
                .unwrap()
                .socket_path()
        );
    }

    #[cfg(unix)]
    #[test]
    fn ordinary_socket_path_is_unix_endpoint() {
        let endpoint = super::socket_endpoint("/tmp/host-test.sock").unwrap();
        assert_eq!(
            endpoint.socket_path(),
            std::path::Path::new("/tmp/host-test.sock")
        );
    }
}
