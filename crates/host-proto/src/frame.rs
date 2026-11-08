//! Length-prefixed frames. Control messages are one UTF-8 JSON frame.
//! Line fragments carry raw bytes behind a fixed header.

use std::io::{Read, Write};

use crate::types::{
    HostError, HostEvent, HostRequest, HostResponse, LINE_HEADER_LEN, MAX_EVENT_ENVELOPE,
    MAX_FRAGMENTS, MAX_FRAME_PAYLOAD, MAX_LINE_BYTES, MAX_REASSEMBLY_BYTES, MAX_UNAUTH_PAYLOAD,
    SessionEpoch, SessionId,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameKind {
    Challenge = 1,
    Hello = 2,
    Proof = 3,
    Welcome = 4,
    Request = 5,
    Response = 6,
    Line = 7,
    Event = 8,
    ExtensionEvent = 9,
}

impl FrameKind {
    pub fn from_u8(byte: u8) -> Result<Self, HostError> {
        Ok(match byte {
            1 => Self::Challenge,
            2 => Self::Hello,
            3 => Self::Proof,
            4 => Self::Welcome,
            5 => Self::Request,
            6 => Self::Response,
            7 => Self::Line,
            8 => Self::Event,
            9 => Self::ExtensionEvent,
            _ => {
                return Err(HostError::Protocol(format!("unknown frame type {byte}")));
            }
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub kind: FrameKind,
    pub payload: Vec<u8>,
}

/// `len` is the on-wire length field: 1 type byte plus the payload.
/// Checked in `u64` before any `usize` cast or allocation.
pub fn checked_frame_len(len: u32) -> Result<usize, HostError> {
    checked_len(len, MAX_FRAME_PAYLOAD)
}

pub fn checked_unauth_len(len: u32) -> Result<usize, HostError> {
    checked_len(len, MAX_UNAUTH_PAYLOAD)
}

fn checked_len(len: u32, max_payload: usize) -> Result<usize, HostError> {
    let len64 = u64::from(len);
    let max = 1u64
        .checked_add(max_payload as u64)
        .ok_or(HostError::FrameLength)?;
    if len64 == 0 || len64 > max {
        return Err(HostError::FrameLength);
    }
    let total = 4u64.checked_add(len64).ok_or(HostError::FrameLength)?;
    let _ = usize::try_from(total).map_err(|_| HostError::FrameLength)?;
    usize::try_from(len64).map_err(|_| HostError::FrameLength)
}

/// Total bytes on the wire for a payload of `payload_len` (the type byte is extra).
pub fn frame_total_len(payload_len: u64) -> Result<u64, HostError> {
    if payload_len > MAX_FRAME_PAYLOAD as u64 {
        return Err(HostError::FrameLength);
    }
    4u64.checked_add(1)
        .and_then(|n| n.checked_add(payload_len))
        .ok_or(HostError::FrameLength)
}

pub fn write_frame(writer: &mut impl Write, frame: &Frame) -> Result<(), HostError> {
    let body = 1usize
        .checked_add(frame.payload.len())
        .ok_or(HostError::FrameLength)?;
    let len = u32::try_from(body).map_err(|_| HostError::FrameLength)?;
    let _ = checked_frame_len(len)?;
    writer.write_all(&len.to_be_bytes())?;
    writer.write_all(&[frame.kind as u8])?;
    writer.write_all(&frame.payload)?;
    Ok(())
}

pub fn read_frame(reader: &mut impl Read) -> Result<Frame, HostError> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf);
    let body_len = checked_frame_len(len)?;
    let mut body = vec![0u8; body_len];
    reader.read_exact(&mut body)?;
    decode_body(body)
}

pub(crate) fn decode_body(mut body: Vec<u8>) -> Result<Frame, HostError> {
    if body.is_empty() {
        return Err(HostError::FrameLength);
    }
    let kind = FrameKind::from_u8(body[0])?;
    body.remove(0);
    Ok(Frame {
        kind,
        payload: body,
    })
}

pub fn encode_request(request: &HostRequest) -> Result<Frame, HostError> {
    encode_request_for_minor(0, request)
}

pub fn encode_request_for_minor(minor: u16, request: &HostRequest) -> Result<Frame, HostError> {
    if let HostRequest::Extended { request, .. } = request {
        crate::extension::require_minor(minor)?;
        request.validate()?;
    }
    if let HostRequest::Spawn { spec, .. } = request {
        spec.validate()?;
    }
    if !crate::supports_message(minor, request.op_name()) && request.op_name() != "resize" {
        return Err(HostError::Protocol("unknown request".into()));
    }
    let payload =
        serde_json::to_vec(request).map_err(|err| HostError::Protocol(err.to_string()))?;
    if payload.len() > MAX_FRAME_PAYLOAD {
        return Err(HostError::FrameLength);
    }
    Ok(Frame {
        kind: FrameKind::Request,
        payload,
    })
}

pub fn decode_request(frame: &Frame) -> Result<HostRequest, HostError> {
    decode_request_for_minor(0, frame)
}

pub fn decode_request_for_minor(minor: u16, frame: &Frame) -> Result<HostRequest, HostError> {
    if frame.kind != FrameKind::Request {
        return Err(HostError::Protocol("expected a request frame".into()));
    }
    if frame.payload.len() > MAX_FRAME_PAYLOAD {
        return Err(HostError::FrameLength);
    }
    let request: HostRequest = serde_json::from_slice(&frame.payload)
        .map_err(|err| HostError::Protocol(err.to_string()))?;
    if let HostRequest::Extended { request, .. } = &request {
        crate::extension::require_minor(minor)?;
        request.validate()?;
    }
    if let HostRequest::Spawn { spec, .. } = &request {
        spec.validate()?;
    }
    Ok(request)
}

pub fn encode_response(response: &HostResponse) -> Result<Frame, HostError> {
    encode_response_for_minor(0, response)
}

pub fn encode_response_for_minor(minor: u16, response: &HostResponse) -> Result<Frame, HostError> {
    if let HostResponse::Extended { response, .. } = response {
        crate::extension::require_minor(minor)?;
        response.validate()?;
    }
    let payload = if matches!(
        response,
        HostResponse::Extended {
            response: crate::ExtensionResponse::Sessions { .. }
                | crate::ExtensionResponse::RenderedCapture { .. },
            ..
        }
    ) {
        // List admission precharges this exact capacity. Serialize into a slice:
        // an oversized response fails without Vec's amortized growth or a second
        // JSON allocation. Truncation retains the charged capacity through write.
        let mut payload = vec![0u8; MAX_FRAME_PAYLOAD];
        let mut remaining = payload.as_mut_slice();
        serde_json::to_writer(&mut remaining, response)
            .map_err(|err| HostError::Protocol(err.to_string()))?;
        let len = MAX_FRAME_PAYLOAD - remaining.len();
        payload.truncate(len);
        payload
    } else {
        serde_json::to_vec(response).map_err(|err| HostError::Protocol(err.to_string()))?
    };
    if payload.len() > MAX_FRAME_PAYLOAD {
        return Err(HostError::FrameLength);
    }
    Ok(Frame {
        kind: FrameKind::Response,
        payload,
    })
}

pub fn decode_response(frame: &Frame) -> Result<HostResponse, HostError> {
    decode_response_for_minor(0, frame)
}

pub fn decode_response_for_minor(minor: u16, frame: &Frame) -> Result<HostResponse, HostError> {
    if frame.kind != FrameKind::Response {
        return Err(HostError::Protocol("expected a response frame".into()));
    }
    if frame.payload.len() > MAX_FRAME_PAYLOAD {
        return Err(HostError::FrameLength);
    }
    let response = serde_json::from_slice(&frame.payload)
        .map_err(|err| HostError::Protocol(err.to_string()))?;
    if let HostResponse::Extended { response, .. } = &response {
        crate::extension::require_minor(minor)?;
        response.validate()?;
    }
    Ok(response)
}

pub fn line_fragment(
    session: &SessionId,
    epoch: SessionEpoch,
    seq: u64,
    more: bool,
    raw: &[u8],
) -> Result<Frame, HostError> {
    line_fragment_with_termination(session, epoch, seq, more, true, raw)
}

pub fn line_fragment_with_termination(
    session: &SessionId,
    epoch: SessionEpoch,
    seq: u64,
    more: bool,
    terminated: bool,
    raw: &[u8],
) -> Result<Frame, HostError> {
    if raw.is_empty() && more {
        return Err(HostError::Protocol(
            "an empty raw fragment must be the sole final frame".into(),
        ));
    }
    let payload = encode_line_payload(session, epoch, seq, more, terminated, raw)?;
    if payload.len() > MAX_FRAME_PAYLOAD {
        return Err(HostError::FrameLength);
    }
    Ok(Frame {
        kind: FrameKind::Line,
        payload,
    })
}

pub fn fragment_line(
    session: &SessionId,
    epoch: SessionEpoch,
    seq: u64,
    raw: &[u8],
) -> Result<Vec<Frame>, HostError> {
    fragment_line_with_termination(session, epoch, seq, true, raw)
}

pub fn fragment_line_with_termination(
    session: &SessionId,
    epoch: SessionEpoch,
    seq: u64,
    terminated: bool,
    raw: &[u8],
) -> Result<Vec<Frame>, HostError> {
    if raw.len() > MAX_LINE_BYTES {
        return Err(HostError::EventTooLarge);
    }
    if raw.is_empty() {
        return Ok(vec![line_fragment_with_termination(
            session, epoch, seq, false, terminated, raw,
        )?]);
    }
    let chunk = MAX_FRAME_PAYLOAD - LINE_HEADER_LEN;
    let mut frames = Vec::new();
    let mut offset = 0;
    while offset < raw.len() {
        if frames.len() >= MAX_FRAGMENTS {
            return Err(HostError::EventTooLarge);
        }
        let end = (offset + chunk).min(raw.len());
        let more = end < raw.len();
        frames.push(line_fragment_with_termination(
            session,
            epoch,
            seq,
            more,
            terminated,
            &raw[offset..end],
        )?);
        offset = end;
    }
    Ok(frames)
}

fn encode_line_payload(
    session: &SessionId,
    epoch: SessionEpoch,
    seq: u64,
    more: bool,
    terminated: bool,
    raw: &[u8],
) -> Result<Vec<u8>, HostError> {
    let name = session.as_str().as_bytes();
    if name.len() > 64 {
        return Err(HostError::Protocol(
            "session id does not fit the line header".into(),
        ));
    }
    let total = LINE_HEADER_LEN
        .checked_add(raw.len())
        .ok_or(HostError::FrameLength)?;
    if total > MAX_FRAME_PAYLOAD {
        return Err(HostError::FrameLength);
    }
    let mut payload = vec![0u8; LINE_HEADER_LEN + raw.len()];
    payload[0] = u8::try_from(name.len()).unwrap_or(0);
    payload[1..1 + name.len()].copy_from_slice(name);
    payload[65..73].copy_from_slice(&epoch.0.to_be_bytes());
    payload[73..81].copy_from_slice(&seq.to_be_bytes());
    payload[81] = u8::from(more) | (u8::from(!terminated) << 1);
    payload[LINE_HEADER_LEN..].copy_from_slice(raw);
    Ok(payload)
}

struct LineHeader<'a> {
    session: SessionId,
    epoch: SessionEpoch,
    seq: u64,
    more: bool,
    terminated: bool,
    raw: &'a [u8],
}

fn decode_line_header(payload: &[u8]) -> Result<LineHeader<'_>, HostError> {
    if payload.len() < LINE_HEADER_LEN || payload.len() > MAX_FRAME_PAYLOAD {
        return Err(HostError::FrameLength);
    }
    let session_len = usize::from(payload[0]);
    if session_len == 0 || session_len > 64 {
        return Err(HostError::Protocol("line session length".into()));
    }
    if payload[1 + session_len..65].iter().any(|byte| *byte != 0) {
        return Err(HostError::Protocol("line session padding".into()));
    }
    let name = std::str::from_utf8(&payload[1..1 + session_len])
        .map_err(|_| HostError::Protocol("line session is not utf-8".into()))?;
    let session = SessionId::new(name)?;
    let epoch = SessionEpoch(u64::from_be_bytes(payload[65..73].try_into().unwrap()));
    let seq = u64::from_be_bytes(payload[73..81].try_into().unwrap());
    if payload[81] & !3 != 0 {
        return Err(HostError::Protocol("line flags".into()));
    }
    let more = payload[81] & 1 != 0;
    let terminated = payload[81] & 2 == 0;
    Ok(LineHeader {
        session,
        epoch,
        seq,
        more,
        terminated,
        raw: &payload[LINE_HEADER_LEN..],
    })
}

#[derive(Debug)]
struct OpenLine {
    terminated: bool,
    session: SessionId,
    epoch: SessionEpoch,
    seq: u64,
}

#[derive(Debug)]
pub struct LineReassembler {
    open: Option<OpenLine>,
    raw: Vec<u8>,
    fragments: usize,
}

impl LineReassembler {
    pub fn new() -> Self {
        Self {
            open: None,
            raw: Vec::new(),
            fragments: 0,
        }
    }

    pub fn fragment_count(&self) -> usize {
        self.fragments
    }

    pub fn allocated(&self) -> usize {
        self.raw
            .capacity()
            .saturating_add(self.fragments.saturating_mul(LINE_HEADER_LEN))
    }

    pub fn has_open_chain(&self) -> bool {
        self.open.is_some()
    }

    /// Drop an unfinished chain. A later fragment must not continue it.
    pub fn discard_chain(&mut self) {
        self.open = None;
        self.raw = Vec::new();
        self.fragments = 0;
    }

    /// Returns `Ok(None)` for an incomplete chain and transfers the sole owned
    /// completed event with `Ok(Some(event))`. Errors produce no event; callers
    /// discard the chain on errors or disconnect to release its storage.
    pub fn push_frame(&mut self, frame: &Frame) -> Result<Option<HostEvent>, HostError> {
        self.push_frame_for_minor(crate::PROTO_MINOR, frame)
    }

    pub fn push_frame_for_minor(
        &mut self,
        minor: u16,
        frame: &Frame,
    ) -> Result<Option<HostEvent>, HostError> {
        if frame.kind != FrameKind::Line {
            if self.fragments > 0 {
                return Err(HostError::Protocol(
                    "control frame interleaved with a line chain".into(),
                ));
            }
            return Err(HostError::Protocol(
                "line reassembler received a control frame".into(),
            ));
        }
        let header = decode_line_header(&frame.payload)?;
        if !header.terminated && minor < 2 {
            return Err(HostError::Protocol(
                "stdout fragments require minor two".into(),
            ));
        }
        if header.raw.is_empty() && (header.more || self.fragments > 0) {
            return Err(HostError::Protocol(
                "empty raw fragments are only a single final empty line".into(),
            ));
        }
        if let Some(open) = &self.open
            && (open.session != header.session
                || open.epoch != header.epoch
                || open.seq != header.seq
                || open.terminated != header.terminated)
        {
            return Err(HostError::Protocol(
                "line fragment identity does not match the open chain".into(),
            ));
        }
        let new_fragments = self.fragments.saturating_add(1);
        if new_fragments > MAX_FRAGMENTS {
            return Err(HostError::EventTooLarge);
        }
        let header_bytes = new_fragments
            .checked_mul(LINE_HEADER_LEN)
            .ok_or(HostError::EventTooLarge)?;
        if header_bytes > MAX_EVENT_ENVELOPE {
            return Err(HostError::EventTooLarge);
        }
        let new_raw = self
            .raw
            .len()
            .checked_add(header.raw.len())
            .ok_or(HostError::EventTooLarge)?;
        if new_raw > MAX_LINE_BYTES {
            return Err(HostError::EventTooLarge);
        }
        let new_alloc = new_raw
            .checked_add(header_bytes)
            .ok_or(HostError::EventTooLarge)?;
        if new_alloc > MAX_REASSEMBLY_BYTES {
            return Err(HostError::EventTooLarge);
        }
        // Reserve only the needed payload, without Vec's amortized growth.
        // The requested owned capacity and envelope are bounded before growth.
        if new_raw > self.raw.capacity() {
            self.raw
                .try_reserve_exact(new_raw - self.raw.len())
                .map_err(|_| HostError::EventTooLarge)?;
        }
        if self.open.is_none() {
            self.open = Some(OpenLine {
                terminated: header.terminated,
                session: header.session.clone(),
                epoch: header.epoch,
                seq: header.seq,
            });
        }
        self.raw.extend_from_slice(header.raw);
        self.fragments = new_fragments;
        if header.more {
            return Ok(None);
        }
        let open = self
            .open
            .take()
            .ok_or_else(|| HostError::Protocol("finished a line with no identity".into()))?;
        let event = HostEvent::Line {
            terminated: open.terminated,
            session: open.session,
            session_epoch: open.epoch,
            seq: open.seq,
            bytes: std::mem::take(&mut self.raw),
        };
        self.fragments = 0;
        Ok(Some(event))
    }
}

impl Default for LineReassembler {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
pub struct ResponseCorrelator {
    last: u64,
    outstanding: Option<u64>,
}

impl ResponseCorrelator {
    pub fn new() -> Self {
        Self {
            last: 0,
            outstanding: None,
        }
    }

    pub fn submit(&mut self, id: u64) -> Result<(), HostError> {
        if id == 0 || self.outstanding.is_some() || id <= self.last {
            return Err(HostError::Protocol("request id refused".into()));
        }
        self.outstanding = Some(id);
        self.last = id;
        Ok(())
    }

    pub fn complete(&mut self, id: u64) -> Result<(), HostError> {
        if self.outstanding != Some(id) {
            return Err(HostError::Protocol("response id refused".into()));
        }
        self.outstanding = None;
        Ok(())
    }
}

impl Default for ResponseCorrelator {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
pub struct EventSeq {
    next: u64,
}

impl EventSeq {
    pub fn at(next: u64) -> Self {
        Self { next }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<u64, HostError> {
        let value = self.next;
        self.next = self
            .next
            .checked_add(1)
            .ok_or_else(|| HostError::Protocol("event seq exhausted".into()))?;
        Ok(value)
    }
}

#[cfg(test)]
mod allocation_tests {
    use super::*;

    #[test]
    fn unterminated_flag_is_accepted_and_multipart_must_agree() {
        let session = SessionId::new("fragment").unwrap();
        let mut frame = line_fragment(&session, SessionEpoch(1), 1, false, b"tail").unwrap();
        frame.payload[81] = 2;
        assert!(
            LineReassembler::new()
                .push_frame_for_minor(1, &frame)
                .is_err()
        );
        assert!(LineReassembler::new().push_frame(&frame).unwrap().is_some());
        let mut first = line_fragment(&session, SessionEpoch(1), 2, true, b"first").unwrap();
        first.payload[81] = 3;
        let last = line_fragment(&session, SessionEpoch(1), 2, false, b"last").unwrap();
        let mut assembler = LineReassembler::new();
        assert!(assembler.push_frame(&first).unwrap().is_none());
        assert!(assembler.push_frame(&last).is_err());
    }

    #[test]
    fn consecutive_maximum_lines_bound_owned_buffer_capacity() {
        let session = SessionId::new("capacity").unwrap();
        let raw = vec![b'x'; MAX_LINE_BYTES];
        let mut lines = LineReassembler::new();
        for seq in 1..=2 {
            let frames = fragment_line(&session, SessionEpoch(1), seq, &raw).unwrap();
            for (i, frame) in frames.iter().enumerate() {
                let event = lines.push_frame(frame).unwrap();
                let owned = lines.raw.capacity();
                assert_eq!(lines.allocated(), owned + lines.fragments * LINE_HEADER_LEN);
                assert!(
                    owned + lines.fragments * LINE_HEADER_LEN <= MAX_REASSEMBLY_BYTES,
                    "real allocation {owned} exceeds cap at fragment {i}"
                );
                if i + 1 == frames.len() {
                    match event.unwrap() {
                        HostEvent::Line {
                            seq: got_seq,
                            bytes,
                            ..
                        } => {
                            assert_eq!(got_seq, seq);
                            assert_eq!(bytes, raw);
                            assert!(bytes.capacity() <= MAX_LINE_BYTES);
                        }
                        other => panic!("{other:?}"),
                    }
                    assert_eq!(owned, 0, "completed bytes must transfer to caller");
                } else {
                    assert!(event.is_none());
                }
            }
        }
        let open = line_fragment(&session, SessionEpoch(1), 3, true, b"pending").unwrap();
        lines.push_frame(&open).unwrap();
        let wrong = line_fragment(&session, SessionEpoch(1), 4, false, b"bad").unwrap();
        assert!(lines.push_frame(&wrong).is_err());
        lines.discard_chain();
        assert_eq!(lines.raw.capacity(), 0);
        assert_eq!(lines.allocated(), 0);
        lines.push_frame(&open).unwrap();
        lines.discard_chain();
        assert_eq!(lines.raw.capacity(), 0, "disconnect discards owned storage");
    }
}
