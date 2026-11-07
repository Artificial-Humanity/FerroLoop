//! Mutual nonce challenge. Token bytes stay off the wire.
//!
//! Transcript is `endpoint_len:u32be || endpoint || major:u16be || minor:u16be || ns || nc`.
//! Proofs are HMAC-SHA256 under `fl-host/auth/1/client` and `fl-host/auth/1/server`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::frame::{Frame, FrameKind, write_frame};
use crate::types::{AuthToken, CLIENT_DOMAIN, HostError, SERVER_DOMAIN};

#[derive(Debug, Clone, Copy)]
pub struct Deadlines {
    pub handshake: Duration,
    pub frame: Duration,
    pub write: Duration,
}

pub struct HandshakeParams<'a> {
    pub identity: &'a [u8],
    pub token: &'a AuthToken,
    pub deadlines: Deadlines,
    pub daemon_version: &'a str,
    pub major: u16,
    pub minor: u16,
    pub capture_writes: Option<Arc<Mutex<Vec<u8>>>>,
}

pub fn auth_transcript(
    identity: &[u8],
    major: u16,
    minor: u16,
    ns: &[u8; 32],
    nc: &[u8; 32],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + identity.len() + 4 + 64);
    let len = u32::try_from(identity.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(identity);
    out.extend_from_slice(&major.to_be_bytes());
    out.extend_from_slice(&minor.to_be_bytes());
    out.extend_from_slice(ns);
    out.extend_from_slice(nc);
    out
}

pub fn client_proof(token: &[u8; 32], transcript: &[u8]) -> [u8; 32] {
    mac(token, CLIENT_DOMAIN, transcript)
}

pub fn server_proof(token: &[u8; 32], transcript: &[u8]) -> [u8; 32] {
    mac(token, SERVER_DOMAIN, transcript)
}

pub fn verify_client_proof(token: &[u8; 32], transcript: &[u8], proof: &[u8; 32]) -> bool {
    verify(token, CLIENT_DOMAIN, transcript, proof)
}

pub fn verify_server_proof(token: &[u8; 32], transcript: &[u8], proof: &[u8; 32]) -> bool {
    verify(token, SERVER_DOMAIN, transcript, proof)
}

pub fn reject_equal_nonces(ns: &[u8; 32], nc: &[u8; 32]) -> Result<(), HostError> {
    if constant_eq(ns, nc) {
        Err(HostError::Auth)
    } else {
        Ok(())
    }
}

fn mac(key: &[u8; 32], domain: &[u8], transcript: &[u8]) -> [u8; 32] {
    let hmac_key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key);
    let mut ctx = ring::hmac::Context::with_key(&hmac_key);
    ctx.update(domain);
    ctx.update(transcript);
    let tag = ctx.sign();
    let mut out = [0u8; 32];
    out.copy_from_slice(tag.as_ref());
    out
}

fn verify(key: &[u8; 32], domain: &[u8], transcript: &[u8], proof: &[u8]) -> bool {
    let hmac_key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key);
    let mut message = Vec::with_capacity(domain.len() + transcript.len());
    message.extend_from_slice(domain);
    message.extend_from_slice(transcript);
    ring::hmac::verify(&hmac_key, &message, proof).is_ok()
}

fn constant_eq(left: &[u8], right: &[u8]) -> bool {
    crate::types::constant_time_eq(left, right)
}

pub fn client_hello_frame(nc: &[u8; 32], major: u16, minor: u16, proof: &[u8; 32]) -> Frame {
    let mut payload = Vec::with_capacity(68);
    payload.extend_from_slice(nc);
    payload.extend_from_slice(&major.to_be_bytes());
    payload.extend_from_slice(&minor.to_be_bytes());
    payload.extend_from_slice(proof);
    Frame {
        kind: FrameKind::Hello,
        payload,
    }
}

pub fn decode_challenge(frame: &Frame) -> Result<[u8; 32], HostError> {
    if frame.kind != FrameKind::Challenge || frame.payload.len() != 32 {
        return Err(HostError::Protocol("expected a challenge".into()));
    }
    let mut ns = [0u8; 32];
    ns.copy_from_slice(&frame.payload);
    Ok(ns)
}

fn decode_hello(frame: &Frame) -> Result<([u8; 32], u16, u16, [u8; 32]), HostError> {
    if frame.kind != FrameKind::Hello || frame.payload.len() != 68 {
        return Err(HostError::Protocol("expected a hello".into()));
    }
    let mut nc = [0u8; 32];
    nc.copy_from_slice(&frame.payload[..32]);
    let major = u16::from_be_bytes(frame.payload[32..34].try_into().unwrap());
    let minor = u16::from_be_bytes(frame.payload[34..36].try_into().unwrap());
    let mut proof = [0u8; 32];
    proof.copy_from_slice(&frame.payload[36..]);
    Ok((nc, major, minor, proof))
}

fn decode_proof(frame: &Frame) -> Result<[u8; 32], HostError> {
    if frame.kind != FrameKind::Proof || frame.payload.len() != 32 {
        return Err(HostError::Protocol("expected a proof".into()));
    }
    let mut proof = [0u8; 32];
    proof.copy_from_slice(&frame.payload);
    Ok(proof)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Welcome {
    pub ok: bool,
    pub major: u16,
    pub minor: u16,
    pub version: String,
}

pub fn encode_welcome(ok: bool, major: u16, minor: u16, version: &str) -> Result<Frame, HostError> {
    let bytes = version.as_bytes();
    let version_len =
        u16::try_from(bytes.len()).map_err(|_| HostError::Protocol("version".into()))?;
    let mut payload = Vec::with_capacity(1 + 2 + 2 + 2 + bytes.len());
    payload.push(u8::from(!ok));
    payload.extend_from_slice(&major.to_be_bytes());
    payload.extend_from_slice(&minor.to_be_bytes());
    payload.extend_from_slice(&version_len.to_be_bytes());
    payload.extend_from_slice(bytes);
    Ok(Frame {
        kind: FrameKind::Welcome,
        payload,
    })
}

pub fn decode_welcome(frame: &Frame) -> Result<Welcome, HostError> {
    if frame.kind != FrameKind::Welcome || frame.payload.len() < 7 {
        return Err(HostError::Protocol("expected a welcome".into()));
    }
    let status = frame.payload[0];
    let major = u16::from_be_bytes(frame.payload[1..3].try_into().unwrap());
    let minor = u16::from_be_bytes(frame.payload[3..5].try_into().unwrap());
    let version_len = usize::from(u16::from_be_bytes(frame.payload[5..7].try_into().unwrap()));
    if frame.payload.len() != 7 + version_len {
        return Err(HostError::Protocol("welcome length".into()));
    }
    let version = std::str::from_utf8(&frame.payload[7..])
        .map_err(|_| HostError::Protocol("welcome version".into()))?
        .to_owned();
    Ok(Welcome {
        ok: status == 0,
        major,
        minor,
        version,
    })
}

pub async fn write_garbage<W>(writer: &mut W, deadline: Instant) -> Result<(), HostError>
where
    W: AsyncWrite + Unpin,
{
    let mut bytes = Vec::with_capacity(5);
    bytes.extend_from_slice(&1u32.to_be_bytes());
    bytes.push(0xff);
    write_all_deadline(writer, &bytes, deadline).await
}

pub async fn read_frame_async<R>(
    reader: &mut R,
    deadline: Instant,
    unauth: bool,
) -> Result<Frame, HostError>
where
    R: AsyncRead + Unpin,
{
    let mut len_buf = [0u8; 4];
    read_exact_deadline(reader, &mut len_buf, deadline, true).await?;
    let len = u32::from_be_bytes(len_buf);
    let body_len = if unauth {
        crate::checked_unauth_len(len)?
    } else {
        crate::checked_frame_len(len)?
    };
    let mut body = vec![0u8; body_len];
    read_exact_deadline(reader, &mut body, deadline, true).await?;
    crate::frame::decode_body(body)
}

pub async fn write_frame_async<W>(
    writer: &mut W,
    frame: &Frame,
    deadline: Instant,
) -> Result<(), HostError>
where
    W: AsyncWrite + Unpin,
{
    write_frame_captured(writer, frame, deadline, None).await
}

pub async fn read_frame_idle<R>(reader: &mut R, frame_timeout: Duration) -> Result<Frame, HostError>
where
    R: AsyncRead + Unpin,
{
    let mut first = [0u8; 1];
    reader.read_exact(&mut first).await.map_err(io_closed)?;
    let deadline = Instant::now() + frame_timeout;
    let mut rest = [0u8; 3];
    read_exact_deadline(reader, &mut rest, deadline, false).await?;
    let mut len_buf = [0u8; 4];
    len_buf[0] = first[0];
    len_buf[1..].copy_from_slice(&rest);
    let len = u32::from_be_bytes(len_buf);
    let body_len = crate::checked_frame_len(len)?;
    let mut body = vec![0u8; body_len];
    read_exact_deadline(reader, &mut body, deadline, false).await?;
    crate::frame::decode_body(body)
}

async fn write_frame_captured<W>(
    writer: &mut W,
    frame: &Frame,
    deadline: Instant,
    capture: Option<&Arc<Mutex<Vec<u8>>>>,
) -> Result<(), HostError>
where
    W: AsyncWrite + Unpin,
{
    // A slice writer prevents hidden amortized capacity growth. This is the
    // second response allocation retained alongside the JSON frame while parked.
    let wire_len = usize::try_from(crate::frame_total_len(frame.payload.len() as u64)?)
        .map_err(|_| HostError::FrameLength)?;
    let mut bytes = vec![0u8; wire_len];
    write_frame(&mut bytes.as_mut_slice(), frame)?;
    write_all_deadline(writer, &bytes, deadline).await?;
    if let Some(capture) = capture {
        capture
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .extend_from_slice(&bytes);
    }
    Ok(())
}

async fn write_all_deadline<W>(
    writer: &mut W,
    bytes: &[u8],
    deadline: Instant,
) -> Result<(), HostError>
where
    W: AsyncWrite + Unpin,
{
    let left = remaining(deadline)?;
    match tokio::time::timeout(left, writer.write_all(bytes)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(err)) => Err(io_closed(err)),
        Err(_) => Err(HostError::FrameTimeout),
    }
}

async fn read_exact_deadline<R>(
    reader: &mut R,
    buf: &mut [u8],
    deadline: Instant,
    eof_is_auth: bool,
) -> Result<(), HostError>
where
    R: AsyncRead + Unpin,
{
    let left = remaining(deadline)?;
    match tokio::time::timeout(left, reader.read_exact(buf)).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(err)) if eof_is_auth && is_eof(&err) => Err(HostError::Auth),
        Ok(Err(err)) => Err(io_closed(err)),
        Err(_) => Err(HostError::FrameTimeout),
    }
}

fn remaining(deadline: Instant) -> Result<Duration, HostError> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|left| !left.is_zero())
        .ok_or(HostError::FrameTimeout)
}

fn is_eof(err: &std::io::Error) -> bool {
    err.kind() == std::io::ErrorKind::UnexpectedEof
        || err.kind() == std::io::ErrorKind::ConnectionReset
}

fn io_closed(err: std::io::Error) -> HostError {
    HostError::Io(err.to_string())
}

fn fresh_nonce() -> Result<[u8; 32], HostError> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|err| HostError::Io(err.to_string()))?;
    Ok(bytes)
}

/// Daemon half of the handshake. The caller supplies the token observed after Hello arrives.
pub async fn server_handshake<S>(stream: S, params: HandshakeParams<'_>) -> Result<(), HostError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    finish_server(stream, params).await.map(|_| ())
}

pub async fn finish_server<S>(stream: S, params: HandshakeParams<'_>) -> Result<(S, u16), HostError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let token = *params.token;
    server_with_token(
        stream,
        params.identity,
        params.deadlines,
        params.daemon_version,
        params.major,
        params.minor,
        params.capture_writes,
        move || token,
    )
    .await
}

/// `load_token` runs only after Hello has been read, so a rotation that landed
/// between the client's token snapshot and its proof is visible here.
#[allow(clippy::too_many_arguments)]
pub async fn server_with_token<S, F>(
    mut stream: S,
    identity: &[u8],
    deadlines: Deadlines,
    daemon_version: &str,
    daemon_major: u16,
    daemon_minor: u16,
    capture: Option<Arc<Mutex<Vec<u8>>>>,
    load_token: F,
) -> Result<(S, u16), HostError>
where
    S: AsyncRead + AsyncWrite + Unpin,
    F: FnOnce() -> AuthToken,
{
    let deadline = Instant::now() + deadlines.handshake;
    let ns = fresh_nonce()?;
    let challenge = Frame {
        kind: FrameKind::Challenge,
        payload: ns.to_vec(),
    };
    write_frame_captured(&mut stream, &challenge, deadline, capture.as_ref()).await?;
    let hello = read_frame_async(&mut stream, deadline, true).await?;
    let (nc, major, minor, proof) = decode_hello(&hello)?;
    reject_equal_nonces(&ns, &nc)?;
    let token = load_token();
    let transcript = auth_transcript(identity, major, minor, &ns, &nc);
    if !verify_client_proof(token.as_bytes(), &transcript, &proof) {
        return Err(HostError::Auth);
    }
    let server = server_proof(token.as_bytes(), &transcript);
    write_frame_captured(
        &mut stream,
        &Frame {
            kind: FrameKind::Proof,
            payload: server.to_vec(),
        },
        deadline,
        capture.as_ref(),
    )
    .await?;
    if major != daemon_major {
        let welcome = encode_welcome(false, daemon_major, daemon_minor, daemon_version)?;
        write_frame_captured(&mut stream, &welcome, deadline, capture.as_ref()).await?;
        return Err(HostError::IncompatibleProtocol {
            daemon_version: daemon_version.to_owned(),
        });
    }
    let agreed = daemon_minor.min(minor);
    let welcome = encode_welcome(true, daemon_major, agreed, daemon_version)?;
    write_frame_captured(&mut stream, &welcome, deadline, capture.as_ref()).await?;
    Ok((stream, agreed))
}

pub async fn client_handshake<S>(
    stream: S,
    identity: &[u8],
    token: &AuthToken,
    major: u16,
    minor: u16,
    deadlines: Deadlines,
    capture: Option<Arc<Mutex<Vec<u8>>>>,
) -> Result<(), HostError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    drive_client(
        stream, identity, token, major, minor, deadlines, capture, None,
    )
    .await
    .map(|_| ())
}

#[allow(clippy::too_many_arguments)]
pub async fn drive_client<S>(
    mut stream: S,
    identity: &[u8],
    token: &AuthToken,
    major: u16,
    minor: u16,
    deadlines: Deadlines,
    capture: Option<Arc<Mutex<Vec<u8>>>>,
    pre_proof: Option<&(dyn Fn() + Send + Sync)>,
) -> Result<(S, u16, u16), HostError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let deadline = Instant::now() + deadlines.handshake;
    let challenge = read_frame_async(&mut stream, deadline, true).await?;
    let ns = decode_challenge(&challenge)?;
    if let Some(hook) = pre_proof {
        hook();
    }
    let mut nc = fresh_nonce()?;
    if constant_eq(&ns, &nc) {
        nc = fresh_nonce()?;
        reject_equal_nonces(&ns, &nc)?;
    }
    let transcript = auth_transcript(identity, major, minor, &ns, &nc);
    let proof = client_proof(token.as_bytes(), &transcript);
    let hello = client_hello_frame(&nc, major, minor, &proof);
    write_frame_captured(&mut stream, &hello, deadline, capture.as_ref()).await?;
    let proof_frame = read_frame_async(&mut stream, deadline, true).await?;
    let server = decode_proof(&proof_frame)?;
    if !verify_server_proof(token.as_bytes(), &transcript, &server) {
        return Err(HostError::Auth);
    }
    let welcome_frame = read_frame_async(&mut stream, deadline, true).await?;
    let welcome = decode_welcome(&welcome_frame)?;
    if !welcome.ok {
        return Err(HostError::IncompatibleProtocol {
            daemon_version: welcome.version,
        });
    }
    if welcome.major != major {
        return Err(HostError::Protocol("welcome major".into()));
    }
    Ok((stream, welcome.major, welcome.minor))
}
