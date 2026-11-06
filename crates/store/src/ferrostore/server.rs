//! One `FerroStore` serving many clients over a Unix socket or `ws://`.
//!
//! The server process is the only opener of the `.fls` file. Unix clients
//! speak length-prefixed JSON. WebSocket clients speak one JSON value per
//! frame. Neither takes `flock`. Every connection authenticates before its
//! first request is read, and every request needs a scope (spec §6.2,
//! [`auth`](super::auth)).

use super::auth::{self, Credential, MethodRule, Peer, Principal, Scope, ServerAuth};
use super::bind;
use super::columnar;
use super::layout::{Op, backend};
use super::retry;
use super::session::{self, Claim, Dedup, ReadKey, Sessions};
use super::vector;
use super::{FerroStore, MqDurability, MqRecord, SyncBatch, lock_mutex};
use fl_core::StoreError;
use fl_core::ids::{GateId, ProjectId};
use fl_core::iri::Iri;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::tls::{self, ServerTls, TlsStream, Wire};
use super::ws;

pub(super) const MAX_FRAME_BYTES: u32 = 16 * 1024 * 1024;
/// How long a client waits for a reply before it resends a write once.
pub(super) const CALL_TIMEOUT: Duration = Duration::from_secs(10);
/// Accept is nonblocking. Drop waits at most one pause to observe the flag.
const ACCEPT_PAUSE: Duration = Duration::from_millis(20);
/// Forwarders wake on this interval so a closed connection can join them.
const FORWARD_PAUSE: Duration = Duration::from_millis(50);
/// Spill overdue adaptive queues even when no client is writing.
const MQ_MAINTENANCE_INTERVAL: Duration = Duration::from_secs(5);

/// A connection that is authenticating.
struct Handshake {
    deadline: Instant,
    tcp: bool,
}

struct ServerState {
    store: Arc<FerroStore>,
    shutdown: AtomicBool,
    conns: Mutex<HashMap<u64, Box<dyn JsonIo>>>,
    next_conn: AtomicU64,
    /// Every connection's open read sessions (spec §6.1).
    sessions: Sessions,
    /// Recent request ids and their replies (spec §6.1).
    dedup: Dedup,
    /// How connections authenticate (spec §6.2).
    auth: ServerAuth,
    /// The uid a Unix peer must have: this process's effective uid.
    own_uid: u32,
    /// The connections still authenticating, and the instant each must be
    /// done by (spec §6.2), and whether it is a TCP one. The accept loop
    /// shuts down one past its deadline, and refuses a TCP connection while
    /// this holds [`auth::MAX_HANDSHAKES`] TCP ones; a Unix connection is
    /// never counted against that cap.
    handshakes: Mutex<HashMap<u64, Handshake>>,
    /// [`auth::HANDSHAKE_DEADLINE`] in milliseconds; a test lowers it.
    handshake_deadline_ms: AtomicU64,
    /// [`auth::MAX_HANDSHAKES`]; a test lowers it.
    max_handshakes: AtomicUsize,
    /// Connections closed at accept because too many were authenticating.
    #[cfg(test)]
    handshakes_refused: AtomicU64,
    /// The uid every Unix peer is taken to have, in place of its own.
    #[cfg(test)]
    peer_uid: Mutex<Option<u32>>,
    /// Requests that reached `dispatch` or a subscription.
    #[cfg(test)]
    dispatched: AtomicU64,
    /// Writes refused because the exactly-once table was full.
    #[cfg(test)]
    dedup_refused: AtomicU64,
}

struct ConnReg {
    state: Arc<ServerState>,
    id: u64,
}

impl Drop for ConnReg {
    /// A closed connection's sessions go with it.
    fn drop(&mut self) {
        lock_mutex(&self.state.conns).remove(&self.id);
        self.state.sessions.drop_conn(self.id);
    }
}

/// Calls of the removed whole-state methods (`read_state`, `commit_ops`,
/// `commit_if_seq`), counted by every server in this test process.
#[cfg(test)]
static LEGACY_CALLS: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
pub(super) fn legacy_calls() -> u64 {
    LEGACY_CALLS.load(Ordering::SeqCst)
}

enum Bound {
    Unix { path: PathBuf, inode: u64 },
    Tcp { addr: SocketAddr },
}

/// Accept thread, shutdown flag, and the socket this server bound.
pub struct FerroStoreServer {
    bound: Bound,
    state: Arc<ServerState>,
    accept: Option<JoinHandle<()>>,
    maintenance: Option<MaintenanceWorker>,
}

/// One server-owned worker; dropping the sender wakes its timed wait immediately.
struct MaintenanceWorker {
    stop: Sender<()>,
    handle: JoinHandle<()>,
}

impl MaintenanceWorker {
    fn start(store: Arc<FerroStore>) -> Result<Self, StoreError> {
        let (stop, receiver) = mpsc::channel();
        let handle = thread::Builder::new()
            .name("fls-mq-maintenance".to_string())
            .spawn(move || {
                loop {
                    match receiver.recv_timeout(MQ_MAINTENANCE_INTERVAL) {
                        Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
                        Err(RecvTimeoutError::Timeout) => {
                            if let Err(error) = store.maintain_mq() {
                                eprintln!("ferrostore: periodic MQ maintenance failed: {error}");
                            }
                        }
                    }
                }
            })
            .map_err(|error| backend(format!("start MQ maintenance thread: {error}")))?;
        Ok(Self { stop, handle })
    }

    fn stop(self) -> thread::Result<()> {
        let _ = self.stop.send(());
        self.handle.join()
    }
}

impl FerroStoreServer {
    /// Bind a Unix socket and serve `store`.
    ///
    /// Moves `store` in and does not open its path again. The socket is mode
    /// 0600 before it listens; a missing parent directory is created 0700.
    /// If `path` is a socket that refuses a connection, it is stale and is
    /// unlinked first; one that accepts is another live server, and this
    /// returns [`StoreError::Backend`]. So does a `path` that is not a socket.
    ///
    /// A peer is served only when its uid (`SO_PEERCRED`) is this process's
    /// effective uid; it then holds the `admin` scope (spec §6.2).
    pub fn bind_unix(path: impl AsRef<Path>, store: FerroStore) -> Result<Self, StoreError> {
        Self::bind_unix_with(path, store, ServerAuth::new())
    }

    /// [`Self::bind_unix`], under `auth`: when `auth` holds a PSK, a peer
    /// must also pass the PSK challenge-response, after its uid is checked.
    /// A token file is refused: bearer tokens are for `ws://`.
    pub fn bind_unix_with(
        path: impl AsRef<Path>,
        store: FerroStore,
        auth: ServerAuth,
    ) -> Result<Self, StoreError> {
        if auth.has_tokens() {
            return Err(backend(
                "a token file is for ws:// and wss:// connections; a Unix socket checks the peer uid \
                 and, when one is configured, the PSK",
            ));
        }
        if auth.has_tcp_transport() {
            return Err(backend(
                "TLS and allow_plaintext_on are for bind_tcp; a Unix socket is local, mode \
                 0600, and checks the peer uid",
            ));
        }
        let path = path.as_ref();
        let parent = match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => Path::new("."),
        };
        if !parent.exists() {
            crate::private::create_private_dir_all(parent)
                .map_err(|err| backend(format!("create directory {}: {err}", parent.display())))?;
        }
        prepare_bind_path(path)?;
        let listener = bind_private_unix(path)
            .map_err(|err| backend(format!("bind {}: {err}", path.display())))?;
        if let Err(err) = listener.set_nonblocking(true) {
            drop(listener);
            let _ = fs::remove_file(path);
            return Err(backend(format!("set socket nonblocking: {err}")));
        }
        let inode = match fs::symlink_metadata(path) {
            Ok(meta) => meta.ino(),
            Err(err) => {
                drop(listener);
                let _ = fs::remove_file(path);
                return Err(backend(format!("stat {}: {err}", path.display())));
            }
        };
        let state = new_state(store, auth);
        let maintenance = match MaintenanceWorker::start(Arc::clone(&state.store)) {
            Ok(worker) => worker,
            Err(error) => {
                unlink_bound(path, inode);
                return Err(error);
            }
        };
        let accept_state = Arc::clone(&state);
        let path_buf = path.to_path_buf();
        let accept = match thread::Builder::new()
            .name("fls-accept".to_string())
            .spawn(move || accept_loop(move || poll_unix(&listener), accept_state))
        {
            Ok(handle) => handle,
            Err(err) => {
                let _ = maintenance.stop();
                unlink_bound(&path_buf, inode);
                return Err(backend(format!("start accept thread: {err}")));
            }
        };
        Ok(Self {
            bound: Bound::Unix {
                path: path_buf,
                inode,
            },
            state,
            accept: Some(accept),
            maintenance: Some(maintenance),
        })
    }

    /// The checks [`Self::bind_tcp`] makes before it binds, with no store
    /// needed: `auth` holds a method, and every address `addr` resolves to
    /// may be bound under it (spec §6.3). Each address, with the interface a
    /// plaintext listener on it must be pinned to. A caller that would create
    /// a store to serve runs this first, so a refused bind creates nothing.
    pub(crate) fn check_tcp_bind(
        addr: impl ToSocketAddrs,
        auth: &ServerAuth,
    ) -> Result<Vec<(SocketAddr, Option<String>)>, StoreError> {
        if !auth.any_method() {
            return Err(backend(
                "bind_tcp needs authentication: configure a PSK, a token file, or both \
                 (ServerAuth)",
            ));
        }
        let addrs: Vec<SocketAddr> = addr
            .to_socket_addrs()
            .map_err(|err| backend(format!("bind tcp: resolve the address: {err}")))?
            .collect();
        if addrs.is_empty() {
            return Err(backend("bind tcp: the address resolves to nothing"));
        }
        let mut checked = Vec::with_capacity(addrs.len());
        for addr in addrs {
            let device = auth.check_bind(addr)?;
            checked.push((addr, device));
        }
        Ok(checked)
    }

    /// Listen on TCP, speak RFC 6455, and serve `store`. `127.0.0.1:0` must bind an ephemeral port.
    ///
    /// `auth` must hold a PSK, a token file, or both: a TCP server without
    /// authentication is refused before it binds. A connection with a valid
    /// bearer token holds that token's scope; one that passes the PSK
    /// challenge-response holds `admin` (spec §6.2).
    ///
    /// With TLS ([`ServerAuth::with_tls_files`]) this serves `wss://` on any
    /// address. Without it, plain `ws://` binds only a loopback address, or
    /// an address of an interface named in [`ServerAuth::allow_plaintext_on`];
    /// every other address, `0.0.0.0` and `::` always, is refused before it is
    /// bound (spec §6.3).
    pub fn bind_tcp(
        addr: impl ToSocketAddrs,
        store: FerroStore,
        auth: ServerAuth,
    ) -> Result<Self, StoreError> {
        let checked = Self::check_tcp_bind(addr, &auth)?;
        // As `TcpListener::bind` does: the first address that binds. A
        // plaintext bind on a named interface's address is pinned to that
        // interface (`SO_BINDTODEVICE`), or not made at all.
        let mut bound = None;
        let mut last_err = None;
        for (addr, device) in &checked {
            match bind::listen(*addr, device.as_deref()) {
                Ok(listener) => {
                    bound = Some((listener, device.clone()));
                    break;
                }
                Err(err) => last_err = Some(err),
            }
        }
        let Some((listener, device)) = bound else {
            let err = last_err.map_or_else(|| "no address".to_string(), |err| err.to_string());
            return Err(backend(format!("bind tcp: {err}")));
        };
        if let Err(err) = listener.set_nonblocking(true) {
            return Err(backend(format!("set socket nonblocking: {err}")));
        }
        let local = listener
            .local_addr()
            .map_err(|err| backend(format!("tcp local address: {err}")))?;
        // The address bound is one of those checked; checked again, so no
        // resolution can widen it, and it must name the interface it was
        // pinned to.
        if auth.check_bind(local)? != device {
            return Err(backend(format!(
                "bind tcp: {local} is pinned to an interface other than the one \
                 allow_plaintext_on names for it"
            )));
        }
        let tls = auth.tls().cloned();
        let state = new_state(store, auth);
        let maintenance = MaintenanceWorker::start(Arc::clone(&state.store))?;
        let accept_state = Arc::clone(&state);
        let accept = match thread::Builder::new()
            .name("fls-accept".to_string())
            .spawn(move || accept_loop(move || poll_tcp(&listener, tls.as_ref()), accept_state))
        {
            Ok(handle) => handle,
            Err(err) => {
                let _ = maintenance.stop();
                return Err(backend(format!("start accept thread: {err}")));
            }
        };
        Ok(Self {
            bound: Bound::Tcp { addr: local },
            state,
            accept: Some(accept),
            maintenance: Some(maintenance),
        })
    }

    /// The address actually bound (`127.0.0.1:<port>` for an ephemeral bind).
    pub fn local_addr(&self) -> SocketAddr {
        match &self.bound {
            Bound::Tcp { addr } => *addr,
            Bound::Unix { .. } => SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, 0)),
        }
    }
}

fn new_state(store: FerroStore, auth: ServerAuth) -> Arc<ServerState> {
    Arc::new(ServerState {
        store: Arc::new(store),
        shutdown: AtomicBool::new(false),
        conns: Mutex::new(HashMap::new()),
        next_conn: AtomicU64::new(1),
        sessions: Sessions::new(),
        dedup: Dedup::default(),
        auth,
        own_uid: auth::own_uid(),
        handshakes: Mutex::new(HashMap::new()),
        handshake_deadline_ms: AtomicU64::new(
            u64::try_from(auth::HANDSHAKE_DEADLINE.as_millis()).unwrap_or(u64::MAX),
        ),
        max_handshakes: AtomicUsize::new(auth::MAX_HANDSHAKES),
        #[cfg(test)]
        handshakes_refused: AtomicU64::new(0),
        #[cfg(test)]
        peer_uid: Mutex::new(None),
        #[cfg(test)]
        dispatched: AtomicU64::new(0),
        #[cfg(test)]
        dedup_refused: AtomicU64::new(0),
    })
}

#[cfg(test)]
impl FerroStoreServer {
    pub(super) fn store(&self) -> &FerroStore {
        &self.state.store
    }

    /// Read sessions open on this server, over every connection.
    pub(super) fn open_sessions(&self) -> usize {
        self.state.sessions.count()
    }

    /// Lower the idle time after which a session is gone.
    pub(super) fn set_session_idle(&self, idle: Duration) {
        self.state.sessions.set_idle(idle);
    }

    /// Lower the time after which a session is gone however often it is read.
    pub(super) fn set_session_lifetime(&self, lifetime: Duration) {
        self.state.sessions.set_lifetime(lifetime);
    }

    /// Take every later Unix peer to have `uid` (`None`: its own).
    pub(super) fn set_peer_uid(&self, uid: Option<u32>) {
        *lock_mutex(&self.state.peer_uid) = uid;
    }

    /// Requests this server ran (dispatched or subscribed).
    pub(super) fn dispatched(&self) -> u64 {
        self.state.dispatched.load(Ordering::SeqCst)
    }

    /// The exactly-once table.
    pub(super) fn dedup(&self) -> &Dedup {
        &self.state.dedup
    }

    /// Writes refused because the exactly-once table was full.
    pub(super) fn dedup_refusals(&self) -> u64 {
        self.state.dedup_refused.load(Ordering::SeqCst)
    }

    /// Lower the time a connection has to authenticate, for connections
    /// accepted from now on.
    pub(super) fn set_handshake_deadline(&self, deadline: Duration) {
        let ms = u64::try_from(deadline.as_millis()).unwrap_or(u64::MAX);
        self.state.handshake_deadline_ms.store(ms, Ordering::SeqCst);
    }

    /// Change how many connections may authenticate at once.
    pub(super) fn set_max_handshakes(&self, max: usize) {
        self.state.max_handshakes.store(max, Ordering::SeqCst);
    }

    /// Connections accepted and still authenticating.
    pub(super) fn handshakes_in_flight(&self) -> usize {
        lock_mutex(&self.state.handshakes).len()
    }

    /// Connections closed at accept because too many were authenticating.
    pub(super) fn handshakes_refused(&self) -> u64 {
        self.state.handshakes_refused.load(Ordering::SeqCst)
    }
}

impl FerroStoreServer {
    /// Stop accepting requests, join all connection handlers, then flush buffered Adaptive
    /// queue messages. An error is returned to the caller, including when flushing fails.
    /// Repeated calls can retry a failed flush after the handlers have been joined.
    pub fn shutdown(&mut self) -> Result<usize, StoreError> {
        self.quiesce()?;
        self.state.store.flush_mq(None)
    }

    fn quiesce(&mut self) -> Result<(), StoreError> {
        // Stop accept and connection readers before joining. Unlink only the
        // inode a unix server bound, so a replacement socket is left alone.
        // A tcp server has no filesystem path.
        self.state.shutdown.store(true, Ordering::SeqCst);
        // Wake the worker before joining the accept loop; an in-progress spill is
        // joined before the final flush so no maintenance races shutdown.
        if let Some(worker) = &self.maintenance {
            let _ = worker.stop.send(());
        }
        shutdown_conns(&self.state);
        match &self.bound {
            Bound::Unix { path, .. } => {
                let _ = UnixStream::connect(path);
            }
            Bound::Tcp { addr } => {
                let _ = TcpStream::connect_timeout(addr, Duration::from_millis(200));
            }
        }
        let joined = self.accept.take().map(|handle| handle.join());
        let maintenance_joined = self.maintenance.take().map(MaintenanceWorker::stop);
        if let Bound::Unix { path, inode } = &self.bound {
            unlink_bound(path, *inode);
        }
        if joined.is_some_and(|result| result.is_err()) {
            return Err(backend("FerroStore accept thread panicked during shutdown"));
        }
        if maintenance_joined.is_some_and(|result| result.is_err()) {
            return Err(backend(
                "FerroStore MQ maintenance thread panicked during shutdown",
            ));
        }
        Ok(())
    }
}

impl Drop for FerroStoreServer {
    fn drop(&mut self) {
        if let Err(error) = self.shutdown() {
            eprintln!("ferrostore: graceful shutdown failed: {error}");
        }
    }
}

/// Clear the way for a bind. A socket is removed only after a `connect` to it
/// was refused: nothing listens there. A socket that accepts belongs to a
/// live server and is left alone.
fn prepare_bind_path(path: &Path) -> Result<(), StoreError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => match UnixStream::connect(path) {
            Ok(_) => Err(backend(format!(
                "a FerroStore server is already listening on {}",
                path.display()
            ))),
            Err(err) if err.kind() == io::ErrorKind::ConnectionRefused => fs::remove_file(path)
                .map_err(|err| backend(format!("unlink {}: {err}", path.display()))),
            // It went away between the stat and the connect.
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(backend(format!(
                "connect {} to see whether it is stale: {err}; it was not removed",
                path.display()
            ))),
        },
        Ok(_) => Err(backend(format!(
            "{} exists and is not a socket",
            path.display()
        ))),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(backend(format!("stat {}: {err}", path.display()))),
    }
}

/// `socket`, `bind`, `chmod 0600`, then `listen`: no peer can connect before
/// the mode is owner-only. A failure after the bind removes the node it made.
fn bind_private_unix(path: &Path) -> io::Result<UnixListener> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;

    let bytes = path.as_os_str().as_bytes();
    // SAFETY: `sockaddr_un` is plain old data; all-zero is a valid value.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.is_empty() || bytes.contains(&0) || !super::socket_path_fits(path) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the socket path is empty, holds a NUL, or is too long for a Unix socket",
        ));
    }
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (slot, byte) in addr.sun_path.iter_mut().zip(bytes) {
        *slot = *byte as libc::c_char;
    }
    let len = libc::socklen_t::try_from(
        std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1,
    )
    .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "socket address too long"))?;

    #[cfg(any(target_os = "linux", target_os = "android"))]
    let kind = libc::SOCK_STREAM | libc::SOCK_CLOEXEC;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let kind = libc::SOCK_STREAM;
    // SAFETY: plain syscall; the result is checked before use.
    let raw = unsafe { libc::socket(libc::AF_UNIX, kind, 0) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `raw` is a new descriptor this function alone owns.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        // SAFETY: `fd` is open; FD_CLOEXEC only marks it close-on-exec.
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    // SAFETY: `addr` is a valid `sockaddr_un` and `len` covers the path and its NUL.
    let rc = unsafe {
        libc::bind(
            fd.as_raw_fd(),
            (&raw const addr).cast::<libc::sockaddr>(),
            len,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    let listened = fs::set_permissions(path, fs::Permissions::from_mode(0o600)).and_then(|()| {
        // SAFETY: `fd` is a bound stream socket.
        if unsafe { libc::listen(fd.as_raw_fd(), 128) } != 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    });
    if let Err(err) = listened {
        let _ = fs::remove_file(path);
        return Err(err);
    }
    Ok(UnixListener::from(fd))
}

fn unlink_bound(path: &Path, inode: u64) {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return;
    };
    if meta.ino() == inode && meta.file_type().is_socket() {
        let _ = fs::remove_file(path);
    }
}

fn shutdown_conns(state: &ServerState) {
    let conns = std::mem::take(&mut *lock_mutex(&state.conns));
    for (_, conn) in conns {
        conn.shutdown_both();
    }
}

enum AcceptPoll {
    Conn(Accepted),
    Retry,
    Wait,
    Stop,
}

struct Accepted {
    read: Box<dyn JsonIo>,
    write: Box<dyn JsonIo>,
    watch: Box<dyn JsonIo>,
    peer: Peer,
}

/// Accept connections and start one thread for each. Between accepts it
/// joins finished threads and shuts down every connection still
/// authenticating past its deadline ([`reap_handshakes`]): this loop wakes
/// at least every [`ACCEPT_PAUSE`], so no thread per connection is needed.
fn accept_loop(mut poll: impl FnMut() -> AcceptPoll, state: Arc<ServerState>) {
    let mut handlers = Vec::new();
    loop {
        reap(&mut handlers);
        reap_handshakes(&state);
        if state.shutdown.load(Ordering::SeqCst) {
            break;
        }
        match poll() {
            AcceptPoll::Conn(accepted) => {
                if state.shutdown.load(Ordering::SeqCst) {
                    accepted.watch.shutdown_both();
                    break;
                }
                let id = state.next_conn.fetch_add(1, Ordering::Relaxed);
                if !begin_handshake(&state, id, matches!(accepted.peer, Peer::Tcp)) {
                    // Too many TCP connections authenticating: closed
                    // before anything is read or any thread started for it.
                    #[cfg(test)]
                    state.handshakes_refused.fetch_add(1, Ordering::SeqCst);
                    accepted.watch.shutdown_both();
                    continue;
                }
                lock_mutex(&state.conns).insert(id, accepted.watch);
                let conn_state = Arc::clone(&state);
                let read = accepted.read;
                let write = Arc::new(Mutex::new(accepted.write));
                let peer = accepted.peer;
                match thread::Builder::new()
                    .name("fls-conn".to_string())
                    .spawn(move || serve_connection(read, write, peer, conn_state, id))
                {
                    Ok(handle) => handlers.push(handle),
                    Err(_) => {
                        lock_mutex(&state.handshakes).remove(&id);
                        lock_mutex(&state.conns).remove(&id);
                    }
                }
            }
            AcceptPoll::Retry => {}
            AcceptPoll::Wait => thread::sleep(ACCEPT_PAUSE),
            AcceptPoll::Stop => break,
        }
    }
    state.shutdown.store(true, Ordering::SeqCst);
    shutdown_conns(&state);
    for handle in handlers {
        let _ = handle.join();
    }
}

/// Register connection `id` as authenticating, due by the deadline from
/// now. `false`: a TCP connection while [`auth::MAX_HANDSHAKES`] TCP
/// connections are authenticating already. A Unix connection (only its
/// owner can connect) always gets a slot, and has the deadline.
fn begin_handshake(state: &ServerState, id: u64, tcp: bool) -> bool {
    let deadline = Duration::from_millis(state.handshake_deadline_ms.load(Ordering::SeqCst));
    let mut pending = lock_mutex(&state.handshakes);
    if tcp {
        let in_flight = pending.values().filter(|h| h.tcp).count();
        if in_flight >= state.max_handshakes.load(Ordering::SeqCst) {
            return false;
        }
    }
    pending.insert(
        id,
        Handshake {
            deadline: Instant::now() + deadline,
            tcp,
        },
    );
    true
}

/// Shut down every connection still authenticating past its deadline,
/// through the transport's own [`JsonIo::shutdown_both`]. Its entry is
/// removed first, under the lock that [`end_handshake`] takes: a
/// connection whose handshake passed after this point finds it gone and
/// is refused, so none is cut off once it is served.
fn reap_handshakes(state: &ServerState) {
    let now = Instant::now();
    let expired: Vec<u64> = {
        let mut pending = lock_mutex(&state.handshakes);
        let expired: Vec<u64> = pending
            .iter()
            .filter(|(_, handshake)| now >= handshake.deadline)
            .map(|(id, _)| *id)
            .collect();
        for id in &expired {
            pending.remove(id);
        }
        expired
    };
    if expired.is_empty() {
        return;
    }
    let conns = lock_mutex(&state.conns);
    for id in expired {
        if let Some(conn) = conns.get(&id) {
            conn.shutdown_both();
        }
    }
}

/// End connection `id`'s handshake. `false`: the deadline passed first and
/// [`reap_handshakes`] has shut it down.
fn end_handshake(state: &ServerState, id: u64) -> bool {
    lock_mutex(&state.handshakes).remove(&id).is_some()
}

/// Ends a connection's handshake however [`authenticate`] returns.
struct HandshakeSlot<'a> {
    state: &'a ServerState,
    id: u64,
}

impl Drop for HandshakeSlot<'_> {
    fn drop(&mut self) {
        end_handshake(self.state, self.id);
    }
}

fn poll_unix(listener: &UnixListener) -> AcceptPoll {
    match listener.accept() {
        Ok((stream, _)) => match unix_accepted(stream) {
            Ok(accepted) => AcceptPoll::Conn(accepted),
            Err(_) => AcceptPoll::Retry,
        },
        Err(err) if err.kind() == io::ErrorKind::WouldBlock => AcceptPoll::Wait,
        Err(err) if accept_again(&err) => AcceptPoll::Retry,
        Err(_) => AcceptPoll::Stop,
    }
}

fn poll_tcp(listener: &TcpListener, tls: Option<&ServerTls>) -> AcceptPoll {
    match listener.accept() {
        Ok((stream, _)) => match tcp_accepted(stream, tls) {
            Ok(accepted) => AcceptPoll::Conn(accepted),
            Err(_) => AcceptPoll::Retry,
        },
        Err(err) if err.kind() == io::ErrorKind::WouldBlock => AcceptPoll::Wait,
        Err(err) if accept_again(&err) => AcceptPoll::Retry,
        Err(_) => AcceptPoll::Stop,
    }
}

fn unix_accepted(stream: UnixStream) -> Result<Accepted, StoreError> {
    // An error is an unknown peer, which the handshake refuses.
    let peer = Peer::Unix(auth::peer_uid(&stream).ok());
    let write = stream
        .try_clone()
        .map_err(|err| backend(format!("clone unix stream: {err}")))?;
    let watch = stream
        .try_clone()
        .map_err(|err| backend(format!("clone unix stream: {err}")))?;
    Ok(Accepted {
        read: Box::new(UnixIo::new(stream, FrameCounter::default())),
        write: Box::new(UnixIo::new(write, FrameCounter::default())),
        watch: Box::new(UnixIo::new(watch, FrameCounter::default())),
        peer,
    })
}

/// The halves of an accepted TCP connection. With `tls`, the reading and
/// writing halves share one TLS state ([`TlsStream`]) whose handshake runs
/// later, on the connection's thread (in `prepare`), under the handshake
/// deadline; the watcher is the bare socket, whose `shutdown_both` ends a
/// TLS handshake as it ends any read.
fn tcp_accepted(stream: TcpStream, tls: Option<&ServerTls>) -> Result<Accepted, StoreError> {
    stream
        .set_nodelay(true)
        .map_err(|err| backend(format!("set nodelay: {err}")))?;
    // `O_NONBLOCK` is per file description and is shared by `try_clone`.
    // Accepted sockets must block in the frame reader.
    stream
        .set_nonblocking(false)
        .map_err(|err| backend(format!("set socket blocking: {err}")))?;
    let watch = stream
        .try_clone()
        .map_err(|err| backend(format!("clone tcp stream: {err}")))?;
    let (read, write) = match tls {
        None => {
            let write = stream
                .try_clone()
                .map_err(|err| backend(format!("clone tcp stream: {err}")))?;
            (Wire::Plain(stream), Wire::Plain(write))
        }
        Some(tls) => {
            let (read, write) = TlsStream::server(stream, tls)?;
            (Wire::Tls(read), Wire::Tls(write))
        }
    };
    Ok(Accepted {
        read: Box::new(WsIo::server(read, true)),
        write: Box::new(WsIo::server(write, false)),
        watch: Box::new(WsIo::server(Wire::Plain(watch), false)),
        peer: Peer::Tcp,
    })
}

fn accept_again(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::Interrupted | io::ErrorKind::ConnectionAborted
    )
}

fn reap(handlers: &mut Vec<JoinHandle<()>>) {
    let pending = std::mem::take(handlers);
    for handle in pending {
        if handle.is_finished() {
            let _ = handle.join();
        } else {
            handlers.push(handle);
        }
    }
}

fn serve_connection(
    mut read: Box<dyn JsonIo>,
    write: Arc<Mutex<Box<dyn JsonIo>>>,
    peer: Peer,
    state: Arc<ServerState>,
    conn_id: u64,
) {
    let _reg = ConnReg {
        state: Arc::clone(&state),
        id: conn_id,
    };
    let Some((scope, principal)) = authenticate(&mut *read, &write, peer, &state, conn_id) else {
        return;
    };
    let alive = Arc::new(AtomicBool::new(true));
    let mut subscribed = HashSet::new();
    let mut forwarders = Vec::new();
    loop {
        if state.shutdown.load(Ordering::SeqCst) {
            break;
        }
        let incoming = match read.read_value(ws::MAX_PAYLOAD) {
            Ok(incoming) => incoming,
            Err(_) => break,
        };
        match incoming {
            Incoming::Ping(payload) => {
                let mut stream = lock_mutex(&write);
                if stream.write_pong(&payload).is_err() {
                    break;
                }
            }
            Incoming::Close => break,
            Incoming::Value(frame) => {
                let Some(id) = frame.get("id").and_then(Value::as_u64) else {
                    break;
                };
                let method = frame.get("method").and_then(Value::as_str).unwrap_or("");
                let params = frame.get("params").cloned().unwrap_or_else(|| json!({}));
                // A method the scope does not cover, or an unknown one, is an
                // error response that ran nothing.
                let reply = match auth::authorize(scope, method) {
                    Err(err) => {
                        #[cfg(test)]
                        if matches!(method, "read_state" | "commit_ops" | "commit_if_seq") {
                            LEGACY_CALLS.fetch_add(1, Ordering::SeqCst);
                        }
                        reply_body(Err(err))
                    }
                    Ok(rule) if rule.name == "subscribe_mq" => {
                        #[cfg(test)]
                        state.dispatched.fetch_add(1, Ordering::SeqCst);
                        reply_body(
                            subscribe_topic(
                                &state.store,
                                &params,
                                &write,
                                &alive,
                                &mut subscribed,
                                &mut forwarders,
                            )
                            .map(|()| Value::Null),
                        )
                    }
                    Ok(rule) => exactly_once(&state, conn_id, &principal, rule, &params),
                };
                // An error response leaves the connection up.
                if send_reply(&write, id, reply).is_err() {
                    break;
                }
            }
        }
    }
    alive.store(false, Ordering::SeqCst);
    for handle in forwarders {
        let _ = handle.join();
    }
    // Over TLS this sends close_notify first (best-effort, bounded), so the
    // peer reads a clean end, not a truncation.
    lock_mutex(&write).shutdown_both();
}

/// Authenticate the connection before anything is read from it as a
/// request (spec §6.2): its scope and principal. `None`: it was refused,
/// or its handshake deadline passed, and it is shut down.
fn authenticate(
    read: &mut dyn JsonIo,
    write: &Mutex<Box<dyn JsonIo>>,
    peer: Peer,
    state: &ServerState,
    conn_id: u64,
) -> Option<(Scope, Principal)> {
    let slot = HandshakeSlot { state, id: conn_id };
    #[cfg(test)]
    let peer = match (peer, *lock_mutex(&state.peer_uid)) {
        (Peer::Unix(_), Some(uid)) => Peer::Unix(Some(uid)),
        (peer, _) => peer,
    };
    let mut write = lock_mutex(write);
    // A peer that sends nothing is refused, not waited for.
    if read
        .set_read_timeout(Some(auth::HANDSHAKE_TIMEOUT))
        .is_err()
    {
        write.shutdown_both();
        return None;
    }
    let credential = match read.prepare() {
        Ok(values) => auth::credential(&values),
        Err(_) => {
            // No frames before the transport's own handshake completes.
            auth::refuse(&mut **write, false);
            return None;
        }
    };
    let credential = match peer {
        Peer::Unix(_) => Credential::None,
        Peer::Tcp => credential,
    };
    let authed = auth::serve_handshake(
        read,
        &mut **write,
        &peer,
        credential,
        &state.auth,
        state.own_uid,
    )?;
    // Past the deadline the reaper has shut the connection down already.
    if !end_handshake(state, slot.id) || read.set_read_timeout(None).is_err() {
        write.shutdown_both();
        return None;
    }
    Some(authed)
}

fn subscribe_topic(
    store: &FerroStore,
    params: &Value,
    write: &Arc<Mutex<Box<dyn JsonIo>>>,
    alive: &Arc<AtomicBool>,
    subscribed: &mut HashSet<String>,
    forwarders: &mut Vec<JoinHandle<()>>,
) -> Result<(), StoreError> {
    let topic = param_str(params, "topic")?.to_string();
    if subscribed.contains(&topic) {
        return Ok(());
    }
    // One in-process subscription per topic. It does not replay history.
    let rx = store.subscribe_mq(&topic);
    let topic_for_thread = topic.clone();
    let write = Arc::clone(write);
    let alive = Arc::clone(alive);
    let handle = thread::Builder::new()
        .name("fls-mq".to_string())
        .spawn(move || forward_mq(rx, topic_for_thread, write, alive))
        .map_err(|err| backend(format!("start mq forwarder: {err}")))?;
    subscribed.insert(topic);
    forwarders.push(handle);
    Ok(())
}

fn forward_mq(
    rx: Receiver<MqRecord>,
    topic: String,
    write: Arc<Mutex<Box<dyn JsonIo>>>,
    alive: Arc<AtomicBool>,
) {
    loop {
        if !alive.load(Ordering::SeqCst) {
            break;
        }
        match rx.recv_timeout(FORWARD_PAUSE) {
            Ok(record) => {
                let frame = json!({
                    "method": "mq",
                    "params": {
                        "topic": topic,
                        "record": record_value(record),
                    }
                });
                let mut stream = lock_mutex(&write);
                if stream.write_value(&frame).is_err() {
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

/// Run one authorized request. A write ([`MethodRule::writes`]) that
/// carries `req` runs once: a repeat of its id by the same principal, for
/// the same method, gets the first reply and applies nothing (spec §6.1);
/// for another method it is refused. Ids are per principal: another
/// principal's id never answers this one. Any other method ignores `req`,
/// so no read's answer is kept.
fn exactly_once(
    state: &ServerState,
    conn: u64,
    principal: &Principal,
    rule: &MethodRule,
    params: &Value,
) -> Value {
    let method = rule.name;
    if !rule.writes {
        return reply_body(dispatch(state, conn, principal, method, params));
    }
    let req = match session::request_id(params) {
        Ok(req) => req,
        Err(err) => return reply_body(Err(err)),
    };
    let Some(req) = req else {
        return reply_body(dispatch(state, conn, principal, method, params));
    };
    match state.dedup.claim(principal, method, req) {
        Err(full) => {
            #[cfg(test)]
            state.dedup_refused.fetch_add(1, Ordering::SeqCst);
            reply_body(Err(full))
        }
        Ok(Claim::Replay(reply)) => reply,
        Ok(Claim::Run(running)) => {
            let reply = reply_body(dispatch(state, conn, principal, method, params));
            running.finish(&reply);
            reply
        }
    }
}

fn dispatch(
    state: &ServerState,
    conn: u64,
    principal: &Principal,
    method: &str,
    params: &Value,
) -> Result<Value, StoreError> {
    #[cfg(test)]
    state.dispatched.fetch_add(1, Ordering::SeqCst);
    let store = &*state.store;
    match method {
        "open_session" => {
            let pinned = store.read_state()?;
            let base_seq = pinned.last_seq;
            let session = state.sessions.open(conn, principal, pinned)?;
            Ok(json!({"session": session, "base_seq": base_seq}))
        }
        "close_session" => {
            state.sessions.close(conn, param_u64(params, "session")?);
            Ok(Value::Null)
        }
        "txn_read" => {
            let key = session::parse_key(params.get("read"))?;
            match session::optional_u64(params, "session")? {
                Some(id) => {
                    let pinned = state.sessions.state(conn, id)?;
                    txn_read(&pinned, &key)
                }
                None => {
                    // The answer first: a read that fails opens no session,
                    // whose id the client would never learn.
                    let pinned = store.read_state()?;
                    let answer = session::answer(&pinned, &key)?;
                    let id = state.sessions.open(conn, principal, Arc::clone(&pinned))?;
                    Ok(txn_read_reply(&pinned, answer, Some(id)))
                }
            }
        }
        "commit_txn" => session::commit_txn(store, &state.sessions, conn, params),
        "put_doc" => {
            let collection = param_str(params, "collection")?;
            let id = param_str(params, "id")?;
            let doc = param_value(params, "doc")?;
            let silent = optional_bool(params, "silent")?;
            if optional_bool(params, "if_absent")? {
                let placed = store.put_if_absent(collection, id, &doc, silent)?;
                let mut body = serde_json::Map::new();
                body.insert("wrote".into(), Value::Bool(placed.wrote));
                body.insert("doc".into(), placed.doc);
                if let Some(seq) = placed.store_seq {
                    body.insert("store_seq".into(), json!(seq));
                }
                Ok(Value::Object(body))
            } else {
                store.put_doc_inner(collection, id, &doc, silent)?;
                Ok(Value::Null)
            }
        }
        "get_doc" => {
            // Null is a missing id. A stored value, including JSON null, is wrapped
            // so the client can tell the two apart.
            match store.get_doc(param_str(params, "collection")?, param_str(params, "id")?)? {
                Some(doc) => Ok(json!({"doc": doc})),
                None => Ok(Value::Null),
            }
        }
        "delete_doc" => {
            store.delete_doc(param_str(params, "collection")?, param_str(params, "id")?)?;
            Ok(Value::Null)
        }
        "query_by_field" => {
            let ids = store.query_by_field(
                param_str(params, "collection")?,
                param_str(params, "field")?,
                param_str(params, "value")?,
            )?;
            Ok(Value::Array(ids.into_iter().map(Value::String).collect()))
        }
        "search_bm25" => {
            let hits = store.search_bm25(
                param_str(params, "collection")?,
                param_str(params, "query")?,
                param_usize(params, "limit")?,
            )?;
            Ok(Value::Array(
                hits.into_iter()
                    .map(|(id, score)| json!([id, score]))
                    .collect(),
            ))
        }
        "publish_mq" => {
            let topic = param_str(params, "topic")?;
            let key = param_str(params, "key")?;
            let payload = param_value(params, "payload")?;
            let require_single = optional_bool(params, "require_single_publish")?;
            if let Some(value) = params.get("durability").filter(|value| !value.is_null()) {
                let durability: MqDurability = serde_json::from_value(value.clone())
                    .map_err(|error| backend(format!("invalid mq durability: {error}")))?;
                if require_single && durability != MqDurability::Durable {
                    return Err(backend("require_single_publish requires durable MQ"));
                }
                let (seq, store_seq) = if require_single {
                    let (seq, commit) = store.publish_mq_single_detail(topic, key, &payload)?;
                    (seq, Some(commit))
                } else {
                    store.publish_mq_with_detail(topic, key, &payload, durability)?
                };
                let tier = if store_seq.is_some() {
                    "disk"
                } else {
                    "memory"
                };
                return Ok(json!({"seq": seq, "store_seq": store_seq, "tier": tier}));
            }
            let (seq, store_seq) = if require_single {
                store.publish_mq_single_detail(topic, key, &payload)?
            } else {
                store.publish_mq_detail(topic, key, &payload)?
            };
            Ok(json!({"seq": seq, "store_seq": store_seq}))
        }
        "publish_mq_v2" => {
            let topic = param_str(params, "topic")?;
            let key = param_str(params, "key")?;
            let payload = param_value(params, "payload")?;
            let value = params
                .get("durability")
                .ok_or_else(|| backend("missing mq durability"))?;
            let durability: MqDurability = serde_json::from_value(value.clone())
                .map_err(|error| backend(format!("invalid mq durability: {error}")))?;
            let (seq, store_seq) =
                store.publish_mq_with_detail(topic, key, &payload, durability)?;
            let tier = if store_seq.is_some() {
                "disk"
            } else {
                "memory"
            };
            Ok(json!({"seq": seq, "store_seq": store_seq, "tier": tier}))
        }
        "poll_mq_group" => {
            let records = store.poll_mq_group(
                param_str(params, "topic")?,
                param_str(params, "group")?,
                param_usize(params, "limit")?,
            )?;
            Ok(Value::Array(
                records.into_iter().map(record_value).collect(),
            ))
        }
        "ack_mq" => {
            store.ack_mq(
                param_str(params, "topic")?,
                param_str(params, "group")?,
                param_u64(params, "seq")?,
            )?;
            Ok(Value::Null)
        }
        "ack_mq_v2" => {
            let store_seq = store.ack_mq_with_detail(
                param_str(params, "topic")?,
                param_str(params, "group")?,
                param_u64(params, "seq")?,
            )?;
            Ok(json!({"store_seq": store_seq}))
        }
        "flush_mq" => Ok(json!({"flushed": store.flush_mq(optional_topic(params)?)?})),
        "maintain_mq" => Ok(json!({"flushed": store.maintain_mq()?})),
        "mq_telemetry" => serde_json::to_value(store.mq_telemetry(optional_topic(params)?)?)
            .map_err(|error| backend(format!("encode mq telemetry: {error}"))),
        "checkpoint" => {
            if optional_bool(params, "forbid_mq_flush")? {
                store.checkpoint_if_mq_idle()?;
            } else {
                store.checkpoint()?;
            }
            Ok(Value::Null)
        }
        "compact" => {
            store.compact()?;
            Ok(Value::Null)
        }
        "create_column_table" => {
            let columns = columnar::parse_columns(
                params
                    .get("columns")
                    .ok_or_else(|| backend("missing param `columns`"))?,
            )?;
            store.create_column_table(param_str(params, "name")?, &columns)?;
            Ok(Value::Null)
        }
        "append_column_rows" => {
            let rows = columnar::parse_rows(
                params
                    .get("rows")
                    .ok_or_else(|| backend("missing param `rows`"))?,
            )?;
            store.append_column_rows(param_str(params, "name")?, &rows)?;
            Ok(Value::Null)
        }
        "scan_column_table" => {
            let filters = columnar::parse_filters(
                params
                    .get("filters")
                    .ok_or_else(|| backend("missing param `filters`"))?,
            )?;
            columnar::scan_json(&store.scan_column_table(param_str(params, "name")?, &filters)?)
        }
        "attempt_rollup" => {
            let project = optional_project(params)?;
            Ok(columnar::attempt_rollup_json(
                &store.attempt_rollup(project.as_ref())?,
            ))
        }
        "gate_run_rollup" => {
            let gate = optional_gate(params)?;
            Ok(columnar::gate_rollup_json(
                &store.gate_run_rollup(gate.as_ref())?,
            ))
        }
        "create_vector_collection" => {
            store.create_vector_collection(
                param_str(params, "name")?,
                param_u32(params, "dim")?,
                vector::storage_from_name(param_str(params, "storage")?)?,
                vector::metric_from_name(param_str(params, "metric")?)?,
                param_u32(params, "m")?,
                param_u32(params, "ef_construction")?,
                param_u32(params, "ef_search")?,
            )?;
            Ok(Value::Null)
        }
        "upsert_vector" => {
            let values = vector::parse_values(
                params
                    .get("values")
                    .ok_or_else(|| backend("missing param `values`"))?,
            )?;
            let metadata = params.get("metadata");
            store.upsert_vector(
                param_str(params, "collection")?,
                param_str(params, "id")?,
                &values,
                metadata,
            )?;
            Ok(Value::Null)
        }
        "delete_vector" => {
            store.delete_vector(param_str(params, "collection")?, param_str(params, "id")?)?;
            Ok(Value::Null)
        }
        "has_vector_collection" => Ok(Value::Bool(
            store.has_vector_collection(param_str(params, "name")?)?,
        )),
        "get_vector" => {
            match store.get_vector(param_str(params, "collection")?, param_str(params, "id")?)? {
                Some((values, metadata)) => vector::vector_get_json(&values, &metadata),
                None => Ok(Value::Null),
            }
        }
        "search_vectors" => vector::hits_json(
            &store.search_vectors(
                param_str(params, "collection")?,
                &vector::parse_values(
                    params
                        .get("values")
                        .ok_or_else(|| backend("missing param `values`"))?,
                )?,
                param_usize(params, "limit")?,
                optional_u32(params, "ef")?,
            )?,
        ),
        "search_hybrid" => vector::hits_json(
            &store.search_hybrid(
                param_str(params, "doc_collection")?,
                param_str(params, "vector_collection")?,
                param_str(params, "query")?,
                &vector::parse_values(
                    params
                        .get("values")
                        .ok_or_else(|| backend("missing param `values`"))?,
                )?,
                param_usize(params, "limit")?,
                vector::fusion_from_json(
                    params
                        .get("fusion")
                        .ok_or_else(|| backend("missing param `fusion`"))?,
                )?,
            )?,
        ),
        "search_knowledge" => vector::knowledge_json(
            &store.search_knowledge(
                param_str(params, "doc_collection")?,
                param_str(params, "vector_collection")?,
                param_str(params, "query")?,
                &vector::parse_values(
                    params
                        .get("values")
                        .ok_or_else(|| backend("missing param `values`"))?,
                )?,
                param_usize(params, "limit")?,
                vector::fusion_from_json(
                    params
                        .get("fusion")
                        .ok_or_else(|| backend("missing param `fusion`"))?,
                )?,
            )?,
        ),
        "apply_sync_batches" => {
            let batches = parse_sync_batches(
                params
                    .get("batches")
                    .ok_or_else(|| backend("missing param `batches`"))?,
            )?;
            let silent = optional_bool(params, "silent")?;
            Ok(Value::from(store.apply_sync_batches(batches, silent)?))
        }
        // `auth::METHODS` names every method above, and only those.
        _ => Err(backend(format!("no such method `{method}`"))),
    }
}

/// One read primitive against an open session's `pinned` state: its
/// answer and the answer's hash.
fn txn_read(pinned: &super::state::DocState, key: &ReadKey) -> Result<Value, StoreError> {
    Ok(txn_read_reply(pinned, session::answer(pinned, key)?, None))
}

/// The reply to a `txn_read` whose answer at `pinned` is `(result, hash)`,
/// with the session when this read opened it.
fn txn_read_reply(
    pinned: &super::state::DocState,
    (result, hash): (Value, String),
    opened: Option<u64>,
) -> Value {
    let mut body = serde_json::Map::new();
    body.insert("result".into(), result);
    body.insert("hash".into(), Value::String(hash));
    if let Some(session) = opened {
        body.insert("session".into(), json!(session));
        body.insert("base_seq".into(), json!(pinned.last_seq));
    }
    Value::Object(body)
}

/// A reply without its frame id: what the exactly-once table keeps.
fn reply_body(result: Result<Value, StoreError>) -> Value {
    match result {
        Ok(result) => json!({"ok": true, "result": result}),
        Err(err) => {
            let mut body = error_value(0, &err);
            if let Some(object) = body.as_object_mut() {
                object.remove("id");
            }
            body
        }
    }
}

fn send_reply(write: &Mutex<Box<dyn JsonIo>>, id: u64, mut body: Value) -> Result<(), StoreError> {
    if let Some(object) = body.as_object_mut() {
        object.insert("id".into(), json!(id));
    }
    let mut stream = lock_mutex(write);
    stream.write_value(&body)
}

/// A failure response. `error` is always the error's message. An
/// [`StoreError::Undelivered`] also carries `kind`, `seq` and `cause` (the
/// cause's own message, as it reads), so the client rebuilds it: the commit
/// is durable on the server, and a client that read it as a plain failure
/// would retry it and apply it twice. A [`StoreError::Backend`] cause also
/// carries its detail as `cause_backend`, so it is rebuilt as that kind.
pub(super) fn error_value(id: u64, err: &StoreError) -> Value {
    let mut body = json!({"id": id, "ok": false, "error": err.to_string()});
    if let StoreError::Backend(detail) = err {
        body["backend"] = json!(detail);
    }
    if let StoreError::Undelivered { seq, cause } = err {
        body["kind"] = json!("undelivered");
        body["seq"] = json!(seq);
        body["cause"] = json!(cause.to_string());
        if let StoreError::Backend(detail) = &**cause {
            body["cause_backend"] = json!(detail);
        }
    }
    body
}

/// The error a failure response names: [`StoreError::Undelivered`] for a
/// structured one ([`error_value`]), whose message is the server's, and
/// otherwise the server's message as [`StoreError::Backend`]. The cause is
/// a [`StoreError::Backend`] when the server's was, and otherwise
/// [`StoreError::Relayed`] with the server's words.
pub(super) fn response_error(frame: &Value) -> StoreError {
    if frame.get("kind").and_then(Value::as_str) == Some("undelivered")
        && let Some(seq) = frame.get("seq").and_then(Value::as_u64)
        && let Some(cause) = frame.get("cause").and_then(Value::as_str)
    {
        let cause = match frame.get("cause_backend").and_then(Value::as_str) {
            Some(detail) => StoreError::Backend(detail.to_string()),
            None => StoreError::Relayed(cause.to_string()),
        };
        return StoreError::Undelivered {
            seq,
            cause: Box::new(cause),
        };
    }
    if let Some(detail) = frame.get("backend").and_then(Value::as_str) {
        return StoreError::Backend(detail.to_string());
    }
    backend(
        frame
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("FerroStore request failed"),
    )
}

fn record_value(record: MqRecord) -> Value {
    json!({
        "seq": record.seq,
        "key": record.key,
        "payload": record.payload,
    })
}

fn param_str<'a>(params: &'a Value, key: &str) -> Result<&'a str, StoreError> {
    params
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| backend(format!("missing string param `{key}`")))
}

fn param_u64(params: &Value, key: &str) -> Result<u64, StoreError> {
    params
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| backend(format!("missing integer param `{key}`")))
}

fn param_usize(params: &Value, key: &str) -> Result<usize, StoreError> {
    usize::try_from(param_u64(params, key)?)
        .map_err(|_| backend(format!("param `{key}` does not fit")))
}

fn param_u32(params: &Value, key: &str) -> Result<u32, StoreError> {
    u32::try_from(param_u64(params, key)?)
        .map_err(|_| backend(format!("param `{key}` does not fit")))
}

fn optional_bool(params: &Value, key: &str) -> Result<bool, StoreError> {
    match params.get(key) {
        None => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(backend(format!("param `{key}` is not a bool"))),
    }
}

fn parse_sync_batches(value: &Value) -> Result<Vec<SyncBatch>, StoreError> {
    serde_json::from_value(value.clone()).map_err(|err| backend(format!("sync batches: {err}")))
}

fn optional_topic(params: &Value) -> Result<Option<&str>, StoreError> {
    match params.get("topic") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(topic)) => Ok(Some(topic)),
        Some(_) => Err(backend("param `topic` is not a string or null")),
    }
}

fn optional_u32(params: &Value, key: &str) -> Result<Option<u32>, StoreError> {
    match params.get(key) {
        None => Ok(None),
        Some(value) => {
            let number = value
                .as_u64()
                .ok_or_else(|| backend(format!("missing integer param `{key}`")))?;
            let number = u32::try_from(number)
                .map_err(|_| backend(format!("param `{key}` does not fit")))?;
            Ok(Some(number))
        }
    }
}

fn param_value(params: &Value, key: &str) -> Result<Value, StoreError> {
    params
        .get(key)
        .cloned()
        .ok_or_else(|| backend(format!("missing param `{key}`")))
}

fn optional_project(params: &Value) -> Result<Option<ProjectId>, StoreError> {
    match params.get("project") {
        Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => {
            let iri = Iri::parse(text).map_err(backend)?;
            Ok(Some(ProjectId(iri)))
        }
        _ => Err(backend("missing project param")),
    }
}

fn optional_gate(params: &Value) -> Result<Option<GateId>, StoreError> {
    match params.get("gate") {
        Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => {
            let iri = Iri::parse(text).map_err(backend)?;
            Ok(Some(GateId(iri)))
        }
        _ => Err(backend("missing gate param")),
    }
}

fn op_value(op: &Op) -> Value {
    match op {
        Op::Put {
            collection,
            id,
            doc,
        } => json!({
            "op": "put",
            "collection": collection,
            "id": id,
            "doc": doc,
        }),
        Op::Delete { collection, id } => json!({
            "op": "delete",
            "collection": collection,
            "id": id,
        }),
        Op::CreateColumnTable { name, columns } => json!({
            "op": "create_column_table",
            "name": name,
            "columns": columns.iter().map(columnar::column_def_json).collect::<Vec<_>>(),
        }),
        Op::AppendColumnGroup { name, bytes } => json!({
            "op": "append_column_group",
            "name": name,
            "bytes": bytes.iter().copied().map(Value::from).collect::<Vec<_>>(),
        }),
        Op::CreateVectorCollection {
            name,
            dim,
            storage,
            metric,
            m,
            ef_construction,
            ef_search,
        } => json!({
            "op": "create_vector_collection",
            "name": name,
            "dim": dim,
            "storage": vector::storage_name(*storage),
            "metric": vector::metric_name(*metric),
            "m": m,
            "ef_construction": ef_construction,
            "ef_search": ef_search,
        }),
        Op::UpsertVector {
            collection,
            id,
            bytes,
            metadata,
        } => {
            let mut item = serde_json::Map::new();
            item.insert("op".into(), json!("upsert_vector"));
            item.insert("collection".into(), json!(collection));
            item.insert("id".into(), json!(id));
            item.insert(
                "bytes".into(),
                Value::Array(bytes.iter().copied().map(Value::from).collect()),
            );
            if let Some(metadata) = metadata {
                item.insert("metadata".into(), metadata.clone());
            }
            Value::Object(item)
        }
        Op::DeleteVector { collection, id } => json!({
            "op": "delete_vector",
            "collection": collection,
            "id": id,
        }),
    }
}

pub(super) fn parse_ops(value: &Value) -> Result<Vec<Op>, StoreError> {
    let items = value
        .as_array()
        .ok_or_else(|| backend("param `ops` is not an array"))?;
    items.iter().map(parse_op).collect()
}

fn parse_op(value: &Value) -> Result<Op, StoreError> {
    let kind = value
        .get("op")
        .and_then(Value::as_str)
        .ok_or_else(|| backend("op is missing op"))?;
    match kind {
        "put" => {
            let doc = value
                .get("doc")
                .cloned()
                .ok_or_else(|| backend("put op is missing doc"))?;
            Ok(Op::Put {
                collection: param_str(value, "collection")?.to_string(),
                id: param_str(value, "id")?.to_string(),
                doc,
            })
        }
        "delete" => Ok(Op::Delete {
            collection: param_str(value, "collection")?.to_string(),
            id: param_str(value, "id")?.to_string(),
        }),
        "create_column_table" => {
            let columns = columnar::parse_columns(
                value
                    .get("columns")
                    .ok_or_else(|| backend("create_column_table op is missing columns"))?,
            )?;
            Ok(Op::CreateColumnTable {
                name: param_str(value, "name")?.to_string(),
                columns,
            })
        }
        "append_column_group" => Ok(Op::AppendColumnGroup {
            name: param_str(value, "name")?.to_string(),
            bytes: columnar::parse_bytes(
                value
                    .get("bytes")
                    .ok_or_else(|| backend("append_column_group op is missing bytes"))?,
            )?,
        }),
        "create_vector_collection" => Ok(Op::CreateVectorCollection {
            name: param_str(value, "name")?.to_string(),
            dim: param_u32(value, "dim")?,
            storage: vector::storage_from_name(param_str(value, "storage")?)?,
            metric: vector::metric_from_name(param_str(value, "metric")?)?,
            m: param_u32(value, "m")?,
            ef_construction: param_u32(value, "ef_construction")?,
            ef_search: param_u32(value, "ef_search")?,
        }),
        "upsert_vector" => {
            let metadata = value.get("metadata").cloned();
            Ok(Op::UpsertVector {
                collection: param_str(value, "collection")?.to_string(),
                id: param_str(value, "id")?.to_string(),
                bytes: columnar::parse_bytes(
                    value
                        .get("bytes")
                        .ok_or_else(|| backend("upsert_vector op is missing bytes"))?,
                )?,
                metadata,
            })
        }
        "delete_vector" => Ok(Op::DeleteVector {
            collection: param_str(value, "collection")?.to_string(),
            id: param_str(value, "id")?.to_string(),
        }),
        other => Err(backend(format!("unknown op `{other}`"))),
    }
}

pub(super) enum Incoming {
    Value(Value),
    Ping(Vec<u8>),
    Close,
}

pub(super) trait JsonIo: Send {
    /// Finish the transport's own handshake (a `ws://` upgrade). Returns
    /// the values of the upgrade's `Authorization` headers.
    fn prepare(&mut self) -> Result<Vec<String>, StoreError> {
        Ok(Vec::new())
    }

    /// Bound each read of the stream (`None`: wait forever).
    fn set_read_timeout(&self, timeout: Option<Duration>) -> Result<(), StoreError>;

    /// The next frame. One that declares more than `max_len` bytes is
    /// refused before anything is allocated for it: before authentication
    /// the caller passes [`auth::PRE_AUTH_FRAME_BYTES`], after it
    /// [`ws::MAX_PAYLOAD`] (16 MiB).
    fn read_value(&mut self, max_len: usize) -> Result<Incoming, StoreError>;

    fn write_value(&mut self, value: &Value) -> Result<(), StoreError>;

    fn write_pong(&mut self, _payload: &[u8]) -> Result<(), StoreError> {
        Ok(())
    }

    fn shutdown_both(&self);

    /// The TLS channel binding of this connection (spec §6.3): `None` on a
    /// transport without TLS. An error on a TLS connection whose binding
    /// cannot be read: the handshake then refuses, never runs unbound.
    fn channel_binding(&self) -> Result<Option<tls::Binding>, StoreError> {
        Ok(None)
    }
}

/// Frame bytes one client moved, both ways (`cfg(test)` only: a build
/// without tests counts nothing and holds nothing).
#[derive(Clone, Default)]
pub(super) struct FrameCounter(#[cfg(test)] Arc<AtomicU64>);

impl FrameCounter {
    #[cfg_attr(not(test), allow(clippy::unused_self))]
    fn add(&self, _bytes: usize) {
        #[cfg(test)]
        self.0
            .fetch_add(u64::try_from(_bytes).unwrap_or(u64::MAX), Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(super) fn total(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(super) fn reset(&self) {
        self.0.store(0, Ordering::Relaxed);
    }
}

struct UnixIo {
    stream: UnixStream,
    counter: FrameCounter,
}

impl UnixIo {
    fn new(stream: UnixStream, counter: FrameCounter) -> Self {
        Self { stream, counter }
    }
}

impl JsonIo for UnixIo {
    fn read_value(&mut self, max_len: usize) -> Result<Incoming, StoreError> {
        let (value, len) = read_frame(&mut self.stream, max_len)?;
        self.counter.add(len);
        Ok(Incoming::Value(value))
    }

    fn write_value(&mut self, value: &Value) -> Result<(), StoreError> {
        let len = write_frame(&mut self.stream, value)?;
        self.counter.add(len);
        Ok(())
    }

    fn set_read_timeout(&self, timeout: Option<Duration>) -> Result<(), StoreError> {
        self.stream.set_read_timeout(timeout).map_err(backend)
    }

    fn shutdown_both(&self) {
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

struct WsIo {
    stream: Wire,
    client: bool,
    handshake: bool,
}

impl WsIo {
    fn client(stream: Wire) -> Self {
        Self {
            stream,
            client: true,
            handshake: false,
        }
    }

    fn server(stream: Wire, handshake: bool) -> Self {
        Self {
            stream,
            client: false,
            handshake,
        }
    }
}

impl JsonIo for WsIo {
    fn prepare(&mut self) -> Result<Vec<String>, StoreError> {
        if !self.handshake {
            return Ok(Vec::new());
        }
        self.handshake = false;
        // TLS first (`wss://`), then the upgrade inside it.
        self.stream.start()?;
        ws::accept(&mut self.stream)
    }

    fn set_read_timeout(&self, timeout: Option<Duration>) -> Result<(), StoreError> {
        self.stream.set_read_timeout(timeout).map_err(backend)
    }

    fn read_value(&mut self, max_len: usize) -> Result<Incoming, StoreError> {
        match ws::read_event(&mut self.stream, self.client, max_len)? {
            ws::WsEvent::Data(payload) => {
                if payload.is_empty() {
                    return Err(backend("frame length is outside 1..=16MiB"));
                }
                let text =
                    std::str::from_utf8(&payload).map_err(|_| backend("frame is not utf-8"))?;
                let value = serde_json::from_str(text).map_err(|_| backend("frame is not json"))?;
                Ok(Incoming::Value(value))
            }
            ws::WsEvent::Ping(payload) => Ok(Incoming::Ping(payload)),
            ws::WsEvent::Close => Ok(Incoming::Close),
        }
    }

    fn write_value(&mut self, value: &Value) -> Result<(), StoreError> {
        let bytes =
            serde_json::to_vec(value).map_err(|err| backend(format!("encode frame: {err}")))?;
        if bytes.is_empty() || bytes.len() > ws::MAX_PAYLOAD {
            return Err(backend("frame length is outside 1..=16MiB"));
        }
        ws::write_frame(&mut self.stream, self.client, 0x1, &bytes)
    }

    fn write_pong(&mut self, payload: &[u8]) -> Result<(), StoreError> {
        ws::write_frame(&mut self.stream, self.client, 0xA, payload)
    }

    fn shutdown_both(&self) {
        self.stream.shutdown_both();
    }

    fn channel_binding(&self) -> Result<Option<tls::Binding>, StoreError> {
        self.stream.channel_binding()
    }
}

/// Write one frame; the bytes it took, length prefix included.
pub(super) fn write_frame(stream: &mut UnixStream, value: &Value) -> Result<usize, StoreError> {
    let bytes = serde_json::to_vec(value).map_err(|err| backend(format!("encode frame: {err}")))?;
    let len = u32::try_from(bytes.len()).map_err(|_| backend("frame length exceeds u32"))?;
    if len == 0 || len > MAX_FRAME_BYTES {
        return Err(backend("frame length is outside 1..=16MiB"));
    }
    stream.write_all(&len.to_be_bytes()).map_err(backend)?;
    stream.write_all(&bytes).map_err(backend)?;
    Ok(bytes.len() + 4)
}

/// Read one frame, and the bytes it took, length prefix included. A frame
/// that declares more than `max_len` bytes (at most 16 MiB) is refused
/// before anything is allocated for it.
pub(super) fn read_frame(
    stream: &mut UnixStream,
    max_len: usize,
) -> Result<(Value, usize), StoreError> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).map_err(backend)?;
    let len = u32::from_be_bytes(len_buf);
    if len == 0 || len > MAX_FRAME_BYTES {
        return Err(backend("frame length is outside 1..=16MiB"));
    }
    if len as usize > max_len {
        return Err(ws::too_long(max_len));
    }
    let mut buf = vec![0u8; len as usize];
    stream.read_exact(&mut buf).map_err(backend)?;
    let text = std::str::from_utf8(&buf).map_err(|_| backend("frame is not utf-8"))?;
    let value = serde_json::from_str(text).map_err(|_| backend("frame is not json"))?;
    Ok((value, buf.len() + 4))
}

/// One client connection, shared by the handle and its transactions.
pub(super) struct RemoteShared {
    write: Mutex<Box<dyn JsonIo>>,
    pending: Mutex<HashMap<u64, Sender<Result<Value, StoreError>>>>,
    next_id: AtomicU64,
    subs: Mutex<HashMap<String, Vec<Arc<Sender<MqRecord>>>>>,
    #[cfg_attr(not(test), allow(dead_code))]
    counter: FrameCounter,
}

/// A call that got no reply in [`CALL_TIMEOUT`].
struct TimedOut;

impl RemoteShared {
    pub(super) fn call(&self, method: &str, params: Value) -> Result<Value, StoreError> {
        match self.call_once(method, &params) {
            Ok(result) => result,
            Err(TimedOut) => Err(backend("timed out waiting for the FerroStore server")),
        }
    }

    fn call_once(
        &self,
        method: &str,
        params: &Value,
    ) -> Result<Result<Value, StoreError>, TimedOut> {
        #[cfg(test)]
        crate::note_preparatory_rpc_attempt();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        lock_mutex(&self.pending).insert(id, tx);
        let request = json!({
            "id": id,
            "method": method,
            "params": params,
        });
        // Release the write lock before waiting. The reader must run, and a
        // response must not need this task to still hold the mutex.
        {
            let mut stream = lock_mutex(&self.write);
            if let Err(err) = stream.write_value(&request) {
                lock_mutex(&self.pending).remove(&id);
                return Ok(Err(err));
            }
        }
        match rx.recv_timeout(CALL_TIMEOUT) {
            Ok(result) => Ok(result),
            Err(RecvTimeoutError::Timeout) => {
                lock_mutex(&self.pending).remove(&id);
                Err(TimedOut)
            }
            Err(RecvTimeoutError::Disconnected) => Ok(Err(backend("FerroStore connection closed"))),
        }
    }

    /// A write `method` that runs once (spec §6.1): `params` gets a new
    /// request id. A request that times out is sent once more with the same
    /// id, so the server, which ran it or is running it, replies with the
    /// first result and applies nothing again.
    ///
    /// A refusal because the server's table of request ids is full
    /// ([`session::DEDUP_FULL`]) ran nothing: it is a lost attempt, sent
    /// again with the same id after the backoff of a lost commit
    /// ([`retry::Backoff`]), within [`retry::REMOTE_ATTEMPTS`] sends.
    pub(super) fn call_req(&self, method: &str, mut params: Value) -> Result<Value, StoreError> {
        if let Some(object) = params.as_object_mut() {
            object.insert("req".into(), Value::String(session::new_request_id()));
        }
        let mut backoff = retry::Backoff::new();
        let mut timeouts = 0;
        let mut refusals = 0;
        loop {
            match self.call_once(method, &params) {
                Ok(Err(err))
                    if session::is_dedup_full(&err) && refusals + 1 < retry::REMOTE_ATTEMPTS =>
                {
                    refusals += 1;
                    backoff.wait(refusals);
                }
                Ok(result) => return result,
                Err(TimedOut) => {
                    timeouts += 1;
                    if timeouts == 2 {
                        return Err(backend(
                            "timed out waiting for the FerroStore server, twice: the write may \
                             have committed",
                        ));
                    }
                }
            }
        }
    }

    /// Send `method` and do not wait: its reply is dropped when it comes.
    pub(super) fn send_only(&self, method: &str, params: Value) {
        #[cfg(test)]
        crate::note_preparatory_rpc_attempt();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = json!({
            "id": id,
            "method": method,
            "params": params,
        });
        let _ = lock_mutex(&self.write).write_value(&request);
    }
}

pub(super) struct RemoteClient {
    shared: Arc<RemoteShared>,
    reader: Mutex<Option<JoinHandle<()>>>,
}

impl RemoteClient {
    pub(super) fn call(&self, method: &str, params: Value) -> Result<Value, StoreError> {
        self.shared.call(method, params)
    }

    /// See [`RemoteShared::call_req`].
    pub(super) fn call_req(&self, method: &str, params: Value) -> Result<Value, StoreError> {
        self.shared.call_req(method, params)
    }

    pub(super) fn shared(&self) -> &Arc<RemoteShared> {
        &self.shared
    }

    /// The frame bytes this client moved.
    #[cfg(test)]
    pub(super) fn counter(&self) -> &FrameCounter {
        &self.shared.counter
    }

    /// The server's last committed sequence: a session's `base_seq`, and
    /// the session closed again.
    pub(super) fn last_seq(&self) -> Result<u64, StoreError> {
        let opened = self.call("open_session", json!({}))?;
        let session = opened.get("session").and_then(Value::as_u64);
        let base_seq = opened.get("base_seq").and_then(Value::as_u64);
        let (Some(session), Some(base_seq)) = (session, base_seq) else {
            return Err(backend(
                "open_session result is missing session or base_seq",
            ));
        };
        self.shared
            .send_only("close_session", json!({ "session": session }));
        Ok(base_seq)
    }

    /// `commit_txn` (spec §6.1) under a new request id. `Ok(None)` is lost:
    /// a read no longer has the value it had, and nothing was written.
    /// `Ok(Some(seq))` committed at `seq`.
    pub(super) fn commit_txn(
        &self,
        session: Option<(u64, u64)>,
        reads: &[(ReadKey, String)],
        ops: &[Op],
        silent: bool,
    ) -> Result<Option<u64>, StoreError> {
        let mut params = serde_json::Map::new();
        if let Some((session, base_seq)) = session {
            params.insert("session".into(), json!(session));
            params.insert("base_seq".into(), json!(base_seq));
        }
        let mut encoded = Vec::with_capacity(reads.len());
        for (key, hash) in reads {
            let key =
                serde_json::to_value(key).map_err(|err| backend(format!("encode read: {err}")))?;
            encoded.push(json!({ "read": key, "hash": hash }));
        }
        params.insert("reads".into(), Value::Array(encoded));
        params.insert(
            "ops".into(),
            Value::Array(ops.iter().map(op_value).collect()),
        );
        if silent {
            params.insert("silent".into(), Value::Bool(true));
        }
        let value = self.call_req("commit_txn", Value::Object(params))?;
        match value.get("committed").and_then(Value::as_bool) {
            Some(false) => Ok(None),
            Some(true) => value
                .get("seq")
                .and_then(Value::as_u64)
                .map(Some)
                .ok_or_else(|| backend("commit_txn committed ops but named no sequence")),
            None => Err(backend("commit_txn result has no `committed`")),
        }
    }

    pub(super) fn call_unit(&self, method: &str, params: Value) -> Result<(), StoreError> {
        let _ = self.call(method, params)?;
        Ok(())
    }

    /// JSON null is a missing document. A present document, including JSON null,
    /// is `{"doc": value}`.
    pub(super) fn call_option(
        &self,
        method: &str,
        params: Value,
    ) -> Result<Option<Value>, StoreError> {
        let value = self.call(method, params)?;
        if value.is_null() {
            return Ok(None);
        }
        let doc = value
            .get("doc")
            .cloned()
            .ok_or_else(|| backend("result is neither null nor a document"))?;
        Ok(Some(doc))
    }

    pub(super) fn call_strings(
        &self,
        method: &str,
        params: Value,
    ) -> Result<Vec<String>, StoreError> {
        let value = self.call(method, params)?;
        let items = value
            .as_array()
            .ok_or_else(|| backend("result is not an array"))?;
        items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(ToString::to_string)
                    .ok_or_else(|| backend("result id is not a string"))
            })
            .collect()
    }

    pub(super) fn call_pairs(
        &self,
        method: &str,
        params: Value,
    ) -> Result<Vec<(String, f64)>, StoreError> {
        let value = self.call(method, params)?;
        let items = value
            .as_array()
            .ok_or_else(|| backend("result is not an array"))?;
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            let pair = item
                .as_array()
                .ok_or_else(|| backend("search hit is not a pair"))?;
            if pair.len() != 2 {
                return Err(backend("search hit is not a pair"));
            }
            let id = pair[0]
                .as_str()
                .ok_or_else(|| backend("search hit id is not a string"))?
                .to_string();
            let score = pair[1]
                .as_f64()
                .ok_or_else(|| backend("search hit score is not a number"))?;
            out.push((id, score));
        }
        Ok(out)
    }

    pub(super) fn call_records(
        &self,
        method: &str,
        params: Value,
    ) -> Result<Vec<MqRecord>, StoreError> {
        let value = self.call(method, params)?;
        let items = value
            .as_array()
            .ok_or_else(|| backend("result is not an array"))?;
        items.iter().map(parse_record).collect()
    }

    pub(super) fn apply_sync_batches(
        &self,
        batches: &[SyncBatch],
        silent: bool,
    ) -> Result<u64, StoreError> {
        let encoded = serde_json::to_value(batches)
            .map_err(|err| backend(format!("encode sync batches: {err}")))?;
        let mut params = serde_json::Map::new();
        params.insert("batches".into(), encoded);
        if silent {
            params.insert("silent".into(), Value::Bool(true));
        }
        let value = self.call_req("apply_sync_batches", Value::Object(params))?;
        value
            .as_u64()
            .ok_or_else(|| backend("apply_sync_batches result is not a u64"))
    }

    pub(super) fn subscribe_topic(&self, topic: &str) -> Receiver<MqRecord> {
        let (tx, rx) = mpsc::channel();
        let tx = Arc::new(tx);
        lock_mutex(&self.shared.subs)
            .entry(topic.to_string())
            .or_default()
            .push(Arc::clone(&tx));
        // Registered first so a push that arrives with the response is
        // delivered. A failed RPC drops this sender, so recv is Disconnected.
        if self.call("subscribe_mq", json!({"topic": topic})).is_err() {
            remove_subscriber(&self.shared, topic, &tx);
        }
        rx
    }
}

fn remove_subscriber(shared: &RemoteShared, topic: &str, tx: &Arc<Sender<MqRecord>>) {
    let mut subs = lock_mutex(&shared.subs);
    let Some(senders) = subs.get_mut(topic) else {
        return;
    };
    senders.retain(|sender| !Arc::ptr_eq(sender, tx));
    if senders.is_empty() {
        subs.remove(topic);
    }
}

impl Drop for RemoteClient {
    fn drop(&mut self) {
        let stream = lock_mutex(&self.shared.write);
        stream.shutdown_both();
        drop(stream);
        if let Some(handle) = lock_mutex(&self.reader).take() {
            let _ = handle.join();
        }
    }
}

/// A client of the Unix socket at `path` with no credential: the server
/// checks the peer's uid.
#[cfg(test)]
pub(super) fn connect_unix(path: &str) -> Result<RemoteClient, StoreError> {
    connect_unix_with(path, &auth::ClientCred::None)
}

/// A client of the Unix socket at `path`, authenticating with `cred`.
pub(super) fn connect_unix_with(
    path: &str,
    cred: &auth::ClientCred,
) -> Result<RemoteClient, StoreError> {
    if cred.bearer().is_some() {
        return Err(backend(
            "a bearer token is for ws:// and wss://; a Unix socket takes the peer uid and, optionally, a PSK",
        ));
    }
    let stream =
        UnixStream::connect(path).map_err(|err| backend(format!("connect {path}: {err}")))?;
    client_from_unix(stream, cred)
}

pub(super) fn client_from_unix(
    stream: UnixStream,
    cred: &auth::ClientCred,
) -> Result<RemoteClient, StoreError> {
    let writer = stream
        .try_clone()
        .map_err(|err| backend(format!("clone unix stream: {err}")))?;
    let counter = FrameCounter::default();
    finish_client(
        Box::new(UnixIo::new(stream, counter.clone())),
        Box::new(UnixIo::new(writer, counter.clone())),
        counter,
        cred,
    )
}

/// A client of the `ws://` endpoint `rest`, authenticating with `cred`.
/// A credential goes to an address that is not loopback only when
/// `allow_plaintext` ([`auth::ClientCred::check_plaintext`]).
#[cfg(test)]
pub(super) fn connect_ws(
    rest: &str,
    cred: &auth::ClientCred,
    allow_plaintext: bool,
) -> Result<RemoteClient, StoreError> {
    connect_tcp(rest, cred, allow_plaintext, None)
}

/// A client of the `ws://` endpoint `rest` (`verify` is `None`) or the
/// `wss://` endpoint `rest` (the server checked by `verify`),
/// authenticating with `cred`. Over `wss://` a credential goes to any
/// host, inside TLS; over `ws://` see [`connect_ws`].
pub(super) fn connect_tcp(
    rest: &str,
    cred: &auth::ClientCred,
    allow_plaintext: bool,
    verify: Option<&tls::Verify>,
) -> Result<RemoteClient, StoreError> {
    let (read, write) = dial_wire(rest, cred.bearer(), verify, |peer| {
        cred.check_plaintext(peer, allow_plaintext)
    })?;
    finish_client(
        Box::new(WsIo::client(read)),
        Box::new(WsIo::client(write)),
        FrameCounter::default(),
        cred,
    )
}

/// Connect to `rest` (`host:port[/path]`), run TLS when `verify` names how
/// to check the server, and upgrade: the reading and the writing half.
/// Without TLS `admit` may refuse the address reached before anything is
/// written; with TLS a credential may go anywhere, and it is not asked.
pub(super) fn dial_wire(
    rest: &str,
    bearer: Option<&str>,
    verify: Option<&tls::Verify>,
    admit: impl FnOnce(SocketAddr) -> Result<(), StoreError>,
) -> Result<(Wire, Wire), StoreError> {
    let scheme = if verify.is_some() { "wss" } else { "ws" };
    let (host_port, path) = ws::split_target(scheme, rest)?;
    ws::check_bearer(bearer)?;
    // Checked before the connect, so no bad name or file costs one.
    let tls_setup = match verify {
        Some(verify) => Some((tls::client_config(verify)?, tls::server_name(host_port)?)),
        None => None,
    };
    let (stream, peer) = ws::dial(host_port)?;
    // The TLS handshake and the upgrade are bounded like any handshake read.
    stream
        .set_read_timeout(Some(auth::HANDSHAKE_TIMEOUT))
        .map_err(|err| backend(format!("set read timeout: {err}")))?;
    let (mut read, write) = match tls_setup {
        None => {
            admit(peer)?;
            let write = stream
                .try_clone()
                .map_err(|err| backend(format!("clone tcp stream: {err}")))?;
            (Wire::Plain(stream), Wire::Plain(write))
        }
        Some((config, name)) => {
            let (read, write) = TlsStream::client(stream, config, name)?;
            (Wire::Tls(read), Wire::Tls(write))
        }
    };
    if let Err(err) = ws::upgrade(&mut read, host_port, &path, bearer) {
        read.shutdown_both();
        return Err(err);
    }
    Ok((read, write))
}

/// Authenticate (spec §6.2), then start the reader. No request is sent
/// before the server accepted the connection and, for a PSK, proved itself.
fn finish_client(
    mut read: Box<dyn JsonIo>,
    mut write: Box<dyn JsonIo>,
    counter: FrameCounter,
    cred: &auth::ClientCred,
) -> Result<RemoteClient, StoreError> {
    read.set_read_timeout(Some(auth::HANDSHAKE_TIMEOUT))?;
    if let Err(err) = auth::client_handshake(&mut *read, &mut *write, cred) {
        write.shutdown_both();
        return Err(err);
    }
    read.set_read_timeout(None)?;
    let shared = Arc::new(RemoteShared {
        write: Mutex::new(write),
        pending: Mutex::new(HashMap::new()),
        next_id: AtomicU64::new(1),
        subs: Mutex::new(HashMap::new()),
        counter,
    });
    let reader_shared = Arc::clone(&shared);
    let reader = thread::Builder::new()
        .name("fls-client".to_string())
        .spawn(move || reader_loop(read, reader_shared))
        .map_err(|err| backend(format!("start client reader: {err}")))?;
    Ok(RemoteClient {
        shared,
        reader: Mutex::new(Some(reader)),
    })
}

fn reader_loop(mut stream: Box<dyn JsonIo>, shared: Arc<RemoteShared>) {
    loop {
        match stream.read_value(ws::MAX_PAYLOAD) {
            Ok(Incoming::Value(frame)) => {
                if frame.get("id").is_none()
                    && frame.get("method").and_then(Value::as_str) == Some("mq")
                {
                    deliver_push(&shared, &frame);
                    continue;
                }
                let Some(id) = frame.get("id").and_then(Value::as_u64) else {
                    continue;
                };
                let outcome = if frame.get("ok").and_then(Value::as_bool) == Some(true) {
                    Ok(frame.get("result").cloned().unwrap_or(Value::Null))
                } else {
                    Err(response_error(&frame))
                };
                if let Some(sender) = lock_mutex(&shared.pending).remove(&id) {
                    let _ = sender.send(outcome);
                }
            }
            Ok(Incoming::Ping(payload)) => {
                let mut write = lock_mutex(&shared.write);
                if write.write_pong(&payload).is_err() {
                    break;
                }
            }
            Ok(Incoming::Close) | Err(_) => break,
        }
    }
    fail_pending(&shared, "FerroStore connection closed");
    // Drop live subscription senders so an already-open recv sees Disconnected.
    let _ = std::mem::take(&mut *lock_mutex(&shared.subs));
}

fn deliver_push(shared: &RemoteShared, frame: &Value) {
    let Some(params) = frame.get("params") else {
        return;
    };
    let Some(topic) = params.get("topic").and_then(Value::as_str) else {
        return;
    };
    let Some(record_value) = params.get("record") else {
        return;
    };
    let Ok(record) = parse_record(record_value) else {
        return;
    };
    let mut subs = lock_mutex(&shared.subs);
    let Some(senders) = subs.get_mut(topic) else {
        return;
    };
    senders.retain(|sender| sender.send(record.clone()).is_ok());
    if senders.is_empty() {
        subs.remove(topic);
    }
}

#[cfg(test)]
mod empty_frame {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn empty_websocket_data_frame_is_outside_the_json_limit_and_empty_ping_is_a_ping() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let mut client = TcpStream::connect(addr).unwrap();
        let (server, _) = listener.accept().unwrap();
        let mut io = WsIo::server(Wire::Plain(server), false);

        ws::write_frame(&mut client, true, 0x1, &[]).unwrap();
        let msg = match io.read_value(ws::MAX_PAYLOAD) {
            Err(err) => err.to_string(),
            Ok(Incoming::Ping(_)) => panic!("empty data frame was a ping"),
            Ok(Incoming::Value(_)) => panic!("empty data frame was accepted"),
            Ok(Incoming::Close) => panic!("empty data frame was a close"),
        };
        assert!(msg.contains("frame length is outside 1..=16MiB"), "{msg}");
        assert!(!msg.contains("frame is not json"), "{msg}");

        ws::write_frame(&mut client, true, 0xA, &[]).unwrap();
        ws::write_frame(&mut client, true, 0x9, &[]).unwrap();
        match io.read_value(ws::MAX_PAYLOAD).unwrap() {
            Incoming::Ping(payload) => assert!(payload.is_empty()),
            Incoming::Value(_) => panic!("empty ping was parsed as json"),
            Incoming::Close => panic!("empty ping was a close"),
        }
    }
}

fn parse_record(value: &Value) -> Result<MqRecord, StoreError> {
    let seq = value
        .get("seq")
        .and_then(Value::as_u64)
        .ok_or_else(|| backend("mq record is missing seq"))?;
    let key = value
        .get("key")
        .and_then(Value::as_str)
        .ok_or_else(|| backend("mq record is missing key"))?
        .to_string();
    let payload = value.get("payload").cloned().unwrap_or(Value::Null);
    Ok(MqRecord { seq, key, payload })
}

fn fail_pending(shared: &RemoteShared, message: &str) {
    let pending = std::mem::take(&mut *lock_mutex(&shared.pending));
    for (_, sender) in pending {
        let _ = sender.send(Err(backend(message)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stalled_maintenance_does_not_block_new_connections() {
        struct StallingProbe {
            block: AtomicBool,
            entered: Mutex<Option<Sender<()>>>,
            release: (Mutex<bool>, std::sync::Condvar),
        }
        impl super::super::MqResourceProbe for StallingProbe {
            fn probe(&self, _: &Path) -> super::super::MqResourceSnapshot {
                if self.block.load(Ordering::SeqCst) {
                    if let Some(entered) = lock_mutex(&self.entered).take() {
                        let _ = entered.send(());
                    }
                    let mut released = lock_mutex(&self.release.0);
                    while !*released {
                        released = self.release.1.wait(released).unwrap();
                    }
                }
                super::super::MqResourceSnapshot {
                    available_ram_bytes: Some(u64::MAX),
                    free_disk_bytes: Some(u64::MAX),
                }
            }
        }
        let (entered_tx, entered_rx) = mpsc::channel();
        let probe = Arc::new(StallingProbe {
            block: AtomicBool::new(false),
            entered: Mutex::new(Some(entered_tx)),
            release: (Mutex::new(false), std::sync::Condvar::new()),
        });
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("stalled.sock");
        let store = FerroStore::in_memory();
        store
            .configure_mq(super::super::MqTierPolicy::default(), false, probe.clone())
            .unwrap();
        store
            .publish_mq_with("events", "one", &json!(1), MqDurability::Adaptive)
            .unwrap();
        let server = FerroStoreServer::bind_unix(&socket, store).unwrap();
        probe.block.store(true, Ordering::SeqCst);
        if entered_rx.recv_timeout(Duration::from_secs(8)).is_err() {
            *lock_mutex(&probe.release.0) = true;
            probe.release.1.notify_all();
            panic!("maintenance did not start");
        }
        let (connected_tx, connected_rx) = mpsc::channel();
        let socket_name = socket.display().to_string();
        let client = thread::spawn(move || {
            let _ = connected_tx.send(connect_unix(&socket_name).is_ok());
        });
        let connected = connected_rx.recv_timeout(Duration::from_secs(1));
        *lock_mutex(&probe.release.0) = true;
        probe.release.1.notify_all();
        client.join().unwrap();
        drop(server);
        assert_eq!(connected, Ok(true), "maintenance blocked the accept loop");
    }

    #[test]
    fn idle_server_spills_expired_adaptive_messages() {
        struct Resources;
        impl super::super::MqResourceProbe for Resources {
            fn probe(&self, _: &Path) -> super::super::MqResourceSnapshot {
                super::super::MqResourceSnapshot {
                    available_ram_bytes: Some(u64::MAX),
                    free_disk_bytes: Some(u64::MAX),
                }
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("idle.sock");
        let store = FerroStore::in_memory();
        let policy = super::super::MqTierPolicy {
            max_memory_lag: Duration::from_millis(100),
            ..Default::default()
        };
        store
            .configure_mq(policy, false, Arc::new(Resources))
            .unwrap();
        let server = FerroStoreServer::bind_unix(&socket, store).unwrap();
        let client = FerroStore::connect_uri(&format!("unix://{}", socket.display())).unwrap();
        client
            .publish_mq_with("idle", "key", &json!(1), MqDurability::Adaptive)
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            if server.store().get_doc("mq:idle", "1").unwrap().is_some() {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        panic!("idle adaptive message was not spilled by the server timer");
    }

    #[test]
    fn remote_maintenance_replays_one_spill_for_the_same_request_id() {
        struct PressureProbe(AtomicBool);
        impl super::super::MqResourceProbe for PressureProbe {
            fn probe(&self, _: &Path) -> super::super::MqResourceSnapshot {
                super::super::MqResourceSnapshot {
                    available_ram_bytes: Some(if self.0.load(Ordering::SeqCst) {
                        0
                    } else {
                        u64::MAX
                    }),
                    free_disk_bytes: Some(u64::MAX),
                }
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("queue.sock");
        let probe = Arc::new(PressureProbe(AtomicBool::new(false)));
        let store = FerroStore::in_memory();
        store
            .configure_mq(super::super::MqTierPolicy::default(), false, probe.clone())
            .unwrap();
        let server = FerroStoreServer::bind_unix(&sock, store).unwrap();
        let client = connect_unix(&sock.display().to_string()).unwrap();
        let published = client
            .call_req(
                "publish_mq_v2",
                json!({"topic":"events","key":"a","payload":1,"durability":"adaptive"}),
            )
            .unwrap();
        assert!(published["store_seq"].is_null());
        let before = server.store().stats().unwrap().last_seq;
        probe.0.store(true, Ordering::SeqCst);
        let request = json!({"req":"01900000000070008000000000000002"});
        let first = client.call("maintain_mq", request.clone()).unwrap();
        let again = client.call("maintain_mq", request).unwrap();
        assert_eq!(first, json!({"flushed":1}));
        assert_eq!(again, first);
        assert_eq!(server.store().stats().unwrap().last_seq, before + 1);
        assert_eq!(
            client
                .call(
                    "poll_mq_group",
                    json!({"topic":"events","group":"g","limit":10})
                )
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn remote_adapter_cannot_maintain_another_clients_buffered_work() {
        struct Probe(AtomicBool);
        impl super::super::MqResourceProbe for Probe {
            fn probe(&self, _: &Path) -> super::super::MqResourceSnapshot {
                super::super::MqResourceSnapshot {
                    available_ram_bytes: Some(if self.0.load(Ordering::SeqCst) {
                        0
                    } else {
                        u64::MAX
                    }),
                    free_disk_bytes: Some(u64::MAX),
                }
            }
        }
        #[derive(Default)]
        struct Sink(AtomicUsize);
        impl super::super::SyncAdapter for Sink {
            fn name(&self) -> &str {
                "maintenance-test"
            }
            fn on_commit(&self, _: &SyncBatch) -> Result<(), StoreError> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("queue.sock");
        let probe = Arc::new(Probe(AtomicBool::new(false)));
        let store = FerroStore::in_memory();
        store
            .configure_mq(super::super::MqTierPolicy::default(), false, probe.clone())
            .unwrap();
        let server = FerroStoreServer::bind_unix(&sock, store).unwrap();
        let uri = format!("unix://{}", sock.display());
        let sender = FerroStore::connect_uri(&uri).unwrap();
        let adapter_client = FerroStore::connect_uri(&uri).unwrap();
        let adapter = Arc::new(Sink::default());
        adapter_client
            .attach_sync_adapter(super::super::SyncMode::WriteThrough, adapter.clone())
            .unwrap();
        let seq = sender
            .publish_mq_with("events", "a", &json!(1), MqDurability::Adaptive)
            .unwrap();
        let before = server.store().stats().unwrap().last_seq;
        probe.0.store(true, Ordering::SeqCst);
        assert!(adapter_client.maintain_mq().is_err());
        assert_eq!(server.store().stats().unwrap().last_seq, before);
        assert_eq!(adapter.0.load(Ordering::SeqCst), 0);
        assert_eq!(sender.poll_mq_group("events", "g", 10).unwrap()[0].seq, seq);
        assert_eq!(sender.maintain_mq().unwrap(), 1);
    }

    #[test]
    fn legacy_publish_rpc_accepts_optional_durability_without_changing_default_response() {
        struct Resources;
        impl super::super::MqResourceProbe for Resources {
            fn probe(&self, _: &Path) -> super::super::MqResourceSnapshot {
                super::super::MqResourceSnapshot {
                    available_ram_bytes: Some(u64::MAX),
                    free_disk_bytes: Some(u64::MAX),
                }
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("q.sock");
        let store = FerroStore::in_memory();
        store
            .configure_mq(
                super::super::MqTierPolicy::default(),
                false,
                Arc::new(Resources),
            )
            .unwrap();
        let _server = FerroStoreServer::bind_unix(&sock, store).unwrap();
        let client = connect_unix(&sock.display().to_string()).unwrap();
        let durable = client
            .call_req(
                "publish_mq",
                json!({"topic":"events","key":"d","payload":1}),
            )
            .unwrap();
        assert_eq!(durable["seq"], 1);
        assert!(durable["store_seq"].is_u64());
        assert!(durable.get("tier").is_none());
        let buffered = client
            .call_req(
                "publish_mq",
                json!({"topic":"events","key":"a","payload":2,"durability":"adaptive"}),
            )
            .unwrap();
        assert_eq!(buffered["seq"], 2);
        assert!(buffered["store_seq"].is_null());
        assert_eq!(buffered["tier"], "memory");
        let ephemeral = client
            .call_req(
                "publish_mq",
                json!({"topic":"temporary","key":"e","payload":4,"durability":"ephemeral"}),
            )
            .unwrap();
        assert_eq!(ephemeral["seq"], 1);
        assert!(ephemeral["store_seq"].is_null());
        assert_eq!(ephemeral["tier"], "memory");
        assert!(client.call_req("publish_mq", json!({"topic":"events","key":"unsafe","payload":3,"durability":"ephemeral","require_single_publish":true})).is_err());
        assert!(client.call_req("publish_mq", json!({"topic":"events","key":"unsafe","payload":3,"durability":"durable","require_single_publish":true})).is_err());
        assert!(
            client
                .call_req(
                    "publish_mq",
                    json!({"topic":"events","key":"unsafe","payload":3,"durability":"unknown"})
                )
                .is_err()
        );
        let all = client
            .call(
                "poll_mq_group",
                json!({"topic":"events","group":"g","limit":10}),
            )
            .unwrap();
        assert_eq!(all.as_array().unwrap().len(), 2);
    }

    #[test]
    fn tiered_mq_rpc_preserves_legacy_framing_and_flushes_after_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("queue.fls");
        let sock = dir.path().join("queue.sock");
        let mut server =
            FerroStoreServer::bind_unix(&sock, FerroStore::open(&file).unwrap()).unwrap();
        let client = connect_unix(&sock.display().to_string()).unwrap();
        let durable = client
            .call_req(
                "publish_mq",
                json!({"topic":"events","key":"d","payload":1}),
            )
            .unwrap();
        assert_eq!(durable["seq"], 1);
        assert!(durable["store_seq"].is_u64());
        assert!(durable.get("tier").is_none());
        let adaptive = client
            .call_req(
                "publish_mq_v2",
                json!({"topic":"events","key":"a","payload":2,"durability":"adaptive"}),
            )
            .unwrap();
        assert_eq!(adaptive["seq"], 2);
        assert!(adaptive["store_seq"].is_null());
        assert_eq!(adaptive["tier"], "memory");
        let telemetry = client
            .call("mq_telemetry", json!({"topic":"events"}))
            .unwrap();
        let topic = telemetry["topics"].as_array().unwrap().first().unwrap();
        assert_eq!(topic["topic"], "events");
        assert_eq!(topic["memory_messages"], 1);
        assert_eq!(topic["active_tier"], "memory");
        let v2 = client
            .call_req("ack_mq_v2", json!({"topic":"events","group":"g","seq":2}))
            .unwrap();
        assert!(v2["store_seq"].is_null());
        let old = client
            .call_req("ack_mq", json!({"topic":"events","group":"g","seq":1}))
            .unwrap();
        assert!(old.is_null());
        drop(client);
        assert_eq!(server.shutdown().unwrap(), 1);
        drop(server);
        let reopened = FerroStore::open(&file).unwrap();
        let other = reopened.poll_mq_group("events", "other", 10).unwrap();
        assert_eq!(
            other.iter().map(|record| record.seq).collect::<Vec<_>>(),
            [1, 2]
        );
        assert!(
            reopened
                .poll_mq_group("events", "g", 10)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn shutdown_reports_a_failed_flush_and_allows_retry_without_losing_buffer() {
        use std::sync::atomic::AtomicBool;
        struct DiskProbe(AtomicBool);
        impl super::super::MqResourceProbe for DiskProbe {
            fn probe(&self, _path: &Path) -> super::super::MqResourceSnapshot {
                super::super::MqResourceSnapshot {
                    available_ram_bytes: Some(u64::MAX),
                    free_disk_bytes: Some(if self.0.load(Ordering::SeqCst) {
                        u64::MAX
                    } else {
                        0
                    }),
                }
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("queue.fls");
        let sock = dir.path().join("queue.sock");
        let probe = Arc::new(DiskProbe(AtomicBool::new(true)));
        let store = FerroStore::open(&file).unwrap();
        store
            .configure_mq(super::super::MqTierPolicy::default(), false, probe.clone())
            .unwrap();
        let mut server = FerroStoreServer::bind_unix(&sock, store).unwrap();
        let client = connect_unix(&sock.display().to_string()).unwrap();
        client
            .call_req(
                "publish_mq_v2",
                json!({"topic":"events","key":"a","payload":2,"durability":"adaptive"}),
            )
            .unwrap();
        probe.0.store(false, Ordering::SeqCst);
        assert!(server.shutdown().is_err());
        assert_eq!(
            server
                .store()
                .poll_mq_group("events", "g", 10)
                .unwrap()
                .len(),
            1
        );
        probe.0.store(true, Ordering::SeqCst);
        assert_eq!(server.shutdown().unwrap(), 1);
        drop(client);
        drop(server);
        let reopened = FerroStore::open(&file).unwrap();
        assert_eq!(reopened.poll_mq_group("events", "g", 10).unwrap().len(), 1);
    }

    #[test]
    fn optional_topic_rejects_invalid_shapes() {
        assert_eq!(optional_topic(&json!({})).unwrap(), None);
        assert_eq!(optional_topic(&json!({"topic":null})).unwrap(), None);
        assert_eq!(
            optional_topic(&json!({"topic":"events"})).unwrap(),
            Some("events")
        );
        assert!(optional_topic(&json!({"topic":7})).is_err());
    }

    #[test]
    fn repeated_adaptive_publish_request_replays_the_original_sequence() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("queue.sock");
        let _server = FerroStoreServer::bind_unix(&sock, FerroStore::in_memory()).unwrap();
        let client = connect_unix(&sock.display().to_string()).unwrap();
        let req = json!({"req":"01900000000070008000000000000001","topic":"events","key":"a","payload":1,"durability":"adaptive"});
        let first = client.call("publish_mq_v2", req.clone()).unwrap();
        let again = client.call("publish_mq_v2", req).unwrap();
        assert_eq!(first, again);
        let records = client
            .call(
                "poll_mq_group",
                json!({"topic":"events","group":"g","limit":10}),
            )
            .unwrap();
        assert_eq!(records.as_array().unwrap().len(), 1);
    }

    #[test]
    fn remote_attached_adapter_uses_durable_ack_for_another_clients_buffered_record() {
        struct Resources;
        impl super::super::MqResourceProbe for Resources {
            fn probe(&self, _path: &Path) -> super::super::MqResourceSnapshot {
                super::super::MqResourceSnapshot {
                    available_ram_bytes: Some(u64::MAX),
                    free_disk_bytes: Some(u64::MAX),
                }
            }
        }
        #[derive(Default)]
        struct Counter(AtomicUsize);
        impl super::super::SyncAdapter for Counter {
            fn name(&self) -> &str {
                "test-counter"
            }
            fn on_commit(&self, _batch: &SyncBatch) -> Result<(), StoreError> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("queue.sock");
        let store = FerroStore::in_memory();
        store
            .configure_mq(
                super::super::MqTierPolicy::default(),
                false,
                Arc::new(Resources),
            )
            .unwrap();
        let _server = FerroStoreServer::bind_unix(&sock, store).unwrap();
        let uri = format!("unix://{}", sock.display());
        let sender = FerroStore::connect_uri(&uri).unwrap();
        let consumer = FerroStore::connect_uri(&uri).unwrap();
        let adapter = Arc::new(Counter::default());
        consumer
            .attach_sync_adapter(super::super::SyncMode::WriteThrough, adapter.clone())
            .unwrap();
        let seq = sender
            .publish_mq_with("events", "a", &json!(1), MqDurability::Adaptive)
            .unwrap();
        consumer.ack_mq("events", "g", seq).unwrap();
        assert_eq!(
            adapter.0.load(Ordering::SeqCst),
            1,
            "ACK must expose a real durable sync batch"
        );
        assert!(
            consumer
                .poll_mq_group("events", "g", 10)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            consumer.poll_mq_group("events", "other", 10).unwrap().len(),
            1
        );
    }

    #[test]
    fn remote_adapter_durable_publish_refuses_another_clients_buffered_topic() {
        struct Resources;
        impl super::super::MqResourceProbe for Resources {
            fn probe(&self, _path: &Path) -> super::super::MqResourceSnapshot {
                super::super::MqResourceSnapshot {
                    available_ram_bytes: Some(u64::MAX),
                    free_disk_bytes: Some(u64::MAX),
                }
            }
        }
        #[derive(Default)]
        struct Counter(AtomicUsize);
        impl super::super::SyncAdapter for Counter {
            fn name(&self) -> &str {
                "test-counter"
            }
            fn on_commit(&self, _batch: &SyncBatch) -> Result<(), StoreError> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("queue.sock");
        let store = FerroStore::in_memory();
        store
            .configure_mq(
                super::super::MqTierPolicy::default(),
                false,
                Arc::new(Resources),
            )
            .unwrap();
        let server = FerroStoreServer::bind_unix(&sock, store).unwrap();
        let uri = format!("unix://{}", sock.display());
        let sender = FerroStore::connect_uri(&uri).unwrap();
        let client = FerroStore::connect_uri(&uri).unwrap();
        let adapter = Arc::new(Counter::default());
        client
            .attach_sync_adapter(super::super::SyncMode::WriteThrough, adapter.clone())
            .unwrap();
        let first = sender
            .publish_mq_with("events", "a", &json!(1), MqDurability::Adaptive)
            .unwrap();
        let before = server.store().stats().unwrap().last_seq;
        assert!(client.publish_mq("events", "b", &json!(2)).is_err());
        assert_eq!(server.store().stats().unwrap().last_seq, before);
        assert_eq!(adapter.0.load(Ordering::SeqCst), 0);
        assert_eq!(
            sender.poll_mq_group("events", "g", 10).unwrap()[0].seq,
            first
        );
        sender.flush_mq(Some("events")).unwrap();
        assert_eq!(
            client.publish_mq("events", "b", &json!(2)).unwrap(),
            first + 1
        );
        assert_eq!(adapter.0.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn remote_adapter_checkpoint_refuses_buffered_work_then_succeeds_after_flush() {
        struct Resources;
        impl super::super::MqResourceProbe for Resources {
            fn probe(&self, _path: &Path) -> super::super::MqResourceSnapshot {
                super::super::MqResourceSnapshot {
                    available_ram_bytes: Some(u64::MAX),
                    free_disk_bytes: Some(u64::MAX),
                }
            }
        }
        struct Sink;
        impl super::super::SyncAdapter for Sink {
            fn name(&self) -> &str {
                "test-sink"
            }
            fn on_commit(&self, _batch: &SyncBatch) -> Result<(), StoreError> {
                Ok(())
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("queue.sock");
        let store = FerroStore::in_memory();
        store
            .configure_mq(
                super::super::MqTierPolicy::default(),
                false,
                Arc::new(Resources),
            )
            .unwrap();
        let server = FerroStoreServer::bind_unix(&sock, store).unwrap();
        let uri = format!("unix://{}", sock.display());
        let sender = FerroStore::connect_uri(&uri).unwrap();
        let guarded = FerroStore::connect_uri(&uri).unwrap();
        guarded
            .attach_sync_adapter(super::super::SyncMode::WriteThrough, Arc::new(Sink))
            .unwrap();
        let seq = sender
            .publish_mq_with("events", "a", &json!(1), MqDurability::Adaptive)
            .unwrap();
        let before = server.store().stats().unwrap().last_seq;
        assert!(guarded.checkpoint().is_err());
        assert_eq!(server.store().stats().unwrap().last_seq, before);
        assert_eq!(sender.poll_mq_group("events", "g", 10).unwrap()[0].seq, seq);
        sender.flush_mq(None).unwrap();
        guarded.checkpoint().unwrap();
    }

    #[test]
    fn ephemeral_publish_to_an_old_server_never_falls_back_to_legacy_publish() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("old.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let (seen_tx, seen_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut wire = UnixIo::new(stream, FrameCounter::default());
            wire.write_value(&json!({"auth":"ok","scope":"admin"}))
                .unwrap();
            let Incoming::Value(request) = wire.read_value(MAX_FRAME_BYTES as usize).unwrap()
            else {
                panic!("expected request")
            };
            let method = request["method"].as_str().unwrap().to_string();
            seen_tx.send(method).unwrap();
            wire.write_value(
                &json!({"id":request["id"],"ok":false,"error":"no such method `publish_mq_v2`"}),
            )
            .unwrap();
            if let Ok(Incoming::Value(next)) = wire.read_value(MAX_FRAME_BYTES as usize) {
                seen_tx
                    .send(next["method"].as_str().unwrap().to_string())
                    .unwrap();
            }
        });
        let client = FerroStore::connect_uri(&format!("unix://{}", sock.display())).unwrap();
        let error = client
            .publish_mq_with("events", "secret", &json!(1), MqDurability::Ephemeral)
            .unwrap_err();
        assert!(error.to_string().contains("no such method"), "{error}");
        drop(client);
        worker.join().unwrap();
        let requests: Vec<_> = seen_rx.try_iter().collect();
        assert_eq!(requests, ["publish_mq_v2"]);
    }

    #[test]
    fn unknown_method_errors_and_the_connection_stays_up() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("store.sock");
        let server = FerroStoreServer::bind_unix(&sock, FerroStore::in_memory()).unwrap();
        let client = connect_unix(&sock.display().to_string()).unwrap();
        let err = client.call("nope", json!({})).unwrap_err();
        assert!(
            matches!(err, StoreError::Backend(ref message) if message.contains("nope")),
            "{err}"
        );
        client
            .call(
                "put_doc",
                json!({"collection": "docs", "id": "a", "doc": {"n": 1}}),
            )
            .unwrap();
        assert_eq!(
            client
                .call("get_doc", json!({"collection": "docs", "id": "a"}))
                .unwrap(),
            json!({"doc": {"n": 1}})
        );
        drop(client);
        drop(server);
    }

    #[test]
    fn ops_round_trip_column_tables_and_a_null_document() {
        use super::columnar::{self, ColumnDef, ColumnType, ColumnValue};
        let columns = vec![ColumnDef {
            name: "n".into(),
            ty: ColumnType::Int64,
        }];
        let blob = columnar::encode_row_group(
            &columns,
            &[
                vec![ColumnValue::Int64(3)],
                vec![ColumnValue::Null],
                vec![ColumnValue::Int64(4)],
            ],
        )
        .unwrap();

        let ops = [
            Op::CreateColumnTable {
                name: "t".into(),
                columns: columns.clone(),
            },
            Op::AppendColumnGroup {
                name: "t".into(),
                bytes: blob.clone(),
            },
            Op::Put {
                collection: "c".into(),
                id: "id".into(),
                doc: Value::Null,
            },
            Op::Delete {
                collection: "c".into(),
                id: "gone".into(),
            },
        ];
        let encoded = Value::Array(ops.iter().map(op_value).collect());
        let again = parse_ops(&encoded).unwrap();
        match &again[..] {
            [
                Op::CreateColumnTable {
                    name,
                    columns: cols,
                },
                Op::AppendColumnGroup {
                    name: appended,
                    bytes,
                },
                Op::Put { doc, .. },
                Op::Delete { id, .. },
            ] => {
                assert_eq!(name, "t");
                assert_eq!(cols, &columns);
                assert_eq!(appended, "t");
                assert_eq!(bytes, &blob);
                assert_eq!(doc, &Value::Null);
                assert_eq!(id, "gone");
            }
            other => panic!("ops did not round-trip: {other:?}"),
        }
    }

    #[test]
    fn vector_ops_round_trip_absent_and_null_metadata() {
        let ops = [
            Op::CreateVectorCollection {
                name: "emb".into(),
                dim: 2,
                storage: vector::VectorStorage::F16,
                metric: vector::VectorMetric::L2,
                m: 4,
                ef_construction: 8,
                ef_search: 9,
            },
            Op::UpsertVector {
                collection: "emb".into(),
                id: "a".into(),
                bytes: vec![0, 60],
                metadata: None,
            },
            Op::UpsertVector {
                collection: "emb".into(),
                id: "b".into(),
                bytes: vec![1, 2],
                metadata: Some(Value::Null),
            },
            Op::DeleteVector {
                collection: "emb".into(),
                id: "a".into(),
            },
        ];
        let encoded = Value::Array(ops.iter().map(op_value).collect());
        let again = parse_ops(&encoded).unwrap();
        match &again[..] {
            [
                Op::CreateVectorCollection {
                    name,
                    dim,
                    storage,
                    metric,
                    m,
                    ef_construction,
                    ef_search,
                },
                Op::UpsertVector {
                    id,
                    bytes: absent_bytes,
                    metadata: absent,
                    ..
                },
                Op::UpsertVector {
                    id: present,
                    metadata: present_meta,
                    ..
                },
                Op::DeleteVector { id: deleted, .. },
            ] => {
                assert_eq!(name, "emb");
                assert_eq!(*dim, 2);
                assert_eq!(*storage, vector::VectorStorage::F16);
                assert_eq!(*metric, vector::VectorMetric::L2);
                assert_eq!(*m, 4);
                assert_eq!(*ef_construction, 8);
                assert_eq!(*ef_search, 9);
                assert_eq!(id, "a");
                assert!(absent.is_none());
                assert_eq!(absent_bytes, &[0, 60]);
                assert_eq!(present, "b");
                assert_eq!(present_meta, &Some(Value::Null));
                assert_eq!(deleted, "a");
            }
            other => panic!("vector ops did not round-trip: {other:?}"),
        }
        let absent_meta = encoded
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["id"] == "a")
            .unwrap();
        assert!(absent_meta.get("metadata").is_none());
        let present_meta = encoded
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["id"] == "b")
            .unwrap();
        assert!(present_meta.get("metadata").unwrap().is_null());
    }
}
