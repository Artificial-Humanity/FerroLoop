//! In-memory owner for one session. Disk updates are applied by the host before waiters wake.

use std::fs::File;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fl_host_proto::{
    ChildExit, ChildRecord, ChildState, HostError, InputState, MAX_STDOUT_BACKLOG,
    STDERR_TAIL_BYTES, SessionEpoch, SessionId, SpawnKind,
};

use crate::child::{CleanupReport, DescendantExit};

// A total completion order, independent of map iteration and session names.
static COMPLETION_ORDER: AtomicU64 = AtomicU64::new(1);

pub struct Session {
    pub session: SessionId,
    pub epoch: u64,
    pub vendor_pid: u32,
    pub vendor_start: u64,
    pub supervisor_pid: u32,
    pub supervisor_start: u64,
    pub pgid: u32,
    pub control_inode: u64,
    pub started_at_ms: u64,
    // Fixed storage bounds retained ownership without exposing launch environment values.
    hosted_owner: [u8; fl_host_proto::MAX_HOSTED_OWNER_BYTES],
    hosted_owner_len: usize,
    control: Mutex<Option<File>>,
    stdin: Mutex<Option<File>>,
    leader: Mutex<Option<ChildExit>>,
    leader_cv: Condvar,
    cleanup: Mutex<Option<CleanupReport>>,
    cleanup_cv: Condvar,
    completion_order: AtomicU64,
    descendants: Mutex<Vec<DescendantExit>>,
    stdout: Mutex<OutBuf>,
    stdout_cv: Condvar,
    stderr: Mutex<Vec<u8>>,
    cleanup_requested: AtomicBool,
    io_stop: AtomicBool,
    #[cfg(unix)]
    signal_reply: Mutex<(u64, Option<Result<(), String>>)>,
    #[cfg(unix)]
    signal_cv: Condvar,
    input: Mutex<InputState>,
    stalled: AtomicBool,
    sink: Mutex<Option<Weak<dyn StdoutSink>>>,
    reliable: Option<Arc<crate::reliable::ReliableJournal>>,
    projection: Mutex<Option<crate::projection::Projection>>,
    #[cfg(unix)]
    resize_master: Option<File>,
    #[cfg(unix)]
    supervisor: Mutex<Option<std::process::Child>>,
    #[cfg(windows)]
    pub job: Mutex<Option<std::os::windows::io::OwnedHandle>>,
    #[cfg(windows)]
    pub process: Mutex<Option<std::os::windows::io::OwnedHandle>>,
}

struct OutBuf {
    bytes: Vec<u8>,
    eof: bool,
}

pub(crate) struct SessionParts {
    pub session: SessionId,
    pub epoch: u64,
    pub vendor_pid: u32,
    pub vendor_start: u64,
    pub supervisor_pid: u32,
    pub supervisor_start: u64,
    pub pgid: u32,
    pub control_inode: u64,
    pub control: Option<File>,
    pub stdin: Option<File>,
}

impl Session {
    pub(crate) fn with_hosted_owner(mut self, owner: Option<&str>) -> Self {
        if let Some(owner) = owner.filter(|s| !s.is_empty() && s.len() <= self.hosted_owner.len()) {
            self.hosted_owner[..owner.len()].copy_from_slice(owner.as_bytes());
            self.hosted_owner_len = owner.len();
        }
        self
    }

    pub(crate) fn owned_by(&self, hosted_id: &str) -> bool {
        self.hosted_owner_len != 0
            && self.hosted_owner[..self.hosted_owner_len] == *hosted_id.as_bytes()
    }

    pub(crate) fn has_hosted_owner(&self) -> bool {
        self.hosted_owner_len != 0
    }

    pub(crate) fn mode(&self) -> fl_host_proto::SessionMode {
        match &self.reliable {
            Some(journal) if journal.pty_mode() => fl_host_proto::SessionMode::Pty,
            Some(_) => fl_host_proto::SessionMode::ReliablePipe,
            None => fl_host_proto::SessionMode::LegacyPipe,
        }
    }
    pub(crate) fn reliable(&self) -> Option<&Arc<crate::reliable::ReliableJournal>> {
        self.reliable.as_ref()
    }

    #[cfg(unix)]
    pub(crate) fn with_projection(
        parts: SessionParts,
        journal: Arc<crate::reliable::ReliableJournal>,
        projection: crate::projection::Projection,
        master: Option<File>,
    ) -> Self {
        assert!(journal.pty_mode());
        let mut session = Self::with_reliable(parts, Some(journal));
        session.projection = Mutex::new(Some(projection));
        session.resize_master = master;
        session
    }
    pub(crate) fn capture_screen(
        &self,
    ) -> Result<crate::projection::ProjectionSnapshot, fl_host_proto::ExtensionRefusal> {
        if cfg!(windows) {
            return Err(fl_host_proto::ExtensionRefusal::Unsupported);
        }
        self.projection
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .ok_or_else(|| {
                if self
                    .reliable
                    .as_ref()
                    .is_some_and(|journal| journal.pty_mode())
                {
                    fl_host_proto::ExtensionRefusal::SessionFenced
                } else {
                    fl_host_proto::ExtensionRefusal::WrongMode
                }
            })?
            .snapshot()
    }
    pub(crate) fn resize_pty(&self, cols: u16, rows: u16) -> Result<u64, HostError> {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let mut projection = self.projection.lock().unwrap_or_else(|e| e.into_inner());
            let projection = projection.as_mut().ok_or_else(|| {
                HostError::Protocol(
                    if self
                        .reliable
                        .as_ref()
                        .is_some_and(|journal| journal.pty_mode())
                    {
                        "SessionFenced"
                    } else {
                        "wrong session mode"
                    }
                    .into(),
                )
            })?;
            projection.resize(cols, rows, || {
                let master = self
                    .resize_master
                    .as_ref()
                    .ok_or_else(|| HostError::Io("PTY master unavailable".into()))?;
                let size = libc::winsize {
                    ws_row: rows,
                    ws_col: cols,
                    ws_xpixel: 0,
                    ws_ypixel: 0,
                };
                // Dedicated live duplicate, independent of the stdin writer's ownership.
                if unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &size) } < 0 {
                    return Err(HostError::Io(std::io::Error::last_os_error().to_string()));
                }
                Ok(())
            })
        }
        #[cfg(not(unix))]
        {
            let _ = (cols, rows);
            Err(HostError::Unsupported {
                subject: "pty resize",
            })
        }
    }

    // The projection lock covers every byte's parser update, raw tail, journal
    // admission/drop and watermark. Reads, polls and waits occur outside it.
    fn accept_output(
        &self,
        journal: &crate::reliable::ReliableJournal,
        bytes: crate::reliable::ChargedBytes,
    ) -> Result<(), fl_host_proto::ExtensionRefusal> {
        let mut projection = self.projection.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(projection) = projection.as_mut() {
            projection.process(bytes.as_slice());
        }
        self.push_stdout_tail(bytes.as_slice(), 64 * 1024);
        let result = journal.accept_read(bytes);
        if let Some(projection) = projection.as_mut() {
            projection.set_output_seq(journal.position().1);
        }
        result
    }
    fn drop_pty_output(&self, journal: &crate::reliable::ReliableJournal, bytes: &[u8]) {
        let mut projection = self.projection.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(projection) = projection.as_mut() {
            projection.process(bytes);
        }
        self.push_stdout_tail(bytes, 64 * 1024);
        journal.drop_pty_unadmitted();
        if let Some(projection) = projection.as_mut() {
            projection.set_output_seq(journal.position().1);
        }
    }

    pub fn new(parts: SessionParts) -> Self {
        Self::with_reliable(parts, None)
    }

    pub(crate) fn with_reliable(
        parts: SessionParts,
        reliable: Option<Arc<crate::reliable::ReliableJournal>>,
    ) -> Self {
        if let Some(journal) = &reliable {
            assert_eq!(
                journal.stream().session,
                parts.session,
                "immutable journal session"
            );
            assert_eq!(
                journal.stream().epoch.0,
                parts.epoch,
                "immutable journal epoch"
            );
        }
        let SessionParts {
            session,
            epoch,
            vendor_pid,
            vendor_start,
            supervisor_pid,
            supervisor_start,
            pgid,
            control_inode,
            control,
            stdin,
        } = parts;
        let started_at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        Self {
            session,
            epoch,
            vendor_pid,
            vendor_start,
            supervisor_pid,
            supervisor_start,
            pgid,
            control_inode,
            started_at_ms,
            hosted_owner: [0; fl_host_proto::MAX_HOSTED_OWNER_BYTES],
            hosted_owner_len: 0,
            control: Mutex::new(control),
            stdin: Mutex::new(stdin),
            leader: Mutex::new(None),
            leader_cv: Condvar::new(),
            cleanup: Mutex::new(None),
            cleanup_cv: Condvar::new(),
            completion_order: AtomicU64::new(0),
            descendants: Mutex::new(Vec::new()),
            stdout: Mutex::new(OutBuf {
                bytes: Vec::new(),
                eof: false,
            }),
            stdout_cv: Condvar::new(),
            stderr: Mutex::new(Vec::new()),
            cleanup_requested: AtomicBool::new(false),
            io_stop: AtomicBool::new(false),
            #[cfg(unix)]
            signal_reply: Mutex::new((0, None)),
            #[cfg(unix)]
            signal_cv: Condvar::new(),
            input: Mutex::new(InputState::Writable),
            stalled: AtomicBool::new(false),
            sink: Mutex::new(None),
            reliable,
            projection: Mutex::new(None),
            #[cfg(unix)]
            resize_master: None,
            #[cfg(unix)]
            supervisor: Mutex::new(None),
            #[cfg(windows)]
            job: Mutex::new(None),
            #[cfg(windows)]
            process: Mutex::new(None),
        }
    }

    #[cfg(unix)]
    pub fn set_supervisor(&self, child: std::process::Child) {
        *self
            .supervisor
            .lock()
            .unwrap_or_else(|err| err.into_inner()) = Some(child);
    }

    #[cfg(any(windows, test))]
    pub fn cleanup_requested(&self) -> bool {
        self.cleanup_requested.load(Ordering::SeqCst)
    }

    pub fn request_cleanup(&self) -> Result<(), HostError> {
        self.cleanup_requested.store(true, Ordering::SeqCst);
        let mut control = self.control.lock().unwrap_or_else(|err| err.into_inner());
        control.take();
        #[cfg(windows)]
        {
            self.terminate_job()?;
        }
        Ok(())
    }

    pub(crate) fn stop_pumps(&self) {
        self.io_stop.store(true, Ordering::SeqCst);
    }

    #[cfg(unix)]
    pub(crate) fn signal(&self, signal: i32) -> Result<(), String> {
        use std::os::fd::AsRawFd;
        let timeout = Duration::from_secs(1);
        let deadline = Instant::now() + timeout;
        let deadline_ms = crate::protocol::signal_clock_ms()? + timeout.as_millis() as u64;
        let mut reply = self
            .signal_reply
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        reply.0 = reply.0.checked_add(1).ok_or("signal id exhausted")?;
        reply.1 = None;
        let id = reply.0;
        let bytes = crate::protocol::to_line(&crate::protocol::DaemonMsg::Signal {
            id,
            signal,
            deadline_ms,
        })?;
        {
            let control = self.control.lock().unwrap_or_else(|err| err.into_inner());
            let file = control.as_ref().ok_or("supervisor unavailable")?;
            // Atomic, nonblocking pipe write: no partial command and no stalled supervisor
            // can hold this authority worker past the response deadline.
            let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
            if flags < 0
                || unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) }
                    < 0
            {
                return Err("supervisor control unavailable".into());
            }
            let n = unsafe { libc::write(file.as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) };
            if n != bytes.len() as isize {
                return Err("supervisor control unavailable".into());
            }
        }
        loop {
            if let Some(result) = reply.1.take() {
                return result;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err("signal outcome unknown".into());
            }
            reply = self
                .signal_cv
                .wait_timeout(reply, left)
                .unwrap_or_else(|err| err.into_inner())
                .0;
        }
    }

    #[cfg(unix)]
    pub(crate) fn note_signal(&self, id: u64, error: Option<String>) {
        let mut reply = self
            .signal_reply
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        if reply.0 == id {
            reply.1 = Some(error.map_or(Ok(()), Err));
            self.signal_cv.notify_all();
        }
    }

    #[cfg(windows)]
    fn terminate_job(&self) -> Result<(), HostError> {
        use std::os::windows::io::AsRawHandle;
        let job = self.job.lock().unwrap_or_else(|err| err.into_inner());
        let Some(job) = job.as_ref() else {
            return Ok(());
        };
        // SAFETY: `job` is the session Job. TerminateJobObject kills members and leaves the
        // handle open so accounting can still reach zero.
        let ok = unsafe {
            windows_sys::Win32::System::JobObjects::TerminateJobObject(job.as_raw_handle() as _, 1)
        };
        if ok == 0 {
            Err(HostError::Io("terminate job".into()))
        } else {
            Ok(())
        }
    }

    pub fn note_leader(&self, exit: ChildExit) {
        let fresh = {
            let mut slot = self.leader.lock().unwrap_or_else(|err| err.into_inner());
            if slot.is_none() {
                *slot = Some(exit.clone());
                true
            } else {
                false
            }
        };
        self.leader_cv.notify_all();
        if fresh && let Some(sink) = self.sink() {
            sink.on_leader(exit);
        }
    }

    #[cfg(unix)]
    pub fn note_descendant(&self, exit: DescendantExit) {
        self.descendants
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .push(exit);
    }

    pub fn note_cleanup(&self, report: CleanupReport) {
        let fresh = {
            let mut slot = self.cleanup.lock().unwrap_or_else(|err| err.into_inner());
            if slot.is_none() {
                if report.complete {
                    self.completion_order.store(
                        COMPLETION_ORDER.fetch_add(1, Ordering::SeqCst),
                        Ordering::SeqCst,
                    );
                }
                *slot = Some(report);
                true
            } else {
                false
            }
        };
        // Publish cleanup before notifying downstream owners, without nesting the
        // cleanup mutex with projection/journal locks. The projection mutex also
        // serializes pump/capture/resize; later drain updates only the raw tail.
        if fresh && report.complete {
            self.projection
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .take();
        }
        self.cleanup_cv.notify_all();
        if fresh && let Some(sink) = self.sink() {
            sink.on_cleanup(report.complete);
        }
    }

    pub fn leader_exit(&self) -> Option<ChildExit> {
        self.leader
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clone()
    }

    pub(crate) fn completion_order(&self) -> u64 {
        self.completion_order.load(Ordering::SeqCst)
    }

    pub fn cleanup_report(&self) -> Option<CleanupReport> {
        *self.cleanup.lock().unwrap_or_else(|err| err.into_inner())
    }

    pub fn push_stdout(&self, chunk: &[u8]) {
        self.push_stdout_tail(chunk, MAX_STDOUT_BACKLOG);
    }

    pub fn push_stdout_tail(&self, chunk: &[u8], max_cap: usize) {
        let mut buf = self.stdout.lock().unwrap_or_else(|err| err.into_inner());
        if chunk.len() >= max_cap {
            buf.bytes.clear();
            buf.bytes.extend_from_slice(&chunk[chunk.len() - max_cap..]);
        } else {
            let total = buf.bytes.len().saturating_add(chunk.len());
            if total > max_cap {
                let drop_n = total - max_cap;
                buf.bytes.drain(..drop_n);
            }
            buf.bytes.extend_from_slice(chunk);
        }
        self.stdout_cv.notify_all();
    }

    pub fn stdout_tail(&self, max_bytes: usize) -> String {
        let buf = self.stdout.lock().unwrap_or_else(|err| err.into_inner());
        let cap = max_bytes.min(fl_host_proto::MAX_RAW_INPUT);
        let start = buf.bytes.len().saturating_sub(cap);
        let mut text = String::from_utf8_lossy(&buf.bytes[start..]).into_owned();
        if text.len() > fl_host_proto::MAX_RAW_INPUT {
            let mut trim_from = text.len() - fl_host_proto::MAX_RAW_INPUT;
            while trim_from < text.len() && !text.is_char_boundary(trim_from) {
                trim_from += 1;
            }
            text.drain(..trim_from);
        }
        text
    }

    pub fn stdout_eof(&self) {
        self.stdout
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .eof = true;
        self.stdout_cv.notify_all();
    }

    pub fn push_stderr(&self, chunk: &[u8]) {
        let mut buf = self.stderr.lock().unwrap_or_else(|err| err.into_inner());
        buf.extend_from_slice(chunk);
        if buf.len() > STDERR_TAIL_BYTES {
            let drop_n = buf.len() - STDERR_TAIL_BYTES;
            buf.drain(..drop_n);
        }
    }

    pub fn write_stdin(&self, bytes: &[u8]) -> Result<(), HostError> {
        let mut stdin = self.stdin.lock().unwrap_or_else(|err| err.into_inner());
        let Some(file) = stdin.as_mut() else {
            return Err(HostError::Io("stdin closed".into()));
        };
        file.write_all(bytes)?;
        Ok(())
    }

    pub fn read_stdout_until(
        &self,
        needle: &[u8],
        timeout: Duration,
    ) -> Result<Vec<u8>, HostError> {
        let deadline = Instant::now() + timeout;
        let mut buf = self.stdout.lock().unwrap_or_else(|err| err.into_inner());
        loop {
            if !needle.is_empty() && find_sub(&buf.bytes, needle).is_some() {
                return Ok(buf.bytes.clone());
            }
            if buf.eof {
                return if needle.is_empty() {
                    Ok(buf.bytes.clone())
                } else {
                    Err(HostError::Io("stdout closed".into()))
                };
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(HostError::FrameTimeout);
            }
            let (guard, wait) = self
                .stdout_cv
                .wait_timeout(buf, deadline.saturating_duration_since(now))
                .unwrap_or_else(|err| err.into_inner());
            buf = guard;
            if wait.timed_out() && Instant::now() >= deadline {
                return Err(HostError::FrameTimeout);
            }
        }
    }

    pub fn wait_leader(&self, timeout: Duration) -> Result<ChildExit, HostError> {
        let deadline = Instant::now() + timeout;
        let mut slot = self.leader.lock().unwrap_or_else(|err| err.into_inner());
        loop {
            if let Some(exit) = slot.clone() {
                return Ok(exit);
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(HostError::FrameTimeout);
            }
            let (guard, wait) = self
                .leader_cv
                .wait_timeout(slot, deadline.saturating_duration_since(now))
                .unwrap_or_else(|err| err.into_inner());
            slot = guard;
            if wait.timed_out() && slot.is_none() && Instant::now() >= deadline {
                return Err(HostError::FrameTimeout);
            }
        }
    }

    pub fn wait_cleanup(&self, timeout: Duration) -> Result<CleanupReport, HostError> {
        let deadline = Instant::now() + timeout;
        let mut slot = self.cleanup.lock().unwrap_or_else(|err| err.into_inner());
        loop {
            if let Some(report) = *slot {
                return Ok(report);
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(HostError::FrameTimeout);
            }
            let (guard, wait) = self
                .cleanup_cv
                .wait_timeout(slot, deadline.saturating_duration_since(now))
                .unwrap_or_else(|err| err.into_inner());
            slot = guard;
            if wait.timed_out() && slot.is_none() && Instant::now() >= deadline {
                return Err(HostError::FrameTimeout);
            }
        }
    }

    pub fn descendants(&self) -> Vec<DescendantExit> {
        self.descendants
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clone()
    }

    pub(crate) fn take_stdin(&self) -> Option<File> {
        self.stdin
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .take()
    }

    pub(crate) fn set_input(&self, input: InputState) {
        *self.input.lock().unwrap_or_else(|err| err.into_inner()) = input;
    }

    pub(crate) fn set_stalled(&self, stalled: bool) {
        self.stalled.store(stalled, Ordering::SeqCst);
    }

    pub(crate) fn set_sink(&self, sink: Weak<dyn StdoutSink>) {
        *self.sink.lock().unwrap_or_else(|err| err.into_inner()) = Some(sink);
    }

    pub(crate) fn sink(&self) -> Option<Arc<dyn StdoutSink>> {
        self.sink
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .as_ref()
            .and_then(Weak::upgrade)
    }

    #[cfg(windows)]
    pub(crate) fn terminate_leader(&self) -> Result<(), HostError> {
        use std::os::windows::io::AsRawHandle;
        let process = self.process.lock().unwrap_or_else(|err| err.into_inner());
        let Some(process) = process.as_ref() else {
            return Err(HostError::Io("leader process".into()));
        };
        let ok = unsafe {
            windows_sys::Win32::System::Threading::TerminateProcess(process.as_raw_handle() as _, 1)
        };
        if ok == 0 {
            Err(HostError::Io("terminate process".into()))
        } else {
            Ok(())
        }
    }

    pub fn state(&self) -> ChildState {
        let exit = self.leader_exit();
        let cleanup = self.cleanup_report();
        let input = self
            .input
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clone();
        let record = ChildRecord {
            session: self.session.clone(),
            session_epoch: SessionEpoch(self.epoch),
            kind: if self.mode() == fl_host_proto::SessionMode::Pty {
                SpawnKind::Pty
            } else {
                SpawnKind::Pipe
            },
            pid: self.vendor_pid,
            input,
            started_at_ms: self.started_at_ms,
            exit,
            consumer_stalled: self.stalled.load(Ordering::SeqCst),
        };
        if matches!(cleanup, Some(report) if report.complete) {
            ChildState::CleanupComplete(record)
        } else if record.exit.is_some() {
            ChildState::LeaderExited(record)
        } else {
            ChildState::Running(record)
        }
    }

    #[cfg(unix)]
    pub fn take_supervisor(&self) -> Option<std::process::Child> {
        self.supervisor
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .take()
    }
}

fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len())
        .position(|window| window == needle)
}

pub(crate) trait StdoutSink: Send + Sync {
    fn reliable(&self) -> Option<&Arc<crate::reliable::ReliableJournal>> {
        None
    }
    fn room(&self) -> usize;
    fn push(&self, bytes: &[u8]) -> bool;
    fn eof(&self);
    fn wait(&self, timeout: Duration);
    fn on_leader(&self, exit: ChildExit);
    fn on_cleanup(&self, complete: bool);
    fn stopped(&self) -> bool;
}

pub fn pump_stdout(mut file: File, session: std::sync::Arc<Session>) {
    if session.sink().is_none() {
        let mut buf = [0u8; 8192];
        loop {
            match pump_read(&mut file, &mut buf, &session) {
                Ok(0) => {
                    session.stdout_eof();
                    break;
                }
                Ok(n) => session.push_stdout(&buf[..n]),
                Err(_) => {
                    session.stdout_eof();
                    break;
                }
            }
        }
        return;
    }
    if let Some(sink) = session.sink()
        && let Some(journal) = sink.reliable()
    {
        pump_reliable_stdout(&mut file, &session, sink.as_ref(), journal);
        return;
    }
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        if session.io_stop.load(Ordering::SeqCst) {
            if let Some(sink) = session.sink() {
                sink.eof();
            }
            session.stdout_eof();
            break;
        }
        let Some(sink) = session.sink() else {
            session.stdout_eof();
            break;
        };
        if sink.stopped() {
            sink.eof();
            session.stdout_eof();
            break;
        }
        let room = sink.room();
        if room == 0 {
            session.set_stalled(true);
            sink.wait(Duration::from_millis(50));
            continue;
        }
        session.set_stalled(false);
        let n = room.min(buf.len());
        match pump_read(&mut file, &mut buf[..n], &session) {
            Ok(0) => {
                sink.eof();
                session.stdout_eof();
                break;
            }
            Ok(n) => {
                session.push_stdout(&buf[..n]);
                if !sink.push(&buf[..n]) {
                    sink.eof();
                    session.stdout_eof();
                    break;
                }
            }
            Err(_) => {
                sink.eof();
                session.stdout_eof();
                break;
            }
        }
    }
}

// No debug-buffer copy in reliable mode. Pending/partial bytes live in the
// journal, so cancelling this thread cannot silently discard an admitted read.
fn pump_reliable_stdout(
    file: &mut File,
    session: &Session,
    sink: &dyn StdoutSink,
    journal: &crate::reliable::ReliableJournal,
) {
    pump_reliable_stdout_with(session, sink, journal, |buffer| {
        pump_read(file, buffer, session)
    });
}
pub(crate) fn pump_reliable_stdout_with(
    session: &Session,
    sink: &dyn StdoutSink,
    journal: &crate::reliable::ReliableJournal,
    mut read: impl FnMut(&mut [u8]) -> std::io::Result<usize>,
) {
    loop {
        if session.io_stop.load(Ordering::SeqCst) || sink.stopped() {
            break;
        }
        let mut buffer = match journal.prepare_read() {
            Ok(buffer) => buffer,
            Err(_) if journal.pty_mode() => {
                let mut scratch = [0u8; 4096];
                match read(&mut scratch) {
                    Ok(0) | Err(_) => break,
                    Ok(len) => {
                        session.drop_pty_output(journal, &scratch[..len]);
                        continue;
                    }
                }
            }
            Err(_) => {
                session.set_stalled(true);
                sink.wait(Duration::from_millis(50));
                continue;
            }
        };
        session.set_stalled(false);
        match read(buffer.as_mut_slice()) {
            Ok(0) | Err(_) => break,
            Ok(len) => {
                buffer.truncate(len);
                if session.accept_output(journal, buffer).is_err() {
                    break;
                }
            }
        }
    }
    sink.eof();
    session.stdout_eof();
}

pub fn pump_stderr(mut file: File, session: std::sync::Arc<Session>) {
    let mut buf = [0u8; 8192];
    loop {
        match pump_read(&mut file, &mut buf, &session) {
            Ok(0) | Err(_) => break,
            Ok(n) => session.push_stderr(&buf[..n]),
        }
    }
}

// One reader owns each pipe. Unix reads are nonblocking; Windows reads never ask
// for more than PeekNamedPipe observed, so a surviving writer cannot hold a read.
fn pump_read(file: &mut File, buf: &mut [u8], session: &Session) -> std::io::Result<usize> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let fd = file.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
        loop {
            if session.io_stop.load(Ordering::SeqCst) {
                return Ok(0);
            }
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let rc = unsafe { libc::poll(&mut pfd, 1, 50) };
            if rc == 0 {
                continue;
            }
            if rc < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            match file.read(buf) {
                Err(err)
                    if matches!(
                        err.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) =>
                {
                    continue;
                }
                result => return result,
            }
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        let raw = file.as_raw_handle();
        let started = Instant::now();
        poll_windows_pipe(
            WINDOWS_PIPE_REFILL_BUDGET,
            || session.io_stop.load(Ordering::SeqCst),
            || {
                let mut available = 0;
                let ok = unsafe {
                    windows_sys::Win32::System::Pipes::PeekNamedPipe(
                        raw as _,
                        std::ptr::null_mut(),
                        0,
                        std::ptr::null_mut(),
                        &mut available,
                        std::ptr::null_mut(),
                    )
                };
                if ok == 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(available as usize)
                }
            },
            |available| {
                let n = buf.len().min(available);
                file.read(&mut buf[..n])
            },
            || started.elapsed(),
            |pause| match pause {
                PipePause::Refill => std::thread::yield_now(),
                PipePause::Idle => std::thread::sleep(Duration::from_millis(50)),
            },
        )
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = session;
        file.read(buf)
    }
}

// Anonymous Windows pipes have no readiness wait. Give an active writer one
// short scheduling window to refill before backing off. Empty peeks and idle
// waits never renew that budget; only a successful read starts a new call.
#[cfg(any(windows, test))]
const WINDOWS_PIPE_REFILL_BUDGET: Duration = Duration::from_millis(2);

#[cfg(any(windows, test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PipePause {
    Refill,
    Idle,
}

#[cfg(any(windows, test))]
fn poll_windows_pipe(
    refill_budget: Duration,
    mut stopped: impl FnMut() -> bool,
    mut peek: impl FnMut() -> std::io::Result<usize>,
    mut read: impl FnMut(usize) -> std::io::Result<usize>,
    mut now: impl FnMut() -> Duration,
    mut pause: impl FnMut(PipePause),
) -> std::io::Result<usize> {
    let refill_until = now().saturating_add(refill_budget);
    loop {
        if stopped() {
            return Ok(0);
        }
        let available = peek()?;
        if available != 0 {
            if stopped() {
                return Ok(0);
            }
            return read(available);
        }
        pause(if now() < refill_until {
            PipePause::Refill
        } else {
            PipePause::Idle
        });
    }
}

#[cfg(test)]
mod pipe_poll_tests {
    use super::{PipePause, WINDOWS_PIPE_REFILL_BUDGET, poll_windows_pipe};
    use std::cell::Cell;
    use std::time::Duration;

    fn sequential_refill_elapsed(budget: Duration) -> Duration {
        let clock = Cell::new(Duration::ZERO);
        let next_ready = Cell::new(Duration::from_micros(100));
        let reads = Cell::new(0);
        for _ in 0..1024 {
            let n = poll_windows_pipe(
                budget,
                || false,
                || {
                    Ok(if clock.get() >= next_ready.get() {
                        4096
                    } else {
                        0
                    })
                },
                |available| {
                    assert_eq!(available, 4096);
                    reads.set(reads.get() + 1);
                    next_ready.set(clock.get() + Duration::from_micros(100));
                    Ok(available)
                },
                || clock.get(),
                |pause| {
                    clock.set(
                        clock.get()
                            + match pause {
                                PipePause::Refill => Duration::from_micros(100),
                                PipePause::Idle => Duration::from_millis(50),
                            },
                    )
                },
            )
            .unwrap();
            assert_eq!(n, 4096);
        }
        assert_eq!(reads.get() * 4096, 4 * 1024 * 1024);
        clock.get()
    }

    #[test]
    fn active_refills_drain_four_mib_without_an_idle_sleep_per_chunk() {
        // With the old zero-active-window policy, this same driver pays 50 ms
        // for each refill even though the writer is ready after 100 us.
        assert!(sequential_refill_elapsed(Duration::ZERO) > Duration::from_secs(2));
        assert!(sequential_refill_elapsed(WINDOWS_PIPE_REFILL_BUDGET) < Duration::from_secs(2));
    }

    #[test]
    fn exhausted_refill_window_stays_idle_and_cancellation_precedes_peek() {
        let clock = Cell::new(Duration::ZERO);
        let stopped = Cell::new(false);
        let peeks = Cell::new(0);
        let refills = Cell::new(0);
        let idles = Cell::new(0);
        assert_eq!(
            poll_windows_pipe(
                Duration::from_millis(2),
                || stopped.get(),
                || {
                    peeks.set(peeks.get() + 1);
                    Ok(0)
                },
                |_| panic!("empty pipe must not read"),
                || clock.get(),
                |pause| match pause {
                    PipePause::Refill => {
                        assert_eq!(idles.get(), 0, "idle sleep must not renew refill window");
                        refills.set(refills.get() + 1);
                        clock.set(clock.get() + Duration::from_millis(1));
                    }
                    PipePause::Idle => {
                        idles.set(idles.get() + 1);
                        clock.set(clock.get() + Duration::from_millis(50));
                        if idles.get() == 2 {
                            stopped.set(true);
                        }
                    }
                },
            )
            .unwrap(),
            0
        );
        assert_eq!(refills.get(), 2);
        assert_eq!(idles.get(), 2);
        assert_eq!(peeks.get(), 4, "cancellation must precede the next peek");
        assert_eq!(clock.get(), Duration::from_millis(102));
    }

    #[test]
    fn cancellation_after_peek_prevents_read_and_errors_remain_errors() {
        let stopped = Cell::new(false);
        assert_eq!(
            poll_windows_pipe(
                Duration::from_millis(2),
                || stopped.get(),
                || {
                    stopped.set(true);
                    Ok(4096)
                },
                |_| panic!("cancelled pipe must not read"),
                || Duration::ZERO,
                |_| panic!("available pipe must not wait"),
            )
            .unwrap(),
            0
        );
        let err = poll_windows_pipe(
            Duration::from_millis(2),
            || false,
            || {
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "closed",
                ))
            },
            |_| panic!("failed peek must not read"),
            || Duration::ZERO,
            |_| panic!("failed peek must not wait"),
        )
        .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts() -> SessionParts {
        SessionParts {
            session: SessionId::new("capture").unwrap(),
            epoch: 1,
            vendor_pid: 0,
            vendor_start: 0,
            supervisor_pid: 0,
            supervisor_start: 0,
            pgid: 0,
            control_inode: 0,
            control: None,
            stdin: None,
        }
    }

    #[test]
    fn stdout_capture_keeps_recent_bytes_across_repeated_overflow() {
        let session = Session::new(parts());
        let chunk = vec![b'x'; 1024 * 1024];
        for _ in 0..9 {
            session.push_stdout(&chunk);
        }
        for marker in [b"recent-one".as_slice(), b"newest-two".as_slice()] {
            session.push_stdout(marker);
            assert_eq!(session.stdout_tail(marker.len()).as_bytes(), marker);
            let capture = session.read_stdout_until(marker, Duration::ZERO).unwrap();
            assert_eq!(capture.len(), 8 * 1024 * 1024);
            assert!(capture.ends_with(marker));
        }
        assert_eq!(session.stdout_tail(0), "");
    }

    #[test]
    fn stdout_capture_keeps_suffix_of_single_oversized_write() {
        let session = Session::new(parts());
        session.push_stdout(b"obsolete-prefix");
        let mut chunk = vec![b'x'; 9 * 1024 * 1024];
        chunk[1024 * 1024 + 10..1024 * 1024 + 15].copy_from_slice(b"start");
        chunk.extend_from_slice(b"newest-end");
        session.push_stdout(&chunk);
        assert_eq!(session.stdout_tail(10), "newest-end");
        session.stdout_eof();
        let capture = session.read_stdout_until(b"", Duration::ZERO).unwrap();
        assert_eq!(capture.len(), 8 * 1024 * 1024);
        assert_eq!(&capture[..5], b"start");
        assert_eq!(capture.as_slice(), &chunk[chunk.len() - 8 * 1024 * 1024..]);
    }

    #[test]
    fn stdout_capture_public_tail_respects_limit_after_lossy_utf8() {
        let session = Session::new(parts());
        session.push_stdout(&vec![0xff; 128 * 1024]);
        session.push_stdout(b"latest");
        let tail = session.stdout_tail(usize::MAX);
        assert!(tail.len() <= fl_host_proto::MAX_RAW_INPUT);
        assert!(tail.ends_with("latest"));
        assert_eq!(session.stdout_tail(6), "latest");
    }

    // Regular files exercise the actual Unix pump read path without scheduling
    // a subprocess or pipe writer. Windows pumps require named-pipe handles.
    #[cfg(unix)]
    fn flood_file() -> File {
        use std::io::{Seek, SeekFrom};
        let mut file = tempfile::tempfile().unwrap();
        let chunk = vec![b'x'; 1024 * 1024];
        for _ in 0..9 {
            file.write_all(&chunk).unwrap();
        }
        file.write_all(b"\r\x1b[0mnewest-pump-marker\n").unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        file
    }

    #[cfg(unix)]
    #[test]
    fn sinkless_stdout_pump_retains_fresh_bounded_capture_after_flood() {
        let session = Arc::new(Session::new(parts()));
        pump_stdout(flood_file(), Arc::clone(&session));
        assert_eq!(session.stdout_tail(19), "newest-pump-marker\n");
        let capture = session.read_stdout_until(b"", Duration::ZERO).unwrap();
        assert_eq!(capture.len(), 8 * 1024 * 1024);
        assert!(capture.ends_with(b"\r\x1b[0mnewest-pump-marker\n"));
    }

    #[cfg(unix)]
    #[test]
    fn reliable_stdout_pump_retains_fresh_bounded_capture_after_flood() {
        use crate::output::{Hub, OutputSession};
        use crate::reliable::ReliableJournal;
        use crate::slice_budget::SliceBudget;
        use fl_host_proto::{HostInstance, StreamId};

        let parts = parts();
        let journal = ReliableJournal::new(
            SliceBudget::new(),
            StreamId {
                host: HostInstance::new([1; 16]),
                session: parts.session.clone(),
                epoch: SessionEpoch(parts.epoch),
            },
        )
        .unwrap();
        let session = Arc::new(Session::with_reliable(parts, Some(Arc::clone(&journal))));
        let output = OutputSession::open(Arc::clone(&session), Arc::new(Hub::new()));
        let sink: Arc<dyn StdoutSink> = output;
        session.set_sink(Arc::downgrade(&sink));
        pump_stdout(flood_file(), Arc::clone(&session));
        assert_eq!(session.stdout_tail(19), "newest-pump-marker\n");
        let capture = session.read_stdout_until(b"", Duration::ZERO).unwrap();
        assert_eq!(capture.len(), 64 * 1024);
        assert!(capture.ends_with(b"\r\x1b[0mnewest-pump-marker\n"));
        assert!(journal.is_eof());
        assert_eq!(
            journal.position().0,
            0,
            "capture must not acknowledge delivery"
        );
    }
    #[cfg(unix)]
    fn pty_fixture() -> (
        Arc<Session>,
        Arc<crate::reliable::ReliableJournal>,
        Arc<dyn StdoutSink>,
    ) {
        let parts = parts();
        let journal = crate::reliable::ReliableJournal::new_with_mode(
            crate::slice_budget::SliceBudget::new(),
            fl_host_proto::StreamId {
                host: fl_host_proto::HostInstance::new([1; 16]),
                session: parts.session.clone(),
                epoch: SessionEpoch(parts.epoch),
            },
            true,
        )
        .unwrap();
        let projection = crate::projection::Projection::new(
            Arc::clone(journal.budget()),
            fl_host_proto::PtyOptions::default(),
        )
        .unwrap();
        let session = Arc::new(Session::with_projection(
            parts,
            Arc::clone(&journal),
            projection,
            None,
        ));
        let sink: Arc<dyn StdoutSink> = crate::output::OutputSession::open(
            Arc::clone(&session),
            Arc::new(crate::output::Hub::new()),
        );
        (session, journal, sink)
    }
    #[cfg(unix)]
    #[test]
    fn successful_pty_cleanup_releases_projection_without_ack_and_drain_keeps_raw_tail() {
        use crate::slice_budget::ChargeKind;
        let (session, journal, sink) = pty_fixture();
        assert!(journal.budget().used(ChargeKind::Projection) > 0);
        session.note_cleanup(CleanupReport { complete: true });
        assert_eq!(journal.budget().used(ChargeKind::Projection), 0);
        assert!(matches!(
            session.capture_screen(),
            Err(fl_host_proto::ExtensionRefusal::SessionFenced)
        ));
        assert!(
            matches!(session.resize_pty(80, 24), Err(HostError::Protocol(reason)) if reason == "SessionFenced")
        );
        // Cleanup can precede EOF. Both admitted output and forced replay drops
        // must drain into the bounded raw tail without recreating the parser.
        let mut left = 80 * 1024;
        pump_reliable_stdout_with(&session, sink.as_ref(), &journal, |buf| {
            let len = left.min(buf.len());
            buf[..len].fill(b'x');
            left -= len;
            Ok(len)
        });
        session.drop_pty_output(&journal, b"final-tail");
        let tail = session.stdout_tail(usize::MAX);
        assert_eq!(tail.len(), 64 * 1024);
        assert!(tail.ends_with("final-tail"));
        assert_eq!(journal.position().0, 0);
        assert_eq!(journal.budget().used(ChargeKind::Projection), 0);
        assert!(matches!(
            session.capture_screen(),
            Err(fl_host_proto::ExtensionRefusal::SessionFenced)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn incomplete_pty_cleanup_preserves_projection_and_charge() {
        use crate::slice_budget::ChargeKind;
        let (session, journal, _sink) = pty_fixture();
        let charged = journal.budget().used(ChargeKind::Projection);
        session.note_cleanup(CleanupReport { complete: false });
        session.drop_pty_output(&journal, b"still-visible");
        assert_eq!(session.capture_screen().unwrap().contents, "still-visible");
        assert_eq!(journal.budget().used(ChargeKind::Projection), charged);
        assert_eq!(journal.position().0, 0);
    }

    #[cfg(unix)]
    #[test]
    fn pty_projection_precedes_forced_unadmitted_drop_and_retains_raw_bytes() {
        use crate::slice_budget::{ChargeKind, EPOCH_DATA, reserve};
        let (session, journal, sink) = pty_fixture();
        let held = reserve(journal.budget(), ChargeKind::ReliableData, EPOCH_DATA).unwrap();
        assert!(journal.prepare_read().is_err());
        let raw = b"visible\x1b]secret\x07";
        let mut read = false;
        pump_reliable_stdout_with(&session, sink.as_ref(), &journal, |buf| {
            if read {
                let s = session.capture_screen().unwrap();
                assert_eq!(s.contents, "visible");
                assert_eq!(
                    (s.generation, s.output_seq, s.output_bytes),
                    (1, 1, raw.len() as u64)
                );
                assert_eq!(session.stdout_tail(100).as_bytes(), raw);
                assert_eq!(journal.position(), (0, 1));
                Ok(0)
            } else {
                read = true;
                buf[..raw.len()].copy_from_slice(raw);
                Ok(raw.len())
            }
        });
        drop(held);
    }
    #[cfg(unix)]
    #[test]
    fn pty_admitted_chunks_have_coherent_screen_and_watermark() {
        let (session, journal, sink) = pty_fixture();
        let chunks = [b"old\r\x1b[2".as_slice(), b"Knew\n"];
        let mut next = 0;
        pump_reliable_stdout_with(&session, sink.as_ref(), &journal, |buf| {
            let s = session.capture_screen().unwrap();
            assert_eq!(s.generation, next as u64);
            assert_eq!(s.output_seq, journal.position().1);
            assert_eq!(
                s.output_bytes,
                chunks[..next].iter().map(|b| b.len() as u64).sum::<u64>()
            );
            if next == chunks.len() {
                assert_eq!(s.contents, "new");
                return Ok(0);
            }
            let chunk = chunks[next];
            next += 1;
            buf[..chunk.len()].copy_from_slice(chunk);
            Ok(chunk.len())
        });
        assert_eq!(journal.position().0, 0);
    }
    #[cfg(unix)]
    #[test]
    fn projection_refuses_pipe_and_failed_session_ioctl_preserves_screen() {
        assert!(matches!(
            Session::new(parts()).capture_screen(),
            Err(fl_host_proto::ExtensionRefusal::WrongMode)
        ));
        let (_, journal, _) = pty_fixture();
        let projection = crate::projection::Projection::new(
            Arc::clone(journal.budget()),
            fl_host_proto::PtyOptions::default(),
        )
        .unwrap();
        // A regular file gives a real ENOTTY, after growth admission but before
        // any parser mutation; no subprocess or timing-dependent failure.
        let session = Session::with_projection(
            parts(),
            journal,
            projection,
            Some(tempfile::tempfile().unwrap()),
        );
        assert!(session.resize_pty(160, 50).is_err());
        let s = session.capture_screen().unwrap();
        assert_eq!((s.cols, s.rows, s.generation), (120, 40, 0));
    }

    #[cfg(unix)]
    #[test]
    fn concurrent_snapshots_never_mix_projection_and_byte_counters() {
        let (session, journal, _sink) = pty_fixture();
        std::thread::scope(|scope| {
            let session = &session;
            let journal = &journal;
            let writer = scope.spawn(move || {
                for _ in 0..1000 {
                    let mut bytes = journal.prepare_read().unwrap();
                    bytes.as_mut_slice()[..3].copy_from_slice(b"\rx\n");
                    bytes.truncate(3);
                    session.accept_output(journal, bytes).unwrap();
                }
            });
            loop {
                let s = session.capture_screen().unwrap();
                assert_eq!(s.output_bytes, s.generation * 3);
                assert!(s.output_seq >= s.generation);
                if s.generation == 1000 {
                    break;
                }
                std::thread::yield_now();
            }
            writer.join().unwrap();
        });
        assert_eq!(journal.position().0, 0);
    }
}
