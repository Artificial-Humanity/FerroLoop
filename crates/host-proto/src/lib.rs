//! Typed host IPC: messages, bounded frames, and authenticated local transport.
//!
//! There is no TCP listener. Unix sockets and Windows named pipes are the only
//! endpoints. Named limits live in [`types`].

mod auth;
mod client;
mod endpoint;
mod extension;
pub use extension::*;
mod frame;
mod transport;
mod types;

pub use auth::{
    Deadlines, HandshakeParams, auth_transcript, client_handshake, client_hello_frame,
    client_proof, decode_challenge, read_frame_async, reject_equal_nonces, server_handshake,
    server_proof, verify_client_proof, verify_server_proof, write_frame_async,
};
#[cfg(feature = "hub-test-isolation")]
pub use client::install_test_endpoint_validator;
pub use client::{
    AsyncHostClient, BlockingHostClient, ConnectOptions, ExtensionEventStream, HostEventStream,
    ReliableControllerGrant, ReliableEventStream, events,
};
pub use endpoint::HostEndpoint;
#[cfg(feature = "hub-test-isolation")]
pub use endpoint::enable_hub_test_isolation;
#[cfg(all(feature = "hub-test-isolation", unix))]
pub use endpoint::{HubTestHostFixture, register_hub_test_host_fixture};
#[cfg(unix)]
pub use endpoint::{probe_explicit_run_user_dir, probe_run_user_dir};
pub use frame::{
    EventSeq, Frame, FrameKind, LineReassembler, ResponseCorrelator, checked_frame_len,
    checked_unauth_len, decode_request, decode_request_for_minor, decode_response,
    decode_response_for_minor, encode_request, encode_request_for_minor, encode_response,
    encode_response_for_minor, fragment_line, fragment_line_with_termination, frame_total_len,
    line_fragment, line_fragment_with_termination, read_frame, write_frame,
};
#[cfg(any(target_os = "linux", target_os = "android"))]
pub use transport::interpret_ucred;
pub use transport::{HostConn, HostListener, ShutdownSwitch, TokenControl};
pub use types::{
    AuthToken, BindDisposition, CLIENT_DOMAIN, ChildExit, ChildRecord, ChildState, ControlEvent,
    ControllerLease, DEFAULT_MAX_SESSIONS, FRAME_READ_TIMEOUT, FirstInstance, HANDSHAKE_TIMEOUT,
    HostError, HostEvent, HostRequest, HostResponse, HostedChild, InputState, InstanceLedger,
    LINE_HEADER_LEN, MAX_ARG_BYTES, MAX_ARGV, MAX_ARGV_AGGREGATE, MAX_CWD_BYTES, MAX_ENV_AGGREGATE,
    MAX_ENV_ENTRIES, MAX_ENV_NAME, MAX_ENV_VALUE, MAX_EVENT_ENVELOPE, MAX_FRAGMENTS,
    MAX_FRAME_PAYLOAD, MAX_LINE_BYTES, MAX_REASSEMBLY_BYTES, MAX_STDOUT_BACKLOG, MAX_STDOUT_LINE,
    MAX_SUBSCRIBERS, MAX_UNAUTH_PAYLOAD, MAX_UNAUTHENTICATED, MAX_WINDOWS_COMMAND_UNITS,
    MutexFastPath, NamedPipeClientOptions, NamedPipeServerOptions, PROTO_MAJOR, PROTO_MINOR,
    PeerQuery, RequestId, SERVER_DOMAIN, STDERR_TAIL_BYTES, STDIN_QUEUE_BYTES, STDIN_QUEUE_FRAMES,
    STDIN_WRITE_TIMEOUT, SendBlock, SendOutcome, SessionEpoch, SessionId, SignalKind, SpawnKind,
    SpawnSpec, TokenFileCreate, WRITE_TIMEOUT, WinPeerLookup, dacl_is_single_sid,
    decide_first_instance, pipe_dacl_sddl, private_parent_ok, supports_message, validate_unix_peer,
    validate_win_peer, windows_command_line, windows_environment_block,
};

use std::time::Duration;

/// Listener and client settings. Debug output is redacted.
pub struct HostConfig {
    pub endpoint: HostEndpoint,
    pub daemon_version: String,
    pub handshake_timeout: Duration,
    pub frame_timeout: Duration,
    pub write_timeout: Duration,
    pub minor: u16,
    pub major: u16,
    pub max_unauthenticated: usize,
}

impl HostConfig {
    pub fn new(endpoint: HostEndpoint) -> Self {
        Self {
            endpoint,
            daemon_version: env!("CARGO_PKG_VERSION").to_owned(),
            handshake_timeout: HANDSHAKE_TIMEOUT,
            frame_timeout: FRAME_READ_TIMEOUT,
            write_timeout: WRITE_TIMEOUT,
            minor: PROTO_MINOR,
            major: PROTO_MAJOR,
            max_unauthenticated: MAX_UNAUTHENTICATED,
        }
    }
}

impl std::fmt::Debug for HostConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HostConfig(redacted)")
    }
}
