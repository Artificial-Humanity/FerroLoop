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
use fl_core::state::{StateConcern, StatePayload, StateScope};
use fl_core::store::{AgentRegistry, MessageStore, StateStore};
use serde::{Deserialize, Serialize};

pub use crate::codex_control::{
    CODEX_QUEUE_INSPECT_TIMEOUT, CODEX_QUEUE_LIST_LIMIT, CodexQueueInspection,
    CodexRouteQueueStatus, VERIFIED_CODEX_QUEUE_RPC_VERSION, parse_uuid_v7_unix_ms,
};
use crate::dispatch_diagnostics::{
    DiagnosticsState, DispatchCallOutcome, DispatchDiagnostics, DispatchReport, FailureStage,
    FenceDiagnostic, FenceReason, FenceRecoveryAttempt, FenceRecoveryOutcome,
    RouteRegistrationDiagnostic, SuppressionPhase,
};
use crate::dispatcher::{
    RecoveryReadiness, RecoveryTarget, dispatch_message_for_recipient_on_recovery_target,
    dispatch_message_for_recipient_with_report, probe_retained_fence_readiness,
};
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

pub fn default_socket_path_with(
    xdg_runtime_dir: Option<&std::ffi::OsStr>,
    run_user_dir: Option<&Path>,
    home: Option<&std::ffi::OsStr>,
) -> PathBuf {
    if let Some(rt) = crate::identity::runtime_dir_path_with(xdg_runtime_dir, run_user_dir) {
        return rt.join("fl-wire.sock");
    }
    if let Some(home) = home.filter(|s| !s.is_empty()) {
        let p = PathBuf::from(home);
        if p.is_absolute() {
            return p.join(".cache").join("fl").join("wire.sock");
        }
    }
    PathBuf::from("/tmp/fl-wire.sock")
}

pub fn default_socket_path() -> PathBuf {
    if let Some(rt) = crate::identity::runtime_dir_path() {
        return rt.join("fl-wire.sock");
    }
    default_socket_path_with(None, None, std::env::var_os("HOME").as_deref())
}

#[cfg(unix)]
pub fn default_socket_path_for_root(
    xdg_runtime_dir: Option<&std::ffi::OsStr>,
    run_user_root: &Path,
    home: Option<&std::ffi::OsStr>,
    expected_uid: u32,
) -> PathBuf {
    let probed = crate::identity::probe_run_user_dir(run_user_root, expected_uid);
    default_socket_path_with(xdg_runtime_dir, probed.as_deref(), home)
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

// Read the hosted reference from the daemon's store, never from a route or client claim.
// Missing, unavailable, or pid-less records retain the existing unverified behavior.
type HostedReference = (u32, Option<u64>);
type RecoveryHostLookup<S> =
    fn(&S, &str, &RouteEntry) -> Result<Option<RecoveryHostBinding>, FenceRecoveryOutcome>;

#[derive(Debug, PartialEq, Eq)]
struct RecoveryHostBinding {
    session_id: String,
    runtime_leader_pid: Option<u32>,
    startup_confirmed: bool,
}

fn recovery_host_session<S: StateStore>(
    store: &S,
    recipient: &str,
    route: &RouteEntry,
) -> Result<Option<RecoveryHostBinding>, FenceRecoveryOutcome> {
    use FenceRecoveryOutcome::HostedBindingUnavailable;

    let vendor = route.route.canonical_vendor();
    let slug = format!("hosted-session-{vendor}-{recipient}");
    let state = store
        .get_state_by_key(&StateScope::Workspace, StateConcern::Runtime, &slug)
        .map_err(|_| HostedBindingUnavailable)?;
    let claims_host = route.hosted_id.is_some() || route.hydration_generation.is_some();
    let Some(state) = state else {
        return if claims_host {
            Err(HostedBindingUnavailable)
        } else {
            Ok(None)
        };
    };
    let StatePayload::Runtime(runtime) = state.payload else {
        return Err(HostedBindingUnavailable);
    };
    let value = &runtime.value;
    if !claims_host && value["hosted"].as_bool() == Some(false) {
        return Ok(None);
    }
    let matching_identity = |raw: Option<&str>| {
        raw.and_then(|id| crate::identity::parse_hosted_identity(id).ok())
            .is_some_and(|(v, p)| v == vendor && p == recipient)
    };
    let generation = value["hydration_generation"]
        .as_str()
        .filter(|g| !g.is_empty());
    if !matching_identity(route.hosted_id.as_deref())
        || !matching_identity(value["session_id"].as_str())
        || !value["persona"]
            .as_str()
            .is_some_and(|name| name.eq_ignore_ascii_case(recipient))
        || value["vendor"]
            .as_str()
            .map(crate::identity::canonical_vendor_id)
            != Some(vendor)
        || generation.is_none()
        || generation != route.hydration_generation.as_deref()
        || value["vendor_session_id"]
            .as_str()
            .filter(|id| crate::identity::is_native_session_id(id))
            .is_some_and(|id| {
                !route
                    .route
                    .session_identity()
                    .is_some_and(|(_, registered)| registered == id)
            })
    {
        return Err(HostedBindingUnavailable);
    }
    if value["status"].as_str() != Some("running") {
        return Err(FenceRecoveryOutcome::DeadRoute);
    }
    let host_session_id = value["host_session_id"].as_str();
    if !matching_identity(host_session_id) {
        return Err(HostedBindingUnavailable);
    }
    Ok(host_session_id.map(|session_id| RecoveryHostBinding {
        session_id: session_id.to_owned(),
        runtime_leader_pid: value["runtime_leader_pid"]
            .as_u64()
            .and_then(|raw| u32::try_from(raw).ok())
            .filter(|pid| *pid > 0),
        startup_confirmed: value["hydration_status"].as_str() == Some("confirmed"),
    }))
}
type HostedLookup<S> = fn(&S, &str, &str, Option<&str>, Option<&str>) -> Option<HostedReference>;

fn hosted_reference<S: StateStore>(
    store: &S,
    persona: &str,
    vendor: &str,
    _hosted_id: Option<&str>,
    _generation: Option<&str>,
) -> Option<HostedReference> {
    if !crate::vendor_adapter::tool_processes_descend_from_cli(vendor) {
        return None;
    }
    let slug = format!("hosted-session-{vendor}-{persona}");
    let entry = store
        .get_state_by_key(&StateScope::Workspace, StateConcern::Runtime, &slug)
        .ok()??;
    let StatePayload::Runtime(runtime) = entry.payload else {
        return None;
    };
    let value = runtime.value;
    let session_id = value.get("session_id")?.as_str()?;
    let pair = crate::identity::parse_hosted_identity(session_id).ok()?;
    if pair != (vendor.to_owned(), persona.to_owned())
        || !value
            .get("persona")?
            .as_str()?
            .eq_ignore_ascii_case(persona)
        || crate::identity::canonical_vendor_id(value.get("vendor")?.as_str()?) != vendor
        || value
            .get("status")?
            .as_str()?
            .eq_ignore_ascii_case("decommissioned")
    {
        return None;
    }
    // A claim is untrusted input: even a malformed ID or stale generation must not
    // suppress checking the independently selected persona/vendor reference PID.
    let pid = u32::try_from(value.get("pid")?.as_u64()?)
        .ok()
        .filter(|p| *p > 0)?;
    let start = value.get("pid_start_time").and_then(|v| v.as_u64());
    Some((pid, start))
}

fn verify_peer_cred_for_hosted_claim(
    peer_cred: PeerCred,
    trusted_reference: Option<HostedReference>,
    replace: bool,
    challenger_route: &VendorRoute,
    challenger_generation: Option<&str>,
    incumbent: Option<&RouteEntry>,
    incumbent_is_dead: bool,
) -> bool {
    if !cfg!(target_os = "linux") {
        return true;
    }
    let Some(peer_pid) = peer_cred.pid else {
        return true;
    };
    if let Some((pid, start)) = trusted_reference {
        if !crate::identity::proc_alive_matching_start(pid, start) {
            return false;
        }
        let effective_pid = if crate::identity::proc_is_shell_wrapper(pid) {
            crate::identity::resolve_vendor_cli_pid(pid).unwrap_or(pid)
        } else {
            pid
        };
        let effective_start = if effective_pid == pid {
            start
        } else {
            crate::identity::proc_start_time_ticks(effective_pid)
        };
        if !crate::identity::pid_is_nearest_vendor_ancestor(
            peer_pid,
            effective_pid,
            effective_start,
        ) {
            return false;
        }
    }
    if let Some(challenger_pid) = challenger_route.route_pid()
        && !(trusted_reference.is_some() && matches!(challenger_route, VendorRoute::Agy { .. }))
        && !crate::identity::pid_is_nearest_vendor_ancestor(peer_pid, challenger_pid, None)
    {
        return false;
    }
    // A detached tool process cannot prove ancestry against either the hosted CLI or
    // an incumbent. Explicit replacement still applies, but remains unverified.
    let detached_tool_replacement = replace
        && !crate::vendor_adapter::tool_processes_descend_from_cli(
            challenger_route.canonical_vendor(),
        );
    if !(replace && trusted_reference.is_some() || detached_tool_replacement)
        && let Some(inc) = incumbent
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
            if !crate::identity::pid_is_nearest_vendor_ancestor(
                peer_pid,
                inc_pid,
                inc.pid_start_time,
            ) {
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

fn is_zero_usize(n: &usize) -> bool {
    *n == 0
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
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub awaiting_daemon_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oldest_awaiting_daemon_at: Option<At>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oldest_awaiting_daemon_age_ms: Option<u64>,
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
    let awaiting: BTreeMap<MessageId, At> = store
        .list_wire_awaiting_daemon()
        .unwrap_or_default()
        .into_iter()
        .collect();
    summarize_pending_map_with_awaiting(&seed_pending_map_from_store(store), &awaiting, now)
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

fn summarize_pending_map_with_awaiting(
    by_recipient: &BTreeMap<String, BTreeMap<MessageId, At>>,
    awaiting_by_msg: &BTreeMap<MessageId, At>,
    now: &At,
) -> Vec<RecipientPendingSummary> {
    let now_ms = now.unix_millis();
    let mut out = Vec::new();
    for (recipient, messages) in by_recipient {
        if messages.is_empty() {
            continue;
        }
        let oldest_ms = messages.values().map(At::unix_millis).min();
        let mut awaiting_count = 0usize;
        let mut oldest_awaiting_ms: Option<u64> = None;
        for mid in messages.keys() {
            if let Some(queued_at) = awaiting_by_msg.get(mid) {
                awaiting_count += 1;
                let q_ms = queued_at.unix_millis();
                oldest_awaiting_ms = Some(oldest_awaiting_ms.map_or(q_ms, |prev| prev.min(q_ms)));
            }
        }
        out.push(RecipientPendingSummary {
            recipient: recipient.clone(),
            pending_count: messages.len(),
            oldest_pending_at: oldest_ms.map(At::from_unix_millis),
            oldest_pending_age_ms: oldest_ms.map(|ms| now_ms.saturating_sub(ms)),
            hold_reason: None,
            awaiting_daemon_count: awaiting_count,
            oldest_awaiting_daemon_at: oldest_awaiting_ms.map(At::from_unix_millis),
            oldest_awaiting_daemon_age_ms: oldest_awaiting_ms.map(|ms| now_ms.saturating_sub(ms)),
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
        #[cfg(feature = "wire-test-isolation")]
        crate::test_isolation::ensure_socket(socket_path)?;
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
const MAX_REGISTER_ROUTE_ATTEMPTS: usize = 8;
const REGISTER_ROUTE_DEADLINE: Duration = Duration::from_secs(2);

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
const DEFAULT_FENCE_RECOVERY_DELAY_SECS: u64 = 300;
const DEFAULT_FENCE_RECOVERY_MAX_ATTEMPTS: u32 = 3;
const MAX_FENCE_PROBES_PER_POLL: usize = 4;
const FENCE_BUSY_ATTENTION_PROBES: u32 = 12;

#[derive(Clone, Copy)]
struct FenceRecoveryPolicy {
    delay_ms: u64,
    max_attempts: u32,
}

impl Default for FenceRecoveryPolicy {
    fn default() -> Self {
        Self {
            delay_ms: DEFAULT_FENCE_RECOVERY_DELAY_SECS * 1_000,
            max_attempts: DEFAULT_FENCE_RECOVERY_MAX_ATTEMPTS,
        }
    }
}

impl FenceRecoveryPolicy {
    fn from_env() -> Self {
        Self {
            delay_ms: recovery_env(
                "FL_WIRE_FENCE_RECOVERY_DELAY_SECS",
                DEFAULT_FENCE_RECOVERY_DELAY_SECS,
                1,
            )
            .saturating_mul(1_000),
            max_attempts: recovery_env(
                "FL_WIRE_FENCE_RECOVERY_MAX_ATTEMPTS",
                DEFAULT_FENCE_RECOVERY_MAX_ATTEMPTS,
                0,
            ),
        }
    }
}

fn recovery_env<T>(key: &str, default: T, minimum: T) -> T
where
    T: std::str::FromStr + PartialOrd + std::fmt::Display,
{
    let value = match std::env::var(key) {
        Ok(value) => value.parse::<T>().ok(),
        Err(std::env::VarError::NotPresent) => return default,
        Err(std::env::VarError::NotUnicode(_)) => None,
    };
    if let Some(value) = value
        && value >= minimum
    {
        return value;
    }
    warn(&format!(
        "{key} must be an integer of at least {minimum}; using the default"
    ));
    default
}

#[derive(Clone)]
struct RouteFenceRelease {
    winning: VendorRoute,
    explicit: bool,
    route_gone_before: bool,
    at: At,
}

#[derive(Default)]
struct DeferredFenceRelease {
    route: Option<RouteFenceRelease>,
    ack: Option<At>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RetainedFencePhase {
    Waiting,
    InProgress,
    /// Unadmitted messages keep the fence after the timed run ends.
    OverflowOnly,
    /// A fault stopped automatic replay and requires a winning route replacement.
    ReplayStopped,
}

struct RetainedFenceRecovery {
    token: Arc<()>,
    route: Option<RouteEntry>,
    pending: VecDeque<MessageId>,
    /// No unrelated inbox item or pre-restart delivery is admitted to this queue.
    pending_capacity_exhausted: bool,
    since: At,
    last_attempt: Option<At>,
    attempts: u32,
    busy_since: Option<At>,
    consecutive_busy_probes: u32,
    phase: RetainedFencePhase,
    deferred_release: DeferredFenceRelease,
}

impl RetainedFenceRecovery {
    fn new(route: Option<RouteEntry>, since: At, phase: RetainedFencePhase) -> Self {
        Self {
            token: Arc::new(()),
            route,
            pending: VecDeque::new(),
            pending_capacity_exhausted: false,
            since,
            last_attempt: None,
            attempts: 0,
            busy_since: None,
            consecutive_busy_probes: 0,
            phase,
            deferred_release: DeferredFenceRelease::default(),
        }
    }

    fn hold(&mut self, id: MessageId) {
        if self.pending.contains(&id) {
            return;
        }
        if self.pending.len() < MAX_QUEUED_PER_RECIPIENT {
            self.pending.push_back(id);
        } else {
            self.pending_capacity_exhausted = true;
        }
    }
}

#[derive(Clone)]
struct FenceRecoveryRun {
    token: Arc<()>,
    route: Option<RouteEntry>,
    attempt: u32,
    at: At,
}

impl FenceRecoveryRun {
    fn is_current(&self, recovery: &RetainedFenceRecovery) -> bool {
        Arc::ptr_eq(&self.token, &recovery.token)
            && self.attempt == recovery.attempts
            && recovery.last_attempt.as_ref() == Some(&self.at)
            && recovery.phase == RetainedFencePhase::InProgress
    }
}

const TIMED_FENCE_REDELIVERY_NOTICE: &str = "Note: this is a redelivery after timed fence recovery; an earlier delivery attempt was unconfirmed and may already have been received.";

#[derive(Default)]
struct RecipientDispatchQueue {
    pending: VecDeque<(MessageId, At)>,
    in_flight: Option<MessageId>,
    overflowed: bool,
    waiting_for_route: bool,
    waiting_for_capacity: bool,
    worker_active: bool,
    route_generation: u64,
    recovery: Option<FenceRecoveryRun>,
    recovery_redeliveries: BTreeSet<MessageId>,
    released_route: Option<RouteEntry>,
    composer_retry_at: Option<At>,
    composer_backoff: Duration,
    composer_reprobes: u32,
    composer_busy_since: Option<At>,
}

#[derive(Clone, Copy)]
enum DispatchOutcome {
    Resolved,
    RecoverySubmitted,
    ComposerBusy,
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
        FailureStage::InitialReceiptReadMissing => FenceReason::InitialReceiptReadMissing,
        FailureStage::TransportError => FenceReason::TransportError,
        FailureStage::DeadExplicitRoute => FenceReason::DeadExplicitRoute,
        FailureStage::ComposerOccupiedByNonWireInput => FenceReason::ComposerOccupiedByNonWireInput,
        FailureStage::ReceiptWriteError => FenceReason::ReceiptWriteError,
        FailureStage::ReceiptWriteMissing => FenceReason::ReceiptWriteMissing,
        FailureStage::FinalReceiptReadError => FenceReason::FinalReceiptReadError,
        FailureStage::FinalReceiptReadMissing => FenceReason::FinalReceiptReadMissing,
        FailureStage::UnresolvedReceiptStatus => FenceReason::UnresolvedReceiptStatus,
        FailureStage::UnconfirmedReceiptAfterSubmit => FenceReason::UnconfirmedReceiptAfterSubmit,
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

/// True when `new` is a different route identity than `old`: another vendor, another
/// vendor session, or (for identity-less routes such as webhooks) a different endpoint.
fn route_identity_changed(
    old: &crate::identity::VendorRoute,
    new: &crate::identity::VendorRoute,
) -> bool {
    old.canonical_vendor() != new.canonical_vendor()
        || old.session_identity() != new.session_identity()
        || (old.session_identity().is_none() && old != new)
}

fn readiness_result(
    readiness: RecoveryReadiness,
    startup_confirmed: bool,
) -> Result<RecoveryTarget, FenceRecoveryOutcome> {
    match readiness {
        RecoveryReadiness::Hold(
            outcome @ (FenceRecoveryOutcome::VendorSignIn | FenceRecoveryOutcome::DialogWaiting),
        ) => Err(outcome),
        _ if !startup_confirmed => Err(FenceRecoveryOutcome::UnconfirmedStartup),
        RecoveryReadiness::Ready(target) => Ok(target),
        RecoveryReadiness::Hold(outcome) => Err(outcome),
    }
}

fn is_transport_failure(stage: FailureStage) -> bool {
    matches!(
        stage,
        FailureStage::TransportError
            | FailureStage::DeadExplicitRoute
            | FailureStage::ComposerOccupiedByNonWireInput
            | FailureStage::UnresolvedReceiptStatus
    )
}

fn replay_stop_outcome(report: &DispatchReport) -> Option<FenceRecoveryOutcome> {
    if report.omitted_failure_stages > 0
        || report
            .failure_stages
            .iter()
            .any(|stage| matches!(stage, FailureStage::WorkerPanic | FailureStage::Unsupported))
    {
        Some(FenceRecoveryOutcome::ReplayWorkerFailed)
    } else if report
        .failure_stages
        .iter()
        .any(|stage| !is_transport_failure(*stage))
    {
        Some(FenceRecoveryOutcome::ReceiptBookkeepingFailed)
    } else {
        None
    }
}

fn is_confirmed_receipt(receipt: &DeliveryReceipt) -> bool {
    matches!(
        receipt.status,
        DeliveryStatus::Delivered | DeliveryStatus::Acked
    )
}

fn transport_fence_can_recover(row: &FenceDiagnostic) -> bool {
    row.route_dispatch_calls > 0
        && row.omitted_failure_stages == 0
        && row.failure_stages.iter().copied().all(is_transport_failure)
        && row
            .route_observations
            .iter()
            .all(|observation| observation.outcome == DispatchCallOutcome::ReturnedError)
}

fn log_first_holds(rows: Vec<FenceDiagnostic>) {
    for row in rows {
        // All strings are bounded safe projections; JSON escapes control bytes.
        if let Ok(json) = serde_json::to_string(&row) {
            warn(&format!("dispatch hold: {json}"));
        }
    }
}

#[cfg(test)]
type RegistrationProbeHook = Box<dyn FnMut(&mut RouteTable) + Send>;

#[cfg(test)]
type FenceRecoveryProbeHook = Box<dyn FnMut() -> RecoveryReadiness + Send>;

#[cfg(test)]
type ComposerProbeHook = Box<dyn FnMut(&str, &VendorRoute) -> Option<bool> + Send>;

const MAX_AWAITING_DAEMON_AGE_MS: u64 = 24 * 60 * 60 * 1000;
const AWAITING_DAEMON_POLL_INTERVAL: Duration = if cfg!(test) {
    Duration::from_millis(50)
} else {
    Duration::from_secs(2)
};

pub const COMPOSER_OCCUPIED_REPROBE_INITIAL_BACKOFF: Duration = Duration::from_secs(10);
pub const COMPOSER_OCCUPIED_REPROBE_MAX_BACKOFF: Duration = Duration::from_secs(120);
pub const COMPOSER_OCCUPIED_MAX_REPROBES: u32 = 16;

#[derive(Debug, Clone)]
struct ComposerOccupiedReprobeEntry {
    next_probe_at: At,
    backoff: Duration,
    probes_run: u32,
    /// Recognised composer payload (`"/clear"`/`"/new"`) at hold time, if any.
    recognised_payload: Option<&'static str>,
}

fn recognised_payload_suffix(payload: Option<&'static str>) -> String {
    match payload {
        Some(payload) => format!("; holds \"{payload}\"; repair: Backspace, then Ctrl-U"),
        None => String::new(),
    }
}

#[derive(Default)]
struct AwaitingDaemonEntry {
    queued_at: Option<At>,
    notified: bool,
    waiting_for_route: bool,
    scheduled_routed: BTreeSet<String>,
    tried_unrouted: BTreeSet<String>,
    unscheduled_recipients: BTreeSet<String>,
}

struct DaemonInner<S> {
    store: Arc<S>,
    hosted_lookup: HostedLookup<S>,
    recovery_host_lookup: RecoveryHostLookup<S>,
    fence_recovery_policy: FenceRecoveryPolicy,
    fence_recoveries: Mutex<BTreeMap<String, RetainedFenceRecovery>>,
    #[cfg(test)]
    fence_recovery_probe_hook: Mutex<Option<FenceRecoveryProbeHook>>,
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
    awaiting_daemon_state: Mutex<BTreeMap<MessageId, AwaitingDaemonEntry>>,
    // Daemon-lifetime only: in-memory reprobe state, lost on restart (which re-arms the
    // budget). An exhausted entry is reset only by a new message, a read/ack, or restart.
    composer_occupied_reprobes: Mutex<BTreeMap<String, ComposerOccupiedReprobeEntry>>,
    /// Route identity recorded when a fence was raised, for B2/S2 release checks.
    fence_routes: Mutex<BTreeMap<String, crate::identity::VendorRoute>>,
    #[cfg(test)]
    composer_probe_hook: Mutex<Option<ComposerProbeHook>>,
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
    #[cfg(test)]
    registration_probe_hook: Mutex<Option<RegistrationProbeHook>>,
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
        self: &Arc<Self>,
        msg: &WireMessage,
        recipient: &str,
        now: At,
        recovery: Option<(&RouteEntry, RecoveryTarget, Option<RecoveryHostBinding>)>,
        report: &mut DispatchReport,
    ) -> Result<(DispatchOutcome, Option<RouteEntry>), FenceRecoveryOutcome> {
        let pinned_replay = recovery.is_some();
        let (updated, attempted, route) = if let Some((entry, target, binding)) = recovery {
            let gate_read_failure = std::cell::Cell::new(None);
            let result = dispatch_message_for_recipient_on_recovery_target(
                self.store.as_ref(),
                msg,
                now.clone(),
                recipient,
                report,
                entry,
                target,
                || {
                    let binding_check = if let Some(ref expected) = binding {
                        (self.recovery_host_lookup)(self.store.as_ref(), recipient, entry).and_then(
                            |current| match current {
                                Some(current) if !current.startup_confirmed => {
                                    Err(FenceRecoveryOutcome::UnconfirmedStartup)
                                }
                                Some(current) if &current == expected => Ok(()),
                                _ => Err(FenceRecoveryOutcome::HostedBindingUnavailable),
                            },
                        )
                    } else {
                        Ok(())
                    };
                    // Acknowledgments can arrive during host capture or controller
                    // acquisition. Read the receipt again before mutating the terminal.
                    let receipt = self
                        .store
                        .list_wire_receipts(&msg.id)
                        .map_err(|_| {
                            gate_read_failure.set(Some(FailureStage::InitialReceiptReadError));
                            FenceRecoveryOutcome::InboxUnavailable
                        })?
                        .into_iter()
                        .find(|receipt| receipt.recipient_name.eq_ignore_ascii_case(recipient))
                        .ok_or_else(|| {
                            gate_read_failure.set(Some(FailureStage::InitialReceiptReadMissing));
                            FenceRecoveryOutcome::InboxUnavailable
                        })?;
                    if receipt.status != DeliveryStatus::Pending {
                        return Ok(false);
                    }
                    binding_check?;
                    if receipt.attempts >= crate::dispatcher::MAX_DELIVER {
                        return Err(FenceRecoveryOutcome::ReplayFailed);
                    }
                    if !self.recovery_route_is_current(recipient, entry) {
                        return Err(FenceRecoveryOutcome::RouteChanged);
                    }
                    Ok(true)
                },
            );
            if let Some(stage) = gate_read_failure.get() {
                report.record_failure(stage);
            }
            let (updated, attempted) = result?;
            (updated, attempted, Some(entry.clone()))
        } else {
            self.dispatch_and_refresh_route(msg, recipient, now.clone(), report)
        };
        // The pinned path selects one recipient. A declined submission observes
        // ReturnedOk for AlreadyResolved but leaves attempted false.
        let recovery_submitted = pinned_replay
            && attempted == Some(true)
            && report
                .route_observations
                .iter()
                .any(|row| row.outcome == DispatchCallOutcome::ReturnedOk);
        let confirmed_update = recovery_submitted && updated.iter().any(is_confirmed_receipt);
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
        let confirmed_submit =
            confirmed_update && receipt.as_ref().is_some_and(is_confirmed_receipt);
        let had_failure = !report.failure_stages.is_empty() || report.omitted_failure_stages > 0;
        if recovery_submitted && !confirmed_submit && !had_failure {
            report.record_failure(FailureStage::UnconfirmedReceiptAfterSubmit);
        }
        let outcome = match receipt {
            _ if confirmed_submit && !had_failure => DispatchOutcome::RecoverySubmitted,
            // Even a concurrent ACK cannot confirm that this attempt's receipt write
            // succeeded. Stop timed replay before it reaches another message.
            _ if pinned_replay && (had_failure || recovery_submitted) => {
                DispatchOutcome::Unconfirmed
            }
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
        };
        Ok((outcome, route))
    }

    fn dispatch_and_refresh_route(
        self: &Arc<Self>,
        msg: &WireMessage,
        recipient: &str,
        now: At,
        report: &mut DispatchReport,
    ) -> (Vec<DeliveryReceipt>, Option<bool>, Option<RouteEntry>) {
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
            now.clone(),
            recipient,
            report,
        );
        // Discovery inside the dispatch also learns replaced sessions and prunes dead routes;
        // carry both back, and persist them so a restart starts from the refreshed table.
        let attempted_route = working.get(recipient).cloned();
        let changed = !working.same_routes(&base);
        let mut live = lock(&self.routes);
        live.merge_daemon_dispatch_result(&base, working);
        if changed {
            let _ = live.save(self.config.routes_path.as_deref());
        }
        let winning_after_merge = live.get(recipient).map(|e| e.route.clone());
        drop(live);
        if let Some(winning) = winning_after_merge {
            // Passive discovery: vendor change alone satisfies the S2 gate.
            let gone = base.get(recipient).is_none();
            if self.should_release_fence_for(recipient, &winning, false, gone) {
                self.release_fence_for_new_route(
                    recipient,
                    RouteFenceRelease {
                        winning,
                        explicit: false,
                        route_gone_before: gone,
                        at: now,
                    },
                    None,
                );
            }
        }
        (updated, attempted, attempted_route)
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
        drop(pending);
        let should_clear_marker = {
            let mut state = lock(&self.awaiting_daemon_state);
            if let Some(entry) = state.get_mut(msg_id) {
                entry.unscheduled_recipients.remove(&key);
                if entry.unscheduled_recipients.is_empty() {
                    state.remove(msg_id);
                    true
                } else {
                    false
                }
            } else {
                false
            }
        };
        if should_clear_marker {
            let _ = self.store.clear_wire_awaiting_daemon(msg_id);
        }
    }

    fn pending_recipients_snapshot(&self, now: &At) -> Vec<RecipientPendingSummary> {
        let merged = lock(&self.pending_by_recipient).clone();
        let awaiting_by_msg: BTreeMap<MessageId, At> = lock(&self.awaiting_daemon_state)
            .iter()
            .filter_map(|(mid, entry)| {
                (!entry.unscheduled_recipients.is_empty())
                    .then(|| entry.queued_at.as_ref().map(|at| (mid.clone(), at.clone())))
                    .flatten()
            })
            .collect();
        summarize_pending_map_with_awaiting(&merged, &awaiting_by_msg, now)
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
            if queue.recovery.is_some() {
                lock(&self.dispatch_diagnostics).set_fence_recovery_attention(
                    recipient,
                    FenceRecoveryOutcome::WaitingForWorker,
                    false,
                );
            }
            return Ok(());
        }
        queue.waiting_for_capacity = false;
        queue.waiting_for_route = false;
        queue.composer_retry_at = None;
        queue.worker_active = true;
        if let Err(error) = self.spawn_recipient_worker(recipient.to_string()) {
            let recovery = queue.recovery.take();
            let mut row = evidence.clone();
            row.phase = SuppressionPhase::RetainedFence;
            row.first_reason = FenceReason::WorkerSpawnFailure;
            row.decisive_reason = FenceReason::WorkerSpawnFailure;
            if let Some(log) = self
                .retain_overflow_fence_at(
                    recipient,
                    row,
                    recovery.as_ref().and_then(|run| run.route.as_ref()),
                    false,
                )
                .0
            {
                first_holds.push(log);
            }
            queues.remove(recipient);
            if let Some(run) = recovery {
                self.finish_fence_recovery(
                    recipient,
                    &run,
                    FenceRecoveryOutcome::ReplayFailed,
                    false,
                );
            }
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
        #[cfg(test)]
        let endpoint = crate::runtime_context::pinned_endpoint();
        std::thread::Builder::new()
            .name("fl-wire-dispatch".into())
            .spawn(move || {
                #[cfg(test)]
                let _scope = endpoint.map(|endpoint| crate::runtime_context::pin(endpoint, None));
                owner.run_recipient_queue(recipient);
            })?;
        Ok(())
    }

    /// A departing worker releases its slot to a queued, capacity-waiting recipient.
    fn wake_capacity_waiters(self: &Arc<Self>) {
        self.wake_dispatch_waiters(None);
    }

    fn wake_dispatch_waiters(self: &Arc<Self>, now: Option<&At>) {
        if self.shutdown.load(Ordering::SeqCst) {
            return;
        }
        // Snapshot inputs only, then project without scheduler locks. Admission
        // below revalidates existence, pending work, worker state and capacity.
        let can_wake = |queue: &RecipientDispatchQueue| {
            !queue.pending.is_empty()
                && (queue.waiting_for_capacity
                    || now.is_some_and(|now| {
                        queue
                            .composer_retry_at
                            .as_ref()
                            .is_some_and(|retry_at| now.unix_millis() >= retry_at.unix_millis())
                    }))
        };
        let candidates: Vec<_> = lock(&self.dispatch_queues)
            .iter()
            .filter(|(_, queue)| can_wake(queue))
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
            if !queues.get(&name).is_some_and(can_wake) {
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
                    self.track_fence_message(&name, msg.id.clone());
                    if !queue.overflowed {
                        queue.overflowed = true;
                        let first_hold = if let Some(route) = &queue.released_route
                            && queue.recovery.is_none()
                        {
                            // The ordinary drain can still lose a receipt write. Hold
                            // ACK releases until it finishes, even if overflow began
                            // after the timed submit had already cleared the fence.
                            let (first_hold, since) =
                                self.retain_overflow_fence_at(&name, evidence.clone(), None, false);
                            if lock(&self.overflow_recipients).contains(&name) {
                                // Route capture is best effort while holding dispatch_queues.
                                // Preserve the captured identity or fall back to the released
                                // route so a later registration can clear this fence.
                                lock(&self.fence_routes)
                                    .entry(name.clone())
                                    .or_insert_with(|| route.route.clone());
                                lock(&self.fence_recoveries).insert(
                                    name.clone(),
                                    RetainedFenceRecovery::new(
                                        Some(route.clone()),
                                        since,
                                        RetainedFencePhase::OverflowOnly,
                                    ),
                                );
                            }
                            first_hold
                        } else {
                            if queue.recovery.is_none() {
                                self.record_fence_route(&name);
                            }
                            let _blocked = lock(&self.overflow_recipients);
                            let mut diagnostics = lock(&self.dispatch_diagnostics);
                            let mut row = evidence.clone();
                            row.hold_observed_at = now_at();
                            diagnostics.retain(row)
                        };
                        if let Some(log) = first_hold {
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
                if queue.recovery.is_some() {
                    self.track_fence_message(&name, msg.id.clone());
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
                self.track_fence_message(&name, msg.id.clone());
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
            let (
                message_id,
                enqueued_at,
                route_generation,
                recovery,
                released_route,
                composer_reprobes,
                composer_busy_since,
                redeliver,
            ) = {
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
                    let recovery = queue.recovery.take();
                    let overflowed = queue.overflowed;
                    let first_hold = if overflowed {
                        self.retain_overflow_fence_at(
                            &recipient,
                            empty_evidence,
                            recovery.as_ref().and_then(|run| run.route.as_ref()),
                            false,
                        )
                        .0
                    } else {
                        None
                    };
                    if let Some(run) = recovery {
                        self.finish_fence_recovery(
                            &recipient,
                            &run,
                            FenceRecoveryOutcome::Resolved,
                            overflowed,
                        );
                    }
                    queues.remove(&recipient);
                    drop(queues);
                    log_first_holds(first_hold.into_iter().collect());
                    self.wake_capacity_waiters();
                    return;
                };
                queue.in_flight = Some(id.clone());
                let redeliver =
                    queue.recovery.is_some() || queue.recovery_redeliveries.contains(&id);
                (
                    id,
                    now,
                    queue.route_generation,
                    queue.recovery.clone(),
                    queue.released_route.clone(),
                    queue.composer_reprobes,
                    queue.composer_busy_since.clone(),
                    redeliver,
                )
            };
            let mut report = DispatchReport::default();
            let mut failed_recovery = None;
            let message = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.store.get_wire_message(&message_id)
            }));
            let (mut outcome, attempted_route) = match message {
                Ok(Ok(Some(mut msg))) => {
                    if redeliver {
                        msg.body = format!("{TIMED_FENCE_REDELIVERY_NOTICE}\n\n{}", msg.body);
                    }
                    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let target = if let Some(run) = &recovery {
                            match self.probe_fence_recovery_route(&recipient, run.route.as_ref()) {
                                Ok((target, binding)) => {
                                    run.route.as_ref().map(|route| (route, target, binding))
                                }
                                Err(reason) => return Err(reason),
                            }
                        } else {
                            if let Some(route) = &released_route
                                && matches!(
                                    self.probe_fence_recovery_route(&recipient, Some(route)),
                                    Err(FenceRecoveryOutcome::SeatBusy)
                                )
                            {
                                return Ok((DispatchOutcome::ComposerBusy, None));
                            }
                            None
                        };
                        let result = self.dispatch_and_propagate(
                            &msg,
                            &recipient,
                            enqueued_at.clone(),
                            target,
                            &mut report,
                        )?;
                        if released_route.is_some()
                            && report.composer_busy
                            && report.route_dispatch_calls == 1
                            && report.omitted_failure_stages == 0
                            && report.failure_stages.iter().all(|stage| {
                                matches!(
                                    stage,
                                    FailureStage::ComposerOccupiedByNonWireInput
                                        | FailureStage::UnresolvedReceiptStatus
                                )
                            })
                            && matches!(result.0, DispatchOutcome::Unconfirmed)
                        {
                            return Ok((DispatchOutcome::ComposerBusy, result.1));
                        }
                        Ok(result)
                    })) {
                        Ok(Ok(outcome)) => outcome,
                        Ok(Err(reason)) => {
                            failed_recovery = Some(reason);
                            (DispatchOutcome::Unconfirmed, None)
                        }
                        Err(_) => {
                            report.record_failure(FailureStage::WorkerPanic);
                            (DispatchOutcome::Unconfirmed, None)
                        }
                    }
                }
                Ok(Ok(None)) => {
                    report.record_failure(FailureStage::MessageLookupMissing);
                    (DispatchOutcome::Unconfirmed, None)
                }
                Ok(Err(_)) => {
                    report.record_failure(FailureStage::MessageLookupError);
                    (DispatchOutcome::Unconfirmed, None)
                }
                Err(_) => {
                    report.record_failure(FailureStage::WorkerPanic);
                    (DispatchOutcome::Unconfirmed, None)
                }
            };
            if recovery.is_some() && matches!(outcome, DispatchOutcome::NoRoute) {
                outcome = DispatchOutcome::Unconfirmed;
                failed_recovery = Some(FenceRecoveryOutcome::RouteChanged);
            }
            let composer_exhausted = matches!(outcome, DispatchOutcome::ComposerBusy)
                && composer_reprobes >= COMPOSER_OCCUPIED_MAX_REPROBES;
            if composer_exhausted {
                report.record_failure(FailureStage::ComposerOccupiedByNonWireInput);
                outcome = DispatchOutcome::Unconfirmed;
            }
            let mut evidence = hold_evidence(
                &recipient,
                Some(&message_id),
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
            let mut parked_composer = false;
            let mut unresolved = false;
            let mut replay_stop = recovery.as_ref().and_then(|_| replay_stop_outcome(&report));
            let mut prefix_overflowed = false;
            let mut held_jobs = VecDeque::new();
            let is_composer_occupied_hold =
                evidence.first_reason == FenceReason::ComposerOccupiedByNonWireInput;
            let recognised_payload = evidence
                .recognised_composer_payload
                .as_deref()
                .and_then(crate::dispatcher::recognised_composer_payload);
            let job_at = enqueued_at.clone();
            let mut queues = lock(&self.dispatch_queues);
            if let Some(queue) = queues.get_mut(&recipient) {
                queue.in_flight = None;
                if recovery.is_none() && released_route.is_some() && queue.overflowed {
                    replay_stop = replay_stop_outcome(&report);
                    // An ACK can resolve the receipt while the push's receipt
                    // write is failing. The overflow fence still needs that fault.
                    if replay_stop.is_some() {
                        outcome = DispatchOutcome::Unconfirmed;
                    }
                }
                match outcome {
                    DispatchOutcome::RecoverySubmitted | DispatchOutcome::Unconfirmed => {
                        if matches!(outcome, DispatchOutcome::RecoverySubmitted)
                            && let Some(run) = queue.recovery.take()
                        {
                            queue.recovery_redeliveries.remove(&message_id);
                            queue.released_route = run.route.clone();
                            // Keep the admitted prefix in order while ownership passes
                            // from timed recovery to the regular worker.
                            let result = if queue.overflowed {
                                FenceRecoveryOutcome::PrefixOverflow
                            } else {
                                lock(&self.fence_routes).remove(&recipient);
                                lock(&self.overflow_recipients).remove(&recipient);
                                lock(&self.dispatch_diagnostics).clear_recipient(&recipient);
                                FenceRecoveryOutcome::Released
                            };
                            self.finish_fence_recovery(&recipient, &run, result, false);
                        } else {
                            if matches!(outcome, DispatchOutcome::RecoverySubmitted) {
                                replay_stop = Some(FenceRecoveryOutcome::ReplayWorkerFailed);
                                evidence.first_reason = FenceReason::ReplayStateUnavailable;
                                evidence.decisive_reason = FenceReason::ReplayStateUnavailable;
                                outcome = DispatchOutcome::Unconfirmed;
                            }
                            // Never let a later message overtake an unconfirmed push.
                            held_jobs = std::mem::take(&mut queue.pending);
                            prefix_overflowed = queue.overflowed;
                            queue.overflowed = true;
                            queue.worker_active = false;
                            unresolved = true;
                        }
                    }
                    DispatchOutcome::Resolved => {
                        queue.recovery_redeliveries.remove(&message_id);
                        queue.composer_reprobes = 0;
                        queue.composer_busy_since = None;
                        queue.composer_backoff = Duration::ZERO;
                        if let Some(run) = &recovery {
                            let mut recoveries = lock(&self.fence_recoveries);
                            if let Some(recovery) = recoveries.get_mut(&recipient)
                                && run.is_current(recovery)
                            {
                                recovery.pending.retain(|id| id != &message_id);
                            }
                        }
                    }
                    DispatchOutcome::ComposerBusy => {
                        // A turn started by the confirmed replay can occupy the composer.
                        // Keep the oldest job queued without spending another recovery attempt.
                        queue.pending.push_front((message_id.clone(), enqueued_at));
                        queue.composer_reprobes += 1;
                        queue.composer_backoff = (queue.composer_backoff * 2).clamp(
                            COMPOSER_OCCUPIED_REPROBE_INITIAL_BACKOFF,
                            COMPOSER_OCCUPIED_REPROBE_MAX_BACKOFF,
                        );
                        let park_at = now_at();
                        queue.composer_retry_at = Some(At::from_unix_millis(
                            park_at
                                .unix_millis()
                                .saturating_add(queue.composer_backoff.as_millis() as u64),
                        ));
                        queue.composer_busy_since.get_or_insert(park_at);
                        queue.worker_active = false;
                        parked_composer = true;
                    }
                    DispatchOutcome::NoRoute => {
                        if queue.route_generation == route_generation {
                            if queue.overflowed || queue.released_route.is_some() {
                                // The accepted prefix must remain ordered and wakeable; only
                                // messages beyond the bound stay exclusively in the inbox.
                                queue.pending.push_front((message_id.clone(), enqueued_at));
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
                            queue.pending.push_front((message_id.clone(), enqueued_at));
                        }
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
                // A prefix overflow has already left untracked messages in the inbox.
                // A long hosted turn can exhaust the ordinary parks without
                // any push call. Reuse the exact registration released by the
                // preceding timed submit so the next probe cannot switch routes.
                let exhausted_busy_route = released_route.as_ref().filter(|_| composer_exhausted);
                let retryable = replay_stop.is_none()
                    && !prefix_overflowed
                    && (exhausted_busy_route.is_some() || transport_fence_can_recover(&evidence));
                // The normal drain may have switched routes after the timed submit.
                // A deferred registration for the route that received this push
                // must not release a stopped fence.
                let fenced_route = recovery
                    .as_ref()
                    .and_then(|run| run.route.as_ref())
                    .or(exhausted_busy_route)
                    .or(attempted_route.as_ref().filter(|_| replay_stop.is_some()));
                let since = self.retain_overflow_fence_at(
                    &recipient,
                    evidence,
                    fenced_route,
                    replay_stop.is_some(),
                );
                first_hold = since.0;
                if let Some(run) = &recovery {
                    let mut recoveries = lock(&self.fence_recoveries);
                    if let Some(recovery) = recoveries.get_mut(&recipient)
                        && run.is_current(recovery)
                        && replay_stop.is_none()
                    {
                        recovery.hold(message_id.clone());
                        for (id, _) in held_jobs {
                            recovery.hold(id);
                        }
                    }
                    drop(recoveries);
                    self.finish_fence_recovery(
                        &recipient,
                        run,
                        replay_stop
                            .or(failed_recovery)
                            .unwrap_or(FenceRecoveryOutcome::ReplayFailed),
                        false,
                    );
                } else if let Some(reason) = replay_stop {
                    if let Some(state) = lock(&self.fence_recoveries).get_mut(&recipient) {
                        state.phase = RetainedFencePhase::ReplayStopped;
                        state.pending.clear();
                        lock(&self.dispatch_diagnostics)
                            .set_fence_recovery_attention(&recipient, reason, false);
                    }
                } else if retryable
                    && normalize_persona(&recipient).as_deref() == Some(recipient.as_str())
                    && lock(&self.overflow_recipients).contains(&recipient)
                {
                    let mut recoveries = lock(&self.fence_recoveries);
                    let recovery = recoveries.entry(recipient.clone()).or_insert_with(|| {
                        let mut state = RetainedFenceRecovery::new(
                            exhausted_busy_route.cloned().or(attempted_route),
                            since.1,
                            RetainedFencePhase::Waiting,
                        );
                        let mut diagnostics = lock(&self.dispatch_diagnostics);
                        if self.fence_recovery_policy.max_attempts == 0 {
                            diagnostics.set_fence_recovery_attention(
                                &recipient,
                                FenceRecoveryOutcome::RecoveryDisabled,
                                false,
                            );
                        } else if exhausted_busy_route.is_some() {
                            let busy_since =
                                composer_busy_since.unwrap_or_else(|| state.since.clone());
                            state.busy_since = Some(busy_since.clone());
                            diagnostics.set_busy_fence_recovery_status(
                                &recipient,
                                FenceRecoveryOutcome::SeatBusy,
                                busy_since,
                            );
                        } else {
                            diagnostics.set_fence_recovery_attention(
                                &recipient,
                                FenceRecoveryOutcome::WaitingForDelay,
                                false,
                            );
                        }
                        state
                    });
                    recovery.hold(message_id.clone());
                    for (id, _) in held_jobs {
                        recovery.hold(id);
                    }
                }
                queues.remove(&recipient);
            }
            drop(queues);
            if matches!(
                outcome,
                DispatchOutcome::Resolved | DispatchOutcome::RecoverySubmitted
            ) {
                lock(&self.composer_occupied_reprobes).remove(&recipient);
            }
            log_first_holds(first_hold.into_iter().collect());
            if evicted_no_route || parked_no_route || parked_composer {
                self.wake_capacity_waiters();
                return;
            }
            if unresolved {
                if is_composer_occupied_hold
                    && recovery.is_none()
                    && replay_stop.is_none()
                    && !composer_exhausted
                {
                    self.schedule_composer_occupied_reprobe(
                        &recipient,
                        &job_at,
                        recognised_payload,
                    );
                }
                warn(&format!(
                    "delivery unresolved for message {}; later messages for {} remain pending",
                    message_id,
                    sanitize_for_warning(&recipient)
                ));
                self.wake_capacity_waiters();
                return;
            }
        }
    }

    /// The caller holds `dispatch_queues`; this matches `schedule_dispatch` lock order.
    fn record_fence_route(&self, recipient: &str) {
        let norm =
            normalize_persona(recipient).unwrap_or_else(|| recipient.trim().to_ascii_lowercase());
        // Registration may already hold `routes` on this thread; recording then would
        // deadlock and would capture the winning route instead of the fenced one.
        let Ok(routes) = self.routes.try_lock() else {
            return;
        };
        if let Some(entry) = routes.get(&norm) {
            lock(&self.fence_routes).insert(norm, entry.route.clone());
        }
    }

    fn retain_overflow_fence(
        &self,
        recipient: &str,
        row: FenceDiagnostic,
    ) -> Option<FenceDiagnostic> {
        self.retain_overflow_fence_at(recipient, row, None, false).0
    }

    fn retain_overflow_fence_at(
        &self,
        recipient: &str,
        mut row: FenceDiagnostic,
        fenced_route: Option<&RouteEntry>,
        replace_evidence: bool,
    ) -> (Option<FenceDiagnostic>, At) {
        if let Some(entry) = fenced_route {
            lock(&self.fence_routes).insert(recipient.to_string(), entry.route.clone());
        } else if !lock(&self.fence_recoveries)
            .get(recipient)
            .is_some_and(|state| state.phase == RetainedFencePhase::OverflowOnly)
        {
            // A timed submit that left an overflow fence recorded its old route.
            // Keep it available for a deferred winning registration during prefix drain.
            self.record_fence_route(recipient);
        }
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
        let retained_at = row.hold_observed_at.clone();
        (
            diagnostics.retain_with_replacement(row, replace_evidence),
            retained_at,
        )
    }

    fn track_fence_message(&self, recipient: &str, id: MessageId) {
        if let Some(recovery) = lock(&self.fence_recoveries).get_mut(recipient)
            && matches!(
                recovery.phase,
                RetainedFencePhase::Waiting | RetainedFencePhase::InProgress
            )
        {
            if recovery.phase == RetainedFencePhase::Waiting
                && recovery.pending.is_empty()
                && recovery.attempts < self.fence_recovery_policy.max_attempts
            {
                lock(&self.dispatch_diagnostics).set_fence_recovery_attention(
                    recipient,
                    FenceRecoveryOutcome::WaitingForDelay,
                    false,
                );
            }
            recovery.hold(id);
        }
    }

    fn is_recipient_fenced(&self, recipient: &str) -> bool {
        let queues = lock(&self.dispatch_queues);
        self.has_recipient_fence(recipient, queues.get(recipient))
    }

    fn has_recipient_fence(&self, recipient: &str, queue: Option<&RecipientDispatchQueue>) -> bool {
        lock(&self.overflow_recipients).contains(recipient)
            || queue.is_some_and(|queue| queue.overflowed && queue.recovery.is_none())
    }

    fn clear_recovery_fence(&self, recipient: &str) {
        lock(&self.fence_recoveries).remove(recipient);
    }

    fn finish_fence_recovery(
        &self,
        recipient: &str,
        run: &FenceRecoveryRun,
        outcome: FenceRecoveryOutcome,
        resolved_prefix_overflowed: bool,
    ) {
        let mut recoveries = lock(&self.fence_recoveries);
        let mut current = recoveries
            .get_mut(recipient)
            .filter(|state| run.is_current(state));
        if let Some(state) = current.as_mut()
            && outcome != FenceRecoveryOutcome::SeatBusy
        {
            state.busy_since = None;
            state.consecutive_busy_probes = 0;
        }
        let mut diagnostics = lock(&self.dispatch_diagnostics);
        let mut busy_probe_count = None;
        let outcome = match current {
            Some(state) if outcome == FenceRecoveryOutcome::SeatBusy => {
                // A typed busy refusal proves zero writes. Keep the delay
                // anchored to this poll but return its reserved attempt.
                state.attempts = state.attempts.saturating_sub(1);
                state.phase = RetainedFencePhase::Waiting;
                state.consecutive_busy_probes = state.consecutive_busy_probes.saturating_add(1);
                busy_probe_count = Some(state.consecutive_busy_probes);
                let busy_since = state.busy_since.get_or_insert_with(|| run.at.clone());
                let status = if state.consecutive_busy_probes >= FENCE_BUSY_ATTENTION_PROBES {
                    FenceRecoveryOutcome::SeatBusyProlonged
                } else {
                    FenceRecoveryOutcome::SeatBusy
                };
                diagnostics.set_busy_fence_recovery_status(recipient, status, busy_since.clone());
                outcome
            }
            Some(_) if outcome == FenceRecoveryOutcome::Released => {
                recoveries.remove(recipient);
                diagnostics.clear_fence_recovery_attention(recipient);
                outcome
            }
            Some(state) if outcome == FenceRecoveryOutcome::PrefixOverflow => {
                state.phase = RetainedFencePhase::OverflowOnly;
                state.pending.clear();
                diagnostics.set_fence_recovery_attention(recipient, outcome, false);
                outcome
            }
            Some(state) if outcome == FenceRecoveryOutcome::Resolved => {
                state.attempts = state.attempts.saturating_sub(1);
                if resolved_prefix_overflowed {
                    state.phase = RetainedFencePhase::OverflowOnly;
                    state.pending.clear();
                    diagnostics.set_fence_recovery_attention(
                        recipient,
                        FenceRecoveryOutcome::PrefixOverflow,
                        false,
                    );
                } else {
                    state.phase = RetainedFencePhase::Waiting;
                    if state.pending.is_empty() {
                        diagnostics.clear_fence_recovery_attention(recipient);
                    } else {
                        diagnostics.set_fence_recovery_attention(
                            recipient,
                            FenceRecoveryOutcome::WaitingForDelay,
                            false,
                        );
                    }
                }
                outcome
            }
            Some(state)
                if matches!(
                    outcome,
                    FenceRecoveryOutcome::ReceiptBookkeepingFailed
                        | FenceRecoveryOutcome::ReplayWorkerFailed
                ) =>
            {
                state.phase = RetainedFencePhase::ReplayStopped;
                state.pending.clear();
                diagnostics.set_fence_recovery_attention(recipient, outcome, false);
                outcome
            }
            Some(state) => {
                state.phase = RetainedFencePhase::Waiting;
                diagnostics.set_fence_recovery_attention(
                    recipient,
                    outcome,
                    run.attempt >= self.fence_recovery_policy.max_attempts,
                );
                outcome
            }
            None => FenceRecoveryOutcome::FenceSuperseded,
        };
        let attempt = FenceRecoveryAttempt {
            recipient: recipient.to_string(),
            attempt: run.attempt,
            limit: self.fence_recovery_policy.max_attempts,
            at: run.at.clone(),
            outcome,
            busy_probe_count,
        };
        diagnostics.record_fence_recovery(attempt);
    }

    fn recovery_route_is_current(&self, recipient: &str, entry: &RouteEntry) -> bool {
        lock(&self.routes)
            .get(recipient)
            .is_some_and(|current| entry.same_registration_identity(current))
    }

    fn probe_fence_recovery_route(
        &self,
        recipient: &str,
        route: Option<&RouteEntry>,
    ) -> Result<(RecoveryTarget, Option<RecoveryHostBinding>), FenceRecoveryOutcome> {
        let Some(route) = route else {
            return Err(FenceRecoveryOutcome::DeadRoute);
        };
        {
            let routes = lock(&self.routes);
            let current = routes
                .get(recipient)
                .ok_or(FenceRecoveryOutcome::DeadRoute)?;
            if !route.same_registration_identity(current) {
                return Err(FenceRecoveryOutcome::RouteChanged);
            }
        }
        #[cfg(test)]
        if let Some(hook) = lock(&self.fence_recovery_probe_hook).as_mut() {
            return readiness_result(hook(), true).map(|target| (target, None));
        }
        if !matches!(route.route.canonical_vendor(), "claude" | "codex" | "muse") {
            return Err(FenceRecoveryOutcome::UnsupportedRoute);
        }
        let binding = (self.recovery_host_lookup)(self.store.as_ref(), recipient, route)?
            .ok_or(FenceRecoveryOutcome::NonHostedRoute)?;
        let readiness = probe_retained_fence_readiness(
            route,
            recipient,
            Some(&binding.session_id),
            self.config.home_override.as_deref(),
            binding.runtime_leader_pid,
        );
        readiness_result(readiness, binding.startup_confirmed).map(|target| (target, Some(binding)))
    }

    /// The caller holds the enqueue gate and queue lock. Keep manual releases until
    /// the timed replay ends. An ACK waits for the whole overflow prefix, while a
    /// route change waits for an in-flight prefix job's receipt result.
    fn defer_fence_release(
        &self,
        recipient: &str,
        queue: Option<&RecipientDispatchQueue>,
        wait_for_overflow_prefix: bool,
        update: impl FnOnce(&mut DeferredFenceRelease),
    ) -> bool {
        let Some(queue) = queue else {
            return false;
        };
        let wait_for_overflow = wait_for_overflow_prefix || queue.in_flight.is_some();
        let mut recoveries = lock(&self.fence_recoveries);
        let Some(state) = recoveries.get_mut(recipient).filter(|state| {
            queue
                .recovery
                .as_ref()
                .is_some_and(|run| run.is_current(state))
                || (wait_for_overflow && state.phase == RetainedFencePhase::OverflowOnly)
        }) else {
            // A saturated fence map cannot retain a release. It still must not
            // release a post-replay overflow while the prefix is running.
            return wait_for_overflow && queue.overflowed && queue.released_route.is_some();
        };
        update(&mut state.deferred_release);
        true
    }

    fn recovery_fence_matches(&self, recipient: &str, expected: Option<&Arc<()>>) -> bool {
        expected.is_none_or(|expected| {
            lock(&self.fence_recoveries)
                .get(recipient)
                .is_some_and(|state| Arc::ptr_eq(expected, &state.token))
        })
    }

    fn release_deferred_fences(self: &Arc<Self>) {
        let ready: Vec<_> = {
            let queues = lock(&self.dispatch_queues);
            lock(&self.fence_recoveries)
                .iter_mut()
                .filter(|(_, state)| state.phase != RetainedFencePhase::InProgress)
                .filter_map(|(recipient, state)| {
                    // A replacement can release the fence while an ordinary tail is
                    // parked. Keep the ACK until that prefix no longer owns the queue.
                    let ack = if state.phase == RetainedFencePhase::OverflowOnly
                        && queues.contains_key(recipient)
                    {
                        None
                    } else {
                        state.deferred_release.ack.take()
                    };
                    let route = state.deferred_release.route.take();
                    (route.is_some() || ack.is_some())
                        .then(|| (recipient.clone(), Arc::clone(&state.token), route, ack))
                })
                .collect()
        };
        for (recipient, token, route, ack) in ready {
            if let Some(route) = route {
                self.release_fence_for_new_route(&recipient, route, Some(&token));
            }
            if let Some(at) = ack {
                self.unfence_after_ack(&recipient, at, Some(&token));
            }
        }
    }

    fn poll_retained_fences(self: &Arc<Self>, now: At) {
        self.release_deferred_fences();
        self.wake_dispatch_waiters(Some(&now));
        let policy = self.fence_recovery_policy;
        let now_ms = now.unix_millis();
        let due: Vec<_> = {
            let mut recoveries = lock(&self.fence_recoveries);
            let mut eligible: Vec<_> = recoveries
                .iter_mut()
                .filter_map(|(name, state)| {
                    if state.phase != RetainedFencePhase::Waiting
                        || state.pending.is_empty()
                        || state.attempts >= policy.max_attempts
                    {
                        return None;
                    }
                    let eligible_at = state
                        .last_attempt
                        .as_ref()
                        .unwrap_or(&state.since)
                        .unix_millis()
                        .saturating_add(policy.delay_ms);
                    (now_ms >= eligible_at).then_some((eligible_at, name, state))
                })
                .collect();
            // Read-only busy probes can retry indefinitely. Serve the oldest
            // eligibility time first, keeping recipient order for equal times.
            eligible.sort_by_key(|(eligible_at, _, _)| *eligible_at);
            eligible
                .into_iter()
                .take(MAX_FENCE_PROBES_PER_POLL)
                .map(|(_, name, state)| {
                    state.phase = RetainedFencePhase::InProgress;
                    state.attempts += 1;
                    state.last_attempt = Some(now.clone());
                    (
                        name.clone(),
                        FenceRecoveryRun {
                            token: Arc::clone(&state.token),
                            route: state.route.clone(),
                            attempt: state.attempts,
                            at: now.clone(),
                        },
                    )
                })
                .collect()
        };
        for (recipient, run) in due {
            let result = self
                .probe_fence_recovery_route(&recipient, run.route.as_ref())
                .and_then(|_| self.resume_retained_fence(&recipient, &run));
            if let Err(reason) = result {
                self.finish_fence_recovery(&recipient, &run, reason, false);
            }
        }
    }

    fn resume_retained_fence(
        self: &Arc<Self>,
        recipient: &str,
        run: &FenceRecoveryRun,
    ) -> Result<(), FenceRecoveryOutcome> {
        use FenceRecoveryOutcome::{InboxUnavailable, ReplayFailed, Resolved, RouteChanged};

        let _order = lock(&self.enqueue_gate);
        let unread = self
            .store
            .list_wire_inbox(&WireFilter {
                recipient: Some(recipient.to_string()),
                unread_only: true,
                ..Default::default()
            })
            .map_err(|_| InboxUnavailable)?;
        // Match route registration's lock order. No I/O while routes and queues
        // are held together, and the worker revalidates its pin before each send.
        let routes = lock(&self.routes);
        if !run.route.as_ref().is_some_and(|expected| {
            routes
                .get(recipient)
                .is_some_and(|current| expected.same_registration_identity(current))
        }) {
            return Err(RouteChanged);
        }
        let mut queues = lock(&self.dispatch_queues);
        if queues.contains_key(recipient) || !lock(&self.overflow_recipients).contains(recipient) {
            return Err(RouteChanged);
        }
        if queues.len() >= MAX_TRACKED_RECIPIENTS
            || self.overflow_capacity_exhausted.load(Ordering::SeqCst)
        {
            return Err(ReplayFailed);
        }
        let mut recoveries = lock(&self.fence_recoveries);
        let state = recoveries
            .get_mut(recipient)
            .filter(|state| run.is_current(state))
            .ok_or(RouteChanged)?;
        if state.pending_capacity_exhausted {
            return Err(ReplayFailed);
        }
        let mut queue = RecipientDispatchQueue {
            recovery: Some(run.clone()),
            ..Default::default()
        };
        let pending_ids: BTreeSet<_> = state.pending.iter().collect();
        let unread_pending: BTreeMap<_, _> = unread
            .into_iter()
            .filter(|item| {
                item.receipt.status == DeliveryStatus::Pending
                    && pending_ids.contains(&item.message.id)
            })
            .map(|item| (item.message.id, item.receipt.attempts))
            .collect();
        for id in &state.pending {
            let Some(&attempts) = unread_pending.get(id) else {
                continue;
            };
            if attempts >= crate::dispatcher::MAX_DELIVER {
                // Do not let later messages bypass a held receipt that cannot be attempted.
                return Err(ReplayFailed);
            }
            if attempts > 0 {
                queue.recovery_redeliveries.insert(id.clone());
            }
            queue.pending.push_back((id.clone(), run.at.clone()));
        }
        state.pending.retain(|id| unread_pending.contains_key(id));
        if queue.pending.is_empty() {
            return Err(Resolved);
        }
        drop(recoveries);
        drop(routes);

        let evidence = hold_evidence(
            recipient,
            queue.pending.front().map(|(id, _)| id),
            SuppressionPhase::RetainedFence,
            FenceReason::WorkerSpawnFailure,
            &DispatchReport::default(),
        );
        lock(&self.composer_occupied_reprobes).remove(recipient);
        // Preserve the original fence identity while the timed worker owns the queue,
        // so a concurrent winning registration can still satisfy the normal release gate.
        if let Some(entry) = &run.route {
            lock(&self.fence_routes).insert(recipient.to_string(), entry.route.clone());
        }
        // A replay worker still owns the fence's overflow slot in case the
        // worker has to retain the fence again. Only a confirmed submit releases it.
        queues.insert(recipient.to_string(), queue);
        let mut first_holds = Vec::new();
        if let Err(error) =
            self.start_queued_worker(&mut queues, recipient, &evidence, &mut first_holds)
        {
            // The worker-admission path has already restored and recorded the fence.
            warn(&format!("{}: {error}", sanitize_for_warning(recipient)));
        }
        drop(queues);
        log_first_holds(first_holds);
        Ok(())
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

    /// Release a retained fence because a winning registration replaced the route
    /// identity recorded on the fence (remodel/vendor change). Redelivery after a
    /// release is at-least-once: a message with `attempts > 0` may already have been
    /// received, so it is redelivered with a notice line before the body.
    fn release_fence_for_new_route(
        self: &Arc<Self>,
        recipient: &str,
        release: RouteFenceRelease,
        expected: Option<&Arc<()>>,
    ) {
        let name =
            normalize_persona(recipient).unwrap_or_else(|| recipient.trim().to_ascii_lowercase());
        if name.is_empty() {
            return;
        }
        let _order = lock(&self.enqueue_gate);
        let still_winning = |routes: &RouteTable| {
            routes
                .get(&name)
                .is_some_and(|current| current.route == release.winning)
        };
        let can_release = |queue: Option<&RecipientDispatchQueue>| {
            self.recovery_fence_matches(&name, expected)
                && self.should_release_fence_for(
                    &name,
                    &release.winning,
                    release.explicit,
                    release.route_gone_before,
                )
                && !self.defer_fence_release(&name, queue, false, |deferred| {
                    deferred.route = Some(release.clone());
                })
                && self.has_recipient_fence(&name, queue)
        };
        if !still_winning(&lock(&self.routes)) {
            return;
        }
        if !can_release(lock(&self.dispatch_queues).get(&name)) {
            return;
        }
        // Do not hold scheduler locks across the store read. Recheck before opening
        // the fence because the ordinary drain may have recorded a receipt fault.
        let Ok(unread) = self.store.list_wire_inbox(&WireFilter {
            recipient: Some(name.clone()),
            unread_only: true,
            ..Default::default()
        }) else {
            return;
        };
        {
            let routes = lock(&self.routes);
            let mut queues = lock(&self.dispatch_queues);
            if !still_winning(&routes) || !can_release(queues.get(&name)) {
                return;
            }
            lock(&self.composer_occupied_reprobes).remove(&name);
            lock(&self.fence_routes).remove(&name);
            self.clear_recovery_fence(&name);
            lock(&self.dispatch_diagnostics).clear_recipient(&name);
            lock(&self.overflow_recipients).remove(&name);
            if let Some(queue) = queues.get_mut(&name) {
                queue.overflowed = false;
            }
        }
        warn(&format!(
            "released retained fence for {}: route identity replaced by remodel/vendor change; rescheduling pending delivery to the new route (at-least-once)",
            sanitize_for_warning(&name)
        ));
        let mut first_holds = Vec::new();
        let at = release.at;
        for item in &unread {
            if item.receipt.status == DeliveryStatus::Pending {
                if item.receipt.attempts > 0 {
                    let mut msg = item.message.clone();
                    msg.body = format!(
                        "Note: this is a redelivery after a session change; an earlier delivery attempt was unconfirmed and may already have been received.\n\n{}",
                        msg.body
                    );
                    if let Err(message) = self.schedule_dispatch(
                        &msg,
                        std::slice::from_ref(&item.receipt),
                        at.clone(),
                        &mut first_holds,
                    ) {
                        warn(&message);
                    }
                } else if let Err(message) = self.schedule_dispatch(
                    &item.message,
                    std::slice::from_ref(&item.receipt),
                    at.clone(),
                    &mut first_holds,
                ) {
                    warn(&message);
                }
            }
        }
        drop(_order);
        log_first_holds(first_holds);
    }

    /// S2 gate: release only when the winning identity differs from the fence-recorded
    /// identity AND (explicit OR vendor changed OR fenced route gone).
    fn should_release_fence_for(
        &self,
        norm: &str,
        winning: &crate::identity::VendorRoute,
        explicit: bool,
        route_gone_before: bool,
    ) -> bool {
        let recorded = lock(&self.fence_routes).get(norm).cloned();
        let Some(old) = recorded else {
            // No recorded identity: the S2 gate requires a recorded identity that
            // differs, so there is nothing to prove a release from.
            return false;
        };
        if !route_identity_changed(&old, winning) {
            return false;
        }
        explicit || route_gone_before || old.canonical_vendor() != winning.canonical_vendor()
    }

    /// Clear `overflow_recipients` and resume queued/pending push delivery for `recipient`
    /// once all unconfirmed (`Pending` with `attempts > 0`) receipts have been acknowledged.
    fn maybe_unfence_and_resume(self: &Arc<Self>, recipient: &str, now: At) {
        self.unfence_after_ack(recipient, now, None);
    }

    fn unfence_after_ack(self: &Arc<Self>, recipient: &str, now: At, expected: Option<&Arc<()>>) {
        let name =
            normalize_persona(recipient).unwrap_or_else(|| recipient.trim().to_ascii_lowercase());
        if name.is_empty() {
            return;
        }
        let _order = lock(&self.enqueue_gate);
        if !self.recovery_fence_matches(&name, expected)
            || {
                let queues = lock(&self.dispatch_queues);
                self.defer_fence_release(&name, queues.get(&name), true, |deferred| {
                    deferred.ack = Some(now.clone());
                })
            }
            || !self.is_recipient_fenced(&name)
            || lock(&self.fence_recoveries)
                .get(&name)
                .is_some_and(|state| state.phase == RetainedFencePhase::ReplayStopped)
        {
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
                    ) || r
                        .failure_stages
                        .iter()
                        .any(|stage| !is_transport_failure(*stage))
                        || r.route_observations
                            .iter()
                            .any(|o| o.outcome == DispatchCallOutcome::ReturnedOk))
            }) {
                return;
            }
        }
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
        self.clear_recovery_fence(&name);
        lock(&self.dispatch_diagnostics).clear_recipient(&name);
        lock(&self.fence_routes).remove(&name);
        lock(&self.overflow_recipients).remove(&name);
        if let Some(queue) = lock(&self.dispatch_queues).get_mut(&name) {
            queue.overflowed = false;
        }
        let mut has_pending = false;
        for item in &unread {
            if item.receipt.status == DeliveryStatus::Pending {
                has_pending = true;
                if let Err(message) = self.schedule_dispatch(
                    &item.message,
                    std::slice::from_ref(&item.receipt),
                    now.clone(),
                    &mut first_holds,
                ) {
                    warn(&message);
                }
            }
        }
        if !has_pending {
            lock(&self.composer_occupied_reprobes).remove(&name);
        }
        drop(_order);
        log_first_holds(first_holds);
    }

    fn recipient_has_zero_attempt_pending_only(&self, recipient: &str) -> bool {
        let Ok(unread) = self.store.list_wire_inbox(&WireFilter {
            recipient: Some(recipient.to_string()),
            unread_only: true,
            ..Default::default()
        }) else {
            return false;
        };
        let mut any_pending = false;
        for item in unread {
            if item.receipt.status == DeliveryStatus::Pending {
                if item.receipt.attempts > 0 {
                    return false;
                }
                any_pending = true;
            }
        }
        any_pending
    }

    fn schedule_composer_occupied_reprobe(
        &self,
        recipient: &str,
        observed_at: &At,
        recognised_payload: Option<&'static str>,
    ) {
        if !self.recipient_has_zero_attempt_pending_only(recipient) {
            lock(&self.composer_occupied_reprobes).remove(recipient);
            return;
        }
        let now_ms = observed_at.unix_millis();
        let mut reprobes = lock(&self.composer_occupied_reprobes);
        reprobes
            .entry(recipient.to_string())
            .and_modify(|entry| {
                // Every new hold overwrites the recognised payload: an
                // unrecognised hold clears a stale "/clear" from an earlier hold.
                entry.recognised_payload = recognised_payload;
                // Never reset an exhausted entry here: this is also called by the hold a
                // reprobe's own dispatch produces, so resetting would defeat the bound.
                // Only a new message, read/ack (see `reset_composer_occupied_reprobe`), or
                // daemon restart re-arms it.
                if entry.probes_run < COMPOSER_OCCUPIED_MAX_REPROBES {
                    entry.next_probe_at = At::from_unix_millis(
                        now_ms.saturating_add(entry.backoff.as_millis() as u64),
                    );
                }
            })
            .or_insert_with(|| {
                let backoff = COMPOSER_OCCUPIED_REPROBE_INITIAL_BACKOFF;
                ComposerOccupiedReprobeEntry {
                    next_probe_at: At::from_unix_millis(
                        now_ms.saturating_add(backoff.as_millis() as u64),
                    ),
                    backoff,
                    probes_run: 0,
                    recognised_payload,
                }
            });
    }

    /// Re-arm an exhausted reprobe entry; called only on a new message or a read/ack.
    fn reset_composer_occupied_reprobe(&self, recipient: &str, now: &At) {
        let name = recipient.trim().to_ascii_lowercase();
        let mut reprobes = lock(&self.composer_occupied_reprobes);
        if let Some(entry) = reprobes.get_mut(&name) {
            entry.probes_run = 0;
            // Keep `recognised_payload`: a reset re-arms probing but must not erase
            // what the composer was seen holding; the next real hold overwrites it.
            entry.backoff = COMPOSER_OCCUPIED_REPROBE_INITIAL_BACKOFF;
            entry.next_probe_at = At::from_unix_millis(
                now.unix_millis()
                    .saturating_add(COMPOSER_OCCUPIED_REPROBE_INITIAL_BACKOFF.as_millis() as u64),
            );
        }
    }

    /// Re-probe recipients held by a zero-key `ComposerOccupiedByNonWireInput` fence on a bounded
    /// exponential backoff schedule (`10s -> 20s -> 40s -> 80s -> 120s`, up to
    /// [`COMPOSER_OCCUPIED_MAX_REPROBES`]). Unconfirmed pastes (`attempts > 0`) are never
    /// auto-retried here and continue to wait for an explicit `fl wire ack` / `fl wire read --ack`.
    pub(crate) fn poll_composer_occupied_holds(self: &Arc<Self>, now: At) {
        let now_ms = now.unix_millis();
        let due: Vec<String> = lock(&self.composer_occupied_reprobes)
            .iter()
            .filter(|(_, entry)| {
                entry.probes_run < COMPOSER_OCCUPIED_MAX_REPROBES
                    && now_ms >= entry.next_probe_at.unix_millis()
            })
            .map(|(recipient, _)| recipient.clone())
            .collect();
        for recipient in due {
            if !lock(&self.overflow_recipients).contains(&recipient) {
                lock(&self.composer_occupied_reprobes).remove(&recipient);
                continue;
            }
            if !self.recipient_has_zero_attempt_pending_only(&recipient) {
                lock(&self.composer_occupied_reprobes).remove(&recipient);
                continue;
            }
            let route = lock(&self.routes).get(&recipient).map(|e| e.route.clone());
            let Some(route) = route else {
                // A missing route still consumes probe budget and is logged, so polling
                // stays bounded instead of retrying silently forever.
                let counted = {
                    let mut reprobes = lock(&self.composer_occupied_reprobes);
                    reprobes.get_mut(&recipient).map(|entry| {
                        entry.probes_run = entry.probes_run.saturating_add(1);
                        entry.backoff =
                            (entry.backoff * 2).min(COMPOSER_OCCUPIED_REPROBE_MAX_BACKOFF);
                        entry.next_probe_at = At::from_unix_millis(
                            now_ms.saturating_add(entry.backoff.as_millis() as u64),
                        );
                        (entry.probes_run, entry.backoff, entry.recognised_payload)
                    })
                };
                if let Some((probes_run, backoff, payload)) = counted {
                    warn(&format!(
                        "composer-occupied reprobe {probes_run}/{COMPOSER_OCCUPIED_MAX_REPROBES} for {recipient}: ready=None (no route), next backoff {backoff:?}"
                    ));
                    if probes_run >= COMPOSER_OCCUPIED_MAX_REPROBES {
                        warn(&format!(
                            "composer-occupied reprobes exhausted for {recipient} after {COMPOSER_OCCUPIED_MAX_REPROBES} probes; holding until new message, read/ack, or daemon restart{}",
                            recognised_payload_suffix(payload)
                        ));
                    }
                }
                continue;
            };
            let ready = {
                #[cfg(test)]
                {
                    let mut hook = lock(&self.composer_probe_hook);
                    if let Some(ref mut f) = *hook {
                        f(&recipient, &route)
                    } else if tests::has_test_transport() {
                        None
                    } else {
                        crate::dispatcher::probe_route_composer_ready(&route)
                    }
                }
                #[cfg(not(test))]
                {
                    crate::dispatcher::probe_route_composer_ready(&route)
                }
            };
            let (probes_run, backoff, payload) = {
                let mut reprobes = lock(&self.composer_occupied_reprobes);
                if let Some(entry) = reprobes.get_mut(&recipient) {
                    entry.probes_run = entry.probes_run.saturating_add(1);
                    entry.backoff = (entry.backoff * 2).min(COMPOSER_OCCUPIED_REPROBE_MAX_BACKOFF);
                    entry.next_probe_at = At::from_unix_millis(
                        now_ms.saturating_add(entry.backoff.as_millis() as u64),
                    );
                    (entry.probes_run, entry.backoff, entry.recognised_payload)
                } else {
                    continue;
                }
            };
            warn(&format!(
                "composer-occupied reprobe {probes_run}/{COMPOSER_OCCUPIED_MAX_REPROBES} for {recipient}: ready={ready:?}, next backoff {backoff:?}"
            ));
            if probes_run >= COMPOSER_OCCUPIED_MAX_REPROBES {
                warn(&format!(
                    "composer-occupied reprobes exhausted for {recipient} after {COMPOSER_OCCUPIED_MAX_REPROBES} probes; holding until new message, read/ack, or daemon restart{}",
                    recognised_payload_suffix(payload)
                ));
            }
            // Fail closed: only a probe that positively observed a clean composer (or one
            // exactly matching recorded deliveries) may resume dispatch. `Some(false)` means
            // the composer still holds non-wire input or a dialog, and `None` means the probe
            // could not observe the composer at all; both keep the fence and never send keys.
            if ready != Some(true) {
                continue;
            }
            self.maybe_unfence_and_resume(&recipient, now.clone());
        }
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
                        for rcpt in &receipts {
                            self.reset_composer_occupied_reprobe(&rcpt.recipient_name, &now);
                        }
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
                    self.reset_composer_occupied_reprobe(rec, &now);
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
                    self.reset_composer_occupied_reprobe(&recipient, &now);
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
                // This is a property of the adapter, not a trustworthy client claim.
                let unverified_process = unverified_process
                    || (explicit
                        && (hosted_id.is_some() || hydration_generation.is_some())
                        && !crate::vendor_adapter::tool_processes_descend_from_cli(
                            route.canonical_vendor(),
                        ));
                let evidence = hold_evidence(
                    &norm,
                    None,
                    SuppressionPhase::RetainedFence,
                    FenceReason::WorkerSpawnFailure,
                    &DispatchReport::default(),
                );
                let mut first_holds = Vec::new();
                let mut fence_release = None;
                let allow_probes =
                    crate::identity::host_probes_allowed(self.config.home_override.as_deref());
                let started_at = Instant::now();
                for attempt in 0..MAX_REGISTER_ROUTE_ATTEMPTS {
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
                    let peer_matches =
                        if explicit && (hosted_id.is_some() || hydration_generation.is_some()) {
                            verify_peer_cred_for_hosted_claim(
                                peer_cred,
                                (self.hosted_lookup)(
                                    self.store.as_ref(),
                                    &norm,
                                    route.canonical_vendor(),
                                    hosted_id.as_deref(),
                                    hydration_generation.as_deref(),
                                ),
                                replace,
                                &route,
                                hydration_generation.as_deref(),
                                current_incumbent.as_ref(),
                                current_dead.unwrap_or(false),
                            )
                        } else {
                            true
                        };
                    if !peer_matches {
                        return (
                            LocalResponse::Error {
                                message: "process_mismatch: caller does not match hosted process"
                                    .into(),
                            },
                            None,
                        );
                    }
                    let effective_verified = hosted_session_verified && peer_matches;

                    #[cfg(test)]
                    if let Some(ref mut hook) = *lock(&self.registration_probe_hook) {
                        let mut routes = lock(&self.routes);
                        hook(&mut routes);
                    }

                    // Publish a route only while ordered with persistence and backlog enqueue.
                    // Otherwise a concurrent send could overtake older pending messages.
                    let _order = lock(&self.enqueue_gate);
                    {
                        let mut routes = lock(&self.routes);
                        if explicit
                            && !crate::identity::same_opt_registration_identity(
                                routes.get(&norm),
                                current_incumbent.as_ref(),
                            )
                        {
                            if attempt + 1 < MAX_REGISTER_ROUTE_ATTEMPTS
                                && started_at.elapsed() < REGISTER_ROUTE_DEADLINE
                            {
                                continue;
                            }
                            let latest_inc = routes.get(&norm);
                            let diag = RouteRegistrationDiagnostic {
                                at: now.clone(),
                                last_seen_at: None,
                                count: 1,
                                persona: norm,
                                outcome: crate::dispatch_diagnostics::RegistrationDiagnosticOutcome::Refused,
                                reason: crate::dispatch_diagnostics::RegistrationConflictReason::RegistrationContention,
                                incumbent_adapter: latest_inc.map(|e| {
                                    crate::dispatch_diagnostics::RouteAdapter::from_route(&e.route)
                                }),
                                challenger_adapter:
                                    crate::dispatch_diagnostics::RouteAdapter::from_route(&route),
                                incumbent_hosted_id: latest_inc.and_then(|e| e.hosted_id.clone()),
                                challenger_hosted_id: hosted_id,
                                incumbent_generation: latest_inc
                                    .and_then(|e| e.hydration_generation.clone()),
                                challenger_generation: hydration_generation,
                                replace,
                                unverified_process,
                            }
                            .sanitized();
                            routes.push_registration_diagnostic(diag);
                            let _ = routes.save(self.config.routes_path.as_deref());
                            return (
                                LocalResponse::Error {
                                    message: "registration_contention: incumbent route changed concurrently; retry registration"
                                        .into(),
                                },
                                None,
                            );
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
                        // Any route-identity change (explicit `--replace`, a different
                        // vendor, or a new session identity on the same vendor) creates a
                        // new route identity when the new entry won registration: the
                        // retained fence belonged to the old route and must be released
                        // after publication. A passive same-identity re-registration
                        // keeps the fence, since the unconfirmed delivery may have
                        // reached that same session.
                        if let Some(new_entry) = routes.get(&norm) {
                            let gone = !before.entries.contains_key(&norm);
                            if self.should_release_fence_for(
                                &norm,
                                &new_entry.route,
                                explicit,
                                gone,
                            ) {
                                fence_release = Some(RouteFenceRelease {
                                    winning: new_entry.route.clone(),
                                    explicit,
                                    route_gone_before: gone,
                                    at: now.clone(),
                                });
                            }
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
                if let Some(release) = fence_release {
                    self.release_fence_for_new_route(&norm, release, None);
                }
                self.drain_awaiting_daemon_messages(true);
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

    fn drain_awaiting_daemon_messages(self: &Arc<Self>, recheck_unrouted: bool) {
        let Ok(awaiting) = self.store.list_wire_awaiting_daemon() else {
            return;
        };
        if awaiting.is_empty() {
            lock(&self.awaiting_daemon_state).clear();
            return;
        }
        let now = now_at();
        let routed_snapshot: BTreeSet<String> =
            lock(&self.routes).entries.keys().cloned().collect();

        for (mid, queued_at) in &awaiting {
            let expired = now.unix_millis().saturating_sub(queued_at.unix_millis())
                >= MAX_AWAITING_DAEMON_AGE_MS;
            if expired {
                let _ = self.store.clear_wire_awaiting_daemon(mid);
                lock(&self.awaiting_daemon_state).remove(mid);
                continue;
            }
            if !recheck_unrouted {
                let skip = lock(&self.awaiting_daemon_state)
                    .get(mid)
                    .is_some_and(|entry| {
                        entry.waiting_for_route
                            && !entry
                                .unscheduled_recipients
                                .iter()
                                .any(|key| routed_snapshot.contains(key))
                    });
                if skip {
                    continue;
                }
            }

            // Store reads happen outside `enqueue_gate` so a slow store never blocks
            // local accept or client sends.
            let msg = match self.store.get_wire_message(mid) {
                Ok(Some(msg)) => msg,
                Ok(None) => {
                    let _ = self.store.clear_wire_awaiting_daemon(mid);
                    lock(&self.awaiting_daemon_state).remove(mid);
                    continue;
                }
                Err(_) => continue,
            };
            let Ok(receipts) = self.store.list_wire_receipts(mid) else {
                continue;
            };
            let pending_receipts: Vec<DeliveryReceipt> = receipts
                .into_iter()
                .filter(|r| r.status == DeliveryStatus::Pending && r.attempts == 0)
                .collect();
            if pending_receipts.is_empty() {
                let _ = self.store.clear_wire_awaiting_daemon(mid);
                lock(&self.awaiting_daemon_state).remove(mid);
                continue;
            }

            let mut first_holds = Vec::new();
            let clear_marker = {
                let _order = lock(&self.enqueue_gate);
                let routed_keys: BTreeSet<String> =
                    lock(&self.routes).entries.keys().cloned().collect();
                self.admit_pending_receipts(&msg, &pending_receipts);
                let mut state = lock(&self.awaiting_daemon_state);
                let entry = state.entry(mid.clone()).or_default();
                entry.queued_at.get_or_insert_with(|| queued_at.clone());
                let should_notify = !entry.notified;
                entry.notified = true;
                let mut to_schedule = Vec::new();
                let mut all_routed = true;
                let mut unscheduled = BTreeSet::new();
                for rcpt in &pending_receipts {
                    let key = normalize_persona(&rcpt.recipient_name)
                        .unwrap_or_else(|| rcpt.recipient_name.trim().to_ascii_lowercase());
                    let is_routed = !key.is_empty() && routed_keys.contains(&key);
                    if is_routed {
                        if !entry.scheduled_routed.contains(&key) {
                            to_schedule.push((key, true, rcpt.clone()));
                        }
                    } else {
                        all_routed = false;
                        if !key.is_empty() {
                            unscheduled.insert(key.clone());
                        }
                        if !entry.tried_unrouted.contains(&key) {
                            to_schedule.push((key, false, rcpt.clone()));
                        }
                    }
                }
                entry.unscheduled_recipients = unscheduled;
                drop(state);

                if should_notify {
                    self.notify_subscribers_message(&msg, &pending_receipts);
                    self.broadcast_to_peers(
                        None,
                        &PeerFrame::Publish {
                            message: msg.clone(),
                            receipts: pending_receipts.clone(),
                        },
                    );
                }

                let mut schedule_ok = true;
                if !to_schedule.is_empty() {
                    let rcpts_to_schedule: Vec<DeliveryReceipt> =
                        to_schedule.iter().map(|(_, _, r)| r.clone()).collect();
                    match self.schedule_dispatch(
                        &msg,
                        &rcpts_to_schedule,
                        now.clone(),
                        &mut first_holds,
                    ) {
                        Ok(()) => {
                            let mut state = lock(&self.awaiting_daemon_state);
                            if let Some(entry) = state.get_mut(mid) {
                                for (key, is_routed, _) in to_schedule {
                                    if is_routed {
                                        entry.scheduled_routed.insert(key);
                                    } else {
                                        entry.tried_unrouted.insert(key);
                                    }
                                }
                                entry.waiting_for_route = !all_routed;
                            }
                        }
                        Err(message) => {
                            schedule_ok = false;
                            let mut state = lock(&self.awaiting_daemon_state);
                            if let Some(entry) = state.get_mut(mid) {
                                entry.waiting_for_route = false;
                                for (key, _, _) in to_schedule {
                                    if !key.is_empty() {
                                        entry.unscheduled_recipients.insert(key);
                                    }
                                }
                            }
                            warn(&message);
                        }
                    }
                } else {
                    let mut state = lock(&self.awaiting_daemon_state);
                    if let Some(entry) = state.get_mut(mid) {
                        entry.waiting_for_route = !all_routed;
                    }
                }

                all_routed && schedule_ok
            };

            log_first_holds(first_holds);
            if clear_marker {
                let _ = self.store.clear_wire_awaiting_daemon(mid);
                lock(&self.awaiting_daemon_state).remove(mid);
            }
        }
    }
}

pub struct DaemonHandle {
    socket_path: PathBuf,
    listen_addr: Option<String>,
    shutdown: Arc<AtomicBool>,
    join_handle: Option<std::thread::JoinHandle<()>>,
    poll_join_handle: Option<std::thread::JoinHandle<()>>,
    #[cfg(feature = "wire-test-isolation")]
    _tcp_fixture: Option<crate::test_isolation::TcpFixture>,
    #[cfg(feature = "wire-test-isolation")]
    _unix_fixture: Option<crate::test_isolation::UnixFixture>,
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
        #[cfg(feature = "wire-test-isolation")]
        let unix_safe = crate::test_isolation::ensure_socket(&self.socket_path).is_ok();
        #[cfg(not(feature = "wire-test-isolation"))]
        let unix_safe = true;
        if unix_safe {
            let _ = UnixStream::connect(&self.socket_path);
        }
        if let Some(ref addr) = self.listen_addr {
            #[cfg(feature = "wire-test-isolation")]
            let tcp_safe = addr
                .parse()
                .ok()
                .is_some_and(|address| crate::test_isolation::ensure_tcp(&address).is_ok());
            #[cfg(not(feature = "wire-test-isolation"))]
            let tcp_safe = true;
            if tcp_safe {
                #[cfg(feature = "wire-test-isolation")]
                if crate::test_isolation::is_enabled() {
                    if let Ok(address) = addr.parse::<std::net::SocketAddr>() {
                        let _ = TcpStream::connect(address);
                    }
                } else {
                    let _ = TcpStream::connect(addr);
                }
                #[cfg(not(feature = "wire-test-isolation"))]
                let _ = TcpStream::connect(addr);
            }
        }
        if let Some(h) = self.join_handle.take() {
            let _ = h.join();
        }
        if let Some(h) = self.poll_join_handle.take() {
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
    S: MessageStore + AgentRegistry + StateStore + Send + Sync + 'static,
{
    #[cfg(feature = "wire-test-isolation")]
    crate::test_isolation::ensure_fixture(&config.socket_path)?;
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
    #[cfg(feature = "wire-test-isolation")]
    let unix_fixture = if crate::test_isolation::is_enabled() {
        Some(crate::test_isolation::register_unix_socket(
            &config.socket_path,
        )?)
    } else {
        None
    };

    let tcp_listener = if let Some(ref bind_addr) = config.listen_addr {
        #[cfg(feature = "wire-test-isolation")]
        if crate::test_isolation::is_enabled()
            && bind_addr != "127.0.0.1:0"
            && bind_addr != "[::1]:0"
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "wire tests must bind a fresh loopback TCP fixture",
            ));
        }
        let l = TcpListener::bind(bind_addr)?;
        let actual = l.local_addr()?.to_string();
        config.listen_addr = Some(actual);
        l.set_nonblocking(true)?;
        Some(l)
    } else {
        None
    };

    #[cfg(feature = "wire-test-isolation")]
    let tcp_fixture = if crate::test_isolation::is_enabled() {
        tcp_listener
            .as_ref()
            .map(crate::test_isolation::register_tcp_listener)
            .transpose()?
    } else {
        None
    };
    let initial_routes = RouteTable::load(config.routes_path.as_deref());
    let initial_pending = seed_pending_map_from_store(store.as_ref());
    let inner = Arc::new(DaemonInner {
        store,
        hosted_lookup: hosted_reference::<S>,
        recovery_host_lookup: recovery_host_session::<S>,
        fence_recovery_policy: FenceRecoveryPolicy::from_env(),
        fence_recoveries: Mutex::new(BTreeMap::new()),
        #[cfg(test)]
        fence_recovery_probe_hook: Mutex::new(None),
        config: config.clone(),
        routes: Mutex::new(initial_routes),
        enqueue_gate: Mutex::new(()),
        dispatch_queues: Mutex::new(BTreeMap::new()),
        overflow_recipients: Mutex::new(BTreeSet::new()),
        overflow_capacity_exhausted: AtomicBool::new(false),
        dispatch_diagnostics: Mutex::new(new_dispatch_diagnostics()),
        fence_routes: Mutex::new(BTreeMap::new()),
        pending_by_recipient: Mutex::new(initial_pending),
        awaiting_daemon_state: Mutex::new(BTreeMap::new()),
        composer_occupied_reprobes: Mutex::new(BTreeMap::new()),
        #[cfg(test)]
        composer_probe_hook: Mutex::new(None),
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
        #[cfg(test)]
        registration_probe_hook: Mutex::new(None),
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
    inner.drain_awaiting_daemon_messages(true);

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
    let shutdown_poll = Arc::clone(&shutdown_flag);
    let inner_poll = Arc::clone(&inner);
    let poll_join_handle = std::thread::Builder::new()
        .name("fl-wire-awaiting-poll".into())
        .spawn(move || {
            let mut last_poll = Instant::now();
            while !shutdown_poll.load(Ordering::SeqCst)
                && !inner_poll.shutdown.load(Ordering::SeqCst)
            {
                if last_poll.elapsed() >= AWAITING_DAEMON_POLL_INTERVAL {
                    last_poll = Instant::now();
                    inner_poll.drain_awaiting_daemon_messages(false);
                    let now = now_at();
                    inner_poll.poll_composer_occupied_holds(now.clone());
                    inner_poll.poll_retained_fences(now);
                } else {
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        })?;

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
        poll_join_handle: Some(poll_join_handle),
        #[cfg(feature = "wire-test-isolation")]
        _tcp_fixture: tcp_fixture,
        #[cfg(feature = "wire-test-isolation")]
        _unix_fixture: unix_fixture,
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

    fn rig_inner<S>(
        tmp: &TempDir,
        store: Arc<S>,
        hosted_lookup: HostedLookup<S>,
    ) -> Arc<DaemonInner<S>> {
        Arc::new(DaemonInner {
            store,
            hosted_lookup,
            recovery_host_lookup: |_, _, _| Ok(None),
            fence_recovery_policy: FenceRecoveryPolicy::default(),
            fence_recoveries: Mutex::new(BTreeMap::new()),
            fence_recovery_probe_hook: Mutex::new(None),
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
            fence_routes: Mutex::new(BTreeMap::new()),
            pending_by_recipient: Mutex::new(BTreeMap::new()),
            awaiting_daemon_state: Mutex::new(BTreeMap::new()),
            composer_occupied_reprobes: Mutex::new(BTreeMap::new()),
            composer_probe_hook: Mutex::new(None),
            no_route_hook: Mutex::new(None),
            empty_exit_hook: Mutex::new(None),
            fail_next_worker_spawn: AtomicBool::new(false),
            queue_limit_override: AtomicUsize::new(MAX_QUEUED_PER_RECIPIENT),
            registration_locked_hook: Mutex::new(None),
            registration_probe_hook: Mutex::new(None),
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
        let inner = rig_inner(&tmp, store, hosted_reference::<RedbStore>);
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
        LeaveReceiptPending,
    }

    struct FaultStore {
        inner: RedbStore,
        gets: Mutex<VecDeque<Fault>>,
        reads: Mutex<VecDeque<Fault>>,
        writes: Mutex<VecDeque<Fault>>,
        before_receipt_write: Mutex<Option<Box<dyn FnOnce() + Send>>>,
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
        fn send_wire_awaiting_daemon(
            &self,
            args: SendWireArgs,
        ) -> Result<(WireMessage, Vec<DeliveryReceipt>), StoreError> {
            lock(&self.calls).push("send");
            self.inner.send_wire_awaiting_daemon(args)
        }
        fn list_wire_awaiting_daemon(&self) -> Result<Vec<(MessageId, At)>, StoreError> {
            self.inner.list_wire_awaiting_daemon()
        }
        fn clear_wire_awaiting_daemon(&self, message_id: &MessageId) -> Result<(), StoreError> {
            self.inner.clear_wire_awaiting_daemon(message_id)
        }
        fn get_wire_message(&self, id: &MessageId) -> Result<Option<WireMessage>, StoreError> {
            lock(&self.calls).push("get");
            match self.fault(&self.gets) {
                Fault::Error => Err(StoreError::Backend(SECRET_FAULT.into())),
                Fault::Missing => Ok(None),
                Fault::Panic => panic!("{SECRET_FAULT}"),
                Fault::Forward => self.inner.get_wire_message(id),
                Fault::LeaveReceiptPending => unreachable!("receipt write fault"),
            }
        }
        fn list_wire_receipts(&self, id: &MessageId) -> Result<Vec<DeliveryReceipt>, StoreError> {
            lock(&self.calls).push("receipts");
            match self.fault(&self.reads) {
                Fault::Error => Err(StoreError::Backend(SECRET_FAULT.into())),
                Fault::Missing => Ok(Vec::new()),
                Fault::Panic => panic!("{SECRET_FAULT}"),
                Fault::Forward => self.inner.list_wire_receipts(id),
                Fault::LeaveReceiptPending => unreachable!("receipt write fault"),
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
            let fault = self.fault(&self.writes);
            let hook = lock(&self.before_receipt_write).take();
            if let Some(hook) = hook {
                hook();
            }
            match fault {
                Fault::Error => Err(StoreError::Backend(SECRET_FAULT.into())),
                Fault::Missing => Ok(None),
                Fault::Panic => panic!("{SECRET_FAULT}"),
                Fault::Forward => self
                    .inner
                    .update_wire_receipt(id, recipient, status, increment, at),
                Fault::LeaveReceiptPending => self.inner.update_wire_receipt(
                    id,
                    recipient,
                    DeliveryStatus::Pending,
                    increment,
                    at,
                ),
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
            before_receipt_write: Mutex::new(None),
            calls: Mutex::new(Vec::new()),
        });
        let inner = rig_inner(&tmp, store, |store: &FaultStore, p, v, h, g| {
            hosted_reference(&store.inner, p, v, h, g)
        });
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

    pub(super) fn has_test_transport() -> bool {
        TRANSPORT_RESULT.with(|cell| cell.borrow().is_some())
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

    fn with_transport<T>(outcome: Result<(), String>, f: impl FnOnce() -> T) -> T {
        TRANSPORT_RESULT.with(|cell| *cell.borrow_mut() = Some(outcome));
        TRANSPORT_CALLS.with(|n| n.set(0));
        let out = f();
        TRANSPORT_RESULT.with(|cell| *cell.borrow_mut() = None);
        out
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
            // S6: fence release clears the diagnostics record.
            assert!(
                inner.diagnostics_snapshot().records.is_empty(),
                "release must clear diagnostics record"
            );
            let calls = TRANSPORT_CALLS.with(|n| n.get());
            // Park `held` behind an already-active worker (as `run_fault_job`
            // does): the send below is then queued without spawning a queue
            // worker, whose asynchronous real dispatch would otherwise race
            // the `attempts == 0` and empty-diagnostics assertions below.
            lock(&inner.dispatch_queues).insert(
                "held".into(),
                RecipientDispatchQueue {
                    worker_active: true,
                    ..Default::default()
                },
            );
            let (response, _) = inner.handle_local_request(LocalRequest::Send {
                args: mail("held", "later"),
            });
            let LocalResponse::Sent { message, .. } = response else {
                panic!("send did not persist")
            };
            // Synchronize on the queue worker state, not on timing: with a
            // bounded deadline, `later` must be pending behind the active
            // worker and must not have been dispatched.
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let queued = lock(&inner.dispatch_queues)
                    .get("held")
                    .is_some_and(|queue| {
                        queue.worker_active && queue.pending.iter().any(|(id, _)| id == &message.id)
                    });
                if queued {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "later message was not queued behind the active held worker"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
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
            assert!(inner.diagnostics_snapshot().records.is_empty());
        });
    }

    fn timed_recovery_fixture() -> (
        TempDir,
        Arc<DaemonInner<FaultStore>>,
        crate::test_isolation::RegisteredUnixListener,
        MessageId,
        At,
    ) {
        let (tmp, inner) = fault_rig();
        let socket = tmp.path().join("timed-replay.sock");
        let listener = crate::test_isolation::bind_unix_fixture(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        lock(&inner.routes).upsert_explicit(
            "held",
            VendorRoute::Webhook {
                socket_path: socket,
            },
            now_at(),
        );
        let first = with_transport(Err("initial delivery uncertain".into()), || {
            run_fault_job(&inner, "held")
        });
        let since = inner.diagnostics_snapshot().records[0]
            .hold_observed_at
            .clone();
        (tmp, inner, listener, first, since)
    }

    fn production_recovery_rig() -> (TempDir, Arc<DaemonInner<FaultStore>>) {
        let (tmp, mut inner) = fault_rig();
        Arc::get_mut(&mut inner).unwrap().recovery_host_lookup =
            |store, recipient, route| recovery_host_session(&store.inner, recipient, route);
        (tmp, inner)
    }

    fn put_recovery_runtime(
        inner: &DaemonInner<FaultStore>,
        vendor: &str,
        value: serde_json::Value,
    ) {
        use fl_core::state::{MergeMode, RuntimeEntry, SensitivityPatch, UpsertStateArgs};
        inner
            .store
            .inner
            .upsert_state(UpsertStateArgs {
                scope: StateScope::Workspace,
                slug: format!("hosted-session-{vendor}-held"),
                sensitivity: SensitivityPatch::Inherit,
                payload: StatePayload::Runtime(RuntimeEntry {
                    value,
                    merge_mode: MergeMode::LastWriter,
                }),
                at: now_at(),
                by: "test".into(),
            })
            .unwrap();
    }

    fn hold_recovery_mail(inner: &Arc<DaemonInner<FaultStore>>) -> (MessageId, At) {
        let id = with_transport(Err("initial delivery uncertain".into()), || {
            run_fault_job(inner, "held")
        });
        let since = inner.diagnostics_snapshot().records[0]
            .hold_observed_at
            .clone();
        (id, since)
    }

    fn wait_on_recovery(predicate: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !predicate() {
            assert!(
                Instant::now() < deadline,
                "dispatch worker did not reach expected state"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn timed_fence_recovery_production_rejects_unsupported_routes_with_zero_sends() {
        let (tmp, mut inner) = production_recovery_rig();
        // With no home override, unit discovery leaves the fixture registration
        // intact rather than applying an Antigravity executable fallback.
        Arc::get_mut(&mut inner).unwrap().config.home_override = None;
        let socket_path = tmp.path().join("unsupported-replay.sock");
        let listener = crate::test_isolation::bind_unix_fixture(&socket_path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let routes = [
            VendorRoute::Webhook { socket_path },
            VendorRoute::Agy {
                conversation_id: "antigravity:held".into(),
                ls_pid: None,
                ls_address: None,
                csrf_token: None,
                agentapi_exe: None,
            },
            VendorRoute::Grok {
                session_id: "grok:held".into(),
                cwd: None,
                tmux_target: None,
                tmux_pane: None,
            },
        ];
        for route in routes {
            lock(&inner.routes).upsert_explicit("held", route, now_at());
            let (id, since) = hold_recovery_mail(&inner);
            inner.poll_retained_fences(At::from_unix_millis(since.unix_millis() + 300_000));
            let status = inner.diagnostics_snapshot();
            assert_eq!(
                status.fence_recovery_attempts.last().unwrap().outcome,
                FenceRecoveryOutcome::UnsupportedRoute
            );
            assert_eq!(
                status.fence_recovery_attention[0].outcome,
                FenceRecoveryOutcome::UnsupportedRoute
            );
            assert!(inner.is_recipient_fenced("held"));
            assert_eq!(inner.store.list_wire_receipts(&id).unwrap()[0].attempts, 1);
            assert_eq!(
                listener.accept().unwrap_err().kind(),
                io::ErrorKind::WouldBlock
            );
            // Reset only the scheduler between fixtures.
            lock(&inner.overflow_recipients).clear();
            lock(&inner.fence_routes).clear();
            inner.clear_recovery_fence("held");
            lock(&inner.dispatch_diagnostics).clear_recipient("held");
        }
    }

    #[test]
    fn timed_fence_recovery_production_rejects_unproven_host_binding() {
        use FenceRecoveryOutcome::{
            DeadRoute, HostedBindingUnavailable, HostedLeaderPidUnavailable, NonHostedRoute,
        };
        use serde_json::json;

        let _env_lock = lock(&crate::dispatcher::TMUX_ENV_LOCK);
        for (hosted_id, route_generation, runtime_generation, state, pid, expected) in [
            (None, None, None, "running", Some(42), NonHostedRoute),
            (
                Some("codex:other"),
                Some("gen-2"),
                Some("gen-2"),
                "running",
                Some(42),
                HostedBindingUnavailable,
            ),
            (
                Some("codex:held"),
                Some("gen-2"),
                Some("gen-1"),
                "running",
                Some(42),
                HostedBindingUnavailable,
            ),
            (
                Some("codex:held"),
                Some("gen-2"),
                Some("gen-2"),
                "exited",
                Some(42),
                DeadRoute,
            ),
            (
                Some("codex:held"),
                Some("gen-2"),
                Some("gen-2"),
                "running",
                None,
                HostedLeaderPidUnavailable,
            ),
        ] {
            let (tmp, mut inner) = production_recovery_rig();
            Arc::get_mut(&mut inner).unwrap().config.home_override = None;
            let endpoint =
                fl_host_proto::HostEndpoint::unix(tmp.path().join("no-probe.sock")).unwrap();
            let _scope = crate::runtime_context::pin(endpoint, None);
            lock(&inner.routes).upsert_explicit_with_meta(
                "held",
                VendorRoute::Codex {
                    thread_id: "SESSION_SECRET".into(),
                    cwd: None,
                },
                now_at(),
                hosted_id.map(str::to_string),
                route_generation.map(str::to_string),
                true,
            );
            if let Some(runtime_generation) = runtime_generation {
                put_recovery_runtime(
                    &inner,
                    "codex",
                    json!({
                        "session_id": "codex:held",
                        "vendor": "codex",
                        "persona": "held",
                        "hosted": true,
                        "host_session_id": "Held.codex",
                        "status": state,
                        "hydration_generation": runtime_generation,
                        "hydration_status": "confirmed",
                        "runtime_leader_pid": pid,
                    }),
                );
            }
            let (id, since) = hold_recovery_mail(&inner);
            inner.poll_retained_fences(At::from_unix_millis(since.unix_millis() + 300_000));
            let status = inner.diagnostics_snapshot();
            assert_eq!(status.fence_recovery_attempts[0].outcome, expected);
            assert_eq!(status.fence_recovery_attention[0].outcome, expected);
            assert!(inner.is_recipient_fenced("held"));
            assert_eq!(inner.store.list_wire_receipts(&id).unwrap()[0].attempts, 1);
        }
    }

    #[test]
    fn timed_fence_recovery_zero_cap_disables_probes_and_preserves_fence() {
        let (_tmp, mut inner) = fault_rig();
        Arc::get_mut(&mut inner)
            .unwrap()
            .fence_recovery_policy
            .max_attempts = 0;
        let (id, since) = hold_recovery_mail(&inner);
        *lock(&inner.fence_recovery_probe_hook) = Some(Box::new(|| panic!("recovery disabled")));
        inner.poll_retained_fences(At::from_unix_millis(since.unix_millis() + 1_200_000));
        let status = inner.diagnostics_snapshot();
        assert!(status.fence_recovery_attempts.is_empty());
        assert_eq!(
            status.fence_recovery_attention[0].outcome,
            FenceRecoveryOutcome::RecoveryDisabled
        );
        assert!(inner.is_recipient_fenced("held"));
        assert_eq!(inner.store.list_wire_receipts(&id).unwrap()[0].attempts, 1);
    }

    #[test]
    fn timed_fence_recovery_notices_zero_attempt_replay_and_waits_for_worker() {
        let (tmp, inner) = fault_rig();
        let socket_path = tmp.path().join("zero-attempt.sock");
        let listener = crate::test_isolation::bind_unix_fixture(&socket_path).unwrap();
        listener.set_nonblocking(true).unwrap();
        lock(&inner.routes).upsert_explicit("held", VendorRoute::Webhook { socket_path }, now_at());
        let composer_draft =
            "composer_occupied_by_non_wire_input: holding delivery without sending keys";
        let first = with_transport(Err(composer_draft.into()), || run_fault_job(&inner, "held"));
        assert_eq!(
            inner.store.list_wire_receipts(&first).unwrap()[0].attempts,
            0
        );
        let since = inner.diagnostics_snapshot().records[0]
            .hold_observed_at
            .clone();
        assert_eq!(
            inner.diagnostics_snapshot().fence_recovery_attention[0].outcome,
            FenceRecoveryOutcome::WaitingForDelay
        );
        *lock(&inner.fence_recovery_probe_hook) = Some(Box::new(|| {
            RecoveryReadiness::Ready(RecoveryTarget::FixtureWebhook)
        }));
        for slot in 0..MAX_DISPATCH_RECIPIENTS {
            lock(&inner.dispatch_queues).insert(
                format!("occupied-{slot}"),
                RecipientDispatchQueue {
                    worker_active: true,
                    ..Default::default()
                },
            );
        }
        inner.poll_retained_fences(At::from_unix_millis(since.unix_millis() + 300_000));
        assert_eq!(
            inner.diagnostics_snapshot().fence_recovery_attention[0].outcome,
            FenceRecoveryOutcome::WaitingForWorker
        );
        assert!(inner.is_recipient_fenced("held"));
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        lock(&inner.dispatch_queues).retain(|name, _| name == "held");
        inner.wake_capacity_waiters();
        wait_on_recovery(|| !inner.is_recipient_fenced("held"));
        let (stream, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        let received: WireMessage = serde_json::from_str(&line).unwrap();
        assert_eq!(received.id, first);
        assert!(received.body.starts_with(TIMED_FENCE_REDELIVERY_NOTICE));
        assert_eq!(
            inner.diagnostics_snapshot().fence_recovery_attempts[0].outcome,
            FenceRecoveryOutcome::Released
        );
    }

    #[test]
    fn timed_fence_recovery_keeps_fence_after_submit_receipt_write_failure_until_route_change() {
        let (tmp, inner, listener, first, since) = timed_recovery_fixture();
        let tail = send_to_held_recipient(&inner, "waiting behind the failed receipt");
        lock(&inner.store.writes).push_back(Fault::Error);
        *lock(&inner.fence_recovery_probe_hook) = Some(Box::new(|| {
            RecoveryReadiness::Ready(RecoveryTarget::FixtureWebhook)
        }));
        inner.poll_retained_fences(At::from_unix_millis(since.unix_millis() + 300_000));
        wait_on_recovery(|| {
            !inner
                .diagnostics_snapshot()
                .fence_recovery_attempts
                .is_empty()
        });
        let (stream, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        let received: WireMessage = serde_json::from_str(&line).unwrap();
        assert_eq!(received.id, first);
        assert!(inner.is_recipient_fenced("held"));
        inner.poll_retained_fences(At::from_unix_millis(since.unix_millis() + 600_001));
        wait_on_recovery(|| !lock(&inner.dispatch_queues).contains_key("held"));
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock,
            "receipt bookkeeping must prevent a later timed replay"
        );
        let (response, _) = inner.handle_local_request(LocalRequest::Ack {
            recipient: "held".into(),
            message_ids: vec![first.clone()],
        });
        assert!(matches!(response, LocalResponse::Acked { .. }));
        assert!(inner.is_recipient_fenced("held"));
        assert_eq!(
            inner.store.list_wire_receipts(&tail).unwrap()[0].attempts,
            0
        );
        assert_eq!(
            inner.diagnostics_snapshot().fence_recovery_attempts.len(),
            1
        );
        assert!(
            inner.diagnostics_snapshot().records[0]
                .failure_stages
                .contains(&FailureStage::ReceiptWriteError)
        );
        assert_eq!(
            inner.diagnostics_snapshot().fence_recovery_attempts[0].outcome,
            FenceRecoveryOutcome::ReceiptBookkeepingFailed
        );
        assert_eq!(
            inner.diagnostics_snapshot().fence_recovery_attention[0].outcome,
            FenceRecoveryOutcome::ReceiptBookkeepingFailed
        );
        assert_eq!(
            inner.store.list_wire_receipts(&first).unwrap()[0].attempts,
            1
        );
        let replacement_path = tmp.path().join("timed-receipt-failure-replacement.sock");
        let replacement = crate::test_isolation::bind_unix_fixture(&replacement_path).unwrap();
        replacement.set_nonblocking(true).unwrap();
        let (response, _) = inner.handle_local_request(LocalRequest::RegisterRoute {
            persona: "held".into(),
            route: VendorRoute::Webhook {
                socket_path: replacement_path,
            },
            explicit: true,
            replace: true,
            hosted_id: None,
            hydration_generation: None,
            hosted_session_verified: false,
            unverified_process: false,
        });
        assert!(matches!(response, LocalResponse::RouteRegistered));
        wait_on_recovery(|| !lock(&inner.dispatch_queues).contains_key("held"));
        assert!(!inner.is_recipient_fenced("held"));
        assert!(
            inner
                .diagnostics_snapshot()
                .fence_recovery_attention
                .is_empty()
        );
        let (stream, _) = replacement.accept().unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        let delivered: WireMessage = serde_json::from_str(&line).unwrap();
        assert_eq!(delivered.id, tail);
    }

    #[test]
    fn timed_fence_recovery_stops_when_run_is_missing_after_confirmed_submit() {
        let (_tmp, inner, listener, first, since) = timed_recovery_fixture();
        let tail = send_to_held_recipient(&inner, "waiting behind the submitted message");
        let daemon = Arc::downgrade(&inner);
        *lock(&inner.store.before_receipt_write) = Some(Box::new(move || {
            let inner = daemon.upgrade().unwrap();
            let mut queues = lock(&inner.dispatch_queues);
            assert!(queues.get_mut("held").unwrap().recovery.take().is_some());
        }));
        *lock(&inner.fence_recovery_probe_hook) = Some(Box::new(|| {
            RecoveryReadiness::Ready(RecoveryTarget::FixtureWebhook)
        }));
        let due = since.unix_millis() + 300_000;
        inner.poll_retained_fences(At::from_unix_millis(due));
        wait_on_recovery(|| !lock(&inner.dispatch_queues).contains_key("held"));

        let (stream, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        let replayed: WireMessage = serde_json::from_str(&line).unwrap();
        assert_eq!(replayed.id, first);
        assert_eq!(
            inner.store.list_wire_receipts(&first).unwrap()[0].status,
            DeliveryStatus::Acked
        );
        assert_eq!(
            inner.store.list_wire_receipts(&tail).unwrap()[0].attempts,
            0
        );
        assert!(inner.is_recipient_fenced("held"));
        let status = inner.diagnostics_snapshot();
        assert_eq!(
            status.records[0].decisive_reason,
            FenceReason::ReplayStateUnavailable
        );
        assert_eq!(
            status.fence_recovery_attempts[0].outcome,
            FenceRecoveryOutcome::ReplayWorkerFailed
        );
        assert_eq!(
            status.fence_recovery_attention[0].outcome,
            FenceRecoveryOutcome::ReplayWorkerFailed
        );

        inner.poll_retained_fences(At::from_unix_millis(due + 300_001));
        wait_on_recovery(|| !lock(&inner.dispatch_queues).contains_key("held"));
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(
            inner.diagnostics_snapshot().fence_recovery_attempts.len(),
            1
        );
    }

    #[test]
    fn timed_fence_recovery_stops_on_pre_submit_store_faults() {
        for (fault, stage) in [
            (Fault::Error, FailureStage::MessageLookupError),
            (Fault::Missing, FailureStage::MessageLookupMissing),
            (Fault::Error, FailureStage::InitialReceiptReadError),
            (Fault::Missing, FailureStage::InitialReceiptReadMissing),
        ] {
            let (_tmp, inner, listener, first, since) = timed_recovery_fixture();
            let faults = match stage {
                FailureStage::MessageLookupError | FailureStage::MessageLookupMissing => {
                    &inner.store.gets
                }
                _ => &inner.store.reads,
            };
            lock(faults).push_back(fault);
            *lock(&inner.fence_recovery_probe_hook) = Some(Box::new(|| {
                RecoveryReadiness::Ready(RecoveryTarget::FixtureWebhook)
            }));
            let due = since.unix_millis() + 300_000;
            inner.poll_retained_fences(At::from_unix_millis(due));
            wait_on_recovery(|| !lock(&inner.dispatch_queues).contains_key("held"));
            assert_eq!(
                listener.accept().unwrap_err().kind(),
                io::ErrorKind::WouldBlock
            );
            let status = inner.diagnostics_snapshot();
            assert!(status.records[0].failure_stages.contains(&stage));
            assert_eq!(
                status.fence_recovery_attempts[0].outcome,
                FenceRecoveryOutcome::ReceiptBookkeepingFailed
            );
            assert_eq!(
                status.fence_recovery_attention[0].outcome,
                FenceRecoveryOutcome::ReceiptBookkeepingFailed
            );

            inner.poll_retained_fences(At::from_unix_millis(due + 300_001));
            wait_on_recovery(|| !lock(&inner.dispatch_queues).contains_key("held"));
            assert_eq!(
                listener.accept().unwrap_err().kind(),
                io::ErrorKind::WouldBlock
            );
            assert_eq!(
                inner.diagnostics_snapshot().fence_recovery_attempts.len(),
                1
            );
            assert!(inner.is_recipient_fenced("held"));
            let receipt = inner.store.list_wire_receipts(&first).unwrap().remove(0);
            assert_eq!(receipt.status, DeliveryStatus::Pending);
            assert_eq!(receipt.attempts, 1);
        }
    }

    #[test]
    fn timed_fence_recovery_stops_after_submit_without_receipt_confirmation() {
        for (fault, stage, outcome) in [
            (
                Fault::Missing,
                FailureStage::ReceiptWriteMissing,
                FenceRecoveryOutcome::ReceiptBookkeepingFailed,
            ),
            (
                Fault::LeaveReceiptPending,
                FailureStage::UnconfirmedReceiptAfterSubmit,
                FenceRecoveryOutcome::ReceiptBookkeepingFailed,
            ),
            (
                Fault::Forward,
                FailureStage::FinalReceiptReadError,
                FenceRecoveryOutcome::ReceiptBookkeepingFailed,
            ),
            (
                Fault::Panic,
                FailureStage::WorkerPanic,
                FenceRecoveryOutcome::ReplayWorkerFailed,
            ),
        ] {
            let (_tmp, inner, listener, _, since) = timed_recovery_fixture();
            if matches!(fault, Fault::Forward) {
                let daemon = Arc::downgrade(&inner);
                *lock(&inner.store.before_receipt_write) = Some(Box::new(move || {
                    lock(&daemon.upgrade().unwrap().store.reads).push_back(Fault::Error);
                }));
            }
            lock(&inner.store.writes).push_back(fault);
            *lock(&inner.fence_recovery_probe_hook) = Some(Box::new(|| {
                RecoveryReadiness::Ready(RecoveryTarget::FixtureWebhook)
            }));
            let due = since.unix_millis() + 300_000;
            inner.poll_retained_fences(At::from_unix_millis(due));
            wait_on_recovery(|| !lock(&inner.dispatch_queues).contains_key("held"));
            listener.accept().unwrap();
            inner.poll_retained_fences(At::from_unix_millis(due + 300_000));
            wait_on_recovery(|| !lock(&inner.dispatch_queues).contains_key("held"));
            assert_eq!(
                listener.accept().unwrap_err().kind(),
                io::ErrorKind::WouldBlock
            );
            assert!(inner.is_recipient_fenced("held"));
            let diagnostics = inner.diagnostics_snapshot();
            assert_eq!(diagnostics.fence_recovery_attempts.len(), 1);
            assert_eq!(diagnostics.fence_recovery_attention[0].outcome, outcome);
            assert!(diagnostics.records[0].failure_stages.contains(&stage));
        }
    }

    #[test]
    fn timed_fence_recovery_refunds_a_run_resolved_without_sending() {
        for resolve_during_worker in [false, true] {
            let (_tmp, inner, listener, first, since) = timed_recovery_fixture();
            let daemon = Arc::downgrade(&inner);
            let resolved_id = first.clone();
            let mut probes = 0;
            *lock(&inner.fence_recovery_probe_hook) = Some(Box::new(move || {
                probes += 1;
                let resolve_at = if resolve_during_worker { 2 } else { 1 };
                if probes == resolve_at {
                    daemon
                        .upgrade()
                        .unwrap()
                        .store
                        .inner
                        .update_wire_receipt(
                            &resolved_id,
                            "held",
                            DeliveryStatus::Acked,
                            false,
                            now_at(),
                        )
                        .unwrap()
                        .unwrap();
                }
                RecoveryReadiness::Ready(RecoveryTarget::FixtureWebhook)
            }));
            let due = since.unix_millis() + 300_000;
            inner.poll_retained_fences(At::from_unix_millis(due));
            wait_on_recovery(|| {
                !inner
                    .diagnostics_snapshot()
                    .fence_recovery_attempts
                    .is_empty()
            });
            assert_eq!(
                listener.accept().unwrap_err().kind(),
                io::ErrorKind::WouldBlock
            );
            assert!(inner.is_recipient_fenced("held"));
            assert_eq!(lock(&inner.fence_recoveries)["held"].attempts, 0);
            assert_eq!(
                inner.diagnostics_snapshot().fence_recovery_attempts[0].outcome,
                FenceRecoveryOutcome::Resolved
            );
            assert!(
                inner
                    .diagnostics_snapshot()
                    .fence_recovery_attention
                    .is_empty()
            );
            inner.poll_retained_fences(At::from_unix_millis(due + 300_000));
            assert_eq!(
                inner.diagnostics_snapshot().fence_recovery_attempts.len(),
                1
            );

            let next = send_to_held_recipient(&inner, "newly admitted after no-op");
            inner.poll_retained_fences(At::from_unix_millis(due + 300_001));
            wait_on_recovery(|| !inner.is_recipient_fenced("held"));
            let (stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(stream).read_line(&mut line).unwrap();
            let replayed: WireMessage = serde_json::from_str(&line).unwrap();
            assert_eq!(replayed.id, next);
            assert_eq!(
                inner.diagnostics_snapshot().fence_recovery_attempts[1].attempt,
                1
            );
        }
    }

    fn send_to_held_recipient(inner: &Arc<DaemonInner<FaultStore>>, body: &str) -> MessageId {
        let (response, _) = inner.handle_local_request(LocalRequest::Send {
            args: mail("held", body),
        });
        let LocalResponse::Sent { message, .. } = response else {
            panic!("held input was not stored: {response:?}");
        };
        message.id
    }

    #[test]
    fn timed_fence_recovery_receipt_write_error_wins_over_concurrent_ack() {
        let (_tmp, inner, listener, first, since) = timed_recovery_fixture();
        let tail = send_to_held_recipient(&inner, "waiting behind the first message");
        let acknowledger = Arc::downgrade(&inner);
        let ack_id = first.clone();
        *lock(&inner.store.before_receipt_write) = Some(Box::new(move || {
            let inner = acknowledger.upgrade().unwrap();
            let (response, _) = inner.handle_local_request(LocalRequest::Ack {
                recipient: "held".into(),
                message_ids: vec![ack_id],
            });
            assert!(matches!(response, LocalResponse::Acked { .. }));
        }));
        lock(&inner.store.writes).push_back(Fault::Error);
        *lock(&inner.fence_recovery_probe_hook) = Some(Box::new(|| {
            RecoveryReadiness::Ready(RecoveryTarget::FixtureWebhook)
        }));
        inner.poll_retained_fences(At::from_unix_millis(since.unix_millis() + 300_000));
        wait_on_recovery(|| !lock(&inner.dispatch_queues).contains_key("held"));

        let (stream, _) = listener.accept().unwrap();
        let mut body = String::new();
        BufReader::new(stream).read_line(&mut body).unwrap();
        let replayed: WireMessage = serde_json::from_str(&body).unwrap();
        assert_eq!(replayed.id, first);
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(
            inner.store.list_wire_receipts(&first).unwrap()[0].status,
            DeliveryStatus::Acked
        );
        assert_eq!(
            inner.store.list_wire_receipts(&tail).unwrap()[0].attempts,
            0
        );
        assert!(inner.is_recipient_fenced("held"));
        assert_eq!(
            inner.diagnostics_snapshot().fence_recovery_attempts[0].outcome,
            FenceRecoveryOutcome::ReceiptBookkeepingFailed
        );
        lock(&inner.dispatch_diagnostics).snapshot.records.clear();
        inner.poll_retained_fences(At::from_unix_millis(since.unix_millis() + 600_000));
        wait_on_recovery(|| !lock(&inner.dispatch_queues).contains_key("held"));
        assert!(inner.is_recipient_fenced("held"));
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn timed_fence_recovery_preserves_manual_release_after_prefix_overflow() {
        for use_route_replacement in [false, true] {
            let (tmp, inner, _listener, first, since) = timed_recovery_fixture();
            let fenced_route = lock(&inner.fence_routes).get("held").unwrap().clone();
            let replacement_path = tmp.path().join("timed-overflow-winner.sock");
            let _replacement = crate::test_isolation::bind_unix_fixture(&replacement_path).unwrap();
            let (write_entered_tx, write_entered_rx) = mpsc::channel();
            let (write_resume_tx, write_resume_rx) = mpsc::channel();
            *lock(&inner.store.before_receipt_write) = Some(Box::new(move || {
                write_entered_tx.send(()).unwrap();
                write_resume_rx
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap();
            }));
            *lock(&inner.fence_recovery_probe_hook) = Some(Box::new(|| {
                RecoveryReadiness::Ready(RecoveryTarget::FixtureWebhook)
            }));
            inner.queue_limit_override.store(1, Ordering::SeqCst);
            let due = since.unix_millis() + 300_000;
            inner.poll_retained_fences(At::from_unix_millis(due));
            write_entered_rx
                .recv_timeout(Duration::from_secs(5))
                .unwrap();

            let tail = send_to_held_recipient(&inner, "admitted to the prefix");
            let overflow = send_to_held_recipient(&inner, "stored beyond the prefix");
            assert!(lock(&inner.dispatch_queues)["held"].overflowed);
            if use_route_replacement {
                let (response, _) = inner.handle_local_request(LocalRequest::RegisterRoute {
                    persona: "held".into(),
                    route: VendorRoute::Webhook {
                        socket_path: replacement_path,
                    },
                    explicit: true,
                    replace: true,
                    hosted_id: None,
                    hydration_generation: None,
                    hosted_session_verified: false,
                    unverified_process: false,
                });
                assert!(matches!(response, LocalResponse::RouteRegistered));
            } else {
                let (response, _) = inner.handle_local_request(LocalRequest::Ack {
                    recipient: "held".into(),
                    message_ids: vec![first],
                });
                assert!(matches!(response, LocalResponse::Acked { .. }));
            }
            write_resume_tx.send(()).unwrap();
            wait_on_recovery(|| !lock(&inner.dispatch_queues).contains_key("held"));

            assert!(inner.is_recipient_fenced("held"));
            assert_eq!(lock(&inner.fence_routes).get("held"), Some(&fenced_route));
            let status = inner.diagnostics_snapshot();
            assert_eq!(status.fence_recovery_attempts.len(), 1);
            assert_eq!(
                status.fence_recovery_attempts[0].outcome,
                FenceRecoveryOutcome::PrefixOverflow
            );
            assert_eq!(
                status.fence_recovery_attention[0].outcome,
                FenceRecoveryOutcome::PrefixOverflow
            );
            assert_eq!(
                inner.store.list_wire_receipts(&tail).unwrap()[0].attempts,
                1
            );
            assert_eq!(
                inner.store.list_wire_receipts(&overflow).unwrap()[0].attempts,
                0
            );

            inner.poll_retained_fences(At::from_unix_millis(due + 300_000));
            wait_on_recovery(|| !lock(&inner.dispatch_queues).contains_key("held"));
            assert!(!inner.is_recipient_fenced("held"));
            assert_eq!(
                inner.store.list_wire_receipts(&overflow).unwrap()[0].attempts,
                1
            );
            assert_eq!(
                inner.diagnostics_snapshot().fence_recovery_attempts.len(),
                1
            );
        }
    }

    #[test]
    fn timed_fence_recovery_stops_after_overflow_prefix_receipt_write_failure() {
        for overflow_during_tail in [false, true] {
            let (tmp, inner, listener, first, since) = timed_recovery_fixture();
            let register = |socket_path| {
                let (response, _) = inner.handle_local_request(LocalRequest::RegisterRoute {
                    persona: "held".into(),
                    route: VendorRoute::Webhook { socket_path },
                    explicit: true,
                    replace: true,
                    hosted_id: None,
                    hydration_generation: None,
                    hosted_session_verified: false,
                    unverified_process: false,
                });
                assert!(matches!(response, LocalResponse::RouteRegistered));
            };
            let tail_path = tmp.path().join("overflow-receipt-tail.sock");
            let tail_listener = crate::test_isolation::bind_unix_fixture(&tail_path).unwrap();
            tail_listener.set_nonblocking(true).unwrap();
            let block_receipt_write = || {
                let (entered_tx, entered_rx) = mpsc::channel();
                let (resume_tx, resume_rx) = mpsc::channel();
                *lock(&inner.store.before_receipt_write) = Some(Box::new(move || {
                    entered_tx.send(()).unwrap();
                    resume_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                }));
                (entered_rx, resume_tx)
            };
            let ack = |id: &MessageId| {
                let (response, _) = inner.handle_local_request(LocalRequest::Ack {
                    recipient: "held".into(),
                    message_ids: vec![id.clone()],
                });
                assert!(matches!(response, LocalResponse::Acked { .. }));
            };
            let (first_write, finish_first) = block_receipt_write();
            *lock(&inner.fence_recovery_probe_hook) = Some(Box::new(|| {
                RecoveryReadiness::Ready(RecoveryTarget::FixtureWebhook)
            }));
            inner.queue_limit_override.store(1, Ordering::SeqCst);
            let due = since.unix_millis() + 300_000;
            inner.poll_retained_fences(At::from_unix_millis(due));
            first_write.recv_timeout(Duration::from_secs(5)).unwrap();

            register(tail_path.clone());
            assert!(
                lock(&inner.fence_recoveries)["held"]
                    .deferred_release
                    .route
                    .is_some()
            );
            let tail = send_to_held_recipient(&inner, "admitted prefix tail");
            let (tail_write, finish_tail) = block_receipt_write();
            lock(&inner.store.writes).push_back(Fault::Error);
            if overflow_during_tail {
                finish_first.send(()).unwrap();
                tail_write.recv_timeout(Duration::from_secs(5)).unwrap();
                send_to_held_recipient(&inner, "queued behind in-flight tail");
            }
            let overflow = {
                // Force the late overflow to retain its fence while routes is locked.
                let _routes = overflow_during_tail.then(|| lock(&inner.routes));
                send_to_held_recipient(&inner, "stored beyond the prefix")
            };
            assert!(lock(&inner.dispatch_queues)["held"].overflowed);
            if overflow_during_tail {
                // Resolve the receipt while its ordinary delivery write is failing.
                ack(&tail);
                assert!(lock(&inner.dispatch_queues)["held"].overflowed);
            } else {
                finish_first.send(()).unwrap();
                tail_write.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            // The previously deferred route and an explicit re-registration of it
            // must both wait until the in-flight job has a receipt result.
            inner.poll_retained_fences(At::from_unix_millis(due + 1));
            register(tail_path.clone());
            assert!(lock(&inner.dispatch_queues)["held"].overflowed);
            finish_tail.send(()).unwrap();
            wait_on_recovery(|| !lock(&inner.dispatch_queues).contains_key("held"));

            for (receiver, expected) in [(&listener, &first), (&tail_listener, &tail)] {
                let (stream, _) = receiver.accept().unwrap();
                let mut line = String::new();
                BufReader::new(stream).read_line(&mut line).unwrap();
                let sent: WireMessage = serde_json::from_str(&line).unwrap();
                assert_eq!(&sent.id, expected);
            }

            ack(&first);
            inner.queue_limit_override.store(8, Ordering::SeqCst);
            inner.poll_retained_fences(At::from_unix_millis(due + 300_000));
            wait_on_recovery(|| !lock(&inner.dispatch_queues).contains_key("held"));
            assert!(
                inner.is_recipient_fenced("held"),
                "deferred tail route must keep the fence"
            );
            register(tail_path);
            wait_on_recovery(|| !lock(&inner.dispatch_queues).contains_key("held"));
            assert!(
                inner.is_recipient_fenced("held"),
                "re-registering the tail route must keep the fence"
            );

            assert_eq!(
                tail_listener.accept().unwrap_err().kind(),
                io::ErrorKind::WouldBlock
            );
            for id in [&tail, &overflow] {
                assert_eq!(inner.store.list_wire_receipts(id).unwrap()[0].attempts, 0);
            }
            let status = inner.diagnostics_snapshot();
            let hold = status
                .records
                .iter()
                .find(|hold| hold.recipient.as_deref() == Some("held"))
                .unwrap();
            assert_eq!(hold.first_reason, FenceReason::ReceiptWriteError);
            assert_eq!(
                status.fence_recovery_attention[0].outcome,
                FenceRecoveryOutcome::ReceiptBookkeepingFailed
            );
            assert_eq!(status.fence_recovery_attempts.len(), 1);

            let replacement_path = tmp.path().join("overflow-receipt-replacement.sock");
            let replacement = crate::test_isolation::bind_unix_fixture(&replacement_path).unwrap();
            replacement.set_nonblocking(true).unwrap();
            register(replacement_path);
            wait_on_recovery(|| !lock(&inner.dispatch_queues).contains_key("held"));
            assert!(!inner.is_recipient_fenced("held"));
            assert!(replacement.accept().is_ok());
            assert_eq!(
                inner.store.list_wire_receipts(&overflow).unwrap()[0].attempts,
                1
            );
        }
    }

    #[test]
    fn timed_fence_recovery_retains_deferred_ack_while_overflow_prefix_drains() {
        for ack_during_replay in [true, false] {
            let (_tmp, inner, _listener, first, since) = timed_recovery_fixture();
            let block_receipt_write = || {
                let (entered_tx, entered_rx) = mpsc::channel();
                let (resume_tx, resume_rx) = mpsc::channel();
                *lock(&inner.store.before_receipt_write) = Some(Box::new(move || {
                    entered_tx.send(()).unwrap();
                    resume_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                }));
                (entered_rx, resume_tx)
            };
            let ack_first = || {
                let (response, _) = inner.handle_local_request(LocalRequest::Ack {
                    recipient: "held".into(),
                    message_ids: vec![first.clone()],
                });
                assert!(matches!(response, LocalResponse::Acked { .. }));
            };
            let (first_write, finish_first) = block_receipt_write();
            *lock(&inner.fence_recovery_probe_hook) = Some(Box::new(|| {
                RecoveryReadiness::Ready(RecoveryTarget::FixtureWebhook)
            }));
            inner.queue_limit_override.store(1, Ordering::SeqCst);
            let due = since.unix_millis() + 300_000;
            inner.poll_retained_fences(At::from_unix_millis(due));
            first_write.recv_timeout(Duration::from_secs(5)).unwrap();

            let tail = send_to_held_recipient(&inner, "previously attempted prefix tail");
            inner
                .store
                .update_wire_receipt(&tail, "held", DeliveryStatus::Pending, true, now_at())
                .unwrap()
                .unwrap();
            let overflow = send_to_held_recipient(&inner, "stored beyond the prefix");
            assert!(lock(&inner.dispatch_queues)["held"].overflowed);
            if ack_during_replay {
                ack_first();
            }
            let (tail_write, finish_tail) = block_receipt_write();
            finish_first.send(()).unwrap();
            tail_write.recv_timeout(Duration::from_secs(5)).unwrap();
            if !ack_during_replay {
                ack_first();
            }

            assert_eq!(
                lock(&inner.dispatch_queues)["held"].in_flight.as_ref(),
                Some(&tail)
            );
            let receipt = &inner.store.list_wire_receipts(&tail).unwrap()[0];
            assert_eq!(receipt.status, DeliveryStatus::Pending);
            assert_eq!(receipt.attempts, 1);
            inner.poll_retained_fences(At::from_unix_millis(due + 300_000));
            let ack_retained = lock(&inner.fence_recoveries)["held"]
                .deferred_release
                .ack
                .is_some();
            let fenced_during_tail = inner.is_recipient_fenced("held");
            finish_tail.send(()).unwrap();
            assert!(
                ack_retained,
                "prefix poll lost ACK (during replay: {ack_during_replay})"
            );
            assert!(fenced_during_tail);
            wait_on_recovery(|| !lock(&inner.dispatch_queues).contains_key("held"));

            inner.poll_retained_fences(At::from_unix_millis(due + 600_000));
            wait_on_recovery(|| !lock(&inner.dispatch_queues).contains_key("held"));
            assert!(!inner.is_recipient_fenced("held"));
            assert_eq!(
                inner.store.list_wire_receipts(&overflow).unwrap()[0].attempts,
                1
            );
            let attempts = inner.diagnostics_snapshot().fence_recovery_attempts;
            assert_eq!(attempts.len(), 1);
            assert_eq!(attempts[0].outcome, FenceRecoveryOutcome::PrefixOverflow);
        }
    }

    #[test]
    fn timed_fence_recovery_one_submit_releases_hosted_backlog_before_claude_turn_finishes() {
        exercise_timed_recovery_hosted_busy_backlog(false);
    }

    #[test]
    fn timed_fence_recovery_retries_after_hosted_busy_parks_are_exhausted() {
        exercise_timed_recovery_hosted_busy_backlog(true);
    }

    fn exercise_timed_recovery_hosted_busy_backlog(exhaust_busy_parks: bool) {
        use fl_host_proto::{
            ChildRecord, ChildState, ControllerLease, ExtensionLimits, ExtensionRequest,
            ExtensionResponse, HostConfig, HostEndpoint, HostInstance, HostListener, HostRequest,
            HostResponse, InputState, RENDERED_PTY_FEATURE, RenderedScreen, SendOutcome,
            SessionEpoch, SessionId, SessionMode, SessionSummary, SpawnKind, StreamId,
        };
        use std::os::unix::fs::PermissionsExt;

        #[derive(Debug, Default)]
        struct ScreenState {
            writes: Vec<String>,
            busy: bool,
            busy_captures: usize,
        }

        let _env_lock = lock(&crate::dispatcher::TMUX_ENV_LOCK);
        let (tmp, mut inner) = production_recovery_rig();
        Arc::get_mut(&mut inner).unwrap().config.home_override = None;
        fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let endpoint = HostEndpoint::unix(tmp.path().join("fl-host.sock")).unwrap();
        let session_id = SessionId::new("Held.claude").unwrap();
        let stream = StreamId {
            host: HostInstance::new([6; 16]),
            session: session_id.clone(),
            epoch: SessionEpoch(1),
        };
        let session = SessionSummary {
            stream: stream.clone(),
            mode: SessionMode::Pty,
            state: ChildState::Running(ChildRecord {
                session: session_id,
                session_epoch: stream.epoch,
                kind: SpawnKind::Pty,
                pid: 42,
                input: InputState::Writable,
                started_at_ms: 0,
                exit: None,
                consumer_stalled: false,
            }),
            input: InputState::Writable,
            acked: 0,
            latest: 0,
        };
        let screen_state = Arc::new(Mutex::new(ScreenState::default()));
        let observed = Arc::clone(&screen_state);
        let (ready_tx, ready_rx) = mpsc::channel();
        let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
        let server_endpoint = endpoint.clone();
        let server = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async move {
                    let mut config = HostConfig::new(server_endpoint);
                    config.minor = 1;
                    let mut listener = HostListener::bind(config).await.unwrap();
                    let _fixture = crate::test_isolation::register_unix_socket(
                        listener.endpoint().socket_path(),
                    )
                    .unwrap();
                    ready_tx.send(()).unwrap();
                    loop {
                        let mut conn = tokio::select! {
                            conn = listener.accept() => conn.unwrap(),
                            _ = &mut stop_rx => break,
                        };
                        while let Ok(HostRequest::Extended { id, request }) = conn.recv().await {
                            let response = match request {
                                ExtensionRequest::ListSessions => ExtensionResponse::Sessions {
                                    records: vec![session.clone()],
                                },
                                ExtensionRequest::CheckSessionOwner { stream, hosted_id } => {
                                    ExtensionResponse::SessionOwner {
                                        matches: stream == session.stream
                                            && hosted_id == "claude:held",
                                    }
                                }
                                ExtensionRequest::Capabilities => ExtensionResponse::Capabilities {
                                    host: session.stream.host.clone(),
                                    features: RENDERED_PTY_FEATURE
                                        | fl_host_proto::SESSION_OWNER_FEATURE,
                                    limits: ExtensionLimits::default(),
                                },
                                ExtensionRequest::ClaimController { .. } => {
                                    ExtensionResponse::Controller {
                                        lease: ControllerLease(1),
                                        acked: 0,
                                        latest: 0,
                                    }
                                }
                                ExtensionRequest::CaptureRendered { stream } => {
                                    let mut state = lock(&observed);
                                    let rule = "────────────────────────────────────────";
                                    let (composer, footer) = if state.busy {
                                        state.busy_captures += 1;
                                        (String::new(), "esc to interrupt")
                                    } else if let Some(paste) =
                                        state.writes.last().filter(|write| write.as_str() != "\r")
                                    {
                                        (
                                            paste
                                                .strip_prefix("\x1b[200~")
                                                .unwrap()
                                                .strip_suffix("\x1b[201~")
                                                .unwrap()
                                                .replace('\n', "\n  "),
                                            "? for shortcuts",
                                        )
                                    } else {
                                        (String::new(), "? for shortcuts")
                                    };
                                    ExtensionResponse::RenderedCapture {
                                        screen: RenderedScreen {
                                            schema: 1,
                                            stream,
                                            contents: format!(
                                                "{rule}\n❯ {composer}\n{rule}\n{footer}"
                                            ),
                                            cursor_row: 1,
                                            cursor_col: 2,
                                            rows: 24,
                                            cols: 100,
                                            generation: 1,
                                            output_seq: 0,
                                            output_bytes: 0,
                                        },
                                    }
                                }
                                ExtensionRequest::Write { bytes, .. } => {
                                    let bytes = String::from_utf8(bytes.into_vec()).unwrap();
                                    let mut state = lock(&observed);
                                    assert!(
                                        !state.busy,
                                        "write attempted during active Claude turn"
                                    );
                                    if bytes == "\r" {
                                        state.busy = true;
                                    }
                                    state.writes.push(bytes);
                                    ExtensionResponse::Input {
                                        outcome: SendOutcome::Written,
                                    }
                                }
                                request => {
                                    panic!("unexpected recovery fixture request: {request:?}")
                                }
                            };
                            conn.respond(&HostResponse::Extended { id, response })
                                .await
                                .unwrap();
                        }
                    }
                });
        });
        ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let _scope = crate::runtime_context::pin(endpoint, None);

        // Ordinary dispatch goes through Claude's native message socket after
        // the test releases the active turn on the hosted stream.
        let socket_path = tmp.path().join("claude-peer.sock");
        let listener = crate::test_isolation::bind_unix_fixture(&socket_path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let key_path = tmp.path().join("0.key");
        fs::write(&key_path, r#"{"peerToken":"fixture"}"#).unwrap();
        lock(&inner.routes).upsert_explicit_with_meta(
            "held",
            VendorRoute::Claude {
                pid: 0,
                session_id: "held-session".into(),
                socket_path,
                key_path: Some(key_path),
            },
            now_at(),
            Some("claude:held".into()),
            Some("gen-1".into()),
            true,
        );
        put_recovery_runtime(
            &inner,
            "claude",
            serde_json::json!({
                "session_id": "claude:held",
                "vendor": "claude",
                "persona": "held",
                "hosted": true,
                "host_session_id": "Held.claude",
                "status": "running",
                "hydration_generation": "gen-1",
                "hydration_status": "confirmed",
                "runtime_leader_pid": 42,
            }),
        );
        let (first, since) = hold_recovery_mail(&inner);
        let admit_tail = |n| {
            let (response, _) = inner.handle_local_request(LocalRequest::Send {
                args: SendWireArgs {
                    id: Some(MessageId(fl_core::ids::seq_iri(n))),
                    ..mail("held", "later")
                },
            });
            let LocalResponse::Sent { message, .. } = response else {
                panic!("held input was not stored: {response:?}");
            };
            message.id
        };
        let second = admit_tail(2);
        let third = admit_tail(1);
        assert!(third < second && second < first);
        inner
            .store
            .update_wire_receipt(&second, "held", DeliveryStatus::Pending, true, now_at())
            .unwrap()
            .unwrap();

        inner.poll_retained_fences(At::from_unix_millis(since.unix_millis() + 300_000));
        wait_on_recovery(|| {
            !inner
                .diagnostics_snapshot()
                .fence_recovery_attempts
                .is_empty()
        });
        assert_eq!(
            inner.diagnostics_snapshot().fence_recovery_attempts[0].outcome,
            FenceRecoveryOutcome::Released,
            "fixture capture state: {:?}",
            lock(&screen_state)
        );
        wait_on_recovery(|| {
            lock(&inner.dispatch_queues)
                .get("held")
                .is_none_or(|queue| queue.composer_retry_at.is_some())
        });
        assert!(!inner.is_recipient_fenced("held"));
        let status = inner.diagnostics_snapshot();
        assert!(status.records.is_empty() && status.fence_recovery_attention.is_empty());
        assert_eq!(status.fence_recovery_attempts.len(), 1);
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        {
            let screen = lock(&screen_state);
            assert_eq!(screen.writes.len(), 2);
            assert!(screen.writes[0].contains(first.as_str()));
            assert!(screen.writes[0].contains(TIMED_FENCE_REDELIVERY_NOTICE));
            assert_eq!(screen.writes[1], "\r");
            assert!(screen.busy_captures > 0);
        }

        const BUSY_POLLS: u32 = 13;
        if exhaust_busy_parks {
            let busy_since = lock(&inner.dispatch_queues)["held"]
                .composer_busy_since
                .clone();
            assert!(busy_since.is_some());
            // The first park is already in place. Revisit each parked head
            // without sleeping for the production backoff.
            for reprobes in 1..=COMPOSER_OCCUPIED_MAX_REPROBES {
                let retry_at = {
                    let queues = lock(&inner.dispatch_queues);
                    let queue = &queues["held"];
                    assert_eq!(queue.composer_reprobes, reprobes);
                    queue.composer_retry_at.clone().unwrap()
                };
                inner.poll_retained_fences(retry_at);
                wait_on_recovery(|| {
                    lock(&inner.dispatch_queues)
                        .get("held")
                        .is_none_or(|queue| {
                            queue.composer_reprobes > reprobes && queue.composer_retry_at.is_some()
                        })
                });
            }

            let status = inner.diagnostics_snapshot();
            assert!(inner.is_recipient_fenced("held"));
            assert_eq!(status.fence_recovery_attempts.len(), 1);
            assert_eq!(
                status.fence_recovery_attention[0].outcome,
                FenceRecoveryOutcome::SeatBusy
            );
            assert_eq!(status.fence_recovery_attention[0].busy_since, busy_since);
            let hold_at = status.records[0].hold_observed_at.unix_millis();
            let delay = inner.fence_recovery_policy.delay_ms;
            for probe in 1..=BUSY_POLLS {
                let due = hold_at + u64::from(probe) * delay;
                inner.poll_retained_fences(At::from_unix_millis(due - 1));
                assert_eq!(
                    inner
                        .diagnostics_snapshot()
                        .fence_recovery_attempts
                        .last()
                        .unwrap()
                        .busy_probe_count,
                    (probe > 1).then_some(probe - 1)
                );
                inner.poll_retained_fences(At::from_unix_millis(due));
                let status = inner.diagnostics_snapshot();
                assert_eq!(status.fence_recovery_attempts.len(), 2);
                let probe_row = &status.fence_recovery_attempts[1];
                assert_eq!(probe_row.outcome, FenceRecoveryOutcome::SeatBusy);
                assert_eq!(probe_row.busy_probe_count, Some(probe));
                assert_eq!(probe_row.at.unix_millis(), due);
                assert_eq!(lock(&inner.fence_recoveries)["held"].attempts, 0);
                let busy = &status.fence_recovery_attention[0];
                assert_eq!(
                    busy.outcome,
                    if probe >= 12 {
                        FenceRecoveryOutcome::SeatBusyProlonged
                    } else {
                        FenceRecoveryOutcome::SeatBusy
                    }
                );
                assert_eq!(busy.busy_since, busy_since);
                assert!(!busy.exhausted);
                assert!(inner.is_recipient_fenced("held"));
            }
            assert_eq!(
                listener.accept().unwrap_err().kind(),
                io::ErrorKind::WouldBlock
            );
            assert_eq!(
                inner.store.list_wire_receipts(&second).unwrap()[0].attempts,
                1
            );
            {
                let mut screen = lock(&screen_state);
                assert_eq!(screen.writes.len(), 2);
                assert!(screen.busy_captures > COMPOSER_OCCUPIED_MAX_REPROBES as usize);
                screen.busy = false;
            }
            inner.poll_retained_fences(At::from_unix_millis(
                hold_at + (u64::from(BUSY_POLLS) + 1) * delay,
            ));
            wait_on_recovery(|| {
                lock(&inner.dispatch_queues)
                    .get("held")
                    .is_some_and(|queue| queue.composer_retry_at.is_some())
            });
            let status = inner.diagnostics_snapshot();
            assert_eq!(status.fence_recovery_attempts.len(), 3);
            let replay = status.fence_recovery_attempts.last().unwrap();
            assert_eq!(replay.attempt, 1);
            assert_eq!(replay.outcome, FenceRecoveryOutcome::Released);
            assert!(!inner.is_recipient_fenced("held"));
            let screen = lock(&screen_state);
            assert_eq!(screen.writes.len(), 4);
            assert!(screen.writes[2].contains(second.as_str()));
            assert!(screen.writes[2].contains(TIMED_FENCE_REDELIVERY_NOTICE));
            assert_eq!(screen.writes[3], "\r");
        }

        lock(&screen_state).busy = false;
        let retry_at = lock(&inner.dispatch_queues)["held"]
            .composer_retry_at
            .clone()
            .unwrap();
        inner.poll_retained_fences(retry_at);
        wait_on_recovery(|| !lock(&inner.dispatch_queues).contains_key("held"));
        if !exhaust_busy_parks {
            assert_hosted_claude_peer_delivery(&listener, &second, true);
        }
        assert_hosted_claude_peer_delivery(&listener, &third, false);
        for id in [&second, &third] {
            assert_eq!(
                inner.store.list_wire_receipts(id).unwrap()[0].status,
                DeliveryStatus::Acked
            );
        }
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(
            inner.diagnostics_snapshot().fence_recovery_attempts.len(),
            if exhaust_busy_parks { 3 } else { 1 }
        );
        assert_eq!(
            lock(&screen_state).writes.len(),
            if exhaust_busy_parks { 4 } else { 2 }
        );
        stop_tx.send(()).unwrap();
        server.join().unwrap();
    }

    fn assert_hosted_claude_peer_delivery(
        listener: &crate::test_isolation::RegisteredUnixListener,
        id: &MessageId,
        already_attempted: bool,
    ) {
        let (socket, _) = listener.accept().unwrap();
        let user_frame: serde_json::Value =
            serde_json::from_str(&BufReader::new(socket).lines().nth(1).unwrap().unwrap()).unwrap();
        let body = user_frame["message"]["content"].as_str().unwrap();
        assert!(body.contains(id.as_str()), "{body}");
        assert_eq!(
            body.contains(TIMED_FENCE_REDELIVERY_NOTICE),
            already_attempted
        );
    }

    #[test]
    fn timed_fence_recovery_resumes_admitted_unread_in_order_with_notice() {
        let (_tmp, inner, listener, first, since) = timed_recovery_fixture();
        let (not_admitted, _) = inner.store.send_wire(mail("held", "unrelated")).unwrap();
        let (response, _) = inner.handle_local_request(LocalRequest::Send {
            args: SendWireArgs {
                id: Some(MessageId(fl_core::ids::seq_iri(1))),
                ..mail("held", "later")
            },
        });
        let LocalResponse::Sent { message, .. } = response else {
            panic!("held input was not stored: {response:?}");
        };
        let second = message.id;
        assert!(
            second < first,
            "store ID order differs from admission order"
        );
        *lock(&inner.fence_recovery_probe_hook) = Some(Box::new(|| {
            RecoveryReadiness::Ready(crate::dispatcher::RecoveryTarget::FixtureWebhook)
        }));
        let due = since.unix_millis() + 300_000;
        inner.poll_retained_fences(At::from_unix_millis(due - 1));
        assert!(lock(&inner.overflow_recipients).contains("held"));
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        inner.poll_retained_fences(At::from_unix_millis(due));

        let deadline = Instant::now() + Duration::from_secs(3);
        let mut received = Vec::<WireMessage>::new();
        while received.len() < 2 && Instant::now() < deadline {
            if let Ok((stream, _)) = listener.accept() {
                let mut line = String::new();
                BufReader::new(stream).read_line(&mut line).unwrap();
                received.push(serde_json::from_str(&line).unwrap());
            } else {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        assert_eq!(
            received.iter().map(|msg| &msg.id).collect::<Vec<_>>(),
            vec![&first, &second]
        );
        assert!(
            received[0]
                .body
                .contains("redelivery after timed fence recovery")
        );
        assert_eq!(received[1].body, "body");
        assert_eq!(
            inner.store.get_wire_message(&first).unwrap().unwrap().body,
            "body"
        );
        assert_eq!(
            inner.store.list_wire_receipts(&not_admitted.id).unwrap()[0].attempts,
            0
        );
        while lock(&inner.overflow_recipients).contains("held") && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!lock(&inner.overflow_recipients).contains("held"));
    }

    #[test]
    fn timed_fence_recovery_busy_reprobes_do_not_starve_older_eligible_fences() {
        let (tmp, mut inner) = fault_rig();
        Arc::get_mut(&mut inner)
            .unwrap()
            .fence_recovery_policy
            .delay_ms = 1_000;
        let socket_path = tmp.path().join("busy-reprobes.sock");
        let listener = crate::test_isolation::bind_unix_fixture(&socket_path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let recipients = ["agent-a", "agent-b", "agent-c", "agent-d", "agent-e"];
        with_transport(Err("initial delivery uncertain".into()), || {
            for recipient in recipients {
                lock(&inner.routes).upsert_explicit(
                    recipient,
                    VendorRoute::Webhook {
                        socket_path: socket_path.clone(),
                    },
                    now_at(),
                );
                run_fault_job(&inner, recipient);
            }
        });
        *lock(&inner.fence_recovery_probe_hook) = Some(Box::new(|| {
            RecoveryReadiness::Hold(FenceRecoveryOutcome::SeatBusy)
        }));
        let since = 100_000;
        for state in lock(&inner.fence_recoveries).values_mut() {
            state.since = At::from_unix_millis(since);
        }

        inner.poll_retained_fences(At::from_unix_millis(since + 1_000));
        let attempts = inner.diagnostics_snapshot().fence_recovery_attempts;
        assert_eq!(attempts.len(), MAX_FENCE_PROBES_PER_POLL);
        assert!(attempts.iter().all(|row| row.recipient != "agent-e"));

        // The first four are due again by this tick. The last recipient has
        // waited longer and must be probed before those busy seats retry.
        inner.poll_retained_fences(At::from_unix_millis(since + 3_000));
        let attempts = inner.diagnostics_snapshot().fence_recovery_attempts;
        assert_eq!(attempts.len(), recipients.len());
        assert_eq!(
            attempts
                .iter()
                .map(|row| (row.recipient.as_str(), row.busy_probe_count))
                .collect::<Vec<_>>(),
            [
                ("agent-d", Some(1)),
                ("agent-e", Some(1)),
                ("agent-a", Some(2)),
                ("agent-b", Some(2)),
                ("agent-c", Some(2)),
            ]
        );
        assert!(
            attempts
                .iter()
                .all(|row| { row.attempt == 1 && row.outcome == FenceRecoveryOutcome::SeatBusy })
        );
        assert!(
            recipients
                .iter()
                .all(|name| inner.is_recipient_fenced(name))
        );
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn timed_fence_recovery_refunds_worker_busy_and_resets_streak_on_other_outcomes() {
        let (_tmp, inner, listener, first, since) = timed_recovery_fixture();
        let mut call = 0;
        *lock(&inner.fence_recovery_probe_hook) = Some(Box::new(move || {
            call += 1;
            match call {
                // The twelfth timed poll sees readiness, but its worker's
                // second read-only check sees busy before attempting delivery.
                12 => RecoveryReadiness::Ready(RecoveryTarget::FixtureWebhook),
                14 => RecoveryReadiness::Hold(FenceRecoveryOutcome::Inconclusive),
                _ => RecoveryReadiness::Hold(FenceRecoveryOutcome::SeatBusy),
            }
        }));
        let delay = inner.fence_recovery_policy.delay_ms;
        let due = |period| At::from_unix_millis(since.unix_millis() + period * delay);
        for period in 1..=12 {
            inner.poll_retained_fences(due(period));
        }
        wait_on_recovery(|| !lock(&inner.dispatch_queues).contains_key("held"));
        let diagnostics = inner.diagnostics_snapshot();
        assert_eq!(diagnostics.fence_recovery_attempts.len(), 1);
        let probe_row = &diagnostics.fence_recovery_attempts[0];
        assert_eq!(probe_row.attempt, 1);
        assert_eq!(probe_row.outcome, FenceRecoveryOutcome::SeatBusy);
        assert_eq!(probe_row.busy_probe_count, Some(12));
        let busy = &diagnostics.fence_recovery_attention[0];
        assert_eq!(busy.outcome, FenceRecoveryOutcome::SeatBusyProlonged);
        assert_eq!(busy.busy_since, Some(due(1)));
        assert_eq!(lock(&inner.fence_recoveries)["held"].attempts, 0);

        inner.poll_retained_fences(due(13));
        let diagnostics = inner.diagnostics_snapshot();
        assert_eq!(
            diagnostics.fence_recovery_attention[0].outcome,
            FenceRecoveryOutcome::Inconclusive
        );
        assert_eq!(diagnostics.fence_recovery_attention[0].busy_since, None);
        assert_eq!(lock(&inner.fence_recoveries)["held"].attempts, 1);

        inner.poll_retained_fences(due(14));
        let diagnostics = inner.diagnostics_snapshot();
        assert_eq!(diagnostics.fence_recovery_attempts.len(), 3);
        assert_eq!(
            diagnostics.fence_recovery_attempts[2].busy_probe_count,
            Some(1)
        );
        assert_eq!(
            diagnostics.fence_recovery_attention[0].outcome,
            FenceRecoveryOutcome::SeatBusy
        );
        assert_eq!(
            diagnostics.fence_recovery_attention[0].busy_since,
            Some(due(14))
        );
        assert_eq!(lock(&inner.fence_recoveries)["held"].attempts, 1);
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(
            inner.store.list_wire_receipts(&first).unwrap()[0].attempts,
            1
        );
    }

    fn exhausted_session_busy_fence(
        max_attempts: u32,
    ) -> (
        TempDir,
        Arc<DaemonInner<FaultStore>>,
        crate::test_isolation::RegisteredUnixListener,
        MessageId,
    ) {
        let (tmp, mut inner) = fault_rig();
        Arc::get_mut(&mut inner)
            .unwrap()
            .fence_recovery_policy
            .max_attempts = max_attempts;
        let socket_path = tmp.path().join("exhausted-session-busy.sock");
        let listener = crate::test_isolation::bind_unix_fixture(&socket_path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let now = now_at();
        lock(&inner.routes).upsert_explicit(
            "held",
            VendorRoute::Webhook { socket_path },
            now.clone(),
        );
        let route = lock(&inner.routes).get("held").unwrap().clone();
        let (mail, _) = inner.store.send_wire(mail("held", "session busy")).unwrap();
        inner
            .store
            .update_wire_receipt(&mail.id, "held", DeliveryStatus::Pending, true, now.clone())
            .unwrap()
            .unwrap();
        // Start from the final ordinary park after a timed release. The
        // unsupported probe route lets ordinary dispatch run. The transport
        // seam supplies Claude's zero-key session-busy return to the real
        // dispatch report, receipt update and queue worker.
        assert!(matches!(
            inner.probe_fence_recovery_route("held", Some(&route)),
            Err(FenceRecoveryOutcome::UnsupportedRoute)
        ));
        lock(&inner.dispatch_queues).insert(
            "held".into(),
            RecipientDispatchQueue {
                pending: VecDeque::from([(mail.id.clone(), now.clone())]),
                worker_active: true,
                released_route: Some(route),
                composer_reprobes: COMPOSER_OCCUPIED_MAX_REPROBES,
                composer_busy_since: Some(now),
                ..Default::default()
            },
        );
        let busy = "composer_occupied_by_non_wire_input: session_busy: Claude session is not idle; holding delivery without sending keys";
        assert!(crate::dispatcher::is_composer_session_busy_error(busy));
        with_transport(Err(busy.into()), || {
            Arc::clone(&inner).run_recipient_queue("held".into())
        });
        assert_eq!(TRANSPORT_CALLS.with(std::cell::Cell::get), 1);
        assert!(inner.is_recipient_fenced("held"));
        let diagnostics = inner.diagnostics_snapshot();
        assert_eq!(diagnostics.records[0].route_observations.len(), 1);
        assert_eq!(
            diagnostics.records[0].decisive_reason,
            FenceReason::ComposerOccupiedByNonWireInput
        );
        assert_eq!(
            inner.store.list_wire_receipts(&mail.id).unwrap()[0].attempts,
            1
        );
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        (tmp, inner, listener, mail.id)
    }

    #[test]
    fn timed_fence_recovery_exhausted_session_busy_respects_disabled_policy() {
        let (_tmp, inner, listener, first) = exhausted_session_busy_fence(0);
        *lock(&inner.fence_recovery_probe_hook) =
            Some(Box::new(|| panic!("disabled recovery probed")));
        let since = lock(&inner.fence_recoveries)["held"].since.unix_millis();
        let delay = inner.fence_recovery_policy.delay_ms;
        inner.poll_retained_fences(At::from_unix_millis(since + delay));
        inner.poll_retained_fences(At::from_unix_millis(since + delay * 20));
        let diagnostics = inner.diagnostics_snapshot();
        assert!(diagnostics.fence_recovery_attempts.is_empty());
        assert_eq!(
            diagnostics.fence_recovery_attention[0].outcome,
            FenceRecoveryOutcome::RecoveryDisabled
        );
        assert!(inner.is_recipient_fenced("held"));
        assert_eq!(
            inner.store.list_wire_receipts(&first).unwrap()[0].attempts,
            1
        );
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn timed_fence_recovery_exhausted_session_busy_can_resume_after_ack() {
        let (_tmp, inner, listener, first) =
            exhausted_session_busy_fence(DEFAULT_FENCE_RECOVERY_MAX_ATTEMPTS);
        let diagnostics = inner.diagnostics_snapshot();
        assert_eq!(
            diagnostics.fence_recovery_attention[0].outcome,
            FenceRecoveryOutcome::SeatBusy
        );
        assert!(diagnostics.fence_recovery_attention[0].busy_since.is_some());
        let tail = send_to_held_recipient(&inner, "waiting for the ACK");
        let (response, _) = inner.handle_local_request(LocalRequest::Ack {
            recipient: "held".into(),
            message_ids: vec![first],
        });
        assert!(matches!(response, LocalResponse::Acked { .. }));
        wait_on_recovery(|| !lock(&inner.dispatch_queues).contains_key("held"));
        assert!(!inner.is_recipient_fenced("held"));
        assert!(
            inner
                .diagnostics_snapshot()
                .fence_recovery_attention
                .is_empty()
        );
        let (stream, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        let delivered: WireMessage = serde_json::from_str(&line).unwrap();
        assert_eq!(delivered.id, tail);
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn timed_fence_recovery_bounds_dead_route_probes_and_sends_nothing() {
        let (_tmp, inner, listener, first, since) = timed_recovery_fixture();
        let probes = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&probes);
        *lock(&inner.fence_recovery_probe_hook) = Some(Box::new(move || {
            count.fetch_add(1, Ordering::SeqCst);
            RecoveryReadiness::Hold(FenceRecoveryOutcome::DeadRoute)
        }));
        for period in 1..=5 {
            let tick = At::from_unix_millis(since.unix_millis() + period * 300_000);
            inner.poll_retained_fences(tick.clone());
            inner.poll_retained_fences(tick);
        }

        assert_eq!(probes.load(Ordering::SeqCst), 3);
        assert!(lock(&inner.overflow_recipients).contains("held"));
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(
            inner.store.list_wire_receipts(&first).unwrap()[0].attempts,
            1
        );
        let diagnostics = serde_json::to_value(inner.diagnostics_snapshot()).unwrap();
        assert!(diagnostics.to_string().contains("dead_route"));
        assert!(diagnostics.to_string().contains("\"exhausted\":true"));
    }

    #[test]
    fn timed_fence_recovery_discards_probe_when_registration_identity_changes() {
        let (tmp, inner, listener, first, since) = timed_recovery_fixture();
        let replacement_path = tmp.path().join("timed-replay-replacement.sock");
        let replacement = crate::test_isolation::bind_unix_fixture(&replacement_path).unwrap();
        replacement.set_nonblocking(true).unwrap();
        let daemon = Arc::downgrade(&inner);
        *lock(&inner.fence_recovery_probe_hook) = Some(Box::new(move || {
            let inner = daemon.upgrade().unwrap();
            lock(&inner.routes).upsert_explicit(
                "held",
                VendorRoute::Webhook {
                    socket_path: replacement_path.clone(),
                },
                now_at(),
            );
            RecoveryReadiness::Ready(crate::dispatcher::RecoveryTarget::FixtureWebhook)
        }));
        inner.poll_retained_fences(At::from_unix_millis(since.unix_millis() + 300_000));

        assert!(lock(&inner.overflow_recipients).contains("held"));
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(
            replacement.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(
            inner.store.list_wire_receipts(&first).unwrap()[0].attempts,
            1
        );
    }

    #[test]
    fn timed_fence_recovery_yields_to_winning_registration_during_replay() {
        let (tmp, inner, listener, first, since) = timed_recovery_fixture();
        let replacement_path = tmp.path().join("timed-replay-winner.sock");
        let replacement = crate::test_isolation::bind_unix_fixture(&replacement_path).unwrap();
        replacement.set_nonblocking(true).unwrap();
        let (replaying_tx, replaying_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let mut probes = 0;
        *lock(&inner.fence_recovery_probe_hook) = Some(Box::new(move || {
            probes += 1;
            if probes == 2 {
                replaying_tx.send(()).unwrap();
                resume_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            RecoveryReadiness::Ready(crate::dispatcher::RecoveryTarget::FixtureWebhook)
        }));
        let due = At::from_unix_millis(since.unix_millis() + 300_000);
        inner.poll_retained_fences(due.clone());
        replaying_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(lock(&inner.overflow_recipients).contains("held"));

        let (response, _) = inner.handle_local_request(LocalRequest::RegisterRoute {
            persona: "held".into(),
            route: VendorRoute::Webhook {
                socket_path: replacement_path,
            },
            explicit: true,
            replace: true,
            hosted_id: None,
            hydration_generation: None,
            hosted_session_verified: false,
            unverified_process: false,
        });
        assert!(matches!(response, LocalResponse::RouteRegistered));
        resume_tx.send(()).unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        let stream = loop {
            inner.poll_retained_fences(due.clone());
            match replacement.accept() {
                Ok((stream, _)) => break stream,
                Err(error)
                    if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("winning registration did not resume dispatch: {error}"),
            }
        };
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        let delivered: WireMessage = serde_json::from_str(&line).unwrap();
        assert_eq!(delivered.id, first);
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn timed_fence_recovery_keeps_budget_after_an_unsuccessful_replay() {
        let (_tmp, inner, listener, first, since) = timed_recovery_fixture();
        drop(listener);
        *lock(&inner.fence_recovery_probe_hook) = Some(Box::new(|| {
            RecoveryReadiness::Ready(crate::dispatcher::RecoveryTarget::FixtureWebhook)
        }));
        for attempt in 1..=3 {
            inner.poll_retained_fences(At::from_unix_millis(
                since.unix_millis() + attempt * 300_000,
            ));
            let deadline = Instant::now() + Duration::from_secs(3);
            while inner.diagnostics_snapshot().fence_recovery_attempts.len() < attempt as usize
                && Instant::now() < deadline
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            let snapshot = inner.diagnostics_snapshot();
            let row = snapshot
                .fence_recovery_attempts
                .last()
                .expect("replay result");
            assert_eq!(u64::from(row.attempt), attempt);
            assert_eq!(row.outcome, FenceRecoveryOutcome::ReplayFailed);
            assert!(lock(&inner.overflow_recipients).contains("held"));
        }

        inner.poll_retained_fences(At::from_unix_millis(since.unix_millis() + 1_200_000));
        let snapshot = inner.diagnostics_snapshot();
        assert_eq!(snapshot.fence_recovery_attempts.len(), 3);
        assert!(snapshot.fence_recovery_attention[0].exhausted);
        assert_eq!(
            inner.store.list_wire_receipts(&first).unwrap()[0].attempts,
            4
        );
    }

    #[test]
    fn timed_fence_recovery_transport_failure_preserves_ack_release() {
        let (_tmp, inner, listener, first, since) = timed_recovery_fixture();
        let acked_while_held = send_to_held_recipient(&inner, "read manually while held");
        let tail = send_to_held_recipient(&inner, "waiting for the ACK to resume dispatch");
        let socket = listener
            .local_addr()
            .unwrap()
            .as_pathname()
            .unwrap()
            .to_path_buf();
        let failed_socket = crate::test_isolation::register_unix_socket(&socket).unwrap();
        drop(listener);
        *lock(&inner.fence_recovery_probe_hook) = Some(Box::new(|| {
            RecoveryReadiness::Ready(RecoveryTarget::FixtureWebhook)
        }));
        inner.poll_retained_fences(At::from_unix_millis(since.unix_millis() + 300_000));
        wait_on_recovery(|| !lock(&inner.dispatch_queues).contains_key("held"));
        assert_eq!(
            inner.diagnostics_snapshot().fence_recovery_attempts[0].outcome,
            FenceRecoveryOutcome::ReplayFailed
        );
        assert_eq!(
            inner.store.list_wire_receipts(&first).unwrap()[0].attempts,
            2
        );

        drop(failed_socket);
        std::fs::remove_file(&socket).unwrap();
        let listener = crate::test_isolation::bind_unix_fixture(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let (response, _) = inner.handle_local_request(LocalRequest::Ack {
            recipient: "held".into(),
            message_ids: vec![acked_while_held],
        });
        assert!(matches!(response, LocalResponse::Acked { .. }));
        assert!(inner.is_recipient_fenced("held"));
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );

        let (response, _) = inner.handle_local_request(LocalRequest::Ack {
            recipient: "held".into(),
            message_ids: vec![first],
        });
        assert!(matches!(response, LocalResponse::Acked { .. }));
        wait_on_recovery(|| !lock(&inner.dispatch_queues).contains_key("held"));
        assert!(!inner.is_recipient_fenced("held"));
        assert!(
            inner
                .diagnostics_snapshot()
                .fence_recovery_attention
                .is_empty()
        );
        let (stream, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        let delivered: WireMessage = serde_json::from_str(&line).unwrap();
        assert_eq!(delivered.id, tail);
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn timed_fence_recovery_validates_hosted_generation_and_hydration() {
        use fl_core::state::{MergeMode, RuntimeEntry, SensitivityPatch, UpsertStateArgs};
        let rig = rig();
        let store = rig.inner.store.as_ref();
        let route = RouteEntry {
            persona: "held".into(),
            route: VendorRoute::Codex {
                thread_id: "018f1e22-9b8b-761c-8c07-b95a99913201".into(),
                cwd: None,
            },
            updated_at: At::from_unix_millis(1),
            identity_at: None,
            explicit: true,
            hosted_id: Some("codex:held".into()),
            hydration_generation: Some("gen-2".into()),
            pid_start_time: None,
        };
        let write_record = |at, generation, hydration| {
            store
                .upsert_state(UpsertStateArgs {
                    scope: StateScope::Workspace,
                    slug: "hosted-session-codex-held".into(),
                    sensitivity: SensitivityPatch::Inherit,
                    payload: StatePayload::Runtime(RuntimeEntry {
                        value: serde_json::json!({
                            "session_id": "codex:held",
                            "persona": "Held",
                            "vendor": "codex",
                            "hosted": true,
                            "host_session_id": "Held.codex",
                            "status": "running",
                            "hydration_generation": generation,
                            "hydration_status": hydration,
                            "runtime_leader_pid": 42,
                            "vendor_session_id": "018f1e22-9b8b-761c-8c07-b95a99913201"
                        }),
                        merge_mode: MergeMode::LastWriter,
                    }),
                    at: At::from_unix_millis(at),
                    by: "test".into(),
                })
                .unwrap();
        };

        write_record(1, "gen-1", "confirmed");
        assert_eq!(
            recovery_host_session(store, "held", &route),
            Err(FenceRecoveryOutcome::HostedBindingUnavailable)
        );
        write_record(2, "gen-2", "unconfirmed");
        assert_eq!(
            recovery_host_session(store, "held", &route),
            Ok(Some(RecoveryHostBinding {
                session_id: "Held.codex".into(),
                runtime_leader_pid: Some(42),
                startup_confirmed: false,
            }))
        );
        write_record(3, "gen-2", "confirmed");
        assert_eq!(
            recovery_host_session(store, "held", &route),
            Ok(Some(RecoveryHostBinding {
                session_id: "Held.codex".into(),
                runtime_leader_pid: Some(42),
                startup_confirmed: true,
            }))
        );
    }

    #[test]
    fn retained_fence_recovery_reports_proven_startup_blockers() {
        use FenceRecoveryOutcome::{DialogWaiting, Inconclusive, UnconfirmedStartup, VendorSignIn};

        assert_eq!(
            readiness_result(RecoveryReadiness::Hold(VendorSignIn), false),
            Err(VendorSignIn)
        );
        assert_eq!(
            readiness_result(RecoveryReadiness::Hold(DialogWaiting), false),
            Err(DialogWaiting)
        );
        assert_eq!(
            readiness_result(RecoveryReadiness::Hold(Inconclusive), false),
            Err(UnconfirmedStartup)
        );
        assert_eq!(
            readiness_result(
                RecoveryReadiness::Ready(RecoveryTarget::FixtureWebhook),
                false
            ),
            Err(UnconfirmedStartup)
        );
    }

    #[test]
    fn remodel_new_vendor_route_releases_fence_and_redelivers_pending() {
        let (tmp, inner) = fault_rig();
        lock(&inner.store.writes).push_back(Fault::Error);
        with_transport(Ok(()), || {
            let first_id = run_fault_job(&inner, "held");
            assert!(lock(&inner.overflow_recipients).contains("held"));
            // A second message arrives while fenced: stored, never dispatched.
            let (second, _) = inner.store.send_wire(mail("held", "second")).unwrap();
            let second_id = second.id.clone();

            // Remodel: a new vendor route replaces the old Codex route (`--replace`).
            let socket = tmp.path().join("remodel.sock");
            let listener = crate::test_isolation::bind_unix_fixture(&socket).unwrap();
            listener.set_nonblocking(true).unwrap();
            let (response, _) = inner.handle_local_request(LocalRequest::RegisterRoute {
                persona: "held".into(),
                route: VendorRoute::Webhook {
                    socket_path: socket,
                },
                explicit: true,
                replace: true,
                hosted_id: None,
                hydration_generation: None,
                hosted_session_verified: false,
                unverified_process: false,
            });
            assert!(matches!(response, LocalResponse::RouteRegistered));
            assert!(
                !lock(&inner.overflow_recipients).contains("held"),
                "fence from the dead old route must not block the remodelled session"
            );
            // Both pending messages are delivered to the new route, in order.
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut received = Vec::new();
            while received.len() < 2 && Instant::now() < deadline {
                if let Ok((stream, _)) = listener.accept() {
                    let mut line = String::new();
                    BufReader::new(stream).read_line(&mut line).unwrap();
                    received.push(serde_json::from_str::<WireMessage>(&line).unwrap().id);
                } else {
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            assert_eq!(received, vec![first_id, second_id]);
        });
    }

    #[test]
    fn unregister_then_register_new_vendor_releases_fence_b2() {
        let (_tmp, inner) = fault_rig();
        lock(&inner.store.writes).push_back(Fault::Error);
        with_transport(Ok(()), || {
            let _first = run_fault_job(&inner, "held");
            assert!(lock(&inner.overflow_recipients).contains("held"));
            let (r, _) = inner.handle_local_request(LocalRequest::UnregisterRoute {
                persona: "held".into(),
            });
            assert!(matches!(r, LocalResponse::RouteUnregistered));
            assert!(
                lock(&inner.overflow_recipients).contains("held"),
                "unregister alone keeps fence"
            );
            let (r, _) = inner.handle_local_request(LocalRequest::register(
                "held",
                VendorRoute::Claude {
                    pid: 0,
                    session_id: "new-claude".into(),
                    socket_path: std::path::PathBuf::from("/tmp/x.sock"),
                    key_path: None,
                },
                true,
            ));
            assert!(matches!(r, LocalResponse::RouteRegistered));
            assert!(
                !lock(&inner.overflow_recipients).contains("held"),
                "register after unregister must release fence (B2 remodel)"
            );
            assert!(
                inner
                    .diagnostics_snapshot()
                    .records
                    .iter()
                    .all(|r| r.recipient.as_deref() != Some("held")),
                "S6 diagnostics cleared"
            );
        });
    }

    #[test]
    fn same_vendor_new_session_without_replace_releases_fence() {
        let (_tmp, inner) = fault_rig();
        lock(&inner.store.writes).push_back(Fault::Error);
        with_transport(Ok(()), || {
            let _first = run_fault_job(&inner, "held");
            assert!(lock(&inner.overflow_recipients).contains("held"));
            // Same-identity passive re-registration keeps the fence.
            let (response, _) = inner.handle_local_request(LocalRequest::register(
                "held",
                VendorRoute::Codex {
                    thread_id: "SESSION_SECRET".into(),
                    cwd: Some("/CWD_SECRET".into()),
                },
                false,
            ));
            assert!(matches!(response, LocalResponse::RouteRegistered));
            assert!(
                lock(&inner.overflow_recipients).contains("held"),
                "same-identity passive re-registration must keep the fence"
            );
            // S2: passive same-vendor new session over a live incumbent does NOT release.
            let (response, _) = inner.handle_local_request(LocalRequest::register(
                "held",
                VendorRoute::Codex {
                    thread_id: "new-session".into(),
                    cwd: None,
                },
                false,
            ));
            assert!(matches!(response, LocalResponse::RouteRegistered));
            assert!(
                lock(&inner.overflow_recipients).contains("held"),
                "passive same-vendor registration must keep the fence (S2)"
            );
            // Explicit registration with the new identity does release.
            let (response, _) = inner.handle_local_request(LocalRequest::register(
                "held",
                VendorRoute::Codex {
                    thread_id: "new-session".into(),
                    cwd: None,
                },
                true,
            ));
            assert!(matches!(response, LocalResponse::RouteRegistered));
            assert!(
                !lock(&inner.overflow_recipients).contains("held"),
                "explicit new-session registration must release the old fence (S2)"
            );
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
        let listener = crate::test_isolation::bind_unix_fixture(&socket).unwrap();
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
        let listener = crate::test_isolation::bind_unix_fixture(&socket).unwrap();
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
        let listener = crate::test_isolation::bind_unix_fixture(&socket).unwrap();
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
        let listener = crate::test_isolation::bind_unix_fixture(&socket).unwrap();
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
        let listener = crate::test_isolation::bind_unix_fixture(&socket).unwrap();
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
        let listener = crate::test_isolation::bind_unix_fixture(&socket).unwrap();
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
        let slow_listener = crate::test_isolation::bind_unix_fixture(&slow_socket).unwrap();
        slow_listener.set_nonblocking(true).unwrap();
        let fast_socket = rig._tmp.path().join("other.sock");
        let fast_listener = crate::test_isolation::bind_unix_fixture(&fast_socket).unwrap();
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
        let listener = crate::test_isolation::bind_unix_fixture(&socket).unwrap();
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
        let listener = crate::test_isolation::bind_unix_fixture(&socket).unwrap();
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
        let listener = crate::test_isolation::bind_unix_fixture(&socket).unwrap();
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
        let listener = crate::test_isolation::bind_unix_fixture(&socket).unwrap();
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
        let listener = crate::test_isolation::bind_unix_fixture(&socket).unwrap();
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
        let webhook_listener = crate::test_isolation::bind_unix_fixture(&webhook_sock).unwrap();
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
    /// vendor delivery froze `Status` and `RegisterRoute` for as long as it ran. Hold the fake
    /// vendor binary open behind a filesystem gate and verify `Send`, `Status`, `RegisterRoute`,
    /// and delivery to another recipient all complete while the gate remains closed.
    #[test]
    fn status_and_route_registration_do_not_wait_behind_a_slow_vendor_delivery() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let slow = tmp.path().join("slow-vendor.sh");
        let started_file = tmp.path().join("started");
        let gate_file = tmp.path().join("gate");
        let done_file = tmp.path().join("done");
        fs::write(
            &slow,
            format!(
                "#!/bin/sh\nprintf started > '{}'\nwhile [ ! -e '{}' ]; do sleep 0.01; done\nprintf done > '{}'\n",
                started_file.display(),
                gate_file.display(),
                done_file.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&slow, fs::Permissions::from_mode(0o755)).unwrap();
        let _slow_fixture = crate::test_isolation::register_executable(&slow).unwrap();

        let store = Arc::new(RedbStore::open(&tmp.path().join("s.redb")).unwrap());
        let cfg = test_config(tmp.path(), "slow");
        let sock = cfg.socket_path.clone();
        let _daemon = spawn_daemon(Arc::clone(&store), cfg).unwrap();

        let mut reg = LocalClient::connect(&sock, Duration::from_secs(5)).unwrap();
        reg.request(&LocalRequest::register(
            "synthetic-recipient",
            VendorRoute::Agy {
                conversation_id: "t1".into(),
                ls_address: None,
                csrf_token: None,
                agentapi_exe: Some(slow.clone()),
                ls_pid: None,
            },
            true,
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
        let sent_promptly = sent_rx.recv_timeout(Duration::from_secs(10));
        assert!(
            matches!(sent_promptly, Ok(Ok(()))),
            "send waited for vendor delivery: {sent_promptly:?}"
        );
        assert!(
            !done_file.exists(),
            "vendor delivery finished before gate was released"
        );
        sender.join().unwrap();

        let deadline = Instant::now() + Duration::from_secs(10);
        while !started_file.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(started_file.exists(), "slow delivery did not start");
        assert!(
            !done_file.exists(),
            "slow delivery finished while gate was still closed"
        );

        let mut probe = LocalClient::connect(&sock, Duration::from_secs(5)).unwrap();
        probe
            .request(&LocalRequest::Status {
                include_codex_queues: false,
            })
            .unwrap();
        let other_socket = tmp.path().join("other.sock");
        let other_listener = crate::test_isolation::bind_unix_fixture(&other_socket).unwrap();
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
        let mut other = LocalClient::connect(&sock, Duration::from_secs(5)).unwrap();
        other
            .request(&LocalRequest::Send {
                args: mail("other", "fast one"),
            })
            .unwrap();
        let other_deadline = Instant::now() + Duration::from_secs(10);
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
            "other recipient did not receive while slow recipient was gate-held"
        );
        assert!(
            !done_file.exists(),
            "slow delivery finished before gate was released"
        );

        fs::write(&gate_file, "go").unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done_file.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(done_file.exists(), "slow delivery did not finish");
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
        let _script_fixture = crate::test_isolation::register_executable(&script).unwrap();
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
        let _script_fixture = crate::test_isolation::register_executable(&script).unwrap();
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
        let listener = crate::test_isolation::bind_unix_fixture(&fast_socket).unwrap();
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

        crate::test_isolation::ensure_socket(&sock).unwrap();
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
            crate::test_isolation::ensure_socket(&sock).unwrap();
            let mut raw = UnixStream::connect(&sock).unwrap();
            raw.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            raw.write_all(line.as_bytes()).unwrap();
            let mut resp = String::new();
            BufReader::new(&raw).read_line(&mut resp).unwrap();
            assert!(resp.contains("invalid request JSON"), "{line:?} -> {resp}");
        }
        // A client that connects and hangs up sends nothing; the daemon must shrug it off.
        crate::test_isolation::ensure_socket(&sock).unwrap();
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
        let stale_listener = UnixListener::bind(&cfg.socket_path).unwrap();
        let _stale_fixture = crate::test_isolation::register_unix_socket(&cfg.socket_path).unwrap();
        drop(stale_listener);
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
        let listener = crate::test_isolation::bind_unix_fixture(&sock).unwrap();
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
        let stale_listener = UnixListener::bind(&stale).unwrap();
        let _stale_fixture = crate::test_isolation::register_unix_socket(&stale).unwrap();
        drop(stale_listener);
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

        let ack_listener = crate::test_isolation::bind_unix_fixture(&ack_socket).unwrap();
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

        let read_listener = crate::test_isolation::bind_unix_fixture(&read_socket).unwrap();
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
        let listener = crate::test_isolation::bind_unix_fixture(&control_sock).unwrap();
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
        crate::test_isolation::ensure_socket(&daemon_sock).unwrap();
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

        let _probe_listener_fixture =
            crate::test_isolation::register_tcp_listener(&probe_listener).unwrap();
        probe_listener.set_nonblocking(true).unwrap();
        let probe_addr = probe_listener.local_addr().unwrap();

        let tmp = TempDir::new().unwrap();
        let store = Arc::new(RedbStore::open(&tmp.path().join("dest.redb")).unwrap());
        let mut inner = rig_inner(&tmp, store, hosted_reference::<RedbStore>);
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
    #[cfg(target_os = "linux")]
    fn codex_app_server_tool_can_register_with_generation_and_replace() {
        use fl_core::state::{
            MergeMode, RuntimeEntry, SensitivityPatch, StatePayload, StateScope, UpsertStateArgs,
        };
        use fl_core::store::StateStore;
        use std::io::{BufRead, BufReader};
        use std::os::unix::process::CommandExt;
        let rig = rig();
        let bin = rig._tmp.path().join("codex");
        std::os::unix::fs::symlink("/bin/sh", &bin).unwrap();
        let mut tui = std::process::Command::new(&bin)
            .args(["-c", "sleep 60"])
            .process_group(0)
            .spawn()
            .unwrap();
        let mut server = std::process::Command::new(&bin)
            .args(["-c", "sh -c 'echo HOOK:$$; sleep 60; :' & wait"])
            .stdout(std::process::Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap();
        let hook: u32 = BufReader::new(server.stdout.take().unwrap())
            .lines()
            .next()
            .unwrap()
            .unwrap()
            .strip_prefix("HOOK:")
            .unwrap()
            .parse()
            .unwrap();
        rig.inner.store.upsert_state(UpsertStateArgs {
            scope: StateScope::Workspace, slug: "hosted-session-codex-feira".into(), sensitivity: SensitivityPatch::Inherit,
            payload: StatePayload::Runtime(RuntimeEntry { value: serde_json::json!({"session_id":"codex:feira", "persona":"feira", "vendor":"codex", "status":"running", "hydration_generation":"gen-1", "pid":tui.id(), "pid_start_time":crate::identity::proc_start_time_ticks(tui.id())}), merge_mode: MergeMode::LastWriter }), at: At::from_unix_millis(1), by: "test".into(),
        }).unwrap();
        let request = |replace| LocalRequest::RegisterRoute {
            persona: "feira".into(),
            route: VendorRoute::Codex {
                thread_id: "thread".into(),
                cwd: None,
            },
            explicit: true,
            replace,
            hosted_id: None,
            hydration_generation: Some("gen-1".into()),
            hosted_session_verified: true,
            unverified_process: false,
        };
        let peer = PeerCred {
            pid: Some(hook),
            ppid: None,
            start_time: crate::identity::proc_start_time_ticks(hook),
        };
        let mut outcomes = Vec::new();
        for vendor in ["claude", "agy"] {
            let incumbent = if vendor == "claude" {
                VendorRoute::Claude {
                    pid: std::process::id(),
                    session_id: "incumbent".into(),
                    socket_path: rig._tmp.path().join("incumbent.sock"),
                    key_path: None,
                }
            } else {
                VendorRoute::Agy {
                    conversation_id: "incumbent".into(),
                    ls_pid: Some(std::process::id()),
                    ls_address: None,
                    csrf_token: None,
                    agentapi_exe: None,
                }
            };
            {
                let mut routes = lock(&rig.inner.routes);
                routes.upsert_explicit_with_meta(
                    "feira",
                    incumbent,
                    At::from_unix_millis(1),
                    None,
                    None,
                    true,
                );
                let entry = routes.get("feira").unwrap();
                assert!(!crate::identity::explicit_route_entry_is_dead(
                    entry,
                    Some("feira"),
                    false
                ));
            }
            let (without_replace, _) = rig
                .inner
                .handle_local_request_with_peer(request(false), peer);
            let (response, _) = rig
                .inner
                .handle_local_request_with_peer(request(true), peer);
            let routes = lock(&rig.inner.routes);
            let codex_route = matches!(
                routes.get("feira").unwrap().route,
                VendorRoute::Codex { .. }
            );
            let unverified = routes
                .registration_diagnostics
                .last()
                .is_some_and(|d| d.unverified_process);
            outcomes.push((vendor, without_replace, response, codex_route, unverified));
        }
        unsafe {
            libc::kill(-(server.id() as i32), libc::SIGKILL);
            libc::kill(-(tui.id() as i32), libc::SIGKILL);
        }
        let _ = server.wait();
        let _ = tui.wait();
        for (vendor, without_replace, response, codex_route, unverified) in outcomes {
            assert!(
                matches!(without_replace, LocalResponse::Error { .. }),
                "{vendor}: {without_replace:?}"
            );
            assert!(
                matches!(response, LocalResponse::RouteRegistered),
                "{vendor}: {response:?}"
            );
            assert!(codex_route, "{vendor}: Codex route missing");
            assert!(
                unverified,
                "{vendor}: replacement must record the unverified classification"
            );
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn hosted_muse_and_grok_replace_checks_peer_against_runtime_record() {
        use fl_core::state::{
            MergeMode, RuntimeEntry, SensitivityPatch, StatePayload, StateScope, UpsertStateArgs,
        };
        use fl_core::store::StateStore;
        use std::io::{BufRead, BufReader};
        use std::os::unix::process::CommandExt;
        for vendor in ["muse", "grok"] {
            let rig = rig();
            let vendor_bin = rig._tmp.path().join(vendor);
            std::os::unix::fs::symlink("/bin/sh", &vendor_bin).unwrap();
            let script = r#"sh -c 'echo HOOK:$$; sleep 60; :' & "$1" -c 'echo CHILD:$$; sleep 60; :' & wait"#;
            let mut child = std::process::Command::new(&vendor_bin)
                .args(["-c", script, "_", vendor_bin.to_str().unwrap()])
                .stdout(std::process::Stdio::piped())
                .process_group(0)
                .spawn()
                .unwrap();
            let parent_pid = child.id();
            let output = child.stdout.take().unwrap();
            let mut hook_pid = None;
            let mut child_pid = None;
            for line in BufReader::new(output).lines() {
                let line = line.unwrap();
                if let Some(v) = line.strip_prefix("HOOK:") {
                    hook_pid = v.parse::<u32>().ok();
                }
                if let Some(v) = line.strip_prefix("CHILD:") {
                    child_pid = v.parse::<u32>().ok();
                }
                if hook_pid.is_some() && child_pid.is_some() {
                    break;
                }
            }
            let hook_pid = hook_pid.unwrap();
            let child_pid = child_pid.unwrap();
            assert!(crate::identity::proc_is_vendor_cli(child_pid));
            rig.inner.store.upsert_state(UpsertStateArgs {
                scope: StateScope::Workspace, slug: format!("hosted-session-{vendor}-feira"),
                sensitivity: SensitivityPatch::Inherit,
                payload: StatePayload::Runtime(RuntimeEntry { value: serde_json::json!({
                    "session_id": format!("{vendor}:feira"), "persona": "feira", "vendor": vendor,
                    "status": "running", "hydration_generation": "gen-1", "pid": parent_pid,
                    "pid_start_time": crate::identity::proc_start_time_ticks(parent_pid)
                }), merge_mode: MergeMode::LastWriter }), at: At::from_unix_millis(1), by: "test".into(),
            }).unwrap();
            let route = match vendor {
                "muse" => VendorRoute::Muse {
                    session_id: "sid".into(),
                    cwd: None,
                    tmux_target: None,
                    tmux_pane: None,
                },
                _ => VendorRoute::Grok {
                    session_id: "sid".into(),
                    cwd: None,
                    tmux_target: None,
                    tmux_pane: None,
                },
            };
            let request = |with_hosted_id: bool| LocalRequest::RegisterRoute {
                persona: "feira".into(),
                route: route.clone(),
                explicit: true,
                replace: true,
                hosted_id: with_hosted_id.then(|| format!("{vendor}:feira")),
                hydration_generation: Some("gen-1".into()),
                hosted_session_verified: false,
                unverified_process: false,
            };
            let peer = |pid| PeerCred {
                pid: Some(pid),
                ppid: None,
                start_time: crate::identity::proc_start_time_ticks(pid),
            };
            let results = [true, false].map(|with_hosted_id| {
                let (ok, _) = rig
                    .inner
                    .handle_local_request_with_peer(request(with_hosted_id), peer(hook_pid));
                let (bad, _) = rig
                    .inner
                    .handle_local_request_with_peer(request(with_hosted_id), peer(child_pid));
                (ok, bad)
            });
            // A caller cannot bypass the trusted runtime PID by changing its claim.
            // The actual hosted process remains eligible for process verification.
            let mut altered_results = Vec::new();
            for (hosted_id, generation) in [
                (None, Some("stale-gen".to_owned())),
                (Some("malformed".to_owned()), None),
                (Some(format!("{vendor}:someone-else")), None),
            ] {
                let altered_request = || LocalRequest::RegisterRoute {
                    persona: "feira".into(),
                    route: route.clone(),
                    explicit: true,
                    replace: true,
                    hosted_id: hosted_id.clone(),
                    hydration_generation: generation.clone(),
                    hosted_session_verified: false,
                    unverified_process: false,
                };
                let (genuine, _) = rig
                    .inner
                    .handle_local_request_with_peer(altered_request(), peer(hook_pid));
                let (child, _) = rig
                    .inner
                    .handle_local_request_with_peer(altered_request(), peer(child_pid));
                altered_results.push((genuine, child));
            }
            unsafe {
                libc::kill(-(parent_pid as i32), libc::SIGKILL);
            }
            let _ = child.wait();
            for (genuine, child) in altered_results {
                assert!(
                    matches!(child, LocalResponse::Error { .. }),
                    "{vendor} altered claim: {child:?}"
                );
                // An independently checked genuine peer must never be mistaken for a child.
                // Other registration fences may still refuse a conflicting claim.
                if let LocalResponse::Error { message } = genuine {
                    assert!(
                        !message.contains("process_mismatch"),
                        "{vendor} genuine peer: {message}"
                    );
                }
            }
            for (ok, bad) in results {
                assert!(
                    matches!(ok, LocalResponse::RouteRegistered),
                    "{vendor}: {ok:?}"
                );
                assert!(
                    matches!(bad, LocalResponse::Error { .. }),
                    "{vendor}: {bad:?}"
                );
            }
        }
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
                replace: true,
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
        let (resp3, _) = rig.inner.handle_local_request_with_peer(
            LocalRequest::RegisterRoute {
                persona: "feira".into(),
                route: VendorRoute::Claude {
                    session_id: "sid-unverified".into(),
                    pid: challenger_pid,
                    socket_path: PathBuf::from(format!("/tmp/claude-{challenger_pid}.sock")),
                    key_path: None,
                },
                explicit: true,
                replace: true,
                hosted_id: None,
                hydration_generation: Some("gen-1".into()),
                hosted_session_verified: false,
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
            matches!(resp3, LocalResponse::Error { .. }),
            "a hosted claim cannot avoid the peer check by clearing verified"
        );
        assert!(
            matches!(resp2, LocalResponse::Error { .. }),
            "daemon must reject spoofed hosted_session_verified when PeerCred PID does not match live incumbent"
        );
    }

    #[test]
    fn register_route_ignores_updated_at_restamps_and_bounds_concurrent_incumbent_contention() {
        use crate::dispatch_diagnostics::{
            RegistrationConflictReason, RegistrationDiagnosticOutcome,
        };
        use std::sync::atomic::AtomicUsize;

        let rig = rig();

        let (resp1, _) = rig.inner.handle_local_request(LocalRequest::RegisterRoute {
            persona: "feira".into(),
            route: VendorRoute::Codex {
                thread_id: "thread-inc-0".into(),
                cwd: None,
            },
            explicit: true,
            replace: false,
            hosted_id: Some("codex:feira".into()),
            hydration_generation: Some("gen-1".into()),
            hosted_session_verified: true,
            unverified_process: true,
        });
        assert!(matches!(resp1, LocalResponse::RouteRegistered));

        // 1. Restamping `updated_at` and `identity_at` between the unlocked probe and lock
        // acquisition must NOT trigger a retry because route registration identity is unchanged.
        let restamp_calls = Arc::new(AtomicUsize::new(0));
        let restamp_calls_hook = Arc::clone(&restamp_calls);
        *lock(&rig.inner.registration_probe_hook) = Some(Box::new(move |routes| {
            restamp_calls_hook.fetch_add(1, Ordering::SeqCst);
            if let Some(entry) = routes.entries.get_mut("feira") {
                entry.updated_at = At::from_unix_millis(999_999);
                entry.identity_at = Some(At::from_unix_millis(888_888));
            }
        }));

        let (resp_restamp, _) = rig.inner.handle_local_request(LocalRequest::RegisterRoute {
            persona: "feira".into(),
            route: VendorRoute::Codex {
                thread_id: "thread-inc-restamp-ok".into(),
                cwd: None,
            },
            explicit: true,
            replace: false,
            hosted_id: Some("codex:feira".into()),
            hydration_generation: Some("gen-1".into()),
            hosted_session_verified: false,
            unverified_process: false,
        });
        assert!(matches!(resp_restamp, LocalResponse::RouteRegistered));
        assert_eq!(
            restamp_calls.load(Ordering::SeqCst),
            1,
            "updated_at restamp must not trigger retry passes"
        );

        // 2. When the incumbent's route identity changes on every attempt between the unlocked
        // probe and lock acquisition, `RegisterRoute` must return within the deadline with
        // `RegistrationContention`, apply nothing, and record a `RegistrationContention` diagnostic.
        let contention_calls = Arc::new(AtomicUsize::new(0));
        let contention_calls_hook = Arc::clone(&contention_calls);
        *lock(&rig.inner.registration_probe_hook) = Some(Box::new(move |routes| {
            let n = contention_calls_hook.fetch_add(1, Ordering::SeqCst) + 1;
            routes.upsert_explicit_with_meta(
                "feira",
                VendorRoute::Agy {
                    conversation_id: format!("conv-concurrent-{n}"),
                    ls_pid: Some(std::process::id()),
                    ls_address: None,
                    csrf_token: None,
                    agentapi_exe: None,
                },
                At::from_unix_millis(1000 + n as u64),
                Some("antigravity:feira".into()),
                Some(format!("gen-concurrent-{n}")),
                false,
            );
        }));

        let started = Instant::now();
        let (resp_contention, _) = rig.inner.handle_local_request(LocalRequest::RegisterRoute {
            persona: "feira".into(),
            route: VendorRoute::Claude {
                pid: std::process::id(),
                session_id: "claude-challenger".into(),
                socket_path: PathBuf::from("/tmp/claude-challenger.sock"),
                key_path: None,
            },
            explicit: true,
            replace: false,
            hosted_id: Some("claude:feira".into()),
            hydration_generation: Some("gen-challenger".into()),
            hosted_session_verified: true,
            unverified_process: false,
        });
        let elapsed = started.elapsed();
        *lock(&rig.inner.registration_probe_hook) = None;

        assert!(
            elapsed < REGISTER_ROUTE_DEADLINE,
            "contention bound must finish within deadline ({elapsed:?})"
        );
        assert_eq!(
            contention_calls.load(Ordering::SeqCst),
            MAX_REGISTER_ROUTE_ATTEMPTS
        );
        let LocalResponse::Error { message } = resp_contention else {
            panic!("expected contention error, got {resp_contention:?}");
        };
        assert!(
            message.contains("registration_contention"),
            "unexpected error message: {message}"
        );

        let routes = lock(&rig.inner.routes);
        let entry = routes.get("feira").expect("incumbent route preserved");
        assert!(
            matches!(
                &entry.route,
                VendorRoute::Agy { conversation_id, .. }
                    if conversation_id == &format!("conv-concurrent-{MAX_REGISTER_ROUTE_ATTEMPTS}")
            ),
            "challenger must not overwrite concurrently modified incumbent: {:?}",
            entry.route
        );
        let latest_diag = routes
            .registration_diagnostics
            .last()
            .expect("contention diagnostic recorded");
        assert_eq!(latest_diag.outcome, RegistrationDiagnosticOutcome::Refused);
        assert_eq!(
            latest_diag.reason,
            RegistrationConflictReason::RegistrationContention
        );
    }

    #[test]
    fn default_socket_path_with_precedence() {
        use std::ffi::OsStr;
        let tmp = TempDir::new().unwrap();
        let run_user = tmp.path().join("run-user-1000");
        fs::create_dir_all(&run_user).unwrap();
        let home = tmp.path().join("home");

        assert_eq!(
            default_socket_path_with(
                Some(OsStr::new("/xdg/rt")),
                Some(&run_user),
                Some(home.as_os_str()),
            ),
            PathBuf::from("/xdg/rt/fl-wire.sock")
        );
        assert_eq!(
            default_socket_path_with(None, Some(&run_user), Some(home.as_os_str())),
            run_user.join("fl-wire.sock")
        );
        assert_eq!(
            default_socket_path_with(
                None,
                Some(&tmp.path().join("missing")),
                Some(home.as_os_str()),
            ),
            home.join(".cache/fl/wire.sock")
        );
        assert_eq!(
            default_socket_path_with(None, None, None),
            PathBuf::from("/tmp/fl-wire.sock")
        );

        #[cfg(unix)]
        {
            let uid = unsafe { libc::geteuid() };
            let other_uid = uid.wrapping_add(1);
            let run_root = tmp.path().join("run/user");
            fs::create_dir_all(&run_root).unwrap();

            assert_eq!(
                default_socket_path_for_root(None, &run_root, Some(home.as_os_str()), uid),
                home.join(".cache/fl/wire.sock")
            );

            let my_dir = run_root.join(uid.to_string());
            fs::create_dir_all(&my_dir).unwrap();
            assert_eq!(
                default_socket_path_for_root(None, &run_root, Some(home.as_os_str()), uid),
                my_dir.join("fl-wire.sock")
            );

            let other_dir = run_root.join(other_uid.to_string());
            fs::create_dir_all(&other_dir).unwrap();
            assert_eq!(
                default_socket_path_for_root(None, &run_root, Some(home.as_os_str()), other_uid),
                home.join(".cache/fl/wire.sock")
            );

            fs::remove_dir(&my_dir).unwrap();
            let real_target = tmp.path().join("real-target");
            fs::create_dir_all(&real_target).unwrap();
            std::os::unix::fs::symlink(&real_target, &my_dir).unwrap();
            assert_eq!(
                default_socket_path_for_root(None, &run_root, Some(home.as_os_str()), uid),
                home.join(".cache/fl/wire.sock")
            );
        }
    }

    #[test]
    fn awaiting_daemon_store_marker_delivers_at_startup_on_poll_on_late_route_and_once_across_restart()
     {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(RedbStore::open(&tmp.path().join("queue.redb")).unwrap());
        // Delivery can reach the webhook before the daemon removes the durable marker.
        // Wait for the actual store condition instead of racing the dispatch worker.
        let wait_for_marker_clear = || {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if store.list_wire_awaiting_daemon().unwrap().is_empty() {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "daemon did not clear awaiting markers"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        let sock = tmp.path().join("daemon.sock");
        let routes_file = tmp.path().join("routes.json");
        RouteTable::default().save(Some(&routes_file)).unwrap();

        let hook_sock = tmp.path().join("felicia-hook.sock");
        let hook_listener = crate::test_isolation::bind_unix_fixture(&hook_sock).unwrap();
        hook_listener.set_nonblocking(true).unwrap();

        let base_ms = now_at().unix_millis();

        // 1. Pre-startup message NOT marked via `send_wire_awaiting_daemon` must NOT be replayed.
        let (ambiguous_msg, _) = store
            .send_wire(SendWireArgs {
                id: None,
                sender_id: None,
                sender_name: "felix".into(),
                sender_host: Some("node-a".into()),
                to: WireAddress::AgentName("felicia".into()),
                kind: MessageKind::Mail,
                urgency: MessageUrgency::Normal,
                durable: true,
                subject: "ambiguous".into(),
                body: "do not replay".into(),
                reply_to: None,
                thread_id: None,
                deadline_at: None,
                at: At::from_unix_millis(base_ms),
            })
            .unwrap();

        // 2. Message queued by offline client before daemon starts (and before route is registered).
        let pre_at = At::from_unix_millis(base_ms + 1);
        let (queued_pre, _) = store
            .send_wire_awaiting_daemon(SendWireArgs {
                id: None,
                sender_id: None,
                sender_name: "felix".into(),
                sender_host: Some("node-a".into()),
                to: WireAddress::AgentName("felicia".into()),
                kind: MessageKind::Mail,
                urgency: MessageUrgency::Normal,
                durable: true,
                subject: "queued-pre".into(),
                body: "deliver after route registration".into(),
                reply_to: None,
                thread_id: None,
                deadline_at: None,
                at: pre_at.clone(),
            })
            .unwrap();
        assert_eq!(
            store.list_wire_awaiting_daemon().unwrap(),
            vec![(queued_pre.id.clone(), pre_at.clone())]
        );

        // Stale marker (> 24h old) for a recipient that never registers is pruned at startup.
        let stale_at =
            At::from_unix_millis(base_ms.saturating_sub(MAX_AWAITING_DAEMON_AGE_MS + 1000));
        let (stale_unrouted, _) = store
            .send_wire_awaiting_daemon(SendWireArgs {
                id: None,
                sender_id: None,
                sender_name: "felix".into(),
                sender_host: Some("node-a".into()),
                to: WireAddress::AgentName("never-registers".into()),
                kind: MessageKind::Mail,
                urgency: MessageUrgency::Normal,
                durable: true,
                subject: "stale-unrouted".into(),
                body: "prune marker after 24h".into(),
                reply_to: None,
                thread_id: None,
                deadline_at: None,
                at: stale_at,
            })
            .unwrap();

        let mut handle = spawn_daemon(
            Arc::clone(&store),
            DaemonConfig {
                node_id: "node-q".into(),
                host: "host-q".into(),
                socket_path: sock.clone(),
                routes_path: Some(routes_file.clone()),
                home_override: Some(tmp.path().to_path_buf()),
                listen_addr: None,
                peers: vec![],
                psk: vec![],
            },
        )
        .unwrap();

        // Before `felicia` registers her route, `queued_pre`'s marker remains in the store,
        // while `stale_unrouted`'s expired marker was pruned.
        let _ = stale_unrouted;
        assert_eq!(
            store.list_wire_awaiting_daemon().unwrap(),
            vec![(queued_pre.id.clone(), pre_at)]
        );

        // Now register `felicia`'s webhook route: `queued_pre` must be delivered and marker cleared,
        // while `ambiguous_msg` stays Pending!
        let mut client = LocalClient::connect(&sock, Duration::from_secs(2)).unwrap();
        let resp = client
            .request(&LocalRequest::register(
                "felicia",
                VendorRoute::Webhook {
                    socket_path: hook_sock.clone(),
                },
                true,
            ))
            .unwrap();
        assert!(matches!(resp, LocalResponse::RouteRegistered));

        let deadline = Instant::now() + Duration::from_secs(3);
        let mut received_subjects = Vec::new();
        while Instant::now() < deadline && received_subjects.is_empty() {
            if let Ok((mut stream, _)) = hook_listener.accept() {
                let mut buf = String::new();
                let _ = stream.read_to_string(&mut buf);
                let val: serde_json::Value = serde_json::from_str(buf.trim()).unwrap();
                received_subjects.push(val["subject"].as_str().unwrap().to_string());
            } else {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        assert_eq!(received_subjects, vec!["queued-pre".to_string()]);
        wait_for_marker_clear();
        let amb_rcpts = store.list_wire_receipts(&ambiguous_msg.id).unwrap();
        assert_eq!(amb_rcpts[0].status, DeliveryStatus::Pending);
        assert_eq!(amb_rcpts[0].attempts, 0);

        // 3. A client whose socket path is wrong while the daemon runs elsewhere writes to the
        // shared store via `send_wire_awaiting_daemon`: the running daemon picks it up on poll,
        // delivers it, and clears the marker.
        let (queued_live, _) = store
            .send_wire_awaiting_daemon(SendWireArgs {
                id: None,
                sender_id: None,
                sender_name: "felix".into(),
                sender_host: Some("node-a".into()),
                to: WireAddress::AgentName("felicia".into()),
                kind: MessageKind::Mail,
                urgency: MessageUrgency::Normal,
                durable: true,
                subject: "queued-live".into(),
                body: "deliver on poll".into(),
                reply_to: None,
                thread_id: None,
                deadline_at: None,
                at: At::from_unix_millis(base_ms + 2),
            })
            .unwrap();
        let _ = queued_live;

        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && received_subjects.len() < 2 {
            if let Ok((mut stream, _)) = hook_listener.accept() {
                let mut buf = String::new();
                let _ = stream.read_to_string(&mut buf);
                let val: serde_json::Value = serde_json::from_str(buf.trim()).unwrap();
                received_subjects.push(val["subject"].as_str().unwrap().to_string());
            } else {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        assert_eq!(
            received_subjects,
            vec!["queued-pre".to_string(), "queued-live".to_string()]
        );
        wait_for_marker_clear();
        handle.stop();

        // 4. Crash/restart between store write and daemon pickup -> delivered once across restarts.
        let (_queued_restart, _) = store
            .send_wire_awaiting_daemon(SendWireArgs {
                id: None,
                sender_id: None,
                sender_name: "felix".into(),
                sender_host: Some("node-a".into()),
                to: WireAddress::AgentName("felicia".into()),
                kind: MessageKind::Mail,
                urgency: MessageUrgency::Normal,
                durable: true,
                subject: "queued-restart".into(),
                body: "deliver once after restart".into(),
                reply_to: None,
                thread_id: None,
                deadline_at: None,
                at: At::from_unix_millis(base_ms + 3),
            })
            .unwrap();
        assert_eq!(store.list_wire_awaiting_daemon().unwrap().len(), 1);

        let mut handle_1 = spawn_daemon(
            Arc::clone(&store),
            DaemonConfig {
                node_id: "node-q".into(),
                host: "host-q".into(),
                socket_path: sock.clone(),
                routes_path: Some(routes_file.clone()),
                home_override: Some(tmp.path().to_path_buf()),
                listen_addr: None,
                peers: vec![],
                psk: vec![],
            },
        )
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && received_subjects.len() < 3 {
            if let Ok((mut stream, _)) = hook_listener.accept() {
                let mut buf = String::new();
                let _ = stream.read_to_string(&mut buf);
                let val: serde_json::Value = serde_json::from_str(buf.trim()).unwrap();
                received_subjects.push(val["subject"].as_str().unwrap().to_string());
            } else {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        assert_eq!(
            received_subjects,
            vec![
                "queued-pre".to_string(),
                "queued-live".to_string(),
                "queued-restart".to_string(),
            ]
        );
        wait_for_marker_clear();
        handle_1.stop();

        // Starting a second daemon on the same store and routes must NOT re-deliver anything.
        let mut handle_2 = spawn_daemon(
            Arc::clone(&store),
            DaemonConfig {
                node_id: "node-q".into(),
                host: "host-q".into(),
                socket_path: sock,
                routes_path: Some(routes_file),
                home_override: Some(tmp.path().to_path_buf()),
                listen_addr: None,
                peers: vec![],
                psk: vec![],
            },
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(120));
        assert!(hook_listener.accept().is_err());
        handle_2.stop();
    }

    #[test]
    fn awaiting_daemon_keeps_marker_on_schedule_failure_and_skips_unrouted_polls_and_reports_status()
     {
        let (_tmp, inner) = fault_rig();
        let now_ms = now_at().unix_millis();

        // 1. Keep-on-failure: recipient "held" has an explicit route in `fault_rig()`.
        // Force `schedule_dispatch` to fail on worker spawn and verify the store marker is retained.
        let (msg_routed, _) = inner
            .store
            .send_wire_awaiting_daemon(SendWireArgs {
                id: None,
                sender_id: None,
                sender_name: "felix".into(),
                sender_host: Some("node-a".into()),
                to: WireAddress::AgentName("held".into()),
                kind: MessageKind::Mail,
                urgency: MessageUrgency::Normal,
                durable: true,
                subject: "routed-fail-spawn".into(),
                body: "keep marker when schedule_dispatch fails".into(),
                reply_to: None,
                thread_id: None,
                deadline_at: None,
                at: At::from_unix_millis(now_ms),
            })
            .unwrap();
        inner.fail_next_worker_spawn.store(true, Ordering::SeqCst);
        inner.drain_awaiting_daemon_messages(false);
        assert_eq!(
            inner.store.list_wire_awaiting_daemon().unwrap(),
            vec![(msg_routed.id.clone(), At::from_unix_millis(now_ms))],
            "marker must remain in store when schedule_dispatch fails"
        );
        inner
            .store
            .clear_wire_awaiting_daemon(&msg_routed.id)
            .unwrap();
        lock(&inner.awaiting_daemon_state).remove(&msg_routed.id);

        // 2. Unrouted recipient: first poll reads message + receipts, discovers no route, and
        // marks the entry waiting_for_route. Subsequent periodic polls (recheck_unrouted = false)
        // must skip get_wire_message and list_wire_receipts until routes change.
        let (_msg_unrouted, _) = inner
            .store
            .send_wire_awaiting_daemon(SendWireArgs {
                id: None,
                sender_id: None,
                sender_name: "felix".into(),
                sender_host: Some("node-a".into()),
                to: WireAddress::AgentName("pull-only-persona".into()),
                kind: MessageKind::Mail,
                urgency: MessageUrgency::Normal,
                durable: true,
                subject: "unrouted".into(),
                body: "skip store reads on subsequent periodic polls".into(),
                reply_to: None,
                thread_id: None,
                deadline_at: None,
                at: At::from_unix_millis(now_ms - 5_000),
            })
            .unwrap();
        inner.drain_awaiting_daemon_messages(false);
        assert_eq!(
            inner.store.list_wire_awaiting_daemon().unwrap().len(),
            1,
            "unrouted marker stays in store while awaiting a route"
        );

        // Status snapshot surfaces aged awaiting_daemon count and age for the recipient.
        let summaries = inner.pending_recipients_snapshot(&now_at());
        let pull_summary = summaries
            .iter()
            .find(|s| s.recipient == "pull-only-persona")
            .expect("pull-only-persona should appear in pending_recipients_snapshot");
        assert_eq!(pull_summary.awaiting_daemon_count, 1);
        assert!(pull_summary.oldest_awaiting_daemon_age_ms.unwrap_or(0) >= 5_000);

        // Wait for the initial unrouted worker thread to finish before measuring the second poll.
        let worker_deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < worker_deadline {
            let active = lock(&inner.dispatch_queues)
                .get("pull-only-persona")
                .is_some_and(|q| q.worker_active);
            if !active {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        // Clear recorded FaultStore calls and run another periodic poll (`recheck_unrouted = false`).
        lock(&inner.store.calls).clear();
        inner.drain_awaiting_daemon_messages(false);
        let calls_on_second_poll = lock(&inner.store.calls).clone();
        assert!(
            calls_on_second_poll.is_empty(),
            "periodic poll should skip get/receipts reads for unrouted markers when routes did not change, got {calls_on_second_poll:?}"
        );
    }

    #[test]
    fn composer_occupied_zero_key_hold_auto_retries_with_backoff_and_delivers_when_cleared() {
        let (tmp, inner) = fault_rig();
        let webhook_sock = tmp.path().join("held-webhook.sock");
        let webhook_listener = crate::test_isolation::bind_unix_fixture(&webhook_sock).unwrap();
        webhook_listener.set_nonblocking(true).unwrap();
        lock(&inner.routes).upsert_explicit(
            "held",
            VendorRoute::Webhook {
                socket_path: webhook_sock,
            },
            now_at(),
        );
        let occupied_err =
            "composer_occupied_by_non_wire_input: composer holds non-wire input or active dialog; holding delivery without sending keys"
                .to_string();

        let id = with_transport(Err(occupied_err), || run_fault_job(&inner, "held"));

        // 1. Pre-paste occupied composer leaves receipt `Pending` with `attempts == 0` and fences `held`.
        let rcpts = inner.store.list_wire_receipts(&id).unwrap();
        assert_eq!(rcpts.len(), 1);
        assert_eq!(rcpts[0].status, DeliveryStatus::Pending);
        assert_eq!(
            rcpts[0].attempts, 0,
            "zero-key pre-paste occupied hold must leave attempts at 0"
        );
        assert!(lock(&inner.overflow_recipients).contains("held"));
        {
            let reprobes = lock(&inner.composer_occupied_reprobes);
            let entry = reprobes
                .get("held")
                .expect("zero-key occupied hold must schedule background reprobe");
            assert_eq!(entry.backoff, Duration::from_secs(10));
            assert_eq!(entry.next_probe_at.unix_millis(), 1 + 10_000);
            assert_eq!(entry.probes_run, 0);
        }

        // 2. While composer remains occupied (`Some(false)`), polls advance exponential backoff
        //    `10s -> 20s -> 40s -> 80s -> 120s` (capped at 2m) and send zero keys.
        let ready_flag = Arc::new(AtomicBool::new(false));
        let ready_for_hook = Arc::clone(&ready_flag);
        *lock(&inner.composer_probe_hook) = Some(Box::new(move |_, _| {
            Some(ready_for_hook.load(Ordering::SeqCst))
        }));

        // Before initial 10s backoff expires -> no probe run.
        inner.poll_composer_occupied_holds(At::from_unix_millis(10_000));
        assert_eq!(
            lock(&inner.composer_occupied_reprobes)["held"].probes_run,
            0
        );
        assert!(webhook_listener.accept().is_err());

        // Expected schedule starting at t = 1:
        // Probe 1 at 10_001 -> backoff becomes 20s, next_probe_at = 30_001
        // Probe 2 at 30_001 -> backoff becomes 40s, next_probe_at = 70_001
        // Probe 3 at 70_001 -> backoff becomes 80s, next_probe_at = 150_001
        // Probe 4 at 150_001 -> backoff becomes 120s (capped), next_probe_at = 270_001
        let expected = [
            (10_001u64, 1u32, Duration::from_secs(20), 30_001u64),
            (30_001u64, 2u32, Duration::from_secs(40), 70_001u64),
            (70_001u64, 3u32, Duration::from_secs(80), 150_001u64),
            (150_001u64, 4u32, Duration::from_secs(120), 270_001u64),
        ];
        for (poll_ms, exp_probes, exp_backoff, exp_next_ms) in expected {
            inner.poll_composer_occupied_holds(At::from_unix_millis(poll_ms));
            let entry = lock(&inner.composer_occupied_reprobes)["held"].clone();
            assert_eq!(entry.probes_run, exp_probes);
            assert_eq!(entry.backoff, exp_backoff);
            assert_eq!(entry.next_probe_at.unix_millis(), exp_next_ms);
            assert!(
                webhook_listener.accept().is_err(),
                "occupied composer probe must send zero keys"
            );
            assert!(lock(&inner.overflow_recipients).contains("held"));
        }

        // 3. Human clears draft / submits their turn (`ready_flag = true`). Next due poll at
        //    270_001 unfences `held` and delivers the held message cleanly with `attempts == 1`.
        ready_flag.store(true, Ordering::SeqCst);
        inner.poll_composer_occupied_holds(At::from_unix_millis(270_001));

        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let rcpts = inner.store.list_wire_receipts(&id).unwrap();
            if rcpts[0].status == DeliveryStatus::Acked {
                assert_eq!(
                    rcpts[0].attempts, 1,
                    "auto-retried delivery after zero-key hold must complete with attempts == 1"
                );
                break;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for auto-retried delivery of {id}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            webhook_listener.accept().is_ok(),
            "expected exactly one delivery after composer became ready"
        );
        assert!(
            !lock(&inner.overflow_recipients).contains("held"),
            "recipient must be unfenced after auto-retry succeeds"
        );
        assert!(
            !lock(&inner.composer_occupied_reprobes).contains_key("held"),
            "reprobe state must be cleared once delivery resolves"
        );
    }

    #[test]
    fn composer_occupied_unobservable_probe_none_fails_closed_and_sends_no_keys() {
        let (_tmp, inner) = fault_rig();
        let occupied_err =
            "composer_occupied_by_non_wire_input: composer holds non-wire input or active dialog; holding delivery without sending keys"
                .to_string();
        let id = with_transport(Err(occupied_err), || run_fault_job(&inner, "held"));
        let rcpts = inner.store.list_wire_receipts(&id).unwrap();
        assert_eq!(rcpts[0].status, DeliveryStatus::Pending);
        assert_eq!(rcpts[0].attempts, 0);
        assert!(lock(&inner.overflow_recipients).contains("held"));

        // Probe cannot observe the composer (`None`): fence must stay, no dispatch, no keys.
        *lock(&inner.composer_probe_hook) = Some(Box::new(|_, _| None));
        with_transport(Ok(()), || {
            inner.poll_composer_occupied_holds(At::from_unix_millis(60_000));
            assert_eq!(
                TRANSPORT_CALLS.with(|n| n.get()),
                0,
                "unobservable composer probe (None) must not re-dispatch or send keys"
            );
            assert!(lock(&inner.overflow_recipients).contains("held"));
        });
        let rcpts = inner.store.list_wire_receipts(&id).unwrap();
        assert_eq!(rcpts[0].status, DeliveryStatus::Pending);
        assert_eq!(rcpts[0].attempts, 0);
    }

    #[test]
    fn composer_occupied_route_missing_probe_counts_toward_max_and_logs() {
        let (_tmp, inner) = fault_rig();
        let occupied_err =
            "composer_occupied_by_non_wire_input: composer holds non-wire input or active dialog; holding delivery without sending keys"
                .to_string();
        let _id = with_transport(Err(occupied_err), || run_fault_job(&inner, "held"));
        assert!(lock(&inner.composer_occupied_reprobes).contains_key("held"));
        // Route disappears before the first reprobe is due.
        lock(&inner.routes).entries.remove("held");
        let _ = take_warnings();

        inner.poll_composer_occupied_holds(At::from_unix_millis(10_001));
        assert_eq!(
            lock(&inner.composer_occupied_reprobes)["held"].probes_run,
            1,
            "route-missing probe must count toward COMPOSER_OCCUPIED_MAX_REPROBES"
        );
        let warnings = take_warnings();
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("composer-occupied reprobe 1/")
                    && w.contains("held")
                    && w.contains("no route")),
            "route-missing probe must be logged via warn, got {warnings:?}"
        );

        // Drive the entry to exhaustion following its own schedule; further polls do nothing.
        loop {
            let entry = lock(&inner.composer_occupied_reprobes)["held"].clone();
            if entry.probes_run >= COMPOSER_OCCUPIED_MAX_REPROBES {
                break;
            }
            inner.poll_composer_occupied_holds(entry.next_probe_at.clone());
        }
        assert_eq!(
            lock(&inner.composer_occupied_reprobes)["held"].probes_run,
            COMPOSER_OCCUPIED_MAX_REPROBES
        );
        let entry = lock(&inner.composer_occupied_reprobes)["held"].clone();
        inner.poll_composer_occupied_holds(entry.next_probe_at.clone());
        assert_eq!(
            lock(&inner.composer_occupied_reprobes)["held"].probes_run,
            COMPOSER_OCCUPIED_MAX_REPROBES,
            "exhausted route-missing entry must not probe further"
        );
    }

    #[test]
    fn composer_occupied_exhausted_log_carries_only_recognised_payload() {
        assert_eq!(COMPOSER_OCCUPIED_MAX_REPROBES, 16);
        for (err_text, recognised) in [
            (
                "composer_occupied_by_non_wire_input: composer_occupied: composer holds \"/clear\"; repair: Backspace, then Ctrl-U; holding delivery without sending keys",
                Some("/clear"),
            ),
            (
                "composer_occupied_by_non_wire_input: composer holds owner secret draft; holding delivery without sending keys",
                None,
            ),
        ] {
            let (_tmp, inner) = fault_rig();
            let _id = with_transport(Err(err_text.to_string()), || run_fault_job(&inner, "held"));
            {
                let entry = &lock(&inner.composer_occupied_reprobes)["held"];
                assert_eq!(entry.recognised_payload, recognised, "{err_text}");
            }
            lock(&inner.routes).entries.remove("held");
            let _ = take_warnings();
            loop {
                let entry = lock(&inner.composer_occupied_reprobes)["held"].clone();
                if entry.probes_run >= COMPOSER_OCCUPIED_MAX_REPROBES {
                    break;
                }
                inner.poll_composer_occupied_holds(entry.next_probe_at.clone());
            }
            let warnings = take_warnings();
            let exhausted: Vec<_> = warnings
                .iter()
                .filter(|w| w.contains("reprobes exhausted"))
                .collect();
            assert_eq!(exhausted.len(), 1, "{warnings:?}");
            if let Some(payload) = recognised {
                assert!(
                    exhausted[0].contains(&format!("holds \"{payload}\"")),
                    "{exhausted:?}"
                );
                assert!(
                    exhausted[0].contains("Backspace, then Ctrl-U"),
                    "{exhausted:?}"
                );
            } else {
                assert!(!exhausted[0].contains("holds \""), "{exhausted:?}");
                assert!(
                    !exhausted[0].contains("owner secret draft"),
                    "{exhausted:?}"
                );
            }
        }
    }

    #[test]
    fn composer_occupied_fresh_hold_does_not_rearm_exhausted_reprobe_entry() {
        let (_tmp, inner) = fault_rig();
        let occupied_err =
            "composer_occupied_by_non_wire_input: composer holds non-wire input or active dialog; holding delivery without sending keys"
                .to_string();
        let _id = with_transport(Err(occupied_err), || run_fault_job(&inner, "held"));
        {
            let mut reprobes = lock(&inner.composer_occupied_reprobes);
            let entry = reprobes.get_mut("held").expect("reprobe entry");
            entry.probes_run = COMPOSER_OCCUPIED_MAX_REPROBES;
            entry.backoff = COMPOSER_OCCUPIED_REPROBE_MAX_BACKOFF;
            entry.next_probe_at = At::from_unix_millis(999_999);
        }
        // A fresh zero-key hold (e.g. from a reprobe's own dispatch) must NOT reset an
        // exhausted entry, or the 16-probe bound defeats itself.
        inner.schedule_composer_occupied_reprobe("held", &At::from_unix_millis(500_000), None);
        let entry = lock(&inner.composer_occupied_reprobes)["held"].clone();
        assert_eq!(
            entry.probes_run, COMPOSER_OCCUPIED_MAX_REPROBES,
            "fresh hold must not reset exhausted entry"
        );
        // Only an explicit reset (new message / read / ack) re-arms it.
        inner.reset_composer_occupied_reprobe("held", &At::from_unix_millis(500_000));
        let entry = lock(&inner.composer_occupied_reprobes)["held"].clone();
        assert_eq!(
            entry.probes_run, 0,
            "explicit reset must re-arm exhausted entry"
        );
        assert_eq!(entry.backoff, COMPOSER_OCCUPIED_REPROBE_INITIAL_BACKOFF);
        assert_eq!(entry.next_probe_at.unix_millis(), 510_000);

        // A non-exhausted entry is only rescheduled, not reset.
        {
            let mut reprobes = lock(&inner.composer_occupied_reprobes);
            reprobes.get_mut("held").unwrap().probes_run = 3;
        }
        inner.schedule_composer_occupied_reprobe("held", &At::from_unix_millis(600_000), None);
        let entry = lock(&inner.composer_occupied_reprobes)["held"].clone();
        assert_eq!(entry.probes_run, 3, "live entry must keep its probe count");
    }

    #[test]
    fn composer_occupied_new_hold_overwrites_recognised_payload() {
        let (_tmp, inner) = fault_rig();
        let clear_err = "composer_occupied_by_non_wire_input: composer_occupied: composer holds \"/clear\"; repair: Backspace, then Ctrl-U; holding delivery without sending keys".to_string();
        let _id = with_transport(Err(clear_err), || run_fault_job(&inner, "held"));
        assert_eq!(
            lock(&inner.composer_occupied_reprobes)["held"].recognised_payload,
            Some("/clear")
        );
        // Unrecognised hold clears the stale "/clear".
        inner.schedule_composer_occupied_reprobe("held", &At::from_unix_millis(500_000), None);
        assert_eq!(
            lock(&inner.composer_occupied_reprobes)["held"].recognised_payload,
            None,
            "unrecognised hold must clear stale recognised payload"
        );
        // Recognised hold overwrites with the new payload.
        inner.schedule_composer_occupied_reprobe(
            "held",
            &At::from_unix_millis(600_000),
            Some("/new"),
        );
        assert_eq!(
            lock(&inner.composer_occupied_reprobes)["held"].recognised_payload,
            Some("/new")
        );
        inner.schedule_composer_occupied_reprobe("held", &At::from_unix_millis(700_000), None);
        assert_eq!(
            lock(&inner.composer_occupied_reprobes)["held"].recognised_payload,
            None
        );
    }

    #[test]
    fn composer_occupied_reset_preserves_recognised_payload_through_exhaustion() {
        let (_tmp, inner) = fault_rig();
        let clear_err = "composer_occupied_by_non_wire_input: composer_occupied: composer holds \"/clear\"; repair: Backspace, then Ctrl-U; holding delivery without sending keys".to_string();
        let _id = with_transport(Err(clear_err), || run_fault_job(&inner, "held"));
        assert_eq!(
            lock(&inner.composer_occupied_reprobes)["held"].recognised_payload,
            Some("/clear")
        );
        // Reset (new message / read / ack) re-arms but must not erase the payload.
        inner.reset_composer_occupied_reprobe("held", &At::from_unix_millis(500_000));
        {
            let entry = &lock(&inner.composer_occupied_reprobes)["held"];
            assert_eq!(entry.probes_run, 0);
            assert_eq!(entry.recognised_payload, Some("/clear"));
        }
        lock(&inner.routes).entries.remove("held");
        let _ = take_warnings();
        loop {
            let entry = lock(&inner.composer_occupied_reprobes)["held"].clone();
            if entry.probes_run >= COMPOSER_OCCUPIED_MAX_REPROBES {
                break;
            }
            inner.poll_composer_occupied_holds(entry.next_probe_at.clone());
        }
        let warnings = take_warnings();
        let exhausted: Vec<_> = warnings
            .iter()
            .filter(|w| w.contains("reprobes exhausted"))
            .collect();
        assert_eq!(exhausted.len(), 1, "{warnings:?}");
        assert!(exhausted[0].contains("; holds \"/clear\""), "{exhausted:?}");
    }

    #[test]
    fn composer_occupied_with_nonzero_attempts_never_auto_retries_without_explicit_ack() {
        let (_tmp, inner) = fault_rig();
        let post_paste_occupied_err =
            "composer_occupied_by_non_wire_input: composer holds non-wire input after paste; aborting Enter retries"
                .to_string();

        let id = with_transport(Err(post_paste_occupied_err), || {
            run_fault_job(&inner, "held")
        });

        let rcpts = inner.store.list_wire_receipts(&id).unwrap();
        assert_eq!(rcpts[0].status, DeliveryStatus::Pending);
        assert_eq!(
            rcpts[0].attempts, 1,
            "post-paste occupied error must record attempts == 1 because keys were sent"
        );
        assert!(lock(&inner.overflow_recipients).contains("held"));
        assert!(
            !lock(&inner.composer_occupied_reprobes).contains_key("held"),
            "unconfirmed paste (attempts > 0) must never schedule background auto-retry"
        );

        *lock(&inner.composer_probe_hook) = Some(Box::new(|_, _| Some(true)));
        with_transport(Ok(()), || {
            inner.poll_composer_occupied_holds(At::from_unix_millis(60_000));
            assert_eq!(
                TRANSPORT_CALLS.with(|n| n.get()),
                0,
                "unconfirmed paste (attempts > 0) must not be re-dispatched by background poll"
            );
            assert!(lock(&inner.overflow_recipients).contains("held"));
        });
    }
}

#[cfg(all(test, target_os = "linux"))]
mod hydration_peer_review_tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    #[test]
    fn verified_replacement_ignores_unrelated_incumbent_but_not_child_caller() {
        let tmp = tempfile::tempdir().unwrap();
        for vendor in ["claude", "agy"] {
            let bin = tmp.path().join(vendor);
            std::os::unix::fs::symlink("/bin/sh", &bin).unwrap();
            let mut process = Command::new(&bin).args(["-c", "sh -c 'echo HOOK:$$; sleep 60; :' & \"$1\" -c 'echo CHILD:$$; sleep 60; :' & wait", "_", bin.to_str().unwrap()])
                .stdout(Stdio::piped()).process_group(0).spawn().unwrap();
            let pid = process.id();
            let mut hook = None;
            let mut child = None;
            for line in BufReader::new(process.stdout.take().unwrap()).lines() {
                let line = line.unwrap();
                if let Some(id) = line.strip_prefix("HOOK:") {
                    hook = id.parse::<u32>().ok();
                }
                if let Some(id) = line.strip_prefix("CHILD:") {
                    child = id.parse::<u32>().ok();
                }
                if hook.is_some() && child.is_some() {
                    break;
                }
            }
            let route = if vendor == "claude" {
                VendorRoute::Claude {
                    pid,
                    session_id: "new".into(),
                    socket_path: tmp.path().join("new.sock"),
                    key_path: None,
                }
            } else {
                VendorRoute::Agy {
                    conversation_id: "new".into(),
                    ls_pid: Some(std::process::id()),
                    ls_address: None,
                    csrf_token: None,
                    agentapi_exe: None,
                }
            };
            let mut table = crate::identity::RouteTable::default();
            table.upsert_explicit_with_meta(
                "ada",
                VendorRoute::Claude {
                    pid: std::process::id(),
                    session_id: "old".into(),
                    socket_path: tmp.path().join("old.sock"),
                    key_path: None,
                },
                At::from_unix_millis(1),
                None,
                None,
                false,
            );
            let incumbent = table.get("ada");
            let peer = |pid| PeerCred {
                pid: Some(pid),
                ppid: None,
                start_time: crate::identity::proc_start_time_ticks(pid),
            };
            let reference = Some((pid, crate::identity::proc_start_time_ticks(pid)));
            let genuine = verify_peer_cred_for_hosted_claim(
                peer(hook.unwrap()),
                reference,
                true,
                &route,
                Some("gen"),
                incumbent,
                false,
            );
            let nested = verify_peer_cred_for_hosted_claim(
                peer(child.unwrap()),
                reference,
                true,
                &route,
                Some("gen"),
                incumbent,
                false,
            );
            let untrusted = verify_peer_cred_for_hosted_claim(
                peer(hook.unwrap()),
                None,
                true,
                &route,
                Some("gen"),
                incumbent,
                false,
            );
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
            let _ = process.wait();
            assert!(genuine, "{vendor} genuine replacement");
            assert!(!nested, "{vendor} nested child");
            assert!(
                !untrusted,
                "{vendor} incumbent requires independently proven replacement"
            );
        }
    }
}
