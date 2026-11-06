//! Per-session stdin writer. One thread owns the pipe and the lease tuple.
//!
//! Queue admission is not a finished write. A lease moves only after the active
//! OS write has been joined. `ControllerLease::generation_bump_settles_os_write`
//! stays false.

use std::collections::{HashSet, VecDeque};
use std::fs::File;
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
    bytes: Vec<u8>,
    reply: oneshot::Sender<SendOutcome>,
}

enum CtrlKind {
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
    reply: oneshot::Sender<Result<u64, String>>,
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
        self.cv.notify_all();
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
    ) -> Result<oneshot::Receiver<Result<u64, String>>, String> {
        self.enqueue_ctrl(conn, epoch, 0, CtrlKind::Claim, true)
    }

    pub fn signal(
        &self,
        conn: u64,
        epoch: u64,
        lease: ControllerLease,
        signal: SignalKind,
    ) -> Result<oneshot::Receiver<Result<u64, String>>, String> {
        let mut state = self.lock();
        if let Some(reason) = self.classify_signal(&state, conn, epoch, lease.0) {
            return Err(reason);
        }
        if state.control.len() >= CONTROL_LIMIT {
            return Err("busy".into());
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
    ) -> Result<oneshot::Receiver<Result<u64, String>>, String> {
        let mut state = self.lock();
        if epoch != self.session.epoch {
            return Err("stale epoch".into());
        }
        if let Some(lease) = lease {
            if state.fenced {
                // An administrative lease is optional. A supplied one still has to match.
            }
            if lease.0 != state.lease || state.lease == 0 {
                return Err("stale lease".into());
            }
            if state.owner != conn {
                return Err("wrong connection".into());
            }
        }
        if state.control.len() >= CONTROL_LIMIT {
            return Err("busy".into());
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
        state.revoked.insert(conn);
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
                let _ = job.reply.send(Err("disconnected".into()));
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
    ) -> Result<oneshot::Receiver<Result<u64, String>>, String> {
        let mut state = self.lock();
        if epoch != self.session.epoch {
            return Err("stale epoch".into());
        }
        if state.control.len() >= CONTROL_LIMIT {
            return Err("busy".into());
        }
        if claim && !state.claim_conns.insert(conn) {
            return Err("busy".into());
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

    fn classify_signal(&self, state: &State, conn: u64, epoch: u64, lease: u64) -> Option<String> {
        if epoch != self.session.epoch {
            return Some("stale epoch".into());
        }
        if lease == 0 || lease != state.lease {
            return Some("stale lease".into());
        }
        if state.owner != conn {
            return Some("wrong connection".into());
        }
        None
    }

    pub(crate) fn run(self: Arc<Self>, host_stop: impl Fn() -> bool) {
        while !host_stop() {
            let next = {
                let mut state = self.lock();
                loop {
                    if state.stop || host_stop() {
                        state.stop = true;
                        return;
                    }
                    if let Some(job) = state.control.pop_front() {
                        break Job::Ctrl(job);
                    }
                    if let Some(job) = state.data.pop_front() {
                        state.queued_bytes = state.queued_bytes.saturating_sub(job.bytes.len());
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
        }
    }

    fn run_ctrl(&self, job: CtrlJob) {
        match job.kind {
            CtrlKind::Claim => self.run_claim(job),
            CtrlKind::Signal(signal) => self.run_signal(job, signal),
            CtrlKind::Kill { lease } => self.run_kill(job, lease),
        }
    }

    fn run_claim(&self, job: CtrlJob) {
        self.pause_phase("before-claim");
        let mut state = self.lock();
        if state.stop || state.revoked.contains(&job.conn) {
            state.claim_conns.remove(&job.conn);
            let _ = job.reply.send(Err("disconnected".into()));
            return;
        }
        if job.epoch != self.session.epoch {
            state.claim_conns.remove(&job.conn);
            let _ = job.reply.send(Err("stale epoch".into()));
            return;
        }
        self.invalidate_queued(&mut state, SendBlock::StaleLease);
        if state.uncertain {
            self.fence_locked(&mut state);
            state.claim_conns.remove(&job.conn);
            state.uncertain = false;
            let _ = job.reply.send(Err("timeout".into()));
            return;
        }
        let Some(next) = state.lease.checked_add(1).filter(|next| *next != 0) else {
            state.claim_conns.remove(&job.conn);
            let _ = job.reply.send(Err("lease exhausted".into()));
            return;
        };
        if Instant::now() >= job.admitted + STDIN_WRITE_TIMEOUT {
            state.claim_conns.remove(&job.conn);
            let _ = job.reply.send(Err("timeout".into()));
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
            let _ = job.reply.send(Err("disconnected".into()));
            return;
        }
        if let Some(reason) = self.classify_signal(&state, job.conn, job.epoch, job.lease) {
            let _ = job.reply.send(Err(reason));
            return;
        }
        drop(state);
        let result = signal_session(&self.session, signal);
        let _ = job.reply.send(result.map(|()| 0));
    }

    fn run_kill(&self, job: CtrlJob, lease: Option<ControllerLease>) {
        self.pause_phase("before-kill");
        let state = self.lock();
        if job.epoch != self.session.epoch {
            let _ = job.reply.send(Err("stale epoch".into()));
            return;
        }
        if let Some(lease) = lease
            && (lease.0 != state.lease || state.owner != job.conn || state.lease == 0)
        {
            let reason = if lease.0 != state.lease || state.lease == 0 {
                "stale lease"
            } else {
                "wrong connection"
            };
            let _ = job.reply.send(Err(reason.into()));
            return;
        }
        drop(state);
        let result = self.session.request_cleanup();
        let _ = job
            .reply
            .send(result.map(|()| 0).map_err(|err| err.to_string()));
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
    Timeout,
    Closed,
    Err,
}

fn write_some(file: &mut File, buf: &[u8], deadline: Instant) -> WriteStep {
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

#[cfg(not(unix))]
fn wait_writable(_file: &File, deadline: Instant) -> bool {
    let remain = deadline.saturating_duration_since(Instant::now());
    if remain.is_zero() {
        return false;
    }
    thread::sleep(remain.min(Duration::from_millis(50)));
    Instant::now() < deadline
}

fn signal_session(session: &Session, signal: SignalKind) -> Result<(), String> {
    #[cfg(unix)]
    {
        let anchor = crate::LeaderAnchor {
            pid: session.vendor_pid,
            start: session.vendor_start,
            pgid: session.pgid,
            reaped: session.leader_exit().is_some(),
        };
        let mapped = match signal {
            SignalKind::Interrupt => libc::SIGINT,
            SignalKind::Terminate => libc::SIGTERM,
            SignalKind::Kill => libc::SIGKILL,
            SignalKind::Hangup => libc::SIGHUP,
        };
        crate::signal_anchored_group(Some(&anchor), mapped).map_err(|err| err.as_str().to_owned())
    }
    #[cfg(windows)]
    {
        let _ = signal;
        session.terminate_leader().map_err(|err| err.to_string())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (session, signal);
        Err("unanchored group".into())
    }
}
