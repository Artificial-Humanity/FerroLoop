//! Local Unix domain socket server/client (`fl-wire.sock`) and multi-peer WebSocket relay daemon (spec §3.3, §3.4).
//!
//! After a crash, an accepted pending message may or may not have been pushed before its
//! receipt was updated. Startup does not scan or replay these ambiguous messages; they remain
//! readable in the inbox for manual recovery. A no-route delivery likewise stays in the inbox
//! after its worker exits; a later push can overtake it. A live recipient worker preserves its
//! queued push order. Durable backlog ordering and dispatch claims are deferred to Record #143.
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
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
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

pub use crate::codex_control::{
    CODEX_QUEUE_INSPECT_TIMEOUT, CODEX_QUEUE_LIST_LIMIT, CodexQueueInspection,
    CodexRouteQueueStatus, VERIFIED_CODEX_QUEUE_RPC_VERSION, parse_uuid_v7_unix_ms,
};
use crate::dispatch_diagnostics::{
    DiagnosticsState, DispatchCallOutcome, DispatchDiagnostics, DispatchReport, FailureStage,
    FenceDiagnostic, FenceReason, RouteRegistrationDiagnostic, SuppressionPhase,
};
use crate::dispatcher::dispatch_message_for_recipient_with_report;
use crate::identity::{RouteEntry, RouteTable, VendorRoute, normalize_persona};
use crate::peer::{PeerFrame, handshake_peer_with_cursors};
use crate::ws::{MAX_FRAME_PAYLOAD, WsError, WsStream};

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

pub fn resolve_socket_path(
    explicit: Option<&Path>,
    configured: Option<&Path>,
    explicit_db: Option<&Path>,
) -> PathBuf {
    if let Some(p) = explicit {
        return p.to_path_buf();
    }
    if let Some(s) = std::env::var_os("FL_WIRE_SOCKET") {
        let p = PathBuf::from(s);
        if p.is_absolute() {
            return p;
        }
    }
    if let Some(p) = configured {
        return p.to_path_buf();
    }
    if let Some(dir) = explicit_db
        .and_then(|p| p.parent())
        .filter(|d| !d.as_os_str().is_empty())
    {
        return dir.join("fl-wire.sock");
    }
    default_socket_path()
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
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        replace: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hosted_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hydration_generation: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        hosted_session_verified: bool,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        unverified_process: bool,
    },
    RecordRegistrationDiagnostic {
        diagnostic: RouteRegistrationDiagnostic,
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
    Status {
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        include_codex_queues: bool,
    },
    Shutdown,
}

impl LocalRequest {
    pub fn register(persona: impl Into<String>, route: VendorRoute, explicit: bool) -> Self {
        Self::RegisterRoute {
            persona: persona.into(),
            route,
            explicit,
            replace: false,
            hosted_id: None,
            hydration_generation: None,
            hosted_session_verified: false,
            unverified_process: false,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PeerCred {
    pub pid: Option<u32>,
    pub ppid: Option<u32>,
    pub start_time: Option<u64>,
}

fn extract_peer_cred(stream: &UnixStream) -> PeerCred {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        let rc = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&raw mut cred).cast::<libc::c_void>(),
                &raw mut len,
            )
        };
        if rc == 0 && len == std::mem::size_of::<libc::ucred>() as libc::socklen_t && cred.pid > 0 {
            let pid = cred.pid as u32;
            let (ppid, start_time) = crate::identity::proc_ppid_and_start_time(pid)
                .map(|(p, st)| (Some(p), Some(st)))
                .unwrap_or((None, None));
            return PeerCred {
                pid: Some(pid),
                ppid,
                start_time,
            };
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = stream;
    }
    PeerCred::default()
}

fn verify_peer_cred_for_hosted_claim(
    peer_cred: PeerCred,
    client_claimed_verified: bool,
    replace: bool,
    challenger_route: &VendorRoute,
    challenger_generation: Option<&str>,
    incumbent: Option<&RouteEntry>,
    incumbent_is_dead: bool,
) -> bool {
    if !client_claimed_verified || replace {
        return client_claimed_verified;
    }
    let Some(peer_pid) = peer_cred.pid else {
        return client_claimed_verified;
    };
    if let Some(challenger_pid) = challenger_route.route_pid()
        && !crate::identity::pid_is_self_or_ancestor(peer_pid, challenger_pid, None)
    {
        return false;
    }
    if let Some(inc) = incumbent
        && inc.explicit
        && !incumbent_is_dead
        && let Some(inc_pid) = inc.route.route_pid()
    {
        let generation_rotated = inc.hydration_generation.is_some()
            && challenger_generation.is_some()
            && inc.hydration_generation.as_deref() != challenger_generation;
        if !generation_rotated {
            if let Some(challenger_pid) = challenger_route.route_pid()
                && challenger_pid != inc_pid
            {
                return false;
            }
            if !crate::identity::pid_is_self_or_ancestor(peer_pid, inc_pid, inc.pid_start_time) {
                return false;
            }
        }
    }
    true
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
    RegistrationDiagnosticRecorded,
    RouteUnregistered,
    Subscribed,
    Status(DaemonStatus),
    ShuttingDown,
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingHoldReason {
    DeadExplicitRoute,
    #[serde(other)]
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecipientPendingSummary {
    pub recipient: String,
    pub pending_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oldest_pending_at: Option<At>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oldest_pending_age_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold_reason: Option<PendingHoldReason>,
}

pub fn annotate_dead_explicit_pending_holds(
    pending: &mut [RecipientPendingSummary],
    routes: &RouteTable,
    allow_host_probes: bool,
) {
    for row in pending {
        if row.pending_count == 0 {
            continue;
        }
        if let Some(entry) = routes.get(&row.recipient)
            && entry.explicit
            && crate::identity::explicit_route_entry_is_dead(
                entry,
                Some(&row.recipient),
                allow_host_probes,
            )
        {
            row.hold_reason = Some(PendingHoldReason::DeadExplicitRoute);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub node_id: String,
    pub host: String,
    pub socket_path: PathBuf,
    pub listen_addr: Option<String>,
    pub connected_peers: Vec<String>,
    pub routes: Vec<RouteEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_diagnostics: Option<DispatchDiagnostics>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub registration_diagnostics: Vec<RouteRegistrationDiagnostic>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_recipients: Vec<RecipientPendingSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub codex_queues: Vec<CodexRouteQueueStatus>,
}

/// Inspect Codex app-server queue depth/age for all registered Codex routes in `routes`.
///
/// Must be called outside any daemon state lock: performs read-only WebSocket RPC over the
/// Codex control socket, reusing a single handshake when multiple Codex routes share the socket.
pub fn collect_codex_route_queues(
    routes: &[RouteEntry],
    registry: Option<&dyn AgentRegistry>,
    home_override: Option<&Path>,
    now_ms: u64,
) -> Vec<CodexRouteQueueStatus> {
    struct RouteSnapshot {
        persona: String,
        project: Option<String>,
        thread_id: Option<String>,
    }

    let mut items = Vec::new();
    for entry in routes {
        let VendorRoute::Codex { thread_id, .. } = &entry.route else {
            continue;
        };
        let trimmed_tid = thread_id.trim();
        let project = registry
            .and_then(|reg| reg.agent_by_name(&entry.persona).ok().flatten())
            .map(|agent| agent.project)
            .filter(|p| !p.trim().is_empty());
        items.push(RouteSnapshot {
            persona: entry.persona.clone(),
            project,
            thread_id: (!trimmed_tid.is_empty()).then(|| trimmed_tid.to_string()),
        });
    }
    if items.is_empty() {
        return Vec::new();
    }

    let resolved_sock = if let Some(home) = home_override {
        crate::codex_control::resolve_control_socket(None, None, None, Some(home.as_os_str()))
    } else {
        crate::codex_control::resolve_control_socket(
            std::env::var_os("FL_WIRE_CODEX_CONTROL_SOCK").as_deref(),
            std::env::var_os("FL_WIRE_CODEX_BIN").as_deref(),
            std::env::var_os("CODEX_HOME").as_deref(),
            std::env::var_os("HOME").as_deref(),
        )
    };

    match resolved_sock {
        Ok(Some(sock)) => {
            let thread_refs: Vec<&str> = items
                .iter()
                .map(|item| item.thread_id.as_deref().unwrap_or(""))
                .collect();
            let inspections = crate::codex_control::inspect_thread_queues(
                &sock,
                &thread_refs,
                now_ms,
                CODEX_QUEUE_INSPECT_TIMEOUT,
            );
            items
                .into_iter()
                .zip(inspections)
                .map(|(item, queue)| CodexRouteQueueStatus {
                    persona: item.persona,
                    project: item.project,
                    thread_id: item.thread_id,
                    queue,
                })
                .collect()
        }
        Ok(None) => items
            .into_iter()
            .map(|item| {
                let reason = if item.thread_id.is_none() {
                    "Codex route has no thread_id"
                } else {
                    "Codex control socket disabled while FL_WIRE_CODEX_BIN is set without FL_WIRE_CODEX_CONTROL_SOCK"
                };
                CodexRouteQueueStatus {
                    persona: item.persona,
                    project: item.project,
                    thread_id: item.thread_id,
                    queue: CodexQueueInspection::unknown(reason, None),
                }
            })
            .collect(),
        Err(err) => {
            let safe_err = if err.starts_with("CODEX_HOME must be an absolute path") {
                "CODEX_HOME must be an absolute path".to_string()
            } else {
                err
            };
            items
                .into_iter()
                .map(|item| {
                    let reason = if item.thread_id.is_none() {
                        "Codex route has no thread_id".to_string()
                    } else {
                        safe_err.clone()
                    };
                    CodexRouteQueueStatus {
                        persona: item.persona,
                        project: item.project,
                        thread_id: item.thread_id,
                        queue: CodexQueueInspection::unknown(reason, None),
                    }
                })
                .collect()
        }
    }
}

pub fn collect_pending_recipient_summaries<S: MessageStore>(
    store: &S,
    now: &At,
) -> Vec<RecipientPendingSummary> {
    summarize_pending_map(&seed_pending_map_from_store(store), now)
}

fn seed_pending_map_from_store<S: MessageStore>(
    store: &S,
) -> BTreeMap<String, BTreeMap<MessageId, At>> {
    let Ok(items) = store.list_wire_inbox(&WireFilter {
        unread_only: true,
        ..Default::default()
    }) else {
        return BTreeMap::new();
    };
    let mut by_recipient = BTreeMap::<String, BTreeMap<MessageId, At>>::new();
    for item in items {
        if matches!(
            item.receipt.status,
            DeliveryStatus::Pending | DeliveryStatus::Delivered
        ) {
            let key = normalize_persona(&item.receipt.recipient_name)
                .unwrap_or_else(|| item.receipt.recipient_name.trim().to_ascii_lowercase());
            if !key.is_empty() {
                by_recipient
                    .entry(key)
                    .or_default()
                    .insert(item.message.id, item.message.created_at);
            }
        }
    }
    by_recipient
}

fn summarize_pending_map(
    by_recipient: &BTreeMap<String, BTreeMap<MessageId, At>>,
    now: &At,
) -> Vec<RecipientPendingSummary> {
    let now_ms = now.unix_millis();
    let mut out = Vec::new();
    for (recipient, messages) in by_recipient {
        if messages.is_empty() {
            continue;
        }
        let oldest_ms = messages.values().map(At::unix_millis).min();
        out.push(RecipientPendingSummary {
            recipient: recipient.clone(),
            pending_count: messages.len(),
            oldest_pending_at: oldest_ms.map(At::from_unix_millis),
            oldest_pending_age_ms: oldest_ms.map(|ms| now_ms.saturating_sub(ms)),
            hold_reason: None,
        });
    }
    out
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
    tx: SyncSender<LocalEvent>,
}

const MAX_SUBSCRIBER_BUFFER: usize = 256;
const MAX_SYNC_BATCH_BYTES: usize = 4 * 1024 * 1024;
const MAX_SYNC_BATCH_ITEMS: usize = 256;

fn build_sync_batches(
    registry_events: Vec<RegistryEvent>,
    messages: Vec<WireMessage>,
    receipts: Vec<DeliveryReceipt>,
) -> Vec<PeerFrame> {
    if registry_events.is_empty() && messages.is_empty() && receipts.is_empty() {
        return vec![PeerFrame::SyncBatch {
            registry_events: Vec::new(),
            messages: Vec::new(),
            receipts: Vec::new(),
        }];
    }
    let byte_limit = MAX_SYNC_BATCH_BYTES.min(MAX_FRAME_PAYLOAD.saturating_sub(64 * 1024));
    let mut batches = Vec::new();
    let mut cur_events = Vec::new();
    let mut cur_msgs = Vec::new();
    let mut cur_rcpts = Vec::new();
    let mut cur_bytes = 64usize;
    let mut cur_items = 0usize;

    let flush = |batches: &mut Vec<PeerFrame>,
                 cur_events: &mut Vec<RegistryEvent>,
                 cur_msgs: &mut Vec<WireMessage>,
                 cur_rcpts: &mut Vec<DeliveryReceipt>,
                 cur_bytes: &mut usize,
                 cur_items: &mut usize| {
        if !cur_events.is_empty() || !cur_msgs.is_empty() || !cur_rcpts.is_empty() {
            batches.push(PeerFrame::SyncBatch {
                registry_events: std::mem::take(cur_events),
                messages: std::mem::take(cur_msgs),
                receipts: std::mem::take(cur_rcpts),
            });
            *cur_bytes = 64;
            *cur_items = 0;
        }
    };

    for ev in registry_events {
        let sz = serde_json::to_vec(&ev).map(|v| v.len() + 2).unwrap_or(256);
        if cur_items > 0 && (cur_items >= MAX_SYNC_BATCH_ITEMS || cur_bytes + sz > byte_limit) {
            flush(
                &mut batches,
                &mut cur_events,
                &mut cur_msgs,
                &mut cur_rcpts,
                &mut cur_bytes,
                &mut cur_items,
            );
        }
        cur_bytes += sz;
        cur_items += 1;
        cur_events.push(ev);
    }
    for msg in messages {
        let sz = serde_json::to_vec(&msg).map(|v| v.len() + 2).unwrap_or(512);
        if cur_items > 0 && (cur_items >= MAX_SYNC_BATCH_ITEMS || cur_bytes + sz > byte_limit) {
            flush(
                &mut batches,
                &mut cur_events,
                &mut cur_msgs,
                &mut cur_rcpts,
                &mut cur_bytes,
                &mut cur_items,
            );
        }
        cur_bytes += sz;
        cur_items += 1;
        cur_msgs.push(msg);
    }
    for rcpt in receipts {
        let sz = serde_json::to_vec(&rcpt)
            .map(|v| v.len() + 2)
            .unwrap_or(256);
        if cur_items > 0 && (cur_items >= MAX_SYNC_BATCH_ITEMS || cur_bytes + sz > byte_limit) {
            flush(
                &mut batches,
                &mut cur_events,
                &mut cur_msgs,
                &mut cur_rcpts,
                &mut cur_bytes,
                &mut cur_items,
            );
        }
        cur_bytes += sz;
        cur_items += 1;
        cur_rcpts.push(rcpt);
    }
    flush(
        &mut batches,
        &mut cur_events,
        &mut cur_msgs,
        &mut cur_rcpts,
        &mut cur_bytes,
        &mut cur_items,
    );
    batches
}

/// Limits keep a stuck vendor from causing unbounded threads or queued metadata. Messages beyond
/// a recipient's metadata bound remain persisted in the inbox for manual recovery.
const MAX_DISPATCH_RECIPIENTS: usize = 128;
/// Bound queued recipient metadata independently of active workers.
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

/// Acquire entropy once per instance; unavailable entropy never blocks startup.
fn new_dispatch_diagnostics() -> DiagnosticsState {
    use ring::rand::{SecureRandom, SystemRandom};
    let mut bytes = [0u8; 16];
    #[cfg(test)]
    let failed = tests::ENTROPY_FAILURE.with(|failure| failure.get());
    #[cfg(not(test))]
    let failed = false;
    #[cfg(test)]
    tests::ENTROPY_ATTEMPTS.with(|n| n.set(n.get() + 1));
    let nonce = if !failed && SystemRandom::new().fill(&mut bytes).is_ok() {
        Some(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
    } else {
        None
    };
    DiagnosticsState::new(now_at(), nonce)
}

fn failure_reason(stage: FailureStage) -> FenceReason {
    match stage {
        FailureStage::MessageLookupMissing => FenceReason::MessageLookupMissing,
        FailureStage::MessageLookupError => FenceReason::MessageLookupError,
        FailureStage::InitialReceiptReadError => FenceReason::InitialReceiptReadError,
        FailureStage::TransportError => FenceReason::TransportError,
        FailureStage::DeadExplicitRoute => FenceReason::DeadExplicitRoute,
        FailureStage::ComposerOccupiedByNonWireInput => FenceReason::ComposerOccupiedByNonWireInput,
        FailureStage::ReceiptWriteError => FenceReason::ReceiptWriteError,
        FailureStage::ReceiptWriteMissing => FenceReason::ReceiptWriteMissing,
        FailureStage::FinalReceiptReadError => FenceReason::FinalReceiptReadError,
        FailureStage::FinalReceiptReadMissing => FenceReason::FinalReceiptReadMissing,
        FailureStage::UnresolvedReceiptStatus => FenceReason::UnresolvedReceiptStatus,
        FailureStage::WorkerPanic => FenceReason::WorkerPanic,
        FailureStage::Unsupported => FenceReason::Unsupported,
    }
}

/// Prepare immutable safe fields outside scheduler/fence locks. The timestamp
/// is replaced at the actual marker transition, never taken from enqueue time.
fn hold_evidence(
    recipient: &str,
    id: Option<&MessageId>,
    phase: SuppressionPhase,
    reason: FenceReason,
    report: &DispatchReport,
) -> FenceDiagnostic {
    let mut row = FenceDiagnostic::from_report(recipient, id, now_at(), phase, reason, report);
    if let Some(stage) = report.failure_stages.first() {
        row.first_reason = failure_reason(*stage);
    }
    if let Some(stage) = report.failure_stages.last() {
        row.decisive_reason = failure_reason(*stage);
    }
    row
}

fn log_first_holds(rows: Vec<FenceDiagnostic>) {
    for row in rows {
        // All strings are bounded safe projections; JSON escapes control bytes.
        if let Ok(json) = serde_json::to_string(&row) {
            warn(&format!("dispatch hold: {json}"));
        }
    }
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
    dispatch_diagnostics: Mutex<DiagnosticsState>,
    pending_by_recipient: Mutex<BTreeMap<String, BTreeMap<MessageId, At>>>,
    #[cfg(test)]
    no_route_hook: Mutex<Option<(Sender<()>, Receiver<()>)>>,
    #[cfg(test)]
    empty_exit_hook: Mutex<Option<(Sender<()>, Receiver<()>)>>,
    #[cfg(test)]
    fail_next_worker_spawn: AtomicBool,
    #[cfg(test)]
    queue_limit_override: AtomicUsize,
    #[cfg(test)]
    registration_locked_hook: Mutex<Option<(Sender<()>, Receiver<()>)>>,
    peers: Mutex<BTreeMap<u64, (String, Sender<PeerFrame>)>>,
    peer_sync_cursors: Mutex<BTreeMap<String, (u64, u64, u64)>>,
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
                    .try_send(LocalEvent::Message {
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
                    .try_send(LocalEvent::Receipt {
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
        report: &mut DispatchReport,
    ) -> DispatchOutcome {
        let base = lock(&self.routes).clone();
        let mut working = base.clone();
        #[cfg(test)]
        let dispatch = tests::dispatch_with_test_transport;
        #[cfg(not(test))]
        let dispatch = dispatch_message_for_recipient_with_report;
        let (updated, attempted) = dispatch(
            self.store.as_ref(),
            &mut working,
            self.config.home_override.as_deref(),
            msg,
            now,
            recipient,
            report,
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
        let receipt = match self.store.list_wire_receipts(&msg.id) {
            Ok(receipts) => {
                let receipt = receipts
                    .into_iter()
                    .find(|receipt| receipt.recipient_name.eq_ignore_ascii_case(recipient));
                if receipt.is_none() {
                    report.record_failure(FailureStage::FinalReceiptReadMissing);
                }
                receipt
            }
            Err(_) => {
                report.record_failure(FailureStage::FinalReceiptReadError);
                None
            }
        };
        if let Some(ref rcpt) = receipt
            && !matches!(
                rcpt.status,
                DeliveryStatus::Pending | DeliveryStatus::Delivered
            )
        {
            self.clear_pending_receipt(&rcpt.recipient_name, &msg.id);
        }
        match receipt {
            Some(receipt)
                if matches!(
                    receipt.status,
                    DeliveryStatus::Delivered | DeliveryStatus::Acked | DeliveryStatus::Expired
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
            _ => {
                if receipt.is_some()
                    && report.failure_stages.last() != Some(&FailureStage::DeadExplicitRoute)
                {
                    let allow_probes =
                        crate::identity::host_probes_allowed(self.config.home_override.as_deref());
                    let explicit_entry = lock(&self.routes)
                        .get(recipient)
                        .filter(|entry| entry.explicit)
                        .cloned();
                    let dead_explicit = explicit_entry.is_some_and(|entry| {
                        crate::identity::explicit_route_entry_is_dead(
                            &entry,
                            Some(recipient),
                            allow_probes,
                        )
                    });
                    if dead_explicit {
                        report.record_failure(FailureStage::DeadExplicitRoute);
                    } else {
                        report.record_failure(FailureStage::UnresolvedReceiptStatus);
                    }
                }
                DispatchOutcome::Unconfirmed
            }
        }
    }

    fn admit_pending_receipts(&self, msg: &WireMessage, receipts: &[DeliveryReceipt]) {
        let mut pending = lock(&self.pending_by_recipient);
        for rcpt in receipts {
            if matches!(
                rcpt.status,
                DeliveryStatus::Pending | DeliveryStatus::Delivered
            ) {
                let key = normalize_persona(&rcpt.recipient_name)
                    .unwrap_or_else(|| rcpt.recipient_name.trim().to_ascii_lowercase());
                if !key.is_empty() {
                    pending
                        .entry(key)
                        .or_default()
                        .entry(msg.id.clone())
                        .or_insert_with(|| msg.created_at.clone());
                }
            }
        }
    }

    fn clear_pending_receipt(&self, recipient_name: &str, msg_id: &MessageId) {
        let key = normalize_persona(recipient_name)
            .unwrap_or_else(|| recipient_name.trim().to_ascii_lowercase());
        if key.is_empty() {
            return;
        }
        let mut pending = lock(&self.pending_by_recipient);
        if let Some(map) = pending.get_mut(&key) {
            map.remove(msg_id);
            if map.is_empty() {
                pending.remove(&key);
            }
        }
    }

    fn pending_recipients_snapshot(&self, now: &At) -> Vec<RecipientPendingSummary> {
        let merged = lock(&self.pending_by_recipient).clone();
        summarize_pending_map(&merged, now)
    }

    /// Start one queued worker, or retain it until an active slot becomes available.
    /// The queue mutex is held by the caller, so a worker cannot race its admission.
    fn start_queued_worker(
        self: &Arc<Self>,
        queues: &mut BTreeMap<String, RecipientDispatchQueue>,
        recipient: &str,
        evidence: &FenceDiagnostic,
        first_holds: &mut Vec<FenceDiagnostic>,
    ) -> Result<(), String> {
        let active = queues.values().filter(|queue| queue.worker_active).count();
        let Some(queue) = queues.get_mut(recipient) else {
            return Ok(());
        };
        if queue.worker_active || queue.pending.is_empty() {
            return Ok(());
        }
        if active >= MAX_DISPATCH_RECIPIENTS {
            queue.waiting_for_capacity = true;
            return Ok(());
        }
        queue.waiting_for_capacity = false;
        queue.waiting_for_route = false;
        queue.worker_active = true;
        if let Err(error) = self.spawn_recipient_worker(recipient.to_string()) {
            let mut row = evidence.clone();
            row.phase = SuppressionPhase::RetainedFence;
            row.first_reason = FenceReason::WorkerSpawnFailure;
            row.decisive_reason = FenceReason::WorkerSpawnFailure;
            if let Some(log) = self.retain_overflow_fence(recipient, row) {
                first_holds.push(log);
            }
            queues.remove(recipient);
            return Err(format!("could not start delivery worker: {error}"));
        }
        Ok(())
    }

    fn spawn_recipient_worker(self: &Arc<Self>, recipient: String) -> io::Result<()> {
        #[cfg(test)]
        if self.fail_next_worker_spawn.swap(false, Ordering::SeqCst) {
            return Err(io::Error::other("injected worker spawn failure"));
        }
        let owner = Arc::clone(self);
        std::thread::Builder::new()
            .name("fl-wire-dispatch".into())
            .spawn(move || owner.run_recipient_queue(recipient))?;
        Ok(())
    }

    /// A departing worker releases its slot to a queued, capacity-waiting recipient.
    fn wake_capacity_waiters(self: &Arc<Self>) {
        if self.shutdown.load(Ordering::SeqCst) {
            return;
        }
        // Snapshot inputs only, then project without scheduler locks. Admission
        // below revalidates existence, pending work, worker state and capacity.
        let candidates: Vec<_> = lock(&self.dispatch_queues)
            .iter()
            .filter(|(_, queue)| queue.waiting_for_capacity && !queue.pending.is_empty())
            .map(|(name, queue)| {
                (
                    name.clone(),
                    queue.pending.front().map(|(id, _)| id.clone()),
                )
            })
            .collect();
        let candidates: Vec<_> = candidates
            .into_iter()
            .map(|(name, id)| {
                let row = hold_evidence(
                    &name,
                    id.as_ref(),
                    SuppressionPhase::RetainedFence,
                    FenceReason::WorkerSpawnFailure,
                    &DispatchReport::default(),
                );
                (name, id, row)
            })
            .collect();
        let mut first_holds = Vec::new();
        let mut queues = lock(&self.dispatch_queues);
        for (name, id, mut row) in candidates {
            if queues.values().filter(|queue| queue.worker_active).count()
                >= MAX_DISPATCH_RECIPIENTS
            {
                break;
            }
            if !queues
                .get(&name)
                .is_some_and(|queue| queue.waiting_for_capacity && !queue.pending.is_empty())
            {
                continue;
            }
            if queues
                .get(&name)
                .and_then(|queue| queue.pending.front())
                .map(|(id, _)| id)
                != id.as_ref()
            {
                // A different oldest job may have arrived during projection.
                // Keep scheduling unchanged and leave its trigger unknown.
                row.trigger_message_id = None;
                row.trigger_message_id_digest = None;
                row.trigger_message_id_omitted = false;
            }
            if let Err(error) = self.start_queued_worker(&mut queues, &name, &row, &mut first_holds)
            {
                warn(&format!("{}: {error}", sanitize_for_warning(&name)));
            }
        }
        drop(queues);
        log_first_holds(first_holds);
    }

    /// Enqueue while the caller holds `enqueue_gate`. A worker owns each persona until its
    /// queue empties, so IDs are deduplicated even while a push is in flight.
    /// Return safe first holds through `first_holds`; the caller logs them after
    /// releasing its enqueue guard as well as the scheduler/fence guards.
    fn schedule_dispatch(
        self: &Arc<Self>,
        msg: &WireMessage,
        recipients: &[DeliveryReceipt],
        now: At,
        first_holds: &mut Vec<FenceDiagnostic>,
    ) -> Result<(), String> {
        let evidence: Vec<_> = recipients
            .iter()
            .map(|receipt| {
                hold_evidence(
                    &receipt.recipient_name.to_ascii_lowercase(),
                    Some(&msg.id),
                    SuppressionPhase::PrefixDraining,
                    FenceReason::QueueOverflow,
                    &DispatchReport::default(),
                )
            })
            .collect();
        let mut queues = lock(&self.dispatch_queues);
        let mut errors = Vec::new();
        for (receipt, evidence) in recipients.iter().zip(evidence) {
            if matches!(
                receipt.status,
                DeliveryStatus::Delivered | DeliveryStatus::Acked | DeliveryStatus::Expired
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
                #[cfg(test)]
                let queue_limit = self.queue_limit_override.load(Ordering::SeqCst);
                #[cfg(not(test))]
                let queue_limit = MAX_QUEUED_PER_RECIPIENT;
                if queue.overflowed || queue.pending.len() >= queue_limit {
                    if !queue.overflowed {
                        let _blocked = lock(&self.overflow_recipients);
                        let mut diagnostics = lock(&self.dispatch_diagnostics);
                        let mut row = evidence.clone();
                        row.hold_observed_at = now_at();
                        queue.overflowed = true;
                        if let Some(log) = diagnostics.retain(row) {
                            first_holds.push(log);
                        }
                    }
                    queue.route_generation = queue.route_generation.wrapping_add(1);
                    let wake = queue.waiting_for_route && !queue.pending.is_empty();
                    if wake
                        && let Err(error) =
                            self.start_queued_worker(&mut queues, &name, &evidence, first_holds)
                    {
                        warn(&format!("{}: {error}", sanitize_for_warning(&name)));
                    }
                    errors.push(name);
                    continue;
                }
                queue.pending.push_back((msg.id.clone(), now.clone()));
                // An arrival during an in-flight no-route lookup is also a wake request.
                // Otherwise the worker can exit after this arrival without observing it.
                queue.route_generation = queue.route_generation.wrapping_add(1);
                let wake = queue.waiting_for_route;
                if wake
                    && let Err(error) =
                        self.start_queued_worker(&mut queues, &name, &evidence, first_holds)
                {
                    warn(&format!("{}: {error}", sanitize_for_warning(&name)));
                    errors.push(name);
                }
                continue;
            }
            if lock(&self.overflow_recipients).contains(&name)
                || self.overflow_capacity_exhausted.load(Ordering::SeqCst)
            {
                errors.push(name);
                continue;
            }
            if queues.len() >= MAX_TRACKED_RECIPIENTS {
                let mut row = evidence.clone();
                row.phase = SuppressionPhase::RetainedFence;
                if let Some(log) = self.retain_overflow_fence(&name, row) {
                    first_holds.push(log);
                }
                errors.push(name);
                continue;
            }
            let mut queue = RecipientDispatchQueue::default();
            queue.pending.push_back((msg.id.clone(), now.clone()));
            queues.insert(name.clone(), queue);
            if let Err(error) = self.start_queued_worker(&mut queues, &name, &evidence, first_holds)
            {
                warn(&format!(
                    "could not start delivery worker for {}: {error}",
                    sanitize_for_warning(&name)
                ));
                errors.push(name);
            }
        }
        drop(queues);
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
            let empty_evidence = hold_evidence(
                &recipient,
                None,
                SuppressionPhase::RetainedFence,
                FenceReason::QueueOverflow,
                &DispatchReport::default(),
            );
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
                    let first_hold = if queue.overflowed {
                        self.retain_overflow_fence(&recipient, empty_evidence)
                    } else {
                        None
                    };
                    queues.remove(&recipient);
                    drop(queues);
                    log_first_holds(first_hold.into_iter().collect());
                    self.wake_capacity_waiters();
                    return;
                };
                queue.in_flight = Some(id.clone());
                (id, now, queue.route_generation)
            };
            let mut report = DispatchReport::default();
            let outcome = match self.store.get_wire_message(&job.0) {
                Ok(Some(msg)) => {
                    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        self.dispatch_and_propagate(&msg, &recipient, job.1.clone(), &mut report)
                    })) {
                        Ok(outcome) => outcome,
                        Err(_) => {
                            report.record_failure(FailureStage::WorkerPanic);
                            DispatchOutcome::Unconfirmed
                        }
                    }
                }
                Ok(None) => {
                    report.record_failure(FailureStage::MessageLookupMissing);
                    DispatchOutcome::Unconfirmed
                }
                Err(_) => {
                    report.record_failure(FailureStage::MessageLookupError);
                    DispatchOutcome::Unconfirmed
                }
            };
            let evidence = hold_evidence(
                &recipient,
                Some(&job.0),
                SuppressionPhase::RetainedFence,
                FenceReason::UnresolvedReceiptStatus,
                &report,
            );
            #[cfg(test)]
            if matches!(outcome, DispatchOutcome::NoRoute) {
                let hook = lock(&self.no_route_hook).take();
                if let Some((entered, resume)) = hook {
                    let _ = entered.send(());
                    let _ = resume.recv_timeout(Duration::from_secs(5));
                }
            }
            let mut evicted_no_route = false;
            let mut parked_no_route = false;
            let mut unresolved = false;
            let mut queues = lock(&self.dispatch_queues);
            if let Some(queue) = queues.get_mut(&recipient) {
                queue.in_flight = None;
                match outcome {
                    DispatchOutcome::Resolved => {}
                    DispatchOutcome::NoRoute => {
                        if queue.route_generation == job.2 {
                            if queue.overflowed {
                                // The accepted prefix must remain ordered and wakeable; only
                                // messages beyond the bound stay exclusively in the inbox.
                                queue.pending.push_front((job.0.clone(), job.1));
                                queue.waiting_for_route = true;
                                queue.worker_active = false;
                                parked_no_route = true;
                            } else {
                                // No route and no concurrent wake: leave messages in the inbox.
                                evicted_no_route = true;
                            }
                        } else {
                            // Registration or a new arrival raced discovery. Retry the oldest
                            // message while still owning this recipient's worker.
                            queue.pending.push_front((job.0.clone(), job.1));
                        }
                    }
                    DispatchOutcome::Unconfirmed => {
                        // Never let a later message overtake an unconfirmed push.
                        queue.pending.clear();
                        queue.overflowed = true;
                        queue.worker_active = false;
                        unresolved = true;
                    }
                }
            }
            if evicted_no_route {
                queues.remove(&recipient);
            }
            let mut first_hold = None;
            if unresolved {
                // Fence and remove while holding the same map lock: no sender may create
                // a replacement worker between those two actions.
                first_hold = self.retain_overflow_fence(&recipient, evidence);
                queues.remove(&recipient);
            }
            drop(queues);
            log_first_holds(first_hold.into_iter().collect());
            if evicted_no_route || parked_no_route {
                self.wake_capacity_waiters();
                return;
            }
            if unresolved {
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
    fn retain_overflow_fence(
        &self,
        recipient: &str,
        mut row: FenceDiagnostic,
    ) -> Option<FenceDiagnostic> {
        let mut blocked = lock(&self.overflow_recipients);
        let mut diagnostics = lock(&self.dispatch_diagnostics);
        row.hold_observed_at = now_at();
        if blocked.contains(recipient) || blocked.len() < MAX_OVERFLOW_RECIPIENTS {
            blocked.insert(recipient.to_string());
        } else {
            self.overflow_capacity_exhausted
                .store(true, Ordering::SeqCst);
            if row.failure_stages.is_empty() {
                row.first_reason = FenceReason::GlobalMetadataCap;
            }
            row.decisive_reason = FenceReason::GlobalMetadataCap;
        }
        diagnostics.snapshot.global_capacity_exhausted =
            self.overflow_capacity_exhausted.load(Ordering::SeqCst);
        diagnostics.retain(row)
    }

    fn diagnostics_snapshot(&self) -> DispatchDiagnostics {
        lock(&self.dispatch_diagnostics).snapshot.clone()
    }

    /// Wake only work admitted by this daemon instance. A pre-startup Pending receipt may
    /// already have been delivered and must not be replayed from the store.
    fn wake_recipient_after_registration(
        self: &Arc<Self>,
        queues: &mut BTreeMap<String, RecipientDispatchQueue>,
        recipient: &str,
        evidence: &FenceDiagnostic,
        first_holds: &mut Vec<FenceDiagnostic>,
    ) {
        let Some(queue) = queues.get_mut(recipient) else {
            return;
        };
        queue.route_generation = queue.route_generation.wrapping_add(1);
        if !queue.pending.is_empty()
            && !queue.worker_active
            && let Err(error) = self.start_queued_worker(queues, recipient, evidence, first_holds)
        {
            warn(&format!("{}: {error}", sanitize_for_warning(recipient)));
        }
    }

    /// Clear `overflow_recipients` and resume queued/pending push delivery for `recipient`
    /// once all unconfirmed (`Pending` with `attempts > 0`) receipts have been acknowledged.
    fn maybe_unfence_and_resume(self: &Arc<Self>, recipient: &str, now: At) {
        let name =
            normalize_persona(recipient).unwrap_or_else(|| recipient.trim().to_ascii_lowercase());
        if name.is_empty() {
            return;
        }
        let is_fenced = lock(&self.overflow_recipients).contains(&name)
            || lock(&self.dispatch_queues)
                .get(&name)
                .is_some_and(|q| q.overflowed);
        if !is_fenced {
            return;
        }
        {
            let diagnostics = lock(&self.dispatch_diagnostics);
            if diagnostics.snapshot.records.iter().any(|r| {
                r.recipient.as_deref() == Some(name.as_str())
                    && (!matches!(
                        r.first_reason,
                        FenceReason::TransportError
                            | FenceReason::DeadExplicitRoute
                            | FenceReason::ComposerOccupiedByNonWireInput
                            | FenceReason::UnresolvedReceiptStatus
                            | FenceReason::QueueOverflow
                    ) || !matches!(
                        r.decisive_reason,
                        FenceReason::TransportError
                            | FenceReason::DeadExplicitRoute
                            | FenceReason::ComposerOccupiedByNonWireInput
                            | FenceReason::UnresolvedReceiptStatus
                            | FenceReason::QueueOverflow
                    ) || r.failure_stages.iter().any(|s| {
                        !matches!(
                            s,
                            FailureStage::TransportError
                                | FailureStage::DeadExplicitRoute
                                | FailureStage::ComposerOccupiedByNonWireInput
                                | FailureStage::UnresolvedReceiptStatus
                        )
                    }) || r
                        .route_observations
                        .iter()
                        .any(|o| o.outcome == DispatchCallOutcome::ReturnedOk))
            }) {
                return;
            }
        }
        let _order = lock(&self.enqueue_gate);
        let Ok(unread) = self.store.list_wire_inbox(&WireFilter {
            recipient: Some(name.clone()),
            unread_only: true,
            ..Default::default()
        }) else {
            return;
        };
        if unread
            .iter()
            .any(|item| item.receipt.status == DeliveryStatus::Pending && item.receipt.attempts > 0)
        {
            return;
        }
        let mut first_holds = Vec::new();
        lock(&self.overflow_recipients).remove(&name);
        if let Some(queue) = lock(&self.dispatch_queues).get_mut(&name) {
            queue.overflowed = false;
        }
        for item in &unread {
            if item.receipt.status == DeliveryStatus::Pending
                && let Err(message) = self.schedule_dispatch(
                    &item.message,
                    std::slice::from_ref(&item.receipt),
                    now.clone(),
                    &mut first_holds,
                )
            {
                warn(&message);
            }
        }
        drop(_order);
        log_first_holds(first_holds);
    }

    #[cfg(test)]
    fn handle_local_request(
        self: &Arc<Self>,
        req: LocalRequest,
    ) -> (LocalResponse, Option<Receiver<LocalEvent>>) {
        self.handle_local_request_with_peer(req, PeerCred::default())
    }

    fn handle_local_request_with_peer(
        self: &Arc<Self>,
        req: LocalRequest,
        peer_cred: PeerCred,
    ) -> (LocalResponse, Option<Receiver<LocalEvent>>) {
        let now = now_at();
        match req {
            LocalRequest::Send { mut args } => {
                let mut first_holds = Vec::new();
                let _order = lock(&self.enqueue_gate);
                if args.sender_host.is_none() {
                    args.sender_host = Some(self.config.host.clone());
                }
                match self.store.send_wire(args) {
                    Ok((msg, receipts)) => {
                        self.admit_pending_receipts(&msg, &receipts);
                        self.notify_subscribers_message(&msg, &receipts);
                        self.broadcast_to_peers(
                            None,
                            &PeerFrame::Publish {
                                message: msg.clone(),
                                receipts: receipts.clone(),
                            },
                        );
                        if let Err(message) =
                            self.schedule_dispatch(&msg, &receipts, now, &mut first_holds)
                        {
                            warn(&message);
                        }
                        drop(_order);
                        log_first_holds(first_holds);
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
            LocalRequest::Read { filter, ack } => {
                if ack && let Some(ref rec) = filter.recipient {
                    let _order = lock(&self.enqueue_gate);
                    let mut items = match self.store.list_wire_inbox(&filter) {
                        Ok(items) => items,
                        Err(e) => {
                            return (
                                LocalResponse::Error {
                                    message: e.to_string(),
                                },
                                None,
                            );
                        }
                    };
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
                            self.clear_pending_receipt(&updated.recipient_name, &item.message.id);
                            item.receipt = updated.clone();
                            self.notify_subscribers_receipt(&updated);
                            self.broadcast_to_peers(
                                None,
                                &PeerFrame::ReceiptUpdate { receipt: updated },
                            );
                        }
                    }
                    drop(_order);
                    self.maybe_unfence_and_resume(rec, now);
                    (LocalResponse::Inbox { items }, None)
                } else {
                    match self.store.list_wire_inbox(&filter) {
                        Ok(items) => (LocalResponse::Inbox { items }, None),
                        Err(e) => (
                            LocalResponse::Error {
                                message: e.to_string(),
                            },
                            None,
                        ),
                    }
                }
            }
            LocalRequest::Ack {
                message_ids,
                recipient,
            } => {
                let mut out = Vec::new();
                let _order = lock(&self.enqueue_gate);
                for mid in message_ids {
                    match self.store.update_wire_receipt(
                        &mid,
                        &recipient,
                        DeliveryStatus::Acked,
                        false,
                        now.clone(),
                    ) {
                        Ok(Some(rcpt)) => {
                            self.clear_pending_receipt(&rcpt.recipient_name, &rcpt.message_id);
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
                drop(_order);
                if !out.is_empty() {
                    self.maybe_unfence_and_resume(&recipient, now);
                }
                (LocalResponse::Acked { receipts: out }, None)
            }
            LocalRequest::RegisterRoute {
                persona,
                route,
                explicit,
                replace,
                hosted_id,
                hydration_generation,
                hosted_session_verified,
                unverified_process,
            } => {
                let Some(norm) = normalize_persona(&persona) else {
                    return (
                        LocalResponse::Error {
                            message: "invalid persona".into(),
                        },
                        None,
                    );
                };
                let evidence = hold_evidence(
                    &norm,
                    None,
                    SuppressionPhase::RetainedFence,
                    FenceReason::WorkerSpawnFailure,
                    &DispatchReport::default(),
                );
                let mut first_holds = Vec::new();
                let allow_probes =
                    crate::identity::host_probes_allowed(self.config.home_override.as_deref());
                loop {
                    let current_incumbent = lock(&self.routes).get(&norm).cloned();
                    let mut current_dead = None;
                    if explicit
                        && let Some(ref inc) = current_incumbent
                        && inc.explicit
                    {
                        let conflict = crate::identity::incumbent_conflict_reason(
                            inc,
                            route.canonical_vendor(),
                            route.session_identity(),
                            route.route_pid(),
                            hosted_id.as_deref(),
                            hydration_generation.as_deref(),
                        );
                        if conflict.is_some() || inc.route.route_pid().is_some() {
                            // Run liveness / host probes completely outside all daemon locks.
                            current_dead = Some(crate::identity::explicit_route_entry_is_dead(
                                inc,
                                Some(&norm),
                                allow_probes,
                            ));
                        }
                    }
                    let effective_verified = verify_peer_cred_for_hosted_claim(
                        peer_cred,
                        hosted_session_verified,
                        replace,
                        &route,
                        hydration_generation.as_deref(),
                        current_incumbent.as_ref(),
                        current_dead.unwrap_or(false),
                    );

                    // Publish a route only while ordered with persistence and backlog enqueue.
                    // Otherwise a concurrent send could overtake older pending messages.
                    let _order = lock(&self.enqueue_gate);
                    {
                        let mut routes = lock(&self.routes);
                        if explicit && routes.get(&norm) != current_incumbent.as_ref() {
                            continue;
                        }
                        if !explicit {
                            let conflicts_with_binding = routes.get(&norm).is_some_and(|entry| {
                                entry.explicit
                                    && std::mem::discriminant(&entry.route)
                                        != std::mem::discriminant(&route)
                            });
                            let claims_bound_alias =
                                route.session_identity().is_some_and(|identity| {
                                    routes.entries.iter().any(|(name, entry)| {
                                        name != &norm
                                            && entry.explicit
                                            && entry.route.session_identity() == Some(identity)
                                    })
                                });
                            if conflicts_with_binding || claims_bound_alias {
                                return (
                                    LocalResponse::Error {
                                        message:
                                            "observed route conflicts with an explicit binding"
                                                .into(),
                                    },
                                    None,
                                );
                            }
                        }
                        // Keep the worker from evicting its queue while a route is published.
                        let mut queues = lock(&self.dispatch_queues);
                        #[cfg(test)]
                        if let Some((entered, resume)) = lock(&self.registration_locked_hook).take()
                        {
                            let _ = entered.send(());
                            let _ = resume.recv_timeout(Duration::from_secs(5));
                        }
                        let before = routes.clone();
                        if explicit {
                            if let Err(refusal) = routes.register_route_checked(
                                crate::identity::RegisterRouteCheckArgs {
                                    persona: norm.clone(),
                                    route,
                                    explicit: true,
                                    replace,
                                    hosted_id,
                                    hydration_generation,
                                    hosted_session_verified: effective_verified,
                                    unverified_process,
                                    allow_host_probes: false,
                                    incumbent_dead_override: current_dead,
                                    at: now.clone(),
                                },
                            ) {
                                let _ = routes.save(self.config.routes_path.as_deref());
                                return (
                                    LocalResponse::Error {
                                        message: refusal.message,
                                    },
                                    None,
                                );
                            }
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
                        // Route publication and the worker's wake generation must be observed
                        // together: a no-route worker may not evict in between them.
                        self.wake_recipient_after_registration(
                            &mut queues,
                            &norm,
                            &evidence,
                            &mut first_holds,
                        );
                    }
                    drop(_order);
                    break;
                }
                log_first_holds(first_holds);
                (LocalResponse::RouteRegistered, None)
            }
            LocalRequest::RecordRegistrationDiagnostic { diagnostic } => {
                let mut routes = lock(&self.routes);
                routes.push_registration_diagnostic(diagnostic);
                let _ = routes.save(self.config.routes_path.as_deref());
                (LocalResponse::RegistrationDiagnosticRecorded, None)
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
                };
                (LocalResponse::RouteUnregistered, None)
            }
            LocalRequest::Subscribe {
                recipient,
                correlation_id,
            } => {
                let (tx, rx) = mpsc::sync_channel(MAX_SUBSCRIBER_BUFFER);
                let norm_rcpt = recipient.as_deref().and_then(normalize_persona);
                lock(&self.subscribers).push(Subscriber {
                    recipient: norm_rcpt,
                    correlation_id,
                    tx,
                });
                (LocalResponse::Subscribed, Some(rx))
            }
            LocalRequest::Status {
                include_codex_queues,
            } => {
                let connected_peers = lock(&self.peers)
                    .values()
                    .map(|(label, _)| label.clone())
                    .collect();
                let routes_table = lock(&self.routes).clone();
                let routes: Vec<RouteEntry> = routes_table.entries.values().cloned().collect();
                let registration_diagnostics = routes_table.registration_diagnostics.clone();
                let mut pending_recipients = self.pending_recipients_snapshot(&now);
                let allow_probes = include_codex_queues
                    && crate::identity::host_probes_allowed(self.config.home_override.as_deref());
                annotate_dead_explicit_pending_holds(
                    &mut pending_recipients,
                    &routes_table,
                    allow_probes,
                );
                let mut diag_snapshot = self.diagnostics_snapshot();
                for row in &pending_recipients {
                    if row.hold_reason != Some(PendingHoldReason::DeadExplicitRoute) {
                        continue;
                    }
                    if let Some(existing) = diag_snapshot
                        .records
                        .iter_mut()
                        .find(|r| r.recipient.as_deref() == Some(row.recipient.as_str()))
                    {
                        if matches!(
                            existing.decisive_reason,
                            FenceReason::TransportError | FenceReason::UnresolvedReceiptStatus
                        ) {
                            existing.decisive_reason = FenceReason::DeadExplicitRoute;
                        }
                    } else if diag_snapshot.records.len() < 128 {
                        let mut report = DispatchReport::default();
                        report.record_failure(FailureStage::DeadExplicitRoute);
                        diag_snapshot.records.push(FenceDiagnostic::from_report(
                            &row.recipient,
                            None,
                            row.oldest_pending_at.clone().unwrap_or_else(|| now.clone()),
                            SuppressionPhase::RetainedFence,
                            FenceReason::DeadExplicitRoute,
                            &report,
                        ));
                    }
                }
                let dispatch_diagnostics = Some(diag_snapshot);
                // All daemon state mutexes are dropped before any store lookup or control-socket I/O.
                let codex_queues = if include_codex_queues {
                    collect_codex_route_queues(
                        &routes,
                        Some(self.store.as_ref() as &dyn AgentRegistry),
                        self.config.home_override.as_deref(),
                        now.unix_millis(),
                    )
                } else {
                    Vec::new()
                };
                (
                    LocalResponse::Status(DaemonStatus {
                        node_id: self.config.node_id.clone(),
                        host: self.config.host.clone(),
                        socket_path: self.config.socket_path.clone(),
                        listen_addr: self.config.listen_addr.clone(),
                        connected_peers,
                        routes,
                        dispatch_diagnostics,
                        registration_diagnostics,
                        pending_recipients,
                        codex_queues,
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
        let hs = handshake_peer_with_cursors(
            &mut ws,
            &self.config.node_id,
            &self.config.host,
            &self.config.psk,
            |remote_node_id| {
                lock(&self.peer_sync_cursors)
                    .get(remote_node_id)
                    .copied()
                    .unwrap_or((0, 0, 0))
            },
        )?;

        // Send initial catch-up SyncBatch frames for everything after remote's cursors,
        // chunked so no single frame can exceed MAX_FRAME_PAYLOAD.
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

        let (tx, rx) = mpsc::channel::<PeerFrame>();
        let mut batches = build_sync_batches(registry_events, messages, receipts).into_iter();
        if let Some(first_batch) = batches.next() {
            ws.send_json(&first_batch)?;
        }
        for batch in batches {
            let _ = tx.send(batch);
        }

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

    fn advance_peer_sync_cursors(
        &self,
        from_peer_id: u64,
        msg_delta: u64,
        rcpt_delta: u64,
        agent_delta: u64,
    ) {
        let label = self.peer_label(from_peer_id);
        let node_id = label.split('@').next().unwrap_or(&label).to_string();
        if node_id.is_empty() {
            return;
        }
        let mut cursors = lock(&self.peer_sync_cursors);
        let entry = cursors.entry(node_id).or_insert((0, 0, 0));
        entry.0 = entry.0.saturating_add(msg_delta);
        entry.1 = entry.1.saturating_add(rcpt_delta);
        entry.2 = entry.2.saturating_add(agent_delta);
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
                self.advance_peer_sync_cursors(
                    from_peer_id,
                    messages.len() as u64,
                    receipts.len() as u64,
                    registry_events.len() as u64,
                );
                let mut first_holds = Vec::new();
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
                        self.admit_pending_receipts(&m, &rcpts);
                        self.notify_subscribers_message(&m, &rcpts);
                        self.broadcast_to_peers(
                            Some(from_peer_id),
                            &PeerFrame::Publish {
                                message: m.clone(),
                                receipts: rcpts.clone(),
                            },
                        );
                        if let Err(error) =
                            self.schedule_dispatch(&m, &rcpts, now.clone(), &mut first_holds)
                        {
                            warn(&format!("peer message scheduling failed: {error}"));
                        }
                    }
                    for r in receipts {
                        if !report.rejects(&r.message_id) {
                            if !matches!(
                                r.status,
                                DeliveryStatus::Pending | DeliveryStatus::Delivered
                            ) {
                                self.clear_pending_receipt(&r.recipient_name, &r.message_id);
                            }
                            self.notify_subscribers_receipt(&r);
                        }
                    }
                }
                drop(_order);
                log_first_holds(first_holds);
            }
            PeerFrame::Publish { message, receipts } => {
                self.advance_peer_sync_cursors(from_peer_id, 1, receipts.len() as u64, 0);
                let mut first_holds = Vec::new();
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
                    self.admit_pending_receipts(&message, &rcpts);
                    self.notify_subscribers_message(&message, &rcpts);
                    self.broadcast_to_peers(
                        Some(from_peer_id),
                        &PeerFrame::Publish {
                            message: message.clone(),
                            receipts: rcpts.clone(),
                        },
                    );
                    if let Err(error) =
                        self.schedule_dispatch(&message, &rcpts, now, &mut first_holds)
                    {
                        warn(&format!("peer message scheduling failed: {error}"));
                    }
                } else if let Ok(ref report) = imported {
                    for r in &receipts {
                        if !report.rejects(&r.message_id)
                            && !matches!(
                                r.status,
                                DeliveryStatus::Pending | DeliveryStatus::Delivered
                            )
                        {
                            self.clear_pending_receipt(&r.recipient_name, &r.message_id);
                        }
                    }
                }
                drop(_order);
                log_first_holds(first_holds);
            }
            PeerFrame::ReceiptUpdate { receipt } => {
                self.advance_peer_sync_cursors(from_peer_id, 0, 1, 0);
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
                    if !matches!(
                        receipt.status,
                        DeliveryStatus::Pending | DeliveryStatus::Delivered
                    ) {
                        self.clear_pending_receipt(&receipt.recipient_name, &receipt.message_id);
                    }
                    self.notify_subscribers_receipt(&receipt);
                    self.broadcast_to_peers(
                        Some(from_peer_id),
                        &PeerFrame::ReceiptUpdate { receipt },
                    );
                }
            }
            PeerFrame::RegistryUpdate { event } => {
                self.advance_peer_sync_cursors(from_peer_id, 0, 0, 1);
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
        fs::set_permissions(&config.socket_path, fs::Permissions::from_mode(0o600))?;
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
    let initial_pending = seed_pending_map_from_store(store.as_ref());
    let inner = Arc::new(DaemonInner {
        store,
        config: config.clone(),
        routes: Mutex::new(initial_routes),
        enqueue_gate: Mutex::new(()),
        dispatch_queues: Mutex::new(BTreeMap::new()),
        overflow_recipients: Mutex::new(BTreeSet::new()),
        overflow_capacity_exhausted: AtomicBool::new(false),
        dispatch_diagnostics: Mutex::new(new_dispatch_diagnostics()),
        pending_by_recipient: Mutex::new(initial_pending),
        #[cfg(test)]
        no_route_hook: Mutex::new(None),
        #[cfg(test)]
        empty_exit_hook: Mutex::new(None),
        #[cfg(test)]
        fail_next_worker_spawn: AtomicBool::new(false),
        #[cfg(test)]
        queue_limit_override: AtomicUsize::new(MAX_QUEUED_PER_RECIPIENT),
        #[cfg(test)]
        registration_locked_hook: Mutex::new(None),
        peers: Mutex::new(BTreeMap::new()),
        peer_sync_cursors: Mutex::new(BTreeMap::new()),
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
                    let peer_cred = extract_peer_cred(&stream);
                    let Ok(mut writer) = stream.try_clone() else {
                        return;
                    };
                    let mut reader = BufReader::new(stream).take(MAX_LOCAL_REQUEST_BYTES);
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    };
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
                    let (resp, sub_rx) = inner_conn.handle_local_request_with_peer(req, peer_cred);
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
    use crate::peer::handshake_peer;
    use fl_core::ferrowire::{MessageKind, MessageUrgency, WireAddress};
    use fl_store::RedbStore;
    use tempfile::TempDir;

    thread_local! {
        pub(super) static TRANSPORT_PANIC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        pub(super) static TRANSPORT_RESULT: std::cell::RefCell<Option<Result<(), String>>> = const { std::cell::RefCell::new(None) };
        pub(super) static TRANSPORT_CALLS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
        pub(super) static ENTROPY_FAILURE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        pub(super) static ENTROPY_ATTEMPTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
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

    fn rig_inner<S>(tmp: &TempDir, store: Arc<S>) -> Arc<DaemonInner<S>> {
        Arc::new(DaemonInner {
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
            dispatch_diagnostics: Mutex::new(new_dispatch_diagnostics()),
            pending_by_recipient: Mutex::new(BTreeMap::new()),
            no_route_hook: Mutex::new(None),
            empty_exit_hook: Mutex::new(None),
            fail_next_worker_spawn: AtomicBool::new(false),
            queue_limit_override: AtomicUsize::new(MAX_QUEUED_PER_RECIPIENT),
            registration_locked_hook: Mutex::new(None),
            peers: Mutex::new(BTreeMap::new()),
            peer_sync_cursors: Mutex::new(BTreeMap::new()),
            subscribers: Mutex::new(Vec::new()),
            next_peer_id: Mutex::new(0),
            inbound_peers: AtomicUsize::new(0),
            shutdown: AtomicBool::new(false),
        })
    }

    fn rig() -> Rig {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(RedbStore::open(&tmp.path().join("dest.redb")).unwrap());
        let source = RedbStore::open(&tmp.path().join("src.redb")).unwrap();
        let inner = rig_inner(&tmp, store);
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

    use fl_core::{
        agent::{AgentRecord, AreaRecord, RegisterAgentArgs},
        ferrowire::ThreadId,
        ids::AgentId,
        store::StoreError,
    };

    #[derive(Clone, Copy, Default)]
    enum Fault {
        #[default]
        Forward,
        Error,
        Missing,
        Panic,
    }

    struct FaultStore {
        inner: RedbStore,
        gets: Mutex<VecDeque<Fault>>,
        reads: Mutex<VecDeque<Fault>>,
        writes: Mutex<VecDeque<Fault>>,
        calls: Mutex<Vec<&'static str>>,
    }

    const SECRET_FAULT: &str = "ERROR_SECRET /private/token panic=PRIVATE_PAYLOAD";
    impl FaultStore {
        fn fault(&self, queue: &Mutex<VecDeque<Fault>>) -> Fault {
            lock(queue).pop_front().unwrap_or_default()
        }
    }
    impl MessageStore for FaultStore {
        fn send_wire(
            &self,
            args: SendWireArgs,
        ) -> Result<(WireMessage, Vec<DeliveryReceipt>), StoreError> {
            lock(&self.calls).push("send");
            self.inner.send_wire(args)
        }
        fn get_wire_message(&self, id: &MessageId) -> Result<Option<WireMessage>, StoreError> {
            lock(&self.calls).push("get");
            match self.fault(&self.gets) {
                Fault::Error => Err(StoreError::Backend(SECRET_FAULT.into())),
                Fault::Missing => Ok(None),
                Fault::Panic => panic!("{SECRET_FAULT}"),
                Fault::Forward => self.inner.get_wire_message(id),
            }
        }
        fn list_wire_receipts(&self, id: &MessageId) -> Result<Vec<DeliveryReceipt>, StoreError> {
            lock(&self.calls).push("receipts");
            match self.fault(&self.reads) {
                Fault::Error => Err(StoreError::Backend(SECRET_FAULT.into())),
                Fault::Missing => Ok(Vec::new()),
                Fault::Panic => panic!("{SECRET_FAULT}"),
                Fault::Forward => self.inner.list_wire_receipts(id),
            }
        }
        fn update_wire_receipt(
            &self,
            id: &MessageId,
            recipient: &str,
            status: DeliveryStatus,
            increment: bool,
            at: At,
        ) -> Result<Option<DeliveryReceipt>, StoreError> {
            lock(&self.calls).push("update");
            match self.fault(&self.writes) {
                Fault::Error => Err(StoreError::Backend(SECRET_FAULT.into())),
                Fault::Missing => Ok(None),
                Fault::Panic => panic!("{SECRET_FAULT}"),
                Fault::Forward => self
                    .inner
                    .update_wire_receipt(id, recipient, status, increment, at),
            }
        }
        fn list_wire_inbox(&self, filter: &WireFilter) -> Result<Vec<WireInboxItem>, StoreError> {
            lock(&self.calls).push("inbox");
            self.inner.list_wire_inbox(filter)
        }
        fn list_wire_thread(&self, id: &ThreadId) -> Result<Vec<WireMessage>, StoreError> {
            lock(&self.calls).push("thread");
            self.inner.list_wire_thread(id)
        }
        fn list_wire_replies(&self, id: &MessageId) -> Result<Vec<WireMessage>, StoreError> {
            lock(&self.calls).push("replies");
            self.inner.list_wire_replies(id)
        }
        fn wire_messages_after(&self, seq: u64) -> Result<Vec<(u64, WireMessage)>, StoreError> {
            lock(&self.calls).push("messages_after");
            self.inner.wire_messages_after(seq)
        }
        fn wire_receipts_after(&self, seq: u64) -> Result<Vec<(u64, DeliveryReceipt)>, StoreError> {
            lock(&self.calls).push("receipts_after");
            self.inner.wire_receipts_after(seq)
        }
        fn import_wire_batch(
            &self,
            messages: &[WireMessage],
            receipts: &[DeliveryReceipt],
        ) -> Result<WireImportReport, StoreError> {
            lock(&self.calls).push("import");
            self.inner.import_wire_batch(messages, receipts)
        }
        fn prune_wire(&self, before: At) -> Result<usize, StoreError> {
            lock(&self.calls).push("prune");
            self.inner.prune_wire(before)
        }
    }
    impl AgentRegistry for FaultStore {
        fn upsert_area(&self, area: AreaRecord) -> Result<AreaRecord, StoreError> {
            self.inner.upsert_area(area)
        }
        fn get_area(&self, key: &str) -> Result<Option<AreaRecord>, StoreError> {
            self.inner.get_area(key)
        }
        fn list_areas(&self) -> Result<Vec<AreaRecord>, StoreError> {
            self.inner.list_areas()
        }
        fn delete_area(&self, key: &str, at: At, by: &str) -> Result<bool, StoreError> {
            self.inner.delete_area(key, at, by)
        }
        fn register_agent(&self, args: RegisterAgentArgs) -> Result<AgentRecord, StoreError> {
            self.inner.register_agent(args)
        }
        fn retire_agent(&self, id: &AgentId, at: At, by: &str) -> Result<AgentRecord, StoreError> {
            self.inner.retire_agent(id, at, by)
        }
        fn get_agent(&self, id: &AgentId) -> Result<Option<AgentRecord>, StoreError> {
            self.inner.get_agent(id)
        }
        fn agent_by_name(&self, key: &str) -> Result<Option<AgentRecord>, StoreError> {
            self.inner.agent_by_name(key)
        }
        fn list_agents(
            &self,
            project: Option<&str>,
            retired: bool,
        ) -> Result<Vec<AgentRecord>, StoreError> {
            self.inner.list_agents(project, retired)
        }
        fn registry_events_after(&self, seq: u64) -> Result<Vec<(u64, RegistryEvent)>, StoreError> {
            self.inner.registry_events_after(seq)
        }
        fn import_registry_events(&self, events: &[RegistryEvent]) -> Result<(), StoreError> {
            self.inner.import_registry_events(events)
        }
    }

    fn fault_rig() -> (TempDir, Arc<DaemonInner<FaultStore>>) {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(FaultStore {
            inner: RedbStore::open(&tmp.path().join("fault.redb")).unwrap(),
            gets: Mutex::new(VecDeque::new()),
            reads: Mutex::new(VecDeque::new()),
            writes: Mutex::new(VecDeque::new()),
            calls: Mutex::new(Vec::new()),
        });
        let inner = rig_inner(&tmp, store);
        lock(&inner.routes).upsert_explicit(
            "held",
            VendorRoute::Codex {
                thread_id: "SESSION_SECRET".into(),
                cwd: Some("/CWD_SECRET".into()),
            },
            now_at(),
        );
        (tmp, inner)
    }

    fn run_fault_job(inner: &Arc<DaemonInner<FaultStore>>, recipient: &str) -> MessageId {
        let (msg, _) = inner
            .store
            .send_wire(mail(recipient, "BODY_SECRET"))
            .unwrap();
        lock(&inner.dispatch_queues).insert(
            recipient.into(),
            RecipientDispatchQueue {
                pending: VecDeque::from([(msg.id.clone(), At::from_unix_millis(1))]),
                worker_active: true,
                ..Default::default()
            },
        );
        Arc::clone(inner).run_recipient_queue(recipient.into());
        msg.id
    }

    pub(super) fn dispatch_with_test_transport<S: MessageStore>(
        store: &S,
        routes: &mut RouteTable,
        home: Option<&Path>,
        msg: &WireMessage,
        now: At,
        recipient: &str,
        report: &mut DispatchReport,
    ) -> (Vec<DeliveryReceipt>, Option<bool>) {
        if TRANSPORT_RESULT.with(|cell| cell.borrow().is_some()) {
            crate::dispatcher::dispatch_message_for_recipient_with_test_dispatch(
                store,
                routes,
                home,
                msg,
                now,
                recipient,
                report,
                |_, _| {
                    TRANSPORT_CALLS.with(|n| n.set(n.get() + 1));
                    TRANSPORT_PANIC.with(|flag| {
                        if flag.get() {
                            panic!("{SECRET_FAULT}");
                        }
                    });
                    TRANSPORT_RESULT.with(|cell| cell.borrow().as_ref().unwrap().clone())
                },
            )
        } else {
            dispatch_message_for_recipient_with_report(
                store, routes, home, msg, now, recipient, report,
            )
        }
    }

    fn with_transport(outcome: Result<(), String>, f: impl FnOnce()) {
        TRANSPORT_RESULT.with(|cell| *cell.borrow_mut() = Some(outcome));
        TRANSPORT_CALLS.with(|n| n.set(0));
        f();
        TRANSPORT_RESULT.with(|cell| *cell.borrow_mut() = None);
    }

    fn check_fault(
        gets: &[Fault],
        reads: &[Fault],
        writes: &[Fault],
        stages: &[FailureStage],
        calls: u64,
        store_calls: &[&str],
    ) {
        let (_tmp, inner) = fault_rig();
        lock(&inner.store.gets).extend(gets);
        lock(&inner.store.reads).extend(reads);
        lock(&inner.store.writes).extend(writes);
        with_transport(Ok(()), || {
            run_fault_job(&inner, "held");
            let snapshot = inner.diagnostics_snapshot();
            assert_eq!(snapshot.records.len(), 1);
            let row = &snapshot.records[0];
            assert_eq!(row.failure_stages, stages);
            assert_eq!(row.route_dispatch_calls, calls);
            assert_eq!(*lock(&inner.store.calls), store_calls);
            assert_eq!(row.first_reason, failure_reason(stages[0]));
            assert_eq!(row.decisive_reason, failure_reason(*stages.last().unwrap()));
            assert_eq!(row.phase, SuppressionPhase::RetainedFence);
            assert!(lock(&inner.overflow_recipients).contains("held"));
            assert!(!serde_json::to_string(row).unwrap().contains("SECRET"));
            assert!(!take_warnings().iter().any(|line| line.contains("SECRET")));
        });
    }

    #[test]
    fn diagnostics_message_lookup_missing() {
        check_fault(
            &[Fault::Missing],
            &[],
            &[],
            &[FailureStage::MessageLookupMissing],
            0,
            &["send", "get"],
        );
    }
    #[test]
    fn diagnostics_message_lookup_error() {
        check_fault(
            &[Fault::Error],
            &[],
            &[],
            &[FailureStage::MessageLookupError],
            0,
            &["send", "get"],
        );
    }
    #[test]
    fn diagnostics_initial_receipt_read_error() {
        check_fault(
            &[],
            &[Fault::Error],
            &[],
            &[
                FailureStage::InitialReceiptReadError,
                FailureStage::UnresolvedReceiptStatus,
            ],
            0,
            &["send", "get", "receipts", "receipts"],
        );
    }
    #[test]
    fn diagnostics_receipt_write_error_preserves_successful_call() {
        check_fault(
            &[],
            &[],
            &[Fault::Error],
            &[
                FailureStage::ReceiptWriteError,
                FailureStage::UnresolvedReceiptStatus,
            ],
            1,
            &["send", "get", "receipts", "update", "receipts"],
        );
    }
    #[test]
    fn diagnostics_receipt_write_missing_is_distinct() {
        check_fault(
            &[],
            &[],
            &[Fault::Missing],
            &[
                FailureStage::ReceiptWriteMissing,
                FailureStage::UnresolvedReceiptStatus,
            ],
            1,
            &["send", "get", "receipts", "update", "receipts"],
        );
    }
    #[test]
    fn diagnostics_final_receipt_read_error() {
        check_fault(
            &[],
            &[Fault::Forward, Fault::Error],
            &[],
            &[FailureStage::FinalReceiptReadError],
            1,
            &["send", "get", "receipts", "update", "receipts"],
        );
    }
    #[test]
    fn diagnostics_final_receipt_read_missing() {
        check_fault(
            &[],
            &[Fault::Forward, Fault::Missing],
            &[],
            &[FailureStage::FinalReceiptReadMissing],
            1,
            &["send", "get", "receipts", "update", "receipts"],
        );
    }
    #[test]
    fn diagnostics_worker_panic_preserves_report() {
        check_fault(
            &[],
            &[],
            &[Fault::Panic],
            &[FailureStage::WorkerPanic],
            1,
            &["send", "get", "receipts", "update"],
        );
    }
    #[test]
    fn diagnostics_transport_panic_keeps_unknown_started_call() {
        let (_tmp, inner) = fault_rig();
        with_transport(Ok(()), || {
            TRANSPORT_PANIC.with(|flag| flag.set(true));
            run_fault_job(&inner, "held");
            TRANSPORT_PANIC.with(|flag| flag.set(false));
            let row = inner.diagnostics_snapshot().records.remove(0);
            assert_eq!(row.failure_stages, vec![FailureStage::WorkerPanic]);
            assert_eq!(row.route_dispatch_calls, 1);
            assert_eq!(
                row.route_observations[0].outcome,
                crate::dispatch_diagnostics::DispatchCallOutcome::Unknown
            );
            assert_eq!(
                inner
                    .store
                    .inner
                    .list_wire_inbox(&WireFilter {
                        recipient: Some("held".into()),
                        ..WireFilter::default()
                    })
                    .unwrap()[0]
                    .receipt
                    .attempts,
                0
            );
        });
    }

    #[test]
    fn diagnostics_panic_appends_to_prior_failure() {
        let (_tmp, inner) = fault_rig();
        lock(&inner.store.writes).push_back(Fault::Panic);
        with_transport(Err(SECRET_FAULT.into()), || {
            run_fault_job(&inner, "held");
            let row = inner.diagnostics_snapshot().records.remove(0);
            assert_eq!(
                row.failure_stages,
                vec![FailureStage::TransportError, FailureStage::WorkerPanic]
            );
            assert_eq!(row.first_reason, FenceReason::TransportError);
            assert_eq!(row.decisive_reason, FenceReason::WorkerPanic);
        });
    }

    #[test]
    fn diagnostics_nested_schedule_defers_first_hold_log() {
        let rig = rig();
        rig.inner
            .fail_next_worker_spawn
            .store(true, Ordering::SeqCst);
        let (message, receipts) = rig
            .inner
            .store
            .send_wire(mail("held", "BODY_SECRET"))
            .unwrap();
        let gate = lock(&rig.inner.enqueue_gate);
        let mut first_holds = Vec::new();
        assert!(
            rig.inner
                .schedule_dispatch(&message, &receipts, now_at(), &mut first_holds)
                .is_err()
        );
        assert_eq!(first_holds.len(), 1);
        assert!(
            take_warnings()
                .iter()
                .all(|line| !line.starts_with("dispatch hold:"))
        );
        drop(gate);
        assert!(rig.inner.enqueue_gate.try_lock().is_ok());
        assert!(rig.inner.dispatch_queues.try_lock().is_ok());
        assert!(rig.inner.overflow_recipients.try_lock().is_ok());
        assert!(rig.inner.dispatch_diagnostics.try_lock().is_ok());
        log_first_holds(first_holds);
        let warnings = take_warnings();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].starts_with("dispatch hold:"));
        assert!(!warnings[0].contains("SECRET"));
    }

    #[test]
    fn diagnostics_transport_error_is_mutation_unknown() {
        let (_tmp, inner) = fault_rig();
        with_transport(Err(SECRET_FAULT.into()), || {
            run_fault_job(&inner, "held");
            let row = &inner.diagnostics_snapshot().records[0];
            assert_eq!(
                row.failure_stages,
                vec![
                    FailureStage::TransportError,
                    FailureStage::UnresolvedReceiptStatus
                ]
            );
            assert_eq!(row.route_dispatch_calls, 1);
            assert_eq!(
                row.route_observations[0].outcome,
                crate::dispatch_diagnostics::DispatchCallOutcome::ReturnedError
            );
        });
    }

    #[test]
    fn diagnostics_composer_occupied_fences_and_unfences_on_ack() {
        let (_tmp, inner) = fault_rig();
        let occupied_err =
            "composer_occupied_by_non_wire_input: composer holds non-wire input or active dialog; holding delivery without sending keys"
                .to_string();
        with_transport(Err(occupied_err), || {
            let id = run_fault_job(&inner, "held");
            let row = &inner.diagnostics_snapshot().records[0];
            assert_eq!(
                row.first_reason,
                FenceReason::ComposerOccupiedByNonWireInput
            );
            assert_eq!(row.decisive_reason, FenceReason::UnresolvedReceiptStatus);
            assert_eq!(
                row.failure_stages,
                vec![
                    FailureStage::ComposerOccupiedByNonWireInput,
                    FailureStage::UnresolvedReceiptStatus,
                ]
            );
            assert!(lock(&inner.overflow_recipients).contains("held"));
            let (response, _) = inner.handle_local_request(LocalRequest::Ack {
                recipient: "held".into(),
                message_ids: vec![id],
            });
            assert!(matches!(response, LocalResponse::Acked { .. }));
            assert!(
                !lock(&inner.overflow_recipients).contains("held"),
                "fl wire read --ack must unfence ComposerOccupiedByNonWireInput once resolved"
            );
        });
    }

    #[test]
    fn diagnostics_ack_and_register_preserve_first_hold() {
        let (_tmp, inner) = fault_rig();
        lock(&inner.store.writes).push_back(Fault::Error);
        with_transport(Ok(()), || {
            let id = run_fault_job(&inner, "held");
            let first = inner.diagnostics_snapshot();
            assert_eq!(
                first.records[0].route_observations[0].outcome,
                crate::dispatch_diagnostics::DispatchCallOutcome::ReturnedOk
            );
            let (response, _) = inner.handle_local_request(LocalRequest::Ack {
                recipient: "held".into(),
                message_ids: vec![id.clone()],
            });
            assert!(matches!(response, LocalResponse::Acked { .. }));
            let (response, _) = inner.handle_local_request(LocalRequest::register(
                "held",
                VendorRoute::Codex {
                    thread_id: "new-session".into(),
                    cwd: None,
                },
                true,
            ));
            assert!(matches!(response, LocalResponse::RouteRegistered));
            assert_eq!(inner.diagnostics_snapshot(), first);
            let calls = TRANSPORT_CALLS.with(|n| n.get());
            let (response, _) = inner.handle_local_request(LocalRequest::Send {
                args: mail("held", "later"),
            });
            let LocalResponse::Sent { message, .. } = response else {
                panic!("send did not persist")
            };
            assert_eq!(TRANSPORT_CALLS.with(|n| n.get()), calls);
            assert_eq!(
                inner.store.inner.list_wire_receipts(&message.id).unwrap()[0].attempts,
                0
            );
            lock(&inner.routes).upsert_explicit(
                "other",
                VendorRoute::Codex {
                    thread_id: "other-session".into(),
                    cwd: None,
                },
                now_at(),
            );
            let other = run_fault_job(&inner, "other");
            assert_eq!(TRANSPORT_CALLS.with(|n| n.get()), calls + 1);
            assert_eq!(
                inner.store.inner.list_wire_receipts(&other).unwrap()[0].status,
                DeliveryStatus::Acked
            );
            assert_eq!(inner.diagnostics_snapshot(), first);
        });
    }

    #[test]
    fn diagnostics_fence_and_snapshot_are_atomic() {
        let rig = rig();
        rig.inner
            .fail_next_worker_spawn
            .store(true, Ordering::SeqCst);
        let (msg, receipts) = rig.inner.store.send_wire(mail("atomic", "stored")).unwrap();
        let guard = lock(&rig.inner.dispatch_diagnostics);
        let owner = Arc::clone(&rig.inner);
        let (done_tx, done_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            assert!(
                owner
                    .schedule_dispatch(&msg, &receipts, now_at(), &mut Vec::new())
                    .is_err()
            );
            done_tx.send(()).unwrap();
        });
        let deadline = Instant::now() + Duration::from_secs(3);
        while rig.inner.dispatch_queues.try_lock().is_ok() {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert!(guard.snapshot.records.is_empty());
        assert!(done_rx.recv_timeout(Duration::from_millis(20)).is_err());
        drop(guard);
        done_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        worker.join().unwrap();
        assert!(lock(&rig.inner.overflow_recipients).contains("atomic"));
        assert_eq!(
            rig.inner.diagnostics_snapshot().records[0]
                .recipient
                .as_deref(),
            Some("atomic")
        );
    }

    #[test]
    fn diagnostics_ordinary_no_route_exit_has_no_hold() {
        let rig = rig();
        let (msg, _) = rig
            .inner
            .store
            .send_wire(mail("offline", "pending"))
            .unwrap();
        lock(&rig.inner.dispatch_queues).insert(
            "offline".into(),
            RecipientDispatchQueue {
                pending: VecDeque::from([(msg.id, At::from_unix_millis(1))]),
                worker_active: true,
                ..Default::default()
            },
        );
        Arc::clone(&rig.inner).run_recipient_queue("offline".into());
        assert!(rig.inner.diagnostics_snapshot().records.is_empty());
        assert!(lock(&rig.inner.overflow_recipients).is_empty());
    }

    #[test]
    fn diagnostics_imported_names_keep_scheduler_prefixes_independent() {
        let (_tmp, inner) = fault_rig();
        inner.queue_limit_override.store(0, Ordering::SeqCst);
        let (message, receipts) = inner.store.send_wire(mail("alice", "rejected")).unwrap();
        lock(&inner.store.calls).clear();
        for name in ["alice", " alice ", "\talice\t"] {
            lock(&inner.dispatch_queues).insert(
                name.into(),
                RecipientDispatchQueue {
                    worker_active: true,
                    ..Default::default()
                },
            );
        }
        // Imported receipts preserve their name bytes; bypass send_wire's slug normalization.
        for name in ["Alice", " ALICE ", "\tAlice\t"] {
            let mut receipt = receipts[0].clone();
            receipt.recipient_name = name.into();
            assert!(
                inner
                    .schedule_dispatch(&message, &[receipt], now_at(), &mut Vec::new())
                    .is_err()
            );
        }
        let first = inner.diagnostics_snapshot();
        assert_eq!(first.records.len(), 3);
        assert_eq!(lock(&inner.dispatch_queues).len(), 3);
        assert!(
            lock(&inner.dispatch_queues)
                .values()
                .all(|queue| queue.overflowed)
        );
        assert!(lock(&inner.overflow_recipients).is_empty());
        assert_eq!(first.records[0].recipient.as_deref(), Some("alice"));
        assert!(
            first.records[1..]
                .iter()
                .all(|row| row.recipient_invalid && row.recipient.is_none())
        );
        let evidence = hold_evidence(
            " alice ",
            None,
            SuppressionPhase::RetainedFence,
            FenceReason::WorkerSpawnFailure,
            &DispatchReport::default(),
        );
        {
            let _queues = lock(&inner.dispatch_queues);
            assert!(inner.retain_overflow_fence(" alice ", evidence).is_none());
        }
        let after = inner.diagnostics_snapshot();
        assert_eq!(after.records[0], first.records[0]);
        assert_eq!(after.records[2], first.records[2]);
        let mut promoted = first.records[1].clone();
        promoted.phase = SuppressionPhase::RetainedFence;
        promoted.decisive_reason = FenceReason::WorkerSpawnFailure;
        assert_eq!(after.records[1], promoted);
        assert_eq!(
            promoted.trigger_message_id.as_deref(),
            Some(message.id.as_str())
        );
        assert!(lock(&inner.overflow_recipients).contains(" alice "));
        assert!(!lock(&inner.overflow_recipients).contains("alice"));
        assert!(lock(&inner.store.calls).is_empty());
    }

    #[test]
    fn diagnostics_padded_registration_promotes_only_canonical_prefix_on_spawn_failure() {
        let (_tmp, inner) = fault_rig();
        inner.queue_limit_override.store(1, Ordering::SeqCst);
        let (accepted, _) = inner.store.send_wire(mail("alice", "accepted")).unwrap();
        for name in ["alice", " alice "] {
            lock(&inner.dispatch_queues).insert(
                name.into(),
                RecipientDispatchQueue {
                    pending: VecDeque::from([(accepted.id.clone(), At::from_unix_millis(1))]),
                    worker_active: true,
                    ..Default::default()
                },
            );
            let (rejected, mut receipts) =
                inner.store.send_wire(mail("alice", "rejected")).unwrap();
            // Imported receipt names keep raw bytes, independently of route registration.
            receipts[0].recipient_name = name.into();
            assert!(
                inner
                    .schedule_dispatch(&rejected, &receipts, now_at(), &mut Vec::new())
                    .is_err()
            );
        }
        let before = inner.diagnostics_snapshot();
        assert_eq!(before.records.len(), 2);
        assert_eq!(before.records[0].recipient.as_deref(), Some("alice"));
        assert!(before.records[1].recipient_invalid);
        for row in &before.records {
            assert_eq!(row.phase, SuppressionPhase::PrefixDraining);
            assert_eq!(row.first_reason, FenceReason::QueueOverflow);
            assert!(row.trigger_message_id.is_some());
        }
        assert_ne!(
            before.records[0].trigger_message_id,
            before.records[1].trigger_message_id
        );
        {
            let mut queues = lock(&inner.dispatch_queues);
            let canonical = queues.get_mut("alice").unwrap();
            canonical.worker_active = false;
            canonical.waiting_for_route = true;
        }
        inner.fail_next_worker_spawn.store(true, Ordering::SeqCst);
        lock(&inner.store.calls).clear();
        let (response, _) = inner.handle_local_request(LocalRequest::register(
            " ALICE ",
            VendorRoute::Codex {
                thread_id: "registration-session".into(),
                cwd: None,
            },
            true,
        ));
        assert!(matches!(response, LocalResponse::RouteRegistered));
        assert!(lock(&inner.routes).get("alice").unwrap().explicit);
        assert_eq!(
            *lock(&inner.overflow_recipients),
            BTreeSet::from(["alice".into()])
        );
        {
            let queues = lock(&inner.dispatch_queues);
            assert!(!queues.contains_key("alice"));
            let raw = queues.get(" alice ").unwrap();
            assert_eq!(raw.pending.front().map(|(id, _)| id), Some(&accepted.id));
            assert_eq!(raw.pending.len(), 1);
            assert!(raw.worker_active && raw.overflowed);
            assert_eq!(raw.route_generation, 1);
        }
        let after = inner.diagnostics_snapshot();
        assert_eq!(after.records.len(), 2);
        let mut promoted = before.records[0].clone();
        promoted.phase = SuppressionPhase::RetainedFence;
        promoted.decisive_reason = FenceReason::WorkerSpawnFailure;
        // Full equality also preserves the first overflow trigger, timestamp and cause.
        assert_eq!(after.records[0], promoted);
        assert_eq!(after.records[1], before.records[1]);
        assert!(lock(&inner.store.calls).is_empty());
    }

    #[test]
    fn diagnostics_prefix_preserves_earliest_rejected_trigger_during_promotion() {
        let (_tmp, inner) = fault_rig();
        inner.queue_limit_override.store(1, Ordering::SeqCst);
        let (accepted, _) = inner.store.send_wire(mail("held", "accepted")).unwrap();
        lock(&inner.dispatch_queues).insert(
            "held".into(),
            RecipientDispatchQueue {
                pending: VecDeque::from([(accepted.id, At::from_unix_millis(1))]),
                worker_active: true,
                ..Default::default()
            },
        );
        let (rejected, receipts) = inner
            .store
            .send_wire(mail("held", "first rejected"))
            .unwrap();
        assert!(
            inner
                .schedule_dispatch(
                    &rejected,
                    &receipts,
                    At::from_unix_millis(1),
                    &mut Vec::new()
                )
                .is_err()
        );
        let first = inner.diagnostics_snapshot().records[0].clone();
        assert_eq!(first.phase, SuppressionPhase::PrefixDraining);
        assert_eq!(
            first.trigger_message_id.as_deref(),
            Some(rejected.id.as_str())
        );
        assert!(!lock(&inner.overflow_recipients).contains("held"));
        let (later, receipts) = inner
            .store
            .send_wire(mail("held", "later rejected"))
            .unwrap();
        assert!(
            inner
                .schedule_dispatch(&later, &receipts, now_at(), &mut Vec::new())
                .is_err()
        );
        assert_eq!(inner.diagnostics_snapshot().records[0], first);
        lock(&inner.store.writes).push_back(Fault::Error);
        with_transport(Ok(()), || {
            Arc::clone(&inner).run_recipient_queue("held".into())
        });
        let promoted = inner.diagnostics_snapshot().records[0].clone();
        assert_eq!(promoted.phase, SuppressionPhase::RetainedFence);
        assert_eq!(promoted.trigger_message_id, first.trigger_message_id);
        assert_eq!(promoted.hold_observed_at, first.hold_observed_at);
        assert_eq!(promoted.first_reason, FenceReason::QueueOverflow);
        assert_eq!(
            promoted.decisive_reason,
            FenceReason::UnresolvedReceiptStatus
        );
        assert_eq!(
            promoted.failure_stages,
            vec![
                FailureStage::ReceiptWriteError,
                FailureStage::UnresolvedReceiptStatus
            ]
        );
        assert!(lock(&inner.overflow_recipients).contains("held"));
    }

    #[test]
    fn diagnostics_prefix_spawn_failure_keeps_first_evidence() {
        let rig = rig();
        rig.inner.queue_limit_override.store(1, Ordering::SeqCst);
        let (accepted, _) = rig
            .inner
            .store
            .send_wire(mail("parked", "accepted"))
            .unwrap();
        lock(&rig.inner.dispatch_queues).insert(
            "parked".into(),
            RecipientDispatchQueue {
                pending: VecDeque::from([(accepted.id, At::from_unix_millis(1))]),
                waiting_for_route: true,
                ..Default::default()
            },
        );
        rig.inner
            .fail_next_worker_spawn
            .store(true, Ordering::SeqCst);
        let (rejected, receipts) = rig
            .inner
            .store
            .send_wire(mail("parked", "rejected"))
            .unwrap();
        assert!(
            rig.inner
                .schedule_dispatch(&rejected, &receipts, now_at(), &mut Vec::new())
                .is_err()
        );
        let row = rig.inner.diagnostics_snapshot().records[0].clone();
        assert_eq!(row.first_reason, FenceReason::QueueOverflow);
        assert_eq!(row.decisive_reason, FenceReason::WorkerSpawnFailure);
        assert_eq!(row.phase, SuppressionPhase::RetainedFence);
        assert_eq!(
            row.trigger_message_id.as_deref(),
            Some(rejected.id.as_str())
        );
        assert!(!lock(&rig.inner.dispatch_queues).contains_key("parked"));
    }

    #[test]
    fn diagnostics_hold_time_is_not_enqueue_time() {
        let rig = rig();
        rig.inner
            .fail_next_worker_spawn
            .store(true, Ordering::SeqCst);
        let (message, receipts) = rig.inner.store.send_wire(mail("held", "queued")).unwrap();
        let before = now_at();
        assert!(
            rig.inner
                .schedule_dispatch(
                    &message,
                    &receipts,
                    At::from_unix_millis(1),
                    &mut Vec::new()
                )
                .is_err()
        );
        let snapshot = rig.inner.diagnostics_snapshot();
        assert_eq!(snapshot.records.len(), 1);
        assert!(snapshot.records[0].hold_observed_at >= before);
        assert_eq!(
            snapshot.records[0].trigger_message_id.as_deref(),
            Some(message.id.as_str())
        );
        assert_eq!(
            snapshot.records[0].first_reason,
            FenceReason::WorkerSpawnFailure
        );
    }

    #[test]
    fn diagnostics_capacity_limit_preserves_suppression() {
        let rig = rig();
        for n in 0..128 {
            let row = FenceDiagnostic::from_report(
                &format!("evidence-{n}"),
                None,
                now_at(),
                SuppressionPhase::PrefixDraining,
                FenceReason::QueueOverflow,
                &DispatchReport::default(),
            );
            lock(&rig.inner.dispatch_diagnostics).retain(row);
        }
        rig.inner
            .fail_next_worker_spawn
            .store(true, Ordering::SeqCst);
        let (message, receipts) = rig.inner.store.send_wire(mail("held", "first")).unwrap();
        assert!(
            rig.inner
                .schedule_dispatch(&message, &receipts, now_at(), &mut Vec::new())
                .is_err()
        );
        assert!(lock(&rig.inner.overflow_recipients).contains("held"));
        assert!(!rig.inner.overflow_capacity_exhausted.load(Ordering::SeqCst));
        let snapshot = rig.inner.diagnostics_snapshot();
        assert_eq!(snapshot.records.len(), 128);
        assert!(snapshot.diagnostics_partial);
        assert_eq!(snapshot.omitted_evidence_total, 1);
        let (later, receipts) = rig.inner.store.send_wire(mail("held", "later")).unwrap();
        assert!(
            rig.inner
                .schedule_dispatch(&later, &receipts, now_at(), &mut Vec::new())
                .is_err()
        );
        assert!(lock(&rig.inner.dispatch_queues).is_empty());
        assert_eq!(
            rig.inner.store.list_wire_receipts(&later.id).unwrap()[0].attempts,
            0
        );
    }

    #[test]
    fn status_diagnostics_capability_and_serde_compatibility() {
        // A permissive older client knows only the pre-existing fields.
        #[derive(Debug, PartialEq, Deserialize)]
        struct OldStatus {
            node_id: String,
            host: String,
            socket_path: PathBuf,
            listen_addr: Option<String>,
            connected_peers: Vec<String>,
            routes: Vec<RouteEntry>,
        }
        let (_tmp, inner) = fault_rig();
        let (response, subscription) = inner.handle_local_request(LocalRequest::Status {
            include_codex_queues: false,
        });
        assert!(subscription.is_none());
        let LocalResponse::Status(status) = response else {
            panic!("expected Status")
        };
        let new_json = serde_json::to_value(&status).unwrap();
        assert!(
            new_json["dispatch_diagnostics"].is_object(),
            "supported-empty must be available"
        );
        assert_eq!(
            new_json["dispatch_diagnostics"]["records"],
            serde_json::json!([])
        );
        let old_client: OldStatus = serde_json::from_value(new_json.clone()).unwrap();
        let mut old_json = new_json;
        old_json
            .as_object_mut()
            .unwrap()
            .remove("dispatch_diagnostics");
        let new_client: DaemonStatus = serde_json::from_value(old_json.clone()).unwrap();
        assert!(new_client.dispatch_diagnostics.is_none());
        assert_eq!(serde_json::to_value(new_client).unwrap(), old_json);
        assert_eq!(
            serde_json::from_value::<OldStatus>(old_json).unwrap(),
            old_client
        );
    }

    #[test]
    fn repeated_status_clones_evidence_without_io_or_state_changes() {
        let (_tmp, inner) = fault_rig();
        lock(&inner.store.writes).push_back(Fault::Error);
        with_transport(Ok(()), || {
            run_fault_job(&inner, "held");
            let before = inner.diagnostics_snapshot();
            let calls = lock(&inner.store.calls).clone();
            let discoveries = crate::identity::DISCOVERIES.with(|n| n.get());
            lock(&inner.dispatch_queues).insert(
                "parked".into(),
                RecipientDispatchQueue {
                    pending: VecDeque::from([(
                        MessageId(fl_core::ids::seq_iri(999)),
                        At::from_unix_millis(1),
                    )]),
                    waiting_for_route: true,
                    ..Default::default()
                },
            );
            let queue_state = || {
                lock(&inner.dispatch_queues)
                    .iter()
                    .map(|(name, q)| {
                        (
                            name.clone(),
                            q.pending.clone(),
                            q.in_flight.clone(),
                            q.overflowed,
                            q.waiting_for_route,
                            q.waiting_for_capacity,
                            q.worker_active,
                            q.route_generation,
                        )
                    })
                    .collect::<Vec<_>>()
            };
            let queues = queue_state();
            let blocked = lock(&inner.overflow_recipients).clone();
            // Further I/O would fail, and must not consume the fault scripts.
            lock(&inner.store.gets).push_back(Fault::Panic);
            lock(&inner.store.reads).push_back(Fault::Panic);
            for _ in 0..5 {
                let (response, _) = inner.handle_local_request(LocalRequest::Status {
                    include_codex_queues: false,
                });
                // Serialize while explicitly owning the diagnostics lock: Status returned
                // an owned clone, so serialization needs no daemon guard.
                let _guard = lock(&inner.dispatch_diagnostics);
                let value = serde_json::to_value(response).unwrap();
                let snapshot = &value["dispatch_diagnostics"];
                assert_eq!(*snapshot, serde_json::to_value(&before).unwrap());
                assert!(snapshot["records"][0]["registration_generation"].is_null());
                assert_eq!(
                    snapshot["records"][0]["route_observations"][0]["outcome"],
                    "returned_ok"
                );
            }
            assert_eq!(*lock(&inner.store.calls), calls);
            assert_eq!(lock(&inner.store.gets).len(), 1);
            assert_eq!(lock(&inner.store.reads).len(), 1);
            assert_eq!(crate::identity::DISCOVERIES.with(|n| n.get()), discoveries);
            assert_eq!(TRANSPORT_CALLS.with(|n| n.get()), 1);
            assert_eq!(queue_state(), queues);
            assert_eq!(*lock(&inner.overflow_recipients), blocked);
            assert_eq!(inner.diagnostics_snapshot(), before);
        });
    }

    #[test]
    fn diagnostics_entropy_failure_does_not_block_startup_or_delivery() {
        ENTROPY_ATTEMPTS.with(|n| n.set(0));
        ENTROPY_FAILURE.with(|failure| failure.set(true));
        let (_tmp, inner) = fault_rig();
        ENTROPY_FAILURE.with(|failure| failure.set(false));
        assert!(inner.diagnostics_snapshot().instance_nonce.is_none());
        with_transport(Ok(()), || {
            let id = run_fault_job(&inner, "held");
            let receipt = inner.store.inner.list_wire_receipts(&id).unwrap().remove(0);
            assert_eq!(receipt.status, DeliveryStatus::Acked);
            assert_eq!(receipt.attempts, 1);
            assert_eq!(TRANSPORT_CALLS.with(|n| n.get()), 1);
        });
        let store_calls = lock(&inner.store.calls).clone();
        for _ in 0..3 {
            let (LocalResponse::Status(status), _) =
                inner.handle_local_request(LocalRequest::Status {
                    include_codex_queues: false,
                })
            else {
                panic!("expected Status");
            };
            assert!(
                status
                    .dispatch_diagnostics
                    .unwrap()
                    .instance_nonce
                    .is_none()
            );
        }
        assert_eq!(*lock(&inner.store.calls), store_calls);
        ENTROPY_ATTEMPTS.with(|n| assert_eq!(n.get(), 1));
    }

    #[test]
    fn registering_route_drains_parked_overflowed_prefix_then_fences() {
        let rig = rig();
        let mut ids = Vec::new();
        let mut pending = VecDeque::new();
        for subject in ["first", "second", "third"] {
            let (message, _) = rig
                .inner
                .store
                .send_wire(mail("overflow", subject))
                .unwrap();
            pending.push_back((message.id.clone(), now_at()));
            ids.push(message.id);
        }
        lock(&rig.inner.dispatch_queues).insert(
            "overflow".into(),
            RecipientDispatchQueue {
                pending,
                overflowed: true,
                ..Default::default()
            },
        );
        let socket = rig._tmp.path().join("overflow.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let (response, _) = rig.inner.handle_local_request(LocalRequest::register(
            "overflow",
            VendorRoute::Webhook {
                socket_path: socket,
            },
            true,
        ));
        assert!(matches!(response, LocalResponse::RouteRegistered));
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut received = Vec::new();
        while received.len() < ids.len() && Instant::now() < deadline {
            if let Ok((stream, _)) = listener.accept() {
                let mut line = String::new();
                BufReader::new(stream).read_line(&mut line).unwrap();
                received.push(serde_json::from_str::<WireMessage>(&line).unwrap().id);
            } else {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        assert_eq!(
            received, ids,
            "overflowed accepted prefix must drain in order"
        );
        let deadline = Instant::now() + Duration::from_secs(3);
        while lock(&rig.inner.dispatch_queues).contains_key("overflow") {
            assert!(Instant::now() < deadline, "drained queue not removed");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(lock(&rig.inner.overflow_recipients).contains("overflow"));
    }

    #[test]
    fn no_route_queue_reaching_limit_drains_accepted_prefix_on_registration() {
        let rig = rig();
        rig.inner.queue_limit_override.store(2, Ordering::SeqCst);
        let (entered_tx, entered_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        *lock(&rig.inner.no_route_hook) = Some((entered_tx, resume_rx));
        let (first, _) = rig.inner.handle_local_request(LocalRequest::Send {
            args: mail("limited", "first"),
        });
        let LocalResponse::Sent { message: first, .. } = first else {
            panic!("first send failed")
        };
        entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let mut ids = vec![first.id];
        for subject in ["second", "third", "overflow"] {
            let (response, _) = rig.inner.handle_local_request(LocalRequest::Send {
                args: mail("limited", subject),
            });
            let LocalResponse::Sent { message, .. } = response else {
                panic!("send failed")
            };
            ids.push(message.id);
        }
        assert!(lock(&rig.inner.dispatch_queues)["limited"].overflowed);
        let first_hold = rig.inner.diagnostics_snapshot().records[0].clone();
        assert_eq!(first_hold.phase, SuppressionPhase::PrefixDraining);
        assert_eq!(
            first_hold.trigger_message_id.as_deref(),
            Some(ids[3].as_str())
        );
        resume_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let parked = lock(&rig.inner.dispatch_queues)
                .get("limited")
                .is_some_and(|q| q.waiting_for_route && q.overflowed && !q.worker_active);
            if parked {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "overflowed no-route prefix did not park"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(rig.inner.diagnostics_snapshot().records[0], first_hold);
        let socket = rig._tmp.path().join("limited.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let (response, _) = rig.inner.handle_local_request(LocalRequest::register(
            "limited",
            VendorRoute::Webhook {
                socket_path: socket,
            },
            true,
        ));
        assert!(matches!(response, LocalResponse::RouteRegistered));
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut received = Vec::new();
        while received.len() < 3 && Instant::now() < deadline {
            if let Ok((stream, _)) = listener.accept() {
                let mut line = String::new();
                BufReader::new(stream).read_line(&mut line).unwrap();
                received.push(serde_json::from_str::<WireMessage>(&line).unwrap().id);
            } else {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        assert_eq!(received, ids[..3]);
        let deadline = Instant::now() + Duration::from_secs(3);
        while !lock(&rig.inner.overflow_recipients).contains("limited") {
            assert!(Instant::now() < deadline, "drained overflow was not fenced");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!lock(&rig.inner.dispatch_queues).contains_key("limited"));
        let retained = rig.inner.diagnostics_snapshot().records[0].clone();
        assert_eq!(retained.phase, SuppressionPhase::RetainedFence);
        assert_eq!(retained.trigger_message_id, first_hold.trigger_message_id);
        assert_eq!(retained.hold_observed_at, first_hold.hold_observed_at);
        assert_eq!(retained.first_reason, FenceReason::QueueOverflow);
        assert_eq!(retained.decisive_reason, FenceReason::QueueOverflow);
        assert_eq!(
            rig.inner.store.list_wire_receipts(&ids[3]).unwrap()[0].status,
            DeliveryStatus::Pending
        );
    }

    #[test]
    fn route_publication_and_no_route_eviction_are_serialized() {
        let rig = rig();
        let (no_route_tx, no_route_rx) = mpsc::channel();
        let (resume_worker_tx, resume_worker_rx) = mpsc::channel();
        *lock(&rig.inner.no_route_hook) = Some((no_route_tx, resume_worker_rx));
        let (response, _) = rig.inner.handle_local_request(LocalRequest::Send {
            args: mail("register-race", "first"),
        });
        let LocalResponse::Sent { message, .. } = response else {
            panic!("send failed")
        };
        no_route_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let socket = rig._tmp.path().join("register-race.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let (locked_tx, locked_rx) = mpsc::channel();
        let (resume_register_tx, resume_register_rx) = mpsc::channel();
        *lock(&rig.inner.registration_locked_hook) = Some((locked_tx, resume_register_rx));
        let inner = Arc::clone(&rig.inner);
        let register = std::thread::spawn(move || {
            inner
                .handle_local_request(LocalRequest::register(
                    "register-race",
                    VendorRoute::Webhook {
                        socket_path: socket,
                    },
                    true,
                ))
                .0
        });
        locked_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        resume_worker_tx.send(()).unwrap();
        resume_register_tx.send(()).unwrap();
        assert!(matches!(
            register.join().unwrap(),
            LocalResponse::RouteRegistered
        ));
        let deadline = Instant::now() + Duration::from_secs(5);
        let stream = loop {
            if let Ok((stream, _)) = listener.accept() {
                break stream;
            }
            assert!(Instant::now() < deadline, "registration wake was lost");
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
    fn capacity_waiter_with_overflowed_prefix_starts_when_slot_frees() {
        let rig = rig();
        let (message, _) = rig.inner.store.send_wire(mail("waiter", "first")).unwrap();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        *lock(&rig.inner.no_route_hook) = Some((entered_tx, resume_rx));
        let mut queues = lock(&rig.inner.dispatch_queues);
        queues.insert(
            "waiter".into(),
            RecipientDispatchQueue {
                pending: VecDeque::from([(message.id, now_at())]),
                overflowed: true,
                waiting_for_capacity: true,
                ..Default::default()
            },
        );
        drop(queues);
        rig.inner.wake_capacity_waiters();
        entered_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("overflowed capacity waiter was not started");
        assert!(
            lock(&rig.inner.dispatch_queues)
                .get("waiter")
                .unwrap()
                .worker_active,
            "overflow must fence new messages without stranding its accepted prefix"
        );
        resume_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !lock(&rig.inner.dispatch_queues)
            .get("waiter")
            .is_some_and(|q| q.waiting_for_route && !q.worker_active)
        {
            assert!(
                Instant::now() < deadline,
                "capacity waiter did not park after no-route result"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
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
                .schedule_dispatch(&first.0, &first.1, now_at(), &mut Vec::new())
                .is_err()
        );
        assert!(
            rig.inner
                .schedule_dispatch(&other.0, &other.1, now_at(), &mut Vec::new())
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
            owner.schedule_dispatch(&message, &receipts, now_at(), &mut Vec::new())
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
    fn no_route_workers_release_entries_and_leave_messages_in_inbox() {
        let rig = rig();
        for n in 0..=MAX_DISPATCH_RECIPIENTS {
            let name = format!("offline-{n}");
            let (response, _) = rig.inner.handle_local_request(LocalRequest::Send {
                args: mail(&name, "pending"),
            });
            assert!(matches!(response, LocalResponse::Sent { .. }));
            let deadline = Instant::now() + Duration::from_secs(5);
            while lock(&rig.inner.dispatch_queues).contains_key(&name) {
                assert!(
                    Instant::now() < deadline,
                    "idle queue was retained for {name}"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            let inbox = rig
                .inner
                .store
                .list_wire_inbox(&WireFilter {
                    recipient: Some(name),
                    unread_only: true,
                    ..Default::default()
                })
                .unwrap();
            assert_eq!(inbox.len(), 1);
            assert_eq!(inbox[0].receipt.status, DeliveryStatus::Pending);
        }
        assert!(lock(&rig.inner.dispatch_queues).is_empty());
        assert!(lock(&rig.inner.overflow_recipients).is_empty());
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
        while listener.accept().is_err() {
            assert!(
                Instant::now() < deadline,
                "offline recipients blocked a live recipient"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn no_route_exit_parks_the_accepted_overflow_prefix() {
        let rig = rig();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        *lock(&rig.inner.no_route_hook) = Some((entered_tx, resume_rx));
        let (response, _) = rig.inner.handle_local_request(LocalRequest::Send {
            args: mail("offline", "pending"),
        });
        assert!(matches!(response, LocalResponse::Sent { .. }));
        entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        lock(&rig.inner.dispatch_queues)
            .get_mut("offline")
            .unwrap()
            .overflowed = true;
        resume_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let parked = lock(&rig.inner.dispatch_queues)
                .get("offline")
                .is_some_and(|q| {
                    q.waiting_for_route && !q.worker_active && q.overflowed && q.pending.len() == 1
                });
            if parked {
                break;
            }
            assert!(Instant::now() < deadline, "overflow prefix was not parked");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!lock(&rig.inner.overflow_recipients).contains("offline"));
    }

    #[test]
    fn worker_spawn_failure_fences_and_removes_the_queue() {
        let rig = rig();
        rig.inner
            .fail_next_worker_spawn
            .store(true, Ordering::SeqCst);
        let (response, _) = rig.inner.handle_local_request(LocalRequest::Send {
            args: mail("spawn-failure", "persisted"),
        });
        assert!(matches!(response, LocalResponse::Sent { .. }));
        assert!(!lock(&rig.inner.dispatch_queues).contains_key("spawn-failure"));
        assert!(lock(&rig.inner.overflow_recipients).contains("spawn-failure"));
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
            .request(&LocalRequest::register(
                "slow",
                VendorRoute::Webhook {
                    socket_path: tmp.path().join("saved-route.sock"),
                },
                true,
            ))
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
    fn late_registration_does_not_replay_evicted_no_route_message() {
        let rig = rig();
        let (response, _) = rig.inner.handle_local_request(LocalRequest::Send {
            args: mail("new-route", "first"),
        });
        let LocalResponse::Sent { message, .. } = response else {
            panic!("send failed")
        };
        let deadline = Instant::now() + Duration::from_secs(3);
        while lock(&rig.inner.dispatch_queues).contains_key("new-route") {
            assert!(Instant::now() < deadline, "no-route queue was retained");
            std::thread::sleep(Duration::from_millis(10));
        }
        let socket = rig._tmp.path().join("late-route.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let (response, _) = rig.inner.handle_local_request(LocalRequest::register(
            "new-route",
            VendorRoute::Webhook {
                socket_path: socket,
            },
            true,
        ));
        assert!(matches!(response, LocalResponse::RouteRegistered));
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            listener.accept().is_err(),
            "registration replayed an evicted message"
        );
        let receipts = rig.inner.store.list_wire_receipts(&message.id).unwrap();
        assert_eq!(receipts[0].status, DeliveryStatus::Pending);
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
        let (response, _) = rig.inner.handle_local_request(LocalRequest::register(
            "racing-route",
            VendorRoute::Webhook {
                socket_path: socket,
            },
            true,
        ));
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
            let (register, _) = inner.handle_local_request(LocalRequest::register(
                "third",
                VendorRoute::Codex {
                    thread_id: "thread".into(),
                    cwd: None,
                },
                true,
            ));
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
            inner.handle_local_request(LocalRequest::register(
                "new-persona",
                VendorRoute::Codex {
                    thread_id: "test".into(),
                    cwd: None,
                },
                true,
            ))
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
            .schedule_dispatch(&message, &receipts, now_at(), &mut Vec::new())
            .unwrap_err();
        assert!(error.contains(message.id.as_str()));
        assert!(lock(&rig.inner.overflow_recipients).contains("slow"));
        lock(&rig.inner.dispatch_queues).remove("occupied-0");
        let (later, later_receipts) = rig.inner.store.send_wire(mail("slow", "later")).unwrap();
        assert!(
            rig.inner
                .schedule_dispatch(&later, &later_receipts, now_at(), &mut Vec::new())
                .is_err()
        );
        let (other, other_receipts) = rig
            .inner
            .store
            .send_wire(mail("other", "independent"))
            .unwrap();
        assert!(
            rig.inner
                .schedule_dispatch(&other, &other_receipts, now_at(), &mut Vec::new())
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
                .schedule_dispatch(&later, &receipts, now_at(), &mut Vec::new())
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
                .schedule_dispatch(&first, &receipts, now_at(), &mut Vec::new())
                .is_err()
        );
        assert!(rig.inner.overflow_capacity_exhausted.load(Ordering::SeqCst));
        let diagnostics = rig.inner.diagnostics_snapshot();
        assert!(diagnostics.global_capacity_exhausted);
        assert!(!diagnostics.diagnostics_partial);
        assert_eq!(
            diagnostics.records[0].first_reason,
            FenceReason::GlobalMetadataCap
        );
        assert_eq!(
            diagnostics.records[0].decisive_reason,
            FenceReason::GlobalMetadataCap
        );
        lock(&rig.inner.dispatch_queues).remove("active-0");
        let (later, receipts) = rig
            .inner
            .store
            .send_wire(mail("different", "later"))
            .unwrap();
        assert!(
            rig.inner
                .schedule_dispatch(&later, &receipts, now_at(), &mut Vec::new())
                .is_err()
        );
    }

    #[test]
    fn a_rig_daemon_registers_and_saves_routes_only_in_its_own_temp_dir() {
        let rig = rig();
        let path = rig._tmp.path().join("routes-b.json");
        assert!(rig.inner.routes.lock().unwrap().entries.is_empty());
        assert!(!path.exists());
        let (response, _) = rig.inner.handle_local_request(LocalRequest::register(
            "synthetic-route-owner",
            VendorRoute::Webhook {
                socket_path: rig._tmp.path().join("hook.sock"),
            },
            false,
        ));
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
        let (reply, _) = rig.inner.handle_local_request(LocalRequest::register(
            "ada",
            VendorRoute::Codex {
                thread_id: "new".into(),
                cwd: None,
            },
            true,
        ));
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
        let (reply, _) =
            rig.inner
                .handle_local_request(LocalRequest::register("ada", route.clone(), true));
        assert!(matches!(reply, LocalResponse::RouteRegistered));
        let (reply, _) = rig.inner.handle_local_request(LocalRequest::register(
            "ada",
            VendorRoute::Muse {
                session_id: "old".into(),
                cwd: None,
                tmux_target: None,
                tmux_pane: None,
            },
            false,
        ));
        assert!(matches!(reply, LocalResponse::Error { .. }));
        assert_eq!(lock(&rig.inner.routes).get("ada").unwrap().route, route);
    }

    #[test]
    fn a_passive_alias_cannot_claim_an_explicit_session_but_same_vendor_refresh_works() {
        let rig = rig();
        let (reply, _) = rig.inner.handle_local_request(LocalRequest::register(
            "ada",
            VendorRoute::Codex {
                thread_id: "current".into(),
                cwd: None,
            },
            true,
        ));
        assert!(matches!(reply, LocalResponse::RouteRegistered));
        let (reply, _) = rig.inner.handle_local_request(LocalRequest::register(
            "old-alias",
            VendorRoute::Codex {
                thread_id: "current".into(),
                cwd: None,
            },
            false,
        ));
        assert!(matches!(reply, LocalResponse::Error { .. }));
        let (reply, _) = rig.inner.handle_local_request(LocalRequest::register(
            "ada",
            VendorRoute::Codex {
                thread_id: "refreshed".into(),
                cwd: None,
            },
            false,
        ));
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
        let (tx, rx) = mpsc::sync_channel(MAX_SUBSCRIBER_BUFFER);
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
        let routes_b = tmp.path().join("routes-b.json");
        let mut routes = RouteTable::load(Some(&routes_b));
        routes.upsert_explicit(
            "synthetic-recipient",
            VendorRoute::Webhook {
                socket_path: webhook_sock,
            },
            now_at(),
        );
        routes.save(Some(&routes_b)).unwrap();

        let sock_b = tmp.path().join("wire-b.sock");
        let _daemon_b = spawn_daemon(
            Arc::clone(&store_b),
            DaemonConfig {
                node_id: "node-b".into(),
                host: "host-b".into(),
                socket_path: sock_b.clone(),
                routes_path: Some(routes_b),
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
        let status_personas = || match request(LocalRequest::Status {
            include_codex_queues: false,
        }) {
            LocalResponse::Status(st) => {
                st.routes.into_iter().map(|r| r.persona).collect::<Vec<_>>()
            }
            other => panic!("unexpected response: {other:?}"),
        };
        request(LocalRequest::register(
            "lmcfarlin-e6",
            VendorRoute::Claude {
                pid: 4242,
                session_id: "s-derived".into(),
                socket_path: tmp.path().join("claude.sock"),
                key_path: None,
            },
            false,
        ));
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
        request(LocalRequest::register(
            "other",
            VendorRoute::Codex {
                thread_id: "t".into(),
                cwd: None,
            },
            false,
        ));
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
        reg.request(&LocalRequest::register(
            "synthetic-recipient",
            VendorRoute::Codex {
                thread_id: "t1".into(),
                cwd: None,
            },
            false,
        ))
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
        probe
            .request(&LocalRequest::Status {
                include_codex_queues: false,
            })
            .unwrap();
        let other_socket = tmp.path().join("other.sock");
        let other_listener = UnixListener::bind(&other_socket).unwrap();
        other_listener.set_nonblocking(true).unwrap();
        // The daemon serves one request per connection.
        let mut probe = LocalClient::connect(&sock, Duration::from_secs(5)).unwrap();
        probe
            .request(&LocalRequest::register(
                "other",
                VendorRoute::Webhook {
                    socket_path: other_socket,
                },
                false,
            ))
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
            .request(&LocalRequest::register(
                "synthetic-recipient",
                VendorRoute::Codex {
                    thread_id: "t".into(),
                    cwd: None,
                },
                false,
            ))
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
                .request(&LocalRequest::register(persona, route, false))
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
            ok.request(&LocalRequest::Status {
                include_codex_queues: false,
            })
            .unwrap(),
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
        assert!(
            ok.request(&LocalRequest::Status {
                include_codex_queues: false,
            })
            .is_ok()
        );
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
        let resp = client.request(&LocalRequest::Status {
            include_codex_queues: false,
        });
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

    #[test]
    fn ack_and_read_ack_clear_overflow_fence_and_resume_push_delivery() {
        let rig = rig();

        // 1. Verify LocalRequest::Ack clears overflow_recipients and resumes pending push delivery.
        let ack_socket = rig._tmp.path().join("ack-recover.sock");
        lock(&rig.inner.routes).upsert_explicit(
            "ack-agent",
            VendorRoute::Webhook {
                socket_path: ack_socket.clone(),
            },
            now_at(),
        );
        let (resp1, _) = rig.inner.handle_local_request(LocalRequest::Send {
            args: mail("ack-agent", "first-unconfirmed"),
        });
        let LocalResponse::Sent { message: m1, .. } = resp1 else {
            panic!("send failed: {resp1:?}")
        };
        let deadline = Instant::now() + Duration::from_secs(3);
        while !lock(&rig.inner.overflow_recipients).contains("ack-agent") {
            assert!(Instant::now() < deadline, "ack-agent was never fenced");
            std::thread::sleep(Duration::from_millis(10));
        }

        let ack_listener = UnixListener::bind(&ack_socket).unwrap();
        ack_listener.set_nonblocking(true).unwrap();

        let (resp2, _) = rig.inner.handle_local_request(LocalRequest::Send {
            args: mail("ack-agent", "second-queued-behind-fence"),
        });
        let LocalResponse::Sent { message: m2, .. } = resp2 else {
            panic!("second send failed: {resp2:?}")
        };
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            ack_listener.accept().is_err(),
            "second message must remain blocked while first is unacknowledged"
        );

        let (ack_resp, _) = rig.inner.handle_local_request(LocalRequest::Ack {
            message_ids: vec![m1.id.clone()],
            recipient: "ack-agent".into(),
        });
        assert!(matches!(ack_resp, LocalResponse::Acked { .. }));
        assert!(
            !lock(&rig.inner.overflow_recipients).contains("ack-agent"),
            "Ack should clear overflow_recipients"
        );

        let deadline = Instant::now() + Duration::from_secs(3);
        let mut accepted = false;
        while Instant::now() < deadline {
            if let Ok((mut stream, _)) = ack_listener.accept() {
                let mut payload = String::new();
                let _ = stream.read_to_string(&mut payload);
                assert!(
                    payload.contains(m2.id.as_str()),
                    "unexpected payload: {payload}"
                );
                accepted = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            accepted,
            "second message was not pushed after Ack cleared the fence"
        );

        // 2. Verify LocalRequest::Read { ack: true, .. } clears overflow_recipients and allows subsequent pushes.
        let read_socket = rig._tmp.path().join("read-recover.sock");
        lock(&rig.inner.routes).upsert_explicit(
            "read-agent",
            VendorRoute::Webhook {
                socket_path: read_socket.clone(),
            },
            now_at(),
        );
        let (resp3, _) = rig.inner.handle_local_request(LocalRequest::Send {
            args: mail("read-agent", "first-for-read"),
        });
        assert!(matches!(resp3, LocalResponse::Sent { .. }));
        let deadline = Instant::now() + Duration::from_secs(3);
        while !lock(&rig.inner.overflow_recipients).contains("read-agent") {
            assert!(Instant::now() < deadline, "read-agent was never fenced");
            std::thread::sleep(Duration::from_millis(10));
        }

        let read_listener = UnixListener::bind(&read_socket).unwrap();
        read_listener.set_nonblocking(true).unwrap();

        let (read_resp, _) = rig.inner.handle_local_request(LocalRequest::Read {
            filter: WireFilter {
                recipient: Some("read-agent".into()),
                unread_only: true,
                ..Default::default()
            },
            ack: true,
        });
        assert!(matches!(read_resp, LocalResponse::Inbox { .. }));
        assert!(
            !lock(&rig.inner.overflow_recipients).contains("read-agent"),
            "Read(ack=true) should clear overflow_recipients"
        );

        let (resp4, _) = rig.inner.handle_local_request(LocalRequest::Send {
            args: mail("read-agent", "after-read-ack"),
        });
        let LocalResponse::Sent { message: m4, .. } = resp4 else {
            panic!("fourth send failed: {resp4:?}")
        };
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut read_accepted = false;
        while Instant::now() < deadline {
            if let Ok((mut stream, _)) = read_listener.accept() {
                let mut payload = String::new();
                let _ = stream.read_to_string(&mut payload);
                assert!(
                    payload.contains(m4.id.as_str()),
                    "unexpected payload: {payload}"
                );
                read_accepted = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            read_accepted,
            "subsequent message was not pushed after Read(ack=true)"
        );
    }

    #[test]
    fn peer_sync_cursors_and_chunked_sync_batch_sync_asymmetric_stores_within_frame_bounds() {
        let tmp = TempDir::new().unwrap();
        let store_a = Arc::new(RedbStore::open(&tmp.path().join("asym-a.redb")).unwrap());
        let store_b = Arc::new(RedbStore::open(&tmp.path().join("asym-b.redb")).unwrap());

        // Pre-populate store_a with 1 message and store_b with 3 messages BEFORE connecting:
        // Previously, store_b passed its local seq (3) as last_msg_seq to store_a, causing store_a
        // to skip its seq 1 message!
        let (msg_a1, _) = store_a
            .send_wire(mail("synthetic-recipient", "from-a-1"))
            .unwrap();
        let (msg_b1, _) = store_b
            .send_wire(mail("synthetic-recipient", "from-b-1"))
            .unwrap();
        let (msg_b2, _) = store_b
            .send_wire(mail("synthetic-recipient", "from-b-2"))
            .unwrap();
        let (msg_b3, _) = store_b
            .send_wire(mail("synthetic-recipient", "from-b-3"))
            .unwrap();

        // Also verify build_sync_batches chunks > MAX_SYNC_BATCH_ITEMS messages into multiple bounded batches.
        let many_msgs: Vec<WireMessage> = (0..(MAX_SYNC_BATCH_ITEMS + 5))
            .map(|_| msg_a1.clone())
            .collect();
        let chunks = build_sync_batches(vec![], many_msgs, vec![]);
        assert_eq!(chunks.len(), 2);
        for chunk in &chunks {
            let encoded = serde_json::to_vec(chunk).unwrap();
            assert!(encoded.len() < MAX_FRAME_PAYLOAD);
        }

        let mut cfg_a = test_config(tmp.path(), "asym-a");
        cfg_a.listen_addr = Some("127.0.0.1:0".into());
        let daemon_a = spawn_daemon(Arc::clone(&store_a), cfg_a).unwrap();
        let addr_a = daemon_a.listen_addr().unwrap().to_string();

        let mut cfg_b = test_config(tmp.path(), "asym-b");
        cfg_b.peers = vec![format!("ws://{addr_a}/wire")];
        let _daemon_b = spawn_daemon(Arc::clone(&store_b), cfg_b).unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let b_has_a1 = store_b.get_wire_message(&msg_a1.id).unwrap().is_some();
            let a_has_b1 = store_a.get_wire_message(&msg_b1.id).unwrap().is_some();
            let a_has_b2 = store_a.get_wire_message(&msg_b2.id).unwrap().is_some();
            let a_has_b3 = store_a.get_wire_message(&msg_b3.id).unwrap().is_some();
            if b_has_a1 && a_has_b1 && a_has_b2 && a_has_b3 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for bidirectional asymmetric SyncBatch (b_has_a1={b_has_a1}, a_has_b1={a_has_b1}, a_has_b2={a_has_b2}, a_has_b3={a_has_b3})"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn slow_subscriber_is_dropped_at_bounded_capacity_and_socket_is_mode_0600() {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(RedbStore::open(&tmp.path().join("sub-cap.redb")).unwrap());
        let cfg = test_config(tmp.path(), "sub-cap");
        let sock = cfg.socket_path.clone();
        let _daemon = spawn_daemon(Arc::clone(&store), cfg).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&sock).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "expected 0o600 on fl-wire.sock, got {mode:#o}");
        }

        let rig = rig();
        let _rx = subscribe_all(&rig);
        assert_eq!(lock(&rig.inner.subscribers).len(), 1);
        let (msg, receipts) = rig
            .source
            .send_wire(send_args("cap", "bounded subscriber check", 1))
            .unwrap();
        for _ in 0..=MAX_SUBSCRIBER_BUFFER {
            rig.inner.notify_subscribers_message(&msg, &receipts);
        }
        assert!(
            lock(&rig.inner.subscribers).is_empty(),
            "slow subscriber exceeding MAX_SUBSCRIBER_BUFFER must be dropped via try_send"
        );
    }

    #[test]
    fn resolve_socket_path_follows_precedence_and_ignores_relative_env() {
        struct SocketEnvGuard(Option<std::ffi::OsString>);
        impl Drop for SocketEnvGuard {
            fn drop(&mut self) {
                unsafe {
                    match self.0.take() {
                        Some(v) => std::env::set_var("FL_WIRE_SOCKET", v),
                        None => std::env::remove_var("FL_WIRE_SOCKET"),
                    }
                }
            }
        }

        let _tmux_lock = crate::dispatcher::TMUX_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _codex_lock = crate::dispatcher::CODEX_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _env_guard = SocketEnvGuard(std::env::var_os("FL_WIRE_SOCKET"));

        let explicit = Path::new("/tmp/explicit-wire.sock");
        let configured = Path::new("/tmp/configured-wire.sock");
        let explicit_db = Path::new("/tmp/custom-db-dir/store.redb");
        let env_abs = Path::new("/tmp/env-wire.sock");

        // SAFETY: guarded by TMUX_ENV_LOCK and CODEX_ENV_LOCK; restored by _env_guard.
        unsafe {
            std::env::set_var("FL_WIRE_SOCKET", env_abs);
        }

        // 1. Explicit path wins over FL_WIRE_SOCKET, configured, and explicit_db.
        assert_eq!(
            resolve_socket_path(Some(explicit), Some(configured), Some(explicit_db)),
            explicit
        );

        // 2. Absolute FL_WIRE_SOCKET wins over configured and explicit_db.
        assert_eq!(
            resolve_socket_path(None, Some(configured), Some(explicit_db)),
            env_abs
        );

        // 3. Relative FL_WIRE_SOCKET is ignored; configured path wins over explicit_db.
        unsafe {
            std::env::set_var("FL_WIRE_SOCKET", "relative/wire.sock");
        }
        assert_eq!(
            resolve_socket_path(None, Some(configured), Some(explicit_db)),
            configured
        );

        // 4. Without explicit/env/configured, explicit_db parent directory supplies fl-wire.sock.
        assert_eq!(
            resolve_socket_path(None, None, Some(explicit_db)),
            PathBuf::from("/tmp/custom-db-dir/fl-wire.sock")
        );

        // 5. Bare filename explicit_db (empty parent) falls back to default_socket_path().
        unsafe {
            std::env::remove_var("FL_WIRE_SOCKET");
        }
        assert_eq!(
            resolve_socket_path(None, None, Some(Path::new("store.redb"))),
            default_socket_path()
        );
        assert_eq!(resolve_socket_path(None, None, None), default_socket_path());
    }

    #[test]
    fn status_populates_codex_queues_outside_daemon_state_locks_in_single_session() {
        use crate::codex_control::fake_server::spawn_at;
        use fl_core::agent::AgentRole;
        use serde_json::json;

        let rig = rig();
        for (name, project) in [("Felix", "ferroloop"), ("Kacey", "ferroloop")] {
            rig.inner
                .store
                .register_agent(RegisterAgentArgs {
                    display_name: name.into(),
                    email: None,
                    project: project.into(),
                    role: AgentRole::Assistant,
                    extra_areas: vec![],
                    create_area: true,
                    replace: false,
                    at: now_at(),
                    by: name.to_ascii_lowercase(),
                })
                .unwrap();
        }

        for (persona, route) in [
            (
                "felix",
                VendorRoute::Codex {
                    thread_id: "thread-felix".into(),
                    cwd: None,
                },
            ),
            (
                "ghost",
                VendorRoute::Codex {
                    thread_id: "   ".into(),
                    cwd: None,
                },
            ),
            (
                "kacey",
                VendorRoute::Codex {
                    thread_id: "thread-kacey".into(),
                    cwd: None,
                },
            ),
            (
                "ada",
                VendorRoute::Webhook {
                    socket_path: rig._tmp.path().join("ada.sock"),
                },
            ),
        ] {
            let (resp, _) = rig
                .inner
                .handle_local_request(LocalRequest::register(persona, route, true));
            assert!(matches!(resp, LocalResponse::RouteRegistered));
        }

        let control_sock = rig
            ._tmp
            .path()
            .join(".codex/app-server-control/app-server-control.sock");
        let (entered_tx, entered_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();

        let now_ms = now_at().unix_millis();
        let ts_ms = now_ms.saturating_sub(15_000);
        let uuid_v7 = format!(
            "{:08x}-{:04x}-7000-8000-000000000001",
            (ts_ms >> 16) as u32,
            (ts_ms & 0xFFFF) as u16
        );

        let server = spawn_at(&control_sock, move |c| {
            c.handshake();
            let init = c.expect_json();
            assert_eq!(init["method"], "initialize");
            entered_tx.send(()).unwrap();
            resume_rx.recv_timeout(Duration::from_secs(3)).unwrap();
            c.send_text(
                &json!({
                    "id": 1,
                    "result": {
                        "userAgent": format!("ferroloop/{VERIFIED_CODEX_QUEUE_RPC_VERSION} (Linux; x86_64)"),
                        "codexHome": "/tmp/.codex",
                        "platformFamily": "unix",
                        "platformOs": "linux",
                    }
                })
                .to_string(),
            );
            assert_eq!(c.expect_json()["method"], "initialized");

            let q1 = c.expect_json();
            assert_eq!(q1["id"], 2);
            assert_eq!(q1["params"]["threadId"], "thread-felix");
            c.send_text(
                &json!({
                    "id": 2,
                    "result": {
                        "data": [
                            {"id": uuid_v7, "clientUserMessageId": "c1", "input": []},
                            {"id": "msg-2", "clientUserMessageId": "c2", "input": []},
                        ],
                        "nextCursor": "100",
                    }
                })
                .to_string(),
            );

            let q2 = c.expect_json();
            assert_eq!(q2["id"], 3);
            assert_eq!(q2["params"]["threadId"], "thread-kacey");
            c.send_text(
                &json!({
                    "id": 3,
                    "result": {
                        "data": [],
                        "nextCursor": null,
                    }
                })
                .to_string(),
            );
            c.drain_until_eof();
        });

        let inner_for_status = Arc::clone(&rig.inner);
        let status_thread = std::thread::spawn(move || {
            inner_for_status.handle_local_request(LocalRequest::Status {
                include_codex_queues: true,
            })
        });

        // Wait until LocalRequest::Status is parked inside codex_control socket I/O.
        entered_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("status entered codex_control socket I/O");

        // Prove no daemon state lock is held while Status waits on the control socket:
        // mutating route table, enqueue_gate, and diagnostics all complete promptly.
        let (reg_resp, _) = rig.inner.handle_local_request(LocalRequest::register(
            "concurrent",
            VendorRoute::Webhook {
                socket_path: rig._tmp.path().join("concurrent.sock"),
            },
            true,
        ));
        assert!(matches!(reg_resp, LocalResponse::RouteRegistered));
        let (unreg_resp, _) = rig
            .inner
            .handle_local_request(LocalRequest::UnregisterRoute {
                persona: "concurrent".into(),
            });
        assert!(matches!(unreg_resp, LocalResponse::RouteUnregistered));
        let _ = rig.inner.diagnostics_snapshot();

        resume_tx.send(()).unwrap();
        let (LocalResponse::Status(status), _) = status_thread.join().unwrap() else {
            panic!("expected Status");
        };
        let log = server.join().unwrap();
        // Single WebSocket handshake + 1 initialize + 1 initialized + 2 thread/queue/list frames (+ close).
        let text_methods: Vec<String> = log
            .iter()
            .filter(|f| f.opcode == 0x1)
            .map(|f| f.json()["method"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(
            text_methods,
            [
                "initialize",
                "initialized",
                "thread/queue/list",
                "thread/queue/list"
            ]
        );

        assert_eq!(status.codex_queues.len(), 3);
        assert_eq!(status.codex_queues[0].persona, "felix");
        assert_eq!(status.codex_queues[0].project.as_deref(), Some("ferroloop"));
        assert_eq!(
            status.codex_queues[0].thread_id.as_deref(),
            Some("thread-felix")
        );
        match &status.codex_queues[0].queue {
            CodexQueueInspection::Known {
                depth,
                has_more,
                estimated_oldest_age_secs,
                verified_codex_version,
            } => {
                assert_eq!(*depth, 2);
                assert!(*has_more);
                let age = estimated_oldest_age_secs.expect("UUIDv7 oldest age");
                assert!((14..=17).contains(&age), "unexpected age {age}");
                assert_eq!(verified_codex_version, VERIFIED_CODEX_QUEUE_RPC_VERSION);
            }
            other => panic!("expected Known for felix, got {other:?}"),
        }

        assert_eq!(status.codex_queues[1].persona, "ghost");
        assert_eq!(status.codex_queues[1].thread_id, None);
        assert!(matches!(
            &status.codex_queues[1].queue,
            CodexQueueInspection::Unknown { reason, .. } if reason.contains("no thread_id")
        ));

        assert_eq!(status.codex_queues[2].persona, "kacey");
        assert_eq!(status.codex_queues[2].project.as_deref(), Some("ferroloop"));
        assert_eq!(
            status.codex_queues[2].queue,
            CodexQueueInspection::Known {
                depth: 0,
                has_more: false,
                estimated_oldest_age_secs: None,
                verified_codex_version: VERIFIED_CODEX_QUEUE_RPC_VERSION.into(),
            }
        );

        // Older daemon JSON without `codex_queues` still deserializes cleanly to empty Vec.
        let mut legacy_json = serde_json::to_value(&status).unwrap();
        legacy_json.as_object_mut().unwrap().remove("codex_queues");
        let parsed: DaemonStatus = serde_json::from_value(legacy_json).unwrap();
        assert!(parsed.codex_queues.is_empty());

        // Future daemon JSON with an unrecognized queue `state` and extra unknown fields
        // deserializes cleanly to `CodexQueueInspection::Unsupported`.
        let mut future_json = serde_json::to_value(&status).unwrap();
        future_json["codex_queues"] = json!([
            {
                "persona": "felix",
                "project": "ferroloop",
                "queue": {
                    "state": "future_v2",
                    "extra": 42,
                    "nested": {"hint": "ignored"}
                }
            }
        ]);
        let future_parsed: DaemonStatus = serde_json::from_str(&future_json.to_string()).unwrap();
        assert_eq!(future_parsed.codex_queues.len(), 1);
        assert_eq!(
            future_parsed.codex_queues[0].queue,
            CodexQueueInspection::Unsupported
        );
    }

    #[test]
    fn status_without_opt_in_is_wire_compatible_and_opens_zero_codex_control_connections() {
        use serde_json::json;

        // 1. Wire-format backward compatibility with older clients and older daemons.
        #[derive(Debug, Deserialize, PartialEq, Eq)]
        #[serde(tag = "op", rename_all = "snake_case")]
        enum OldLocalRequest {
            Status,
        }

        let legacy_req: LocalRequest = serde_json::from_str(r#"{"op":"status"}"#).unwrap();
        assert!(matches!(
            legacy_req,
            LocalRequest::Status {
                include_codex_queues: false
            }
        ));
        let default_serialized = serde_json::to_value(&LocalRequest::Status {
            include_codex_queues: false,
        })
        .unwrap();
        assert_eq!(default_serialized, json!({"op": "status"}));
        assert_eq!(
            serde_json::from_value::<OldLocalRequest>(default_serialized).unwrap(),
            OldLocalRequest::Status
        );

        let opt_in_serialized = serde_json::to_value(&LocalRequest::Status {
            include_codex_queues: true,
        })
        .unwrap();
        assert_eq!(
            opt_in_serialized,
            json!({"op": "status", "include_codex_queues": true})
        );
        assert_eq!(
            serde_json::from_value::<OldLocalRequest>(opt_in_serialized.clone()).unwrap(),
            OldLocalRequest::Status
        );
        let opt_in_roundtrip: LocalRequest = serde_json::from_value(opt_in_serialized).unwrap();
        assert!(matches!(
            opt_in_roundtrip,
            LocalRequest::Status {
                include_codex_queues: true
            }
        ));

        // 2. Daemon with registered Codex routes + live mock Codex control socket listener:
        //    non-opt-in Status (both Rust variant and raw legacy `{"op":"status"}` wire JSON)
        //    must open ZERO connections on the Codex control socket and return empty `codex_queues`.
        let tmp = TempDir::new().unwrap();
        let control_sock = tmp
            .path()
            .join(".codex/app-server-control/app-server-control.sock");
        fs::create_dir_all(control_sock.parent().unwrap()).unwrap();
        let listener = UnixListener::bind(&control_sock).unwrap();
        listener.set_nonblocking(true).unwrap();

        let accept_count = Arc::new(AtomicUsize::new(0));
        let stop_accept = Arc::new(AtomicBool::new(false));
        let accept_count_bg = Arc::clone(&accept_count);
        let stop_accept_bg = Arc::clone(&stop_accept);
        let accept_watcher = std::thread::spawn(move || {
            while !stop_accept_bg.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((_stream, _)) => {
                        accept_count_bg.fetch_add(1, Ordering::SeqCst);
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        let store = Arc::new(RedbStore::open(&tmp.path().join("s.redb")).unwrap());
        let mut cfg = test_config(tmp.path(), "no-codex-probe");
        cfg.home_override = Some(tmp.path().to_path_buf());
        let daemon_sock = cfg.socket_path.clone();
        let mut daemon = spawn_daemon(Arc::clone(&store), cfg).unwrap();

        let mut reg = LocalClient::connect(&daemon_sock, DEFAULT_LOCAL_RPC_TIMEOUT).unwrap();
        let reg_resp = reg
            .request(&LocalRequest::register(
                "felix",
                VendorRoute::Codex {
                    thread_id: "thread-felix".into(),
                    cwd: None,
                },
                true,
            ))
            .unwrap();
        assert!(matches!(reg_resp, LocalResponse::RouteRegistered));

        // Default non-opt-in LocalRequest::Status via LocalClient.
        let mut c1 = LocalClient::connect(&daemon_sock, DEFAULT_LOCAL_RPC_TIMEOUT).unwrap();
        let LocalResponse::Status(st1) = c1
            .request(&LocalRequest::Status {
                include_codex_queues: false,
            })
            .unwrap()
        else {
            panic!("expected Status");
        };
        assert_eq!(st1.routes.len(), 1);
        assert!(st1.codex_queues.is_empty());

        // Legacy raw `{"op":"status"}` line from an older client.
        let mut raw = UnixStream::connect(&daemon_sock).unwrap();
        raw.set_read_timeout(Some(DEFAULT_LOCAL_RPC_TIMEOUT))
            .unwrap();
        raw.write_all(b"{\"op\":\"status\"}\n").unwrap();
        let mut reply_line = String::new();
        BufReader::new(&raw).read_line(&mut reply_line).unwrap();
        let LocalResponse::Status(st2) = serde_json::from_str(&reply_line).unwrap() else {
            panic!("expected Status from legacy raw JSON, got {reply_line}");
        };
        assert_eq!(st2.routes.len(), 1);
        assert!(st2.codex_queues.is_empty());

        std::thread::sleep(Duration::from_millis(30));
        stop_accept.store(true, Ordering::SeqCst);
        accept_watcher.join().unwrap();
        daemon.stop();

        assert_eq!(
            accept_count.load(Ordering::SeqCst),
            0,
            "non-opt-in Status must open zero connections on Codex control socket"
        );
    }

    #[test]
    fn register_route_and_diagnostics_two_way_wire_compatibility() {
        use crate::dispatch_diagnostics::{
            RegistrationConflictReason, RegistrationDiagnosticOutcome, RouteAdapter,
        };
        use serde_json::json;

        #[derive(Debug, Deserialize, PartialEq, Eq)]
        #[serde(tag = "op", rename_all = "snake_case")]
        enum LegacyLocalRequest {
            RegisterRoute {
                persona: String,
                route: VendorRoute,
                #[serde(default)]
                explicit: bool,
            },
        }

        let new_req = LocalRequest::RegisterRoute {
            persona: "feira".into(),
            route: VendorRoute::Codex {
                thread_id: "t-compat".into(),
                cwd: None,
            },
            explicit: true,
            replace: true,
            hosted_id: Some("codex:feira".into()),
            hydration_generation: Some("gen-2".into()),
            hosted_session_verified: true,
            unverified_process: true,
        };
        let encoded = serde_json::to_string(&new_req).unwrap();
        let legacy_decoded: LegacyLocalRequest = serde_json::from_str(&encoded).unwrap();
        assert_eq!(
            legacy_decoded,
            LegacyLocalRequest::RegisterRoute {
                persona: "feira".into(),
                route: VendorRoute::Codex {
                    thread_id: "t-compat".into(),
                    cwd: None,
                },
                explicit: true,
            }
        );

        let rig = rig();
        let diag = RouteRegistrationDiagnostic {
            at: At::from_unix_millis(5000),
            last_seen_at: None,
            count: 1,
            persona: "Feira".into(),
            outcome: RegistrationDiagnosticOutcome::Refused,
            reason: RegistrationConflictReason::ForeignHostedEnv,
            incumbent_adapter: Some(RouteAdapter::Agy),
            challenger_adapter: RouteAdapter::Claude,
            incumbent_hosted_id: Some("antigravity:feira".into()),
            challenger_hosted_id: Some("antigravity:feira".into()),
            incumbent_generation: Some("g1".into()),
            challenger_generation: None,
            replace: false,
            unverified_process: false,
        };
        let (resp, _) = rig
            .inner
            .handle_local_request(LocalRequest::RecordRegistrationDiagnostic { diagnostic: diag });
        assert!(matches!(
            resp,
            LocalResponse::RegistrationDiagnosticRecorded
        ));

        let (LocalResponse::Status(status), _) =
            rig.inner.handle_local_request(LocalRequest::Status {
                include_codex_queues: false,
            })
        else {
            panic!("expected Status");
        };
        assert_eq!(status.registration_diagnostics.len(), 1);
        assert_eq!(status.registration_diagnostics[0].persona, "feira");

        let future_diag_json = json!({
            "at": status.registration_diagnostics[0].at,
            "persona": "feira",
            "outcome": "future_outcome",
            "reason": "future_reason",
            "challenger_adapter": "claude",
            "replace": false,
        });
        let future_diag: RouteRegistrationDiagnostic =
            serde_json::from_value(future_diag_json).unwrap();
        assert_eq!(
            future_diag.outcome,
            RegistrationDiagnosticOutcome::Unsupported
        );
        assert_eq!(future_diag.reason, RegistrationConflictReason::Unsupported);
    }

    #[test]
    fn dead_explicit_route_with_held_mail_reports_dead_explicit_route_in_daemon_status() {
        let mut child = std::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let dead_pid = child.id();
        let _ = child.wait();

        let rig = rig();
        let dead_route = VendorRoute::Claude {
            pid: dead_pid,
            session_id: "s-dead".into(),
            socket_path: rig._tmp.path().join("missing-claude.sock"),
            key_path: None,
        };
        let (reg_resp, _) = rig
            .inner
            .handle_local_request(LocalRequest::register("feira", dead_route, true));
        assert!(matches!(reg_resp, LocalResponse::RouteRegistered));

        let (sent_resp, _) = rig.inner.handle_local_request(LocalRequest::Send {
            args: mail("feira", "held behind dead explicit route"),
        });
        assert!(matches!(sent_resp, LocalResponse::Sent { .. }));

        let deadline = Instant::now() + Duration::from_secs(3);
        let status = loop {
            let (LocalResponse::Status(st), _) =
                rig.inner.handle_local_request(LocalRequest::Status {
                    include_codex_queues: false,
                })
            else {
                panic!("expected Status");
            };
            let settled = st
                .dispatch_diagnostics
                .as_ref()
                .is_some_and(|d| !d.records.is_empty());
            if settled || Instant::now() >= deadline {
                break st;
            }
            std::thread::sleep(Duration::from_millis(10));
        };

        let pending = status
            .pending_recipients
            .iter()
            .find(|p| p.recipient == "feira")
            .expect("feira pending summary");
        assert_eq!(pending.pending_count, 1);
        assert_eq!(
            pending.hold_reason,
            Some(PendingHoldReason::DeadExplicitRoute)
        );

        let diag = status
            .dispatch_diagnostics
            .as_ref()
            .and_then(|d| {
                d.records
                    .iter()
                    .find(|r| r.recipient.as_deref() == Some("feira"))
            })
            .expect("feira fence diagnostic");
        assert_eq!(diag.decisive_reason, FenceReason::DeadExplicitRoute);
    }

    #[test]
    fn status_preserves_store_and_scheduler_fence_reasons_over_dead_explicit_route() {
        let mut child = std::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let dead_pid = child.id();
        let _ = child.wait();

        let rig = rig();
        let dead_route = VendorRoute::Claude {
            pid: dead_pid,
            session_id: "s-dead".into(),
            socket_path: rig._tmp.path().join("missing-claude.sock"),
            key_path: None,
        };
        let (reg_resp, _) = rig
            .inner
            .handle_local_request(LocalRequest::register("feira", dead_route, true));
        assert!(matches!(reg_resp, LocalResponse::RouteRegistered));

        let (msg, receipts) = rig
            .inner
            .store
            .send_wire(mail("feira", "held after store fault"))
            .unwrap();
        rig.inner.admit_pending_receipts(&msg, &receipts);

        for preserved_reason in [
            FenceReason::ReceiptWriteError,
            FenceReason::FinalReceiptReadError,
            FenceReason::WorkerPanic,
            FenceReason::GlobalMetadataCap,
        ] {
            {
                let mut diags = lock(&rig.inner.dispatch_diagnostics);
                diags.snapshot.records.clear();
                diags.retain(FenceDiagnostic::from_report(
                    "feira",
                    Some(&msg.id),
                    At::from_unix_millis(10),
                    SuppressionPhase::RetainedFence,
                    preserved_reason,
                    &DispatchReport::default(),
                ));
            }
            let (LocalResponse::Status(status), _) =
                rig.inner.handle_local_request(LocalRequest::Status {
                    include_codex_queues: false,
                })
            else {
                panic!("expected Status");
            };
            let pending = status
                .pending_recipients
                .iter()
                .find(|p| p.recipient == "feira")
                .expect("feira pending summary");
            assert_eq!(
                pending.hold_reason,
                Some(PendingHoldReason::DeadExplicitRoute)
            );
            let diag = status
                .dispatch_diagnostics
                .as_ref()
                .and_then(|d| {
                    d.records
                        .iter()
                        .find(|r| r.recipient.as_deref() == Some("feira"))
                })
                .expect("feira fence diagnostic");
            assert_eq!(
                diag.decisive_reason, preserved_reason,
                "store/scheduler fault {preserved_reason:?} must not be overwritten by DeadExplicitRoute"
            );
        }

        for promoted_reason in [
            FenceReason::TransportError,
            FenceReason::UnresolvedReceiptStatus,
        ] {
            {
                let mut diags = lock(&rig.inner.dispatch_diagnostics);
                diags.snapshot.records.clear();
                diags.retain(FenceDiagnostic::from_report(
                    "feira",
                    Some(&msg.id),
                    At::from_unix_millis(10),
                    SuppressionPhase::RetainedFence,
                    promoted_reason,
                    &DispatchReport::default(),
                ));
            }
            let (LocalResponse::Status(status), _) =
                rig.inner.handle_local_request(LocalRequest::Status {
                    include_codex_queues: false,
                })
            else {
                panic!("expected Status");
            };
            let diag = status
                .dispatch_diagnostics
                .as_ref()
                .and_then(|d| {
                    d.records
                        .iter()
                        .find(|r| r.recipient.as_deref() == Some("feira"))
                })
                .expect("feira fence diagnostic");
            assert_eq!(
                diag.decisive_reason,
                FenceReason::DeadExplicitRoute,
                "transport-level {promoted_reason:?} should promote to DeadExplicitRoute"
            );
        }
    }

    #[test]
    fn non_opt_in_status_skips_host_tcp_probes_for_explicit_routes() {
        let mut child = std::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let dead_pid = child.id();
        let _ = child.wait();

        let probe_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        probe_listener.set_nonblocking(true).unwrap();
        let probe_addr = probe_listener.local_addr().unwrap();

        let tmp = TempDir::new().unwrap();
        let store = Arc::new(RedbStore::open(&tmp.path().join("dest.redb")).unwrap());
        let mut inner = rig_inner(&tmp, store);
        Arc::get_mut(&mut inner)
            .expect("unique inner")
            .config
            .home_override = None;

        let agy_route = VendorRoute::Agy {
            conversation_id: "conv-probe-gate".into(),
            ls_pid: Some(dead_pid),
            ls_address: Some(probe_addr.to_string()),
            csrf_token: None,
            agentapi_exe: None,
        };
        let (reg_resp, _) =
            inner.handle_local_request(LocalRequest::register("feira", agy_route, true));
        assert!(matches!(reg_resp, LocalResponse::RouteRegistered));

        let (msg, receipts) = inner.store.send_wire(mail("feira", "pending")).unwrap();
        inner.admit_pending_receipts(&msg, &receipts);

        let (LocalResponse::Status(_), _) = inner.handle_local_request(LocalRequest::Status {
            include_codex_queues: false,
        }) else {
            panic!("expected Status");
        };

        assert!(
            matches!(
                probe_listener.accept(),
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock
            ),
            "Status {{ include_codex_queues: false }} must not TCP-probe explicit Agy ls_address"
        );
    }

    #[test]
    fn so_peercred_verification_rejects_unverified_pid_claiming_hosted_session_verified() {
        let rig = rig();
        let mut incumbent_child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let incumbent_pid = incumbent_child.id();

        let (resp1, _) = rig.inner.handle_local_request(LocalRequest::RegisterRoute {
            persona: "feira".into(),
            route: VendorRoute::Claude {
                session_id: "sid-incumbent".into(),
                pid: incumbent_pid,
                socket_path: PathBuf::from(format!("/tmp/claude-{incumbent_pid}.sock")),
                key_path: None,
            },
            explicit: true,
            replace: false,
            hosted_id: Some("claude:feira".into()),
            hydration_generation: Some("gen-1".into()),
            hosted_session_verified: true,
            unverified_process: false,
        });
        assert!(matches!(resp1, LocalResponse::RouteRegistered));

        let mut challenger_child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let challenger_pid = challenger_child.id();

        // Challenger claims `hosted_session_verified: true` with the same generation,
        // but daemon-observed PeerCred shows `challenger_pid` is not `incumbent_pid` or its child.
        let (resp2, _) = rig.inner.handle_local_request_with_peer(
            LocalRequest::RegisterRoute {
                persona: "feira".into(),
                route: VendorRoute::Claude {
                    session_id: "sid-challenger".into(),
                    pid: challenger_pid,
                    socket_path: PathBuf::from(format!("/tmp/claude-{challenger_pid}.sock")),
                    key_path: None,
                },
                explicit: true,
                replace: false,
                hosted_id: Some("claude:feira".into()),
                hydration_generation: Some("gen-1".into()),
                hosted_session_verified: true,
                unverified_process: false,
            },
            PeerCred {
                pid: Some(challenger_pid),
                ppid: Some(std::process::id()),
                start_time: crate::identity::proc_start_time_ticks(challenger_pid),
            },
        );
        let _ = incumbent_child.kill();
        let _ = incumbent_child.wait();
        let _ = challenger_child.kill();
        let _ = challenger_child.wait();

        assert!(
            matches!(resp2, LocalResponse::Error { .. }),
            "daemon must reject spoofed hosted_session_verified when PeerCred PID does not match live incumbent"
        );
    }
}
