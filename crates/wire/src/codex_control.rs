//! Minimal client for the Codex app-server control socket.
//!
//! `codex queue --message "/compact"` injects the literal text as a chat turn; it does not run
//! the TUI slash command. Compaction is an app-server RPC (`thread/compact/start`), reachable
//! over the app-server's control socket, which speaks JSON-RPC over a WebSocket (RFC 6455) on a
//! Unix stream. This module is that client: handshake, masked text frames, ping/pong, close and
//! fragmented messages, with an overall deadline. Nothing here falls back to `codex queue`.
//!
//! The wire shapes follow `codex app-server generate-json-schema`: JSON-RPC messages carry no
//! `"jsonrpc"` member, `initialize` takes `clientInfo {name, version}`, `initialized` is a
//! notification, and `thread/compact/start` takes `{threadId}`. Active-turn steering follows
//! the generated Codex 0.166.0 schema: metadata-only `thread/read`, latest `thread/turns/list`,
//! then `turn/steer` with an `expectedTurnId` precondition. Schema and mock coverage do not
//! establish that an installed daemon exposes or accepts these methods at runtime.

use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use ring::rand::{SecureRandom, SystemRandom};
use serde_json::{Value, json};

use crate::ws::{WsError, compute_accept_key, read_http_headers};

/// Overall budget for one compaction request (connect through the `thread/compact/start` reply).
pub(crate) const COMPACT_TIMEOUT: Duration = Duration::from_secs(10);
/// Overall budget for active-turn socket connect, preflight, and acknowledgment.
pub(crate) const STEER_TIMEOUT: Duration = Duration::from_secs(10);
/// Longest a single socket read or write may block.
const IO_TIMEOUT: Duration = Duration::from_secs(5);
/// Largest data frame or reassembled message accepted from the server.
const MAX_MESSAGE: usize = 1024 * 1024;
/// Longest RPC error text carried into an `Err`.
const MAX_ERROR_TEXT: usize = 300;
/// The control socket, relative to the Codex home directory.
const SOCKET_REL: &str = "app-server-control/app-server-control.sock";

const OP_CONTINUATION: u8 = 0x0;
const OP_TEXT: u8 = 0x1;
const OP_BINARY: u8 = 0x2;
const OP_CLOSE: u8 = 0x8;
const OP_PING: u8 = 0x9;
const OP_PONG: u8 = 0xA;

/// Where `/compact` goes. `Ok(None)` means "use `codex queue`" (the `FL_WIRE_CODEX_BIN` test
/// override is set and there is no control-socket override). Empty values count as unset,
/// except `codex_bin`, which is "set" whenever present (as the queue path treats it).
pub(crate) fn resolve_control_socket(
    sock_override: Option<&OsStr>,
    codex_bin: Option<&OsStr>,
    codex_home: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Result<Option<PathBuf>, String> {
    fn non_empty(v: Option<&OsStr>) -> Option<&OsStr> {
        v.filter(|s| !s.is_empty())
    }
    if let Some(sock) = non_empty(sock_override) {
        return Ok(Some(PathBuf::from(sock)));
    }
    if codex_bin.is_some() {
        return Ok(None);
    }
    if let Some(dir) = non_empty(codex_home) {
        let dir = Path::new(dir);
        // A relative path would resolve against the daemon's working directory, not Codex's.
        if !dir.is_absolute() {
            return Err(format!(
                "CODEX_HOME must be an absolute path, got `{}`",
                dir.display()
            ));
        }
        return Ok(Some(dir.join(SOCKET_REL)));
    }
    if let Some(dir) = non_empty(home) {
        return Ok(Some(PathBuf::from(dir).join(".codex").join(SOCKET_REL)));
    }
    Err("cannot locate the Codex app-server control socket: \
         set FL_WIRE_CODEX_CONTROL_SOCK, CODEX_HOME or HOME"
        .to_string())
}

/// Ask the Codex app-server behind `sock` to compact `thread_id`. Returns once the server has
/// accepted the request (`thread/compact/start` replied with a result).
pub(crate) fn compact_thread(
    sock: &Path,
    thread_id: &str,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    // Known limit: a Unix-socket `connect` has no timeout (it blocks only on a full backlog).
    let stream = UnixStream::connect(sock).map_err(|e| {
        format!(
            "cannot connect to the Codex app-server control socket {}: {e}",
            sock.display()
        )
    })?;
    let mut conn = Conn {
        stream,
        rbuf: Vec::new(),
        rng: SystemRandom::new(),
        deadline,
    };
    conn.handshake()?;

    conn.send_json(&json!({
        "id": 1,
        "method": "initialize",
        "params": { "clientInfo": { "name": "ferroloop", "version": "0.1.0" } },
    }))?;
    conn.await_response(1, "initialize")?;
    conn.send_json(&json!({ "method": "initialized", "params": {} }))?;
    conn.send_json(&json!({
        "id": 2,
        "method": "thread/compact/start",
        "params": { "threadId": thread_id },
    }))?;
    let outcome = conn.await_response(2, "thread/compact/start").map(|_| ());
    // Best effort: the outcome is already decided.
    let _ = conn.send(OP_CLOSE, &1000u16.to_be_bytes());
    outcome
}

/// An app-server acknowledgment that an existing turn accepted steering input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexSteerAck {
    pub turn_id: String,
}

/// The operation whose outcome failed to be established.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexSteerStage {
    Route,
    Connect,
    Handshake,
    Initialize,
    ThreadRead,
    TurnsList,
    Steer,
}

/// Machine-readable preflight or protocol refusal; it never authorizes automatic delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexSteerRefusalReason {
    Idle,
    NotLoaded,
    DirectInputUnavailable,
    NotActive,
    NoActiveTurn,
    InvalidResponse,
    Unsupported,
    Rejected,
    Transport,
}

/// Whether a steering failure is known to have avoided accepting input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexSteerError {
    /// Preflight failed, or the server rejected steering with JSON-RPC invalid request,
    /// method-not-found, or invalid-params. This does not authorize an automatic retry.
    Refused {
        stage: CodexSteerStage,
        reason: CodexSteerRefusalReason,
        message: String,
    },
    /// A steer write may have reached the server, but its acknowledgment cannot be confirmed.
    /// Callers must not automatically retry or enqueue the same input.
    Unconfirmed {
        stage: CodexSteerStage,
        message: String,
    },
}

impl CodexSteerError {
    pub fn stage(&self) -> CodexSteerStage {
        match self {
            Self::Refused { stage, .. } | Self::Unconfirmed { stage, .. } => *stage,
        }
    }

    pub fn refusal_reason(&self) -> Option<CodexSteerRefusalReason> {
        match self {
            Self::Refused { reason, .. } => Some(*reason),
            Self::Unconfirmed { .. } => None,
        }
    }

    pub(crate) fn refused(
        stage: CodexSteerStage,
        reason: CodexSteerRefusalReason,
        message: impl Into<String>,
    ) -> Self {
        Self::Refused {
            stage,
            reason,
            message: message.into(),
        }
    }

    fn failure(
        stage: CodexSteerStage,
        reason: CodexSteerRefusalReason,
        message: impl Into<String>,
    ) -> Self {
        if stage == CodexSteerStage::Steer {
            Self::Unconfirmed {
                stage,
                message: message.into(),
            }
        } else {
            Self::refused(stage, reason, message)
        }
    }
}

impl std::fmt::Display for CodexSteerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused { stage, message, .. } => {
                write!(f, "Codex steering refused at {stage:?}: {message}")
            }
            Self::Unconfirmed { stage, message } => {
                write!(f, "Codex steering unconfirmed at {stage:?}: {message}")
            }
        }
    }
}

impl std::error::Error for CodexSteerError {}

/// Steer only the latest proven active turn. No method here loads/resumes a thread, starts a
/// turn, modifies a queue, or retries steering. The server's `expectedTurnId` precondition must
/// reject a turn that changes between preflight and the steering request.
pub(crate) fn steer_active_turn(
    sock: &Path,
    thread_id: &str,
    text: &str,
    timeout: Duration,
) -> Result<CodexSteerAck, CodexSteerError> {
    use CodexSteerRefusalReason as Reason;
    use CodexSteerStage as Stage;
    let deadline = Instant::now() + timeout;
    let stream = connect_for_steering(sock, deadline).map_err(|e| {
        CodexSteerError::refused(
            Stage::Connect,
            Reason::Transport,
            format!(
                "cannot connect to the Codex app-server control socket {}: {e}",
                sock.display()
            ),
        )
    })?;
    let mut conn = Conn {
        stream,
        rbuf: Vec::new(),
        rng: SystemRandom::new(),
        deadline,
    };
    conn.handshake().map_err(|message| {
        CodexSteerError::refused(Stage::Handshake, Reason::Transport, message)
    })?;
    conn.send_json(&json!({
        "id": 1, "method": "initialize",
        "params": {"clientInfo": {"name": "ferroloop", "version": "0.1.0"}},
    }))
    .map_err(|message| CodexSteerError::refused(Stage::Initialize, Reason::Transport, message))?;
    conn.await_steering_response(1, "initialize", Stage::Initialize)?;
    conn.send_json(&json!({"method": "initialized", "params": {}}))
        .map_err(|message| {
            CodexSteerError::refused(Stage::Initialize, Reason::Transport, message)
        })?;
    conn.send_json(&json!({
        "id": 2, "method": "thread/read",
        "params": {"threadId": thread_id, "includeTurns": false},
    }))
    .map_err(|message| CodexSteerError::refused(Stage::ThreadRead, Reason::Transport, message))?;
    let metadata = conn.await_steering_response(2, "thread/read", Stage::ThreadRead)?;
    let thread = &metadata["thread"];
    if thread_id.trim().is_empty() || thread["id"].as_str() != Some(thread_id) {
        return Err(CodexSteerError::refused(
            Stage::ThreadRead,
            Reason::InvalidResponse,
            "thread/read did not prove the requested thread identity",
        ));
    }
    let refusal = match thread["status"]["type"].as_str() {
        Some("notLoaded") => Some(Reason::NotLoaded),
        Some("active" | "idle") if thread["canAcceptDirectInput"].as_bool() != Some(true) => {
            Some(Reason::DirectInputUnavailable)
        }
        Some("idle") => Some(Reason::Idle),
        Some("systemError") => Some(Reason::NotActive),
        Some("active") => None,
        _ => Some(Reason::InvalidResponse),
    };
    if let Some(reason) = refusal {
        return Err(CodexSteerError::refused(
            Stage::ThreadRead,
            reason,
            "thread/read did not prove an active thread accepting direct input",
        ));
    }
    conn.send_json(&json!({
        "id": 3, "method": "thread/turns/list",
        "params": {"threadId": thread_id, "limit": 1, "sortDirection": "desc", "itemsView": "notLoaded"},
    })).map_err(|message| CodexSteerError::refused(Stage::TurnsList, Reason::Transport, message))?;
    let turns = conn.await_steering_response(3, "thread/turns/list", Stage::TurnsList)?;
    let latest = turns["data"]
        .as_array()
        .filter(|turns| turns.len() == 1)
        .and_then(|turns| turns.first())
        .filter(|turn| turn["status"].as_str() == Some("inProgress"));
    let turn_id = latest
        .and_then(|turn| turn["id"].as_str())
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| {
            CodexSteerError::refused(
                Stage::TurnsList,
                Reason::NoActiveTurn,
                "thread/turns/list did not prove one latest active turn",
            )
        })?;
    // From the first write attempt onward, a transport failure cannot prove non-delivery.
    conn.send_json(&json!({
        "id": 4, "method": "turn/steer",
        "params": {"threadId": thread_id, "expectedTurnId": turn_id, "input": [{"type": "text", "text": text}]},
    })).map_err(|message| CodexSteerError::failure(Stage::Steer, Reason::Transport, message))?;
    let outcome = conn
        .await_steering_response(4, "turn/steer", Stage::Steer)
        .and_then(|ack| {
            if ack["turnId"].as_str() != Some(turn_id)
                || ack
                    .get("threadId")
                    .is_some_and(|id| id.as_str() != Some(thread_id))
            {
                return Err(CodexSteerError::failure(
                    Stage::Steer,
                    Reason::InvalidResponse,
                    "turn/steer acknowledgment did not match the requested thread and active turn",
                ));
            }
            Ok(CodexSteerAck {
                turn_id: turn_id.to_owned(),
            })
        });
    // A close failure cannot change the acknowledgment's meaning.
    let _ = conn.send(OP_CLOSE, &1000u16.to_be_bytes());
    outcome
}

/// Nonblocking connect + deadline-bounded poll, with descriptor ownership held by `Socket`
/// throughout. A full Unix backlog may fail immediately; it is never retried. Keep the
/// compaction path unchanged. `peer_addr` prevents an unconnected writable socket from being
/// mistaken for success on platforms whose backlog handling differs.
fn connect_for_steering(sock: &Path, deadline: Instant) -> io::Result<UnixStream> {
    use socket2::{Domain, SockAddr, Socket, Type};
    let remaining = || {
        deadline
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
            .ok_or_else(|| io::Error::from(io::ErrorKind::TimedOut))
    };
    remaining()?;
    let address = SockAddr::unix(sock)?;
    let socket = Socket::new(Domain::UNIX, Type::STREAM, None)?;
    socket.connect_timeout(&address, remaining()?)?;
    if let Some(error) = socket.take_error()? {
        return Err(error);
    }
    socket.peer_addr()?;
    remaining()?;
    let descriptor: std::os::fd::OwnedFd = socket.into();
    Ok(UnixStream::from(descriptor))
}

/// Reads from a socket under an overall deadline: before every read it re-arms the socket read
/// timeout to the time left (at most `IO_TIMEOUT`) and fails with `TimedOut` once none is left.
struct DeadlineReader<'a> {
    stream: &'a UnixStream,
    deadline: Instant,
}

impl Read for DeadlineReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        self.stream.set_read_timeout(Some(left.min(IO_TIMEOUT)))?;
        self.stream.read(buf)
    }
}

struct Conn {
    stream: UnixStream,
    /// Bytes read but not yet forming a whole frame.
    rbuf: Vec<u8>,
    rng: SystemRandom,
    deadline: Instant,
}

impl Conn {
    /// Time left before the overall deadline, capped at one socket operation's budget.
    fn io_budget(&self) -> Result<Duration, String> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(timed_out());
        }
        Ok(left.min(IO_TIMEOUT))
    }

    fn handshake(&mut self) -> Result<(), String> {
        let mut nonce = [0u8; 16];
        self.rng
            .fill(&mut nonce)
            .map_err(|_| "random number generator failure".to_string())?;
        let key = BASE64.encode(nonce);
        let request = format!(
            "GET / HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\n\
             Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        );
        self.stream
            .set_write_timeout(Some(self.io_budget()?))
            .map_err(io_err)?;
        self.stream
            .write_all(request.as_bytes())
            .map_err(|e| io_failure("sending the WebSocket handshake", &e))?;
        // `read_http_headers` reads one byte at a time; the reader re-arms the socket timeout
        // before every read so a slow drip cannot outlast the overall deadline.
        let mut reader = DeadlineReader {
            stream: &self.stream,
            deadline: self.deadline,
        };
        let headers = read_http_headers(&mut reader).map_err(|e| match e {
            WsError::Io(e) => io_failure("reading the WebSocket handshake reply", &e),
            e => format!("Codex app-server WebSocket handshake failed: {e}"),
        })?;

        let mut lines = headers.split("\r\n");
        let status_line = lines.next().unwrap_or_default();
        if status_line.split_whitespace().nth(1) != Some("101") {
            return Err(format!(
                "Codex app-server refused the WebSocket upgrade: {}",
                truncate(status_line, MAX_ERROR_TEXT)
            ));
        }
        let accept = lines.find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("sec-websocket-accept")
                .then(|| value.trim().to_string())
        });
        match accept {
            Some(accept) if accept == compute_accept_key(&key) => Ok(()),
            Some(_) => Err("Codex app-server sent a wrong Sec-WebSocket-Accept".to_string()),
            None => Err("Codex app-server upgrade reply has no Sec-WebSocket-Accept".to_string()),
        }
    }

    fn send_json(&mut self, value: &Value) -> Result<(), String> {
        let bytes = serde_json::to_vec(value).map_err(|e| format!("cannot encode request: {e}"))?;
        self.send(OP_TEXT, &bytes)
    }

    fn send(&mut self, opcode: u8, payload: &[u8]) -> Result<(), String> {
        let mut mask = [0u8; 4];
        self.rng
            .fill(&mut mask)
            .map_err(|_| "random number generator failure".to_string())?;
        let frame = encode_client_frame(opcode, payload, mask);
        self.stream
            .set_write_timeout(Some(self.io_budget()?))
            .map_err(io_err)?;
        self.stream
            .write_all(&frame)
            .and_then(|()| self.stream.flush())
            .map_err(|e| io_failure("writing to the Codex app-server", &e))
    }

    fn read_frame(&mut self) -> Result<Frame, String> {
        loop {
            if let Some((frame, used)) = decode_server_frame(&self.rbuf)? {
                self.rbuf.drain(..used);
                return Ok(frame);
            }
            self.stream
                .set_read_timeout(Some(self.io_budget()?))
                .map_err(io_err)?;
            let mut chunk = [0u8; 8192];
            match self.stream.read(&mut chunk) {
                Ok(0) => return Err("Codex app-server closed the connection".to_string()),
                Ok(n) => self.rbuf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(io_failure("reading from the Codex app-server", &e)),
            }
        }
    }

    /// Next whole text message: answers pings, skips pongs and binary messages, reassembles
    /// fragments, and fails on a close frame.
    fn recv_text(&mut self) -> Result<String, String> {
        // (is_text, bytes) of the fragmented message being assembled.
        let mut partial: Option<(bool, Vec<u8>)> = None;
        loop {
            self.io_budget()?;
            let frame = self.read_frame()?;
            let (is_text, bytes) = match frame.opcode {
                OP_PING => {
                    self.send(OP_PONG, &frame.payload)?;
                    continue;
                }
                OP_PONG => continue,
                OP_CLOSE => return Err(close_error(&frame.payload)),
                OP_TEXT | OP_BINARY => {
                    if partial.is_some() {
                        return Err("Codex app-server started a new message inside a \
                                    fragmented one"
                            .to_string());
                    }
                    if !frame.fin {
                        partial = Some((frame.opcode == OP_TEXT, frame.payload));
                        continue;
                    }
                    (frame.opcode == OP_TEXT, frame.payload)
                }
                _ => {
                    let Some((is_text, mut bytes)) = partial.take() else {
                        return Err("Codex app-server sent a stray continuation frame".to_string());
                    };
                    if bytes.len() + frame.payload.len() > MAX_MESSAGE {
                        return Err(format!(
                            "Codex app-server message exceeds {MAX_MESSAGE} bytes"
                        ));
                    }
                    bytes.extend_from_slice(&frame.payload);
                    if !frame.fin {
                        partial = Some((is_text, bytes));
                        continue;
                    }
                    (is_text, bytes)
                }
            };
            if !is_text {
                continue;
            }
            return String::from_utf8(bytes)
                .map_err(|_| "Codex app-server sent a text message that is not UTF-8".to_string());
        }
    }

    /// Steering fails closed on malformed messages or unrelated response IDs. Compaction keeps
    /// its existing tolerant reader below. Only protocol validation errors establish refusal;
    /// unknown/internal errors after a steer write may follow partial handling.
    fn await_steering_response(
        &mut self,
        id: i64,
        method: &str,
        stage: CodexSteerStage,
    ) -> Result<Value, CodexSteerError> {
        use CodexSteerRefusalReason as Reason;
        loop {
            let text = self
                .recv_text()
                .map_err(|message| CodexSteerError::failure(stage, Reason::Transport, message))?;
            let malformed = || {
                CodexSteerError::failure(
                    stage,
                    Reason::InvalidResponse,
                    format!("Codex app-server sent a malformed response to `{method}`"),
                )
            };
            let message: Value = serde_json::from_str(&text).map_err(|_| malformed())?;
            if !message.is_object() {
                return Err(malformed());
            }
            if let Some(method) = message.get("method") {
                if method.is_string()
                    && message.get("result").is_none()
                    && message.get("error").is_none()
                {
                    continue; // Server notification or request, never an acknowledgment.
                }
                return Err(malformed());
            }
            if message.get("id").and_then(Value::as_i64) != Some(id) {
                return Err(CodexSteerError::failure(
                    stage,
                    Reason::InvalidResponse,
                    format!("Codex app-server response ID did not match `{method}`"),
                ));
            }
            return match (
                message.get("error").filter(|error| !error.is_null()),
                message.get("result"),
            ) {
                (Some(error), None) => {
                    let description = format!(
                        "Codex app-server rejected `{method}`: {}",
                        describe_rpc_error(error)
                    );
                    let code = error.get("code").and_then(Value::as_i64);
                    let reason = if code == Some(-32601) {
                        Reason::Unsupported
                    } else {
                        Reason::Rejected
                    };
                    if error.get("message").is_some_and(Value::is_string)
                        && matches!(code, Some(-32602..=-32600))
                    {
                        Err(CodexSteerError::refused(stage, reason, description))
                    } else {
                        Err(CodexSteerError::failure(stage, reason, description))
                    }
                }
                (None, Some(result)) if result.is_object() => Ok(result.clone()),
                _ => Err(malformed()),
            };
        }
    }

    /// Read messages until the JSON-RPC response to request `id` arrives. Notifications, server
    /// requests (they carry a `method`) and responses to other ids are ignored.
    fn await_response(&mut self, id: i64, method: &str) -> Result<Value, String> {
        loop {
            let text = self.recv_text()?;
            let Ok(message) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if message.get("method").is_some()
                || message.get("id").and_then(Value::as_i64) != Some(id)
            {
                continue;
            }
            // A null `error` is no error; success needs a present `result` (`{}` is one).
            return match (
                message.get("error").filter(|e| !e.is_null()),
                message.get("result"),
            ) {
                (Some(error), _) => Err(format!(
                    "Codex app-server rejected `{method}`: {}",
                    describe_rpc_error(error)
                )),
                (None, Some(_)) => Ok(message),
                (None, None) => Err(format!(
                    "Codex app-server sent a malformed response to `{method}`: \
                     neither `result` nor `error`"
                )),
            };
        }
    }
}

/// Error text built only from the server's `message` (string) and `code` (number); `data` and
/// anything else is dropped, control characters are stripped, and the length is bounded.
fn describe_rpc_error(error: &Value) -> String {
    let message: Option<String> = error
        .get("message")
        .and_then(Value::as_str)
        .map(|m| m.chars().filter(|c| !c.is_control()).collect());
    let code = error.get("code").and_then(Value::as_i64);
    let text = match (message.filter(|m| !m.is_empty()), code) {
        (Some(message), Some(code)) => format!("{message} (code {code})"),
        (Some(message), None) => message,
        (None, Some(code)) => format!("(code {code})"),
        (None, None) => "no message".to_string(),
    };
    truncate(&text, MAX_ERROR_TEXT)
}

fn close_error(payload: &[u8]) -> String {
    match payload {
        [hi, lo, ..] => format!(
            "Codex app-server closed the connection (code {})",
            u16::from_be_bytes([*hi, *lo])
        ),
        _ => "Codex app-server closed the connection".to_string(),
    }
}

fn truncate(text: &str, max_chars: usize) -> String {
    let mut out: String = text.chars().take(max_chars).collect();
    if text.chars().count() > max_chars {
        out.push_str("...");
    }
    out
}

fn timed_out() -> String {
    "timed out waiting for the Codex app-server".to_string()
}

fn io_err(e: io::Error) -> String {
    format!("Codex app-server socket error: {e}")
}

fn io_failure(doing: &str, e: &io::Error) -> String {
    match e.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => timed_out(),
        _ => format!("error {doing}: {e}"),
    }
}

/// One decoded WebSocket frame.
#[derive(Debug, PartialEq, Eq)]
struct Frame {
    fin: bool,
    opcode: u8,
    payload: Vec<u8>,
}

/// Encode one final (FIN) client frame, masked with `mask` as RFC 6455 section 5.3 requires.
fn encode_client_frame(opcode: u8, payload: &[u8], mask: [u8; 4]) -> Vec<u8> {
    let mut out = Vec::with_capacity(14 + payload.len());
    out.push(0x80 | opcode);
    let len = payload.len();
    if len < 126 {
        out.push(0x80 | len as u8);
    } else if let Ok(len) = u16::try_from(len) {
        out.push(0x80 | 126);
        out.extend_from_slice(&len.to_be_bytes());
    } else {
        out.push(0x80 | 127);
        out.extend_from_slice(&(len as u64).to_be_bytes());
    }
    out.extend_from_slice(&mask);
    out.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i & 3]));
    out
}

/// Decode the server frame at the start of `buf`: `Ok(None)` when more bytes are needed, else
/// the frame and the bytes it used. Whatever the header alone shows to be invalid (masked,
/// reserved bits, bad control frame, too large) is refused before the payload is awaited.
fn decode_server_frame(buf: &[u8]) -> Result<Option<(Frame, usize)>, String> {
    if buf.len() < 2 {
        return Ok(None);
    }
    if buf[0] & 0x70 != 0 {
        return Err("Codex app-server frame has reserved bits set".to_string());
    }
    let fin = buf[0] & 0x80 != 0;
    let opcode = buf[0] & 0x0F;
    if !matches!(
        opcode,
        OP_CONTINUATION | OP_TEXT | OP_BINARY | OP_CLOSE | OP_PING | OP_PONG
    ) {
        return Err(format!("Codex app-server sent unknown opcode 0x{opcode:x}"));
    }
    if buf[1] & 0x80 != 0 {
        return Err("Codex app-server sent a masked frame".to_string());
    }
    let mut pos = 2usize;
    let mut len = u64::from(buf[1] & 0x7F);
    if len == 126 {
        if buf.len() < pos + 2 {
            return Ok(None);
        }
        len = u64::from(u16::from_be_bytes([buf[pos], buf[pos + 1]]));
        pos += 2;
    } else if len == 127 {
        if buf.len() < pos + 8 {
            return Ok(None);
        }
        let mut ext = [0u8; 8];
        ext.copy_from_slice(&buf[pos..pos + 8]);
        len = u64::from_be_bytes(ext);
        pos += 8;
    }
    if opcode & 0x8 != 0 && (!fin || len > 125) {
        return Err("Codex app-server sent a fragmented or oversize control frame".to_string());
    }
    if len > MAX_MESSAGE as u64 {
        return Err(format!(
            "Codex app-server frame of {len} bytes exceeds {MAX_MESSAGE}"
        ));
    }
    let len = len as usize;
    if buf.len() < pos + len {
        return Ok(None);
    }
    let frame = Frame {
        fin,
        opcode,
        payload: buf[pos..pos + len].to_vec(),
    };
    Ok(Some((frame, pos + len)))
}

/// A scripted WebSocket server on a Unix socket, shared by this module's tests and the
/// dispatcher's end-to-end test.
#[cfg(test)]
pub(crate) mod fake_server {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::thread::JoinHandle;

    /// A client frame as the server saw it.
    #[derive(Debug, Clone)]
    pub(crate) struct Received {
        pub(crate) masked: bool,
        pub(crate) opcode: u8,
        pub(crate) payload: Vec<u8>,
    }

    impl Received {
        pub(crate) fn json(&self) -> Value {
            serde_json::from_slice(&self.payload).expect("client text frame is JSON")
        }
    }

    pub(crate) struct ServerConn {
        stream: UnixStream,
        rbuf: Vec<u8>,
        pub(crate) log: Vec<Received>,
    }

    impl ServerConn {
        /// Complete the upgrade with a correct `Sec-WebSocket-Accept`.
        pub(crate) fn handshake(&mut self) {
            let key = self.read_upgrade_request();
            let accept = compute_accept_key(&key);
            self.write_raw(
                format!(
                    "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\
                     Connection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
                )
                .as_bytes(),
            );
        }

        /// Read and validate the client's upgrade request; returns its `Sec-WebSocket-Key`.
        pub(crate) fn read_upgrade_request(&mut self) -> String {
            let request = read_http_headers(&mut self.stream).expect("upgrade request");
            let mut lines = request.split("\r\n");
            assert_eq!(lines.next(), Some("GET / HTTP/1.1"));
            let header = |name: &str| -> String {
                request
                    .split("\r\n")
                    .find_map(|l| {
                        let (n, v) = l.split_once(':')?;
                        n.trim()
                            .eq_ignore_ascii_case(name)
                            .then(|| v.trim().to_string())
                    })
                    .unwrap_or_else(|| panic!("missing header {name}"))
            };
            assert_eq!(header("Host"), "localhost");
            assert_eq!(header("Upgrade"), "websocket");
            assert_eq!(header("Connection"), "Upgrade");
            assert_eq!(header("Sec-WebSocket-Version"), "13");
            let key = header("Sec-WebSocket-Key");
            assert_eq!(BASE64.decode(&key).expect("base64 key").len(), 16);
            key
        }

        pub(crate) fn write_raw(&mut self, bytes: &[u8]) {
            // The client may already have hung up (timeout tests); its verdict is what counts.
            let _ = self
                .stream
                .write_all(bytes)
                .and_then(|()| self.stream.flush());
        }

        /// Send one unmasked server frame (the length form follows the payload size).
        pub(crate) fn send_frame(&mut self, fin: bool, opcode: u8, payload: &[u8]) {
            let mut out = vec![if fin { 0x80 } else { 0 } | opcode];
            if payload.len() < 126 {
                out.push(payload.len() as u8);
            } else if let Ok(n) = u16::try_from(payload.len()) {
                out.push(126);
                out.extend_from_slice(&n.to_be_bytes());
            } else {
                out.push(127);
                out.extend_from_slice(&(payload.len() as u64).to_be_bytes());
            }
            out.extend_from_slice(payload);
            self.write_raw(&out);
        }

        pub(crate) fn send_text(&mut self, text: &str) {
            self.send_frame(true, OP_TEXT, text.as_bytes());
        }

        /// Read one client frame (independent of the production encoder) and log it.
        pub(crate) fn read_client_frame(&mut self) -> Option<Received> {
            loop {
                if let Some((received, used)) = parse_client_frame(&self.rbuf) {
                    self.rbuf.drain(..used);
                    self.log.push(received.clone());
                    return Some(received);
                }
                let mut chunk = [0u8; 4096];
                match self.stream.read(&mut chunk) {
                    Ok(0) | Err(_) => return None,
                    Ok(n) => self.rbuf.extend_from_slice(&chunk[..n]),
                }
            }
        }

        /// Read the next client text frame as JSON, skipping nothing.
        pub(crate) fn expect_json(&mut self) -> Value {
            let frame = self.read_client_frame().expect("client frame");
            assert_eq!(frame.opcode, OP_TEXT, "expected a text frame");
            frame.json()
        }

        /// Block until the client disconnects.
        pub(crate) fn drain_until_eof(&mut self) {
            while self.read_client_frame().is_some() {}
        }
    }

    fn parse_client_frame(buf: &[u8]) -> Option<(Received, usize)> {
        if buf.len() < 2 {
            return None;
        }
        assert_eq!(buf[0] & 0x80, 0x80, "client frames are single (FIN) frames");
        let opcode = buf[0] & 0x0F;
        let masked = buf[1] & 0x80 != 0;
        let mut pos = 2;
        let mut len = (buf[1] & 0x7F) as usize;
        if len == 126 {
            if buf.len() < 4 {
                return None;
            }
            len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
            pos = 4;
        } else if len == 127 {
            if buf.len() < 10 {
                return None;
            }
            len = u64::from_be_bytes(buf[2..10].try_into().unwrap()) as usize;
            pos = 10;
        }
        let mut mask = [0u8; 4];
        if masked {
            if buf.len() < pos + 4 {
                return None;
            }
            mask.copy_from_slice(&buf[pos..pos + 4]);
            pos += 4;
        }
        if buf.len() < pos + len {
            return None;
        }
        let mut payload = buf[pos..pos + len].to_vec();
        if masked {
            for (i, b) in payload.iter_mut().enumerate() {
                *b ^= mask[i & 3];
            }
        }
        Some((
            Received {
                masked,
                opcode,
                payload,
            },
            pos + len,
        ))
    }

    /// Bind `dir/codex.sock` and run `script` against the first connection on a thread. The
    /// handle yields every client frame the server read.
    pub(crate) fn spawn(
        dir: &Path,
        script: impl FnOnce(&mut ServerConn) + Send + 'static,
    ) -> (PathBuf, JoinHandle<Vec<Received>>) {
        let sock = dir.join("codex.sock");
        let listener = UnixListener::bind(&sock).expect("bind fake control socket");
        let handle = std::thread::spawn(move || {
            // Bounded, so a client that never connects fails the test instead of hanging it.
            listener
                .set_nonblocking(true)
                .expect("nonblocking listener");
            let give_up = Instant::now() + Duration::from_secs(3);
            let stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < give_up,
                            "no client connected to the fake server"
                        );
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(e) => panic!("accept: {e}"),
                }
            };
            stream.set_nonblocking(false).expect("blocking stream");
            stream
                .set_read_timeout(Some(Duration::from_secs(15)))
                .expect("server read timeout");
            let mut conn = ServerConn {
                stream,
                rbuf: Vec::new(),
                log: Vec::new(),
            };
            script(&mut conn);
            conn.log
        });
        (sock, handle)
    }
}

#[cfg(test)]
mod tests {
    use super::fake_server::{ServerConn, spawn};
    use super::*;
    use std::ffi::OsString;
    use tempfile::TempDir;

    const SHORT: Duration = Duration::from_secs(10);

    #[test]
    fn steering_connect_refuses_an_expired_deadline_without_connecting() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("expired.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let error = connect_for_steering(&path, Instant::now()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        listener.set_nonblocking(true).unwrap();
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn steering_connect_reports_a_local_refused_socket() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("refused.sock");
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        let error = connect_for_steering(&path, Instant::now() + SHORT).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::ConnectionRefused);
    }

    // Linux Unix sockets report EAGAIN on a full backlog without initiating a connection.
    // The helper must fail promptly rather than block or mistake writability for success.
    #[cfg(target_os = "linux")]
    #[test]
    fn steering_connect_is_bounded_when_the_local_backlog_is_full() {
        use socket2::{Domain, SockAddr, Socket, Type};
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("backlog.sock");
        let address = SockAddr::unix(&path).unwrap();
        let listener = Socket::new(Domain::UNIX, Type::STREAM, None).unwrap();
        listener.bind(&address).unwrap();
        listener.listen(0).unwrap();
        let filler = UnixStream::connect(&path).unwrap();
        // Release the backlog eventually even if a regression makes connect block.
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(400));
            listener.accept().unwrap();
        });
        let started = Instant::now();
        let result = connect_for_steering(&path, started + Duration::from_millis(50));
        let elapsed = started.elapsed();
        release.join().unwrap();
        drop(filler);
        assert!(
            result.is_err(),
            "full backlog must not yield a connected stream"
        );
        assert!(
            elapsed < Duration::from_millis(250),
            "connect took {elapsed:?}"
        );
    }

    // These fixtures mirror the required fields of the generated Codex 0.166.0 schemas.
    // They establish our boundary contract, not runtime vendor conformance.
    fn active_thread_reply() -> Value {
        json!({"id": 2, "result": {"thread": {
            "id": "thread-9", "sessionId": "session-9", "cliVersion": "0.166.0",
            "createdAt": 1, "updatedAt": 2, "cwd": "/tmp/mock-project", "ephemeral": false,
            "modelProvider": "openai", "preview": "fixture", "projectId": null,
            "source": "cli", "status": {"type": "active", "activeFlags": []},
            "canAcceptDirectInput": true, "turns": []
        }}})
    }

    fn active_turn_reply() -> Value {
        json!({"id": 3, "result": {"data": [{
            "id": "turn-7", "status": "inProgress", "items": [], "itemsView": "notLoaded"
        }], "nextCursor": null, "backwardsCursor": null}})
    }

    fn steer_exchange(
        replies: Vec<String>,
        text: &str,
        timeout: Duration,
    ) -> (
        Result<CodexSteerAck, CodexSteerError>,
        Vec<fake_server::Received>,
    ) {
        let dir = TempDir::new().unwrap();
        let (sock, server) = spawn(dir.path(), move |c| {
            c.handshake();
            for (index, reply) in replies.iter().enumerate() {
                c.expect_json();
                c.send_text(reply);
                if index == 0 {
                    // Initialized notification follows a successful initialization.
                    c.expect_json();
                }
            }
            c.drain_until_eof();
        });
        let result = steer_active_turn(&sock, "thread-9", text, timeout);
        (result, server.join().unwrap())
    }

    fn steer_replies() -> Vec<String> {
        vec![
            json!({"id": 1, "result": {}}).to_string(),
            active_thread_reply().to_string(),
            active_turn_reply().to_string(),
            json!({"id": 4, "result": {"turnId": "turn-7"}}).to_string(),
        ]
    }

    fn methods(log: &[fake_server::Received]) -> Vec<String> {
        log.iter()
            .filter(|frame| frame.opcode == OP_TEXT)
            .map(|frame| frame.json()["method"].as_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn steering_proves_active_thread_and_turn_and_preserves_escaped_input() {
        let text = "owner says \"stop\"\nnext line\t\\ café";
        let dir = TempDir::new().unwrap();
        let (sock, server) = spawn(dir.path(), |c| {
            c.handshake();
            for (index, reply) in steer_replies().iter().enumerate() {
                c.expect_json();
                c.send_text(r#"{"method":"thread/status/changed","params":{"threadId":"thread-9","status":{"type":"active","activeFlags":[]}}}"#);
                c.send_text(reply);
                if index == 0 {
                    c.expect_json();
                }
            }
            c.drain_until_eof();
        });
        let result = steer_active_turn(&sock, "thread-9", text, SHORT);
        assert_eq!(
            result.unwrap(),
            CodexSteerAck {
                turn_id: "turn-7".into()
            }
        );
        let log = server.join().unwrap();
        assert_eq!(
            methods(&log),
            [
                "initialize",
                "initialized",
                "thread/read",
                "thread/turns/list",
                "turn/steer"
            ]
        );
        assert!(log.iter().all(|frame| frame.masked));
        assert_eq!(
            log[2].json()["params"],
            json!({"threadId": "thread-9", "includeTurns": false})
        );
        assert_eq!(
            log[3].json()["params"],
            json!({"threadId": "thread-9", "limit": 1, "sortDirection": "desc", "itemsView": "notLoaded"})
        );
        assert_eq!(
            log[4].json()["params"],
            json!({"threadId": "thread-9", "expectedTurnId": "turn-7", "input": [{"type": "text", "text": text}]})
        );
    }

    #[test]
    fn steering_idle_refusal_is_typed_and_distinct_from_other_inactive_states() {
        for (state, direct_input, reason) in [
            ("idle", true, CodexSteerRefusalReason::Idle),
            (
                "idle",
                false,
                CodexSteerRefusalReason::DirectInputUnavailable,
            ),
            ("notLoaded", true, CodexSteerRefusalReason::NotLoaded),
            ("systemError", true, CodexSteerRefusalReason::NotActive),
            (
                "futureState",
                true,
                CodexSteerRefusalReason::InvalidResponse,
            ),
        ] {
            let mut thread = active_thread_reply();
            thread["result"]["thread"]["status"] = json!({"type": state});
            thread["result"]["thread"]["canAcceptDirectInput"] = json!(direct_input);
            let replies = vec![json!({"id":1,"result":{}}).to_string(), thread.to_string()];
            let (result, log) = steer_exchange(replies, "hello", SHORT);
            let error = result.unwrap_err();
            assert_eq!(error.stage(), CodexSteerStage::ThreadRead);
            assert_eq!(error.refusal_reason(), Some(reason));
            assert_eq!(methods(&log), ["initialize", "initialized", "thread/read"]);
        }
    }

    #[test]
    fn steering_refuses_unproven_thread_state_before_requesting_turns() {
        for (field, value) in [
            ("id", json!("other-thread")),
            ("id", Value::Null),
            ("status", json!({"type": "idle"})),
            ("status", json!({"type": "notLoaded"})),
            ("status", json!({"type": "systemError"})),
            ("status", json!({"type": "futureState"})),
            ("status", Value::Null),
            ("canAcceptDirectInput", json!(false)),
            ("canAcceptDirectInput", Value::Null),
            ("canAcceptDirectInput", json!("true")),
        ] {
            let mut thread = active_thread_reply();
            thread["result"]["thread"][field] = value;
            let replies = vec![
                json!({"id": 1, "result": {}}).to_string(),
                thread.to_string(),
            ];
            let (result, log) = steer_exchange(replies, "hello", SHORT);
            assert!(
                matches!(result, Err(CodexSteerError::Refused { .. })),
                "{field}: {result:?}"
            );
            assert_eq!(methods(&log), ["initialize", "initialized", "thread/read"]);
        }
    }

    #[test]
    fn steering_refuses_missing_or_non_active_latest_turn_without_steering() {
        for data in [
            json!([]),
            Value::Null,
            json!([{"id": "turn-7", "status": "completed", "items": []}]),
            json!([{"id": "turn-7", "status": "interrupted", "items": []}]),
            json!([{"id": "turn-7", "status": "failed", "items": []}]),
            json!([{"id": "turn-7", "status": "futureState", "items": []}]),
            json!([{"id": "", "status": "inProgress", "items": []}]),
            json!([{"id": " ", "status": "inProgress", "items": []}]),
            json!([{"status": "inProgress", "items": []}]),
            json!([{"id": "turn-7", "status": "inProgress", "items": []}, {"id": "turn-8", "status": "inProgress", "items": []}]),
        ] {
            let mut reply = active_turn_reply();
            reply["result"]["data"] = data;
            let mut replies = steer_replies();
            replies.truncate(3);
            replies[2] = reply.to_string();
            let (result, log) = steer_exchange(replies, "hello", SHORT);
            assert!(
                matches!(result, Err(CodexSteerError::Refused { .. })),
                "{result:?}"
            );
            assert_eq!(
                methods(&log),
                [
                    "initialize",
                    "initialized",
                    "thread/read",
                    "thread/turns/list"
                ]
            );
        }
    }

    #[test]
    fn steering_rpc_errors_never_retry_and_only_validation_rejection_is_refused() {
        for (stage, code, message) in [
            (1, -32601, "method unsupported"),
            (2, -32601, "turn listing unsupported"),
            (3, -32601, "steering unsupported"),
            (3, -32602, "invalid parameters"),
            (3, -32603, "internal error"),
            (3, -32000, "expected turn is stale"),
            (3, -32000, "no active turn"),
        ] {
            let mut replies = steer_replies();
            replies.truncate(stage + 1);
            replies[stage] =
                json!({"id": stage + 1, "error": {"code": code, "message": message}}).to_string();
            let (result, log) = steer_exchange(replies, "hello", SHORT);
            match result {
                Err(CodexSteerError::Refused { message: error, .. })
                    if stage < 3 || matches!(code, -32601 | -32602) =>
                {
                    assert!(error.contains(message), "{error}")
                }
                Err(CodexSteerError::Unconfirmed { message: error, .. })
                    if stage == 3 && !matches!(code, -32601 | -32602) =>
                {
                    assert!(error.contains(message), "{error}")
                }
                result => panic!("{code}: {result:?}"),
            }
            assert_eq!(methods(&log).len(), stage + 2);
            assert_eq!(
                methods(&log)
                    .iter()
                    .filter(|method| *method == "turn/steer")
                    .count(),
                usize::from(stage == 3)
            );
        }
    }

    #[test]
    fn steering_malformed_or_mismatched_ack_is_unconfirmed_without_retry() {
        for reply in [
            "not json",
            r#"{"id":99,"result":{"turnId":"turn-7"}}"#,
            r#"{"id":"4","result":{"turnId":"turn-7"}}"#,
            r#"{"id":4,"result":{"turnId":"other-turn"}}"#,
            r#"{"id":4,"result":{"turnId":""}}"#,
            r#"{"id":4,"result":{"threadId":"other-thread","turnId":"turn-7"}}"#,
            r#"{"id":4,"result":{}}"#,
            r#"{"id":4,"result":null}"#,
            r#"{"id":4}"#,
            r#"{"id":4,"result":{"turnId":"turn-7"},"error":{"code":1,"message":"ambiguous"}}"#,
        ] {
            let mut replies = steer_replies();
            replies[3] = reply.into();
            let (result, log) = steer_exchange(replies, "hello", Duration::from_millis(250));
            assert!(
                matches!(result, Err(CodexSteerError::Unconfirmed { .. })),
                "{reply}: {result:?}"
            );
            assert_eq!(
                methods(&log),
                [
                    "initialize",
                    "initialized",
                    "thread/read",
                    "thread/turns/list",
                    "turn/steer"
                ]
            );
        }
    }

    #[test]
    fn steering_preflight_malformed_or_wrong_rpc_id_is_refused_without_mutation() {
        for reply in [
            "not json",
            r#"{"id":99,"result":{}}"#,
            r#"{"id":2}"#,
            r#"{"id":2,"result":null}"#,
        ] {
            let mut replies = steer_replies();
            replies.truncate(2);
            replies[1] = reply.into();
            let (result, log) = steer_exchange(replies, "hello", Duration::from_millis(250));
            assert!(
                matches!(result, Err(CodexSteerError::Refused { .. })),
                "{reply}: {result:?}"
            );
            assert_eq!(methods(&log), ["initialize", "initialized", "thread/read"]);
        }
    }

    #[test]
    fn steering_timeout_before_mutation_is_refused_and_after_write_is_unconfirmed() {
        for stage in [1, 2, 3] {
            let mut replies = steer_replies();
            replies.truncate(stage); // Server drains the next request without replying.
            let started = Instant::now();
            let (result, log) = steer_exchange(replies, "hello", Duration::from_millis(150));
            assert!(started.elapsed() < Duration::from_secs(3));
            assert_eq!(
                result.as_ref().unwrap_err().stage(),
                match stage {
                    1 => CodexSteerStage::ThreadRead,
                    2 => CodexSteerStage::TurnsList,
                    _ => CodexSteerStage::Steer,
                }
            );
            match result {
                Err(CodexSteerError::Refused { message: error, .. }) if stage < 3 => {
                    assert!(error.contains("timed out"), "{error}")
                }
                Err(CodexSteerError::Unconfirmed { message: error, .. }) if stage == 3 => {
                    assert!(error.contains("timed out"), "{error}")
                }
                result => panic!("stage {stage}: {result:?}"),
            }
            assert_eq!(methods(&log).len(), stage + 2);
        }
    }

    fn os(s: &str) -> OsString {
        OsString::from(s)
    }

    // ---- frame codec -------------------------------------------------------------------

    #[test]
    fn encode_matches_the_rfc_6455_masked_hello_example() {
        let frame = encode_client_frame(OP_TEXT, b"Hello", [0x37, 0xfa, 0x21, 0x3d]);
        assert_eq!(
            frame,
            [
                0x81, 0x85, 0x37, 0xfa, 0x21, 0x3d, 0x7f, 0x9f, 0x4d, 0x51, 0x58
            ]
        );
    }

    #[test]
    fn encode_uses_7_16_and_64_bit_lengths_and_always_masks() {
        let mask = [1, 2, 3, 4];
        for (len, header) in [
            (0usize, 2usize),
            (125, 2),
            (126, 4),
            (65535, 4),
            (65536, 10),
        ] {
            let payload = vec![0xAB; len];
            let frame = encode_client_frame(OP_TEXT, &payload, mask);
            assert_eq!(frame[0], 0x81, "FIN | text for len {len}");
            assert_eq!(frame[1] & 0x80, 0x80, "mask bit for len {len}");
            match len {
                0..=125 => assert_eq!(frame[1] & 0x7F, len as u8),
                126..=65535 => {
                    assert_eq!(frame[1] & 0x7F, 126);
                    assert_eq!(u16::from_be_bytes([frame[2], frame[3]]) as usize, len);
                }
                _ => {
                    assert_eq!(frame[1] & 0x7F, 127);
                    assert_eq!(
                        u64::from_be_bytes(frame[2..10].try_into().unwrap()),
                        len as u64
                    );
                }
            }
            assert_eq!(&frame[header..header + 4], &mask);
            assert_eq!(frame.len(), header + 4 + len);
            assert!(
                frame[header + 4..]
                    .iter()
                    .enumerate()
                    .all(|(i, b)| b ^ mask[i & 3] == 0xAB)
            );
        }
    }

    #[test]
    fn decode_reads_unmasked_server_frames_of_every_length_form() {
        for len in [0usize, 5, 125, 126, 300, 65535, 65536, 70_000] {
            let payload = vec![b'x'; len];
            let mut wire = vec![0x81];
            if len < 126 {
                wire.push(len as u8);
            } else if len <= 65535 {
                wire.push(126);
                wire.extend_from_slice(&(len as u16).to_be_bytes());
            } else {
                wire.push(127);
                wire.extend_from_slice(&(len as u64).to_be_bytes());
            }
            wire.extend_from_slice(&payload);
            wire.extend_from_slice(&[0x89, 0x00]); // a following ping stays in the buffer
            let (frame, used) = decode_server_frame(&wire).unwrap().unwrap();
            assert_eq!(used, wire.len() - 2);
            assert_eq!(
                frame,
                Frame {
                    fin: true,
                    opcode: OP_TEXT,
                    payload
                }
            );
        }
    }

    #[test]
    fn decode_waits_for_more_bytes() {
        let wire = [0x81, 0x05, b'H', b'e', b'l'];
        for cut in 0..wire.len() {
            assert_eq!(
                decode_server_frame(&wire[..cut]).unwrap(),
                None,
                "cut {cut}"
            );
        }
        assert_eq!(decode_server_frame(&[0x81, 126, 0x01]).unwrap(), None);
        assert_eq!(decode_server_frame(&[0x81, 127, 0, 0, 0]).unwrap(), None);
    }

    #[test]
    fn decode_handles_ping_pong_close_and_fragments() {
        let (ping, _) = decode_server_frame(&[0x89, 0x02, 1, 2]).unwrap().unwrap();
        assert_eq!(
            (ping.opcode, ping.fin, ping.payload),
            (OP_PING, true, vec![1, 2])
        );
        let (pong, _) = decode_server_frame(&[0x8A, 0x00]).unwrap().unwrap();
        assert_eq!((pong.opcode, pong.payload.len()), (OP_PONG, 0));
        let (close, _) = decode_server_frame(&[0x88, 0x02, 0x03, 0xE8])
            .unwrap()
            .unwrap();
        assert_eq!((close.opcode, close.payload), (OP_CLOSE, vec![0x03, 0xE8]));
        assert_eq!(
            close_error(&[0x03, 0xE8]),
            "Codex app-server closed the connection (code 1000)"
        );
        let (first, _) = decode_server_frame(&[0x01, 0x01, b'a']).unwrap().unwrap();
        assert_eq!((first.opcode, first.fin), (OP_TEXT, false));
        let (last, _) = decode_server_frame(&[0x80, 0x01, b'b']).unwrap().unwrap();
        assert_eq!((last.opcode, last.fin), (OP_CONTINUATION, true));
    }

    #[test]
    fn decode_refuses_invalid_frames_from_the_header_alone() {
        // Masked server frame.
        assert!(decode_server_frame(&[0x81, 0x80, 0, 0, 0, 0]).is_err());
        // Reserved bit.
        assert!(decode_server_frame(&[0xC1, 0x00]).is_err());
        // Unknown opcodes.
        assert!(decode_server_frame(&[0x83, 0x00]).is_err());
        assert!(decode_server_frame(&[0x8B, 0x00]).is_err());
        // Fragmented and oversize control frames.
        assert!(decode_server_frame(&[0x09, 0x00]).is_err());
        assert!(decode_server_frame(&[0x89, 126, 0, 200]).is_err());
        // Declared length over the bound: refused without the payload.
        let mut huge = vec![0x81, 127];
        huge.extend_from_slice(&((MAX_MESSAGE as u64) + 1).to_be_bytes());
        assert!(decode_server_frame(&huge).unwrap_err().contains("exceeds"));
    }

    // ---- path resolution ---------------------------------------------------------------

    #[test]
    fn socket_resolution_order() {
        let sock = os("/run/x.sock");
        let bin = os("/bin/fake-codex");
        let ch = os("/codex-home");
        let home = os("/home/u");
        let r = |s: Option<&OsString>,
                 b: Option<&OsString>,
                 c: Option<&OsString>,
                 h: Option<&OsString>| {
            resolve_control_socket(
                s.map(OsString::as_os_str),
                b.map(OsString::as_os_str),
                c.map(OsString::as_os_str),
                h.map(OsString::as_os_str),
            )
        };
        // 1: the override wins over everything.
        assert_eq!(
            r(Some(&sock), Some(&bin), Some(&ch), Some(&home)),
            Ok(Some(PathBuf::from("/run/x.sock")))
        );
        // 2: a fake-binary override with no socket override keeps the queue path.
        assert_eq!(r(None, Some(&bin), Some(&ch), Some(&home)), Ok(None));
        assert_eq!(
            r(Some(&os("")), Some(&bin), Some(&ch), Some(&home)),
            Ok(None)
        );
        // 3: CODEX_HOME.
        assert_eq!(
            r(None, None, Some(&ch), Some(&home)),
            Ok(Some(PathBuf::from(
                "/codex-home/app-server-control/app-server-control.sock"
            )))
        );
        // 4: HOME/.codex, also when CODEX_HOME is empty.
        let expected = Ok(Some(PathBuf::from(
            "/home/u/.codex/app-server-control/app-server-control.sock",
        )));
        assert_eq!(r(None, None, None, Some(&home)), expected);
        assert_eq!(r(None, None, Some(&os("")), Some(&home)), expected);
        assert_eq!(r(Some(&os("")), None, None, Some(&home)), expected);
        // A relative CODEX_HOME is refused, not resolved against the daemon's cwd.
        let err = r(None, None, Some(&os("rel/codex")), Some(&home)).unwrap_err();
        assert!(
            err.contains("CODEX_HOME") && err.contains("absolute"),
            "{err}"
        );
        // ... but an override still wins over it.
        assert_eq!(
            r(Some(&sock), None, Some(&os("rel")), Some(&home)),
            Ok(Some(PathBuf::from("/run/x.sock")))
        );
        // Nothing to go on.
        assert!(r(None, None, None, None).unwrap_err().contains("HOME"));
        assert!(r(None, None, Some(&os("")), Some(&os(""))).is_err());
    }

    // ---- client against a scripted server ---------------------------------------------

    /// Serve the full happy path, tolerating noise before each response.
    fn happy_script(c: &mut ServerConn) {
        c.handshake();
        let init = c.expect_json();
        assert_eq!(init["id"], 1);
        c.send_text(r#"{"method":"remote/noise","params":{}}"#);
        c.send_frame(true, OP_PING, b"hi");
        c.send_text(r#"{"id":99,"result":{}}"#);
        c.send_text(r#"{"id":1,"method":"server/request","params":{}}"#);
        c.send_text("not json at all");
        c.send_frame(true, OP_PONG, b"");
        c.send_text(r#"{"id":1,"result":{"userAgent":"codex/test"}}"#);
        let pong = c.read_client_frame().expect("pong for the ping");
        assert_eq!((pong.opcode, &pong.payload[..]), (OP_PONG, &b"hi"[..]));
        let initialized = c.expect_json();
        assert_eq!(initialized["method"], "initialized");
        let compact = c.expect_json();
        assert_eq!(compact["id"], 2);
        c.send_text(r#"{"method":"thread/compacted","params":{}}"#);
        // The reply, fragmented, with a ping in the middle.
        c.send_frame(false, OP_TEXT, br#"{"id":2,"#);
        c.send_frame(true, OP_PING, b"mid");
        c.send_frame(true, OP_CONTINUATION, br#""result":{}}"#);
        c.drain_until_eof();
    }

    #[test]
    fn compacts_with_the_exact_message_sequence_all_masked() {
        let dir = TempDir::new().unwrap();
        let (sock, server) = spawn(dir.path(), happy_script);
        compact_thread(&sock, "thread-123", SHORT).expect("compact succeeds");
        let log = server.join().unwrap();

        assert!(log.iter().all(|f| f.masked), "every client frame is masked");
        let ops: Vec<u8> = log.iter().map(|f| f.opcode).collect();
        assert_eq!(
            ops,
            [OP_TEXT, OP_PONG, OP_TEXT, OP_TEXT, OP_PONG, OP_CLOSE],
            "initialize, pong(ping), initialized, compact, pong(ping), close"
        );
        assert_eq!(
            log[0].json(),
            json!({"id":1,"method":"initialize",
                   "params":{"clientInfo":{"name":"ferroloop","version":"0.1.0"}}})
        );
        assert_eq!(log[1].payload, b"hi");
        assert_eq!(log[2].json(), json!({"method":"initialized","params":{}}));
        assert_eq!(
            log[3].json(),
            json!({"id":2,"method":"thread/compact/start","params":{"threadId":"thread-123"}})
        );
        assert_eq!(log[4].payload, b"mid");
        assert_eq!(&log[5].payload[..], &1000u16.to_be_bytes());
    }

    #[test]
    fn thread_ids_are_escaped_in_the_request() {
        let dir = TempDir::new().unwrap();
        let tricky = "a\"b\\c\n</x>\u{1F600}";
        let (sock, server) = spawn(dir.path(), |c| {
            c.handshake();
            c.expect_json();
            c.send_text(r#"{"id":1,"result":{}}"#);
            c.expect_json();
            c.expect_json();
            c.send_text(r#"{"id":2,"result":{}}"#);
            c.drain_until_eof();
        });
        compact_thread(&sock, tricky, SHORT).unwrap();
        let log = server.join().unwrap();
        let raw = std::str::from_utf8(&log[2].payload).unwrap();
        assert!(
            raw.contains(r#""threadId":"a\"b\\c\n</x>"#),
            "escaped on the wire: {raw}"
        );
        assert_eq!(log[2].json()["params"]["threadId"], tricky);
    }

    #[test]
    fn large_replies_in_16_and_64_bit_frames_are_read() {
        let dir = TempDir::new().unwrap();
        let (sock, server) = spawn(dir.path(), |c| {
            c.handshake();
            c.expect_json();
            let pad = "p".repeat(300);
            c.send_text(&format!(r#"{{"id":1,"result":{{"pad":"{pad}"}}}}"#));
            c.expect_json();
            c.expect_json();
            let pad = "p".repeat(70_000);
            c.send_text(&format!(r#"{{"id":2,"result":{{"pad":"{pad}"}}}}"#));
            c.drain_until_eof();
        });
        compact_thread(&sock, "t", SHORT).unwrap();
        server.join().unwrap();
    }

    #[test]
    fn rpc_error_on_compact_is_returned_with_its_message() {
        let dir = TempDir::new().unwrap();
        let (sock, server) = spawn(dir.path(), |c| {
            c.handshake();
            c.expect_json();
            c.send_text(r#"{"id":1,"result":{}}"#);
            c.expect_json();
            c.expect_json();
            c.send_text(r#"{"id":2,"error":{"code":-32600,"message":"thread not found: t-9"}}"#);
            c.drain_until_eof();
        });
        let err = compact_thread(&sock, "t-9", SHORT).unwrap_err();
        server.join().unwrap();
        assert!(err.contains("thread/compact/start"), "{err}");
        assert!(err.contains("thread not found: t-9"), "{err}");
        assert!(err.contains("-32600"), "{err}");
    }

    #[test]
    fn rpc_error_on_initialize_stops_before_compact() {
        let dir = TempDir::new().unwrap();
        let (sock, server) = spawn(dir.path(), |c| {
            c.handshake();
            c.expect_json();
            c.send_text(r#"{"id":1,"error":{"code":-32002,"message":"Not initialized"}}"#);
            c.drain_until_eof();
        });
        let err = compact_thread(&sock, "t", SHORT).unwrap_err();
        let log = server.join().unwrap();
        assert!(
            err.contains("initialize") && err.contains("Not initialized"),
            "{err}"
        );
        assert!(
            log.iter()
                .all(|f| f.opcode != OP_TEXT || f.json()["method"] != "thread/compact/start"),
            "compact must not be requested after a failed initialize"
        );
    }

    #[test]
    fn long_error_text_is_bounded() {
        let long = "e".repeat(5000);
        let err = describe_rpc_error(&json!({"message": long, "code": 1}));
        assert!(
            err.chars().count() <= MAX_ERROR_TEXT + 3 + 10,
            "{}",
            err.len()
        );
        assert!(err.ends_with("..."));
    }

    /// Run a client against a server that, after the handshake and `initialize` request, runs
    /// `script`; returns the client's error text.
    fn error_after_initialize(script: impl FnOnce(&mut ServerConn) + Send + 'static) -> String {
        let dir = TempDir::new().unwrap();
        let (sock, server) = spawn(dir.path(), move |c| {
            c.handshake();
            c.expect_json();
            script(c);
            c.drain_until_eof();
        });
        let err = compact_thread(&sock, "t", SHORT).unwrap_err();
        let _ = server.join();
        err
    }

    #[test]
    fn rpc_error_text_is_only_message_and_code_without_control_characters() {
        let data = "D".repeat(100_000);
        let err = describe_rpc_error(&json!({
            "code": -32000,
            "message": "bad\nthing\x1b[31m\r\t\u{7f}\u{85}",
            "data": {"secret": "token-abc", "pad": data},
        }));
        assert_eq!(err, "badthing[31m (code -32000)");
        assert!(!err.contains("token-abc") && !err.chars().any(char::is_control));

        // No message: the code alone, never the raw JSON.
        let err = describe_rpc_error(&json!({"code": 7, "data": "token-abc\n"}));
        assert_eq!(err, "(code 7)");
        let err = describe_rpc_error(&json!({"data": "token-abc"}));
        assert_eq!(err, "no message");
        // A non-string message and a non-number code are ignored.
        let err = describe_rpc_error(&json!({"message": {"x": "token-abc"}, "code": "c"}));
        assert_eq!(err, "no message");
        assert!(
            describe_rpc_error(&json!({"message": "m".repeat(9000)}))
                .chars()
                .count()
                < 400
        );
    }

    #[test]
    fn null_error_with_a_result_is_success() {
        let dir = TempDir::new().unwrap();
        let (sock, server) = spawn(dir.path(), |c| {
            c.handshake();
            c.expect_json();
            c.send_text(r#"{"id":1,"result":{},"error":null}"#);
            c.expect_json();
            c.expect_json();
            c.send_text(r#"{"id":2,"error":null,"result":{"started":true}}"#);
            c.drain_until_eof();
        });
        compact_thread(&sock, "t", SHORT).expect("null error is no error");
        server.join().unwrap();
    }

    #[test]
    fn response_with_neither_result_nor_error_is_malformed() {
        for reply in [r#"{"id":1}"#, r#"{"id":1,"error":null}"#] {
            let err = error_after_initialize(move |c| c.send_text(reply));
            assert!(
                err.contains("malformed response") && err.contains("initialize"),
                "{reply}: {err}"
            );
        }
    }

    #[test]
    fn error_text_on_the_wire_is_sanitised_end_to_end() {
        let err = error_after_initialize(|c| {
            let data = "D".repeat(50_000);
            c.send_text(
                &json!({"id":1,"error":{"code":5,"message":"line1\nline2\u{1b}[0m","data":data}})
                    .to_string(),
            );
        });
        assert!(err.contains("line1line2[0m (code 5)"), "{err}");
        assert!(
            !err.chars().any(char::is_control) && err.len() < 500,
            "{err}"
        );
    }

    #[test]
    fn slow_handshake_reply_cannot_outlast_the_deadline() {
        let dir = TempDir::new().unwrap();
        let (sock, _server) = spawn(dir.path(), |c| {
            c.read_upgrade_request();
            // One byte per 100 ms, never finishing the headers: each read succeeds, so only an
            // overall deadline (not a per-read timeout) can stop the client. Not joined.
            for byte in b"HTTP/1.1 101 Switching".iter().take(20) {
                c.write_raw(&[*byte]);
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        let started = Instant::now();
        let err = compact_thread(&sock, "t", Duration::from_millis(300)).unwrap_err();
        assert!(
            started.elapsed() < Duration::from_millis(1200),
            "took {:?}",
            started.elapsed()
        );
        assert!(err.contains("timed out"), "{err}");
    }

    #[test]
    fn invalid_utf8_in_a_reassembled_message_is_an_error() {
        let err = error_after_initialize(|c| {
            c.send_frame(false, OP_TEXT, &[b'{', 0xC3]);
            c.send_frame(true, OP_CONTINUATION, &[0x28, b'}']); // 0xC3 0x28 is invalid
        });
        assert!(err.contains("not UTF-8"), "{err}");
    }

    #[test]
    fn stray_continuation_frame_is_an_error() {
        let err = error_after_initialize(|c| c.send_frame(true, OP_CONTINUATION, b"x"));
        assert!(err.contains("stray continuation"), "{err}");
    }

    #[test]
    fn new_data_frame_inside_a_fragmented_message_is_an_error() {
        for second in [OP_TEXT, OP_BINARY] {
            let err = error_after_initialize(move |c| {
                c.send_frame(false, OP_TEXT, b"{\"id\":");
                c.send_frame(true, second, b"1}");
            });
            assert!(err.contains("inside a fragmented"), "{err}");
        }
    }

    #[test]
    fn fragmented_message_over_the_bound_is_an_error() {
        let err = error_after_initialize(|c| {
            let part = vec![b'x'; MAX_MESSAGE / 2 + 1];
            c.send_frame(false, OP_TEXT, &part);
            c.send_frame(false, OP_CONTINUATION, &part);
            c.send_frame(true, OP_CONTINUATION, b"y");
        });
        assert!(err.contains("exceeds"), "{err}");
    }

    #[test]
    fn masked_server_frame_is_an_error_through_the_client() {
        let err = error_after_initialize(|c| {
            c.write_raw(&[0x81, 0x82, 1, 2, 3, 4, b'{' ^ 1, b'}' ^ 2]);
        });
        assert!(err.contains("masked"), "{err}");
    }

    #[test]
    fn handshake_rejection_is_an_error() {
        let dir = TempDir::new().unwrap();
        let (sock, server) = spawn(dir.path(), |c| {
            c.read_upgrade_request();
            c.write_raw(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n");
        });
        let err = compact_thread(&sock, "t", SHORT).unwrap_err();
        server.join().unwrap();
        assert!(
            err.contains("refused the WebSocket upgrade") && err.contains("403"),
            "{err}"
        );
    }

    #[test]
    fn wrong_or_missing_accept_key_is_an_error() {
        for reply in [
            "HTTP/1.1 101 Switching Protocols\r\nSec-WebSocket-Accept: AAAAAAAAAAAAAAAAAAAAAAAAAAA=\r\n\r\n",
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\r\n",
        ] {
            let dir = TempDir::new().unwrap();
            let (sock, server) = spawn(dir.path(), move |c| {
                c.read_upgrade_request();
                c.write_raw(reply.as_bytes());
            });
            let err = compact_thread(&sock, "t", SHORT).unwrap_err();
            server.join().unwrap();
            assert!(err.contains("Sec-WebSocket-Accept"), "{err}");
        }
    }

    #[test]
    fn server_closing_early_is_an_error() {
        // Drops the socket after the handshake and the initialize request.
        let dir = TempDir::new().unwrap();
        let (sock, server) = spawn(dir.path(), |c| {
            c.handshake();
            c.expect_json();
        });
        let err = compact_thread(&sock, "t", SHORT).unwrap_err();
        server.join().unwrap();
        assert!(err.contains("closed the connection"), "{err}");

        // Drops the socket before answering the handshake.
        let dir = TempDir::new().unwrap();
        let (sock, server) = spawn(dir.path(), |c| {
            c.read_upgrade_request();
        });
        assert!(compact_thread(&sock, "t", SHORT).is_err());
        server.join().unwrap();
    }

    #[test]
    fn server_close_frame_is_an_error() {
        let dir = TempDir::new().unwrap();
        let (sock, server) = spawn(dir.path(), |c| {
            c.handshake();
            c.expect_json();
            c.send_frame(true, OP_CLOSE, &1011u16.to_be_bytes());
            c.drain_until_eof();
        });
        let err = compact_thread(&sock, "t", SHORT).unwrap_err();
        server.join().unwrap();
        assert!(
            err.contains("closed the connection") && err.contains("1011"),
            "{err}"
        );
    }

    #[test]
    fn silence_after_the_request_times_out() {
        let dir = TempDir::new().unwrap();
        let (sock, server) = spawn(dir.path(), |c| {
            c.handshake();
            c.expect_json();
            c.drain_until_eof(); // never answers; ends when the client gives up
        });
        let started = Instant::now();
        let err = compact_thread(&sock, "t", Duration::from_millis(300)).unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "bounded by the deadline"
        );
        assert!(err.contains("timed out"), "{err}");
        server.join().unwrap();
    }

    #[test]
    fn endless_pings_cannot_outlast_the_deadline() {
        let dir = TempDir::new().unwrap();
        let (sock, server) = spawn(dir.path(), |c| {
            c.handshake();
            c.expect_json();
            for _ in 0..2000 {
                c.send_frame(true, OP_PING, b"");
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        let started = Instant::now();
        let err = compact_thread(&sock, "t", Duration::from_millis(300)).unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(err.contains("timed out") || err.contains("closed"), "{err}");
        let _ = server.join();
    }

    #[test]
    fn missing_socket_is_a_clear_error() {
        let dir = TempDir::new().unwrap();
        let sock = dir.path().join("absent.sock");
        let err = compact_thread(&sock, "t", SHORT).unwrap_err();
        assert!(
            err.contains("cannot connect") && err.contains("absent.sock"),
            "{err}"
        );
    }
}
