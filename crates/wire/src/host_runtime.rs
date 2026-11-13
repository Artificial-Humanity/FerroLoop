//! Blocking host transport for the dependency-inverted vendor runtime port.

use std::path::PathBuf;

use fl_core::vendor_adapter::{
    AdapterIdentityBinding, AdapterReadiness, RuntimePort, TeardownOutcome, VendorAdapterError,
};
use fl_host_proto::{
    BlockingHostClient, ChildState, HostEndpoint, HostError, MAX_RAW_INPUT, SendOutcome,
    SessionMode, SessionSummary,
};

use crate::vendor_adapter::unit_to_host_session_id;

/// A runtime port that connects to the host for each operation.
///
/// The default endpoint is discovered at call time. Explicit endpoints keep callers and tests
/// independent of process-wide environment variables. No session or controller lease is cached.
#[derive(Debug, Clone, Default)]
pub struct HostRuntimePort {
    endpoint: Option<HostEndpoint>,
}

impl HostRuntimePort {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_endpoint(endpoint: HostEndpoint) -> Self {
        Self {
            endpoint: Some(endpoint),
        }
    }

    /// Validate a Unix socket path without connecting to it.
    pub fn with_socket_path(path: impl Into<PathBuf>) -> Result<Self, HostError> {
        HostEndpoint::unix(path.into()).map(Self::with_endpoint)
    }

    fn connect(
        &self,
        binding: &AdapterIdentityBinding,
    ) -> Result<BlockingHostClient, VendorAdapterError> {
        let endpoint = match &self.endpoint {
            Some(endpoint) => endpoint.clone(),
            None => HostEndpoint::discover()
                .map_err(|err| transport(binding, "discover host endpoint", err))?,
        };
        #[cfg(feature = "wire-test-isolation")]
        crate::test_isolation::ensure_fixture(endpoint.socket_path())
            .map_err(|err| transport(binding, "validate test host endpoint", err))?;
        BlockingHostClient::connect(&endpoint)
            .map_err(|err| transport(binding, "connect to host", err))
    }

    fn lookup(
        &self,
        binding: &AdapterIdentityBinding,
    ) -> Result<(BlockingHostClient, Option<SessionSummary>), VendorAdapterError> {
        let mut client = self.connect(binding)?;
        let sessions = client
            .list_sessions()
            .map_err(|err| transport(binding, "list host sessions", err))?;
        let session = unit_to_host_session_id(&binding.host_session).and_then(|wanted| {
            sessions
                .into_iter()
                .filter(|summary| summary.stream.session == wanted)
                .max_by_key(|summary| summary.stream.epoch.0)
        });
        Ok((client, session))
    }
}

fn transport(
    binding: &AdapterIdentityBinding,
    operation: &str,
    error: impl std::fmt::Display,
) -> VendorAdapterError {
    VendorAdapterError::Transport {
        vendor: binding.host_session.clone(),
        reason: format!(
            "{operation} for host session `{}`: {error}",
            binding.host_session
        ),
    }
}

fn pid_matches(binding: &AdapterIdentityBinding, summary: &SessionSummary) -> bool {
    let record = match &summary.state {
        ChildState::Running(record)
        | ChildState::LeaderExited(record)
        | ChildState::CleanupComplete(record) => record,
    };
    // Birth ticks and agent generation are different clocks from host timestamps and epochs.
    binding.pid_birth.is_none_or(|(pid, _)| pid == record.pid)
}

fn require_bound_session(
    binding: &AdapterIdentityBinding,
    session: Option<SessionSummary>,
) -> Result<SessionSummary, VendorAdapterError> {
    let summary =
        session.ok_or_else(|| transport(binding, "resolve binding", "session is absent"))?;
    if !pid_matches(binding, &summary) {
        return Err(transport(
            binding,
            "resolve binding",
            "host PID changed; refresh the session binding",
        ));
    }
    Ok(summary)
}

impl RuntimePort for HostRuntimePort {
    fn session_readiness(
        &self,
        binding: &AdapterIdentityBinding,
    ) -> Result<AdapterReadiness, VendorAdapterError> {
        let (_, session) = self.lookup(binding)?;
        let Some(summary) = session.filter(|summary| pid_matches(binding, summary)) else {
            return Ok(AdapterReadiness::Exited);
        };
        Ok(match summary.state {
            ChildState::Running(_) => AdapterReadiness::ReadyIdle,
            ChildState::LeaderExited(_) => AdapterReadiness::Exiting,
            ChildState::CleanupComplete(_) => AdapterReadiness::Exited,
        })
    }

    fn write_bytes(
        &self,
        binding: &AdapterIdentityBinding,
        bytes: &[u8],
    ) -> Result<(), VendorAdapterError> {
        let (mut client, session) = self.lookup(binding)?;
        let summary = require_bound_session(binding, session)?;
        if !matches!(summary.state, ChildState::Running(_)) {
            return Err(transport(
                binding,
                "write bytes",
                "session is no longer running",
            ));
        }
        let grant = client
            .claim_controller(summary.stream.clone())
            .map_err(|err| transport(binding, "claim input controller", err))?;
        let outcome = client
            .write_raw(summary.stream, grant.lease, bytes.to_vec())
            .map_err(|err| {
                transport(
                    binding,
                    "write raw input (delivery may be unknown; do not retry blindly)",
                    err,
                )
            })?;
        match outcome {
            SendOutcome::Written => Ok(()),
            other => Err(transport(
                binding,
                "write raw input (do not retry blindly)",
                format!("{other:?}"),
            )),
        }
    }

    fn capture_output(
        &self,
        binding: &AdapterIdentityBinding,
        max_bytes: usize,
    ) -> Result<String, VendorAdapterError> {
        let (mut client, session) = self.lookup(binding)?;
        let summary = require_bound_session(binding, session)?;
        if summary.mode == SessionMode::Pty
            && let Ok(screen) = client.capture_rendered(summary.stream.clone())
        {
            return Ok(screen.contents);
        }
        let limit = u32::try_from(max_bytes)
            .unwrap_or(u32::MAX)
            .min(MAX_RAW_INPUT as u32);
        client
            .capture_output(summary.stream.session, limit)
            .map_err(|err| transport(binding, "capture raw output", err))
    }

    fn stop_session(
        &self,
        binding: &AdapterIdentityBinding,
    ) -> Result<TeardownOutcome, VendorAdapterError> {
        let (mut client, session) = self.lookup(binding)?;
        let Some(summary) = session.filter(|summary| pid_matches(binding, summary)) else {
            return Ok(TeardownOutcome::AlreadyAbsent);
        };
        if matches!(summary.state, ChildState::CleanupComplete(_)) {
            return Ok(TeardownOutcome::AlreadyAbsent);
        }
        let grant = client
            .claim_controller(summary.stream.clone())
            .map_err(|err| transport(binding, "claim stop controller", err))?;
        client
            .kill_session(summary.stream, grant.lease)
            .map_err(|err| transport(binding, "kill host session", err))?;
        Ok(TeardownOutcome::Completed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_host_proto::{
        ChildRecord, HostInstance, InputState, SessionEpoch, SessionId, SpawnKind, StreamId,
    };

    #[test]
    fn pid_guard_uses_pid_only_across_all_child_states() {
        let session = SessionId::new("ada.claude").unwrap();
        let record = ChildRecord {
            session: session.clone(),
            session_epoch: SessionEpoch(3),
            kind: SpawnKind::Pipe,
            pid: 42,
            input: InputState::Writable,
            started_at_ms: 100,
            exit: None,
            consumer_stalled: false,
        };
        for state in [
            ChildState::Running(record.clone()),
            ChildState::LeaderExited(record.clone()),
            ChildState::CleanupComplete(record),
        ] {
            let summary = SessionSummary {
                stream: StreamId {
                    host: HostInstance::new([1; 16]),
                    session: session.clone(),
                    epoch: SessionEpoch(3),
                },
                mode: SessionMode::ReliablePipe,
                state,
                acked: 0,
                latest: 0,
                input: InputState::Writable,
            };
            assert!(pid_matches(
                &AdapterIdentityBinding::new("ada", 900, "ada.claude", Some((42, 999999))),
                &summary
            ));
            assert!(pid_matches(
                &AdapterIdentityBinding::new("ada", 900, "ada.claude", None),
                &summary
            ));
            assert!(!pid_matches(
                &AdapterIdentityBinding::new("ada", 900, "ada.claude", Some((43, 100))),
                &summary
            ));
        }
    }
}
