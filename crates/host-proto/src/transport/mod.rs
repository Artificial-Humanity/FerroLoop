//! Authenticated listener. The accept loop admits a connection only after
//! peer checks and the mutual proof. Later tasks reuse this handshake.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::sync::{mpsc, watch};

use crate::HostConfig;

pub(crate) type TokenPublisher = Arc<dyn Fn(&[u8; 32]) -> Result<(), HostError> + Send + Sync>;
use crate::auth::{self, Deadlines};
use crate::endpoint::HostEndpoint;
use crate::frame::{decode_request, encode_response};
use crate::types::{AuthToken, HostError, HostRequest, HostResponse};

#[cfg(unix)]
pub(crate) mod unix;
#[cfg(any(target_os = "linux", target_os = "android"))]
pub use unix::interpret_ucred;

pub mod windows;

struct Shared {
    token: Arc<RwLock<AuthToken>>,
    attempts: AtomicUsize,
    requests: Arc<AtomicUsize>,
    garbage: AtomicBool,
    slots: AtomicUsize,
    frame_timeout: Duration,
    write_timeout: Duration,
    handshake_timeout: Duration,
    max_unauth: usize,
    daemon_version: String,
    major: u16,
    minor: u16,
    identity: Vec<u8>,
    failures: std::sync::Mutex<Option<HostError>>,
}

impl Shared {
    fn note_handshake_failure(&self, err: HostError) {
        *self
            .failures
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(err);
    }
}

struct Slot(Arc<Shared>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.slots.fetch_sub(1, Ordering::AcqRel);
    }
}

fn reserve(shared: &Arc<Shared>) -> Option<Slot> {
    loop {
        let current = shared.slots.load(Ordering::Acquire);
        if current >= shared.max_unauth {
            return None;
        }
        if shared
            .slots
            .compare_exchange(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return Some(Slot(Arc::clone(shared)));
        }
    }
}

enum ConnIo {
    #[cfg(unix)]
    Unix(tokio::net::UnixStream),
    #[cfg(windows)]
    Server(tokio::net::windows::named_pipe::NamedPipeServer),
}

pub struct HostConn {
    io: ConnIo,
    frame_timeout: Duration,
    write_timeout: Duration,
    requests: Arc<AtomicUsize>,
    stream_only: bool,
    /// Partial frame bytes. A cancelled `recv` must not discard them.
    pending: Vec<u8>,
    /// Set when the first byte of the current frame arrived. The first byte itself waits.
    frame_started: Option<std::time::Instant>,
}

impl HostConn {
    pub async fn recv(&mut self) -> Result<HostRequest, HostError> {
        if self.stream_only {
            return Err(HostError::Protocol("this connection is stream-only".into()));
        }
        let frame = self.read_buffered_frame().await?;
        if frame.kind != crate::FrameKind::Request {
            return Err(HostError::Protocol(
                "authenticated connection expected a request".into(),
            ));
        }
        let request = decode_request(&frame)?;
        self.requests.fetch_add(1, Ordering::SeqCst);
        Ok(request)
    }

    /// A subscribed event connection accepts no further requests.
    pub fn enter_stream(&mut self) {
        self.stream_only = true;
    }

    /// True when the peer has closed its write half. Does not consume bytes.
    pub fn peer_closed(&self) -> bool {
        peer_half_closed(&self.io)
    }

    pub async fn respond(&mut self, response: &HostResponse) -> Result<(), HostError> {
        let frame = encode_response(response)?;
        let deadline = std::time::Instant::now() + self.write_timeout;
        write_frame(&mut self.io, &frame, deadline).await
    }

    /// One frame with this connection's absolute write deadline.
    /// A timeout may already have written a prefix; the caller closes.
    pub async fn write_frame(&mut self, frame: &crate::Frame) -> Result<(), HostError> {
        let deadline = std::time::Instant::now() + self.write_timeout;
        write_frame(&mut self.io, frame, deadline).await
    }

    /// Resolves when the peer closes. Cancel-safe: it does not consume bytes.
    ///
    /// A stuck read-ready bit must not spin. `select!` polls this future, and a
    /// poll that always returns Ready would starve the request handler.
    pub async fn wait_peer_closed(&mut self) {
        loop {
            if peer_half_closed(&self.io) {
                return;
            }
            match &mut self.io {
                #[cfg(unix)]
                ConnIo::Unix(stream) => {
                    let _ = stream.readable().await;
                    if peer_half_closed(&self.io) {
                        return;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                #[cfg(windows)]
                ConnIo::Server(_stream) => {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            }
        }
    }

    /// Bound a non-reading subscriber so its writer blocks instead of buffering the backlog in the kernel.
    pub fn shrink_send_buffer(&self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let fd = match &self.io {
                ConnIo::Unix(stream) => stream.as_raw_fd(),
            };
            let size: libc::c_int = 2048;
            let _ = unsafe {
                libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    &size as *const libc::c_int as *const libc::c_void,
                    std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                )
            };
        }
    }

    pub fn dup_shutdown(&self) -> Option<Arc<ShutdownSwitch>> {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let fd = match &self.io {
                ConnIo::Unix(stream) => stream.as_raw_fd(),
            };
            let duped = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
            if duped < 0 {
                return None;
            }
            Some(Arc::new(ShutdownSwitch {
                fd: AtomicI32::new(duped),
            }))
        }
        #[cfg(not(unix))]
        {
            Some(Arc::new(ShutdownSwitch {
                fd: AtomicI32::new(-1),
            }))
        }
    }
}

/// Duplicated socket. Tripping it unblocks a writer stuck in `write`.
pub struct ShutdownSwitch {
    fd: AtomicI32,
}

impl ShutdownSwitch {
    pub fn trip(&self) {
        let fd = self.fd.swap(-1, Ordering::SeqCst);
        if fd < 0 {
            return;
        }
        #[cfg(unix)]
        unsafe {
            libc::shutdown(fd, libc::SHUT_RDWR);
            libc::close(fd);
        }
        #[cfg(not(unix))]
        {
            let _ = fd;
        }
    }

    /// Close the duplicate without shutting down the accepted socket.
    pub fn disarm(self) {
        let fd = self.fd.swap(-1, Ordering::SeqCst);
        if fd < 0 {
            return;
        }
        #[cfg(unix)]
        unsafe {
            libc::close(fd);
        }
        #[cfg(not(unix))]
        {
            let _ = fd;
        }
    }
}

impl Drop for ShutdownSwitch {
    fn drop(&mut self) {
        self.trip();
    }
}

fn peer_half_closed(io: &ConnIo) -> bool {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let fd = match io {
            ConnIo::Unix(stream) => stream.as_raw_fd(),
        };
        let mut buf = [0u8; 1];
        let n = unsafe {
            libc::recv(
                fd,
                buf.as_mut_ptr().cast(),
                1,
                libc::MSG_PEEK | libc::MSG_DONTWAIT,
            )
        };
        if n == 0 {
            return true;
        }
        if n < 0 {
            let err = std::io::Error::last_os_error();
            return err.kind() != std::io::ErrorKind::WouldBlock;
        }
        false
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        let ConnIo::Server(pipe) = io;
        // Peek does not consume pending bytes and does not wait for input on
        // our byte-mode pipe. Broken/disconnected handles and query failures
        // all close admission fail-closed, even on an otherwise idle stream.
        unsafe {
            windows_sys::Win32::System::Pipes::PeekNamedPipe(
                pipe.as_raw_handle(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            ) == 0
        }
    }
}

pub struct HostListener {
    endpoint: HostEndpoint,
    incoming: mpsc::Receiver<HostConn>,
    stop: watch::Sender<bool>,
    task: Option<tokio::task::JoinHandle<()>>,
    shared: Arc<Shared>,
    publish: TokenPublisher,
    cleanup: Option<Box<dyn FnOnce() + Send>>,
}

impl std::fmt::Debug for HostListener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HostListener(redacted)")
    }
}

impl HostListener {
    pub async fn bind(config: HostConfig) -> Result<Self, HostError> {
        #[cfg(unix)]
        {
            let setup = unix::bind_setup(&config.endpoint)?;
            Ok(spawn_unix(config, setup))
        }
        #[cfg(windows)]
        {
            windows::bind_listener(config).await
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = config;
            Err(HostError::Protocol(
                "no host transport on this platform".into(),
            ))
        }
    }

    pub fn endpoint(&self) -> &HostEndpoint {
        &self.endpoint
    }

    pub fn handshake_attempts(&self) -> usize {
        self.shared.attempts.load(Ordering::SeqCst)
    }

    pub fn handshake_error(&self) -> Option<HostError> {
        self.shared
            .failures
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    pub fn authenticated_requests(&self) -> usize {
        self.shared.requests.load(Ordering::SeqCst)
    }

    pub fn testing_set_garbage_banner(&self, enabled: bool) {
        self.shared.garbage.store(enabled, Ordering::SeqCst);
    }

    pub fn token_control(&self) -> TokenControl {
        TokenControl {
            publish: Arc::clone(&self.publish),
            token: Arc::clone(&self.shared.token),
        }
    }

    pub async fn accept(&mut self) -> Result<HostConn, HostError> {
        self.incoming
            .recv()
            .await
            .ok_or_else(|| HostError::Io("listener closed".into()))
    }

    pub async fn shutdown(mut self) -> Result<(), HostError> {
        self.stop_and_clean();
        Ok(())
    }

    fn stop_and_clean(&mut self) {
        let _ = self.stop.send(true);
        if let Some(task) = self.task.take() {
            task.abort();
        }
        if let Some(cleanup) = self.cleanup.take() {
            cleanup();
        }
    }
}

impl Drop for HostListener {
    fn drop(&mut self) {
        self.stop_and_clean();
    }
}

#[derive(Clone)]
pub struct TokenControl {
    publish: TokenPublisher,
    token: Arc<RwLock<AuthToken>>,
}

impl TokenControl {
    pub fn rotate(&self) -> Result<(), HostError> {
        let fresh = AuthToken::generate()?;
        (self.publish)(fresh.as_bytes())
    }

    pub fn current_bytes(&self) -> [u8; 32] {
        *self
            .token
            .read()
            .unwrap_or_else(|err| err.into_inner())
            .as_bytes()
    }
}

#[cfg(unix)]
fn spawn_unix(config: HostConfig, setup: unix::UnixSetup) -> HostListener {
    use std::os::fd::AsRawFd;
    let shared = Arc::new(Shared {
        token: Arc::clone(&setup.token),
        attempts: AtomicUsize::new(0),
        requests: Arc::new(AtomicUsize::new(0)),
        garbage: AtomicBool::new(false),
        slots: AtomicUsize::new(0),
        frame_timeout: config.frame_timeout,
        write_timeout: config.write_timeout,
        handshake_timeout: config.handshake_timeout,
        max_unauth: config.max_unauthenticated,
        daemon_version: config.daemon_version,
        major: config.major,
        minor: config.minor,
        identity: setup.identity,
        failures: std::sync::Mutex::new(None),
    });
    let (tx, rx) = mpsc::channel(64);
    let (stop_tx, stop_rx) = watch::channel(false);
    let task_shared = Arc::clone(&shared);
    let mut stop = stop_rx;
    let listener = setup.listener;
    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() {
                        break;
                    }
                }
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else {
                        continue;
                    };
                    if *stop.borrow() {
                        break;
                    }
                    if unix::peer_of(stream.as_raw_fd()).is_err() {
                        continue;
                    }
                    let Some(slot) = reserve(&task_shared) else {
                        drop(stream);
                        continue;
                    };
                    let shared = Arc::clone(&task_shared);
                    let tx = tx.clone();
                    tokio::spawn(async move {
                        let _slot = slot;
                        shared.attempts.fetch_add(1, Ordering::SeqCst);
                        match admit(Arc::clone(&shared), ConnIo::Unix(stream)).await {
                            Ok(conn) => {
                                let _ = tx.send(conn).await;
                            }
                            Err(err) => shared.note_handshake_failure(err),
                        }
                    });
                }
            }
        }
    });
    HostListener {
        endpoint: setup.endpoint,
        incoming: rx,
        stop: stop_tx,
        task: Some(task),
        shared,
        publish: setup.publish,
        cleanup: Some(setup.cleanup),
    }
}

async fn admit(shared: Arc<Shared>, io: ConnIo) -> Result<HostConn, HostError> {
    let deadlines = Deadlines {
        handshake: shared.handshake_timeout,
        frame: shared.frame_timeout,
        write: shared.write_timeout,
    };
    let token = Arc::clone(&shared.token);
    let load = move || *token.read().unwrap_or_else(|err| err.into_inner());
    let io = match io {
        #[cfg(unix)]
        ConnIo::Unix(mut stream) => {
            if shared.garbage.load(Ordering::SeqCst) {
                let deadline = std::time::Instant::now() + shared.handshake_timeout;
                let _ = auth::write_garbage(&mut stream, deadline).await;
                return Err(HostError::Protocol("garbage banner".into()));
            }
            let (stream, _agreed) = auth::server_with_token(
                stream,
                &shared.identity,
                deadlines,
                &shared.daemon_version,
                shared.major,
                shared.minor,
                None,
                load,
            )
            .await?;
            ConnIo::Unix(stream)
        }
        #[cfg(windows)]
        ConnIo::Server(mut stream) => {
            if shared.garbage.load(Ordering::SeqCst) {
                let deadline = std::time::Instant::now() + shared.handshake_timeout;
                let _ = auth::write_garbage(&mut stream, deadline).await;
                return Err(HostError::Protocol("garbage banner".into()));
            }
            let (stream, _agreed) = auth::server_with_token(
                stream,
                &shared.identity,
                deadlines,
                &shared.daemon_version,
                shared.major,
                shared.minor,
                None,
                load,
            )
            .await?;
            ConnIo::Server(stream)
        }
    };
    Ok(HostConn {
        io,
        frame_timeout: shared.frame_timeout,
        write_timeout: shared.write_timeout,
        requests: Arc::clone(&shared.requests),
        stream_only: false,
        pending: Vec::new(),
        frame_started: None,
    })
}

impl HostConn {
    /// Full frames only. Bytes already read stay on `self` if this future is dropped.
    async fn read_buffered_frame(&mut self) -> Result<crate::Frame, HostError> {
        loop {
            if let Some(frame) = take_frame(&mut self.pending)? {
                self.frame_started = if self.pending.is_empty() {
                    None
                } else {
                    Some(std::time::Instant::now())
                };
                return Ok(frame);
            }
            match fill_pending(&self.io, &mut self.pending, &mut self.frame_started)? {
                Fill::Closed => return Err(HostError::Io("connection closed".into())),
                Fill::Bytes => {}
                Fill::Block => self.wait_readable().await?,
            }
        }
    }

    async fn wait_readable(&mut self) -> Result<(), HostError> {
        if let Some(started) = self.frame_started {
            let deadline = started + self.frame_timeout;
            let now = std::time::Instant::now();
            if now >= deadline {
                return Err(HostError::FrameTimeout);
            }
            return match tokio::time::timeout(deadline - now, self.readable()).await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(err)) => Err(HostError::Io(err.to_string())),
                Err(_) => Err(HostError::FrameTimeout),
            };
        }
        self.readable()
            .await
            .map_err(|err| HostError::Io(err.to_string()))
    }

    async fn readable(&self) -> std::io::Result<()> {
        match &self.io {
            #[cfg(unix)]
            ConnIo::Unix(stream) => stream.readable().await,
            #[cfg(windows)]
            ConnIo::Server(stream) => stream.readable().await,
        }
    }
}

enum Fill {
    Closed,
    Bytes,
    Block,
}

fn fill_pending(
    io: &ConnIo,
    pending: &mut Vec<u8>,
    started: &mut Option<std::time::Instant>,
) -> Result<Fill, HostError> {
    let mut buf = [0u8; 8192];
    let read = match io {
        #[cfg(unix)]
        ConnIo::Unix(stream) => stream.try_read(&mut buf),
        #[cfg(windows)]
        ConnIo::Server(stream) => stream.try_read(&mut buf),
    };
    match read {
        Ok(0) => Ok(Fill::Closed),
        Ok(n) => {
            if pending.is_empty() {
                *started = Some(std::time::Instant::now());
            }
            pending.extend_from_slice(&buf[..n]);
            Ok(Fill::Bytes)
        }
        Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {
            fill_pending(io, pending, started)
        }
        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => Ok(Fill::Block),
        Err(err) => Err(HostError::Io(err.to_string())),
    }
}

fn take_frame(pending: &mut Vec<u8>) -> Result<Option<crate::Frame>, HostError> {
    if pending.len() < 4 {
        return Ok(None);
    }
    let mut len_buf = [0u8; 4];
    len_buf.copy_from_slice(&pending[..4]);
    let body_len = crate::frame::checked_frame_len(u32::from_be_bytes(len_buf))?;
    let total = 4usize.checked_add(body_len).ok_or(HostError::FrameLength)?;
    if pending.len() < total {
        return Ok(None);
    }
    let rest = pending.split_off(total);
    let mut framed = std::mem::replace(pending, rest);
    let body = framed.split_off(4);
    crate::frame::decode_body(body).map(Some)
}

async fn write_frame(
    io: &mut ConnIo,
    frame: &crate::Frame,
    deadline: std::time::Instant,
) -> Result<(), HostError> {
    match io {
        #[cfg(unix)]
        ConnIo::Unix(stream) => auth::write_frame_async(stream, frame, deadline).await,
        #[cfg(windows)]
        ConnIo::Server(stream) => auth::write_frame_async(stream, frame, deadline).await,
    }
}

#[cfg(all(test, unix))]
mod descriptor_tests {
    use super::*;
    #[tokio::test]
    async fn invalid_wire_session_never_counts_as_an_admitted_request() {
        let (stream, mut peer) = tokio::net::UnixStream::pair().unwrap();
        let mut conn = HostConn {
            io: ConnIo::Unix(stream),
            frame_timeout: Duration::from_secs(1),
            write_timeout: Duration::from_secs(1),
            requests: Arc::new(AtomicUsize::new(0)),
            stream_only: false,
            pending: Vec::new(),
            frame_started: None,
        };
        let frame = crate::Frame {
            kind: crate::FrameKind::Request,
            payload:
                br#"{"op":"kill","id":1,"session":"bad/name","expected_epoch":1,"lease":null}"#
                    .to_vec(),
        };
        crate::auth::write_frame_async(
            &mut peer,
            &frame,
            std::time::Instant::now() + Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert!(conn.recv().await.is_err());
        assert_eq!(conn.requests.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn shutdown_duplicate_is_close_on_exec() {
        let (stream, _peer) = tokio::net::UnixStream::pair().unwrap();
        let conn = HostConn {
            io: ConnIo::Unix(stream),
            frame_timeout: Duration::from_secs(1),
            write_timeout: Duration::from_secs(1),
            requests: Arc::new(AtomicUsize::new(0)),
            stream_only: false,
            pending: Vec::new(),
            frame_started: None,
        };
        let switch = conn.dup_shutdown().unwrap();
        let flags = unsafe { libc::fcntl(switch.fd.load(Ordering::SeqCst), libc::F_GETFD) };
        assert!(flags >= 0);
        assert_ne!(
            flags & libc::FD_CLOEXEC,
            0,
            "shutdown duplicate leaked across exec"
        );
    }
}
