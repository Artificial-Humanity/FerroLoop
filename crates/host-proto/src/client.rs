//! Blocking and async clients. Both use one handshake.
//!
//! A token-proof failure may reconnect once, and only when a fresh validated
//! read of the token file differs from the token that failed. The snapshot
//! used for the proof is taken before `pre_proof`, so a rotation in that hook
//! is a different token on the daemon.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::auth;
#[cfg(unix)]
use crate::auth::Deadlines;
use crate::endpoint::HostEndpoint;
use crate::frame::{LineReassembler, decode_response_for_minor, encode_request_for_minor};
#[cfg(unix)]
use crate::types::AuthToken;
#[cfg(unix)]
use crate::types::HANDSHAKE_TIMEOUT;
#[cfg(unix)]
use crate::types::constant_time_eq;
use crate::types::{
    FRAME_READ_TIMEOUT, HostError, HostEvent, HostRequest, HostResponse, SessionId, WRITE_TIMEOUT,
};

#[derive(Clone)]
pub struct ConnectOptions {
    pub major: u16,
    pub minor: u16,
    pub pre_proof: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Runs after the token snapshot and before the platform open.
    /// A rename here is still authenticated; it is not a bypass.
    pub pre_connect: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl Default for ConnectOptions {
    fn default() -> Self {
        Self {
            major: crate::PROTO_MAJOR,
            minor: crate::PROTO_MINOR,
            pre_proof: None,
            pre_connect: None,
        }
    }
}

enum ClientIo {
    #[cfg(unix)]
    Unix(tokio::net::UnixStream),
    #[cfg(windows)]
    Pipe(tokio::net::windows::named_pipe::NamedPipeClient),
    #[cfg(test)]
    Mem(tokio::io::DuplexStream),
}

enum SessionMode {
    Rpc,
    Stream,
}

// The split API can pipeline requests. IDs remain increasing across both APIs.
#[derive(Default)]
struct ClientCorrelator {
    last: u64,
    outstanding: std::collections::BTreeSet<u64>,
}

impl ClientCorrelator {
    fn new() -> Self {
        Self::default()
    }
    fn submit(&mut self, id: u64) -> Result<(), HostError> {
        if id == 0 || id <= self.last {
            return Err(HostError::Protocol("request id refused".into()));
        }
        self.outstanding.insert(id);
        self.last = id;
        Ok(())
    }
    fn complete(&mut self, id: u64) -> Result<(), HostError> {
        if !self.outstanding.remove(&id) {
            return Err(HostError::Protocol("response id refused".into()));
        }
        Ok(())
    }
}

pub struct AsyncHostClient {
    io: ClientIo,
    major: u16,
    minor: u16,
    correlator: ClientCorrelator,
    frame_timeout: Duration,
    write_timeout: Duration,
    mode: SessionMode,
    endpoint: Option<HostEndpoint>,
}

impl std::fmt::Debug for AsyncHostClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AsyncHostClient")
            .field("major", &self.major)
            .field("minor", &self.minor)
            .finish()
    }
}

// Optional guard installed before wire test processes start. Production and other test
// binaries never install it, including when Cargo unifies this feature.
#[cfg(feature = "hub-test-isolation")]
type TestEndpointValidator = fn(&HostEndpoint) -> Result<(), HostError>;
#[cfg(feature = "hub-test-isolation")]
static TEST_ENDPOINT_VALIDATOR: std::sync::OnceLock<TestEndpointValidator> =
    std::sync::OnceLock::new();

#[cfg(feature = "hub-test-isolation")]
#[doc(hidden)]
pub fn install_test_endpoint_validator(validator: fn(&HostEndpoint) -> Result<(), HostError>) {
    TEST_ENDPOINT_VALIDATOR
        .set(validator)
        .expect("host test validator installed twice");
}

#[cfg(feature = "hub-test-isolation")]
pub(crate) fn validate_test_endpoint(endpoint: &HostEndpoint) -> Result<(), HostError> {
    if let Some(validate) = TEST_ENDPOINT_VALIDATOR.get() {
        validate(endpoint)?;
    }
    Ok(())
}

impl AsyncHostClient {
    pub async fn connect(endpoint: &HostEndpoint) -> Result<Self, HostError> {
        Self::connect_with(endpoint, ConnectOptions::default()).await
    }

    pub async fn connect_with(
        endpoint: &HostEndpoint,
        options: ConnectOptions,
    ) -> Result<Self, HostError> {
        #[cfg(feature = "hub-test-isolation")]
        validate_test_endpoint(endpoint)?;
        #[cfg(unix)]
        {
            let mut client = connect_retry(endpoint, &options).await?;
            client.endpoint = Some(endpoint.clone());
            Ok(client)
        }
        #[cfg(windows)]
        {
            let mut client = crate::transport::windows::connect_client(endpoint, &options).await?;
            client.endpoint = Some(endpoint.clone());
            Ok(client)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (endpoint, options);
            Err(HostError::Protocol(
                "no host transport on this platform".into(),
            ))
        }
    }

    #[cfg(unix)]
    pub async fn connect_preconnected(
        endpoint: &HostEndpoint,
        stream: tokio::net::UnixStream,
    ) -> Result<Self, HostError> {
        let canonical = canonical_unix(endpoint)?;
        let token = crate::transport::unix::read_token(&canonical)?;
        let mut client = drive_unix(canonical, token, stream, &ConnectOptions::default()).await?;
        client.endpoint = Some(endpoint.clone());
        Ok(client)
    }

    pub async fn request(&mut self, request: HostRequest) -> Result<HostResponse, HostError> {
        if !self.correlator.outstanding.is_empty() {
            return Err(HostError::Protocol(
                "split responses are still outstanding".into(),
            ));
        }
        let id = request.request_id();
        self.write_request(&request).await?;
        let response = self.read_response().await?;
        if response.request_id() != id {
            return Err(HostError::Protocol("response id does not match".into()));
        }
        Ok(response)
    }

    /// Writes one request and tracks its ID until `read_response` completes it.
    pub async fn write_request(&mut self, request: &HostRequest) -> Result<(), HostError> {
        if !matches!(self.mode, SessionMode::Rpc) {
            return Err(HostError::Protocol(
                "stream connection accepts no RPC".into(),
            ));
        }
        if !crate::supports_message(self.minor, request.op_name()) {
            return Err(HostError::Protocol(
                "message is not in the negotiated minor".into(),
            ));
        }
        let frame = encode_request_for_minor(self.minor, request)?;
        self.correlator.submit(request.request_id().0)?;
        let deadline = Instant::now() + self.write_timeout;
        write_client(&mut self.io, &frame, deadline).await
    }

    /// Next response frame. Pair with `write_request` when more than one request is in flight.
    pub async fn read_response(&mut self) -> Result<HostResponse, HostError> {
        if !matches!(self.mode, SessionMode::Rpc) {
            return Err(HostError::Protocol(
                "stream connection accepts no RPC".into(),
            ));
        }
        let response = read_response_io(&mut self.io, self.frame_timeout, self.minor).await?;
        self.correlator.complete(response.request_id().0)?;
        Ok(response)
    }

    pub fn negotiated(&self) -> (u16, u16) {
        (self.major, self.minor)
    }

    pub fn allows(&self, name: &str) -> bool {
        crate::supports_message(self.minor, name)
    }

    /// Explicit minor-one operation. Refusal never triggers a legacy fallback.
    pub async fn extension_request(
        &mut self,
        request: crate::ExtensionRequest,
    ) -> Result<crate::ExtensionResponse, HostError> {
        let id = self
            .correlator
            .last
            .checked_add(1)
            .ok_or_else(|| HostError::Protocol("request id exhausted".into()))?;
        match self
            .request(HostRequest::Extended {
                id: crate::RequestId(id),
                request,
            })
            .await?
        {
            HostResponse::Extended { response, .. } => Ok(response),
            _ => Err(HostError::Protocol("extension response required".into())),
        }
    }
    /// Query the serving binary. None explicitly means identity is unavailable on
    /// this older host; never synthesize it from this client's compiled version.
    pub async fn host_status(&mut self) -> Result<Option<crate::HostBuildInfo>, HostError> {
        if self.minor < 1 {
            return Ok(None);
        }
        match self
            .extension_request(crate::ExtensionRequest::Capabilities)
            .await?
        {
            crate::ExtensionResponse::Capabilities { features, .. } => {
                if features & crate::HOST_STATUS_FEATURE == 0 {
                    return Ok(None);
                }
            }
            _ => return Err(HostError::Protocol("capabilities response required".into())),
        }
        match self
            .extension_request(crate::ExtensionRequest::HostStatus)
            .await?
        {
            crate::ExtensionResponse::HostStatus { build } => Ok(Some(build)),
            _ => Err(HostError::Protocol("host status response required".into())),
        }
    }

    /// None means this host cannot report retention; it never means zero usage.
    pub async fn pty_retention_status(
        &mut self,
    ) -> Result<Option<crate::PtyRetentionStatus>, HostError> {
        if self.minor < 1 {
            return Ok(None);
        }
        match self
            .extension_request(crate::ExtensionRequest::Capabilities)
            .await?
        {
            crate::ExtensionResponse::Capabilities { features, .. } => {
                if features & crate::PTY_RETENTION_FEATURE == 0 {
                    return Ok(None);
                }
            }
            _ => return Err(HostError::Protocol("capabilities response required".into())),
        }
        match self
            .extension_request(crate::ExtensionRequest::PtyRetentionStatus)
            .await?
        {
            crate::ExtensionResponse::PtyRetentionStatus { status } => Ok(Some(status)),
            _ => Err(HostError::Protocol(
                "PTY retention status response required".into(),
            )),
        }
    }

    // Raw protocol requests retain their existing refusal shape. Typed helpers
    // enrich failures once, without resending the mutation or recursively diagnosing.
    async fn diagnosed_extension_request(
        &mut self,
        request: crate::ExtensionRequest,
    ) -> Result<crate::ExtensionResponse, HostError> {
        use crate::{ExtensionRefusal as F, ExtensionRequest as R, ExtensionResponse as S};
        let stream = match &request {
            R::ClaimController { stream }
            | R::SubscribeReliable { stream, .. }
            | R::Observe { stream, .. }
            | R::SendLine { stream, .. }
            | R::SignalSession { stream, .. }
            | R::KillSession { stream, .. }
            | R::Write { stream, .. }
            | R::CaptureRendered { stream }
            | R::Resize { stream, .. } => Some(stream.clone()),
            R::Ack { cursor, .. } => Some(cursor.stream.clone()),
            _ => None,
        };
        let pty_capacity = matches!(
            &request,
            R::SpawnSession {
                mode: crate::SessionMode::Pty,
                ..
            } | R::Resize { .. }
        );
        let response = self.extension_request(request).await?;
        if matches!(
            &response,
            S::Refused {
                code: F::StaleEpoch
            }
        ) && let Some(stream) = stream
        {
            let diagnostic = self.pty_retention_status().await.ok().flatten();
            let evicted = diagnostic
                .as_ref()
                .is_some_and(|s| s.recent_evictions.contains(&stream));
            return Err(if evicted {
                HostError::PtyEpochEvicted { stream }
            } else {
                HostError::StaleEpoch { stream }
            });
        }
        if pty_capacity
            && matches!(
                &response,
                S::Refused {
                    code: F::ResourceLimit
                }
            )
        {
            let usage = match self.pty_retention_status().await.ok().flatten() {
                Some(s) => format!(
                    "{} / {} bytes reserved",
                    s.projection_reserved_bytes, s.projection_limit_bytes
                ),
                None => "usage/limit unavailable on this host".into(),
            };
            return Err(HostError::Protocol(format!(
                "PTY operation refused (ResourceLimit); projection capacity is one possible cause ({usage}). Inspect fl host status; reduce geometry or scrollback, or stop unused live sessions. The host does not identify the specific spawn failure."
            )));
        }
        Ok(response)
    }

    /// False for missing/mismatched ownership or an older host without this capability.
    /// The caller must use the same stream for the subsequent controller claim and mutation.
    pub async fn check_session_owner(
        &mut self,
        stream: crate::StreamId,
        hosted_id: &str,
    ) -> Result<bool, HostError> {
        if hosted_id.is_empty() || hosted_id.len() > crate::MAX_HOSTED_OWNER_BYTES || self.minor < 1
        {
            return Ok(false);
        }
        match self
            .extension_request(crate::ExtensionRequest::Capabilities)
            .await?
        {
            crate::ExtensionResponse::Capabilities { features, .. } => {
                if features & crate::SESSION_OWNER_FEATURE == 0 {
                    return Ok(false);
                }
            }
            _ => return Err(HostError::Protocol("capabilities response required".into())),
        }
        match self
            .extension_request(crate::ExtensionRequest::CheckSessionOwner {
                stream,
                hosted_id: hosted_id.to_owned(),
            })
            .await?
        {
            crate::ExtensionResponse::SessionOwner { matches } => Ok(matches),
            _ => Err(HostError::Protocol(
                "session owner response required".into(),
            )),
        }
    }

    pub async fn claim_controller(
        &mut self,
        stream: crate::StreamId,
    ) -> Result<ReliableControllerGrant, HostError> {
        match self
            .diagnosed_extension_request(crate::ExtensionRequest::ClaimController { stream })
            .await?
        {
            crate::ExtensionResponse::Controller {
                lease,
                acked,
                latest,
            } => Ok(ReliableControllerGrant {
                lease,
                acked,
                latest,
            }),
            other => Err(HostError::Protocol(format!(
                "controller refused: {other:?}"
            ))),
        }
    }
    pub async fn subscribe_reliable(
        &mut self,
        stream: crate::StreamId,
        lease: crate::ControllerLease,
        after: Option<u64>,
    ) -> Result<ReliableEventStream, HostError> {
        let response = self
            .diagnosed_extension_request(crate::ExtensionRequest::SubscribeReliable {
                stream: stream.clone(),
                lease,
                after_seq: after,
            })
            .await?;
        let (ticket, acked) = match response {
            crate::ExtensionResponse::Subscription { ticket, acked, .. } => (ticket, acked),
            other => {
                return Err(HostError::Protocol(format!(
                    "subscription refused: {other:?}"
                )));
            }
        };
        let endpoint = self
            .endpoint
            .as_ref()
            .ok_or_else(|| HostError::Protocol("event endpoint unavailable".into()))?;
        let event = Self::connect_with(
            endpoint,
            ConnectOptions {
                major: self.major,
                minor: self.minor,
                ..ConnectOptions::default()
            },
        )
        .await?;
        let inner = event.open_extension_stream(ticket).await?;
        let seq = after.unwrap_or(acked);
        Ok(ReliableEventStream {
            inner,
            reassembly: crate::ReliableReassembler::for_stream(stream.clone(), seq),
            cursor: crate::Cursor { stream, seq },
        })
    }
    /// Call only after durable application, ownership mapping and cursor commit.
    pub async fn ack(
        &mut self,
        cursor: crate::Cursor,
        lease: crate::ControllerLease,
    ) -> Result<u64, HostError> {
        match self
            .diagnosed_extension_request(crate::ExtensionRequest::Ack { cursor, lease })
            .await?
        {
            crate::ExtensionResponse::Acked { through } => Ok(through),
            other => Err(HostError::Protocol(format!("ack refused: {other:?}"))),
        }
    }

    pub async fn list_sessions(&mut self) -> Result<Vec<crate::SessionSummary>, HostError> {
        match self
            .diagnosed_extension_request(crate::ExtensionRequest::ListSessions)
            .await?
        {
            crate::ExtensionResponse::Sessions { records } => Ok(records),
            other => Err(HostError::Protocol(format!(
                "list_sessions refused: {other:?}"
            ))),
        }
    }

    pub async fn spawn_reliable_session(
        &mut self,
        spec: crate::SpawnSpec,
    ) -> Result<(crate::StreamId, crate::ChildState), HostError> {
        match self
            .diagnosed_extension_request(crate::ExtensionRequest::SpawnSession {
                spec,
                mode: crate::SessionMode::ReliablePipe,
                pty: None,
            })
            .await?
        {
            crate::ExtensionResponse::Session { stream, state } => Ok((stream, state)),
            other => Err(HostError::Protocol(format!(
                "spawn_session refused: {other:?}"
            ))),
        }
    }

    pub async fn spawn_pty_session(
        &mut self,
        spec: crate::SpawnSpec,
        pty: Option<crate::PtyOptions>,
    ) -> Result<(crate::StreamId, crate::ChildState), HostError> {
        match self
            .diagnosed_extension_request(crate::ExtensionRequest::SpawnSession {
                spec,
                mode: crate::SessionMode::Pty,
                pty,
            })
            .await?
        {
            crate::ExtensionResponse::Session { stream, state } => Ok((stream, state)),
            other => Err(HostError::Protocol(format!(
                "spawn_pty_session refused: {other:?}"
            ))),
        }
    }

    pub async fn send_line(
        &mut self,
        stream: crate::StreamId,
        lease: crate::ControllerLease,
        line: String,
    ) -> Result<crate::SendOutcome, HostError> {
        match self
            .diagnosed_extension_request(crate::ExtensionRequest::SendLine {
                stream,
                lease,
                line,
            })
            .await?
        {
            crate::ExtensionResponse::Input { outcome } => Ok(outcome),
            other => Err(HostError::Protocol(format!("send_line refused: {other:?}"))),
        }
    }

    pub async fn write_raw(
        &mut self,
        stream: crate::StreamId,
        lease: crate::ControllerLease,
        bytes: Vec<u8>,
    ) -> Result<crate::SendOutcome, HostError> {
        let bytes = crate::RawInput::new(bytes)?;
        match self
            .diagnosed_extension_request(crate::ExtensionRequest::Write {
                stream,
                lease,
                bytes,
            })
            .await?
        {
            crate::ExtensionResponse::Input { outcome } => Ok(outcome),
            other => Err(HostError::Protocol(format!("write_raw refused: {other:?}"))),
        }
    }

    pub async fn signal_session(
        &mut self,
        stream: crate::StreamId,
        lease: crate::ControllerLease,
        signal: crate::SignalKind,
    ) -> Result<(), HostError> {
        match self
            .diagnosed_extension_request(crate::ExtensionRequest::SignalSession {
                stream,
                lease,
                signal,
            })
            .await?
        {
            crate::ExtensionResponse::Controlled => Ok(()),
            other => Err(HostError::Protocol(format!(
                "signal_session refused: {other:?}"
            ))),
        }
    }

    pub async fn kill_session(
        &mut self,
        stream: crate::StreamId,
        lease: crate::ControllerLease,
    ) -> Result<(), HostError> {
        match self
            .diagnosed_extension_request(crate::ExtensionRequest::KillSession { stream, lease })
            .await?
        {
            crate::ExtensionResponse::Controlled => Ok(()),
            other => Err(HostError::Protocol(format!(
                "kill_session refused: {other:?}"
            ))),
        }
    }

    async fn require_rendered_pty(&mut self) -> Result<(), HostError> {
        if self.minor < 1 {
            return Err(HostError::Unsupported {
                subject: "rendered PTY",
            });
        }
        match self
            .diagnosed_extension_request(crate::ExtensionRequest::Capabilities)
            .await?
        {
            crate::ExtensionResponse::Capabilities { features, .. }
                if features & crate::RENDERED_PTY_FEATURE != 0 =>
            {
                Ok(())
            }
            crate::ExtensionResponse::Capabilities { .. } => Err(HostError::Unsupported {
                subject: "rendered PTY",
            }),
            _ => Err(HostError::Protocol("capabilities response required".into())),
        }
    }

    pub async fn capture_rendered(
        &mut self,
        stream: crate::StreamId,
    ) -> Result<crate::RenderedScreen, HostError> {
        self.require_rendered_pty().await?;
        match self
            .diagnosed_extension_request(crate::ExtensionRequest::CaptureRendered { stream })
            .await?
        {
            crate::ExtensionResponse::RenderedCapture { screen } => Ok(screen),
            other => Err(HostError::Protocol(format!(
                "capture_rendered refused: {other:?}"
            ))),
        }
    }

    pub async fn resize_session(
        &mut self,
        stream: crate::StreamId,
        lease: crate::ControllerLease,
        cols: u16,
        rows: u16,
    ) -> Result<u64, HostError> {
        self.require_rendered_pty().await?;
        match self
            .diagnosed_extension_request(crate::ExtensionRequest::Resize {
                stream,
                lease,
                cols,
                rows,
            })
            .await?
        {
            crate::ExtensionResponse::Resized {
                projection_revision,
                ..
            } => Ok(projection_revision),
            other => Err(HostError::Protocol(format!(
                "resize_session refused: {other:?}"
            ))),
        }
    }

    pub async fn capture_output(
        &mut self,
        session: crate::SessionId,
        max_bytes: u32,
    ) -> Result<String, HostError> {
        match self
            .diagnosed_extension_request(crate::ExtensionRequest::CaptureOutput {
                session,
                max_bytes,
            })
            .await?
        {
            crate::ExtensionResponse::CapturedOutput { text } => Ok(text),
            other => Err(HostError::Protocol(format!(
                "capture_output refused: {other:?}"
            ))),
        }
    }

    #[cfg(unix)]
    pub fn into_stream(self) -> tokio::net::UnixStream {
        match self.io {
            ClientIo::Unix(stream) => stream,
            #[cfg(test)]
            ClientIo::Mem(_) => panic!("memory client has no unix stream"),
        }
    }

    /// Opens this independently authenticated connection as an explicit extension
    /// stream. A ticket is not an implicit upgrade of a legacy event connection.
    pub async fn open_extension_stream(
        mut self,
        ticket: crate::Ticket,
    ) -> Result<ExtensionEventStream, HostError> {
        crate::extension::require_minor(self.minor)?;
        let id = self
            .correlator
            .last
            .checked_add(1)
            .ok_or_else(|| HostError::Protocol("request id exhausted".into()))?;
        let response = self
            .request(HostRequest::Extended {
                id: crate::RequestId(id),
                request: crate::ExtensionRequest::OpenStream { ticket },
            })
            .await?;
        if !matches!(
            response,
            HostResponse::Extended {
                response: crate::ExtensionResponse::Opened,
                ..
            }
        ) {
            return Err(HostError::Protocol("extension stream did not open".into()));
        }
        self.mode = SessionMode::Stream;
        Ok(ExtensionEventStream { client: self })
    }

    #[cfg(windows)]
    pub fn into_pipe(self) -> tokio::net::windows::named_pipe::NamedPipeClient {
        match self.io {
            ClientIo::Pipe(pipe) => pipe,
            #[cfg(test)]
            ClientIo::Mem(_) => panic!("memory client has no pipe"),
        }
    }

    #[cfg(windows)]
    pub(crate) fn from_pipe(
        pipe: tokio::net::windows::named_pipe::NamedPipeClient,
        major: u16,
        minor: u16,
    ) -> Self {
        Self {
            io: ClientIo::Pipe(pipe),
            major,
            minor,
            correlator: ClientCorrelator::new(),
            frame_timeout: FRAME_READ_TIMEOUT,
            write_timeout: WRITE_TIMEOUT,
            mode: SessionMode::Rpc,
            endpoint: None,
        }
    }
}

pub struct BlockingHostClient {
    // `inner` is declared first so it drops while `runtime` is still alive.
    inner: AsyncHostClient,
    runtime: tokio::runtime::Runtime,
}

impl BlockingHostClient {
    pub fn connect(endpoint: &HostEndpoint) -> Result<Self, HostError> {
        Self::connect_with(endpoint, ConnectOptions::default())
    }

    pub fn connect_with(
        endpoint: &HostEndpoint,
        options: ConnectOptions,
    ) -> Result<Self, HostError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|err| HostError::Io(err.to_string()))?;
        let endpoint = endpoint.clone();
        let inner = runtime.block_on(AsyncHostClient::connect_with(&endpoint, options))?;
        Ok(Self { inner, runtime })
    }

    pub fn negotiated(&self) -> (u16, u16) {
        self.inner.negotiated()
    }

    pub fn request(&mut self, request: HostRequest) -> Result<HostResponse, HostError> {
        self.runtime.block_on(self.inner.request(request))
    }

    pub fn write_request(&mut self, request: &HostRequest) -> Result<(), HostError> {
        self.runtime.block_on(self.inner.write_request(request))
    }

    pub fn read_response(&mut self) -> Result<HostResponse, HostError> {
        self.runtime.block_on(self.inner.read_response())
    }

    pub fn extension_request(
        &mut self,
        request: crate::ExtensionRequest,
    ) -> Result<crate::ExtensionResponse, HostError> {
        self.runtime.block_on(self.inner.extension_request(request))
    }

    /// None means the daemon does not support build identity telemetry.
    pub fn host_status(&mut self) -> Result<Option<crate::HostBuildInfo>, HostError> {
        self.runtime.block_on(self.inner.host_status())
    }

    pub fn pty_retention_status(&mut self) -> Result<Option<crate::PtyRetentionStatus>, HostError> {
        self.runtime.block_on(self.inner.pty_retention_status())
    }

    pub fn check_session_owner(
        &mut self,
        stream: crate::StreamId,
        hosted_id: &str,
    ) -> Result<bool, HostError> {
        self.runtime
            .block_on(self.inner.check_session_owner(stream, hosted_id))
    }

    pub fn list_sessions(&mut self) -> Result<Vec<crate::SessionSummary>, HostError> {
        self.runtime.block_on(self.inner.list_sessions())
    }

    pub fn spawn_reliable_session(
        &mut self,
        spec: crate::SpawnSpec,
    ) -> Result<(crate::StreamId, crate::ChildState), HostError> {
        self.runtime
            .block_on(self.inner.spawn_reliable_session(spec))
    }

    pub fn spawn_pty_session(
        &mut self,
        spec: crate::SpawnSpec,
        pty: Option<crate::PtyOptions>,
    ) -> Result<(crate::StreamId, crate::ChildState), HostError> {
        self.runtime
            .block_on(self.inner.spawn_pty_session(spec, pty))
    }

    pub fn claim_controller(
        &mut self,
        stream: crate::StreamId,
    ) -> Result<ReliableControllerGrant, HostError> {
        self.runtime.block_on(self.inner.claim_controller(stream))
    }

    pub fn send_line(
        &mut self,
        stream: crate::StreamId,
        lease: crate::ControllerLease,
        line: String,
    ) -> Result<crate::SendOutcome, HostError> {
        self.runtime
            .block_on(self.inner.send_line(stream, lease, line))
    }

    pub fn write_raw(
        &mut self,
        stream: crate::StreamId,
        lease: crate::ControllerLease,
        bytes: Vec<u8>,
    ) -> Result<crate::SendOutcome, HostError> {
        self.runtime
            .block_on(self.inner.write_raw(stream, lease, bytes))
    }

    pub fn signal_session(
        &mut self,
        stream: crate::StreamId,
        lease: crate::ControllerLease,
        signal: crate::SignalKind,
    ) -> Result<(), HostError> {
        self.runtime
            .block_on(self.inner.signal_session(stream, lease, signal))
    }

    pub fn kill_session(
        &mut self,
        stream: crate::StreamId,
        lease: crate::ControllerLease,
    ) -> Result<(), HostError> {
        self.runtime
            .block_on(self.inner.kill_session(stream, lease))
    }

    pub fn capture_rendered(
        &mut self,
        stream: crate::StreamId,
    ) -> Result<crate::RenderedScreen, HostError> {
        self.runtime.block_on(self.inner.capture_rendered(stream))
    }

    pub fn resize_session(
        &mut self,
        stream: crate::StreamId,
        lease: crate::ControllerLease,
        cols: u16,
        rows: u16,
    ) -> Result<u64, HostError> {
        self.runtime
            .block_on(self.inner.resize_session(stream, lease, cols, rows))
    }

    pub fn capture_output(
        &mut self,
        session: crate::SessionId,
        max_bytes: u32,
    ) -> Result<String, HostError> {
        self.runtime
            .block_on(self.inner.capture_output(session, max_bytes))
    }
}

pub struct ExtensionEventStream {
    client: AsyncHostClient,
}
impl ExtensionEventStream {
    pub async fn next_frame(&mut self) -> Result<crate::ExtensionFrame, HostError> {
        let frame = read_idle_client(&mut self.client.io, self.client.frame_timeout).await?;
        crate::decode_extension_frame(self.client.minor, &frame)
    }
}

pub struct HostEventStream {
    client: AsyncHostClient,
    lines: LineReassembler,
}

impl HostEventStream {
    pub async fn next(&mut self) -> Result<HostEvent, HostError> {
        loop {
            let frame = match read_idle_client(&mut self.client.io, self.client.frame_timeout).await
            {
                Ok(frame) => frame,
                Err(err) => {
                    self.lines.discard_chain();
                    return Err(err);
                }
            };
            if frame.kind == crate::FrameKind::Line {
                match self.lines.push_frame_for_minor(self.client.minor, &frame) {
                    Ok(Some(event)) => return Ok(event),
                    Ok(None) => continue,
                    Err(err) => {
                        self.lines.discard_chain();
                        return Err(err);
                    }
                }
            }
            if self.lines.has_open_chain() {
                self.lines.discard_chain();
                return Err(HostError::Protocol(
                    "control frame interleaved with a line chain".into(),
                ));
            }
            if frame.kind == crate::FrameKind::Event {
                let event = serde_json::from_slice(&frame.payload)
                    .map_err(|err| HostError::Protocol(err.to_string()))?;
                return Ok(HostEvent::Control(event));
            }
            return Err(HostError::Protocol(
                "unexpected frame on an event stream".into(),
            ));
        }
    }
}

pub async fn events(
    endpoint: &HostEndpoint,
    filter: Option<SessionId>,
) -> Result<HostEventStream, HostError> {
    let mut client = AsyncHostClient::connect(endpoint).await?;
    let response = client
        .request(HostRequest::Events {
            id: crate::RequestId(1),
            session: filter,
        })
        .await?;
    if !matches!(response, HostResponse::Subscribed { .. }) {
        return Err(HostError::Protocol("events did not subscribe".into()));
    }
    client.mode = SessionMode::Stream;
    Ok(HostEventStream {
        client,
        lines: LineReassembler::new(),
    })
}

#[cfg(unix)]
async fn connect_retry(
    endpoint: &HostEndpoint,
    options: &ConnectOptions,
) -> Result<AsyncHostClient, HostError> {
    let canonical = canonical_unix(endpoint)?;
    let first = crate::transport::unix::read_token(&canonical)?;
    match connect_unix(&canonical, &first, options).await {
        Err(HostError::Auth) => {
            let fresh = crate::transport::unix::read_token(&canonical)?;
            if constant_time_eq(fresh.as_bytes(), first.as_bytes()) {
                Err(HostError::Auth)
            } else {
                connect_unix(&canonical, &fresh, options).await
            }
        }
        other => other,
    }
}

#[cfg(unix)]
async fn connect_unix(
    endpoint: &HostEndpoint,
    token: &AuthToken,
    options: &ConnectOptions,
) -> Result<AsyncHostClient, HostError> {
    if let Some(hook) = &options.pre_connect {
        hook();
    }
    // Recheck immediately before each connect, including auth retries and test hooks.
    #[cfg(feature = "hub-test-isolation")]
    validate_test_endpoint(endpoint)?;
    let stream = tokio::net::UnixStream::connect(endpoint.socket_path())
        .await
        .map_err(|err| HostError::Io(err.to_string()))?;
    drive_unix(endpoint.clone(), *token, stream, options).await
}

#[cfg(unix)]
async fn drive_unix(
    endpoint: HostEndpoint,
    token: AuthToken,
    stream: tokio::net::UnixStream,
    options: &ConnectOptions,
) -> Result<AsyncHostClient, HostError> {
    use std::os::fd::AsRawFd;
    crate::transport::unix::peer_of(stream.as_raw_fd())?;
    let hook = options.pre_proof.as_ref().map(|hook| hook.as_ref());
    let (stream, major, minor) = auth::drive_client(
        stream,
        &endpoint.identity_bytes(),
        &token,
        options.major,
        options.minor,
        client_deadlines(),
        None,
        hook,
    )
    .await?;
    Ok(AsyncHostClient {
        io: ClientIo::Unix(stream),
        major,
        minor,
        correlator: ClientCorrelator::new(),
        frame_timeout: FRAME_READ_TIMEOUT,
        write_timeout: WRITE_TIMEOUT,
        mode: SessionMode::Rpc,
        endpoint: None,
    })
}

#[cfg(unix)]
fn canonical_unix(endpoint: &HostEndpoint) -> Result<HostEndpoint, HostError> {
    let path = crate::transport::unix::canonical_socket(endpoint.socket_path())?;
    HostEndpoint::unix(path)
}

#[cfg(unix)]
fn client_deadlines() -> Deadlines {
    Deadlines {
        handshake: HANDSHAKE_TIMEOUT,
        frame: FRAME_READ_TIMEOUT,
        write: WRITE_TIMEOUT,
    }
}

async fn write_client(
    io: &mut ClientIo,
    frame: &crate::Frame,
    deadline: Instant,
) -> Result<(), HostError> {
    match io {
        #[cfg(unix)]
        ClientIo::Unix(stream) => auth::write_frame_async(stream, frame, deadline).await,
        #[cfg(windows)]
        ClientIo::Pipe(stream) => auth::write_frame_async(stream, frame, deadline).await,
        #[cfg(test)]
        ClientIo::Mem(stream) => auth::write_frame_async(stream, frame, deadline).await,
    }
}

async fn read_response_io(
    io: &mut ClientIo,
    timeout: Duration,
    minor: u16,
) -> Result<HostResponse, HostError> {
    let frame = read_idle_client(io, timeout).await?;
    decode_response_for_minor(minor, &frame)
}

async fn read_idle_client(io: &mut ClientIo, timeout: Duration) -> Result<crate::Frame, HostError> {
    match io {
        #[cfg(unix)]
        ClientIo::Unix(stream) => auth::read_frame_idle(stream, timeout).await,
        #[cfg(windows)]
        ClientIo::Pipe(stream) => auth::read_frame_idle(stream, timeout).await,
        #[cfg(test)]
        ClientIo::Mem(stream) => auth::read_frame_idle(stream, timeout).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{Frame, FrameKind, line_fragment, write_frame};
    use crate::types::{ChildExit, ControlEvent, HostEvent, SessionEpoch, SessionId};
    use tokio::io::AsyncWriteExt;

    fn write_one(bytes: &mut Vec<u8>, frame: &Frame) {
        write_frame(bytes, frame).unwrap();
    }

    async fn send(stream: &mut tokio::io::DuplexStream, frame: &Frame) {
        let mut bytes = Vec::new();
        write_one(&mut bytes, frame);
        stream.write_all(&bytes).await.unwrap();
    }

    fn memory_stream() -> (HostEventStream, tokio::io::DuplexStream) {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let events = HostEventStream {
            client: AsyncHostClient {
                io: ClientIo::Mem(client),
                major: 1,
                minor: 0,
                correlator: ClientCorrelator::new(),
                frame_timeout: Duration::from_secs(2),
                write_timeout: Duration::from_secs(2),
                mode: SessionMode::Stream,
                endpoint: None,
            },
            lines: LineReassembler::new(),
        };
        (events, server)
    }

    fn memory_rpc() -> (AsyncHostClient, tokio::io::DuplexStream) {
        let (mut events, server) = memory_stream();
        events.client.mode = SessionMode::Rpc;
        (events.client, server)
    }

    // Catches sending new operations to an old daemon, including minor-zero peers.
    #[tokio::test]
    async fn old_daemon_receives_no_rendered_or_resize_request_without_capability() {
        for minor in [0, 1] {
            let (mut client, mut server) = memory_rpc();
            client.minor = minor;
            let stream = crate::StreamId {
                host: crate::HostInstance::new([0; 16]),
                session: crate::SessionId::new("old").unwrap(),
                epoch: crate::SessionEpoch(1),
            };
            let caller = async {
                assert!(matches!(
                    client.capture_rendered(stream.clone()).await,
                    Err(HostError::Unsupported { .. })
                ));
                assert!(matches!(
                    client
                        .resize_session(stream, crate::ControllerLease(1), 80, 24)
                        .await,
                    Err(HostError::Unsupported { .. })
                ));
                // A following legacy request is a deterministic sentinel. Unknown
                // requests cannot be silently left unread in the mock socket.
                let id = if minor == 0 { 1 } else { 3 };
                assert!(matches!(
                    client.request(list(id)).await.unwrap(),
                    HostResponse::Listed { .. }
                ));
            };
            let daemon = async {
                for index in 0..if minor == 0 { 1 } else { 3 } {
                    let frame = auth::read_frame_async(
                        &mut server,
                        Instant::now() + Duration::from_secs(2),
                        false,
                    )
                    .await
                    .unwrap();
                    let request = crate::decode_request_for_minor(minor, &frame).unwrap();
                    let response = if minor == 1 && index < 2 {
                        let HostRequest::Extended {
                            id,
                            request: crate::ExtensionRequest::Capabilities,
                        } = request
                        else {
                            panic!("unknown request reached old daemon: {request:?}")
                        };
                        HostResponse::Extended {
                            id,
                            response: crate::ExtensionResponse::Capabilities {
                                host: crate::HostInstance::new([0; 16]),
                                features: crate::RELIABLE_FEATURE | crate::PTY_FEATURE,
                                limits: crate::ExtensionLimits::default(),
                            },
                        }
                    } else {
                        let HostRequest::List { id } = request else {
                            panic!("unknown request reached old daemon: {request:?}")
                        };
                        HostResponse::Listed {
                            id,
                            children: vec![],
                        }
                    };
                    let frame = crate::encode_response_for_minor(minor, &response).unwrap();
                    auth::write_frame_async(
                        &mut server,
                        &frame,
                        Instant::now() + Duration::from_secs(2),
                    )
                    .await
                    .unwrap();
                }
            };
            tokio::join!(caller, daemon);
        }
    }

    #[tokio::test]
    async fn pty_retention_query_is_capability_gated_and_capacity_errors_preserve_uncertainty() {
        for minor in [0, 1] {
            for supported in [false, true] {
                let (mut client, mut server) = memory_rpc();
                client.minor = minor;
                let caller = async {
                    let status = client.pty_retention_status().await.unwrap();
                    assert_eq!(status.is_some(), minor > 0 && supported);
                    let id = client.correlator.last + 1;
                    client.request(list(id)).await.unwrap();
                };
                let daemon = async {
                    loop {
                        let frame = auth::read_frame_async(
                            &mut server,
                            Instant::now() + Duration::from_secs(2),
                            false,
                        )
                        .await
                        .unwrap();
                        let request = crate::decode_request_for_minor(minor, &frame).unwrap();
                        let done = matches!(request, HostRequest::List { .. });
                        let response = match request {
                            HostRequest::Extended {
                                id,
                                request: crate::ExtensionRequest::Capabilities,
                            } if minor > 0 => HostResponse::Extended {
                                id,
                                response: crate::ExtensionResponse::Capabilities {
                                    host: crate::HostInstance::new([0; 16]),
                                    features: if supported {
                                        crate::PTY_RETENTION_FEATURE
                                    } else {
                                        0
                                    },
                                    limits: Default::default(),
                                },
                            },
                            HostRequest::Extended {
                                id,
                                request: crate::ExtensionRequest::PtyRetentionStatus,
                            } if minor > 0 && supported => HostResponse::Extended {
                                id,
                                response: crate::ExtensionResponse::PtyRetentionStatus {
                                    status: crate::PtyRetentionStatus {
                                        projection_reserved_bytes: 12,
                                        projection_limit_bytes: 64,
                                        completed_pty_sessions: 0,
                                        completed_pty_limit: 8,
                                        evicted_total: 0,
                                        recent_evictions: vec![],
                                    },
                                },
                            },
                            HostRequest::List { id } => HostResponse::Listed {
                                id,
                                children: vec![],
                            },
                            other => panic!("unsupported diagnostic sent: {other:?}"),
                        };
                        auth::write_frame_async(
                            &mut server,
                            &crate::encode_response_for_minor(minor, &response).unwrap(),
                            Instant::now() + Duration::from_secs(2),
                        )
                        .await
                        .unwrap();
                        if done {
                            break;
                        }
                    }
                };
                tokio::join!(caller, daemon);
            }
        }
        let (mut client, mut server) = memory_rpc();
        client.minor = 1;
        let caller = async {
            let stream = crate::StreamId {
                host: crate::HostInstance::new([0; 16]),
                session: crate::SessionId::new("capacity").unwrap(),
                epoch: crate::SessionEpoch(1),
            };
            let error = client
                .diagnosed_extension_request(crate::ExtensionRequest::Resize {
                    stream,
                    lease: crate::ControllerLease(1),
                    cols: 80,
                    rows: 24,
                })
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains("one possible cause"), "{error}");
            assert!(error.contains("12 / 64 bytes"), "{error}");
            assert!(
                error.contains("fl host status")
                    && error.contains("scrollback")
                    && error.contains("stop unused live")
            );
        };
        let daemon = async {
            for step in 0..3 {
                let frame = auth::read_frame_async(
                    &mut server,
                    Instant::now() + Duration::from_secs(2),
                    false,
                )
                .await
                .unwrap();
                let HostRequest::Extended { id, request } =
                    crate::decode_request_for_minor(1, &frame).unwrap()
                else {
                    panic!("extended required")
                };
                let response = match (step, request) {
                    (0, crate::ExtensionRequest::Resize { .. }) => {
                        crate::ExtensionResponse::Refused {
                            code: crate::ExtensionRefusal::ResourceLimit,
                        }
                    }
                    (1, crate::ExtensionRequest::Capabilities) => {
                        crate::ExtensionResponse::Capabilities {
                            host: crate::HostInstance::new([0; 16]),
                            features: crate::PTY_RETENTION_FEATURE,
                            limits: Default::default(),
                        }
                    }
                    (2, crate::ExtensionRequest::PtyRetentionStatus) => {
                        crate::ExtensionResponse::PtyRetentionStatus {
                            status: crate::PtyRetentionStatus {
                                projection_reserved_bytes: 12,
                                projection_limit_bytes: 64,
                                completed_pty_sessions: 0,
                                completed_pty_limit: 8,
                                evicted_total: 0,
                                recent_evictions: vec![],
                            },
                        }
                    }
                    other => panic!("mutation was retried or diagnostics recursed: {other:?}"),
                };
                auth::write_frame_async(
                    &mut server,
                    &crate::encode_response_for_minor(1, &HostResponse::Extended { id, response })
                        .unwrap(),
                    Instant::now() + Duration::from_secs(2),
                )
                .await
                .unwrap();
            }
        };
        tokio::join!(caller, daemon);
    }

    fn list(id: u64) -> HostRequest {
        HostRequest::List {
            id: crate::RequestId(id),
        }
    }
    fn listed(id: u64) -> Frame {
        crate::frame::encode_response(&HostResponse::Listed {
            id: crate::RequestId(id),
            children: vec![],
        })
        .unwrap()
    }

    async fn split_regression(client: &mut AsyncHostClient, server: &mut tokio::io::DuplexStream) {
        assert!(
            client.write_request(&list(0)).await.is_err(),
            "zero request id"
        );
        client.write_request(&list(2)).await.unwrap();
        assert!(
            client.write_request(&list(2)).await.is_err(),
            "reused outstanding id"
        );
        assert!(
            client.write_request(&list(1)).await.is_err(),
            "decreasing id"
        );
        client.write_request(&list(3)).await.unwrap();
        assert!(
            client.request(list(4)).await.is_err(),
            "mixed request must not consume a split response"
        );
        send(server, &listed(3)).await;
        send(server, &listed(2)).await;
        assert_eq!(
            client.read_response().await.unwrap().request_id(),
            crate::RequestId(3)
        );
        assert_eq!(
            client.read_response().await.unwrap().request_id(),
            crate::RequestId(2)
        );
        send(server, &listed(2)).await;
        assert!(client.read_response().await.is_err(), "duplicate response");
        send(server, &listed(99)).await;
        assert!(client.read_response().await.is_err(), "unknown response");
        send(server, &listed(4)).await;
        assert_eq!(
            client.request(list(4)).await.unwrap().request_id(),
            crate::RequestId(4)
        );
        assert!(client.write_request(&list(4)).await.is_err(), "mixed reuse");
        client.write_request(&list(5)).await.unwrap();
        send(server, &listed(5)).await;
        client.read_response().await.unwrap();
    }

    #[tokio::test]
    async fn async_split_client_enforces_correlation_and_mixed_use() {
        let (mut client, mut server) = memory_rpc();
        split_regression(&mut client, &mut server).await;
    }

    #[test]
    fn blocking_split_client_enforces_correlation_and_mixed_use() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (inner, mut server) = runtime.block_on(async { memory_rpc() });
        let mut client = BlockingHostClient { inner, runtime };
        assert!(client.write_request(&list(0)).is_err());
        client.write_request(&list(2)).unwrap();
        assert!(client.write_request(&list(2)).is_err());
        assert!(client.write_request(&list(1)).is_err());
        client.write_request(&list(3)).unwrap();
        assert!(client.request(list(4)).is_err());
        client.runtime.block_on(async {
            send(&mut server, &listed(3)).await;
            send(&mut server, &listed(2)).await;
        });
        assert_eq!(
            client.read_response().unwrap().request_id(),
            crate::RequestId(3)
        );
        assert_eq!(
            client.read_response().unwrap().request_id(),
            crate::RequestId(2)
        );
        client.runtime.block_on(send(&mut server, &listed(2)));
        assert!(client.read_response().is_err());
        client.runtime.block_on(send(&mut server, &listed(99)));
        assert!(client.read_response().is_err());
        client.runtime.block_on(send(&mut server, &listed(4)));
        assert_eq!(
            client.request(list(4)).unwrap().request_id(),
            crate::RequestId(4)
        );
        assert!(client.write_request(&list(4)).is_err());
    }

    #[tokio::test]
    async fn local_encode_failure_does_not_consume_id_but_io_failure_does() {
        let (mut client, server) = memory_rpc();
        let oversized = HostRequest::Send {
            id: crate::RequestId(1),
            session: SessionId::new("s").unwrap(),
            expected_epoch: SessionEpoch(1),
            lease: crate::ControllerLease(1),
            line: "x".repeat(crate::MAX_FRAME_PAYLOAD),
        };
        assert!(client.write_request(&oversized).await.is_err());
        client.write_request(&list(1)).await.unwrap();
        drop(server);
        assert!(client.write_request(&list(2)).await.is_err());
        assert!(matches!(
            client.write_request(&list(2)).await,
            Err(HostError::Protocol(_))
        ));
    }

    #[tokio::test]
    async fn event_disconnect_discards_unfinished_line_storage() {
        let (mut events, mut server) = memory_stream();
        let open = line_fragment(
            &SessionId::new("s").unwrap(),
            SessionEpoch(1),
            1,
            true,
            b"pending",
        )
        .unwrap();
        send(&mut server, &open).await;
        drop(server);
        assert!(events.next().await.is_err());
        assert_eq!(
            events.lines.allocated(),
            0,
            "disconnect retained unfinished line storage"
        );
        assert!(!events.lines.has_open_chain());
    }

    fn control_frame() -> Frame {
        let payload = serde_json::to_vec(&ControlEvent::Exited {
            session: SessionId::new("s").unwrap(),
            session_epoch: SessionEpoch(1),
            seq: 4,
            exit: ChildExit {
                code: Some(0),
                signal: None,
            },
        })
        .unwrap();
        Frame {
            kind: FrameKind::Event,
            payload,
        }
    }

    #[tokio::test]
    async fn control_frame_inside_an_open_line_chain_is_refused() {
        let (mut events, mut server) = memory_stream();
        let open = line_fragment(
            &SessionId::new("s").unwrap(),
            SessionEpoch(1),
            1,
            true,
            b"ab",
        )
        .unwrap();
        send(&mut server, &open).await;
        send(&mut server, &control_frame()).await;
        let err = events.next().await.unwrap_err();
        assert!(
            matches!(err, HostError::Protocol(ref text) if text.contains("interleaved")),
            "{err:?}"
        );
        assert!(!events.lines.has_open_chain());
        let done = line_fragment(
            &SessionId::new("s").unwrap(),
            SessionEpoch(1),
            2,
            false,
            b"ok",
        )
        .unwrap();
        send(&mut server, &done).await;
        match events.next().await.unwrap() {
            HostEvent::Line { seq, bytes, .. } => {
                assert_eq!(seq, 2);
                assert_eq!(bytes, b"ok");
            }
            other => panic!("discarded chain must not swallow the next line: {other:?}"),
        }

        let (mut events, mut server) = memory_stream();
        send(&mut server, &control_frame()).await;
        assert!(matches!(
            events.next().await.unwrap(),
            HostEvent::Control(_)
        ));
    }
}

#[cfg(test)]
mod extension_client_tests {
    use super::*;
    #[tokio::test]
    async fn negotiated_zero_client_rejects_raw_extended_response() {
        let (left, mut right) = tokio::io::duplex(64 * 1024);
        let mut client = AsyncHostClient {
            io: ClientIo::Mem(left),
            major: 1,
            minor: 0,
            correlator: ClientCorrelator::new(),
            frame_timeout: Duration::from_secs(1),
            write_timeout: Duration::from_secs(1),
            mode: SessionMode::Rpc,
            endpoint: None,
        };
        client
            .write_request(&HostRequest::List {
                id: crate::RequestId(1),
            })
            .await
            .unwrap();
        let forged = crate::Frame {
            kind: crate::FrameKind::Response,
            payload: br#"{"op":"extended","id":1,"response":{"op":"opened"}}"#.to_vec(),
        };
        auth::write_frame_async(&mut right, &forged, Instant::now() + Duration::from_secs(1))
            .await
            .unwrap();
        assert!(client.read_response().await.is_err());
    }

    #[tokio::test]
    async fn diagnosed_extension_request_returns_typed_evicted_and_stale_epoch_errors() {
        let host = crate::HostInstance::new([0x11; 16]);
        let evicted_stream = crate::StreamId {
            host: host.clone(),
            session: SessionId::new("evicted-pty").unwrap(),
            epoch: crate::SessionEpoch(7),
        };
        let stale_stream = crate::StreamId {
            host,
            session: SessionId::new("stale-pty").unwrap(),
            epoch: crate::SessionEpoch(3),
        };
        for (target, expected_evicted) in [(evicted_stream.clone(), true), (stale_stream, false)] {
            let (left, mut right) = tokio::io::duplex(64 * 1024);
            let mut client = AsyncHostClient {
                io: ClientIo::Mem(left),
                major: 1,
                minor: 1,
                correlator: ClientCorrelator::new(),
                frame_timeout: Duration::from_secs(1),
                write_timeout: Duration::from_secs(1),
                mode: SessionMode::Rpc,
                endpoint: None,
            };
            let evicted_for_server = evicted_stream.clone();
            let server = tokio::spawn(async move {
                let deadline = Instant::now() + Duration::from_secs(1);
                let frame = auth::read_frame_async(&mut right, deadline, false)
                    .await
                    .unwrap();
                let HostRequest::Extended { id, .. } =
                    crate::decode_request_for_minor(1, &frame).unwrap()
                else {
                    panic!("expected extended request");
                };
                let refused = crate::encode_response_for_minor(
                    1,
                    &HostResponse::Extended {
                        id,
                        response: crate::ExtensionResponse::Refused {
                            code: crate::ExtensionRefusal::StaleEpoch,
                        },
                    },
                )
                .unwrap();
                auth::write_frame_async(&mut right, &refused, deadline)
                    .await
                    .unwrap();
                let frame = auth::read_frame_async(&mut right, deadline, false)
                    .await
                    .unwrap();
                let HostRequest::Extended {
                    id,
                    request: crate::ExtensionRequest::Capabilities,
                } = crate::decode_request_for_minor(1, &frame).unwrap()
                else {
                    panic!("expected capabilities query");
                };
                let caps = crate::encode_response_for_minor(
                    1,
                    &HostResponse::Extended {
                        id,
                        response: crate::ExtensionResponse::Capabilities {
                            host: evicted_for_server.host.clone(),
                            features: crate::PTY_RETENTION_FEATURE,
                            limits: crate::ExtensionLimits::default(),
                        },
                    },
                )
                .unwrap();
                auth::write_frame_async(&mut right, &caps, deadline)
                    .await
                    .unwrap();
                let frame = auth::read_frame_async(&mut right, deadline, false)
                    .await
                    .unwrap();
                let HostRequest::Extended {
                    id,
                    request: crate::ExtensionRequest::PtyRetentionStatus,
                } = crate::decode_request_for_minor(1, &frame).unwrap()
                else {
                    panic!("expected pty_retention_status query");
                };
                let status = crate::encode_response_for_minor(
                    1,
                    &HostResponse::Extended {
                        id,
                        response: crate::ExtensionResponse::PtyRetentionStatus {
                            status: crate::PtyRetentionStatus {
                                projection_reserved_bytes: 0,
                                projection_limit_bytes: 1024,
                                completed_pty_sessions: 0,
                                completed_pty_limit: 8,
                                evicted_total: 1,
                                recent_evictions: vec![evicted_for_server],
                            },
                        },
                    },
                )
                .unwrap();
                auth::write_frame_async(&mut right, &status, deadline)
                    .await
                    .unwrap();
            });
            let err = client
                .claim_controller(target.clone())
                .await
                .expect_err("must fail with typed epoch error");
            server.await.unwrap();
            if expected_evicted {
                assert_eq!(err, HostError::PtyEpochEvicted { stream: target });
            } else {
                assert_eq!(err, HostError::StaleEpoch { stream: target });
            }
        }
    }
}

/// A controller grant does not imply that any output was durably applied.
#[derive(Debug)]
pub struct ReliableControllerGrant {
    pub lease: crate::ControllerLease,
    pub acked: u64,
    pub latest: u64,
}
/// Complete reliable events; reading/displaying never sends Ack.
pub struct ReliableEventStream {
    inner: ExtensionEventStream,
    reassembly: crate::ReliableReassembler,
    cursor: crate::Cursor,
}
impl ReliableEventStream {
    pub fn cursor(&self) -> crate::Cursor {
        self.cursor.clone()
    }
    pub async fn next_event(&mut self) -> Result<crate::ReliableEvent, HostError> {
        loop {
            let frame = match self.inner.next_frame().await {
                Ok(frame) => frame,
                Err(error) => {
                    self.reassembly.discard();
                    return Err(error);
                }
            };
            match self.reassembly.push(&frame) {
                Ok(Some(event)) => {
                    if event.kind != crate::ReliableKind::Gap {
                        self.cursor.seq = event.seq;
                    }
                    return Ok(event);
                }
                Ok(None) => {}
                Err(error) => {
                    self.reassembly.discard();
                    return Err(error);
                }
            }
        }
    }
}
