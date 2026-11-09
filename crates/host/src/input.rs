//! Per-session stdin writer. One thread owns the pipe and the lease tuple.
//!
//! Queue admission is not a finished write. A lease moves only after the active
//! OS write has been joined. `ControllerLease::generation_bump_settles_os_write`
//! stays false.

use std::collections::{HashSet, VecDeque};
use std::fs::File;
#[cfg(not(windows))]
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use fl_host_proto::{
    ControllerLease, STDIN_QUEUE_BYTES, STDIN_QUEUE_FRAMES, STDIN_WRITE_TIMEOUT, SendBlock,
    SendOutcome, SignalKind,
};
use tokio::sync::oneshot;

use crate::session::Session;

#[cfg(unix)]
use std::os::fd::AsRawFd;

const CONTROL_LIMIT: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Barrier {
    Waiting,
    Pulsed,
    Cleared,
    Expired,
}

pub(crate) fn barrier_dir() -> Option<PathBuf> {
    std::env::var_os("FL_HOST_WRITER_BARRIER").map(PathBuf::from)
}

pub(crate) fn hold_dir() -> Option<PathBuf> {
    std::env::var_os("FL_HOST_SUBSCRIBER_HOLD").map(PathBuf::from)
}

/// One observation of a barrier. Deadline expiry is reported before `go` is consumed.
pub(crate) fn poll_barrier(dir: &Path, phase: &str, deadline: Option<Instant>) -> Barrier {
    let arm = dir.join(format!("arm-{phase}"));
    if !arm.exists() {
        return Barrier::Cleared;
    }
    let at = dir.join(format!("at-{phase}"));
    let _ = std::fs::write(&at, b"1");
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        return Barrier::Expired;
    }
    let go = dir.join(format!("go-{phase}"));
    if go.exists() {
        let _ = std::fs::remove_file(&go);
        let _ = std::fs::remove_file(&at);
        return if arm.exists() {
            Barrier::Pulsed
        } else {
            Barrier::Cleared
        };
    }
    if arm.exists() {
        Barrier::Waiting
    } else {
        Barrier::Cleared
    }
}

fn chunk_limit() -> usize {
    std::env::var("FL_HOST_WRITE_CHUNK")
        .ok()
        .and_then(|text| text.parse().ok())
        .filter(|size: &usize| *size > 0)
        .unwrap_or(usize::MAX)
}

struct DataJob {
    conn: u64,
    epoch: u64,
    lease: u64,
    bytes: InputBytes,
    reply: oneshot::Sender<SendOutcome>,
}

enum InputBytes {
    Legacy(Vec<u8>),
    Reliable(crate::reliable::ChargedBytes),
}
impl std::ops::Deref for InputBytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            Self::Legacy(bytes) => bytes,
            Self::Reliable(bytes) => bytes.as_slice(),
        }
    }
}

struct ResizeControl {
    authority: std::sync::Weak<crate::authority::AuthorityRegistry>,
    stream: fl_host_proto::StreamId,
    lease: ControllerLease,
    cols: u16,
    rows: u16,
    _metadata: crate::slice_budget::Reservation,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControlError {
    StaleEpoch,
    StaleLease,
    WrongConnection,
    Disconnected,
    WriterStopped,
    Busy,
    Timeout,
    LeaseExhausted,
    Refusal(fl_host_proto::ExtensionRefusal),
    Io(String),
}

impl ControlError {
    pub(crate) fn into_refusal(self) -> fl_host_proto::ExtensionRefusal {
        use fl_host_proto::ExtensionRefusal as Refusal;
        match self {
            Self::WrongConnection | Self::Disconnected => Refusal::WrongConnection,
            Self::StaleLease => Refusal::StaleLease,
            Self::StaleEpoch => Refusal::StaleEpoch,
            Self::Busy | Self::Timeout => Refusal::Busy,
            Self::WriterStopped => Refusal::SessionFenced,
            Self::LeaseExhausted => Refusal::ResourceLimit,
            Self::Refusal(code) => code,
            Self::Io(_) => Refusal::Io,
        }
    }
}

impl std::fmt::Display for ControlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StaleEpoch => f.write_str("stale epoch"),
            Self::StaleLease => f.write_str("stale lease"),
            Self::WrongConnection => f.write_str("wrong connection"),
            Self::Disconnected => f.write_str("disconnected"),
            Self::WriterStopped => f.write_str("writer stopped"),
            Self::Busy => f.write_str("busy"),
            Self::Timeout => f.write_str("timeout"),
            Self::LeaseExhausted => f.write_str("lease exhausted"),
            Self::Refusal(code) => code.fmt(f),
            Self::Io(reason) => f.write_str(reason),
        }
    }
}

enum CtrlKind {
    Resize(ResizeControl),
    Claim,
    Signal(SignalKind),
    Kill { lease: Option<ControllerLease> },
}

struct CtrlJob {
    kind: CtrlKind,
    conn: u64,
    epoch: u64,
    lease: u64,
    admitted: Instant,
    reply: oneshot::Sender<Result<u64, ControlError>>,
}

struct State {
    stdin: Option<File>,
    lease: u64,
    owner: u64,
    fenced: bool,
    data: VecDeque<DataJob>,
    control: VecDeque<CtrlJob>,
    claim_conns: HashSet<u64>,
    revoked: HashSet<u64>,
    queued_bytes: usize,
    active: bool,
    inflight: Option<u64>,
    active_bytes: usize,
    stop: bool,
    /// Set when the active write's completion could not be proven.
    uncertain: bool,
}

pub struct WriterHandle {
    state: Mutex<State>,
    cv: Condvar,
    session: Arc<Session>,
    barrier: Option<PathBuf>,
    chunk: usize,
}

impl WriterHandle {
    pub(crate) fn mode(&self) -> fl_host_proto::SessionMode {
        self.session.mode()
    }

    pub fn start(session: Arc<Session>, stdin: Option<File>) -> Arc<Self> {
        #[cfg(unix)]
        if let Some(file) = stdin.as_ref() {
            let fd = file.as_raw_fd();
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags >= 0 {
                unsafe {
                    libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
                }
            }
        }
        Arc::new(Self {
            state: Mutex::new(State {
                stdin,
                lease: 0,
                owner: 0,
                fenced: false,
                data: VecDeque::new(),
                control: VecDeque::new(),
                claim_conns: HashSet::new(),
                revoked: HashSet::new(),
                queued_bytes: 0,
                active: false,
                inflight: None,
                active_bytes: 0,
                stop: false,
                uncertain: false,
            }),
            cv: Condvar::new(),
            session,
            barrier: barrier_dir(),
            chunk: chunk_limit(),
        })
    }

    pub fn shutdown(&self) {
        let mut state = self.lock();
        state.stop = true;
        self.invalidate_queued(&mut state, SendBlock::SessionFenced);
        for job in state.control.drain(..) {
            let _ = job.reply.send(Err(ControlError::WriterStopped));
        }
        state.claim_conns.clear();
        self.cv.notify_all();
    }

    fn completed(&self) -> bool {
        self.session
            .cleanup_report()
            .is_some_and(|report| report.complete)
    }

    pub fn fence(&self) {
        let mut state = self.lock();
        self.fence_locked(&mut state);
        self.cv.notify_all();
    }

    pub fn send(
        &self,
        conn: u64,
        epoch: u64,
        lease: ControllerLease,
        line: String,
    ) -> Result<oneshot::Receiver<SendOutcome>, SendOutcome> {
        if line.as_bytes().contains(&b'\n') {
            return Err(SendOutcome::NotWritten {
                reason: SendBlock::Io,
            });
        }
        let mut bytes = line.into_bytes();
        bytes.push(b'\n');
        self.enqueue_data(conn, epoch, lease, InputBytes::Legacy(bytes))
    }

    pub(crate) fn send_reliable(
        &self,
        conn: u64,
        epoch: u64,
        lease: ControllerLease,
        line: String,
        budget: &Arc<crate::slice_budget::SliceBudget>,
    ) -> Result<Result<oneshot::Receiver<SendOutcome>, SendOutcome>, fl_host_proto::ExtensionRefusal>
    {
        if line.as_bytes().contains(&b'\n') {
            return Ok(Err(SendOutcome::NotWritten {
                reason: SendBlock::Io,
            }));
        }
        let len = line
            .len()
            .checked_add(1)
            .ok_or(fl_host_proto::ExtensionRefusal::ResourceLimit)?;
        // The continuation already covers the original String. Admit the entire
        // concurrent copy before allocating, then carry its charge in the job.
        let reservation =
            crate::slice_budget::reserve(budget, crate::slice_budget::ChargeKind::Metadata, len)?;
        let mut bytes = reservation.allocate_bytes(len)?;
        bytes.as_mut_slice()[..line.len()].copy_from_slice(line.as_bytes());
        bytes.as_mut_slice()[line.len()] = b'\n';
        drop(line);
        Ok(self.enqueue_data(conn, epoch, lease, InputBytes::Reliable(bytes)))
    }

    pub(crate) fn write_raw_reliable(
        &self,
        conn: u64,
        epoch: u64,
        lease: ControllerLease,
        raw: Vec<u8>,
        budget: &Arc<crate::slice_budget::SliceBudget>,
    ) -> Result<Result<oneshot::Receiver<SendOutcome>, SendOutcome>, fl_host_proto::ExtensionRefusal>
    {
        if raw.is_empty() || raw.len() > fl_host_proto::MAX_RAW_INPUT {
            return Ok(Err(SendOutcome::NotWritten {
                reason: SendBlock::Io,
            }));
        }
        let len = raw.len();
        let reservation =
            crate::slice_budget::reserve(budget, crate::slice_budget::ChargeKind::Metadata, len)?;
        let mut bytes = reservation.allocate_bytes(len)?;
        bytes.as_mut_slice().copy_from_slice(&raw);
        drop(raw);
        Ok(self.enqueue_data(conn, epoch, lease, InputBytes::Reliable(bytes)))
    }

    fn enqueue_data(
        &self,
        conn: u64,
        epoch: u64,
        lease: ControllerLease,
        bytes: InputBytes,
    ) -> Result<oneshot::Receiver<SendOutcome>, SendOutcome> {
        let (tx, rx) = oneshot::channel();
        let mut state = self.lock();
        let outcome = self.classify_send(&state, conn, epoch, lease, bytes.len());
        if let Some(outcome) = outcome {
            return Err(outcome);
        }
        state.queued_bytes = state.queued_bytes.saturating_add(bytes.len());
        state.data.push_back(DataJob {
            conn,
            epoch,
            lease: lease.0,
            bytes,
            reply: tx,
        });
        self.publish_depth(&state);
        self.cv.notify_all();
        Ok(rx)
    }

    pub fn claim(
        &self,
        conn: u64,
        epoch: u64,
    ) -> Result<oneshot::Receiver<Result<u64, ControlError>>, ControlError> {
        self.enqueue_ctrl(conn, epoch, 0, CtrlKind::Claim, true)
    }

    pub(crate) fn resize(
        &self,
        conn: fl_host_proto::ConnId,
        stream: fl_host_proto::StreamId,
        leases: (ControllerLease, ControllerLease),
        geometry: (u16, u16),
        authority: &Arc<crate::authority::AuthorityRegistry>,
        budget: &Arc<crate::slice_budget::SliceBudget>,
    ) -> Result<oneshot::Receiver<Result<u64, ControlError>>, fl_host_proto::ExtensionRefusal> {
        use fl_host_proto::ExtensionRefusal as Refusal;
        let (cols, rows) = geometry;
        let (authority_lease, writer_lease) = leases;
        fl_host_proto::validate_pty_geometry(cols, rows).map_err(|_| Refusal::ResourceLimit)?;
        if !cfg!(unix) {
            return Err(Refusal::Unsupported);
        }
        if self.mode() != fl_host_proto::SessionMode::Pty {
            return Err(Refusal::WrongMode);
        }
        // Bound queue growth, the control payload and oneshot allocation before enqueue.
        // This reservation remains in the job until execution/cancellation completes.
        let metadata = crate::slice_budget::reserve(
            budget,
            crate::slice_budget::ChargeKind::Metadata,
            4 * std::mem::size_of::<CtrlJob>() + 1024,
        )?;
        let epoch = stream.epoch.0;
        let rx = self
            .enqueue_ctrl(
                conn.0,
                epoch,
                writer_lease.0,
                CtrlKind::Resize(ResizeControl {
                    authority: Arc::downgrade(authority),
                    stream,
                    lease: authority_lease,
                    cols,
                    rows,
                    _metadata: metadata,
                }),
                false,
            )
            .map_err(ControlError::into_refusal)?;
        if let Some(dir) = self.barrier.as_ref() {
            let _ = std::fs::write(dir.join(format!("queued-resize-{cols}-{rows}")), b"1");
        }
        Ok(rx)
    }

    pub fn signal(
        &self,
        conn: u64,
        epoch: u64,
        lease: ControllerLease,
        signal: SignalKind,
    ) -> Result<oneshot::Receiver<Result<u64, ControlError>>, ControlError> {
        let mut state = self.lock();
        if let Some(reason) = self.classify_signal(&state, conn, epoch, lease.0) {
            return Err(reason);
        }
        if state.control.len() >= CONTROL_LIMIT {
            return Err(ControlError::Busy);
        }
        let (tx, rx) = oneshot::channel();
        state.control.push_back(CtrlJob {
            kind: CtrlKind::Signal(signal),
            conn,
            epoch,
            lease: lease.0,
            admitted: Instant::now(),
            reply: tx,
        });
        self.cv.notify_all();
        Ok(rx)
    }

    pub fn kill(
        &self,
        conn: u64,
        epoch: u64,
        lease: Option<ControllerLease>,
    ) -> Result<oneshot::Receiver<Result<u64, ControlError>>, ControlError> {
        let mut state = self.lock();
        if epoch != self.session.epoch {
            return Err(ControlError::StaleEpoch);
        }
        if state.stop || self.completed() {
            return Err(ControlError::WriterStopped);
        }
        if let Some(lease) = lease {
            if state.fenced {
                // An administrative lease is optional. A supplied one still has to match.
            }
            if lease.0 != state.lease || state.lease == 0 {
                return Err(ControlError::StaleLease);
            }
            if state.owner != conn {
                return Err(ControlError::WrongConnection);
            }
        }
        if state.control.len() >= CONTROL_LIMIT {
            return Err(ControlError::Busy);
        }
        let (tx, rx) = oneshot::channel();
        state.control.push_back(CtrlJob {
            kind: CtrlKind::Kill { lease },
            conn,
            epoch,
            lease: lease.map(|lease| lease.0).unwrap_or(0),
            admitted: Instant::now(),
            reply: tx,
        });
        self.cv.notify_all();
        Ok(rx)
    }

    pub fn revoke(&self, conn: u64) {
        let mut state = self.lock();
        if state.inflight == Some(conn) {
            state.revoked.insert(conn);
        }
        if state.owner == conn {
            state.owner = 0;
        }
        let mut kept = VecDeque::new();
        while let Some(job) = state.data.pop_front() {
            if job.conn == conn {
                state.queued_bytes = state.queued_bytes.saturating_sub(job.bytes.len());
                let _ = job.reply.send(SendOutcome::NotWritten {
                    reason: SendBlock::WrongConnection,
                });
            } else {
                kept.push_back(job);
            }
        }
        state.data = kept;
        let mut control = VecDeque::new();
        while let Some(job) = state.control.pop_front() {
            if job.conn == conn {
                if matches!(job.kind, CtrlKind::Claim) {
                    state.claim_conns.remove(&conn);
                }
                let _ = job.reply.send(Err(ControlError::Disconnected));
            } else {
                control.push_back(job);
            }
        }
        state.control = control;
        self.publish_depth(&state);
        if let Some(dir) = self.barrier.as_ref() {
            let _ = std::fs::write(dir.join("revoked"), b"1");
        }
        self.cv.notify_all();
    }

    fn enqueue_ctrl(
        &self,
        conn: u64,
        epoch: u64,
        lease: u64,
        kind: CtrlKind,
        claim: bool,
    ) -> Result<oneshot::Receiver<Result<u64, ControlError>>, ControlError> {
        let mut state = self.lock();
        if epoch != self.session.epoch {
            return Err(ControlError::StaleEpoch);
        }
        if state.stop || self.completed() {
            return Err(ControlError::WriterStopped);
        }
        if state.control.len() >= CONTROL_LIMIT {
            return Err(ControlError::Busy);
        }
        if claim && !state.claim_conns.insert(conn) {
            return Err(ControlError::Busy);
        }
        let (tx, rx) = oneshot::channel();
        state.control.push_back(CtrlJob {
            kind,
            conn,
            epoch,
            lease,
            admitted: Instant::now(),
            reply: tx,
        });
        self.cv.notify_all();
        Ok(rx)
    }

    fn classify_send(
        &self,
        state: &State,
        conn: u64,
        epoch: u64,
        lease: ControllerLease,
        extra: usize,
    ) -> Option<SendOutcome> {
        if epoch != self.session.epoch {
            return Some(SendOutcome::NotWritten {
                reason: SendBlock::StaleEpoch,
            });
        }
        if state.stop || self.completed() {
            return Some(SendOutcome::NotWritten {
                reason: SendBlock::SessionFenced,
            });
        }
        if state.fenced {
            return Some(SendOutcome::NotWritten {
                reason: SendBlock::SessionFenced,
            });
        }
        if lease.0 == 0 || lease.0 != state.lease {
            return Some(SendOutcome::NotWritten {
                reason: SendBlock::StaleLease,
            });
        }
        if state.owner != conn {
            return Some(SendOutcome::NotWritten {
                reason: SendBlock::WrongConnection,
            });
        }
        let frames = state.data.len() + usize::from(state.active);
        let bytes = state.queued_bytes.saturating_add(state.active_bytes);
        if frames >= STDIN_QUEUE_FRAMES || bytes.saturating_add(extra) > STDIN_QUEUE_BYTES {
            return Some(SendOutcome::NotWritten {
                reason: SendBlock::QueueFull,
            });
        }
        None
    }

    fn classify_signal(
        &self,
        state: &State,
        conn: u64,
        epoch: u64,
        lease: u64,
    ) -> Option<ControlError> {
        if epoch != self.session.epoch {
            return Some(ControlError::StaleEpoch);
        }
        if state.stop || self.completed() {
            return Some(ControlError::WriterStopped);
        }
        if lease == 0 || lease != state.lease {
            return Some(ControlError::StaleLease);
        }
        if state.owner != conn {
            return Some(ControlError::WrongConnection);
        }
        None
    }

    pub(crate) fn run(self: Arc<Self>, host_stop: impl Fn() -> bool) {
        loop {
            let next = {
                let mut state = self.lock();
                loop {
                    if state.stop || host_stop() || self.completed() {
                        drop(state);
                        self.shutdown();
                        return;
                    }
                    if let Some(job) = state.control.pop_front() {
                        state.inflight = Some(job.conn);
                        break Job::Ctrl(job);
                    }
                    if let Some(job) = state.data.pop_front() {
                        state.queued_bytes = state.queued_bytes.saturating_sub(job.bytes.len());
                        state.inflight = Some(job.conn);
                        state.active = true;
                        state.active_bytes = job.bytes.len();
                        self.publish_depth(&state);
                        break Job::Data(job);
                    }
                    let (guard, _) = self
                        .cv
                        .wait_timeout(state, Duration::from_millis(200))
                        .unwrap_or_else(|err| err.into_inner());
                    state = guard;
                }
            };
            match next {
                Job::Ctrl(job) => self.run_ctrl(job),
                Job::Data(job) => self.run_send(job),
            }
            let mut state = self.lock();
            if let Some(conn) = state.inflight.take() {
                state.revoked.remove(&conn);
            }
        }
    }

    fn run_ctrl(&self, job: CtrlJob) {
        if let CtrlKind::Resize(ref resize) = job.kind {
            let result = self.run_resize(&job, resize);
            let _ = job.reply.send(result.map_err(ControlError::Refusal));
            return;
        }
        match job.kind {
            CtrlKind::Resize(_) => unreachable!("handled above"),
            CtrlKind::Claim => self.run_claim(job),
            CtrlKind::Signal(signal) => self.run_signal(job, signal),
            CtrlKind::Kill { lease } => self.run_kill(job, lease),
        }
    }

    fn run_resize(
        &self,
        job: &CtrlJob,
        resize: &ResizeControl,
    ) -> Result<u64, fl_host_proto::ExtensionRefusal> {
        use fl_host_proto::ExtensionRefusal as Refusal;
        let deadline = job.admitted + STDIN_WRITE_TIMEOUT;
        // Fixture gates release without holding either fence; timeout never renews.
        while let Some(dir) = self.barrier.as_ref() {
            if self.lock().stop {
                return Err(Refusal::SessionFenced);
            }
            match poll_barrier(dir, "before-resize", Some(deadline)) {
                Barrier::Cleared => break,
                Barrier::Expired => return Err(Refusal::Busy),
                Barrier::Pulsed | Barrier::Waiting => thread::sleep(Duration::from_millis(5)),
            }
        }
        let authority = resize.authority.upgrade().ok_or(Refusal::SessionFenced)?;
        authority.with_mutation_fence(
            fl_host_proto::ConnId(job.conn),
            &resize.stream,
            resize.lease,
            || {
                let state = self.lock();
                if state.stop || state.fenced || self.completed() {
                    return Err(Refusal::SessionFenced);
                }
                if state.revoked.contains(&job.conn) {
                    return Err(Refusal::WrongConnection);
                }
                if job.epoch != self.session.epoch {
                    return Err(Refusal::StaleEpoch);
                }
                if job.lease == 0 || job.lease != state.lease {
                    return Err(Refusal::StaleLease);
                }
                if state.owner != job.conn {
                    return Err(Refusal::WrongConnection);
                }
                if Instant::now() >= deadline {
                    return Err(Refusal::Busy);
                }
                // Registry and writer locks cover both kernel and parser mutation.
                self.session
                    .resize_pty(resize.cols, resize.rows)
                    .map_err(|error| match error {
                        crate::session::ResizeError::SessionFenced => Refusal::SessionFenced,
                        crate::session::ResizeError::WrongMode => Refusal::WrongMode,
                        crate::session::ResizeError::Host(
                            fl_host_proto::HostError::Unsupported { .. },
                        ) => Refusal::Unsupported,
                        crate::session::ResizeError::Host(fl_host_proto::HostError::Io(_)) => {
                            Refusal::Io
                        }
                        crate::session::ResizeError::Host(_) => Refusal::ResourceLimit,
                    })
            },
        )
    }

    fn run_claim(&self, job: CtrlJob) {
        self.pause_phase("before-claim");
        let mut state = self.lock();
        if state.stop || state.revoked.contains(&job.conn) {
            state.claim_conns.remove(&job.conn);
            let _ = job.reply.send(Err(ControlError::Disconnected));
            return;
        }
        if job.epoch != self.session.epoch {
            state.claim_conns.remove(&job.conn);
            let _ = job.reply.send(Err(ControlError::StaleEpoch));
            return;
        }
        self.invalidate_queued(&mut state, SendBlock::StaleLease);
        if state.uncertain {
            self.fence_locked(&mut state);
            state.claim_conns.remove(&job.conn);
            state.uncertain = false;
            let _ = job.reply.send(Err(ControlError::Timeout));
            return;
        }
        let Some(next) = state.lease.checked_add(1).filter(|next| *next != 0) else {
            state.claim_conns.remove(&job.conn);
            let _ = job.reply.send(Err(ControlError::LeaseExhausted));
            return;
        };
        if Instant::now() >= job.admitted + STDIN_WRITE_TIMEOUT {
            state.claim_conns.remove(&job.conn);
            let _ = job.reply.send(Err(ControlError::Timeout));
            return;
        }
        state.lease = next;
        state.owner = job.conn;
        state.claim_conns.remove(&job.conn);
        let _ = job.reply.send(Ok(next));
    }

    fn run_signal(&self, job: CtrlJob, signal: SignalKind) {
        self.pause_phase("before-signal");
        let state = self.lock();
        if state.stop {
            let _ = job.reply.send(Err(ControlError::Disconnected));
            return;
        }
        if let Some(reason) = self.classify_signal(&state, job.conn, job.epoch, job.lease) {
            let _ = job.reply.send(Err(reason));
            return;
        }
        drop(state);
        let result = signal_session(&self.session, signal);
        let _ = job.reply.send(result.map(|()| 0).map_err(ControlError::Io));
    }

    fn run_kill(&self, job: CtrlJob, lease: Option<ControllerLease>) {
        self.pause_phase("before-kill");
        let state = self.lock();
        if job.epoch != self.session.epoch {
            let _ = job.reply.send(Err(ControlError::StaleEpoch));
            return;
        }
        if let Some(lease) = lease
            && (lease.0 != state.lease || state.owner != job.conn || state.lease == 0)
        {
            let reason = if lease.0 != state.lease || state.lease == 0 {
                ControlError::StaleLease
            } else {
                ControlError::WrongConnection
            };
            let _ = job.reply.send(Err(reason));
            return;
        }
        drop(state);
        let result = self.session.request_cleanup();
        let _ = job.reply.send(
            result
                .map(|()| 0)
                .map_err(|err| ControlError::Io(err.to_string())),
        );
    }

    fn send_block(&self, job: &DataJob) -> Option<SendBlock> {
        let state = self.lock();
        if job.epoch != self.session.epoch {
            return Some(SendBlock::StaleEpoch);
        }
        if state.fenced {
            return Some(SendBlock::SessionFenced);
        }
        if state.revoked.contains(&job.conn) {
            return Some(SendBlock::WrongConnection);
        }
        if job.lease == 0 || job.lease != state.lease || state.owner != job.conn {
            return Some(SendBlock::StaleLease);
        }
        if state
            .control
            .iter()
            .any(|ctrl| matches!(ctrl.kind, CtrlKind::Claim))
        {
            return Some(SendBlock::StaleLease);
        }
        None
    }

    fn run_send(&self, job: DataJob) {
        if let Some(reason) = self.send_block(&job) {
            self.finish_active(0);
            self.reply_refused(job, reason);
            return;
        }
        self.pause_before_first(job.conn);
        if self.lock().stop {
            self.finish_active(0);
            self.reply_refused(job, SendBlock::Io);
            return;
        }
        if let Some(reason) = self.send_block(&job) {
            self.finish_active(0);
            self.reply_refused(job, reason);
            return;
        }
        let deadline = Instant::now() + STDIN_WRITE_TIMEOUT;
        let mut written = 0usize;
        while written < job.bytes.len() {
            if Instant::now() >= deadline {
                self.abandon(job, written, SendBlock::Timeout);
                return;
            }
            if written > 0 && self.chunk != usize::MAX && written.is_multiple_of(self.chunk) {
                match self.pause_after_chunk(deadline, job.conn) {
                    Pause::Go => {}
                    Pause::Expired => {
                        self.abandon(job, written, SendBlock::Timeout);
                        return;
                    }
                    Pause::Preempt => {
                        self.abandon(job, written, SendBlock::Io);
                        return;
                    }
                }
            }
            if self.preempt(job.conn) {
                self.abandon(job, written, SendBlock::StaleLease);
                return;
            }
            let end = written.saturating_add(self.chunk).min(job.bytes.len());
            let mut file = {
                let mut state = self.lock();
                if state.fenced {
                    drop(state);
                    self.abandon(job, written, SendBlock::SessionFenced);
                    return;
                }
                state.stdin.take()
            };
            let step = match file.as_mut() {
                Some(file) => write_some(file, &job.bytes[written..end], deadline),
                None => WriteStep::Closed,
            };
            {
                let mut state = self.lock();
                if state.fenced {
                    drop(file);
                } else {
                    state.stdin = file;
                }
            }
            match step {
                WriteStep::Wrote(0) | WriteStep::Timeout if written == 0 => {
                    self.abandon(job, 0, SendBlock::Timeout);
                    return;
                }
                WriteStep::Timeout => {
                    self.abandon(job, written, SendBlock::Timeout);
                    return;
                }
                #[cfg(windows)]
                WriteStep::Uncertain => {
                    // The overlapped write was joined. The count is still not proven,
                    // so the epoch stays fenced and a waiting claim must not succeed.
                    self.finish_active(0);
                    let mut state = self.lock();
                    state.uncertain = true;
                    self.fence_locked(&mut state);
                    let _ = job.reply.send(SendOutcome::Ambiguous {
                        reason: SendBlock::Timeout,
                    });
                    return;
                }
                WriteStep::Wrote(n) => written = written.saturating_add(n),
                WriteStep::Closed => {
                    self.abandon(job, written, SendBlock::Io);
                    return;
                }
                WriteStep::Err => {
                    self.abandon(job, written, SendBlock::Io);
                    return;
                }
            }
        }
        match self.pause_pending(deadline) {
            Pause::Go => {
                self.finish_active(0);
                let mut state = self.lock();
                state.uncertain = false;
                let _ = job.reply.send(SendOutcome::Written);
            }
            Pause::Expired => {
                self.finish_active(0);
                let mut state = self.lock();
                state.uncertain = true;
                self.fence_locked(&mut state);
                let _ = job.reply.send(SendOutcome::Ambiguous {
                    reason: SendBlock::Timeout,
                });
            }
            Pause::Preempt => {
                self.abandon(job, written, SendBlock::Io);
            }
        }
    }

    /// Zero bytes is a proven refusal and does not fence. Any abandoned byte fences.
    fn abandon(&self, job: DataJob, written: usize, reason: SendBlock) {
        self.finish_active(0);
        if written == 0 {
            let _ = job.reply.send(SendOutcome::NotWritten { reason });
            return;
        }
        let mut state = self.lock();
        state.uncertain = false;
        self.fence_locked(&mut state);
        let _ = job.reply.send(SendOutcome::Ambiguous { reason });
    }

    fn reply_refused(&self, job: DataJob, reason: SendBlock) {
        let _ = job.reply.send(SendOutcome::NotWritten { reason });
    }

    fn finish_active(&self, active_bytes: usize) {
        let mut state = self.lock();
        state.active = false;
        state.active_bytes = active_bytes;
        self.publish_depth(&state);
    }

    fn preempt(&self, conn: u64) -> bool {
        let state = self.lock();
        state.revoked.contains(&conn)
            || state
                .control
                .iter()
                .any(|job| matches!(job.kind, CtrlKind::Claim))
            || state.fenced
    }

    fn pause_before_first(&self, conn: u64) {
        loop {
            if self.preempt(conn) || self.lock().stop {
                return;
            }
            let Some(dir) = self.barrier.as_ref() else {
                return;
            };
            match poll_barrier(dir, "before-first", None) {
                Barrier::Cleared => return,
                Barrier::Pulsed | Barrier::Waiting | Barrier::Expired => {
                    thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }

    fn pause_after_chunk(&self, deadline: Instant, conn: u64) -> Pause {
        loop {
            if self.lock().stop || Instant::now() >= deadline {
                return Pause::Expired;
            }
            if self.preempt(conn) {
                return Pause::Preempt;
            }
            let Some(dir) = self.barrier.as_ref() else {
                return Pause::Go;
            };
            match poll_barrier(dir, "after-chunk", Some(deadline)) {
                Barrier::Expired => return Pause::Expired,
                Barrier::Cleared | Barrier::Pulsed => return Pause::Go,
                Barrier::Waiting => thread::sleep(Duration::from_millis(5)),
            }
        }
    }

    fn pause_pending(&self, deadline: Instant) -> Pause {
        loop {
            let claim_deadline = {
                let state = self.lock();
                state
                    .control
                    .iter()
                    .find(|job| matches!(job.kind, CtrlKind::Claim))
                    .map(|job| job.admitted + STDIN_WRITE_TIMEOUT)
            };
            if self.lock().stop {
                return Pause::Expired;
            }
            let now = Instant::now();
            if now >= deadline || claim_deadline.is_some_and(|limit| now >= limit) {
                return Pause::Expired;
            }
            let Some(dir) = self.barrier.as_ref() else {
                return Pause::Go;
            };
            let limit = claim_deadline
                .map(|limit| limit.min(deadline))
                .unwrap_or(deadline);
            match poll_barrier(dir, "pending", Some(limit)) {
                Barrier::Expired => return Pause::Expired,
                Barrier::Cleared => return Pause::Go,
                // A `go` pulse is progress. It does not prove the syscall finished.
                Barrier::Pulsed | Barrier::Waiting => thread::sleep(Duration::from_millis(5)),
            }
        }
    }

    fn pause_phase(&self, phase: &str) {
        loop {
            if self.lock().stop {
                return;
            }
            let Some(dir) = self.barrier.as_ref() else {
                return;
            };
            match poll_barrier(dir, phase, None) {
                Barrier::Cleared => return,
                Barrier::Pulsed | Barrier::Waiting | Barrier::Expired => {
                    thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }

    fn invalidate_queued(&self, state: &mut State, reason: SendBlock) {
        while let Some(job) = state.data.pop_front() {
            state.queued_bytes = state.queued_bytes.saturating_sub(job.bytes.len());
            let _ = job.reply.send(SendOutcome::NotWritten { reason });
        }
        self.publish_depth(state);
    }

    fn fence_locked(&self, state: &mut State) {
        if !state.fenced {
            state.fenced = true;
            state.stdin.take();
            self.session.set_input(fl_host_proto::InputState::Fenced);
        } else {
            state.stdin.take();
        }
    }

    fn publish_depth(&self, state: &State) {
        let Some(dir) = self.barrier.as_ref() else {
            return;
        };
        let _ = std::fs::write(dir.join("depth"), state.data.len().to_string());
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|err| err.into_inner())
    }
}

enum Job {
    Data(DataJob),
    Ctrl(CtrlJob),
}

enum Pause {
    Go,
    Expired,
    Preempt,
}

enum WriteStep {
    Wrote(usize),
    /// Deadline expired before this call started an OS write.
    Timeout,
    /// A started Windows write was cancelled and its byte count could not be proven.
    #[cfg(windows)]
    Uncertain,
    Closed,
    Err,
}

fn write_some(file: &mut File, buf: &[u8], deadline: Instant) -> WriteStep {
    #[cfg(windows)]
    {
        write_some_windows(file, buf, deadline)
    }
    #[cfg(not(windows))]
    {
        if buf.is_empty() {
            return WriteStep::Wrote(0);
        }
        loop {
            if Instant::now() >= deadline {
                return WriteStep::Timeout;
            }
            match file.write(buf) {
                Ok(0) => return WriteStep::Closed,
                Ok(n) => return WriteStep::Wrote(n),
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    if !wait_writable(file, deadline) {
                        return WriteStep::Timeout;
                    }
                }
                Err(_) => return WriteStep::Err,
            }
        }
    }
}

#[cfg(unix)]
fn wait_writable(file: &File, deadline: Instant) -> bool {
    let remain = deadline.saturating_duration_since(Instant::now());
    if remain.is_zero() {
        return false;
    }
    let millis = remain.as_millis().clamp(1, 50) as i32;
    let mut pollfd = libc::pollfd {
        fd: file.as_raw_fd(),
        events: libc::POLLOUT,
        revents: 0,
    };
    let rc = unsafe { libc::poll(&mut pollfd, 1, millis) };
    rc >= 0 || io::Error::last_os_error().kind() == io::ErrorKind::Interrupted
}

#[cfg(all(not(unix), not(windows)))]
fn wait_writable(_file: &File, deadline: Instant) -> bool {
    let remain = deadline.saturating_duration_since(Instant::now());
    if remain.is_zero() {
        return false;
    }
    thread::sleep(remain.min(Duration::from_millis(50)));
    Instant::now() < deadline
}

#[cfg(windows)]
fn write_some_windows(file: &mut File, buf: &[u8], deadline: Instant) -> WriteStep {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_NO_DATA, ERROR_OPERATION_ABORTED,
        GetLastError, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::Storage::FileSystem::WriteFile;
    use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
    use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

    if buf.is_empty() {
        return WriteStep::Wrote(0);
    }
    if Instant::now() >= deadline {
        return WriteStep::Timeout;
    }
    let handle = file.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE;
    // SAFETY: manual-reset event used only as the overlapped signal for this write.
    let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
    if event.is_null() {
        return WriteStep::Err;
    }
    struct CloseEvent(windows_sys::Win32::Foundation::HANDLE);
    impl Drop for CloseEvent {
        fn drop(&mut self) {
            // SAFETY: `0` is the event created above and not yet closed.
            unsafe { CloseHandle(self.0) };
        }
    }
    let _event = CloseEvent(event);
    let mut overlapped = OVERLAPPED {
        hEvent: event,
        ..Default::default()
    };
    let mut written = 0u32;
    let to_write = u32::try_from(buf.len()).unwrap_or(u32::MAX);
    // SAFETY: `handle` is the session stdin write end, opened overlapped. `buf` stays
    // borrowed until this function joins the operation. `overlapped` stays on this stack.
    let started = unsafe {
        WriteFile(
            handle,
            buf.as_ptr(),
            to_write,
            &mut written,
            &mut overlapped,
        )
    };
    if started != 0 {
        return if written == 0 {
            WriteStep::Closed
        } else {
            WriteStep::Wrote(written as usize)
        };
    }
    let err = unsafe { GetLastError() };
    if err != ERROR_IO_PENDING {
        return if err == ERROR_BROKEN_PIPE || err == ERROR_NO_DATA {
            WriteStep::Closed
        } else {
            WriteStep::Err
        };
    }
    let remain = deadline.saturating_duration_since(Instant::now());
    let millis = u32::try_from(remain.as_millis()).unwrap_or(u32::MAX);
    // SAFETY: `event` is the overlapped event. A timeout returns so the write can be cancelled.
    let waited = unsafe { WaitForSingleObject(event, millis) };
    if waited == WAIT_OBJECT_0 {
        let mut transferred = 0u32;
        // SAFETY: the write completed. `bWait` is false because the event is already signaled.
        let ok = unsafe { GetOverlappedResult(handle, &overlapped, &mut transferred, 0) };
        if ok != 0 {
            return if transferred == 0 {
                WriteStep::Closed
            } else {
                WriteStep::Wrote(transferred as usize)
            };
        }
        let err = unsafe { GetLastError() };
        return if err == ERROR_BROKEN_PIPE || err == ERROR_NO_DATA {
            WriteStep::Closed
        } else {
            WriteStep::Err
        };
    }
    // SAFETY: cancel this operation only. The following get joins it before the buffer ends.
    unsafe { CancelIoEx(handle, &overlapped) };
    let mut transferred = 0u32;
    // SAFETY: `bWait` joins the cancelled or raced write before the caller fences stdin.
    let ok = unsafe { GetOverlappedResult(handle, &overlapped, &mut transferred, 1) };
    if ok != 0 {
        return if transferred == 0 {
            WriteStep::Closed
        } else {
            WriteStep::Wrote(transferred as usize)
        };
    }
    let err = unsafe { GetLastError() };
    if err == ERROR_OPERATION_ABORTED {
        WriteStep::Uncertain
    } else if err == ERROR_BROKEN_PIPE || err == ERROR_NO_DATA {
        WriteStep::Closed
    } else {
        WriteStep::Err
    }
}

fn signal_session(session: &Session, signal: SignalKind) -> Result<(), String> {
    #[cfg(unix)]
    {
        let mapped = match signal {
            SignalKind::Interrupt => libc::SIGINT,
            SignalKind::Terminate => libc::SIGTERM,
            SignalKind::Kill => libc::SIGKILL,
            SignalKind::Hangup => libc::SIGHUP,
        };
        session.signal(mapped)
    }
    #[cfg(windows)]
    {
        if matches!(signal, SignalKind::Interrupt | SignalKind::Hangup) {
            return Err("unsupported Windows signal".into());
        }
        // Windows Terminate and Kill both use TerminateProcess on the retained handle.
        session.terminate_leader().map_err(|err| err.to_string())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (session, signal);
        Err("unanchored group".into())
    }
}

#[cfg(test)]
mod resource_tests {
    use super::*;
    use crate::session::SessionParts;
    use fl_host_proto::SessionId;

    #[test]
    fn idle_disconnection_does_not_retain_revocation_tombstones() {
        let session = Arc::new(Session::new(SessionParts {
            session: SessionId::new("bounded").unwrap(),
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
        let writer = WriterHandle::start(session, None);
        for conn in 1..10_000 {
            writer.revoke(conn);
        }
        assert!(writer.lock().revoked.is_empty());
    }
    fn decoded_exact_line() -> String {
        use fl_host_proto::*;
        let payload = format!(
            r#"{{"op":"extended","id":1,"request":{{"op":"send_line","stream":{{"host":"00000000000000000000000000000000","session":"bounded","epoch":1}},"lease":1,"line":"{}"}}}}"#,
            "x".repeat(65536)
        );
        let request = decode_request_for_minor(
            1,
            &Frame {
                kind: FrameKind::Request,
                payload: payload.into_bytes(),
            },
        )
        .unwrap();
        let HostRequest::Extended {
            request: ExtensionRequest::SendLine { line, .. },
            ..
        } = request
        else {
            panic!("wrong request")
        };
        assert_eq!(line.len(), 65536);
        assert_eq!(line.capacity(), 65536);
        line
    }

    fn parked_reliable_writer() -> Arc<WriterHandle> {
        let session = Arc::new(Session::new(SessionParts {
            session: SessionId::new("bounded").unwrap(),
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
        // No writer runner: actual production queue remains parked deterministically.
        let writer = WriterHandle::start(session, None);
        {
            let mut state = writer.lock();
            state.lease = 1;
            state.owner = 7;
        }
        writer
    }

    #[test]
    fn reliable_input_refuses_before_preparation_when_metadata_is_exhausted() {
        use crate::slice_budget::{ChargeKind, METADATA, SliceBudget, reserve};
        let budget = SliceBudget::new();
        let baseline = budget.used(ChargeKind::Metadata);
        let held = reserve(&budget, ChargeKind::Metadata, METADATA - baseline - 65536).unwrap();
        let writer = parked_reliable_writer();
        assert!(matches!(
            writer.send_reliable(7, 1, ControllerLease(1), decoded_exact_line(), &budget),
            Err(fl_host_proto::ExtensionRefusal::ResourceLimit)
        ));
        assert!(writer.lock().data.is_empty());
        assert_eq!(writer.lock().queued_bytes, 0);
        drop(held);
        assert_eq!(budget.used(ChargeKind::Metadata), baseline);
    }

    #[test]
    fn reliable_input_charge_outlives_cancelled_waiter_until_writer_retirement() {
        use crate::slice_budget::{ChargeKind, SliceBudget};
        let budget = SliceBudget::new();
        let baseline = budget.used(ChargeKind::Metadata);
        let writer = parked_reliable_writer();
        let rx = writer
            .send_reliable(7, 1, ControllerLease(1), decoded_exact_line(), &budget)
            .unwrap()
            .unwrap();
        drop(rx); // RPC cancellation does not retire writer-owned bytes.
        assert_eq!(writer.lock().queued_bytes, 65537);
        assert!(budget.used(ChargeKind::Metadata) >= baseline + 65537);
        writer.revoke(7);
        assert!(writer.lock().data.is_empty());
        assert_eq!(budget.used(ChargeKind::Metadata), baseline);
    }

    #[test]
    fn reliable_input_charge_follows_active_job_and_retires_before_retained_reply() {
        use crate::slice_budget::{ChargeKind, SliceBudget};
        let budget = SliceBudget::new();
        let baseline = budget.used(ChargeKind::Metadata);
        let writer = parked_reliable_writer();
        let mut rx = writer
            .send_reliable(7, 1, ControllerLease(1), decoded_exact_line(), &budget)
            .unwrap()
            .unwrap();
        let job = {
            let mut state = writer.lock();
            let job = state.data.pop_front().unwrap();
            state.queued_bytes = 0;
            state.active = true;
            state.inflight = Some(job.conn);
            state.active_bytes = job.bytes.len();
            job
        };
        writer.revoke(7); // An active job remains owned until the OS-write path returns.
        assert!(budget.used(ChargeKind::Metadata) >= baseline + 65537);
        writer.run_send(job);
        assert_eq!(budget.used(ChargeKind::Metadata), baseline);
        assert!(matches!(rx.try_recv(), Ok(SendOutcome::NotWritten { .. })));
    }

    #[test]
    fn retiring_writer_settles_queued_requests_and_refuses_new_work() {
        let session = Arc::new(Session::new(SessionParts {
            session: SessionId::new("done").unwrap(),
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
        let writer = WriterHandle::start(session, None);
        {
            let mut state = writer.lock();
            state.lease = 1;
            state.owner = 7;
        }
        let mut queued = writer
            .send(7, 1, ControllerLease(1), "queued".into())
            .unwrap();
        let mut claim = writer.claim(8, 1).unwrap();
        writer.shutdown();
        assert!(
            matches!(queued.try_recv(), Ok(SendOutcome::NotWritten { .. })),
            "retirement left data reply pending"
        );
        assert!(
            matches!(claim.try_recv(), Ok(Err(_))),
            "retirement left authority reply pending"
        );
        assert!(writer.claim(9, 1).is_err());
        assert!(
            writer
                .signal(7, 1, ControllerLease(1), SignalKind::Kill)
                .is_err()
        );
        assert!(writer.kill(7, 1, None).is_err());
        assert!(
            writer
                .send(7, 1, ControllerLease(1), "late".into())
                .is_err()
        );
    }
}
