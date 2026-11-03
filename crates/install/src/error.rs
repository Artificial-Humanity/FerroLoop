//! The one error type of the transactional installer, and the exit status each kind maps to.

use crate::fault::FaultPoint;
use fl_core::install::PlanError;
use std::io;

/// Why an install, resume or rollback did not complete.
#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    /// The request is not well formed (exit 2).
    #[error("{0}")]
    Usage(String),
    /// A well-formed request the installer refuses, before or without changing anything it
    /// does not own (exit 3).
    #[error("{0}")]
    Refused(String),
    /// The planner refused (exit 2 or 3, as the planner says).
    #[error("{0}")]
    Plan(#[from] PlanError),
    /// Another installer holds `install.lock` (exit 3).
    #[error("another install is running{}; nothing was changed", pid_note(.pid))]
    Busy { pid: Option<u32> },
    /// A file system call failed (exit 1).
    #[error("{what}: {source}")]
    Io {
        what: String,
        #[source]
        source: io::Error,
    },
    /// Test seam: the process "died" here. Nothing is undone.
    #[error("simulated crash at {0}")]
    Crashed(FaultPoint),
    /// A step failed and the transaction was undone; the install is as it was (exit as the cause).
    #[error("{cause}; the transaction was rolled back and nothing was changed")]
    RolledBack { cause: Box<InstallError> },
    /// A step failed and the undo failed too: the journal stays for `fl install --resume`
    /// (exit 1).
    #[error(
        "{cause}; undoing it failed too ({undo}); the journal is kept: run `fl install --resume`"
    )]
    UndoFailed {
        cause: Box<InstallError>,
        undo: Box<InstallError>,
    },
}

fn pid_note(pid: &Option<u32>) -> String {
    pid.map(|p| format!(" (pid {p})")).unwrap_or_default()
}

impl InstallError {
    pub fn io(what: impl Into<String>, source: io::Error) -> Self {
        Self::Io {
            what: what.into(),
            source,
        }
    }

    pub fn refused(msg: impl Into<String>) -> Self {
        Self::Refused(msg.into())
    }

    /// The process exit status: 2 malformed request, 3 refused, 1 a failure.
    pub fn exit_code(&self) -> i32 {
        match self {
            InstallError::Usage(_) => 2,
            InstallError::Refused(_) | InstallError::Busy { .. } => 3,
            InstallError::Plan(e) => e.exit_code(),
            InstallError::Io { .. } | InstallError::UndoFailed { .. } => 1,
            InstallError::Crashed(_) => 70,
            InstallError::RolledBack { cause } => cause.exit_code(),
        }
    }
}

pub type Result<T> = std::result::Result<T, InstallError>;

/// Attach what was being done to a bare `io::Error`.
pub(crate) trait Ctx<T> {
    fn ctx(self, what: impl FnOnce() -> String) -> Result<T>;
}

impl<T> Ctx<T> for io::Result<T> {
    fn ctx(self, what: impl FnOnce() -> String) -> Result<T> {
        self.map_err(|e| InstallError::io(what(), e))
    }
}
