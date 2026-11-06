//! Frame bounds, line reassembly, and typed-message limits from Task 1.
use std::io::{self, Read};
use std::path::PathBuf;
use std::time::Duration;

use fl_host_proto::{
    ChildExit, ChildRecord, ChildState, ControllerLease, DEFAULT_MAX_SESSIONS, EventSeq,
    FRAME_READ_TIMEOUT, Frame, FrameKind, HANDSHAKE_TIMEOUT, HostError, HostEvent, HostRequest,
    HostResponse, InputState, LineReassembler, MAX_ARG_BYTES, MAX_ARGV, MAX_ARGV_AGGREGATE,
    MAX_CWD_BYTES, MAX_ENV_AGGREGATE, MAX_ENV_ENTRIES, MAX_ENV_NAME, MAX_ENV_VALUE,
    MAX_EVENT_ENVELOPE, MAX_FRAGMENTS, MAX_FRAME_PAYLOAD, MAX_LINE_BYTES, MAX_REASSEMBLY_BYTES,
    MAX_STDOUT_LINE, MAX_SUBSCRIBERS, MAX_UNAUTH_PAYLOAD, MAX_WINDOWS_COMMAND_UNITS, RequestId,
    ResponseCorrelator, STDERR_TAIL_BYTES, STDIN_QUEUE_BYTES, STDIN_QUEUE_FRAMES,
    STDIN_WRITE_TIMEOUT, SendBlock, SendOutcome, SessionEpoch, SessionId, SignalKind, SpawnKind,
    SpawnSpec, WRITE_TIMEOUT, checked_frame_len, checked_unauth_len, decode_request,
    encode_request, fragment_line, frame_total_len, read_frame, windows_command_line,
    windows_environment_block,
};

struct HeaderOnly {
    data: Vec<u8>,
    pos: usize,
}

impl Read for HeaderOnly {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= 4 {
            return Err(io::Error::other("read past the length prefix"));
        }
        let n = (4 - self.pos).min(buf.len());
        buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

fn header_reader(len: u32) -> HeaderOnly {
    HeaderOnly {
        data: len.to_be_bytes().to_vec(),
        pos: 0,
    }
}

fn session(name: &str) -> SessionId {
    SessionId::new(name).unwrap()
}

#[test]
fn frame_rejects_oversize_before_payload_read() {
    let len = (1 + MAX_FRAME_PAYLOAD as u32).saturating_add(1);
    let mut reader = header_reader(len);
    let err = read_frame(&mut reader).unwrap_err();
    assert!(matches!(err, HostError::FrameLength), "{err}");
    assert_eq!(reader.pos, 4);
    assert!(checked_frame_len(1 + MAX_FRAME_PAYLOAD as u32).is_ok());
    assert!(checked_frame_len(1 + MAX_FRAME_PAYLOAD as u32 + 1).is_err());
}

#[test]
fn frame_rejects_zero_and_overflow_lengths() {
    for len in [0u32, u32::MAX, u32::MAX - 4, u32::MAX - 5] {
        let mut reader = header_reader(len);
        let err = read_frame(&mut reader).unwrap_err();
        assert!(matches!(err, HostError::FrameLength), "{len}: {err}");
        assert_eq!(reader.pos, 4, "{len}");
    }
    assert!(frame_total_len(u64::MAX).is_err());
    assert!(frame_total_len(u64::from(u32::MAX)).is_err());
    assert!(frame_total_len(MAX_FRAME_PAYLOAD as u64).is_ok());
    assert!(frame_total_len(MAX_FRAME_PAYLOAD as u64 + 1).is_err());
    let exact = frame_total_len(MAX_FRAME_PAYLOAD as u64).unwrap();
    assert_eq!(exact, 4 + 1 + MAX_FRAME_PAYLOAD as u64);
}

#[test]
fn event_fragments_round_trip_at_max_line() {
    let raw = vec![0x61u8; MAX_LINE_BYTES];
    let frames = fragment_line(&session("line-1"), SessionEpoch(7), 3, &raw).unwrap();
    assert!(frames.len() > 1);
    assert!(frames.len() <= MAX_FRAGMENTS);
    for frame in &frames {
        assert_eq!(frame.kind, FrameKind::Line);
        assert!(frame.payload.len() <= MAX_FRAME_PAYLOAD);
        assert!(frame.payload.len() < 1 + MAX_FRAME_PAYLOAD);
    }
    let mut reassembler = LineReassembler::new();
    let mut got = None;
    for frame in &frames {
        got = reassembler.push_frame(frame).unwrap();
    }
    match got.unwrap() {
        HostEvent::Line {
            session,
            session_epoch,
            seq,
            bytes,
        } => {
            assert_eq!(session.as_str(), "line-1");
            assert_eq!(session_epoch, SessionEpoch(7));
            assert_eq!(seq, 3);
            assert_eq!(bytes, raw);
        }
        other => panic!("expected a line, got {other:?}"),
    }
    assert_eq!(reassembler.fragment_count(), 0);
}

#[test]
fn truncated_event_never_becomes_a_line() {
    let big = fragment_line(&session("s"), SessionEpoch(1), 4, &vec![0x80; 300_000]).unwrap();
    assert!(big.len() > 1);
    let mut reassembler = LineReassembler::new();
    assert!(reassembler.push_frame(&big[0]).unwrap().is_none());
    assert!(reassembler.completed().is_none());
    // The unfinished bytes are not a Line, even if a later control frame arrives.
    let partial = Frame {
        kind: FrameKind::Request,
        payload: br#"{"op":"list","id":1}"#.to_vec(),
    };
    assert!(reassembler.push_frame(&partial).is_err());
    assert!(reassembler.completed().is_none());
}

#[test]
fn line_is_raw_bytes_non_utf8_round_trips() {
    let raw = [0xff, 0xfe, 0x00, 0x80, b'\n', 0xfd, 0xc3, 0x28];
    let frames = fragment_line(&session("raw"), SessionEpoch(2), 9, &raw).unwrap();
    assert_eq!(frames.len(), 1);
    let payload = &frames[0].payload;
    assert!(
        payload.windows(raw.len()).any(|window| window == raw),
        "raw bytes must appear literally, not as text or base64"
    );
    assert!(!payload.windows(4).any(|window| window == b"base64"));
    let mut reassembler = LineReassembler::new();
    match reassembler.push_frame(&frames[0]).unwrap().unwrap() {
        HostEvent::Line { bytes, .. } => assert_eq!(bytes, raw),
        other => panic!("{other:?}"),
    }
    let mut encoded = Vec::new();
    fl_host_proto::write_frame(&mut encoded, &frames[0]).unwrap();
    assert!(!encoded.windows(raw.len()).any(|window| window == b"/w=="));
}

#[test]
fn interleaved_fragment_chains_refused() {
    let first = fragment_line(&session("alpha"), SessionEpoch(1), 1, &vec![b'a'; 300_000]).unwrap();
    let other = fragment_line(&session("beta"), SessionEpoch(1), 1, b"nope").unwrap();
    let mut reassembler = LineReassembler::new();
    assert!(reassembler.push_frame(&first[0]).unwrap().is_none());
    let before = reassembler.allocated();
    let err = reassembler.push_frame(&other[0]).unwrap_err();
    assert!(matches!(err, HostError::Protocol(_)), "{err}");
    assert_eq!(reassembler.allocated(), before);
    assert!(reassembler.completed().is_none());
    let different_seq = fragment_line(&session("alpha"), SessionEpoch(1), 2, b"x").unwrap();
    assert!(reassembler.push_frame(&different_seq[0]).is_err());
    assert!(reassembler.completed().is_none());
}

#[test]
fn reassembly_cap_checked_before_append() {
    let mut reassembler = LineReassembler::new();
    let frames = fragment_line(
        &session("cap"),
        SessionEpoch(1),
        1,
        &vec![b'z'; MAX_LINE_BYTES],
    )
    .unwrap();
    for frame in &frames[..frames.len() - 1] {
        let last_open = reassembler.push_frame(frame).unwrap();
        assert!(last_open.is_none());
    }
    let before = reassembler.allocated();
    // One byte over the raw cap, same identity, must fail before the buffer grows.
    let mut over = frames.last().unwrap().clone();
    over.payload.push(b'z');
    let err = reassembler.push_frame(&over).unwrap_err();
    assert!(
        matches!(err, HostError::EventTooLarge | HostError::FrameLength),
        "{err}"
    );
    assert_eq!(reassembler.allocated(), before);
    assert!(reassembler.completed().is_none());
}

#[test]
fn fragment_count_and_connection_allocation_are_bounded() {
    assert_eq!(MAX_FRAGMENTS, 64);
    assert_eq!(MAX_REASSEMBLY_BYTES, MAX_LINE_BYTES + MAX_EVENT_ENVELOPE);
    let mut reassembler = LineReassembler::new();
    for index in 0..MAX_FRAGMENTS {
        let frame = fl_host_proto::line_fragment(
            &session("frag"),
            SessionEpoch(1),
            8,
            true,
            &[u8::try_from(index).unwrap_or(1)],
        )
        .unwrap();
        assert!(reassembler.push_frame(&frame).unwrap().is_none());
    }
    let before = reassembler.allocated();
    let extra =
        fl_host_proto::line_fragment(&session("frag"), SessionEpoch(1), 8, false, b"Z").unwrap();
    assert!(reassembler.push_frame(&extra).is_err());
    assert_eq!(reassembler.allocated(), before);
    assert_eq!(reassembler.fragment_count(), MAX_FRAGMENTS);
    assert!(reassembler.completed().is_none());
}

#[test]
fn control_fragmentation_refused() {
    let request = HostRequest::Send {
        id: RequestId(1),
        session: session("s"),
        expected_epoch: SessionEpoch(1),
        lease: ControllerLease(1),
        line: "x".repeat(MAX_FRAME_PAYLOAD + 1),
    };
    let err = encode_request(&request).unwrap_err();
    assert!(
        matches!(err, HostError::FrameLength | HostError::Protocol(_)),
        "{err}"
    );
    let partial = Frame {
        kind: FrameKind::Request,
        payload: br#"{"op":"list""#.to_vec(),
    };
    assert!(decode_request(&partial).is_err());
    let mut reassembler = LineReassembler::new();
    let open = fragment_line(&session("s"), SessionEpoch(1), 1, &vec![b'q'; 300_000]).unwrap();
    assert!(reassembler.push_frame(&open[0]).unwrap().is_none());
    let before = reassembler.allocated();
    assert!(reassembler.push_frame(&partial).is_err());
    assert_eq!(reassembler.allocated(), before);
    assert!(reassembler.completed().is_none());
}

#[test]
fn unknown_or_duplicate_response_id_refused() {
    let mut correlator = ResponseCorrelator::new();
    assert!(correlator.submit(0).is_err());
    correlator.submit(1).unwrap();
    assert!(correlator.submit(1).is_err());
    assert!(
        correlator.submit(2).is_err(),
        "one response is still outstanding"
    );
    correlator.complete(1).unwrap();
    assert!(correlator.complete(1).is_err(), "duplicate response");
    correlator.submit(4).unwrap();
    assert!(correlator.complete(5).is_err(), "unknown response id");
    correlator.complete(4).unwrap();
    assert!(correlator.submit(4).is_err(), "reused id");
    assert!(correlator.submit(3).is_err(), "not strictly increasing");
    let mut exhausted = ResponseCorrelator::new();
    exhausted.submit(u64::MAX).unwrap();
    exhausted.complete(u64::MAX).unwrap();
    assert!(exhausted.submit(u64::MAX).is_err());
    let mut seq = EventSeq::at(u64::MAX - 1);
    assert_eq!(seq.next().unwrap(), u64::MAX - 1);
    assert!(seq.next().is_err());
}

#[test]
fn spawn_field_limits_refused_before_spawn() {
    let ok = base_spec(1, 1, 1, 1, 1);
    assert!(ok.validate().is_ok());
    assert!(base_spec(0, 1, 1, 1, 1).validate().is_err());
    assert!(base_spec(MAX_ARGV, 1, 1, 1, 1).validate().is_ok());
    assert!(base_spec(MAX_ARGV + 1, 1, 1, 1, 1).validate().is_err());
    assert!(base_spec(1, MAX_ARG_BYTES, 1, 1, 1).validate().is_ok());
    assert!(base_spec(1, MAX_ARG_BYTES + 1, 1, 1, 1).validate().is_err());
    assert!(base_spec(4, MAX_ARG_BYTES, 1, 1, 1).validate().is_ok());
    assert!(argv_aggregate_one_over().validate().is_err());
    assert!(env_count(MAX_ENV_ENTRIES).validate().is_ok());
    assert!(env_count(MAX_ENV_ENTRIES + 1).validate().is_err());
    assert!(allow_count(MAX_ENV_ENTRIES).validate().is_ok());
    assert!(allow_count(MAX_ENV_ENTRIES + 1).validate().is_err());
    assert!(name_len(MAX_ENV_NAME).validate().is_ok());
    assert!(name_len(MAX_ENV_NAME + 1).validate().is_err());
    assert!(value_len(MAX_ENV_VALUE).validate().is_ok());
    assert!(value_len(MAX_ENV_VALUE + 1).validate().is_err());
    assert!(env_aggregate_exact().validate().is_ok());
    assert!(env_aggregate_one_over().validate().is_err());
    assert!(cwd_len(MAX_CWD_BYTES).validate().is_ok());
    assert!(cwd_len(MAX_CWD_BYTES + 1).validate().is_err());
    let mut nul = base_spec(1, 1, 1, 1, 1);
    nul.argv[0] = "a\0b".into();
    assert!(nul.validate().is_err());
    let mut bad_name = base_spec(1, 1, 1, 1, 1);
    bad_name.env.insert("A=B".into(), "v".into());
    bad_name.env_allow.insert("A=B".into());
    assert!(bad_name.validate().is_err());
    let mut missing = base_spec(1, 1, 1, 1, 1);
    missing.env.insert("SECRET".into(), "x".into());
    assert!(missing.validate().is_err());
    let mut pty = base_spec(1, 1, 1, 1, 1);
    pty.kind = SpawnKind::Pty;
    assert!(matches!(
        pty.validate(),
        Err(HostError::Unsupported { subject }) if subject == "pty"
    ));
    assert!(SessionId::new("").is_err());
    assert!(SessionId::new(&"a".repeat(64)).is_ok());
    assert!(SessionId::new(&"a".repeat(65)).is_err());
    assert!(SessionId::new("has/slash").is_err());
    assert!(SessionId::new("ok.Name_1-2").is_ok());
    assert_eq!(MAX_ARGV_AGGREGATE, 128 * 1024);
    assert_eq!(MAX_ENV_AGGREGATE, 64 * 1024);
}

#[test]
fn windows_quoted_command_line_limit() {
    let ok_arg = "a".repeat(MAX_WINDOWS_COMMAND_UNITS - 1);
    let ok = windows_command_line(&[ok_arg]).unwrap();
    assert_eq!(ok.encode_utf16().count() + 1, MAX_WINDOWS_COMMAND_UNITS);
    let over = "a".repeat(MAX_WINDOWS_COMMAND_UNITS);
    assert!(windows_command_line(&[over]).is_err());
    let quoted = windows_command_line(&["a b".into(), r#"c:\win"#.into()]).unwrap();
    assert!(quoted.contains('"'));
    assert!(!quoted.contains('\n'));
    let value = "b".repeat(MAX_WINDOWS_COMMAND_UNITS - 4);
    // "A=" + value + NUL + final NUL = 2 + value + 2.
    let block = windows_environment_block(&[("A".into(), value)]).unwrap();
    assert_eq!(block.len(), MAX_WINDOWS_COMMAND_UNITS);
    assert_eq!(*block.last().unwrap(), 0);
    let too_big = "b".repeat(MAX_WINDOWS_COMMAND_UNITS);
    assert!(windows_environment_block(&[("A".into(), too_big)]).is_err());
}

#[test]
fn type_round_trips_and_fenced_input_is_permanent() {
    let spec = base_spec(1, 1, 1, 1, 1);
    let samples = [
        HostRequest::Spawn {
            id: RequestId(7),
            spec: spec.clone(),
        },
        HostRequest::List { id: RequestId(1) },
        HostRequest::Kill {
            id: RequestId(2),
            session: session("s"),
            expected_epoch: SessionEpoch(4),
            lease: None,
        },
        HostRequest::Kill {
            id: RequestId(8),
            session: session("s"),
            expected_epoch: SessionEpoch(4),
            lease: Some(ControllerLease(3)),
        },
        HostRequest::Events {
            id: RequestId(3),
            session: Some(session("s")),
        },
        HostRequest::ClaimLease {
            id: RequestId(4),
            session: session("s"),
            expected_epoch: SessionEpoch(4),
        },
        HostRequest::Send {
            id: RequestId(5),
            session: session("s"),
            expected_epoch: SessionEpoch(4),
            lease: ControllerLease(3),
            line: "{\"type\":\"user\"}".into(),
        },
        HostRequest::Signal {
            id: RequestId(6),
            session: session("s"),
            expected_epoch: SessionEpoch(4),
            lease: ControllerLease(3),
            signal: SignalKind::Interrupt,
        },
    ];
    for request in samples {
        let bytes = serde_json::to_vec(&request).unwrap();
        assert!(!bytes.windows(12).any(|window| window == b"thread_epoch"));
        let back: HostRequest = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back, request);
        assert_eq!(back.request_id(), request.request_id());
    }
    let claim = serde_json::to_value(&HostRequest::ClaimLease {
        id: RequestId(4),
        session: session("s"),
        expected_epoch: SessionEpoch(4),
    })
    .unwrap();
    assert!(claim.get("lease").is_none());
    assert_eq!(claim["expected_epoch"], 4);
    assert!(serde_json::from_str::<HostRequest>(r#"{"op":"hello"}"#).is_err());
    let signal = serde_json::to_value(&HostRequest::Signal {
        id: RequestId(6),
        session: session("s"),
        expected_epoch: SessionEpoch(4),
        lease: ControllerLease(3),
        signal: SignalKind::Terminate,
    })
    .unwrap();
    assert!(signal.get("expected_epoch").is_some());
    assert!(signal.get("lease").is_some());
    let record = ChildRecord {
        session: session("s"),
        session_epoch: SessionEpoch(2),
        kind: SpawnKind::Pipe,
        pid: 44,
        input: InputState::Writable,
        started_at_ms: 10,
        exit: None,
        consumer_stalled: false,
    };
    for state in [
        ChildState::Running(record.clone()),
        ChildState::LeaderExited(ChildRecord {
            exit: Some(ChildExit {
                code: Some(1),
                signal: None,
            }),
            input: InputState::Fenced,
            ..record.clone()
        }),
        ChildState::CleanupComplete(ChildRecord {
            exit: Some(ChildExit {
                code: None,
                signal: Some(15),
            }),
            ..record.clone()
        }),
    ] {
        let bytes = serde_json::to_vec(&state).unwrap();
        assert_eq!(serde_json::from_slice::<ChildState>(&bytes).unwrap(), state);
    }
    let leader = ChildState::LeaderExited(record.clone());
    let cleanup = ChildState::CleanupComplete(record);
    assert_ne!(leader, cleanup);
    assert_eq!(InputState::Fenced.after_lease(), InputState::Fenced);
    assert_eq!(InputState::Writable.after_lease(), InputState::Writable);
    assert!(
        InputState::Fenced
            .after_respawn(SessionEpoch(4), SessionEpoch(4))
            .is_err()
    );
    assert_eq!(
        InputState::Fenced
            .after_respawn(SessionEpoch(4), SessionEpoch(5))
            .unwrap(),
        InputState::Writable
    );
    assert!(!ControllerLease::generation_bump_settles_os_write());
    let outcome = SendOutcome::NotWritten {
        reason: SendBlock::SessionFenced,
    };
    let bytes = serde_json::to_vec(&outcome).unwrap();
    assert_eq!(
        serde_json::from_slice::<SendOutcome>(&bytes).unwrap(),
        outcome
    );
    let response = HostResponse::Sent {
        id: RequestId(5),
        outcome: SendOutcome::Ambiguous {
            reason: SendBlock::Timeout,
        },
    };
    let bytes = serde_json::to_vec(&response).unwrap();
    assert_eq!(
        serde_json::from_slice::<HostResponse>(&bytes).unwrap(),
        response
    );
    assert_eq!(HANDSHAKE_TIMEOUT, Duration::from_secs(5));
    assert_eq!(FRAME_READ_TIMEOUT, Duration::from_secs(10));
    assert_eq!(WRITE_TIMEOUT, Duration::from_secs(5));
    assert_eq!(STDIN_WRITE_TIMEOUT, Duration::from_secs(5));
    assert_eq!(MAX_STDOUT_LINE, 1024 * 1024);
    assert_eq!(MAX_LINE_BYTES, 1024 * 1024);
    assert_eq!(MAX_EVENT_ENVELOPE, 4 * 1024);
    assert_eq!(STDIN_QUEUE_FRAMES, 64);
    assert_eq!(STDIN_QUEUE_BYTES, 256 * 1024);
    assert_eq!(STDERR_TAIL_BYTES, 64 * 1024);
    assert_eq!(DEFAULT_MAX_SESSIONS, 64);
    assert_eq!(MAX_SUBSCRIBERS, 16);
    assert_eq!(MAX_UNAUTH_PAYLOAD, 4 * 1024);
    assert!(checked_unauth_len(1 + MAX_UNAUTH_PAYLOAD as u32).is_ok());
    assert!(checked_unauth_len(1 + MAX_UNAUTH_PAYLOAD as u32 + 1).is_err());
}

fn base_spec(
    argc: usize,
    arg_len: usize,
    env_n: usize,
    name_len: usize,
    value_len: usize,
) -> SpawnSpec {
    let mut env = std::collections::BTreeMap::new();
    let mut env_allow = std::collections::BTreeSet::new();
    for index in 0..env_n.max(1) {
        let name = format!(
            "N{index:0width$}",
            width = name_len.saturating_sub(1).max(1)
        );
        let name = if name.len() > name_len {
            name.chars().take(name_len).collect()
        } else {
            format!("{name}{}", "x".repeat(name_len - name.len()))
        };
        if index < env_n {
            env.insert(name.clone(), "v".repeat(value_len));
            env_allow.insert(name);
        }
    }
    if env_n == 0 {
        env.clear();
        env_allow.clear();
    }
    SpawnSpec {
        session: session("s"),
        kind: SpawnKind::Pipe,
        argv: vec!["a".repeat(arg_len); argc],
        cwd: PathBuf::from("/work"),
        env,
        env_allow,
    }
}

fn argv_aggregate_one_over() -> SpawnSpec {
    let mut spec = base_spec(4, MAX_ARG_BYTES, 0, 1, 0);
    spec.argv.push("b".into());
    spec
}

fn env_count(count: usize) -> SpawnSpec {
    // One-byte names collapse in `base_spec` and cannot be unique at 129.
    let mut spec = base_spec(1, 1, 0, 1, 0);
    for index in 0..count {
        let name = format!("e{index:03}");
        spec.env.insert(name.clone(), "v".into());
        spec.env_allow.insert(name);
    }
    spec
}

fn allow_count(count: usize) -> SpawnSpec {
    let mut spec = base_spec(1, 1, 1, 1, 1);
    spec.env_allow = (0..count).map(|index| format!("A{index}")).collect();
    spec.env_allow.insert("Nx".into());
    // Keep the override inside the allow list without exceeding `count` when
    // the caller is at the limit: rebuild so allow len is exact.
    if spec.env_allow.len() != count {
        spec.env.clear();
        spec.env_allow = (0..count).map(|index| format!("A{index}")).collect();
    }
    spec
}

fn name_len(len: usize) -> SpawnSpec {
    base_spec(1, 1, 1, len, 1)
}

fn value_len(len: usize) -> SpawnSpec {
    base_spec(1, 1, 1, 1, len)
}

fn env_aggregate_exact() -> SpawnSpec {
    // Two values of 32768 and 32766 plus two 1-byte unique names = 65536.
    let mut spec = base_spec(1, 1, 0, 1, 0);
    spec.env.insert("A".into(), "b".repeat(MAX_ENV_VALUE));
    spec.env.insert("B".into(), "c".repeat(MAX_ENV_VALUE - 2));
    spec.env_allow.insert("A".into());
    spec.env_allow.insert("B".into());
    spec
}

fn env_aggregate_one_over() -> SpawnSpec {
    let mut spec = env_aggregate_exact();
    spec.env.insert("B".into(), "c".repeat(MAX_ENV_VALUE - 1));
    spec
}

fn cwd_len(len: usize) -> SpawnSpec {
    let mut spec = base_spec(1, 1, 0, 1, 0);
    spec.cwd = PathBuf::from("c".repeat(len));
    spec
}
