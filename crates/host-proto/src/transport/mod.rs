//! Authenticated listener. The accept loop admits a connection only after
//! peer checks and the mutual proof. Later tasks reuse this handshake.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::sync::{mpsc, watch};

use crate::HostConfig;

pub(crate) type TokenPublisher = Arc<dyn Fn(&[u8; 32]) -> Result<(), HostError> + Send + Sync>;
use crate::auth::{self, Deadlines};
use crate::endpoint::HostEndpoint;
use crate::frame::{decode_request, encode_response};
use crate::types::{AuthToken, HostError, HostRequest, HostResponse};

pub(crate) mod unix;
#[cfg(unix)]
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
    #[cfg(windows)]
    Client(tokio::net::windows::named_pipe::NamedPipeClient),
}

pub struct HostConn {
    io: ConnIo,
    frame_timeout: Duration,
    write_timeout: Duration,
    requests: Arc<AtomicUsize>,
    stream_only: bool,
}

impl HostConn {
    pub async fn recv(&mut self) -> Result<HostRequest, HostError> {
        if self.stream_only {
            return Err(HostError::Protocol("this connection is stream-only".into()));
        }
        let timeout = self.frame_timeout;
        let frame = read_idle(&mut self.io, timeout).await?;
        if frame.kind != crate::FrameKind::Request {
            return Err(HostError::Protocol(
                "authenticated connection expected a request".into(),
            ));
        }
        let request = decode_request(&frame)?;
        self.requests.fetch_add(1, Ordering::SeqCst);
        if matches!(request, HostRequest::Events { .. }) {
            self.stream_only = true;
        }
        Ok(request)
    }

    pub async fn respond(&mut self, response: &HostResponse) -> Result<(), HostError> {
        let frame = encode_response(response)?;
        let deadline = std::time::Instant::now() + self.write_timeout;
        write_frame(&mut self.io, &frame, deadline).await
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
                        if let Ok(conn) = admit(shared, ConnIo::Unix(stream)).await {
                            let _ = tx.send(conn).await;
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
        #[cfg(windows)]
        ConnIo::Client(_) => {
            return Err(HostError::Protocol("client pipe is not a listener".into()));
        }
    };
    Ok(HostConn {
        io,
        frame_timeout: shared.frame_timeout,
        write_timeout: shared.write_timeout,
        requests: Arc::clone(&shared.requests),
        stream_only: false,
    })
}

async fn read_idle(io: &mut ConnIo, timeout: Duration) -> Result<crate::Frame, HostError> {
    match io {
        #[cfg(unix)]
        ConnIo::Unix(stream) => auth::read_frame_idle(stream, timeout).await,
        #[cfg(windows)]
        ConnIo::Server(stream) => auth::read_frame_idle(stream, timeout).await,
        #[cfg(windows)]
        ConnIo::Client(stream) => auth::read_frame_idle(stream, timeout).await,
    }
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
        #[cfg(windows)]
        ConnIo::Client(stream) => auth::write_frame_async(stream, frame, deadline).await,
    }
}
