//! Local Unix domain socket server/client (`fl-wire.sock`) and multi-peer WebSocket relay daemon (spec §3.3, §3.4).

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
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
        let resp: LocalResponse =
            serde_json::from_str(&resp_line).map_err(|e| format!("invalid daemon response: {e}"))?;
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
    peers: Mutex<BTreeMap<u64, (String, Sender<PeerFrame>)>>,
    subscribers: Mutex<Vec<Subscriber>>,
    next_peer_id: Mutex<u64>,
    shutdown: AtomicBool,
}

impl<S> DaemonInner<S>
where
    S: MessageStore + AgentRegistry + Send + Sync + 'static,
{
    fn register_peer(&self, label: String, tx: Sender<PeerFrame>) -> u64 {
        let mut id_guard = self.next_peer_id.lock().unwrap();
        *id_guard += 1;
        let id = *id_guard;
        self.peers.lock().unwrap().insert(id, (label, tx));
        id
    }

    fn unregister_peer(&self, id: u64) {
        self.peers.lock().unwrap().remove(&id);
    }

    fn broadcast_to_peers(&self, exclude_peer: Option<u64>, frame: &PeerFrame) {
        let mut peers = self.peers.lock().unwrap();
        peers.retain(|&id, (_, tx)| {
            if Some(id) == exclude_peer {
                return true;
            }
            tx.send(frame.clone()).is_ok()
        });
    }

    fn notify_subscribers_message(&self, msg: &WireMessage, receipts: &[DeliveryReceipt]) {
        let mut subs = self.subscribers.lock().unwrap();
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
        let mut subs = self.subscribers.lock().unwrap();
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
        let updated = {
            let mut routes = self.routes.lock().unwrap();
            dispatch_message_for_store(
                self.store.as_ref(),
                &mut routes,
                self.config.home_override.as_deref(),
                msg,
                now,
            )
        };
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
                        let latest_receipts = self
                            .store
                            .list_wire_receipts(&msg.id)
                            .unwrap_or(receipts);
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
                    if ack
                        && let Some(ref rec) = filter.recipient
                    {
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
                    let mut routes = self.routes.lock().unwrap();
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
                self.subscribers.lock().unwrap().push(Subscriber {
                    recipient: norm_rcpt,
                    correlation_id,
                    tx,
                });
                (LocalResponse::Subscribed, Some(rx))
            }
            LocalRequest::Status => {
                let connected_peers = self
                    .peers
                    .lock()
                    .unwrap()
                    .values()
                    .map(|(label, _)| label.clone())
                    .collect();
                let routes = self
                    .routes
                    .lock()
                    .unwrap()
                    .entries
                    .values()
                    .cloned()
                    .collect();
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
    if let Some(parent) = config.socket_path.parent() {
        fs::create_dir_all(parent)?;
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
        peers: Mutex::new(BTreeMap::new()),
        subscribers: Mutex::new(Vec::new()),
        next_peer_id: Mutex::new(0),
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
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
                    let Ok(mut writer) = stream.try_clone() else {
                        return;
                    };
                    let mut reader = BufReader::new(stream);
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
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
                std::thread::spawn(move || {
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
}
