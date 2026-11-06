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
use crate::frame::{LineReassembler, ResponseCorrelator, decode_response, encode_request};
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

pub struct AsyncHostClient {
    io: ClientIo,
    major: u16,
    minor: u16,
    correlator: ResponseCorrelator,
    frame_timeout: Duration,
    write_timeout: Duration,
    mode: SessionMode,
}

impl std::fmt::Debug for AsyncHostClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AsyncHostClient")
            .field("major", &self.major)
            .field("minor", &self.minor)
            .finish()
    }
}

impl AsyncHostClient {
    pub async fn connect(endpoint: &HostEndpoint) -> Result<Self, HostError> {
        Self::connect_with(endpoint, ConnectOptions::default()).await
    }

    pub async fn connect_with(
        endpoint: &HostEndpoint,
        options: ConnectOptions,
    ) -> Result<Self, HostError> {
        #[cfg(unix)]
        {
            return connect_retry(endpoint, &options).await;
        }
        #[cfg(windows)]
        {
            return crate::transport::windows::connect_client(endpoint, &options).await;
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
        drive_unix(canonical, token, stream, &ConnectOptions::default()).await
    }

    pub async fn request(&mut self, request: HostRequest) -> Result<HostResponse, HostError> {
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
        let id = request.request_id().0;
        self.correlator.submit(id)?;
        let frame = encode_request(&request)?;
        let deadline = Instant::now() + self.write_timeout;
        write_client(&mut self.io, &frame, deadline).await?;
        let response = read_response_io(&mut self.io, self.frame_timeout).await?;
        if response.request_id().0 != id {
            return Err(HostError::Protocol("response id does not match".into()));
        }
        self.correlator.complete(id)?;
        Ok(response)
    }

    /// Writes one request and does not wait. The single-id correlator is unchanged.
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
        let frame = encode_request(request)?;
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
        read_response_io(&mut self.io, self.frame_timeout).await
    }

    pub fn negotiated(&self) -> (u16, u16) {
        (self.major, self.minor)
    }

    pub fn allows(&self, name: &str) -> bool {
        crate::supports_message(self.minor, name)
    }

    #[cfg(unix)]
    pub fn into_stream(self) -> tokio::net::UnixStream {
        match self.io {
            ClientIo::Unix(stream) => stream,
            #[cfg(test)]
            ClientIo::Mem(_) => panic!("memory client has no unix stream"),
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
            correlator: ResponseCorrelator::new(),
            frame_timeout: FRAME_READ_TIMEOUT,
            write_timeout: WRITE_TIMEOUT,
            mode: SessionMode::Rpc,
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
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|err| HostError::Io(err.to_string()))?;
        let endpoint = endpoint.clone();
        let inner = runtime.block_on(AsyncHostClient::connect(&endpoint))?;
        Ok(Self { inner, runtime })
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
}

pub struct HostEventStream {
    client: AsyncHostClient,
    lines: LineReassembler,
}

impl HostEventStream {
    pub async fn next(&mut self) -> Result<HostEvent, HostError> {
        loop {
            let frame = read_idle_client(&mut self.client.io, self.client.frame_timeout).await?;
            if frame.kind == crate::FrameKind::Line {
                match self.lines.push_frame(&frame) {
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
        correlator: ResponseCorrelator::new(),
        frame_timeout: FRAME_READ_TIMEOUT,
        write_timeout: WRITE_TIMEOUT,
        mode: SessionMode::Rpc,
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

async fn read_response_io(io: &mut ClientIo, timeout: Duration) -> Result<HostResponse, HostError> {
    let frame = read_idle_client(io, timeout).await?;
    decode_response(&frame)
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
                correlator: ResponseCorrelator::new(),
                frame_timeout: Duration::from_secs(2),
                write_timeout: Duration::from_secs(2),
                mode: SessionMode::Stream,
            },
            lines: LineReassembler::new(),
        };
        (events, server)
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
