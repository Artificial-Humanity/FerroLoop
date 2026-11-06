//! Named host bounds and the typed messages both sides share.
//!
//! Frame payload is 256 KiB after the type byte. A logical Line is 1 MiB of
//! raw bytes plus at most 4 KiB of envelope. Stdout lines are 1 MiB. Stdin
//! queues 64 lines and 256 KiB including newlines. An input operation has 5 s.
//! The stderr tail is 64 KiB. A daemon admits 64 sessions and 16 subscribers
//! by default, and one session keeps an 8 MiB shared stdout backlog. Handshake
//! payloads are 4 KiB. Thirty-two connections may be unauthenticated at once.
//! The handshake deadline is 5 s, a started frame has 10 s, and an RPC or
//! event write has 5 s.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub const PROTO_MAJOR: u16 = 1;
pub const PROTO_MINOR: u16 = 0;

pub const DEFAULT_MAX_SESSIONS: usize = 64;
pub const MAX_SUBSCRIBERS: usize = 16;
pub const MAX_STDOUT_BACKLOG: usize = 8 * 1024 * 1024;

pub const MAX_FRAME_PAYLOAD: usize = 256 * 1024;
pub const MAX_LINE_BYTES: usize = 1024 * 1024;
pub const MAX_STDOUT_LINE: usize = 1024 * 1024;
pub const MAX_EVENT_ENVELOPE: usize = 4 * 1024;
pub const MAX_REASSEMBLY_BYTES: usize = MAX_LINE_BYTES + MAX_EVENT_ENVELOPE;
pub const MAX_FRAGMENTS: usize = 64;
pub const MAX_UNAUTH_PAYLOAD: usize = 4 * 1024;
pub const MAX_UNAUTHENTICATED: usize = 32;

pub const STDIN_QUEUE_FRAMES: usize = 64;
pub const STDIN_QUEUE_BYTES: usize = 256 * 1024;
pub const STDERR_TAIL_BYTES: usize = 64 * 1024;

pub const MAX_ARGV: usize = 128;
pub const MAX_ARG_BYTES: usize = 32 * 1024;
pub const MAX_ARGV_AGGREGATE: usize = 128 * 1024;
pub const MAX_ENV_ENTRIES: usize = 128;
pub const MAX_ENV_NAME: usize = 256;
pub const MAX_ENV_VALUE: usize = 32 * 1024;
pub const MAX_ENV_AGGREGATE: usize = 64 * 1024;
pub const MAX_CWD_BYTES: usize = 4096;
pub const MAX_WINDOWS_COMMAND_UNITS: usize = 32_767;

pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
pub const FRAME_READ_TIMEOUT: Duration = Duration::from_secs(10);
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
pub const STDIN_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

pub const CLIENT_DOMAIN: &[u8] = b"fl-host/auth/1/client";
pub const SERVER_DOMAIN: &[u8] = b"fl-host/auth/1/server";

/// Fixed Line header: `session_len` + 64 session bytes + epoch + seq + more.
pub const LINE_HEADER_LEN: usize = 1 + 64 + 8 + 8 + 1;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HostError {
    #[error("frame length is outside the accepted range")]
    FrameLength,
    #[error("event exceeds a reassembly bound")]
    EventTooLarge,
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("unsupported: {subject}")]
    Unsupported { subject: &'static str },
    #[error("authentication failed")]
    Auth,
    #[error("incompatible protocol")]
    IncompatibleProtocol { daemon_version: String },
    #[error("peer uid {peer} does not match {own}")]
    ForeignPeer { peer: u32, own: u32 },
    #[error("peer identity is unavailable")]
    PeerIdentityUnavailable,
    #[error("peer identity does not match")]
    ForeignIdentity,
    #[error("a host daemon already holds this endpoint")]
    AlreadyRunning,
    #[error("endpoint is busy")]
    EndpointBusy,
    #[error("endpoint path is not a socket")]
    NotASocket,
    #[error("endpoint parent is not a private directory")]
    ParentDenied,
    #[error("an ancestor of the endpoint can be renamed by an untrusted user")]
    RenameableAncestor,
    #[error("endpoint state is not a private file")]
    EndpointStateDenied,
    #[error("frame deadline exceeded")]
    FrameTimeout,
    #[error("host io: {0}")]
    Io(String),
}

impl From<std::io::Error> for HostError {
    fn from(err: std::io::Error) -> Self {
        HostError::Io(err.to_string())
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(String);

impl SessionId {
    pub fn new(name: &str) -> Result<Self, HostError> {
        let bytes = name.as_bytes();
        let ok = (1..=64).contains(&bytes.len())
            && bytes
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
        if ok {
            Ok(Self(name.to_owned()))
        } else {
            Err(HostError::Protocol("invalid session id".into()))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("SessionId").field(&self.0).finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionEpoch(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RequestId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ControllerLease(pub u64);

impl ControllerLease {
    /// An atomic generation bump does not prove an in-progress OS write stopped.
    pub fn generation_bump_settles_os_write() -> bool {
        false
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpawnKind {
    Pipe,
    Pty,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnSpec {
    pub session: SessionId,
    pub kind: SpawnKind,
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
    pub env_allow: BTreeSet<String>,
}

impl SpawnSpec {
    pub fn validate(&self) -> Result<(), HostError> {
        if self.kind == SpawnKind::Pty {
            return Err(HostError::Unsupported { subject: "pty" });
        }
        if self.argv.is_empty() || self.argv.len() > MAX_ARGV {
            return Err(HostError::Protocol("argv count".into()));
        }
        let mut argv_bytes = 0usize;
        for arg in &self.argv {
            if arg.as_bytes().contains(&0) || arg.len() > MAX_ARG_BYTES {
                return Err(HostError::Protocol("argv entry".into()));
            }
            argv_bytes = argv_bytes
                .checked_add(arg.len())
                .ok_or_else(|| HostError::Protocol("argv aggregate".into()))?;
        }
        if argv_bytes > MAX_ARGV_AGGREGATE {
            return Err(HostError::Protocol("argv aggregate".into()));
        }
        if self.env.len() > MAX_ENV_ENTRIES || self.env_allow.len() > MAX_ENV_ENTRIES {
            return Err(HostError::Protocol("env count".into()));
        }
        for name in self.env.keys().chain(self.env_allow.iter()) {
            if !env_name_ok(name) {
                return Err(HostError::Protocol("env name".into()));
            }
        }
        for (name, value) in &self.env {
            if !self.env_allow.contains(name) {
                return Err(HostError::Protocol("disallowed env override".into()));
            }
            if value.as_bytes().contains(&0) || value.len() > MAX_ENV_VALUE {
                return Err(HostError::Protocol("env value".into()));
            }
        }
        for name in &self.env_allow {
            if name.as_bytes().contains(&0) {
                return Err(HostError::Protocol("env name".into()));
            }
        }
        let mut names: BTreeSet<&str> = self.env.keys().map(String::as_str).collect();
        for name in &self.env_allow {
            names.insert(name.as_str());
        }
        let name_bytes: usize = names.iter().map(|name| name.len()).sum();
        let value_bytes: usize = self.env.values().map(String::len).sum();
        if name_bytes.saturating_add(value_bytes) > MAX_ENV_AGGREGATE {
            return Err(HostError::Protocol("env aggregate".into()));
        }
        let cwd = self.cwd.as_os_str().as_encoded_bytes();
        if cwd.contains(&0) || cwd.len() > MAX_CWD_BYTES {
            return Err(HostError::Protocol("cwd".into()));
        }
        Ok(())
    }
}

fn env_name_ok(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= MAX_ENV_NAME
        && !bytes.contains(&0)
        && !bytes.contains(&b'=')
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalKind {
    Interrupt,
    Terminate,
    Kill,
    Hangup,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum HostRequest {
    Spawn {
        id: RequestId,
        spec: SpawnSpec,
    },
    List {
        id: RequestId,
    },
    Kill {
        id: RequestId,
        session: SessionId,
        expected_epoch: SessionEpoch,
        lease: Option<ControllerLease>,
    },
    Events {
        id: RequestId,
        session: Option<SessionId>,
    },
    ClaimLease {
        id: RequestId,
        session: SessionId,
        expected_epoch: SessionEpoch,
    },
    Send {
        id: RequestId,
        session: SessionId,
        expected_epoch: SessionEpoch,
        lease: ControllerLease,
        line: String,
    },
    Signal {
        id: RequestId,
        session: SessionId,
        expected_epoch: SessionEpoch,
        lease: ControllerLease,
        signal: SignalKind,
    },
}

impl HostRequest {
    pub fn request_id(&self) -> RequestId {
        match self {
            Self::Spawn { id, .. }
            | Self::List { id }
            | Self::Kill { id, .. }
            | Self::Events { id, .. }
            | Self::ClaimLease { id, .. }
            | Self::Send { id, .. }
            | Self::Signal { id, .. } => *id,
        }
    }

    pub fn op_name(&self) -> &'static str {
        match self {
            Self::Spawn { .. } => "spawn",
            Self::List { .. } => "list",
            Self::Kill { .. } => "kill",
            Self::Events { .. } => "events",
            Self::ClaimLease { .. } => "claim_lease",
            Self::Send { .. } => "send",
            Self::Signal { .. } => "signal",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputState {
    Writable,
    Fenced,
}

impl InputState {
    /// `Fenced` stays fenced. A new lease does not reopen input.
    pub fn after_lease(self) -> Self {
        self
    }

    /// Fenced input stays closed until kill and respawn with a strictly newer epoch.
    pub fn after_respawn(
        self,
        current: SessionEpoch,
        newer: SessionEpoch,
    ) -> Result<Self, HostError> {
        if newer.0 > current.0 {
            Ok(InputState::Writable)
        } else {
            Err(HostError::Protocol("epoch is not newer".into()))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildExit {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildRecord {
    pub session: SessionId,
    pub session_epoch: SessionEpoch,
    pub kind: SpawnKind,
    pub pid: u32,
    pub input: InputState,
    pub started_at_ms: u64,
    pub exit: Option<ChildExit>,
    pub consumer_stalled: bool,
}

/// Leader exit and descendant cleanup are different states.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildState {
    Running(ChildRecord),
    LeaderExited(ChildRecord),
    CleanupComplete(ChildRecord),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostedChild {
    pub state: ChildState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SendBlock {
    StaleEpoch,
    StaleLease,
    WrongConnection,
    QueueFull,
    SessionFenced,
    Busy,
    Timeout,
    Io,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SendOutcome {
    Written,
    NotWritten { reason: SendBlock },
    Ambiguous { reason: SendBlock },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum HostResponse {
    Spawned {
        id: RequestId,
        child: HostedChild,
    },
    Listed {
        id: RequestId,
        children: Vec<HostedChild>,
    },
    Killed {
        id: RequestId,
    },
    Subscribed {
        id: RequestId,
    },
    Lease {
        id: RequestId,
        lease: ControllerLease,
    },
    Sent {
        id: RequestId,
        outcome: SendOutcome,
    },
    Signaled {
        id: RequestId,
    },
}

impl HostResponse {
    pub fn request_id(&self) -> RequestId {
        match self {
            Self::Spawned { id, .. }
            | Self::Listed { id, .. }
            | Self::Killed { id }
            | Self::Subscribed { id }
            | Self::Lease { id, .. }
            | Self::Sent { id, .. }
            | Self::Signaled { id } => *id,
        }
    }
}

/// JSON control events. Line bytes travel in a raw frame, not in this enum's encoding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ControlEvent {
    Exited {
        session: SessionId,
        session_epoch: SessionEpoch,
        seq: u64,
        exit: ChildExit,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostEvent {
    Line {
        session: SessionId,
        session_epoch: SessionEpoch,
        seq: u64,
        bytes: Vec<u8>,
    },
    Control(ControlEvent),
}

/// Messages a negotiated minor is allowed to send.
pub fn supports_message(minor: u16, name: &str) -> bool {
    match name {
        "spawn" | "list" | "kill" | "events" | "claim_lease" | "send" | "signal" => true,
        "resize" | "write" => minor >= 1,
        _ => false,
    }
}

/// Quote `args` the way `CommandLineToArgvW` parses a command line, without a shell.
pub fn windows_command_line(args: &[String]) -> Result<String, HostError> {
    let mut parts = Vec::with_capacity(args.len());
    for arg in args {
        if arg.as_bytes().contains(&0) {
            return Err(HostError::Protocol("command line contains NUL".into()));
        }
        parts.push(quote_windows_arg(arg));
    }
    let joined = parts.join(" ");
    let units = joined.encode_utf16().count().saturating_add(1);
    if units > MAX_WINDOWS_COMMAND_UNITS {
        return Err(HostError::Protocol(
            "command line exceeds 32767 UTF-16 units".into(),
        ));
    }
    Ok(joined)
}

fn quote_windows_arg(arg: &str) -> String {
    let quote = arg.is_empty()
        || arg
            .chars()
            .any(|ch| matches!(ch, ' ' | '\t' | '\n' | '\u{000B}' | '"'));
    if !quote {
        return arg.to_owned();
    }
    let mut out = String::from("\"");
    let mut slashes = 0usize;
    for ch in arg.chars() {
        if ch == '\\' {
            slashes += 1;
            continue;
        }
        if ch == '"' {
            out.push_str(&"\\".repeat(slashes * 2 + 1));
            out.push('"');
            slashes = 0;
            continue;
        }
        out.push_str(&"\\".repeat(slashes));
        out.push(ch);
        slashes = 0;
    }
    out.push_str(&"\\".repeat(slashes * 2));
    out.push('"');
    out
}

/// `key=value\0` entries plus the final NUL. The length includes every NUL.
pub fn windows_environment_block(entries: &[(String, String)]) -> Result<Vec<u16>, HostError> {
    let mut block = Vec::new();
    for (key, value) in entries {
        if !env_name_ok(key) || value.as_bytes().contains(&0) {
            return Err(HostError::Protocol("environment block entry".into()));
        }
        block.extend(key.encode_utf16());
        block.push(u16::from(b'='));
        block.extend(value.encode_utf16());
        block.push(0);
    }
    block.push(0);
    if block.len() > MAX_WINDOWS_COMMAND_UNITS {
        return Err(HostError::Protocol(
            "environment block exceeds 32767 UTF-16 units".into(),
        ));
    }
    Ok(block)
}

/// Result of an OS peer-uid lookup. Errors and unsupported platforms fail closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerQuery {
    Uid(u32),
    Error,
    Unsupported,
}

pub fn validate_unix_peer(peer: PeerQuery, own: u32) -> Result<(), HostError> {
    match peer {
        PeerQuery::Uid(uid) if uid == own => Ok(()),
        PeerQuery::Uid(uid) => Err(HostError::ForeignPeer { peer: uid, own }),
        PeerQuery::Error | PeerQuery::Unsupported => Err(HostError::PeerIdentityUnavailable),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WinPeerLookup {
    SidMatch,
    SidMismatch,
    AccessDenied,
    Failed,
}

pub fn validate_win_peer(lookup: WinPeerLookup) -> Result<(), HostError> {
    match lookup {
        WinPeerLookup::SidMatch => Ok(()),
        WinPeerLookup::SidMismatch => Err(HostError::ForeignIdentity),
        WinPeerLookup::AccessDenied | WinPeerLookup::Failed => {
            Err(HostError::PeerIdentityUnavailable)
        }
    }
}

pub fn private_parent_ok(uid: u32, euid: u32, mode: u32, is_dir: bool) -> Result<(), HostError> {
    if is_dir && uid == euid && mode & 0o077 == 0 {
        Ok(())
    } else {
        Err(HostError::ParentDenied)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FirstInstance {
    Created,
    AccessDenied,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutexFastPath {
    Free,
    HeldElsewhere,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BindDisposition {
    pub proceed: bool,
    pub touch_token: bool,
    pub error: Option<HostError>,
}

/// `FILE_FLAG_FIRST_PIPE_INSTANCE` wins over the mutex fast path.
pub fn decide_first_instance(first: FirstInstance, _mutex: MutexFastPath) -> BindDisposition {
    match first {
        FirstInstance::AccessDenied => BindDisposition {
            proceed: false,
            touch_token: false,
            error: Some(HostError::AlreadyRunning),
        },
        FirstInstance::Created => BindDisposition {
            proceed: true,
            touch_token: true,
            error: None,
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstanceLedger {
    alive: usize,
}

impl InstanceLedger {
    pub fn first() -> Self {
        Self { alive: 1 }
    }

    pub fn alive(&self) -> usize {
        self.alive
    }

    pub fn precreate_standby(&mut self) {
        self.alive = self.alive.saturating_add(1);
    }

    pub fn handoff(&mut self) -> Result<(), HostError> {
        if self.alive < 2 {
            return Err(HostError::Protocol(
                "refusing to hand off the last pipe instance".into(),
            ));
        }
        self.alive -= 1;
        Ok(())
    }

    pub fn shutdown(&mut self) {
        self.alive = 0;
    }
}

pub fn pipe_dacl_sddl(sid: &str) -> String {
    format!("D:P(A;;GA;;;{sid})")
}

pub fn dacl_is_single_sid(sddl: &str, sid: &str) -> bool {
    sddl == pipe_dacl_sddl(sid)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenFileCreate {
    pub disposition_create_new: bool,
    pub dacl_in_security_attributes: bool,
    pub inherit_handle: bool,
    pub post_write_acl_fixup: bool,
}

impl TokenFileCreate {
    pub fn protected() -> Self {
        Self {
            disposition_create_new: true,
            dacl_in_security_attributes: true,
            inherit_handle: false,
            post_write_acl_fixup: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NamedPipeServerOptions {
    pub byte_mode: bool,
    pub reject_remote_clients: bool,
    pub inherit_handle: bool,
    pub first_pipe_instance: bool,
}

impl NamedPipeServerOptions {
    pub fn authenticated_local(first: bool) -> Self {
        Self {
            byte_mode: true,
            reject_remote_clients: true,
            inherit_handle: false,
            first_pipe_instance: first,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NamedPipeClientOptions {
    pub sqos_identification: bool,
    pub inherit_handle: bool,
}

impl NamedPipeClientOptions {
    pub fn identification() -> Self {
        Self {
            sqos_identification: true,
            inherit_handle: false,
        }
    }
}

/// 32-byte daemon token. Debug and Display never include the bytes.
#[derive(Clone, Copy)]
pub struct AuthToken([u8; 32]);

impl AuthToken {
    pub fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn generate() -> Result<Self, HostError> {
        let mut bytes = [0u8; 32];
        getrandom::getrandom(&mut bytes).map_err(|err| HostError::Io(err.to_string()))?;
        Ok(Self(bytes))
    }
}

impl std::fmt::Debug for AuthToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AuthToken(redacted)")
    }
}

impl std::fmt::Display for AuthToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("redacted")
    }
}

pub fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}
