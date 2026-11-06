//! Finite containment bounds. Cleanup never waits past these.

use std::time::Duration;

/// How long a supervisor waits after SIGTERM before SIGKILL.
pub const TERM_GRACE: Duration = Duration::from_millis(400);

/// Bound for one session's TERM-to-KILL cleanup, including reaping.
pub const CLEANUP_DEADLINE: Duration = Duration::from_secs(5);

/// Bound for startup recovery. Unproven ownership refuses inside this window.
pub const STARTUP_CLEANUP_DEADLINE: Duration = Duration::from_secs(5);

/// Bound for the publication handshake (identity, durable replace, exec ack).
pub const HANDOFF_TIMEOUT: Duration = Duration::from_secs(5);
