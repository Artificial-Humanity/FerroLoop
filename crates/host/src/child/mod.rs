//! Per-session owner. Unix keeps the supervisor control writer here. Windows keeps
//! the kill-on-close Job. `LeaderExited` and `CleanupComplete` are separate, and the
//! owner stays reachable after the leader exits.

use std::time::Duration;

use fl_host_proto::{ChildExit, ChildState, HostError, SessionEpoch, SessionId};

#[cfg(unix)]
pub mod unix;
#[cfg(windows)]
pub mod windows;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CleanupReport {
    pub complete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DescendantExit {
    pub pid: u32,
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub adopted: bool,
}

#[derive(Clone)]
pub struct SessionHandle {
    pub(crate) inner: std::sync::Arc<crate::session::Session>,
}

impl SessionHandle {
    pub fn session(&self) -> &SessionId {
        &self.inner.session
    }

    pub fn epoch(&self) -> SessionEpoch {
        SessionEpoch(self.inner.epoch)
    }

    pub fn vendor_pid(&self) -> u32 {
        self.inner.vendor_pid
    }

    pub fn vendor_start(&self) -> u64 {
        self.inner.vendor_start
    }

    pub fn supervisor_pid(&self) -> u32 {
        self.inner.supervisor_pid
    }

    pub fn pgid(&self) -> u32 {
        self.inner.pgid
    }

    pub fn control_inode(&self) -> u64 {
        self.inner.control_inode
    }

    pub fn state(&self) -> ChildState {
        self.inner.state()
    }

    pub fn write_stdin(&self, bytes: &[u8]) -> Result<(), HostError> {
        self.inner.write_stdin(bytes)
    }

    pub fn read_stdout_until(
        &self,
        needle: &[u8],
        timeout: Duration,
    ) -> Result<Vec<u8>, HostError> {
        self.inner.read_stdout_until(needle, timeout)
    }

    pub fn wait_leader(&self, timeout: Duration) -> Result<ChildExit, HostError> {
        self.inner.wait_leader(timeout)
    }

    pub fn wait_cleanup(&self, timeout: Duration) -> Result<CleanupReport, HostError> {
        self.inner.wait_cleanup(timeout)
    }

    pub fn descendants(&self) -> Vec<DescendantExit> {
        self.inner.descendants()
    }

    pub fn request_cleanup(&self) -> Result<(), HostError> {
        self.inner.request_cleanup()
    }
}

impl std::fmt::Debug for SessionHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionHandle")
            .field("session", &self.inner.session)
            .field("epoch", &self.inner.epoch)
            .field("vendor_pid", &self.inner.vendor_pid)
            .finish()
    }
}
