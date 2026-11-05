//! Local Unix domain socket server/client (`fl-wire.sock`) and multi-peer WebSocket relay daemon (spec §3.3, §3.4).
//!
//! After a crash, an accepted pending message may or may not have been pushed before its
//! receipt was updated. Startup does not scan or replay these ambiguous messages; they remain
//! readable in the inbox for manual recovery. Push ordering applies within one daemon lifetime;
//! cross-restart ordering and durable dispatch claims are deferred to Record #143.
//!
//! If a delivery is unresolved or a recipient exceeds its in-memory dispatch limit, later
//! sends to that recipient remain in its inbox for the lifetime of this daemon. The operator
//! can read and acknowledge them; the daemon does not replay them automatically. A separate
//! bound limits tracked recipient queues independently of the active worker limit. Once both
//! tracked-queue and overflow-fence metadata bounds are exhausted, new recipients remain pending
//! too; already-tracked recipients continue to use their own queues.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
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
    DeliveryReceipt, DeliveryStatus, SendWireArgs, WireFilter, WireImportReport, WireInboxItem,
    WireMessage,
};
use fl_core::ids::MessageId;
use fl_core::store::{AgentRegistry, MessageStore};
use serde::{Deserialize, Serialize};

use crate::dispatcher::dispatch_message_for_recipient;
use crate::identity::{RouteEntry, RouteTable, VendorRoute, normalize_persona};
use crate::peer::{PeerFrame, handshake_peer};
use crate::ws::{WsError, WsStream};

/// How long a [`LocalClient`] waits for `fl wire daemon` to read a request or answer it: the
/// socket's read and write timeout. A daemon's store is a remote FerroStore, so a reply can take
/// many store round trips. It does not bound the connect: a missing socket, or one nothing
/// listens on, still fails at once. Only a daemon that accepts and then is slow or hung makes a
/// caller wait this long. [`LocalClient::wait_for_event`] sets its own read timeout.
pub const DEFAULT_LOCAL_RPC_TIMEOUT: Duration = Duration::from_secs(5);

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

/// Write one daemon warning line to stderr, where the daemon's other diagnostics go. A test
/// build also records the line, so a test can assert what was warned without capturing stderr.
fn warn(line: &str) {
    #[cfg(test)]
    tests::WARNINGS.with(|w| w.borrow_mut().push(line.to_string()));
    eprintln!("fl-wire: warning: {line}");
}

/// The longest a peer-supplied string may be in a warning line, in characters.
const WARN_FIELD_MAX_CHARS: usize = 80;

/// How many skipped items one warning line lists before it says "and N more".
const WARN_MAX_LISTED_SKIPS: usize = 10;

/// Render a peer-controlled string for a warning line: control characters are escaped, so a peer
/// cannot inject terminal escapes or new lines, and the text is cut to `WARN_FIELD_MAX_CHARS`
/// characters with an ellipsis, so a peer cannot make one line unbounded.
fn sanitize_for_warning(text: &str) -> String {
    let mut out = String::new();
    for (count, c) in text.chars().enumerate() {
        if count == WARN_FIELD_MAX_CHARS {
            out.push('\u{2026}');
            break;
        }
        if c.is_control() {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
}

/// The warning for one peer import that skipped something or failed, or `None` for a clean one.
/// Lists message ids and the reason for each skip, never a message body or its validation text.
/// The peer label and ids come from the peer, so they are escaped and cut (see
/// `sanitize_for_warning`), and at most `WARN_MAX_LISTED_SKIPS` skips are listed.
fn import_warning(
    peer: &str,
    frame: &str,
    result: &Result<WireImportReport, fl_core::store::StoreError>,
) -> Option<String> {
    let peer = sanitize_for_warning(peer);
    match result {
        Ok(report) if report.is_clean() => None,
        Ok(report) => {
            let mut skipped: Vec<String> = report
                .rejected
                .iter()
                .take(WARN_MAX_LISTED_SKIPS)
                .map(|r| {
                    format!(
                        "{} {} ({})",
                        if r.is_receipt {
                            "receipt for"
                        } else {
                            "message"
                        },
                        sanitize_for_warning(r.message_id.as_str()),
                        sanitize_for_warning(&r.reason.to_string())
                    )
                })
                .collect();
            let unlisted = report.rejected.len().saturating_sub(WARN_MAX_LISTED_SKIPS);
            if unlisted > 0 {
                skipped.push(format!("and {unlisted} more"));
            }
            Some(format!(
                "{frame} from {peer}: skipped {} item(s), imported {} message(s) and {} receipt(s): {}",
                report.rejected.len(),
                report.imported_messages,
                report.imported_receipts,
                skipped.join("; ")
            ))
        }
        Err(e) => Some(format!(
            "{frame} from {peer}: import failed: {}",
            sanitize_for_warning(&e.to_string())
        )),
    }
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
        #[serde(default)]
        explicit: bool,
    },
    UnregisterRoute {
        persona: String,
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
    RouteUnregistered,
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

/// Limits keep a stuck vendor from causing unbounded threads or queued metadata. Messages beyond
/// a recipient's metadata bound remain persisted in the inbox for manual recovery.
const MAX_DISPATCH_RECIPIENTS: usize = 128;
/// Parked queues retain current-process, never-attempted messages without consuming workers.
const MAX_TRACKED_RECIPIENTS: usize = 512;
const MAX_QUEUED_PER_RECIPIENT: usize = 1024;
const MAX_OVERFLOW_RECIPIENTS: usize = 128;

#[derive(Default)]
struct RecipientDispatchQueue {
    pending: VecDeque<(MessageId, At)>,
    in_flight: Option<MessageId>,
    overflowed: bool,
    waiting_for_route: bool,
    waiting_for_capacity: bool,
    worker_active: bool,
    route_generation: u64,
}

#[derive(Clone, Copy)]
enum DispatchOutcome {
    Resolved,
    NoRoute,
    Unconfirmed,
}

struct DaemonInner<S> {
    store: Arc<S>,
    config: DaemonConfig,
    routes: Mutex<RouteTable>,
    /// Serializes only persistence and enqueue, never vendor execution.
    enqueue_gate: Mutex<()>,
    dispatch_queues: Mutex<BTreeMap<String, RecipientDispatchQueue>>,
    /// Recipients that could not acquire a worker; blocked until daemon restart so later
    /// messages cannot overtake their already-stored backlog.
    overflow_recipients: Mutex<BTreeSet<String>>,
    /// At the hard metadata limit we can no longer distinguish additional recipients safely.
    overflow_capacity_exhausted: AtomicBool,
    #[cfg(test)]
    no_route_hook: Mutex<Option<(Sender<()>, Receiver<()>)>>,
    #[cfg(test)]
    empty_exit_hook: Mutex<Option<(Sender<()>, Receiver<()>)>>,
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

    fn dispatch_and_propagate(
        &self,
        msg: &WireMessage,
        recipient: &str,
        now: At,
    ) -> DispatchOutcome {
        let base = lock(&self.routes).clone();
        let mut working = base.clone();
        let (updated, attempted) = dispatch_message_for_recipient(
            self.store.as_ref(),
            &mut working,
            self.config.home_override.as_deref(),
            msg,
            now,
            recipient,
        );
        // Discovery inside the dispatch also learns replaced sessions and prunes dead routes;
        // carry both back, and persist them so a restart starts from the refreshed table.
        let changed = !working.same_routes(&base);
        let mut live = lock(&self.routes);
        live.merge_daemon_dispatch_result(&base, working);
        if changed {
            let _ = live.save(self.config.routes_path.as_deref());
        }
        drop(live);
        for rcpt in updated {
            self.notify_subscribers_receipt(&rcpt);
            self.broadcast_to_peers(None, &PeerFrame::ReceiptUpdate { receipt: rcpt });
        }
        let receipt = self
            .store
            .list_wire_receipts(&msg.id)
            .ok()
            .and_then(|receipts| {
                receipts
                    .into_iter()
                    .find(|receipt| receipt.recipient_name.eq_ignore_ascii_case(recipient))
            });
        match receipt {
            Some(receipt)
                if matches!(
                    receipt.status,
                    DeliveryStatus::Acked | DeliveryStatus::Expired
                ) =>
            {
                DispatchOutcome::Resolved
            }
            Some(receipt)
                if attempted == Some(false)
                    && receipt.attempts < crate::dispatcher::MAX_DELIVER =>
            {
                DispatchOutcome::NoRoute
            }
            _ => DispatchOutcome::Unconfirmed,
        }
    }

    /// Start one queued worker, or retain it until an active slot becomes available.
    /// The queue mutex is held by the caller, so a worker cannot race its admission.
    fn start_queued_worker(
        self: &Arc<Self>,
        queues: &mut BTreeMap<String, RecipientDispatchQueue>,
        recipient: &str,
    ) -> Result<(), String> {
        let active = queues.values().filter(|queue| queue.worker_active).count();
        let Some(queue) = queues.get_mut(recipient) else {
            return Ok(());
        };
        if queue.worker_active || queue.overflowed {
            return Ok(());
        }
        if active >= MAX_DISPATCH_RECIPIENTS {
            queue.waiting_for_capacity = true;
            return Ok(());
        }
        queue.waiting_for_capacity = false;
        queue.waiting_for_route = false;
        queue.worker_active = true;
        let owner = Arc::clone(self);
        let name = recipient.to_string();
        if let Err(error) = std::thread::Builder::new()
            .name("fl-wire-dispatch".into())
            .spawn(move || owner.run_recipient_queue(name))
        {
            let queue = queues.get_mut(recipient).expect("queue exists");
            queue.worker_active = false;
            queue.overflowed = true;
            queue.pending.clear();
            return Err(format!("could not start delivery worker: {error}"));
        }
        Ok(())
    }

    /// A departing worker releases its slot to a queued, capacity-waiting recipient.
    fn wake_capacity_waiters(self: &Arc<Self>) {
        if self.shutdown.load(Ordering::SeqCst) {
            return;
        }
        let mut queues = lock(&self.dispatch_queues);
        let names: Vec<_> = queues
            .iter()
            .filter(|(_, queue)| queue.waiting_for_capacity && !queue.overflowed)
            .map(|(name, _)| name.clone())
            .collect();
        for name in names {
            if queues.values().filter(|queue| queue.worker_active).count()
                >= MAX_DISPATCH_RECIPIENTS
            {
                break;
            }
            if let Err(error) = self.start_queued_worker(&mut queues, &name) {
                warn(&format!("{}: {error}", sanitize_for_warning(&name)));
            }
        }
    }

    /// Enqueue while the caller holds `enqueue_gate`. A worker owns each persona until its
    /// queue empties, so IDs are deduplicated even while a push is in flight.
    fn schedule_dispatch(
        self: &Arc<Self>,
        msg: &WireMessage,
        recipients: &[DeliveryReceipt],
        now: At,
    ) -> Result<(), String> {
        let mut queues = lock(&self.dispatch_queues);
        let mut blocked = lock(&self.overflow_recipients);
        let mut errors = Vec::new();
        for receipt in recipients {
            if matches!(
                receipt.status,
                DeliveryStatus::Acked | DeliveryStatus::Expired
            ) {
                continue;
            }
            let name = receipt.recipient_name.to_ascii_lowercase();
            if let Some(queue) = queues.get_mut(&name) {
                if queue.in_flight.as_ref() == Some(&msg.id)
                    || queue.pending.iter().any(|(id, _)| id == &msg.id)
                {
                    continue;
                }
                if queue.overflowed || queue.pending.len() >= MAX_QUEUED_PER_RECIPIENT {
                    queue.overflowed = true;
                    errors.push(name);
                    continue;
                }
                queue.pending.push_back((msg.id.clone(), now.clone()));
                // An arrival during an in-flight no-route lookup is also a wake request.
                // Otherwise the worker can park after this arrival without observing it.
                queue.route_generation = queue.route_generation.wrapping_add(1);
                // A new arrival can make a previously unavailable passive route discoverable.
                // Retry the oldest never-started item first, without reading stored history.
                if queue.waiting_for_route
                    && let Err(error) = self.start_queued_worker(&mut queues, &name)
                {
                    warn(&format!("{}: {error}", sanitize_for_warning(&name)));
                    errors.push(name);
                }
                continue;
            }
            if blocked.contains(&name) || self.overflow_capacity_exhausted.load(Ordering::SeqCst) {
                errors.push(name);
                continue;
            }
            if queues.len() >= MAX_TRACKED_RECIPIENTS {
                if blocked.len() < MAX_OVERFLOW_RECIPIENTS {
                    blocked.insert(name.clone());
                } else {
                    self.overflow_capacity_exhausted
                        .store(true, Ordering::SeqCst);
                }
                errors.push(name);
                continue;
            }
            let mut queue = RecipientDispatchQueue::default();
            queue.pending.push_back((msg.id.clone(), now.clone()));
            queues.insert(name.clone(), queue);
            if let Err(error) = self.start_queued_worker(&mut queues, &name) {
                warn(&format!(
                    "could not start delivery worker for {}: {error}",
                    sanitize_for_warning(&name)
                ));
                errors.push(name);
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            let names = errors
                .iter()
                .take(WARN_MAX_LISTED_SKIPS)
                .map(|name| sanitize_for_warning(name))
                .collect::<Vec<_>>()
                .join(", ");
            Err(format!(
                "message {} is stored; delivery remains pending for {} recipient(s): {names}",
                msg.id,
                errors.len()
            ))
        }
    }

    fn run_recipient_queue(self: Arc<Self>, recipient: String) {
        loop {
            let job = {
                let mut queues = lock(&self.dispatch_queues);
                if self.shutdown.load(Ordering::SeqCst) {
                    queues.remove(&recipient);
                    drop(queues);
                    self.wake_capacity_waiters();
                    return;
                }
                let Some(queue) = queues.get_mut(&recipient) else {
                    return;
                };
                let Some((id, now)) = queue.pending.pop_front() else {
                    #[cfg(test)]
                    if let Some((entered, resume)) = lock(&self.empty_exit_hook).take() {
                        let _ = entered.send(());
                        let _ = resume.recv_timeout(Duration::from_secs(5));
                    }
                    if queue.overflowed {
                        self.retain_overflow_fence(&recipient);
                    }
                    queues.remove(&recipient);
                    drop(queues);
                    self.wake_capacity_waiters();
                    return;
                };
                queue.in_flight = Some(id.clone());
                (id, now, queue.route_generation)
            };
            let outcome = match self.store.get_wire_message(&job.0) {
                Ok(Some(msg)) => std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    self.dispatch_and_propagate(&msg, &recipient, job.1.clone())
                }))
                .unwrap_or(DispatchOutcome::Unconfirmed),
                Ok(None) | Err(_) => DispatchOutcome::Unconfirmed,
            };
            #[cfg(test)]
            if matches!(outcome, DispatchOutcome::NoRoute) {
                let hook = lock(&self.no_route_hook).take();
                if let Some((entered, resume)) = hook {
                    let _ = entered.send(());
                    let _ = resume.recv_timeout(Duration::from_secs(5));
                }
            }
            let mut wait_for_route = false;
            if let Some(queue) = lock(&self.dispatch_queues).get_mut(&recipient) {
                queue.in_flight = None;
                match outcome {
                    DispatchOutcome::Resolved => {}
                    DispatchOutcome::NoRoute => {
                        queue.pending.push_front((job.0.clone(), job.1));
                        if queue.route_generation == job.2 {
                            queue.waiting_for_route = true;
                            queue.worker_active = false;
                            wait_for_route = true;
                        }
                    }
                    DispatchOutcome::Unconfirmed => {
                        // Never let a later message overtake an unconfirmed push.
                        queue.pending.clear();
                        queue.overflowed = true;
                        queue.worker_active = false;
                    }
                }
            }
            if wait_for_route {
                self.wake_capacity_waiters();
                return;
            }
            if matches!(outcome, DispatchOutcome::Unconfirmed) {
                // Move the fence while holding the queue map lock, so a concurrent enqueue
                // cannot start a replacement worker between removal and fencing.
                let mut queues = lock(&self.dispatch_queues);
                if queues.get(&recipient).is_some_and(|queue| queue.overflowed) {
                    self.retain_overflow_fence(&recipient);
                    queues.remove(&recipient);
                }
                drop(queues);
                warn(&format!(
                    "delivery unresolved for message {}; later messages for {} remain pending",
                    job.0,
                    sanitize_for_warning(&recipient)
                ));
                self.wake_capacity_waiters();
                return;
            }
        }
    }

    /// The caller holds `dispatch_queues`; this matches `schedule_dispatch` lock order.
    fn retain_overflow_fence(&self, recipient: &str) {
        let mut blocked = lock(&self.overflow_recipients);
        if blocked.contains(recipient) || blocked.len() < MAX_OVERFLOW_RECIPIENTS {
            blocked.insert(recipient.to_string());
        } else {
            self.overflow_capacity_exhausted
                .store(true, Ordering::SeqCst);
        }
    }

    /// Wake only work admitted by this daemon instance. A pre-startup Pending receipt may
    /// already have been delivered and must not be replayed from the store.
    fn wake_recipient_after_registration(self: &Arc<Self>, persona: &str) {
        let Some(recipient) = normalize_persona(persona) else {
            return;
        };
        let mut queues = lock(&self.dispatch_queues);
        let Some(queue) = queues.get_mut(&recipient) else {
            return;
        };
        queue.route_generation = queue.route_generation.wrapping_add(1);
        if !queue.waiting_for_route || queue.overflowed {
            return;
        }
        if let Err(error) = self.start_queued_worker(&mut queues, &recipient) {
            drop(queues);
            warn(&format!(
                "could not resume delivery worker for {}: {error}; messages remain in inbox",
                sanitize_for_warning(&recipient)
            ));
        }
    }

    fn handle_local_request(
        self: &Arc<Self>,
        req: LocalRequest,
    ) -> (LocalResponse, Option<Receiver<LocalEvent>>) {
        let now = now_at();
        match req {
            LocalRequest::Send { mut args } => {
                let _order = lock(&self.enqueue_gate);
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
                        if let Err(message) = self.schedule_dispatch(&msg, &receipts, now) {
                            warn(&message);
                        }
                        (
                            LocalResponse::Sent {
                                message: msg,
                                receipts,
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
            LocalRequest::RegisterRoute {
                persona,
                route,
                explicit,
            } => {
                // Publish a route only while ordered with persistence and backlog enqueue.
                // Otherwise a concurrent send could overtake older pending messages.
                let _order = lock(&self.enqueue_gate);
                {
                    let mut routes = lock(&self.routes);
                    let Some(norm) = normalize_persona(&persona) else {
                        return (
                            LocalResponse::Error {
                                message: "invalid persona".into(),
                            },
                            None,
                        );
                    };
                    if !explicit {
                        let conflicts_with_binding = routes.get(&norm).is_some_and(|entry| {
                            entry.explicit
                                && std::mem::discriminant(&entry.route)
                                    != std::mem::discriminant(&route)
                        });
                        let claims_bound_alias = route.session_identity().is_some_and(|identity| {
                            routes.entries.iter().any(|(name, entry)| {
                                name != &norm
                                    && entry.explicit
                                    && entry.route.session_identity() == Some(identity)
                            })
                        });
                        if conflicts_with_binding || claims_bound_alias {
                            return (
                                LocalResponse::Error {
                                    message: "observed route conflicts with an explicit binding"
                                        .into(),
                                },
                                None,
                            );
                        }
                    }
                    let before = routes.clone();
                    if explicit {
                        routes.upsert_explicit(&persona, route, now.clone());
                    } else {
                        routes.upsert(&persona, route, now.clone());
                    }
                    if let Err(error) = routes.save(self.config.routes_path.as_deref()) {
                        *routes = before;
                        return (
                            LocalResponse::Error {
                                message: format!("cannot save route: {error}"),
                            },
                            None,
                        );
                    }
                }
                self.wake_recipient_after_registration(&persona);
                (LocalResponse::RouteRegistered, None)
            }
            LocalRequest::UnregisterRoute { persona } => {
                let Some(persona) = normalize_persona(&persona) else {
                    return (
                        LocalResponse::Error {
                            message: "invalid persona".into(),
                        },
                        None,
                    );
                };
                let mut routes = lock(&self.routes);
                let before = routes.clone();
                routes.entries.remove(&persona);
                if let Err(error) = routes.save(self.config.routes_path.as_deref()) {
                    *routes = before;
                    return (
                        LocalResponse::Error {
                            message: format!("cannot save route removal: {error}"),
                        },
                        None,
                    );
                }
                (LocalResponse::RouteUnregistered, None)
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

    fn handle_peer_connection(self: &Arc<Self>, mut ws: WsStream) -> Result<(), WsError> {
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

    fn peer_label(&self, id: u64) -> String {
        lock(&self.peers)
            .get(&id)
            .map(|(label, _)| label.clone())
            .unwrap_or_else(|| format!("peer #{id}"))
    }

    /// Warn about a peer import that skipped an item or failed.
    fn warn_on_import(
        &self,
        from_peer_id: u64,
        frame: &str,
        result: &Result<WireImportReport, fl_core::store::StoreError>,
    ) {
        let peer = self.peer_label(from_peer_id);
        if let Some(line) = import_warning(&peer, frame, result) {
            warn(&line);
        }
    }

    fn process_incoming_peer_frame(self: &Arc<Self>, from_peer_id: u64, frame: PeerFrame) {
        let now = now_at();
        match frame {
            PeerFrame::SyncBatch {
                registry_events,
                messages,
                receipts,
            } => {
                let _order = lock(&self.enqueue_gate);
                if !registry_events.is_empty()
                    && let Err(e) = self.store.import_registry_events(&registry_events)
                {
                    let peer = self.peer_label(from_peer_id);
                    warn(&format!(
                        "SyncBatch from {peer}: registry import failed: {e}"
                    ));
                }
                let mut new_msgs = Vec::new();
                for m in &messages {
                    if self.store.get_wire_message(&m.id).ok().flatten().is_none() {
                        new_msgs.push(m.clone());
                    }
                }
                let imported = self.store.import_wire_batch(&messages, &receipts);
                self.warn_on_import(from_peer_id, "SyncBatch", &imported);
                // One bad item no longer drops the batch, so what is announced and dispatched
                // is exactly what was stored: a message the store refused is never forwarded
                // to subscribers or peers and never dispatched to a vendor CLI.
                if let Ok(report) = imported {
                    new_msgs.retain(|m| !report.rejects_message(&m.id));
                    for m in new_msgs {
                        let rcpts = self.store.list_wire_receipts(&m.id).unwrap_or_default();
                        self.notify_subscribers_message(&m, &rcpts);
                        self.broadcast_to_peers(
                            Some(from_peer_id),
                            &PeerFrame::Publish {
                                message: m.clone(),
                                receipts: rcpts.clone(),
                            },
                        );
                        if let Err(error) = self.schedule_dispatch(&m, &rcpts, now.clone()) {
                            warn(&format!("peer message scheduling failed: {error}"));
                        }
                    }
                    for r in receipts {
                        if !report.rejects(&r.message_id) {
                            self.notify_subscribers_receipt(&r);
                        }
                    }
                }
            }
            PeerFrame::Publish { message, receipts } => {
                let _order = lock(&self.enqueue_gate);
                let known = self
                    .store
                    .get_wire_message(&message.id)
                    .ok()
                    .flatten()
                    .is_some();
                let imported = self
                    .store
                    .import_wire_batch(std::slice::from_ref(&message), &receipts);
                self.warn_on_import(from_peer_id, "Publish", &imported);
                // A message the store refused is neither forwarded nor dispatched. A receipt
                // the store refused (already warned about) does not stop the message itself:
                // it is stored, and a later copy would see it as known and never dispatch it.
                let message_stored = imported
                    .as_ref()
                    .is_ok_and(|report| !report.rejects_message(&message.id));
                if message_stored && !known {
                    let rcpts = self
                        .store
                        .list_wire_receipts(&message.id)
                        .unwrap_or(receipts);
                    self.notify_subscribers_message(&message, &rcpts);
                    self.broadcast_to_peers(
                        Some(from_peer_id),
                        &PeerFrame::Publish {
                            message: message.clone(),
                            receipts: rcpts.clone(),
                        },
                    );
                    if let Err(error) = self.schedule_dispatch(&message, &rcpts, now) {
                        warn(&format!("peer message scheduling failed: {error}"));
                    }
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
                let imported = self
                    .store
                    .import_wire_batch(&[], std::slice::from_ref(&receipt));
                self.warn_on_import(from_peer_id, "ReceiptUpdate", &imported);
                if imported.as_ref().is_ok_and(WireImportReport::is_clean)
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
        // Connect-only liveness probe: it sends no request, so its short read/write timeout never applies.
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
        enqueue_gate: Mutex::new(()),
        dispatch_queues: Mutex::new(BTreeMap::new()),
        overflow_recipients: Mutex::new(BTreeSet::new()),
        overflow_capacity_exhausted: AtomicBool::new(false),
        #[cfg(test)]
        no_route_hook: Mutex::new(None),
        #[cfg(test)]
        empty_exit_hook: Mutex::new(None),
        peers: Mutex::new(BTreeMap::new()),
        subscribers: Mutex::new(Vec::new()),
        next_peer_id: Mutex::new(0),
        inbound_peers: AtomicUsize::new(0),
        shutdown: AtomicBool::new(false),
    });

    warn(
        "startup: pre-restart pending messages remain readable in the inbox; delivery may be unconfirmed",
    );

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
                    let _ = stream.set_read_timeout(Some(DEFAULT_LOCAL_RPC_TIMEOUT));
                    let _ = stream.set_write_timeout(Some(DEFAULT_LOCAL_RPC_TIMEOUT));
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

    thread_local! {
        /// The warning lines `warn` wrote on this thread.
        pub(super) static WARNINGS: std::cell::RefCell<Vec<String>> =
            const { std::cell::RefCell::new(Vec::new()) };
    }

    fn take_warnings() -> Vec<String> {
        WARNINGS.with(|w| std::mem::take(&mut *w.borrow_mut()))
    }

    fn send_args(subject: &str, body: &str, ms: u64) -> SendWireArgs {
        SendWireArgs {
            id: None,
            sender_id: None,
            sender_name: "synthetic-sender".into(),
            sender_host: Some("node-a".into()),
            to: WireAddress::AgentName("synthetic-recipient".into()),
            kind: MessageKind::Mail,
            urgency: MessageUrgency::Normal,
            durable: true,
            subject: subject.into(),
            body: body.into(),
            reply_to: None,
            thread_id: None,
            deadline_at: None,
            at: At::from_unix_millis(ms),
        }
    }

    /// A daemon core over a fresh store (no sockets), one "sender" peer and one "observer" peer
    /// whose channel shows what the daemon forwards.
    struct Rig {
        _tmp: TempDir,
        inner: Arc<DaemonInner<RedbStore>>,
        sender: u64,
        observed: Receiver<PeerFrame>,
        source: RedbStore,
    }

    fn rig() -> Rig {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(RedbStore::open(&tmp.path().join("dest.redb")).unwrap());
        let source = RedbStore::open(&tmp.path().join("src.redb")).unwrap();
        let inner = Arc::new(DaemonInner {
            store,
            config: DaemonConfig {
                node_id: "node-b".into(),
                host: "host-b".into(),
                socket_path: tmp.path().join("b.sock"),
                routes_path: Some(tmp.path().join("routes-b.json")),
                home_override: Some(tmp.path().to_path_buf()),
                listen_addr: None,
                peers: vec![],
                psk: b"top-secret-psk".to_vec(),
            },
            // The table of this rig's own `routes_path`: `load(None)` would read the machine's
            // real route table (and panics under test, see `refuse_default_route_path`).
            routes: Mutex::new(RouteTable::load(Some(&tmp.path().join("routes-b.json")))),
            enqueue_gate: Mutex::new(()),
            dispatch_queues: Mutex::new(BTreeMap::new()),
            overflow_recipients: Mutex::new(BTreeSet::new()),
            overflow_capacity_exhausted: AtomicBool::new(false),
            no_route_hook: Mutex::new(None),
            empty_exit_hook: Mutex::new(None),
            peers: Mutex::new(BTreeMap::new()),
            subscribers: Mutex::new(Vec::new()),
            next_peer_id: Mutex::new(0),
            inbound_peers: AtomicUsize::new(0),
            shutdown: AtomicBool::new(false),
        });
        let (tx, _unused) = mpsc::channel();
        let sender = inner.register_peer("peer-a".into(), tx);
        let (tx, observed) = mpsc::channel();
        inner.register_peer("observer".into(), tx);
        take_warnings();
        Rig {
            _tmp: tmp,
            inner,
            sender,
            observed,
            source,
        }
    }

    fn published_ids(rx: &Receiver<PeerFrame>) -> Vec<MessageId> {
        rx.try_iter()
            .filter_map(|f| match f {
                PeerFrame::Publish { message, .. } => Some(message.id),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_saturated_recipient_does_not_block_another() {
        let rig = rig();
        let first = rig.inner.store.send_wire(mail("slow", "overflow")).unwrap();
        let other = rig
            .inner
            .store
            .send_wire(mail("other", "independent"))
            .unwrap();
        let mut queue = RecipientDispatchQueue::default();
        for n in 1..=MAX_QUEUED_PER_RECIPIENT {
            queue.pending.push_back((
                MessageId(fl_core::ids::seq_iri(n as u64 + 10_000)),
                now_at(),
            ));
        }
        lock(&rig.inner.dispatch_queues).insert("slow".into(), queue);
        assert!(
            rig.inner
                .schedule_dispatch(&first.0, &first.1, now_at())
                .is_err()
        );
        assert!(
            rig.inner
                .schedule_dispatch(&other.0, &other.1, now_at())
                .is_ok()
        );
    }

    #[test]
    fn empty_overflowed_worker_releases_its_queue_and_preserves_fence() {
        let rig = rig();
        lock(&rig.inner.dispatch_queues).insert(
            "slow".into(),
            RecipientDispatchQueue {
                worker_active: true,
                overflowed: true,
                ..Default::default()
            },
        );
        Arc::clone(&rig.inner).run_recipient_queue("slow".into());
        assert!(!lock(&rig.inner.dispatch_queues).contains_key("slow"));
        assert!(lock(&rig.inner.overflow_recipients).contains("slow"));
    }

    #[test]
    fn enqueue_racing_empty_worker_exit_starts_a_new_worker() {
        let rig = rig();
        let socket = rig._tmp.path().join("empty-exit.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        lock(&rig.inner.routes).upsert_explicit(
            "live",
            VendorRoute::Webhook {
                socket_path: socket,
            },
            now_at(),
        );
        let (message, receipts) = rig.inner.store.send_wire(mail("live", "new")).unwrap();
        lock(&rig.inner.dispatch_queues).insert(
            "live".into(),
            RecipientDispatchQueue {
                worker_active: true,
                ..Default::default()
            },
        );
        let (entered_tx, entered_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        *lock(&rig.inner.empty_exit_hook) = Some((entered_tx, resume_rx));
        let owner = Arc::clone(&rig.inner);
        let exiting = std::thread::spawn(move || owner.run_recipient_queue("live".into()));
        entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let owner = Arc::clone(&rig.inner);
        let (starting_tx, starting_rx) = mpsc::channel();
        let enqueue = std::thread::spawn(move || {
            starting_tx.send(()).unwrap();
            owner.schedule_dispatch(&message, &receipts, now_at())
        });
        starting_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        resume_tx.send(()).unwrap();
        exiting.join().unwrap();
        enqueue.join().unwrap().unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if listener.accept().is_ok() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "enqueue was stranded during worker exit"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn parked_offline_recipients_do_not_exhaust_active_worker_slots() {
        let rig = rig();
        for n in 0..=MAX_DISPATCH_RECIPIENTS {
            lock(&rig.inner.dispatch_queues).insert(
                format!("offline-{n}"),
                RecipientDispatchQueue {
                    waiting_for_route: true,
                    ..Default::default()
                },
            );
        }
        let socket = rig._tmp.path().join("live.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        lock(&rig.inner.routes).upsert_explicit(
            "live",
            VendorRoute::Webhook {
                socket_path: socket,
            },
            now_at(),
        );
        let (response, _) = rig.inner.handle_local_request(LocalRequest::Send {
            args: mail("live", "independent"),
        });
        assert!(matches!(response, LocalResponse::Sent { .. }));
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if listener.accept().is_ok() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "parked queues blocked a live recipient"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_capacity_waiter_runs_when_a_worker_slot_is_released() {
        let rig = rig();
        for n in 0..MAX_DISPATCH_RECIPIENTS {
            lock(&rig.inner.dispatch_queues).insert(
                format!("active-{n}"),
                RecipientDispatchQueue {
                    worker_active: true,
                    ..Default::default()
                },
            );
        }
        let socket = rig._tmp.path().join("capacity.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        lock(&rig.inner.routes).upsert_explicit(
            "live",
            VendorRoute::Webhook {
                socket_path: socket,
            },
            now_at(),
        );
        let (response, _) = rig.inner.handle_local_request(LocalRequest::Send {
            args: mail("live", "waiting"),
        });
        assert!(matches!(response, LocalResponse::Sent { .. }));
        assert!(lock(&rig.inner.dispatch_queues)["live"].waiting_for_capacity);
        lock(&rig.inner.dispatch_queues).remove("active-0");
        rig.inner.wake_capacity_waiters();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if listener.accept().is_ok() {
                break;
            }
            assert!(Instant::now() < deadline, "capacity waiter never resumed");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn unresolved_first_delivery_fences_only_its_recipient() {
        let rig = rig();
        let slow_socket = rig._tmp.path().join("unavailable.sock");
        lock(&rig.inner.routes).upsert_explicit(
            "slow",
            VendorRoute::Webhook {
                socket_path: slow_socket.clone(),
            },
            now_at(),
        );
        let (response, _) = rig.inner.handle_local_request(LocalRequest::Send {
            args: mail("slow", "first"),
        });
        let LocalResponse::Sent { message: first, .. } = response else {
            panic!("send failed: {response:?}")
        };
        let deadline = Instant::now() + Duration::from_secs(3);
        while rig.inner.store.list_wire_receipts(&first.id).unwrap()[0].attempts == 0 {
            assert!(Instant::now() < deadline, "first push never failed");
            std::thread::sleep(Duration::from_millis(10));
        }
        let slow_listener = UnixListener::bind(&slow_socket).unwrap();
        slow_listener.set_nonblocking(true).unwrap();
        let fast_socket = rig._tmp.path().join("other.sock");
        let fast_listener = UnixListener::bind(&fast_socket).unwrap();
        fast_listener.set_nonblocking(true).unwrap();
        lock(&rig.inner.routes).upsert_explicit(
            "other",
            VendorRoute::Webhook {
                socket_path: fast_socket,
            },
            now_at(),
        );
        let (later, _) = rig.inner.handle_local_request(LocalRequest::Send {
            args: mail("slow", "second"),
        });
        assert!(matches!(later, LocalResponse::Sent { .. }));
        let (other, _) = rig.inner.handle_local_request(LocalRequest::Send {
            args: mail("other", "independent"),
        });
        assert!(matches!(other, LocalResponse::Sent { .. }));
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut other_received = false;
        while Instant::now() < deadline {
            if fast_listener.accept().is_ok() {
                other_received = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(other_received, "unrelated recipient did not receive");
        assert!(
            slow_listener.accept().is_err(),
            "second message overtook unresolved first delivery"
        );
        assert_eq!(
            rig.inner.store.list_wire_receipts(&first.id).unwrap()[0].status,
            DeliveryStatus::Pending
        );
    }

    #[test]
    fn saturated_send_is_acknowledged_as_stored_and_remains_readable() {
        let rig = rig();
        let mut queue = RecipientDispatchQueue::default();
        for n in 1..=MAX_QUEUED_PER_RECIPIENT {
            queue.pending.push_back((
                MessageId(fl_core::ids::seq_iri(n as u64 + 20_000)),
                now_at(),
            ));
        }
        lock(&rig.inner.dispatch_queues).insert("slow".into(), queue);
        let (response, _) = rig.inner.handle_local_request(LocalRequest::Send {
            args: mail("slow", "stored"),
        });
        let LocalResponse::Sent { message, .. } = response else {
            panic!("expected persisted message acknowledgement: {response:?}")
        };
        assert!(
            rig.inner
                .store
                .get_wire_message(&message.id)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn broadcast_still_delivers_other_recipients_if_one_is_saturated() {
        use fl_core::agent::{AgentRole, RegisterAgentArgs};
        let rig = rig();
        for name in ["slow", "fast"] {
            rig.inner
                .store
                .register_agent(RegisterAgentArgs {
                    display_name: name.into(),
                    email: None,
                    project: "test".into(),
                    role: AgentRole::Assistant,
                    extra_areas: vec![],
                    create_area: true,
                    replace: false,
                    at: now_at(),
                    by: "owner".into(),
                })
                .unwrap();
        }
        let socket = rig._tmp.path().join("broadcast-fast.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        lock(&rig.inner.routes).upsert_explicit(
            "fast",
            VendorRoute::Webhook {
                socket_path: socket,
            },
            now_at(),
        );
        let mut queue = RecipientDispatchQueue::default();
        for n in 1..=MAX_QUEUED_PER_RECIPIENT {
            queue.pending.push_back((
                MessageId(fl_core::ids::seq_iri(n as u64 + 30_000)),
                now_at(),
            ));
        }
        lock(&rig.inner.dispatch_queues).insert("slow".into(), queue);
        let mut args = mail("slow", "broadcast");
        args.to = WireAddress::All;
        let (response, _) = rig.inner.handle_local_request(LocalRequest::Send { args });
        assert!(matches!(response, LocalResponse::Sent { .. }));
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if listener.accept().is_ok() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "unrelated broadcast recipient did not receive"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn startup_warns_about_inbox_recovery_without_dispatch() {
        let tmp = TempDir::new().unwrap();
        let socket = tmp.path().join("saved-route.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let store = Arc::new(RedbStore::open(&tmp.path().join("s.redb")).unwrap());
        let (pending, _) = store.send_wire(mail("slow", "pending")).unwrap();
        let (done, _) = store.send_wire(mail("slow", "already delivered")).unwrap();
        store
            .update_wire_receipt(&done.id, "slow", DeliveryStatus::Acked, true, now_at())
            .unwrap();
        store.send_wire(mail("other", "other pending")).unwrap();
        let cfg = test_config(tmp.path(), "startup-pending");
        let mut routes = RouteTable::load(cfg.routes_path.as_deref());
        routes.upsert_explicit(
            "slow",
            VendorRoute::Webhook {
                socket_path: socket,
            },
            now_at(),
        );
        routes.save(cfg.routes_path.as_deref()).unwrap();
        take_warnings();
        let daemon_socket = cfg.socket_path.clone();
        let _daemon = spawn_daemon(Arc::clone(&store), cfg).unwrap();
        let warnings = take_warnings();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].contains("pre-restart pending messages remain readable in the inbox"),
            "{warnings:?}"
        );
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            listener.accept().is_err(),
            "startup must not replay ambiguous messages"
        );
        let mut client = LocalClient::connect(&daemon_socket, Duration::from_secs(2)).unwrap();
        client
            .request(&LocalRequest::RegisterRoute {
                persona: "slow".into(),
                explicit: true,
                route: VendorRoute::Webhook {
                    socket_path: tmp.path().join("saved-route.sock"),
                },
            })
            .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            listener.accept().is_err(),
            "registration must not replay pre-startup messages"
        );
        assert_eq!(
            store.get_wire_message(&pending.id).unwrap().unwrap().id,
            pending.id
        );
    }

    #[test]
    fn registration_wakes_only_current_process_unstarted_messages_in_fifo_order() {
        let rig = rig();
        let (first, _) = rig.inner.handle_local_request(LocalRequest::Send {
            args: mail("new-route", "first"),
        });
        let LocalResponse::Sent { message: first, .. } = first else {
            panic!("first send failed")
        };
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if lock(&rig.inner.dispatch_queues)
                .get("new-route")
                .is_some_and(|queue| queue.waiting_for_route)
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "first message did not wait for route"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let (second, _) = rig.inner.handle_local_request(LocalRequest::Send {
            args: mail("new-route", "second"),
        });
        let LocalResponse::Sent {
            message: second, ..
        } = second
        else {
            panic!("second send failed")
        };
        let socket = rig._tmp.path().join("late-route.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let (response, _) = rig.inner.handle_local_request(LocalRequest::RegisterRoute {
            persona: "new-route".into(),
            explicit: true,
            route: VendorRoute::Webhook {
                socket_path: socket,
            },
        });
        assert!(matches!(response, LocalResponse::RouteRegistered));
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut ids = Vec::new();
        while ids.len() < 2 && Instant::now() < deadline {
            if let Ok((stream, _)) = listener.accept() {
                let mut line = String::new();
                BufReader::new(stream).read_line(&mut line).unwrap();
                ids.push(serde_json::from_str::<WireMessage>(&line).unwrap().id);
            } else {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        assert_eq!(ids, vec![first.id, second.id]);
    }

    #[test]
    fn new_message_retries_current_process_queue_after_route_is_discovered() {
        let rig = rig();
        let (first, _) = rig.inner.handle_local_request(LocalRequest::Send {
            args: mail("passive-route", "first"),
        });
        let LocalResponse::Sent { message: first, .. } = first else {
            panic!("first send failed")
        };
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if lock(&rig.inner.dispatch_queues)
                .get("passive-route")
                .is_some_and(|queue| queue.waiting_for_route)
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "first message did not wait for route"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let socket = rig._tmp.path().join("passive-route.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        lock(&rig.inner.routes).upsert_explicit(
            "passive-route",
            VendorRoute::Webhook {
                socket_path: socket,
            },
            now_at(),
        );
        let (second, _) = rig.inner.handle_local_request(LocalRequest::Send {
            args: mail("passive-route", "second"),
        });
        let LocalResponse::Sent {
            message: second, ..
        } = second
        else {
            panic!("second send failed")
        };
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut ids = Vec::new();
        while ids.len() < 2 && Instant::now() < deadline {
            if let Ok((stream, _)) = listener.accept() {
                let mut line = String::new();
                BufReader::new(stream).read_line(&mut line).unwrap();
                ids.push(serde_json::from_str::<WireMessage>(&line).unwrap().id);
            } else {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        assert_eq!(ids, vec![first.id, second.id]);
    }

    #[test]
    fn registration_during_no_route_discovery_wakes_the_inflight_message() {
        let rig = rig();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        *lock(&rig.inner.no_route_hook) = Some((entered_tx, resume_rx));
        let (response, _) = rig.inner.handle_local_request(LocalRequest::Send {
            args: mail("racing-route", "first"),
        });
        let LocalResponse::Sent { message, .. } = response else {
            panic!("send failed")
        };
        entered_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("no-route discovery did not pause");
        let socket = rig._tmp.path().join("racing-route.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let (response, _) = rig.inner.handle_local_request(LocalRequest::RegisterRoute {
            persona: "racing-route".into(),
            explicit: true,
            route: VendorRoute::Webhook {
                socket_path: socket,
            },
        });
        assert!(matches!(response, LocalResponse::RouteRegistered));
        resume_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let stream = loop {
            if let Ok((stream, _)) = listener.accept() {
                break stream;
            }
            assert!(
                Instant::now() < deadline,
                "in-flight no-route message was stranded"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        assert_eq!(
            serde_json::from_str::<WireMessage>(&line).unwrap().id,
            message.id
        );
    }

    #[test]
    fn arrival_during_no_route_discovery_wakes_the_inflight_message() {
        let rig = rig();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        *lock(&rig.inner.no_route_hook) = Some((entered_tx, resume_rx));
        let (response, _) = rig.inner.handle_local_request(LocalRequest::Send {
            args: mail("passive-race", "first"),
        });
        let LocalResponse::Sent { message: first, .. } = response else {
            panic!("send failed")
        };
        entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let socket = rig._tmp.path().join("passive-race.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        // Model a route becoming passively discoverable, without RegisterRoute.
        lock(&rig.inner.routes).upsert_explicit(
            "passive-race",
            VendorRoute::Webhook {
                socket_path: socket,
            },
            now_at(),
        );
        let (response, _) = rig.inner.handle_local_request(LocalRequest::Send {
            args: mail("passive-race", "second"),
        });
        let LocalResponse::Sent {
            message: second, ..
        } = response
        else {
            panic!("second send failed")
        };
        resume_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut ids = Vec::new();
        while ids.len() < 2 && Instant::now() < deadline {
            if let Ok((stream, _)) = listener.accept() {
                let mut line = String::new();
                BufReader::new(stream).read_line(&mut line).unwrap();
                ids.push(serde_json::from_str::<WireMessage>(&line).unwrap().id);
            } else {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        assert_eq!(ids, [first.id, second.id]);
    }

    #[test]
    fn blocked_no_route_work_does_not_hold_global_sequencing_gate() {
        let rig = rig();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        *lock(&rig.inner.no_route_hook) = Some((entered_tx, resume_rx));
        rig.inner.handle_local_request(LocalRequest::Send {
            args: mail("blocked", "first"),
        });
        entered_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("no-route work did not pause");
        let inner = Arc::clone(&rig.inner);
        let (done_tx, done_rx) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            let (send, _) = inner.handle_local_request(LocalRequest::Send {
                args: mail("other", "independent"),
            });
            let (register, _) = inner.handle_local_request(LocalRequest::RegisterRoute {
                persona: "third".into(),
                explicit: true,
                route: VendorRoute::Codex {
                    thread_id: "thread".into(),
                    cwd: None,
                },
            });
            done_tx.send((send, register)).unwrap();
        });
        let answer = done_rx.recv_timeout(Duration::from_secs(1));
        resume_tx.send(()).unwrap();
        thread.join().unwrap();
        let (send, register) = answer.expect("unrelated RPCs waited on blocked no-route work");
        assert!(matches!(send, LocalResponse::Sent { .. }));
        assert!(matches!(register, LocalResponse::RouteRegistered));
    }

    #[test]
    fn registration_does_not_publish_route_before_backlog_sequencing() {
        let rig = rig();
        let gate = lock(&rig.inner.enqueue_gate);
        let inner = Arc::clone(&rig.inner);
        let (started_tx, started_rx) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            inner.handle_local_request(LocalRequest::RegisterRoute {
                persona: "new-persona".into(),
                route: VendorRoute::Codex {
                    thread_id: "test".into(),
                    cwd: None,
                },
                explicit: true,
            })
        });
        started_rx.recv().unwrap();
        let deadline = Instant::now() + Duration::from_millis(300);
        let mut published_early = false;
        while Instant::now() < deadline {
            if lock(&rig.inner.routes).get("new-persona").is_some() {
                published_early = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        drop(gate);
        let (response, _) = thread.join().unwrap();
        assert!(matches!(response, LocalResponse::RouteRegistered));
        assert!(
            !published_early,
            "registration published its route before backlog sequencing"
        );
    }

    #[test]
    fn worker_limit_fences_only_the_recipient_with_persisted_overflow() {
        let rig = rig();
        {
            let mut queues = lock(&rig.inner.dispatch_queues);
            for n in 0..MAX_TRACKED_RECIPIENTS {
                queues.insert(format!("occupied-{n}"), RecipientDispatchQueue::default());
            }
        }
        let (message, receipts) = rig.inner.store.send_wire(mail("slow", "overflow")).unwrap();
        let error = rig
            .inner
            .schedule_dispatch(&message, &receipts, now_at())
            .unwrap_err();
        assert!(error.contains(message.id.as_str()));
        assert!(lock(&rig.inner.overflow_recipients).contains("slow"));
        lock(&rig.inner.dispatch_queues).remove("occupied-0");
        let (later, later_receipts) = rig.inner.store.send_wire(mail("slow", "later")).unwrap();
        assert!(
            rig.inner
                .schedule_dispatch(&later, &later_receipts, now_at())
                .is_err()
        );
        let (other, other_receipts) = rig
            .inner
            .store
            .send_wire(mail("other", "independent"))
            .unwrap();
        assert!(
            rig.inner
                .schedule_dispatch(&other, &other_receipts, now_at())
                .is_ok()
        );
        assert!(
            rig.inner
                .store
                .get_wire_message(&message.id)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn overflow_fence_survives_worker_drain() {
        let rig = rig();
        lock(&rig.inner.dispatch_queues).insert(
            "slow".into(),
            RecipientDispatchQueue {
                overflowed: true,
                ..Default::default()
            },
        );
        Arc::clone(&rig.inner).run_recipient_queue("slow".into());
        let (later, receipts) = rig.inner.store.send_wire(mail("slow", "later")).unwrap();
        assert!(
            rig.inner
                .schedule_dispatch(&later, &receipts, now_at())
                .is_err()
        );
    }

    #[test]
    fn exhausted_overflow_metadata_fails_closed_for_new_recipients() {
        let rig = rig();
        for n in 0..MAX_TRACKED_RECIPIENTS {
            lock(&rig.inner.dispatch_queues)
                .insert(format!("active-{n}"), RecipientDispatchQueue::default());
        }
        for n in 0..MAX_OVERFLOW_RECIPIENTS {
            lock(&rig.inner.overflow_recipients).insert(format!("blocked-{n}"));
        }
        let (first, receipts) = rig
            .inner
            .store
            .send_wire(mail("unknown", "overflow"))
            .unwrap();
        assert!(
            rig.inner
                .schedule_dispatch(&first, &receipts, now_at())
                .is_err()
        );
        assert!(rig.inner.overflow_capacity_exhausted.load(Ordering::SeqCst));
        lock(&rig.inner.dispatch_queues).remove("active-0");
        let (later, receipts) = rig
            .inner
            .store
            .send_wire(mail("different", "later"))
            .unwrap();
        assert!(
            rig.inner
                .schedule_dispatch(&later, &receipts, now_at())
                .is_err()
        );
    }

    #[test]
    fn a_rig_daemon_registers_and_saves_routes_only_in_its_own_temp_dir() {
        let rig = rig();
        let path = rig._tmp.path().join("routes-b.json");
        assert!(rig.inner.routes.lock().unwrap().entries.is_empty());
        assert!(!path.exists());
        let (response, _) = rig.inner.handle_local_request(LocalRequest::RegisterRoute {
            persona: "synthetic-route-owner".into(),
            explicit: false,
            route: VendorRoute::Webhook {
                socket_path: rig._tmp.path().join("hook.sock"),
            },
        });
        assert!(matches!(response, LocalResponse::RouteRegistered));
        // The registration reached the rig's own file, and `RouteTable::load(None)` / `save(None)`
        // (the real machine table) would have panicked above: they refuse under test.
        assert!(
            RouteTable::load(Some(&path))
                .get("synthetic-route-owner")
                .is_some()
        );
    }

    #[test]
    fn explicit_registration_and_unregister_update_the_live_and_saved_table() {
        let rig = rig();
        let path = rig._tmp.path().join("routes-b.json");
        let (reply, _) = rig.inner.handle_local_request(LocalRequest::RegisterRoute {
            persona: "ada".into(),
            route: VendorRoute::Codex {
                thread_id: "new".into(),
                cwd: None,
            },
            explicit: true,
        });
        assert!(matches!(reply, LocalResponse::RouteRegistered));
        assert!(lock(&rig.inner.routes).get("ada").unwrap().explicit);
        assert!(RouteTable::load(Some(&path)).get("ada").unwrap().explicit);
        let (reply, _) = rig
            .inner
            .handle_local_request(LocalRequest::UnregisterRoute {
                persona: "ada".into(),
            });
        assert!(matches!(reply, LocalResponse::RouteUnregistered));
        assert!(lock(&rig.inner.routes).get("ada").is_none());
        assert!(RouteTable::load(Some(&path)).get("ada").is_none());
    }

    #[test]
    fn a_passive_cross_harness_observation_is_refused_by_the_daemon() {
        let rig = rig();
        let route = VendorRoute::Codex {
            thread_id: "current".into(),
            cwd: None,
        };
        let (reply, _) = rig.inner.handle_local_request(LocalRequest::RegisterRoute {
            persona: "ada".into(),
            route: route.clone(),
            explicit: true,
        });
        assert!(matches!(reply, LocalResponse::RouteRegistered));
        let (reply, _) = rig.inner.handle_local_request(LocalRequest::RegisterRoute {
            persona: "ada".into(),
            route: VendorRoute::Muse {
                session_id: "old".into(),
                cwd: None,
                tmux_target: None,
                tmux_pane: None,
            },
            explicit: false,
        });
        assert!(matches!(reply, LocalResponse::Error { .. }));
        assert_eq!(lock(&rig.inner.routes).get("ada").unwrap().route, route);
    }

    #[test]
    fn a_passive_alias_cannot_claim_an_explicit_session_but_same_vendor_refresh_works() {
        let rig = rig();
        let (reply, _) = rig.inner.handle_local_request(LocalRequest::RegisterRoute {
            persona: "ada".into(),
            route: VendorRoute::Codex {
                thread_id: "current".into(),
                cwd: None,
            },
            explicit: true,
        });
        assert!(matches!(reply, LocalResponse::RouteRegistered));
        let (reply, _) = rig.inner.handle_local_request(LocalRequest::RegisterRoute {
            persona: "old-alias".into(),
            route: VendorRoute::Codex {
                thread_id: "current".into(),
                cwd: None,
            },
            explicit: false,
        });
        assert!(matches!(reply, LocalResponse::Error { .. }));
        let (reply, _) = rig.inner.handle_local_request(LocalRequest::RegisterRoute {
            persona: "ada".into(),
            route: VendorRoute::Codex {
                thread_id: "refreshed".into(),
                cwd: None,
            },
            explicit: false,
        });
        assert!(matches!(reply, LocalResponse::RouteRegistered));
        let routes = lock(&rig.inner.routes);
        let entry = routes.get("ada").unwrap();
        assert!(entry.explicit);
        assert!(
            matches!(&entry.route, VendorRoute::Codex { thread_id, .. } if thread_id == "refreshed")
        );
        assert!(routes.get("old-alias").is_none());
    }

    #[test]
    fn register_route_wire_format_defaults_to_passive_and_accepts_explicit() {
        let old =
            r#"{"op":"register_route","persona":"ada","route":{"kind":"codex","thread_id":"t"}}"#;
        assert!(matches!(
            serde_json::from_str::<LocalRequest>(old).unwrap(),
            LocalRequest::RegisterRoute {
                explicit: false,
                ..
            }
        ));
        let new = r#"{"op":"register_route","persona":"ada","route":{"kind":"codex","thread_id":"t"},"explicit":true}"#;
        assert!(matches!(
            serde_json::from_str::<LocalRequest>(new).unwrap(),
            LocalRequest::RegisterRoute { explicit: true, .. }
        ));
    }

    #[test]
    fn a_bad_message_in_a_sync_batch_is_skipped_warned_and_never_forwarded() {
        let rig = rig();
        let store = &rig.inner.store;
        let (existing, _) = store.send_wire(send_args("held", "held body", 5)).unwrap();
        let (good_a, _) = rig
            .source
            .send_wire(send_args("one", "first good body", 10))
            .unwrap();
        let (good_b, _) = rig
            .source
            .send_wire(send_args("two", "second good body", 11))
            .unwrap();
        let (bad_new, _) = rig
            .source
            .send_wire(send_args("bad", "valid when sent", 12))
            .unwrap();
        let mut tampered = existing.clone();
        tampered.body = "SECRET tampered body".into();
        let mut invalid = bad_new.clone();
        invalid.body = "   ".into();

        rig.inner.process_incoming_peer_frame(
            rig.sender,
            PeerFrame::SyncBatch {
                registry_events: vec![],
                messages: vec![good_a.clone(), tampered, invalid.clone(), good_b.clone()],
                receipts: vec![],
            },
        );

        // Both good messages were imported around the two bad ones.
        assert_eq!(store.get_wire_message(&good_a.id).unwrap().unwrap(), good_a);
        assert_eq!(store.get_wire_message(&good_b.id).unwrap().unwrap(), good_b);
        // The tampered copy did not replace the stored one, and the invalid one was not stored.
        assert_eq!(
            store.get_wire_message(&existing.id).unwrap().unwrap(),
            existing
        );
        assert!(store.get_wire_message(&invalid.id).unwrap().is_none());
        // Only what was stored is forwarded; a refused message never reaches peers (or dispatch).
        let forwarded = published_ids(&rig.observed);
        assert_eq!(forwarded.len(), 2, "{forwarded:?}");
        assert!(forwarded.contains(&good_a.id) && forwarded.contains(&good_b.id));
        // One warning names the peer, the frame and both skipped ids, and never a body.
        let warnings = take_warnings();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        let line = &warnings[0];
        assert!(
            line.contains("SyncBatch") && line.contains("peer-a"),
            "{line}"
        );
        assert!(line.contains("skipped 2 item(s)"), "{line}");
        assert!(line.contains(existing.id.as_str()) && line.contains(invalid.id.as_str()));
        assert!(
            !line.contains("SECRET") && !line.contains("tampered body"),
            "{line}"
        );
    }

    #[test]
    fn a_clean_sync_batch_imports_everything_and_warns_about_nothing() {
        let rig = rig();
        let (a, _) = rig.source.send_wire(send_args("a", "body a", 10)).unwrap();
        let (b, _) = rig.source.send_wire(send_args("b", "body b", 11)).unwrap();
        rig.inner.process_incoming_peer_frame(
            rig.sender,
            PeerFrame::SyncBatch {
                registry_events: vec![],
                messages: vec![a.clone(), b.clone()],
                receipts: vec![],
            },
        );
        assert!(rig.inner.store.get_wire_message(&a.id).unwrap().is_some());
        assert!(rig.inner.store.get_wire_message(&b.id).unwrap().is_some());
        assert_eq!(published_ids(&rig.observed).len(), 2);
        assert!(take_warnings().is_empty());
    }

    #[test]
    fn a_single_published_message_that_fails_validation_is_not_forwarded() {
        let rig = rig();
        let (m, _) = rig.source.send_wire(send_args("x", "body x", 10)).unwrap();
        let mut invalid = m.clone();
        invalid.body = "  ".into();
        rig.inner.process_incoming_peer_frame(
            rig.sender,
            PeerFrame::Publish {
                message: invalid.clone(),
                receipts: vec![],
            },
        );
        assert!(
            rig.inner
                .store
                .get_wire_message(&invalid.id)
                .unwrap()
                .is_none()
        );
        assert!(published_ids(&rig.observed).is_empty());
        assert_eq!(take_warnings().len(), 1);
    }

    /// A subscriber on the rig's daemon that sees every message event.
    fn subscribe_all(rig: &Rig) -> Receiver<LocalEvent> {
        let (tx, rx) = mpsc::channel();
        lock(&rig.inner.subscribers).push(Subscriber {
            recipient: None,
            correlation_id: None,
            tx,
        });
        rx
    }

    fn message_events(rx: &Receiver<LocalEvent>) -> usize {
        rx.try_iter()
            .filter(|e| matches!(e, LocalEvent::Message { .. }))
            .count()
    }

    #[test]
    fn a_valid_publish_with_a_rejected_receipt_is_still_announced_once() {
        let rig = rig();
        let events = subscribe_all(&rig);
        let (m, _) = rig
            .source
            .send_wire(send_args("x", "valid body", 10))
            .unwrap();
        let (other, other_receipts) = rig
            .source
            .send_wire(send_args("y", "other body", 11))
            .unwrap();
        // A receipt for a message the destination has never seen: the store refuses it.
        let stray = other_receipts[0].clone();
        assert_eq!(stray.message_id, other.id);
        let publish = PeerFrame::Publish {
            message: m.clone(),
            receipts: vec![stray],
        };

        rig.inner
            .process_incoming_peer_frame(rig.sender, publish.clone());

        assert_eq!(rig.inner.store.get_wire_message(&m.id).unwrap().unwrap(), m);
        assert_eq!(published_ids(&rig.observed), vec![m.id.clone()]);
        assert_eq!(message_events(&events), 1);
        let warnings = take_warnings();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].contains("receipt for") && warnings[0].contains(other.id.as_str()),
            "{warnings:?}"
        );

        // The same publish arriving again is known: it is not announced or forwarded twice.
        rig.inner.process_incoming_peer_frame(rig.sender, publish);
        assert!(published_ids(&rig.observed).is_empty());
        assert_eq!(message_events(&events), 0);
    }

    #[test]
    fn a_warning_escapes_peer_controlled_text_and_caps_what_it_lists() {
        use fl_core::ferrowire::{WireImportRejectReason, WireImportRejection};
        let evil_peer = "node\u{1b}[31m@host\nforged: line";
        let id = |text: String| MessageId(fl_core::iri::Iri::parse(&text).unwrap());
        let long_id = id(format!("urn:m:{}", "z".repeat(500)));
        let rejected: Vec<WireImportRejection> = (0..25)
            .map(|i| WireImportRejection {
                message_id: if i == 0 {
                    long_id.clone()
                } else {
                    id(format!("urn:m:msg-{i}\u{1b}[2J"))
                },
                is_receipt: i % 2 == 1,
                reason: WireImportRejectReason::InvalidMessage,
                detail: "SECRET body text".into(),
            })
            .collect();
        let report = WireImportReport {
            imported_messages: 3,
            imported_receipts: 4,
            rejected,
        };

        let line = import_warning(evil_peer, "SyncBatch", &Ok(report)).unwrap();

        assert!(!line.chars().any(char::is_control), "{line:?}");
        assert!(line.contains("\\u{1b}") && line.contains("\\n"), "{line}");
        assert!(line.contains("skipped 25 item(s)"), "{line}");
        assert!(line.contains("and 15 more"), "{line}");
        assert!(line.contains("msg-9") && !line.contains("msg-10"), "{line}");
        assert!(!line.contains(&"z".repeat(100)), "{line}");
        assert!(line.contains('\u{2026}'), "{line}");
        assert!(!line.contains("SECRET"), "{line}");
        // The whole line stays bounded however long the peer-supplied text was.
        assert!(line.chars().count() < 1500, "{}", line.chars().count());
    }

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
                sender_name: "synthetic-sender".into(),
                sender_host: Some("node-a".into()),
                to: WireAddress::AgentName("synthetic-recipient".into()),
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
                persona: "synthetic-recipient".into(),
                explicit: false,
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
                    sender_name: "synthetic-sender".into(),
                    sender_host: None,
                    to: WireAddress::AgentName("synthetic-recipient".into()),
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
            .subscribe(Some("synthetic-sender".into()), Some(ask_msg.id.clone()))
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
                    sender_name: "synthetic-recipient".into(),
                    sender_host: None,
                    to: WireAddress::AgentName("synthetic-sender".into()),
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
            sender_name: "synthetic-sender".into(),
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

    /// A dispatch runs route discovery on a working copy; routes it prunes (a derived Claude
    /// session name, a dead pid) must leave the live table, `Status` and the saved file too.
    #[test]
    fn a_route_pruned_during_dispatch_leaves_status_and_the_saved_file() {
        let tmp = TempDir::new().unwrap();
        let sessions = tmp.path().join(".claude").join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        fs::write(
            sessions.join("4242.json"),
            r#"{"sessionId":"s-derived","name":"lmcfarlin-e6","nameSource":"derived"}"#,
        )
        .unwrap();
        let store = Arc::new(RedbStore::open(&tmp.path().join("s.redb")).unwrap());
        let cfg = test_config(tmp.path(), "prune");
        let sock = cfg.socket_path.clone();
        let routes_file = cfg.routes_path.clone().unwrap();
        let _daemon = spawn_daemon(Arc::clone(&store), cfg).unwrap();
        let request = |req: LocalRequest| {
            LocalClient::connect(&sock, Duration::from_secs(5))
                .unwrap()
                .request(&req)
                .unwrap()
        };
        let status_personas = || match request(LocalRequest::Status) {
            LocalResponse::Status(st) => {
                st.routes.into_iter().map(|r| r.persona).collect::<Vec<_>>()
            }
            other => panic!("unexpected response: {other:?}"),
        };
        request(LocalRequest::RegisterRoute {
            persona: "lmcfarlin-e6".into(),
            explicit: false,
            route: VendorRoute::Claude {
                pid: 4242,
                session_id: "s-derived".into(),
                socket_path: tmp.path().join("claude.sock"),
                key_path: None,
            },
        });
        assert_eq!(status_personas(), vec!["lmcfarlin-e6"]);

        // Any send runs a dispatch. With a home override every dispatch discovers (in production
        // the first one does, then one per `ROUTE_REDISCOVERY_INTERVAL`); that discovery prunes
        // the derived route.
        request(LocalRequest::Send {
            args: mail("synthetic-recipient", "prune"),
        });
        let deadline = Instant::now() + Duration::from_secs(3);
        while !status_personas().is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            status_personas().is_empty(),
            "the pruned route must not linger in Status"
        );
        // The next RegisterRoute saves the live table; the pruned route must not come back.
        request(LocalRequest::RegisterRoute {
            persona: "other".into(),
            explicit: false,
            route: VendorRoute::Codex {
                thread_id: "t".into(),
                cwd: None,
            },
        });
        let saved = fs::read_to_string(&routes_file).unwrap();
        assert!(saved.contains("other"), "{saved}");
        assert!(!saved.contains("lmcfarlin-e6"), "{saved}");
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
        let started_file = tmp.path().join("started");
        let done_file = tmp.path().join("done");
        fs::write(
            &slow,
            format!(
                "#!/bin/sh\nprintf started > '{}'\nsleep 2\nprintf done > '{}'\n",
                started_file.display(),
                done_file.display()
            ),
        )
        .unwrap();
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
            persona: "synthetic-recipient".into(),
            explicit: false,
            route: VendorRoute::Codex {
                thread_id: "t1".into(),
                cwd: None,
            },
        })
        .unwrap();

        let send_sock = sock.clone();
        let (sent_tx, sent_rx) = mpsc::channel();
        let sender = std::thread::spawn(move || {
            let mut c = LocalClient::connect(&send_sock, Duration::from_secs(10)).unwrap();
            let result = c
                .request(&LocalRequest::Send {
                    args: mail("synthetic-recipient", "slow one"),
                })
                .map(|_| ());
            let _ = sent_tx.send(result);
        });
        let sent_promptly = sent_rx.recv_timeout(Duration::from_millis(400));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !started_file.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(started_file.exists(), "slow delivery did not start");

        let started = Instant::now();
        let mut probe = LocalClient::connect(&sock, Duration::from_secs(5)).unwrap();
        probe.request(&LocalRequest::Status).unwrap();
        let other_socket = tmp.path().join("other.sock");
        let other_listener = UnixListener::bind(&other_socket).unwrap();
        other_listener.set_nonblocking(true).unwrap();
        // The daemon serves one request per connection.
        let mut probe = LocalClient::connect(&sock, Duration::from_secs(5)).unwrap();
        probe
            .request(&LocalRequest::RegisterRoute {
                persona: "other".into(),
                explicit: false,
                route: VendorRoute::Webhook {
                    socket_path: other_socket,
                },
            })
            .unwrap();
        let mut other = LocalClient::connect(&sock, Duration::from_secs(1)).unwrap();
        other
            .request(&LocalRequest::Send {
                args: mail("other", "fast one"),
            })
            .unwrap();
        let other_deadline = Instant::now() + Duration::from_millis(800);
        let mut delivered_other = false;
        while Instant::now() < other_deadline {
            if other_listener.accept().is_ok() {
                delivered_other = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            delivered_other,
            "other recipient did not receive while slow recipient ran"
        );
        let waited = started.elapsed();

        sender.join().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done_file.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(done_file.exists(), "slow delivery did not finish");
        unsafe {
            match prev {
                Some(v) => std::env::set_var("FL_WIRE_CODEX_BIN", v),
                None => std::env::remove_var("FL_WIRE_CODEX_BIN"),
            }
        }
        assert!(
            matches!(sent_promptly, Ok(Ok(()))),
            "send waited for vendor delivery"
        );
        assert!(
            waited < Duration::from_millis(1200),
            "Status/RegisterRoute waited {waited:?} behind a vendor delivery"
        );
    }

    #[test]
    fn a_recipients_blocked_delivery_preserves_fifo() {
        use std::os::unix::fs::PermissionsExt;
        let _lock = crate::dispatcher::CODEX_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        let script = tmp.path().join("codex.sh");
        let log = tmp.path().join("log");
        let release = tmp.path().join("release");
        fs::write(&script, format!("#!/bin/sh\nprintf '%s\\n' \"$5\" >> '{}'\ncase \"$5\" in *first*) while [ ! -e '{}' ]; do sleep 0.02; done ;; esac\n", log.display(), release.display())).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let prev = std::env::var_os("FL_WIRE_CODEX_BIN");
        unsafe { std::env::set_var("FL_WIRE_CODEX_BIN", &script) };
        let store = Arc::new(RedbStore::open(&tmp.path().join("s.redb")).unwrap());
        let cfg = test_config(tmp.path(), "fifo");
        let sock = cfg.socket_path.clone();
        let _daemon = spawn_daemon(store, cfg).unwrap();
        LocalClient::connect(&sock, Duration::from_secs(2))
            .unwrap()
            .request(&LocalRequest::RegisterRoute {
                persona: "synthetic-recipient".into(),
                explicit: false,
                route: VendorRoute::Codex {
                    thread_id: "t".into(),
                    cwd: None,
                },
            })
            .unwrap();
        for subject in ["first", "second"] {
            LocalClient::connect(&sock, Duration::from_secs(2))
                .unwrap()
                .request(&LocalRequest::Send {
                    args: mail("synthetic-recipient", subject),
                })
                .unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while !fs::read_to_string(&log)
            .unwrap_or_default()
            .contains("first")
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        let before_release = fs::read_to_string(&log).unwrap_or_default();
        fs::write(&release, "go").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !fs::read_to_string(&log)
            .unwrap_or_default()
            .contains("second")
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        let after_release = fs::read_to_string(&log).unwrap_or_default();
        unsafe {
            match prev {
                Some(v) => std::env::set_var("FL_WIRE_CODEX_BIN", v),
                None => std::env::remove_var("FL_WIRE_CODEX_BIN"),
            }
        }
        assert!(
            before_release.contains("first"),
            "first delivery never began: {before_release}"
        );
        assert!(
            !before_release.contains("second"),
            "second delivery overtook the first: {before_release}"
        );
        assert!(
            after_release.contains("second"),
            "second delivery never began: {after_release}"
        );
        assert!(after_release.find("first") < after_release.find("second"));
    }

    #[test]
    fn a_broadcast_delivers_to_other_recipients_while_one_is_blocked() {
        use fl_core::agent::{AgentRole, RegisterAgentArgs};
        use std::os::unix::fs::PermissionsExt;
        let _lock = crate::dispatcher::CODEX_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        let script = tmp.path().join("codex.sh");
        let started = tmp.path().join("started");
        let release = tmp.path().join("release");
        let done = tmp.path().join("done");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\ntouch '{}'\nwhile [ ! -e '{}' ]; do sleep 0.02; done\ntouch '{}'\n",
                started.display(),
                release.display(),
                done.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let prev = std::env::var_os("FL_WIRE_CODEX_BIN");
        unsafe { std::env::set_var("FL_WIRE_CODEX_BIN", &script) };
        let store = Arc::new(RedbStore::open(&tmp.path().join("s.redb")).unwrap());
        for name in ["slow", "fast"] {
            store
                .register_agent(RegisterAgentArgs {
                    display_name: name.into(),
                    email: None,
                    project: "test".into(),
                    role: AgentRole::Assistant,
                    extra_areas: vec![],
                    create_area: true,
                    replace: false,
                    at: At::from_unix_millis(1),
                    by: "owner".into(),
                })
                .unwrap();
        }
        let cfg = test_config(tmp.path(), "group");
        let sock = cfg.socket_path.clone();
        let _daemon = spawn_daemon(Arc::clone(&store), cfg).unwrap();
        let fast_socket = tmp.path().join("fast.sock");
        let listener = UnixListener::bind(&fast_socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        for (persona, route) in [
            (
                "slow",
                VendorRoute::Codex {
                    thread_id: "t".into(),
                    cwd: None,
                },
            ),
            (
                "fast",
                VendorRoute::Webhook {
                    socket_path: fast_socket,
                },
            ),
        ] {
            LocalClient::connect(&sock, Duration::from_secs(2))
                .unwrap()
                .request(&LocalRequest::RegisterRoute {
                    persona: persona.into(),
                    explicit: false,
                    route,
                })
                .unwrap();
        }
        let mut args = mail("slow", "broadcast");
        args.to = WireAddress::All;
        LocalClient::connect(&sock, Duration::from_secs(2))
            .unwrap()
            .request(&LocalRequest::Send { args })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut fast_received = false;
        while Instant::now() < deadline {
            fast_received |= listener.accept().is_ok();
            if fast_received && started.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let slow_started = started.exists();
        fs::write(&release, "go").unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !done.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        unsafe {
            match prev {
                Some(v) => std::env::set_var("FL_WIRE_CODEX_BIN", v),
                None => std::env::remove_var("FL_WIRE_CODEX_BIN"),
            }
        }
        assert!(slow_started, "slow recipient did not start");
        assert!(
            fast_received,
            "broadcast fast recipient waited for slow recipient"
        );
        assert!(done.exists(), "slow recipient did not finish");
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
        let r = spawn_daemon(store, cfg);
        assert!(r.is_ok(), "{:?}", r.err());
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

    /// Flake cause (reproduced, see the audit report): another test thread forks a child
    /// (`Command::spawn`) while this test's listener exists. The child holds a copy of the
    /// listener fd until it execs, so after the listener is dropped the socket file still
    /// accepts connections for a short while, `spawn_daemon` correctly reports `AddrInUse`, and
    /// a one-shot assertion fails under load. A socket that only a leaked fd keeps alive becomes
    /// stale once the child execs, so the replacement is retried within a bound.
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
        let deadline = Instant::now() + Duration::from_secs(10);
        let replaced = loop {
            match spawn_daemon(Arc::clone(&store), cfg.clone()) {
                Err(e) if e.kind() == io::ErrorKind::AddrInUse && Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                other => break other,
            }
        };
        assert!(replaced.is_ok(), "{:?}", replaced.err());
    }

    #[test]
    fn a_peer_with_the_wrong_psk_is_refused_and_receives_no_messages() {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(RedbStore::open(&tmp.path().join("s.redb")).unwrap());
        store
            .send_wire(mail("synthetic-recipient", "secret subject"))
            .unwrap();
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
        sub.subscribe(Some("SYNTHETIC-RECIPIENT".into()), None)
            .unwrap();
        let mut other = LocalClient::connect(&sock, Duration::from_secs(2)).unwrap();
        other.subscribe(Some("someone-else".into()), None).unwrap();

        let mut sender = LocalClient::connect(&sock, Duration::from_secs(2)).unwrap();
        sender
            .request(&LocalRequest::Send {
                args: mail("synthetic-recipient", "for synthetic-recipient"),
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

    #[test]
    fn a_request_to_a_slow_daemon_succeeds_with_the_default_timeout() {
        let dir = TempDir::new().unwrap();
        let sock = dir.path().join("slow.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let fake = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert!(line.contains("Status") || line.contains("status"), "{line}");
            // A daemon whose store is remote takes this long, or longer.
            std::thread::sleep(Duration::from_millis(200));
            let mut writer = stream;
            let mut reply = serde_json::to_string(&LocalResponse::RouteRegistered).unwrap();
            reply.push('\n');
            writer.write_all(reply.as_bytes()).unwrap();
        });
        let mut client = LocalClient::connect(&sock, DEFAULT_LOCAL_RPC_TIMEOUT).unwrap();
        let resp = client.request(&LocalRequest::Status);
        fake.join().unwrap();
        assert!(
            matches!(resp, Ok(LocalResponse::RouteRegistered)),
            "{resp:?}"
        );
    }

    #[test]
    fn a_connect_to_a_missing_or_refused_socket_fails_at_once() {
        let dir = TempDir::new().unwrap();
        // A socket file that nothing listens on any more refuses the connect.
        let stale = dir.path().join("stale.sock");
        drop(UnixListener::bind(&stale).unwrap());
        for sock in [dir.path().join("none.sock"), stale] {
            let started = Instant::now();
            let err = LocalClient::connect(&sock, DEFAULT_LOCAL_RPC_TIMEOUT);
            assert!(err.is_err(), "{}", sock.display());
            assert!(
                started.elapsed() < Duration::from_secs(1),
                "{:?}",
                started.elapsed()
            );
        }
    }
}
