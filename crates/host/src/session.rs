//! In-memory owner for one session. Disk updates are applied by the host before waiters wake.

use std::fs::File;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fl_host_proto::{
    ChildExit, ChildRecord, ChildState, HostError, InputState, MAX_STDOUT_BACKLOG,
    STDERR_TAIL_BYTES, SessionEpoch, SessionId, SpawnKind,
};

use crate::child::{CleanupReport, DescendantExit};

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
    control: Mutex<Option<File>>,
    stdin: Mutex<Option<File>>,
    leader: Mutex<Option<ChildExit>>,
    leader_cv: Condvar,
    cleanup: Mutex<Option<CleanupReport>>,
    cleanup_cv: Condvar,
    descendants: Mutex<Vec<DescendantExit>>,
    stdout: Mutex<OutBuf>,
    stdout_cv: Condvar,
    stderr: Mutex<Vec<u8>>,
    cleanup_requested: AtomicBool,
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
    pub fn new(parts: SessionParts) -> Self {
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
            control: Mutex::new(control),
            stdin: Mutex::new(stdin),
            leader: Mutex::new(None),
            leader_cv: Condvar::new(),
            cleanup: Mutex::new(None),
            cleanup_cv: Condvar::new(),
            descendants: Mutex::new(Vec::new()),
            stdout: Mutex::new(OutBuf {
                bytes: Vec::new(),
                eof: false,
            }),
            stdout_cv: Condvar::new(),
            stderr: Mutex::new(Vec::new()),
            cleanup_requested: AtomicBool::new(false),
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

    #[cfg(windows)]
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
        let mut slot = self.leader.lock().unwrap_or_else(|err| err.into_inner());
        if slot.is_none() {
            *slot = Some(exit);
        }
        self.leader_cv.notify_all();
    }

    pub fn note_descendant(&self, exit: DescendantExit) {
        self.descendants
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .push(exit);
    }

    pub fn note_cleanup(&self, report: CleanupReport) {
        let mut slot = self.cleanup.lock().unwrap_or_else(|err| err.into_inner());
        if slot.is_none() {
            *slot = Some(report);
        }
        self.cleanup_cv.notify_all();
    }

    pub fn leader_exit(&self) -> Option<ChildExit> {
        self.leader
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clone()
    }

    pub fn cleanup_report(&self) -> Option<CleanupReport> {
        *self.cleanup.lock().unwrap_or_else(|err| err.into_inner())
    }

    pub fn push_stdout(&self, chunk: &[u8]) {
        let mut buf = self.stdout.lock().unwrap_or_else(|err| err.into_inner());
        let room = MAX_STDOUT_BACKLOG.saturating_sub(buf.bytes.len());
        let take = room.min(chunk.len());
        buf.bytes.extend_from_slice(&chunk[..take]);
        self.stdout_cv.notify_all();
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

    pub fn state(&self) -> ChildState {
        let exit = self.leader_exit();
        let cleanup = self.cleanup_report();
        let record = ChildRecord {
            session: self.session.clone(),
            session_epoch: SessionEpoch(self.epoch),
            kind: SpawnKind::Pipe,
            pid: self.vendor_pid,
            input: InputState::Writable,
            started_at_ms: self.started_at_ms,
            exit,
            consumer_stalled: false,
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

pub fn pump_stdout(mut file: File, session: std::sync::Arc<Session>) {
    let mut buf = [0u8; 8192];
    loop {
        match file.read(&mut buf) {
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
}

pub fn pump_stderr(mut file: File, session: std::sync::Arc<Session>) {
    let mut buf = [0u8; 8192];
    loop {
        match file.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => session.push_stderr(&buf[..n]),
        }
    }
}
