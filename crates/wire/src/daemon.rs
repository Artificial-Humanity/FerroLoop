//! Local Unix domain socket server/client (`fl-wire.sock`) and multi-peer WebSocket relay daemon (spec §3.3, §3.4).

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fl_core::agent::RegistryEvent;
use fl_core::at::At;
use fl_core::ferrowire::{
    DeliveryReceipt, DeliveryStatus, SendWireArgs, WireFilter, WireInboxItem, WireMessage,
};
use fl_core::ids::MessageId;
use fl_core::store::{AgentRegistry, MessageStore};
use serde::{Deserialize, Serialize};

use crate::dispatcher::dispatch_message_for_store;
use crate::identity::{RouteEntry, RouteTable, VendorRoute, normalize_persona};
use crate::peer::{PeerFrame, handshake_peer};
use crate::ws::{WsError, WsStream};

pub const DEFAULT_LOCAL_CONNECT_TIMEOUT: Duration = Duration::from_millis(50);

/// Longest request line the daemon reads from a local client: a send carries at most a 256 KiB
/// body, which JSON escaping can at most roughly six-fold, plus the other fields.
pub const MAX_LOCAL_REQUEST_BYTES: u64 = 2 * 1024 * 1024;

/// How many inbound peer connections may be open (handshaking or synced) at once.
pub const MAX_INBOUND_PEER_CONNECTIONS: usize = 64;

/// Lock `m`, recovering the data if a panicking thread poisoned it. The daemon's state stays
/// consistent across a panic (every update is one statement), and a poisoned lock must not turn
/// one failed connection thread into a daemon that panics on every later request.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A claimed slot among `limit`; the slot is released on drop.
struct SlotGuard<'a> {
    counter: &'a AtomicUsize,
}

impl<'a> SlotGuard<'a> {
    fn try_claim(counter: &'a AtomicUsize, limit: usize) -> Option<Self> {
        let mut current = counter.load(Ordering::Relaxed);
        loop {
            if current >= limit {
                return None;
            }
            match counter.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Some(Self { counter }),
                Err(seen) => current = seen,
            }
        }
    }
}

impl Drop for SlotGuard<'_> {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::AcqRel);
    }
}

pub fn default_socket_path() -> PathBuf {
    if let Some(rt) = std::env::var_os("XDG_RUNTIME_DIR") {
        let p = PathBuf::from(rt);
        if p.is_absolute() {
            return p.join("fl-wire.sock");
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let p = PathBuf::from(home);
        if p.is_absolute() {
            return p.join(".cache").join("fl").join("wire.sock");
        }
    }
    PathBuf::from("/tmp/fl-wire.sock")
}

fn now_at() -> At {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    At::from_unix_millis(millis)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum LocalRequest {
    Send {
        args: SendWireArgs,
    },
    Read {
        filter: WireFilter,
        ack: bool,
    },
    Ack {
        message_ids: Vec<MessageId>,
        recipient: String,
    },
    RegisterRoute {
        persona: String,
        route: VendorRoute,
    },
    Subscribe {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        recipient: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        correlation_id: Option<MessageId>,
    },
    Status,
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum LocalResponse {
    Sent {
        message: WireMessage,
        receipts: Vec<DeliveryReceipt>,
    },
    Inbox {
        items: Vec<WireInboxItem>,
    },
    Acked {
        receipts: Vec<DeliveryReceipt>,
    },
    RouteRegistered,
    Subscribed,
    Status(DaemonStatus),
    ShuttingDown,
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub node_id: String,
    pub host: String,
    pub socket_path: PathBuf,
    pub listen_addr: Option<String>,
    pub connected_peers: Vec<String>,
    pub routes: Vec<RouteEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum LocalEvent {
    Message { message: WireMessage },
    Receipt { receipt: DeliveryReceipt },
}

pub struct LocalClient {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl LocalClient {
    pub fn connect(socket_path: &Path, timeout: Duration) -> io::Result<Self> {
        let stream = UnixStream::connect(socket_path)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        let writer = stream.try_clone()?;
        Ok(Self {
            reader: BufReader::new(stream),
            writer,
        })
    }

    pub fn request(&mut self, req: &LocalRequest) -> Result<LocalResponse, String> {
        let mut line = serde_json::to_string(req).map_err(|e| e.to_string())?;
        line.push('\n');
        self.writer
            .write_all(line.as_bytes())
            .and_then(|()| self.writer.flush())
            .map_err(|e| e.to_string())?;
        let mut resp_line = String::new();
        let n = self
            .reader
            .read_line(&mut resp_line)
            .map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("unexpected EOF from fl-wire daemon".into());
        }
        let resp: LocalResponse = serde_json::from_str(&resp_line)
            .map_err(|e| format!("invalid daemon response: {e}"))?;
        if let LocalResponse::Error { message } = resp {
            return Err(message);
        }
        Ok(resp)
    }

    pub fn subscribe(
        &mut self,
        recipient: Option<String>,
        correlation_id: Option<MessageId>,
    ) -> Result<(), String> {
        let resp = self.request(&LocalRequest::Subscribe {
            recipient,
            correlation_id,
        })?;
        match resp {
            LocalResponse::Subscribed => Ok(()),
            other => Err(format!("unexpected subscribe response: {other:?}")),
        }
    }

    pub fn wait_for_event(&mut self, timeout: Duration) -> Result<Option<LocalEvent>, String> {
        self.reader
            .get_ref()
            .set_read_timeout(Some(timeout))
            .map_err(|e| e.to_string())?;
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(0) => Ok(None),
            Ok(_) => {
                let ev: LocalEvent = serde_json::from_str(&line)
                    .map_err(|e| format!("invalid daemon event: {e}"))?;
                Ok(Some(ev))
            }
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut =>
            {
                Ok(None)
            }
            Err(e) => Err(e.to_string()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct DaemonConfig {
    pub node_id: String,
    pub host: String,
    pub socket_path: PathBuf,
    pub routes_path: Option<PathBuf>,
    pub home_override: Option<PathBuf>,
    pub listen_addr: Option<String>,
    pub peers: Vec<String>,
    pub psk: Vec<u8>,
}

struct Subscriber {
    recipient: Option<String>,
    correlation_id: Option<MessageId>,
    tx: Sender<LocalEvent>,
}

struct DaemonInner<S> {
    store: Arc<S>,
    config: DaemonConfig,
    routes: Mutex<RouteTable>,
    dispatch_gate: Mutex<()>,
    peers: Mutex<BTreeMap<u64, (String, Sender<PeerFrame>)>>,
    subscribers: Mutex<Vec<Subscriber>>,
    next_peer_id: Mutex<u64>,
    inbound_peers: AtomicUsize,
    shutdown: AtomicBool,
}

impl<S> DaemonInner<S>
where
    S: MessageStore + AgentRegistry + Send + Sync + 'static,
{
    fn register_peer(&self, label: String, tx: Sender<PeerFrame>) -> u64 {
        let mut id_guard = lock(&self.next_peer_id);
        *id_guard = id_guard.wrapping_add(1);
        let id = *id_guard;
        lock(&self.peers).insert(id, (label, tx));
        id
    }

    fn unregister_peer(&self, id: u64) {
        lock(&self.peers).remove(&id);
    }

    fn broadcast_to_peers(&self, exclude_peer: Option<u64>, frame: &PeerFrame) {
        let mut peers = lock(&self.peers);
        peers.retain(|&id, (_, tx)| {
            if Some(id) == exclude_peer {
                return true;
            }
            tx.send(frame.clone()).is_ok()
        });
    }

    fn notify_subscribers_message(&self, msg: &WireMessage, receipts: &[DeliveryReceipt]) {
        let mut subs = lock(&self.subscribers);
        subs.retain(|sub| {
            let matches_corr = sub
                .correlation_id
                .as_ref()
                .is_none_or(|cid| msg.correlation_id.as_ref() == Some(cid));
            let matches_rcpt = sub.recipient.as_ref().is_none_or(|r| {
                let norm = r.to_ascii_lowercase();
                receipts.iter().any(|rcpt| rcpt.recipient_name == norm)
            });
            if matches_corr && matches_rcpt {
                sub.tx
                    .send(LocalEvent::Message {
                        message: msg.clone(),
                    })
                    .is_ok()
            } else {
                true
            }
        });
    }

    fn notify_subscribers_receipt(&self, rcpt: &DeliveryReceipt) {
        let mut subs = lock(&self.subscribers);
        subs.retain(|sub| {
            if sub.correlation_id.is_some() {
                return true;
            }
            let matches_rcpt = sub
                .recipient
                .as_ref()
                .is_none_or(|r| r.eq_ignore_ascii_case(&rcpt.recipient_name));
            if matches_rcpt {
                sub.tx
                    .send(LocalEvent::Receipt {
                        receipt: rcpt.clone(),
                    })
                    .is_ok()
            } else {
                true
            }
        });
    }

    fn dispatch_and_propagate(&self, msg: &WireMessage, now: At) {
        // One dispatch at a time, so two threads cannot both push the same pending message. The
        // `routes` lock is held only to copy the table out and to merge it back: pushing runs
        // vendor CLIs, and `Status` and `RegisterRoute` must not wait behind one.
        let _one_at_a_time = lock(&self.dispatch_gate);
        let mut working = lock(&self.routes).clone();
        let updated = dispatch_message_for_store(
            self.store.as_ref(),
            &mut working,
            self.config.home_override.as_deref(),
            msg,
            now,
        );
        lock(&self.routes).merge_newer(working);
        for rcpt in updated {
            self.notify_subscribers_receipt(&rcpt);
            self.broadcast_to_peers(None, &PeerFrame::ReceiptUpdate { receipt: rcpt });
        }
    }

    fn dispatch_pending_for_persona(&self, persona: &str, now: At) {
        let filter = WireFilter {
            recipient: Some(persona.to_string()),
            unread_only: true,
            thread_id: None,
            correlation_id: None,
            limit: None,
        };
        if let Ok(items) = self.store.list_wire_inbox(&filter) {
            for item in items {
                self.dispatch_and_propagate(&item.message, now.clone());
            }
        }
    }

    fn handle_local_request(
        &self,
        req: LocalRequest,
    ) -> (LocalResponse, Option<Receiver<LocalEvent>>) {
        let now = now_at();
        match req {
            LocalRequest::Send { mut args } => {
                if args.sender_host.is_none() {
                    args.sender_host = Some(self.config.host.clone());
                }
                match self.store.send_wire(args) {
                    Ok((msg, receipts)) => {
                        self.notify_subscribers_message(&msg, &receipts);
                        self.broadcast_to_peers(
                            None,
                            &PeerFrame::Publish {
                                message: msg.clone(),
                                receipts: receipts.clone(),
                            },
                        );
                        self.dispatch_and_propagate(&msg, now);
                        let latest_receipts =
                            self.store.list_wire_receipts(&msg.id).unwrap_or(receipts);
                        (
                            LocalResponse::Sent {
                                message: msg,
                                receipts: latest_receipts,
                            },
                            None,
                        )
                    }
                    Err(e) => (
                        LocalResponse::Error {
                            message: e.to_string(),
                        },
                        None,
                    ),
                }
            }
            LocalRequest::Read { filter, ack } => match self.store.list_wire_inbox(&filter) {
                Ok(mut items) => {
                    if ack && let Some(ref rec) = filter.recipient {
                        for item in &mut items {
                            if item.receipt.status != DeliveryStatus::Acked
                                && let Ok(Some(updated)) = self.store.update_wire_receipt(
                                    &item.message.id,
                                    rec,
                                    DeliveryStatus::Acked,
                                    false,
                                    now.clone(),
                                )
                            {
                                item.receipt = updated.clone();
                                self.notify_subscribers_receipt(&updated);
                                self.broadcast_to_peers(
                                    None,
                                    &PeerFrame::ReceiptUpdate { receipt: updated },
                                );
                            }
                        }
                    }
                    (LocalResponse::Inbox { items }, None)
                }
                Err(e) => (
                    LocalResponse::Error {
                        message: e.to_string(),
                    },
                    None,
                ),
            },
            LocalRequest::Ack {
                message_ids,
                recipient,
            } => {
                let mut out = Vec::new();
                for mid in message_ids {
                    match self.store.update_wire_receipt(
                        &mid,
                        &recipient,
                        DeliveryStatus::Acked,
                        false,
                        now.clone(),
                    ) {
                        Ok(Some(rcpt)) => {
                            self.notify_subscribers_receipt(&rcpt);
                            self.broadcast_to_peers(
                                None,
                                &PeerFrame::ReceiptUpdate {
                                    receipt: rcpt.clone(),
                                },
                            );
                            out.push(rcpt);
                        }
                        Ok(None) => {}
                        Err(e) => {
                            return (
                                LocalResponse::Error {
                                    message: e.to_string(),
                                },
                                None,
                            );
                        }
                    }
                }
                (LocalResponse::Acked { receipts: out }, None)
            }
            LocalRequest::RegisterRoute { persona, route } => {
                {
                    let mut routes = lock(&self.routes);
                    routes.upsert(&persona, route, now.clone());
                    let _ = routes.save(self.config.routes_path.as_deref());
                }
                self.dispatch_pending_for_persona(&persona, now);
                (LocalResponse::RouteRegistered, None)
            }
            LocalRequest::Subscribe {
                recipient,
                correlation_id,
            } => {
                let (tx, rx) = mpsc::channel();
                let norm_rcpt = recipient.as_deref().and_then(normalize_persona);
                lock(&self.subscribers).push(Subscriber {
                    recipient: norm_rcpt,
                    correlation_id,
                    tx,
                });
                (LocalResponse::Subscribed, Some(rx))
            }
            LocalRequest::Status => {
                let connected_peers = lock(&self.peers)
                    .values()
                    .map(|(label, _)| label.clone())
                    .collect();
                let routes = lock(&self.routes).entries.values().cloned().collect();
                (
                    LocalResponse::Status(DaemonStatus {
                        node_id: self.config.node_id.clone(),
                        host: self.config.host.clone(),
                        socket_path: self.config.socket_path.clone(),
                        listen_addr: self.config.listen_addr.clone(),
                        connected_peers,
                        routes,
                    }),
                    None,
                )
            }
            LocalRequest::Shutdown => {
                self.shutdown.store(true, Ordering::SeqCst);
                (LocalResponse::ShuttingDown, None)
            }
        }
    }

    fn handle_peer_connection(&self, mut ws: WsStream) -> Result<(), WsError> {
        let last_msg_seq = self
            .store
            .wire_messages_after(0)
            .ok()
            .and_then(|v| v.last().map(|(s, _)| *s))
            .unwrap_or(0);
        let last_rcpt_seq = self
            .store
            .wire_receipts_after(0)
            .ok()
            .and_then(|v| v.last().map(|(s, _)| *s))
            .unwrap_or(0);
        let last_agent_seq = self
            .store
            .registry_events_after(0)
            .ok()
            .and_then(|v| v.last().map(|(s, _)| *s))
            .unwrap_or(0);

        let hs = handshake_peer(
            &mut ws,
            &self.config.node_id,
            &self.config.host,
            &self.config.psk,
            last_msg_seq,
            last_rcpt_seq,
            last_agent_seq,
        )?;

        // Send initial catch-up SyncBatch for everything after remote's cursors
        let registry_events: Vec<RegistryEvent> = self
            .store
            .registry_events_after(hs.remote_last_agent_seq)
            .unwrap_or_default()
            .into_iter()
            .map(|(_, e)| e)
            .collect();
        let messages: Vec<WireMessage> = self
            .store
            .wire_messages_after(hs.remote_last_msg_seq)
            .unwrap_or_default()
            .into_iter()
            .map(|(_, m)| m)
            .collect();
        let receipts: Vec<DeliveryReceipt> = self
            .store
            .wire_receipts_after(hs.remote_last_rcpt_seq)
            .unwrap_or_default()
            .into_iter()
            .map(|(_, r)| r)
            .collect();

        ws.send_json(&PeerFrame::SyncBatch {
            registry_events,
            messages,
            receipts,
        })?;

        let (tx, rx) = mpsc::channel::<PeerFrame>();
        let peer_label = format!("{}@{}", hs.remote_node_id, hs.remote_host);
        let peer_id = self.register_peer(peer_label, tx);

        ws.set_read_timeout(Some(Duration::from_millis(50)))?;
        let mut last_ping = Instant::now();

        let res = 'peer_loop: loop {
            if self.shutdown.load(Ordering::SeqCst) {
                let _ = ws.close();
                break Ok(());
            }
            // We ping every PEER_PING_INTERVAL; a live peer answers with a pong. Silence for
            // PEER_READ_TIMEOUT means the connection is dead (or the peer is hung).
            if ws.idle_for() >= crate::peer::PEER_READ_TIMEOUT {
                let _ = ws.close();
                break Err(WsError::Io(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "peer sent nothing for the read timeout",
                )));
            }

            while let Ok(out_frame) = rx.try_recv() {
                if let Err(e) = ws.send_json(&out_frame) {
                    break 'peer_loop Err(e);
                }
            }

            if last_ping.elapsed() >= crate::peer::PEER_PING_INTERVAL {
                let _ = ws.send_ping(b"ka");
                last_ping = Instant::now();
            }

            match ws.recv_json::<PeerFrame>() {
                Ok(frame) => {
                    self.process_incoming_peer_frame(peer_id, frame);
                }
                Err(WsError::Io(ref e))
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::TimedOut =>
                {
                    continue;
                }
                Err(WsError::Closed) => break Ok(()),
                Err(e) => break Err(e),
            }
        };

        self.unregister_peer(peer_id);
        res
    }

    fn process_incoming_peer_frame(&self, from_peer_id: u64, frame: PeerFrame) {
        let now = now_at();
        match frame {
            PeerFrame::SyncBatch {
                registry_events,
                messages,
                receipts,
            } => {
                if !registry_events.is_empty() {
                    let _ = self.store.import_registry_events(&registry_events);
                }
                let mut new_msgs = Vec::new();
                for m in &messages {
                    if self.store.get_wire_message(&m.id).ok().flatten().is_none() {
                        new_msgs.push(m.clone());
                    }
                }
                if self.store.import_wire_batch(&messages, &receipts).is_ok() {
                    for m in new_msgs {
                        let rcpts = self.store.list_wire_receipts(&m.id).unwrap_or_default();
                        self.notify_subscribers_message(&m, &rcpts);
                        self.broadcast_to_peers(
                            Some(from_peer_id),
                            &PeerFrame::Publish {
                                message: m.clone(),
                                receipts: rcpts,
                            },
                        );
                        self.dispatch_and_propagate(&m, now.clone());
                    }
                    for r in receipts {
                        self.notify_subscribers_receipt(&r);
                    }
                }
            }
            PeerFrame::Publish { message, receipts } => {
                let known = self
                    .store
                    .get_wire_message(&message.id)
                    .ok()
                    .flatten()
                    .is_some();
                if self
                    .store
                    .import_wire_batch(std::slice::from_ref(&message), &receipts)
                    .is_ok()
                    && !known
                {
                    let rcpts = self
                        .store
                        .list_wire_receipts(&message.id)
                        .unwrap_or(receipts);
                    self.notify_subscribers_message(&message, &rcpts);
                    self.broadcast_to_peers(
                        Some(from_peer_id),
                        &PeerFrame::Publish {
                            message: message.clone(),
                            receipts: rcpts,
                        },
                    );
                    self.dispatch_and_propagate(&message, now);
                }
            }
            PeerFrame::ReceiptUpdate { receipt } => {
                let prev_rank = self
                    .store
                    .list_wire_receipts(&receipt.message_id)
                    .ok()
                    .and_then(|list| {
                        list.into_iter()
                            .find(|r| {
                                r.recipient_id == receipt.recipient_id
                                    || r.recipient_name == receipt.recipient_name
                            })
                            .map(|r| r.status.rank())
                    })
                    .unwrap_or(0);
                if self
                    .store
                    .import_wire_batch(&[], std::slice::from_ref(&receipt))
                    .is_ok()
                    && receipt.status.rank() > prev_rank
                {
                    self.notify_subscribers_receipt(&receipt);
                    self.broadcast_to_peers(
                        Some(from_peer_id),
                        &PeerFrame::ReceiptUpdate { receipt },
                    );
                }
            }
            PeerFrame::RegistryUpdate { event } => {
                if self
                    .store
                    .import_registry_events(std::slice::from_ref(&event))
                    .is_ok()
                {
                    self.broadcast_to_peers(
                        Some(from_peer_id),
                        &PeerFrame::RegistryUpdate { event },
                    );
                }
            }
            PeerFrame::Challenge { .. } | PeerFrame::Auth { .. } => {}
        }
    }
}

pub struct DaemonHandle {
    socket_path: PathBuf,
    listen_addr: Option<String>,
    shutdown: Arc<AtomicBool>,
    join_handle: Option<std::thread::JoinHandle<()>>,
}

impl DaemonHandle {
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub fn listen_addr(&self) -> Option<&str> {
        self.listen_addr.as_deref()
    }

    pub fn stop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = UnixStream::connect(&self.socket_path);
        if let Some(ref addr) = self.listen_addr {
            let _ = TcpStream::connect(addr);
        }
        if let Some(h) = self.join_handle.take() {
            let _ = h.join();
        }
        let _ = fs::remove_file(&self.socket_path);
    }
}

impl Drop for DaemonHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

pub fn spawn_daemon<S>(store: Arc<S>, mut config: DaemonConfig) -> io::Result<DaemonHandle>
where
    S: MessageStore + AgentRegistry + Send + Sync + 'static,
{
    if config.psk.is_empty() && (config.listen_addr.is_some() || !config.peers.is_empty()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a daemon that listens for or dials peers needs a non-empty pre-shared key",
        ));
    }
    if let Some(parent) = config.socket_path.parent() {
        fs::create_dir_all(parent)?;
    }
    if let Ok(meta) = fs::symlink_metadata(&config.socket_path) {
        use std::os::unix::fs::FileTypeExt;
        if !meta.file_type().is_socket() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "{} exists and is not a socket; refusing to remove it",
                    config.socket_path.display()
                ),
            ));
        }
    }
    if config.socket_path.exists() {
        if LocalClient::connect(&config.socket_path, Duration::from_millis(50)).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!(
                    "fl-wire daemon is already listening at {}",
                    config.socket_path.display()
                ),
            ));
        }
        let _ = fs::remove_file(&config.socket_path);
    }

    let unix_listener = UnixListener::bind(&config.socket_path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&config.socket_path, fs::Permissions::from_mode(0o600));
    }
    unix_listener.set_nonblocking(true)?;

    let tcp_listener = if let Some(ref bind_addr) = config.listen_addr {
        let l = TcpListener::bind(bind_addr)?;
        let actual = l.local_addr()?.to_string();
        config.listen_addr = Some(actual);
        l.set_nonblocking(true)?;
        Some(l)
    } else {
        None
    };

    let initial_routes = RouteTable::load(config.routes_path.as_deref());
    let inner = Arc::new(DaemonInner {
        store,
        config: config.clone(),
        routes: Mutex::new(initial_routes),
        dispatch_gate: Mutex::new(()),
        peers: Mutex::new(BTreeMap::new()),
        subscribers: Mutex::new(Vec::new()),
        next_peer_id: Mutex::new(0),
        inbound_peers: AtomicUsize::new(0),
        shutdown: AtomicBool::new(false),
    });

    // Spawn outbound peer loops
    for peer_url in config.peers.clone() {
        let inner_peer = Arc::clone(&inner);
        std::thread::spawn(move || {
            let mut backoff = Duration::from_millis(100);
            while !inner_peer.shutdown.load(Ordering::SeqCst) {
                if let Ok(ws) = WsStream::connect(&peer_url, Duration::from_secs(2)) {
                    backoff = Duration::from_millis(100);
                    let _ = inner_peer.handle_peer_connection(ws);
                }
                if inner_peer.shutdown.load(Ordering::SeqCst) {
                    break;
                }
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(5));
            }
        });
    }

    let shutdown_flag = Arc::new(AtomicBool::new(false));
    let shutdown_clone = Arc::clone(&shutdown_flag);
    let inner_main = Arc::clone(&inner);

    let join_handle = std::thread::spawn(move || {
        while !shutdown_clone.load(Ordering::SeqCst) && !inner_main.shutdown.load(Ordering::SeqCst)
        {
            let mut did_work = false;

            if let Ok((stream, _)) = unix_listener.accept() {
                did_work = true;
                let inner_conn = Arc::clone(&inner_main);
                std::thread::spawn(move || {
                    // A listener in non-blocking mode can hand out non-blocking streams
                    // (BSD and macOS do); the reads below expect blocking ones.
                    let _ = stream.set_nonblocking(false);
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
                    let Ok(mut writer) = stream.try_clone() else {
                        return;
                    };
                    let mut reader = BufReader::new(stream).take(MAX_LOCAL_REQUEST_BYTES);
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    if !line.ends_with('\n') && line.len() as u64 >= MAX_LOCAL_REQUEST_BYTES {
                        let err = LocalResponse::Error {
                            message: format!(
                                "request line exceeds {MAX_LOCAL_REQUEST_BYTES} bytes"
                            ),
                        };
                        let _ = writeln!(writer, "{}", serde_json::to_string(&err).unwrap());
                        return;
                    }
                    let req: LocalRequest = match serde_json::from_str(&line) {
                        Ok(r) => r,
                        Err(e) => {
                            let err = LocalResponse::Error {
                                message: format!("invalid request JSON: {e}"),
                            };
                            let _ = writeln!(writer, "{}", serde_json::to_string(&err).unwrap());
                            return;
                        }
                    };
                    let (resp, sub_rx) = inner_conn.handle_local_request(req);
                    if writeln!(writer, "{}", serde_json::to_string(&resp).unwrap()).is_err() {
                        return;
                    }
                    let _ = writer.flush();

                    if let Some(rx) = sub_rx {
                        while !inner_conn.shutdown.load(Ordering::SeqCst) {
                            match rx.recv_timeout(Duration::from_millis(100)) {
                                Ok(ev) => {
                                    if writeln!(writer, "{}", serde_json::to_string(&ev).unwrap())
                                        .is_err()
                                    {
                                        break;
                                    }
                                    let _ = writer.flush();
                                }
                                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                            }
                        }
                    }
                });
            }

            if let Some(ref tcp) = tcp_listener
                && let Ok((stream, _)) = tcp.accept()
            {
                did_work = true;
                let inner_ws = Arc::clone(&inner_main);
                let _ = stream.set_nonblocking(false);
                std::thread::spawn(move || {
                    // Every inbound connection costs a thread before it has proved it holds
                    // the PSK, so the number open at once is capped.
                    let Some(_slot) =
                        SlotGuard::try_claim(&inner_ws.inbound_peers, MAX_INBOUND_PEER_CONNECTIONS)
                    else {
                        return;
                    };
                    if let Ok(ws) = WsStream::accept(stream, Duration::from_secs(5)) {
                        let _ = inner_ws.handle_peer_connection(ws);
                    }
                });
            }

            if !did_work {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        inner_main.shutdown.store(true, Ordering::SeqCst);
    });

    Ok(DaemonHandle {
        socket_path: config.socket_path,
        listen_addr: config.listen_addr,
        shutdown: shutdown_flag,
        join_handle: Some(join_handle),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::ferrowire::{MessageKind, MessageUrgency, WireAddress};
    use fl_store::RedbStore;
    use tempfile::TempDir;

    #[test]
    fn two_daemons_sync_over_websocket_push_to_webhook_and_propagate_ack_and_reply() {
        let tmp = TempDir::new().unwrap();
        let store_a = Arc::new(RedbStore::open(&tmp.path().join("a.redb")).unwrap());
        let store_b = Arc::new(RedbStore::open(&tmp.path().join("b.redb")).unwrap());

        // Pre-populate a message on store_a before daemon B connects to verify initial SyncBatch catch-up
        let (boot_msg, _) = store_a
            .send_wire(SendWireArgs {
                id: None,
                sender_id: None,
                sender_name: "jarvis".into(),
                sender_host: Some("node-a".into()),
                to: WireAddress::AgentName("kayla".into()),
                kind: MessageKind::Mail,
                urgency: MessageUrgency::Normal,
                durable: true,
                subject: "catchup".into(),
                body: "sent before peer connected".into(),
                reply_to: None,
                thread_id: None,
                deadline_at: None,
                at: At::from_unix_millis(10),
            })
            .unwrap();

        let sock_a = tmp.path().join("wire-a.sock");
        let daemon_a = spawn_daemon(
            Arc::clone(&store_a),
            DaemonConfig {
                node_id: "node-a".into(),
                host: "host-a".into(),
                socket_path: sock_a.clone(),
                routes_path: Some(tmp.path().join("routes-a.json")),
                home_override: Some(tmp.path().to_path_buf()),
                listen_addr: Some("127.0.0.1:0".into()),
                peers: vec![],
                psk: b"top-secret-psk".to_vec(),
            },
        )
        .unwrap();
        let addr_a = daemon_a.listen_addr().unwrap().to_string();

        // Set up a webhook Unix listener for Kayla on host B to verify push delivery + auto-Ack propagation
        let webhook_sock = tmp.path().join("kayla-webhook.sock");
        let webhook_listener = UnixListener::bind(&webhook_sock).unwrap();

        let sock_b = tmp.path().join("wire-b.sock");
        let _daemon_b = spawn_daemon(
            Arc::clone(&store_b),
            DaemonConfig {
                node_id: "node-b".into(),
                host: "host-b".into(),
                socket_path: sock_b.clone(),
                routes_path: Some(tmp.path().join("routes-b.json")),
                home_override: Some(tmp.path().to_path_buf()),
                listen_addr: None,
                peers: vec![format!("ws://{addr_a}/wire")],
                psk: b"top-secret-psk".to_vec(),
            },
        )
        .unwrap();

        // Wait for initial SyncBatch to reach store_b
        let deadline = Instant::now() + Duration::from_secs(3);
        while store_b.get_wire_message(&boot_msg.id).unwrap().is_none() {
            assert!(Instant::now() < deadline, "timed out waiting for SyncBatch");
            std::thread::sleep(Duration::from_millis(20));
        }

        // Register Kayla's webhook route on daemon B (which also dispatches pending messages)
        let mut client_b = LocalClient::connect(&sock_b, Duration::from_secs(1)).unwrap();
        client_b
            .request(&LocalRequest::RegisterRoute {
                persona: "kayla".into(),
                route: VendorRoute::Webhook {
                    socket_path: webhook_sock,
                },
            })
            .unwrap();

        // Accept the pushed boot_msg on Kayla's webhook socket
        let (wh_stream, _) = webhook_listener.accept().unwrap();
        let mut wh_line = String::new();
        BufReader::new(wh_stream).read_line(&mut wh_line).unwrap();
        let pushed: WireMessage = serde_json::from_str(&wh_line).unwrap();
        assert_eq!(pushed.id, boot_msg.id);

        // Verify Acked receipt propagated back to store_a over WebSocket!
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let rcpts = store_a.list_wire_receipts(&boot_msg.id).unwrap();
            if rcpts[0].status == DeliveryStatus::Acked {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for Acked receipt propagation to store_a"
            );
            std::thread::sleep(Duration::from_millis(20));
        }

        // Now test live Ask -> Subscribe -> Reply across the two daemons
        let mut client_a = LocalClient::connect(&sock_a, Duration::from_secs(1)).unwrap();
        let LocalResponse::Sent {
            message: ask_msg, ..
        } = client_a
            .request(&LocalRequest::Send {
                args: SendWireArgs {
                    id: None,
                    sender_id: None,
                    sender_name: "jarvis".into(),
                    sender_host: None,
                    to: WireAddress::AgentName("kayla".into()),
                    kind: MessageKind::Ask,
                    urgency: MessageUrgency::Urgent,
                    durable: false,
                    subject: "status?".into(),
                    body: "need status".into(),
                    reply_to: None,
                    thread_id: None,
                    deadline_at: None,
                    at: At::from_unix_millis(50),
                },
            })
            .unwrap()
        else {
            panic!("expected Sent");
        };

        // Drain webhook push for ask_msg
        let _ = webhook_listener.accept().unwrap();

        // Subscribe on daemon A for replies correlated to ask_msg.id
        let mut sub_a = LocalClient::connect(&sock_a, Duration::from_secs(1)).unwrap();
        sub_a
            .subscribe(Some("jarvis".into()), Some(ask_msg.id.clone()))
            .unwrap();

        // Wait until ask_msg is present on daemon B, then send Reply on daemon B
        let deadline = Instant::now() + Duration::from_secs(3);
        while store_b.get_wire_message(&ask_msg.id).unwrap().is_none() {
            assert!(Instant::now() < deadline, "timed out waiting for ask_msg");
            std::thread::sleep(Duration::from_millis(10));
        }

        let mut reply_client_b = LocalClient::connect(&sock_b, Duration::from_secs(1)).unwrap();
        reply_client_b
            .request(&LocalRequest::Send {
                args: SendWireArgs {
                    id: None,
                    sender_id: None,
                    sender_name: "kayla".into(),
                    sender_host: None,
                    to: WireAddress::AgentName("jarvis".into()),
                    kind: MessageKind::Reply,
                    urgency: MessageUrgency::Normal,
                    durable: false,
                    subject: "Re: status?".into(),
                    body: "all green".into(),
                    reply_to: Some(ask_msg.id.clone()),
                    thread_id: None,
                    deadline_at: None,
                    at: At::from_unix_millis(60),
                },
            })
            .unwrap();

        let ev = sub_a
            .wait_for_event(Duration::from_secs(3))
            .unwrap()
            .expect("expected reply event on subscriber");
        match ev {
            LocalEvent::Message { message } => {
                assert_eq!(message.kind, MessageKind::Reply);
                assert_eq!(message.correlation_id, Some(ask_msg.id));
                assert_eq!(message.body, "all green");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    fn test_config(tmp: &Path, name: &str) -> DaemonConfig {
        DaemonConfig {
            node_id: name.into(),
            host: format!("host-{name}"),
            socket_path: tmp.join(format!("{name}.sock")),
            routes_path: Some(tmp.join(format!("routes-{name}.json"))),
            home_override: Some(tmp.to_path_buf()),
            listen_addr: None,
            peers: vec![],
            psk: b"audit-psk".to_vec(),
        }
    }

    fn mail(to: &str, subject: &str) -> SendWireArgs {
        SendWireArgs {
            id: None,
            sender_id: None,
            sender_name: "jarvis".into(),
            sender_host: None,
            to: WireAddress::AgentName(to.into()),
            kind: MessageKind::Mail,
            urgency: MessageUrgency::Normal,
            durable: true,
            subject: subject.into(),
            body: "body".into(),
            reply_to: None,
            thread_id: None,
            deadline_at: None,
            at: At::from_unix_millis(10),
        }
    }

    /// Audit D-1: the dispatch ran vendor CLIs while holding the route-table lock, so one slow
    /// `codex` froze `Status` and `RegisterRoute` for as long as it ran.
    #[test]
    fn status_and_route_registration_do_not_wait_behind_a_slow_vendor_delivery() {
        use std::os::unix::fs::PermissionsExt;
        let _codex_lock = crate::dispatcher::CODEX_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        let slow = tmp.path().join("slow-codex.sh");
        fs::write(&slow, "#!/bin/sh\nsleep 2\n").unwrap();
        fs::set_permissions(&slow, fs::Permissions::from_mode(0o755)).unwrap();
        // SAFETY: serialised by CODEX_ENV_LOCK; restored below.
        let prev = std::env::var_os("FL_WIRE_CODEX_BIN");
        unsafe { std::env::set_var("FL_WIRE_CODEX_BIN", &slow) };

        let store = Arc::new(RedbStore::open(&tmp.path().join("s.redb")).unwrap());
        let cfg = test_config(tmp.path(), "slow");
        let sock = cfg.socket_path.clone();
        let _daemon = spawn_daemon(Arc::clone(&store), cfg).unwrap();

        let mut reg = LocalClient::connect(&sock, Duration::from_secs(5)).unwrap();
        reg.request(&LocalRequest::RegisterRoute {
            persona: "kayla".into(),
            route: VendorRoute::Codex {
                thread_id: "t1".into(),
                cwd: None,
            },
        })
        .unwrap();

        let send_sock = sock.clone();
        let sender = std::thread::spawn(move || {
            let mut c = LocalClient::connect(&send_sock, Duration::from_secs(10)).unwrap();
            c.request(&LocalRequest::Send {
                args: mail("kayla", "slow one"),
            })
            .map(|_| ())
        });
        std::thread::sleep(Duration::from_millis(300));

        let started = Instant::now();
        let mut probe = LocalClient::connect(&sock, Duration::from_secs(5)).unwrap();
        probe.request(&LocalRequest::Status).unwrap();
        // The daemon serves one request per connection.
        let mut probe = LocalClient::connect(&sock, Duration::from_secs(5)).unwrap();
        probe
            .request(&LocalRequest::RegisterRoute {
                persona: "other".into(),
                route: VendorRoute::Codex {
                    thread_id: "t2".into(),
                    cwd: None,
                },
            })
            .unwrap();
        let waited = started.elapsed();

        sender.join().unwrap().unwrap();
        unsafe {
            match prev {
                Some(v) => std::env::set_var("FL_WIRE_CODEX_BIN", v),
                None => std::env::remove_var("FL_WIRE_CODEX_BIN"),
            }
        }
        assert!(
            waited < Duration::from_millis(1200),
            "Status/RegisterRoute waited {waited:?} behind a vendor delivery"
        );
    }

    #[test]
    fn oversize_request_line_is_refused_and_the_daemon_keeps_serving() {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(RedbStore::open(&tmp.path().join("s.redb")).unwrap());
        let cfg = test_config(tmp.path(), "big");
        let sock = cfg.socket_path.clone();
        let _daemon = spawn_daemon(store, cfg).unwrap();

        let mut raw = UnixStream::connect(&sock).unwrap();
        raw.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let chunk = vec![b'a'; 64 * 1024];
        let mut sent = 0u64;
        // The daemon stops reading at the cap; writes after that may fail, which is fine.
        while sent < MAX_LOCAL_REQUEST_BYTES + 1024 * 1024 {
            if raw.write_all(&chunk).is_err() {
                break;
            }
            sent += chunk.len() as u64;
        }
        let mut resp = String::new();
        BufReader::new(&raw).read_line(&mut resp).unwrap();
        assert!(resp.contains("exceeds"), "{resp}");

        let mut ok = LocalClient::connect(&sock, Duration::from_secs(2)).unwrap();
        assert!(matches!(
            ok.request(&LocalRequest::Status).unwrap(),
            LocalResponse::Status(_)
        ));
    }

    #[test]
    fn malformed_and_unknown_requests_get_an_error_not_a_dead_daemon() {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(RedbStore::open(&tmp.path().join("s.redb")).unwrap());
        let cfg = test_config(tmp.path(), "bad");
        let sock = cfg.socket_path.clone();
        let _daemon = spawn_daemon(store, cfg).unwrap();
        for line in [
            "not json\n",
            "{}\n",
            "{\"op\":\"nope\"}\n",
            "{\"op\":\"send\"}\n",
        ] {
            let mut raw = UnixStream::connect(&sock).unwrap();
            raw.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            raw.write_all(line.as_bytes()).unwrap();
            let mut resp = String::new();
            BufReader::new(&raw).read_line(&mut resp).unwrap();
            assert!(resp.contains("invalid request JSON"), "{line:?} -> {resp}");
        }
        // A client that connects and hangs up sends nothing; the daemon must shrug it off.
        drop(UnixStream::connect(&sock).unwrap());
        let mut ok = LocalClient::connect(&sock, Duration::from_secs(2)).unwrap();
        assert!(ok.request(&LocalRequest::Status).is_ok());
    }

    #[test]
    fn a_networked_daemon_needs_a_pre_shared_key() {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(RedbStore::open(&tmp.path().join("s.redb")).unwrap());
        let mut cfg = test_config(tmp.path(), "nopsk");
        cfg.psk.clear();
        cfg.listen_addr = Some("127.0.0.1:0".into());
        let err = spawn_daemon(Arc::clone(&store), cfg.clone()).err().unwrap();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        cfg.listen_addr = None;
        cfg.peers = vec!["ws://127.0.0.1:1/wire".into()];
        let err = spawn_daemon(Arc::clone(&store), cfg.clone()).err().unwrap();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        // A purely local daemon has no peer to authenticate.
        cfg.peers.clear();
        assert!(spawn_daemon(store, cfg).is_ok());
    }

    #[test]
    fn startup_never_deletes_a_file_that_is_not_a_socket() {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(RedbStore::open(&tmp.path().join("s.redb")).unwrap());
        let mut cfg = test_config(tmp.path(), "file");
        let precious = tmp.path().join("precious.txt");
        fs::write(&precious, "keep me").unwrap();
        cfg.socket_path = precious.clone();
        let err = spawn_daemon(store, cfg).err().unwrap();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(&precious).unwrap(), "keep me");
    }

    #[test]
    fn a_second_daemon_on_a_live_socket_is_refused_and_a_stale_socket_is_replaced() {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(RedbStore::open(&tmp.path().join("s.redb")).unwrap());
        let cfg = test_config(tmp.path(), "dup");
        let first = spawn_daemon(Arc::clone(&store), cfg.clone()).unwrap();
        let err = spawn_daemon(Arc::clone(&store), cfg.clone()).err().unwrap();
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
        drop(first);
        // A socket file left behind by a crashed daemon is stale and gets replaced.
        drop(UnixListener::bind(&cfg.socket_path).unwrap());
        assert!(cfg.socket_path.exists());
        assert!(spawn_daemon(store, cfg).is_ok());
    }

    #[test]
    fn a_peer_with_the_wrong_psk_is_refused_and_receives_no_messages() {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(RedbStore::open(&tmp.path().join("s.redb")).unwrap());
        store.send_wire(mail("kayla", "secret subject")).unwrap();
        let mut cfg = test_config(tmp.path(), "guarded");
        cfg.listen_addr = Some("127.0.0.1:0".into());
        let daemon = spawn_daemon(store, cfg).unwrap();
        let url = format!("ws://{}/wire", daemon.listen_addr().unwrap());

        let mut intruder = WsStream::connect(&url, Duration::from_secs(2)).unwrap();
        let outcome = handshake_peer(&mut intruder, "evil", "evil-host", b"guess", 0, 0, 0);
        assert!(outcome.is_err());
        // Whatever the daemon sends after a failed handshake must not be a SyncBatch.
        intruder
            .set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();
        if let Ok(frame) = intruder.recv_json::<PeerFrame>() {
            assert!(!matches!(frame, PeerFrame::SyncBatch { .. }), "{frame:?}");
        }
    }

    #[test]
    fn inbound_peer_slots_are_capped_and_released() {
        let counter = AtomicUsize::new(0);
        let a = SlotGuard::try_claim(&counter, 2).unwrap();
        let b = SlotGuard::try_claim(&counter, 2).unwrap();
        assert!(SlotGuard::try_claim(&counter, 2).is_none());
        drop(a);
        let c = SlotGuard::try_claim(&counter, 2).unwrap();
        drop((b, c));
        assert_eq!(counter.load(Ordering::SeqCst), 0);
        assert!(SlotGuard::try_claim(&counter, 0).is_none());
    }

    #[test]
    fn inbound_peer_slots_never_exceed_the_limit_under_contention() {
        let counter = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let handles: Vec<_> = (0..16)
            .map(|_| {
                let counter = Arc::clone(&counter);
                let peak = Arc::clone(&peak);
                std::thread::spawn(move || {
                    for _ in 0..200 {
                        if let Some(_slot) = SlotGuard::try_claim(&counter, 3) {
                            let now = counter.load(Ordering::SeqCst);
                            peak.fetch_max(now, Ordering::SeqCst);
                            std::thread::yield_now();
                        }
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert!(peak.load(Ordering::SeqCst) <= 3);
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_poisoned_daemon_lock_is_recovered_not_propagated() {
        let m = Arc::new(Mutex::new(5u32));
        let m2 = Arc::clone(&m);
        let _ = std::thread::spawn(move || {
            let _g = m2.lock().unwrap();
            panic!("poison it");
        })
        .join();
        assert!(m.is_poisoned());
        assert_eq!(*lock(&m), 5);
    }

    #[test]
    fn subscriber_filters_match_recipient_and_correlation_case_insensitively() {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(RedbStore::open(&tmp.path().join("s.redb")).unwrap());
        let cfg = test_config(tmp.path(), "sub");
        let sock = cfg.socket_path.clone();
        let _daemon = spawn_daemon(Arc::clone(&store), cfg).unwrap();

        let mut sub = LocalClient::connect(&sock, Duration::from_secs(2)).unwrap();
        sub.subscribe(Some("KAYLA".into()), None).unwrap();
        let mut other = LocalClient::connect(&sock, Duration::from_secs(2)).unwrap();
        other.subscribe(Some("someone-else".into()), None).unwrap();

        let mut sender = LocalClient::connect(&sock, Duration::from_secs(2)).unwrap();
        sender
            .request(&LocalRequest::Send {
                args: mail("kayla", "for kayla"),
            })
            .unwrap();
        let ev = sub.wait_for_event(Duration::from_secs(3)).unwrap();
        assert!(matches!(ev, Some(LocalEvent::Message { .. })), "{ev:?}");
        assert!(
            other
                .wait_for_event(Duration::from_millis(300))
                .unwrap()
                .is_none(),
            "a subscriber for another recipient saw the message"
        );
    }
}
