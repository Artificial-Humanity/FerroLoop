//! Opt-in minor-one messages and bounded reliable framing. PTY payload codecs
//! remain unavailable until the separately reviewed snapshot-schema gate.
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PtyOptions {
    pub cols: u16,
    pub rows: u16,
    pub scrollback_rows: u16,
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
    ListSessions,
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
            Self::SpawnSession { spec, mode, pty } => {
                if *mode == SessionMode::Pty {
                    return Err(HostError::Unsupported {
                        subject: "PTY backend/schema",
                    });
                }
                if spec.kind != SpawnKind::Pipe || pty.is_some() {
                    return Err(HostError::Protocol(
                        "contradictory session mode/options".into(),
                    ));
                }
                spec.validate()?;
            }
            Self::SendLine { line, .. } if line.len() > MAX_RAW_INPUT => {
                return Err(HostError::EventTooLarge);
            }
            Self::Write { .. } | Self::Resize { .. } | Self::Attach { .. } => {
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
            Self::Sessions { records } if records.len() > 64 => Err(HostError::EventTooLarge),
            Self::Attachment { .. } | Self::Resized { .. } => Err(HostError::Unsupported {
                subject: "PTY backend/schema",
            }),
            _ => Ok(()),
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReliableKind {
    Line,
    Lifecycle,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExtensionFrame {
    ReliablePart {
        seq: u64,
        kind: ReliableKind,
        more: bool,
        bytes: BoundedBytes,
    },
    Notice {
        code: ExtensionRefusal,
    },
}

pub fn encode_extension_frame(
    minor: u16,
    event: &ExtensionFrame,
) -> Result<crate::Frame, HostError> {
    require_minor(minor)?;
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
    let encoded_bytes = match &event {
        ExtensionFrame::ReliablePart { bytes, .. } => bytes.as_slice().len().div_ceil(3) * 4,
        ExtensionFrame::Notice { .. } => 0,
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
    pub seq: u64,
    pub kind: ReliableKind,
    pub bytes: Vec<u8>,
}
/// One ticket-bound stream. Stream identity is supplied by the authenticated
/// subscription, not an untrusted per-part field. No partial event is yielded.
pub struct ReliableReassembler {
    after: u64,
    stream: Option<StreamId>,
    open: Option<(u64, ReliableKind, usize, Vec<u8>)>,
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
        let ExtensionFrame::ReliablePart {
            seq,
            kind,
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
            ReliableKind::Line => crate::MAX_LINE_BYTES,
            ReliableKind::Lifecycle => crate::MAX_EVENT_ENVELOPE,
        };
        if bytes.as_slice().is_empty() && *more {
            return Err(HostError::Protocol("empty continuing fragment".into()));
        }
        if let Some((open_seq, open_kind, count, body)) = &mut self.open {
            if *open_seq != *seq || open_kind != kind {
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
            self.open = Some((*seq, *kind, 1, body));
        }
        if *more {
            return Ok(None);
        }
        let (_, kind, _, body) = self.open.take().expect("open fragment");
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
            seq: *seq,
            kind,
            bytes: body,
        }))
    }
}
