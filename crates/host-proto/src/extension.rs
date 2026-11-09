//! Opt-in messages, bounded reliable framing, and rendered PTY snapshots.
use std::fmt;

use base64::Engine;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use crate::{
    ChildState, ControlEvent, ControllerLease, HostError, InputState, SendOutcome, SessionEpoch,
    SessionId, SignalKind, SpawnKind, SpawnSpec,
};

pub const MAX_EXTENSION_PART: usize = 32 * 1024;
pub const MAX_RAW_INPUT: usize = 64 * 1024;
pub const RELIABLE_FEATURE: u64 = 1;
pub const PTY_FEATURE: u64 = 2;
/// Opt-in daemon build telemetry; does not change existing response shapes.
pub const HOST_STATUS_FEATURE: u64 = 4;
pub const RENDERED_PTY_FEATURE: u64 = 8;
/// Opt-in, epoch-bound ownership check; existing response shapes are unchanged.
pub const SESSION_OWNER_FEATURE: u64 = 16;
/// Opt-in completed PTY retention and projection accounting diagnostics.
pub const PTY_RETENTION_FEATURE: u64 = 32;
pub const MAX_RECENT_PTY_EVICTIONS: usize = 64;
pub const MAX_HOSTED_OWNER_BYTES: usize = 128;
pub const MAX_RENDERED_TEXT: usize = 196_608;
pub const MAX_BUILD_FIELD_BYTES: usize = 128;

/// Identity compiled into the serving binary, never the querying client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostBuildInfo {
    #[serde(deserialize_with = "bounded_build_field")]
    pub version: String,
    #[serde(deserialize_with = "bounded_build_field")]
    pub commit: String,
    pub dirty: Option<bool>,
    pub build_time_unix: Option<u64>,
}
impl HostBuildInfo {
    pub fn validate(&self) -> Result<(), HostError> {
        if self.version.len() > MAX_BUILD_FIELD_BYTES || self.commit.len() > MAX_BUILD_FIELD_BYTES {
            return Err(HostError::EventTooLarge);
        }
        Ok(())
    }
}
fn bounded_build_field<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    struct Visitor;
    impl de::Visitor<'_> for Visitor {
        type Value = String;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "at most {MAX_BUILD_FIELD_BYTES} bytes of build metadata")
        }
        fn visit_str<E: de::Error>(self, value: &str) -> Result<String, E> {
            if value.len() > MAX_BUILD_FIELD_BYTES {
                return Err(E::custom("build metadata field too long"));
            }
            Ok(value.to_owned())
        }
    }
    d.deserialize_str(Visitor)
}

fn bounded_hosted_owner<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    struct Visitor;
    impl de::Visitor<'_> for Visitor {
        type Value = String;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(
                f,
                "at most {MAX_HOSTED_OWNER_BYTES} bytes of hosted owner id"
            )
        }
        fn visit_str<E: de::Error>(self, value: &str) -> Result<String, E> {
            if value.len() > MAX_HOSTED_OWNER_BYTES {
                return Err(E::custom("hosted owner id too long"));
            }
            Ok(value.to_owned())
        }
    }
    d.deserialize_str(Visitor)
}

macro_rules! fixed_hex {
    ($name:ident,$n:expr) => {
        #[derive(Clone, PartialEq, Eq, Hash)]
        pub struct $name([u8; $n]);
        impl $name {
            pub const fn new(bytes: [u8; $n]) -> Self {
                Self(bytes)
            }
            pub fn as_bytes(&self) -> &[u8; $n] {
                &self.0
            }
        }
        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(concat!(stringify!($name), "(redacted)"))
            }
        }
        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                let mut out = [0u8; $n * 2];
                for (i, b) in self.0.iter().enumerate() {
                    out[i * 2] = b"0123456789abcdef"[(b >> 4) as usize];
                    out[i * 2 + 1] = b"0123456789abcdef"[(b & 15) as usize];
                }
                s.serialize_str(std::str::from_utf8(&out).expect("hex is ASCII"))
            }
        }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                struct Visitor;
                impl de::Visitor<'_> for Visitor {
                    type Value = $name;
                    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                        write!(f, "{} lowercase hex characters", $n * 2)
                    }
                    fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                        if v.len() != $n * 2
                            || !v
                                .bytes()
                                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                        {
                            return Err(E::custom("invalid fixed lowercase hex"));
                        }
                        let mut out = [0u8; $n];
                        fn nibble(b: u8) -> u8 {
                            if b <= b'9' { b - b'0' } else { b - b'a' + 10 }
                        }
                        for (i, pair) in v.as_bytes().chunks_exact(2).enumerate() {
                            out[i] = (nibble(pair[0]) << 4) | nibble(pair[1]);
                        }
                        Ok($name(out))
                    }
                }
                d.deserialize_str(Visitor)
            }
        }
    };
}
fixed_hex!(HostInstance, 16);
fixed_hex!(Ticket, 32);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamId {
    pub host: HostInstance,
    pub session: SessionId,
    pub epoch: SessionEpoch,
}
impl fmt::Display for StreamId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&serde_json::to_string(self).expect("stream identity"))
    }
}
/// Accounting counters, not measured RSS. Recent identities are oldest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PtyRetentionStatus {
    pub projection_reserved_bytes: u64,
    pub projection_limit_bytes: u64,
    pub completed_pty_sessions: u32,
    pub completed_pty_limit: u32,
    pub evicted_total: u64,
    #[serde(deserialize_with = "bounded_evictions")]
    pub recent_evictions: Vec<StreamId>,
}
fn bounded_evictions<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<StreamId>, D::Error> {
    struct Visitor;
    impl<'de> de::Visitor<'de> for Visitor {
        type Value = Vec<StreamId>;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("at most 64 evicted stream identities")
        }
        fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut records = Vec::with_capacity(MAX_RECENT_PTY_EVICTIONS);
            while records.len() < MAX_RECENT_PTY_EVICTIONS {
                let Some(record) = seq.next_element()? else {
                    return Ok(records);
                };
                records.push(record);
            }
            if seq.next_element::<de::IgnoredAny>()?.is_some() {
                return Err(de::Error::custom("eviction history count"));
            }
            Ok(records)
        }
    }
    d.deserialize_seq(Visitor)
}

/// One coherent screen and progress watermark. Cursor coordinates are zero based.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenderedScreen {
    pub schema: u16,
    pub stream: StreamId,
    #[serde(deserialize_with = "bounded_rendered_text")]
    pub contents: String,
    pub cursor_row: u16,
    pub cursor_col: u16,
    pub rows: u16,
    pub cols: u16,
    pub generation: u64,
    pub output_seq: u64,
    pub output_bytes: u64,
}
impl RenderedScreen {
    pub fn validate(&self) -> Result<(), HostError> {
        if self.schema != 1 {
            return Err(HostError::Protocol("unsupported rendered schema".into()));
        }
        validate_pty_geometry(self.cols, self.rows)?;
        if self.cursor_row >= self.rows || self.cursor_col >= self.cols {
            return Err(HostError::Protocol("rendered cursor outside screen".into()));
        }
        if self.contents.len() > MAX_RENDERED_TEXT {
            return Err(HostError::EventTooLarge);
        }
        // Escaping may expand an arbitrary submitted string sixfold. Bound
        // the actual escaped length without allocating; reserve 4096 bytes for
        // all metadata and the outer response (including u64 request id).
        let escaped = self
            .contents
            .bytes()
            .try_fold(0usize, |n, b| {
                n.checked_add(match b {
                    b'"' | b'\\' | b'\n' | b'\r' | b'\t' | 8 | 12 => 2,
                    0..=31 => 6,
                    _ => 1,
                })
            })
            .ok_or(HostError::EventTooLarge)?;
        if escaped > crate::MAX_FRAME_PAYLOAD - 4096 {
            return Err(HostError::EventTooLarge);
        }
        Ok(())
    }
}
fn bounded_rendered_text<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    struct Visitor;
    impl de::Visitor<'_> for Visitor {
        type Value = String;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "at most {MAX_RENDERED_TEXT} rendered bytes")
        }
        fn visit_str<E: de::Error>(self, value: &str) -> Result<String, E> {
            if value.len() > MAX_RENDERED_TEXT {
                return Err(E::custom("rendered text too long"));
            }
            Ok(value.to_owned())
        }
    }
    d.deserialize_str(Visitor)
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cursor {
    pub stream: StreamId,
    pub seq: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ConnId(pub u64);
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SnapshotId(pub u64);
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMode {
    #[serde(rename = "legacy")]
    LegacyPipe,
    ReliablePipe,
    Pty,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PtyOptions {
    pub cols: u16,
    pub rows: u16,
    pub scrollback_rows: u16,
}
pub const MAX_PTY_ROWS: u16 = 128;
pub const MAX_PTY_COLS: u16 = 256;
pub const MAX_PTY_CELLS: usize = 8192;
pub const MAX_PTY_SCROLLBACK_ROWS: u16 = 512;

pub fn validate_pty_geometry(cols: u16, rows: u16) -> Result<(), HostError> {
    if rows == 0
        || cols == 0
        || rows > MAX_PTY_ROWS
        || cols > MAX_PTY_COLS
        || usize::from(rows) * usize::from(cols) > MAX_PTY_CELLS
    {
        return Err(HostError::Protocol("invalid pty dimensions".into()));
    }
    Ok(())
}
impl PtyOptions {
    pub fn validate(&self) -> Result<(), HostError> {
        validate_pty_geometry(self.cols, self.rows)?;
        if self.scrollback_rows > MAX_PTY_SCROLLBACK_ROWS {
            return Err(HostError::Protocol("invalid pty scrollback".into()));
        }
        Ok(())
    }
}
impl Default for PtyOptions {
    fn default() -> Self {
        Self {
            cols: 120,
            rows: 40,
            scrollback_rows: 512,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectionResume {
    pub output_seq: u64,
    pub revision: u64,
    pub schema: u16,
    pub exact_state: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Coverage {
    pub screen_complete: bool,
    pub state_complete: bool,
    pub scrollback_rows: u16,
    pub history_prefix_lost: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentAdmission {
    PendingSnapshot,
    ResumePending,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionRefusal {
    Unsupported,
    WrongMode,
    WrongInstance,
    StaleEpoch,
    StaleLease,
    WrongConnection,
    CursorExpired,
    FutureCursor,
    TicketInvalid,
    ResourceLimit,
    SessionFenced,
    SnapshotNotReady,
    ProjectionUnavailable,
    ResyncRequired,
    UnsupportedSchema,
    Busy,
    Io,
}
impl fmt::Display for ExtensionRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for ExtensionRefusal {}

macro_rules! bounded_bytes {
    ($name:ident,$max:expr) => {
        #[derive(Clone, PartialEq, Eq)]
        pub struct $name(Vec<u8>);
        impl $name {
            pub fn new(bytes: Vec<u8>) -> Result<Self, HostError> {
                if bytes.len() > $max {
                    Err(HostError::EventTooLarge)
                } else {
                    Ok(Self(bytes))
                }
            }
            pub fn as_slice(&self) -> &[u8] {
                &self.0
            }
            pub fn into_vec(self) -> Vec<u8> {
                self.0
            }
        }
        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_struct(stringify!($name))
                    .field("len", &self.0.len())
                    .finish()
            }
        }
        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(&base64::engine::general_purpose::STANDARD.encode(&self.0))
            }
        }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                struct Visitor;
                impl de::Visitor<'_> for Visitor {
                    type Value = $name;
                    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                        write!(f, "base64 encoding at most {} bytes", $max)
                    }
                    fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                        // Validate length, alphabet and padding before allocating decoded bytes.
                        let len = v.len();
                        if len > ($max).div_ceil(3) * 4 || !len.is_multiple_of(4) {
                            return Err(E::custom("base64 length"));
                        }
                        let padding = if v.ends_with("==") {
                            2
                        } else if v.ends_with('=') {
                            1
                        } else {
                            0
                        };
                        let decoded = len / 4 * 3 - padding;
                        if decoded > $max
                            || !v.as_bytes()[..len - padding]
                                .iter()
                                .all(|b| b.is_ascii_alphanumeric() || *b == b'+' || *b == b'/')
                        {
                            return Err(E::custom("base64 alphabet or bound"));
                        }
                        let mut bytes = vec![0; decoded];
                        let actual = base64::engine::general_purpose::STANDARD
                            .decode_slice(v, &mut bytes)
                            .map_err(E::custom)?;
                        if actual != decoded {
                            return Err(E::custom("base64 decoded length"));
                        }
                        Ok($name(bytes))
                    }
                }
                d.deserialize_str(Visitor)
            }
        }
    };
}
bounded_bytes!(BoundedBytes, MAX_EXTENSION_PART);
bounded_bytes!(RawInput, MAX_RAW_INPUT);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExtensionRequest {
    Capabilities,
    HostStatus,
    PtyRetentionStatus,
    ListSessions,
    CheckSessionOwner {
        stream: StreamId,
        #[serde(deserialize_with = "bounded_hosted_owner")]
        hosted_id: String,
    },
    SpawnSession {
        spec: SpawnSpec,
        mode: SessionMode,
        pty: Option<PtyOptions>,
    },
    ClaimController {
        stream: StreamId,
    },
    SubscribeReliable {
        stream: StreamId,
        lease: ControllerLease,
        after_seq: Option<u64>,
    },
    Observe {
        stream: StreamId,
        after_seq: Option<u64>,
    },
    OpenStream {
        ticket: Ticket,
    },
    Ack {
        cursor: Cursor,
        lease: ControllerLease,
    },
    SendLine {
        stream: StreamId,
        lease: ControllerLease,
        line: String,
    },
    SignalSession {
        stream: StreamId,
        lease: ControllerLease,
        signal: SignalKind,
    },
    KillSession {
        stream: StreamId,
        lease: ControllerLease,
    },
    Write {
        stream: StreamId,
        lease: ControllerLease,
        bytes: RawInput,
    },
    CaptureOutput {
        session: SessionId,
        max_bytes: u32,
    },
    CaptureRendered {
        stream: StreamId,
    },
    Resize {
        stream: StreamId,
        lease: ControllerLease,
        cols: u16,
        rows: u16,
    },
    Attach {
        stream: StreamId,
        resume: Option<ProjectionResume>,
    },
}
impl ExtensionRequest {
    pub fn validate(&self) -> Result<(), HostError> {
        match self {
            Self::CheckSessionOwner { hosted_id, .. }
                if hosted_id.len() > MAX_HOSTED_OWNER_BYTES =>
            {
                return Err(HostError::EventTooLarge);
            }
            Self::SpawnSession { spec, mode, pty } => {
                match (*mode, spec.kind, pty.is_some()) {
                    (
                        SessionMode::LegacyPipe | SessionMode::ReliablePipe,
                        SpawnKind::Pipe,
                        false,
                    ) => {}
                    (SessionMode::Pty, SpawnKind::Pty, _) => {
                        if let Some(opts) = pty {
                            opts.validate()?;
                        }
                    }
                    _ => {
                        return Err(HostError::Protocol(
                            "contradictory session mode/options".into(),
                        ));
                    }
                }
                spec.validate_fields()?;
            }
            Self::SendLine { line, .. } if line.len() > MAX_RAW_INPUT => {
                return Err(HostError::EventTooLarge);
            }
            Self::Write { bytes, .. } if bytes.as_slice().len() > MAX_RAW_INPUT => {
                return Err(HostError::EventTooLarge);
            }
            Self::CaptureOutput { max_bytes, .. } if *max_bytes as usize > MAX_RAW_INPUT => {
                return Err(HostError::EventTooLarge);
            }
            Self::Resize { cols, rows, .. } => validate_pty_geometry(*cols, *rows)?,
            Self::Attach { .. } => {
                return Err(HostError::Unsupported {
                    subject: "PTY backend/schema",
                });
            }
            _ => {}
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionLimits {
    pub retained_epochs: u16,
    pub subscribers: u16,
    pub frame_bytes: u32,
    pub logical_line_bytes: u32,
    pub reliable_data_bytes: u32,
    pub reliable_events: u32,
    pub lifecycle_bytes: u32,
    pub lifecycle_records: u16,
}
impl Default for ExtensionLimits {
    fn default() -> Self {
        Self {
            retained_epochs: 64,
            subscribers: 16,
            frame_bytes: 256 * 1024,
            logical_line_bytes: 1024 * 1024,
            reliable_data_bytes: 8 * 1024 * 1024,
            reliable_events: 4096,
            lifecycle_bytes: 16 * 1024,
            lifecycle_records: 16,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSummary {
    pub stream: StreamId,
    pub mode: SessionMode,
    pub state: ChildState,
    pub acked: u64,
    pub latest: u64,
    pub input: InputState,
}

fn bounded_sessions<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<SessionSummary>, D::Error> {
    struct Visitor;
    impl<'de> de::Visitor<'de> for Visitor {
        type Value = Vec<SessionSummary>;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("at most 64 session summaries")
        }
        fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut records = Vec::with_capacity(64);
            while records.len() < 64 {
                let Some(record) = seq.next_element()? else {
                    return Ok(records);
                };
                records.push(record);
            }
            if seq.next_element::<de::IgnoredAny>()?.is_some() {
                return Err(de::Error::custom("session count"));
            }
            Ok(records)
        }
    }
    d.deserialize_seq(Visitor)
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExtensionResponse {
    PtyRetentionStatus {
        status: PtyRetentionStatus,
    },
    SessionOwner {
        matches: bool,
    },
    HostStatus {
        build: HostBuildInfo,
    },
    Capabilities {
        host: HostInstance,
        features: u64,
        limits: ExtensionLimits,
    },
    Sessions {
        #[serde(deserialize_with = "bounded_sessions")]
        records: Vec<SessionSummary>,
    },
    Session {
        stream: StreamId,
        state: ChildState,
    },
    Controller {
        lease: ControllerLease,
        acked: u64,
        latest: u64,
    },
    Subscription {
        ticket: Ticket,
        acked: u64,
        oldest: u64,
        latest: u64,
        watermark: u64,
    },
    Opened,
    Acked {
        through: u64,
    },
    Input {
        outcome: SendOutcome,
    },
    Controlled,
    CapturedOutput {
        text: String,
    },
    RenderedCapture {
        screen: RenderedScreen,
    },
    Resized {
        cols: u16,
        rows: u16,
        projection_revision: u64,
    },
    Attachment {
        ticket: Ticket,
        projection_revision: u64,
        admission: AttachmentAdmission,
    },
    Refused {
        code: ExtensionRefusal,
    },
}
impl ExtensionResponse {
    pub(crate) fn validate(&self) -> Result<(), HostError> {
        match self {
            Self::HostStatus { build } => build.validate(),
            Self::PtyRetentionStatus { status }
                if status.recent_evictions.len() > MAX_RECENT_PTY_EVICTIONS =>
            {
                Err(HostError::EventTooLarge)
            }
            Self::Sessions { records } if records.len() > 64 => Err(HostError::EventTooLarge),
            Self::CapturedOutput { text } if text.len() > MAX_RAW_INPUT => {
                Err(HostError::EventTooLarge)
            }
            Self::RenderedCapture { screen } => screen.validate(),
            Self::Resized { cols, rows, .. } => validate_pty_geometry(*cols, *rows),
            Self::Attachment { .. } => Err(HostError::Unsupported {
                subject: "PTY backend/schema",
            }),
            _ => Ok(()),
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReliableKind {
    /// Local event kind for an explicit retention gap; never valid in ReliablePart.
    Gap,
    Line,
    Lifecycle,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExtensionFrame {
    Gap {
        from_seq: u64,
        next_seq: u64,
    },
    ReliablePart {
        #[serde(default = "default_terminated", skip_serializing_if = "is_terminated")]
        terminated: bool,
        seq: u64,
        kind: ReliableKind,
        more: bool,
        bytes: BoundedBytes,
    },
    Notice {
        code: ExtensionRefusal,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReliableGap {
    pub from_seq: u64,
    pub next_seq: u64,
}

fn default_terminated() -> bool {
    true
}
fn is_terminated(value: &bool) -> bool {
    *value
}
fn validate_termination(minor: u16, event: &ExtensionFrame) -> Result<(), HostError> {
    match event {
        ExtensionFrame::Gap { from_seq, next_seq }
            if minor < 2 || *from_seq == 0 || next_seq <= from_seq =>
        {
            return Err(HostError::Protocol(
                "invalid or unnegotiated retention gap".into(),
            ));
        }
        ExtensionFrame::ReliablePart {
            kind: ReliableKind::Gap,
            ..
        } => return Err(HostError::Protocol("gap cannot be a reliable part".into())),
        _ => {}
    }
    if let ExtensionFrame::ReliablePart {
        terminated: false,
        kind,
        ..
    } = event
        && (minor < 2 || *kind != ReliableKind::Line)
    {
        return Err(HostError::Protocol(
            "stdout fragments require minor two and line kind".into(),
        ));
    }
    Ok(())
}

pub fn encode_extension_frame(
    minor: u16,
    event: &ExtensionFrame,
) -> Result<crate::Frame, HostError> {
    require_minor(minor)?;
    validate_termination(minor, event)?;
    let payload = serde_json::to_vec(event).map_err(|e| HostError::Protocol(e.to_string()))?;
    if payload.len() > crate::MAX_FRAME_PAYLOAD {
        return Err(HostError::FrameLength);
    }
    Ok(crate::Frame {
        kind: crate::FrameKind::ExtensionEvent,
        payload,
    })
}
pub fn decode_extension_frame(
    minor: u16,
    frame: &crate::Frame,
) -> Result<ExtensionFrame, HostError> {
    require_minor(minor)?;
    if frame.kind != crate::FrameKind::ExtensionEvent {
        return Err(HostError::Protocol("expected extension event".into()));
    }
    if frame.payload.len() > crate::MAX_FRAME_PAYLOAD {
        return Err(HostError::FrameLength);
    }
    let event: ExtensionFrame =
        serde_json::from_slice(&frame.payload).map_err(|e| HostError::Protocol(e.to_string()))?;
    validate_termination(minor, &event)?;
    let encoded_bytes = match &event {
        ExtensionFrame::ReliablePart { bytes, .. } => bytes.as_slice().len().div_ceil(3) * 4,
        ExtensionFrame::Notice { .. } | ExtensionFrame::Gap { .. } => 0,
    };
    if frame.payload.len().saturating_sub(encoded_bytes) > crate::MAX_EVENT_ENVELOPE {
        return Err(HostError::EventTooLarge);
    }
    Ok(event)
}
pub(crate) fn require_minor(minor: u16) -> Result<(), HostError> {
    if minor < 1 {
        Err(HostError::Protocol(
            "extension requires negotiated minor one".into(),
        ))
    } else {
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct ReliableEvent {
    pub gap: Option<ReliableGap>,
    pub terminated: bool,
    pub seq: u64,
    pub kind: ReliableKind,
    pub bytes: Vec<u8>,
}
/// One ticket-bound stream. Stream identity is supplied by the authenticated
/// subscription, not an untrusted per-part field. No partial event is yielded.
pub struct ReliableReassembler {
    after: u64,
    stream: Option<StreamId>,
    open: Option<(u64, ReliableKind, bool, usize, Vec<u8>)>,
}
impl ReliableReassembler {
    pub fn new(after: u64) -> Self {
        Self {
            after,
            stream: None,
            open: None,
        }
    }
    pub fn for_stream(stream: StreamId, after: u64) -> Self {
        Self {
            after,
            stream: Some(stream),
            open: None,
        }
    }
    pub fn discard(&mut self) {
        self.open = None;
    }
    pub fn push(&mut self, part: &ExtensionFrame) -> Result<Option<ReliableEvent>, HostError> {
        let result = self.push_inner(part);
        if result.is_err() {
            self.discard();
        }
        result
    }
    fn push_inner(&mut self, part: &ExtensionFrame) -> Result<Option<ReliableEvent>, HostError> {
        validate_termination(crate::PROTO_MINOR, part)?;
        if let ExtensionFrame::Gap { from_seq, next_seq } = part {
            if self.open.is_some()
                || self.after.checked_add(1) != Some(*from_seq)
                || next_seq <= from_seq
            {
                return Err(HostError::Protocol(
                    "invalid or interleaved retention gap".into(),
                ));
            }
            self.after = next_seq - 1;
            return Ok(Some(ReliableEvent {
                seq: self.after,
                kind: ReliableKind::Gap,
                terminated: false,
                bytes: Vec::new(),
                gap: Some(ReliableGap {
                    from_seq: *from_seq,
                    next_seq: *next_seq,
                }),
            }));
        }
        let ExtensionFrame::ReliablePart {
            seq,
            kind,
            terminated,
            more,
            bytes,
        } = part
        else {
            return Err(HostError::Protocol(
                "non-event interleaved with reliable fragments".into(),
            ));
        };
        if *seq != self.after.checked_add(1).ok_or(HostError::EventTooLarge)? {
            return Err(HostError::Protocol(
                "noncontiguous reliable sequence".into(),
            ));
        }
        let limit = match kind {
            ReliableKind::Gap => {
                return Err(HostError::Protocol("gap cannot be a reliable part".into()));
            }
            ReliableKind::Line => crate::MAX_LINE_BYTES,
            ReliableKind::Lifecycle => crate::MAX_EVENT_ENVELOPE,
        };
        if bytes.as_slice().is_empty() && *more {
            return Err(HostError::Protocol("empty continuing fragment".into()));
        }
        if let Some((open_seq, open_kind, open_terminated, count, body)) = &mut self.open {
            if *open_seq != *seq || open_kind != kind || open_terminated != terminated {
                return Err(HostError::Protocol("interleaved reliable event".into()));
            }
            let total = body
                .len()
                .checked_add(bytes.as_slice().len())
                .ok_or(HostError::EventTooLarge)?;
            if *count >= crate::MAX_FRAGMENTS || total > limit {
                return Err(HostError::EventTooLarge);
            }
            body.try_reserve_exact(total - body.len())
                .map_err(|_| HostError::EventTooLarge)?;
            body.extend_from_slice(bytes.as_slice());
            *count += 1;
        } else {
            if bytes.as_slice().len() > limit {
                return Err(HostError::EventTooLarge);
            }
            // Grow only to the validated next length, avoiding exponential spare
            // capacity and maximum-line storage retained by tiny completed events.
            // Completion moves this buffer instead of cloning the logical event.
            let mut body = Vec::new();
            body.try_reserve_exact(bytes.as_slice().len())
                .map_err(|_| HostError::EventTooLarge)?;
            body.extend_from_slice(bytes.as_slice());
            self.open = Some((*seq, *kind, *terminated, 1, body));
        }
        if *more {
            return Ok(None);
        }
        let (_, kind, terminated, _, body) = self.open.take().expect("open fragment");
        if kind == ReliableKind::Lifecycle {
            let event: ControlEvent =
                serde_json::from_slice(&body).map_err(|e| HostError::Protocol(e.to_string()))?;
            let (session, epoch, event_seq) = match &event {
                ControlEvent::Exited {
                    session,
                    session_epoch,
                    seq,
                    ..
                }
                | ControlEvent::Spawned {
                    session,
                    session_epoch,
                    seq,
                    ..
                }
                | ControlEvent::CleanupComplete {
                    session,
                    session_epoch,
                    seq,
                    ..
                }
                | ControlEvent::ProtocolFailure {
                    session,
                    session_epoch,
                    seq,
                    ..
                } => (session, session_epoch, *seq),
                ControlEvent::SlowSubscriber { .. } => {
                    return Err(HostError::Protocol(
                        "connection diagnostic is not authoritative".into(),
                    ));
                }
            };
            if let Some(stream) = &self.stream
                && (session != &stream.session || epoch != &stream.epoch)
            {
                return Err(HostError::Protocol("lifecycle stream mismatch".into()));
            }
            if event_seq != *seq {
                return Err(HostError::Protocol("lifecycle sequence mismatch".into()));
            }
        }
        self.after = *seq;
        Ok(Some(ReliableEvent {
            gap: None,
            terminated,
            seq: *seq,
            kind,
            bytes: body,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // A request cannot retain an unbounded owner string during deserialization.
    #[test]
    fn owner_query_bounds_id_before_retaining_it() {
        let stream = serde_json::json!({"host": "01010101010101010101010101010101", "session": "Ada.claude", "epoch": 1});
        for len in [128, 129] {
            let request = serde_json::json!({"op": "check_session_owner", "stream": stream, "hosted_id": "x".repeat(len)});
            let result = serde_json::from_value::<ExtensionRequest>(request);
            assert_eq!(result.is_ok(), len == 128, "{result:?}");
        }
    }

    // Catches absent opt-in codecs, permissive snapshot fields and invalid geometry/schema.
    #[test]
    fn rendered_snapshot_roundtrip_is_strict_and_bounded() {
        let stream = serde_json::json!({"host":"00000000000000000000000000000000","session":"screen","epoch":1});
        let screen = serde_json::json!({"schema":1,"stream":stream,"contents":"hi\n世界","cursor_row":1,"cursor_col":2,"rows":40,"cols":120,"generation":7,"output_seq":3,"output_bytes":19});
        let value = serde_json::json!({"op":"rendered_capture","screen":screen});
        let response: ExtensionResponse =
            serde_json::from_value(value.clone()).expect("rendered snapshot codec");
        response.validate().unwrap();
        assert_eq!(serde_json::to_value(&response).unwrap(), value);
        for (field, invalid) in [
            ("schema", serde_json::json!(2)),
            ("rows", serde_json::json!(0)),
            ("cols", serde_json::json!(257)),
            ("cursor_row", serde_json::json!(40)),
            ("cursor_col", serde_json::json!(120)),
            ("contents", serde_json::json!("x".repeat(196609))),
            ("extra", serde_json::json!(true)),
        ] {
            let mut bad = value.clone();
            bad["screen"][field] = invalid;
            let decoded = serde_json::from_value::<ExtensionResponse>(bad);
            assert!(
                decoded.is_err() || decoded.unwrap().validate().is_err(),
                "accepted {field}"
            );
        }
        let capture = serde_json::json!({"op":"capture_rendered","stream":stream});
        let request: ExtensionRequest = serde_json::from_value(capture.clone()).unwrap();
        request.validate().unwrap();
        assert_eq!(serde_json::to_value(request).unwrap(), capture);
        let resize: ExtensionRequest = serde_json::from_value(
            serde_json::json!({"op":"resize","stream":stream,"lease":4,"cols":160,"rows":50}),
        )
        .unwrap();
        resize.validate().expect("reserved resize is now enabled");
        assert!(
            ExtensionResponse::Resized {
                cols: 160,
                rows: 50,
                projection_revision: 8
            }
            .validate()
            .is_ok()
        );
        assert_eq!(
            serde_json::to_value(ExtensionResponse::CapturedOutput { text: "raw".into() }).unwrap(),
            serde_json::json!({"op":"captured_output","text":"raw"})
        );
    }
    // Catches checking only unescaped text bytes, growing JSON above the wire
    // ceiling, and silently truncating an oversized coherent snapshot.
    #[test]
    fn rendered_json_escaping_is_bounded_before_encoding_without_truncation() {
        let mut screen = RenderedScreen {
            schema: 1,
            stream: StreamId {
                host: HostInstance::new([0; 16]),
                session: SessionId::new("screen").unwrap(),
                epoch: SessionEpoch(1),
            },
            contents: "\\".repeat(128000),
            cursor_row: 0,
            cursor_col: 0,
            rows: 32,
            cols: 256,
            generation: u64::MAX,
            output_seq: u64::MAX,
            output_bytes: u64::MAX,
        };
        let response = crate::HostResponse::Extended {
            id: crate::RequestId(u64::MAX),
            response: ExtensionResponse::RenderedCapture {
                screen: screen.clone(),
            },
        };
        let frame = crate::encode_response_for_minor(1, &response).unwrap();
        assert_eq!(frame.payload.capacity(), 262144);
        assert!(frame.payload.len() < 262144);
        assert_eq!(
            crate::decode_response_for_minor(1, &frame).unwrap(),
            response
        );
        screen.contents = "\0".repeat(50000);
        assert!(matches!(screen.validate(), Err(HostError::EventTooLarge)));
        let response = crate::HostResponse::Extended {
            id: crate::RequestId(1),
            response: ExtensionResponse::RenderedCapture { screen },
        };
        assert!(matches!(
            crate::encode_response_for_minor(1, &response),
            Err(HostError::EventTooLarge)
        ));
    }

    fn part(terminated: bool, more: bool) -> ExtensionFrame {
        ExtensionFrame::ReliablePart {
            seq: 1,
            kind: ReliableKind::Line,
            more,
            terminated,
            bytes: BoundedBytes::new(b"x".to_vec()).unwrap(),
        }
    }
    #[test]
    fn host_status_is_bounded_and_legacy_response_shapes_stay_strict() {
        let caps = ExtensionResponse::Capabilities {
            host: HostInstance::new([0; 16]),
            features: RELIABLE_FEATURE,
            limits: ExtensionLimits::default(),
        };
        let mut value = serde_json::to_value(caps).unwrap();
        assert_eq!(value.as_object().unwrap().len(), 4);
        value["build"] = serde_json::json!({});
        assert!(serde_json::from_value::<ExtensionResponse>(value).is_err());
        let sessions =
            serde_json::to_value(ExtensionResponse::Sessions { records: vec![] }).unwrap();
        assert_eq!(
            sessions,
            serde_json::json!({"op": "sessions", "records": []})
        );
        let mut status = serde_json::json!({"op": "host_status", "build": {
            "version": "0.1.0", "commit": "unknown", "dirty": null, "build_time_unix": null
        }});
        assert!(serde_json::from_value::<ExtensionResponse>(status.clone()).is_ok());
        for field in ["version", "commit"] {
            let mut invalid = status.clone();
            invalid["build"][field] = "v".repeat(129).into();
            assert!(serde_json::from_value::<ExtensionResponse>(invalid).is_err());
        }
        status["build"]["version"] = "v".repeat(128).into();
        let valid: ExtensionResponse = serde_json::from_value(status).unwrap();
        assert!(valid.validate().is_ok());
        let ExtensionResponse::HostStatus { mut build } = valid else {
            panic!("status")
        };
        build.version.push('v');
        assert!(
            crate::encode_response_for_minor(
                1,
                &crate::HostResponse::Extended {
                    id: crate::RequestId(1),
                    response: ExtensionResponse::HostStatus { build },
                }
            )
            .is_err()
        );
    }

    #[test]
    fn terminated_json_is_legacy_identical_and_unterminated_requires_minor_two() {
        let frame = encode_extension_frame(1, &part(true, false)).unwrap();
        assert_eq!(
            frame.payload,
            br#"{"op":"reliable_part","seq":1,"kind":"line","more":false,"bytes":"eA=="}"#
        );
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct OldPart {
            op: String,
            seq: u64,
            kind: String,
            more: bool,
            bytes: String,
        }
        let old: OldPart = serde_json::from_slice(&frame.payload).unwrap();
        assert_eq!(
            (
                old.op.as_str(),
                old.seq,
                old.kind.as_str(),
                old.more,
                old.bytes.as_str()
            ),
            ("reliable_part", 1, "line", false, "eA==")
        );
        assert_eq!(
            decode_extension_frame(2, &frame).unwrap(),
            part(true, false)
        );
        assert!(encode_extension_frame(1, &part(false, false)).is_err());
        let new = encode_extension_frame(2, &part(false, false)).unwrap();
        assert!(serde_json::from_slice::<OldPart>(&new.payload).is_err());
        assert!(decode_extension_frame(1, &new).is_err());
        assert_eq!(decode_extension_frame(2, &new).unwrap(), part(false, false));
    }
    #[test]
    fn multipart_termination_disagreement_and_disguised_gap_are_rejected() {
        let mut r = ReliableReassembler::new(0);
        assert!(r.push(&part(false, true)).unwrap().is_none());
        assert!(r.push(&part(true, false)).is_err());
        let mut r = ReliableReassembler::new(0);
        let event = r.push(&part(false, false)).unwrap().unwrap();
        assert!(!event.terminated);
        assert_eq!(event.bytes, b"x");
        let bad = ExtensionFrame::ReliablePart {
            seq: 1,
            kind: ReliableKind::Gap,
            more: false,
            terminated: true,
            bytes: BoundedBytes::new(Vec::new()).unwrap(),
        };
        assert!(encode_extension_frame(2, &bad).is_err());
        assert!(ReliableReassembler::new(0).push(&bad).is_err());
    }
    #[test]
    fn gap_is_explicit_bounded_and_cannot_interrupt_record() {
        let gap = ExtensionFrame::Gap {
            from_seq: 1,
            next_seq: 5,
        };
        assert!(encode_extension_frame(1, &gap).is_err());
        let frame = encode_extension_frame(2, &gap).unwrap();
        assert!(decode_extension_frame(1, &frame).is_err());
        assert_eq!(decode_extension_frame(2, &frame).unwrap(), gap);
        let mut r = ReliableReassembler::new(0);
        let event = r.push(&gap).unwrap().unwrap();
        assert_eq!(event.kind, ReliableKind::Gap);
        assert_eq!(
            event.gap,
            Some(ReliableGap {
                from_seq: 1,
                next_seq: 5
            })
        );
        let next = ExtensionFrame::ReliablePart {
            seq: 5,
            kind: ReliableKind::Line,
            more: false,
            terminated: true,
            bytes: BoundedBytes::new(b"kept".to_vec()).unwrap(),
        };
        assert_eq!(r.push(&next).unwrap().unwrap().bytes, b"kept");
        assert!(r.push(&gap).is_err());
        for bad in [
            ExtensionFrame::Gap {
                from_seq: 1,
                next_seq: 1,
            },
            ExtensionFrame::Gap {
                from_seq: 0,
                next_seq: 1,
            },
        ] {
            assert!(encode_extension_frame(2, &bad).is_err());
        }
        let mut r = ReliableReassembler::new(0);
        r.push(&part(true, true)).unwrap();
        assert!(r.push(&gap).is_err());
    }
    #[test]
    fn spawn_session_validates_pty_and_pipe_combinations() {
        let pipe_spec = SpawnSpec {
            session: SessionId::new("pipe-ok").unwrap(),
            kind: SpawnKind::Pipe,
            argv: vec!["/bin/sh".into()],
            cwd: std::path::PathBuf::from("/tmp"),
            env: Default::default(),
            env_allow: Default::default(),
        };
        let mut pty_spec = pipe_spec.clone();
        pty_spec.session = SessionId::new("pty-ok").unwrap();
        pty_spec.kind = SpawnKind::Pty;

        assert!(
            ExtensionRequest::SpawnSession {
                spec: pty_spec.clone(),
                mode: SessionMode::Pty,
                pty: None,
            }
            .validate()
            .is_ok()
        );
        assert!(
            ExtensionRequest::SpawnSession {
                spec: pty_spec.clone(),
                mode: SessionMode::Pty,
                pty: Some(PtyOptions::default()),
            }
            .validate()
            .is_ok()
        );
        assert!(
            ExtensionRequest::SpawnSession {
                spec: pty_spec.clone(),
                mode: SessionMode::Pty,
                pty: Some(PtyOptions {
                    cols: 0,
                    rows: 40,
                    scrollback_rows: 512,
                }),
            }
            .validate()
            .is_err()
        );
        assert!(
            ExtensionRequest::SpawnSession {
                spec: pty_spec,
                mode: SessionMode::ReliablePipe,
                pty: None,
            }
            .validate()
            .is_err()
        );
        assert!(
            ExtensionRequest::SpawnSession {
                spec: pipe_spec.clone(),
                mode: SessionMode::Pty,
                pty: None,
            }
            .validate()
            .is_err()
        );
        assert!(
            ExtensionRequest::SpawnSession {
                spec: pipe_spec,
                mode: SessionMode::ReliablePipe,
                pty: Some(PtyOptions::default()),
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn retention_diagnostic_roundtrip_and_history_bound() {
        let stream = serde_json::json!({"host":"01010101010101010101010101010101", "session":"retained", "epoch":1});
        let mut response = serde_json::json!({"op":"pty_retention_status", "status": {
            "projection_reserved_bytes":0, "projection_limit_bytes":593000000,
            "completed_pty_sessions":8, "completed_pty_limit":8, "evicted_total":100,
            "recent_evictions":vec![stream.clone();64]
        }});
        let decoded: ExtensionResponse =
            serde_json::from_value(response.clone()).expect("new diagnostic decodes");
        assert_eq!(serde_json::to_value(decoded).unwrap(), response);
        response["status"]["recent_evictions"] = serde_json::json!(vec![stream; 65]);
        assert!(serde_json::from_value::<ExtensionResponse>(response).is_err());
        // Existing deny_unknown_fields responses must remain strict.
        assert!(serde_json::from_str::<ExtensionResponse>(r#"{"op":"host_status","build":{"version":"1","commit":"x","dirty":null,"build_time_unix":null},"pty_retention":{}}"#).is_err());
    }
}
