//! Multiplexed `/ws/v1` (and `/api/v1/stream`) RFC 6455 WebSocket event bus,
//! `HubServer` TCP listener, and client helper (spec §4.1, §4.2).

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream as StdTcpStream, ToSocketAddrs};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use fl_wire::ws::{WsError, WsFrame, WsOpcode};
use ring::rand::{SecureRandom, SystemRandom};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, watch};

use crate::api::{
    HttpRequest, HttpResponse, drain_ready_session_queues, handle_http_request, parse_http_request,
};
use crate::auth::HubAuthority;
use crate::terminal::TerminalManager;
use crate::{HubServerConfig, HubState, HubWsFrame};

const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
const MAX_HTTP_REQUEST_BYTES: usize = 24 * 1024 * 1024;
const MAX_WS_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;
const HTTP_READ_TIMEOUT: Duration = Duration::from_secs(15);

/// Bound FerroLoop Hub HTTP/1.1 + `/ws/v1` WebSocket server (spec §2, §3, §4).
pub struct HubServer {
    state: HubState,
    listener: TcpListener,
    local_addr: SocketAddr,
    shutdown_tx: watch::Sender<bool>,
    shutdown_rx: watch::Receiver<bool>,
}

impl HubServer {
    /// Bind a [`HubServer`] on `config.listen` (e.g. `"127.0.0.1:8420"` or `"127.0.0.1:0"`).
    pub async fn bind(config: HubServerConfig) -> io::Result<Self> {
        let listener = TcpListener::bind(&config.listen).await?;
        let local_addr = listener.local_addr()?;
        let state = HubState::new(config);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        Ok(Self {
            state,
            listener,
            local_addr,
            shutdown_tx,
            shutdown_rx,
        })
    }

    /// Construct a [`HubServer`] from an already-bound `std::net::TcpListener`.
    pub fn from_std(
        config: HubServerConfig,
        std_listener: std::net::TcpListener,
    ) -> io::Result<Self> {
        std_listener.set_nonblocking(true)?;
        let local_addr = std_listener.local_addr()?;
        let listener = TcpListener::from_std(std_listener)?;
        let state = HubState::new(config);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        Ok(Self {
            state,
            listener,
            local_addr,
            shutdown_tx,
            shutdown_rx,
        })
    }

    /// Bound local socket address (useful when `config.listen` uses port `0`).
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Shared [`HubState`] backing this server.
    pub fn state(&self) -> &HubState {
        &self.state
    }

    /// Return a handle that can signal [`HubServer::serve`] to stop accepting connections.
    pub fn shutdown_handle(&self) -> watch::Sender<bool> {
        self.shutdown_tx.clone()
    }

    /// Run the accept loop until shutdown is signaled or an unrecoverable accept error occurs.
    pub async fn serve(mut self) -> io::Result<()> {
        let mut drain_tick = tokio::time::interval(Duration::from_secs(1));
        drain_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                accept_res = self.listener.accept() => {
                    match accept_res {
                        Ok((stream, _peer_addr)) => {
                            let state = self.state.clone();
                            tokio::spawn(async move {
                                let _ = handle_connection(stream, state).await;
                            });
                        }
                        Err(e) => return Err(e),
                    }
                }
                _ = drain_tick.tick() => {
                    if !self.state.queued_session_keys().is_empty() {
                        let state = self.state.clone();
                        let _ = tokio::task::spawn_blocking(move || {
                            if let Ok(store) = state.open_store() {
                                drain_ready_session_queues(&state, &store);
                            }
                        }).await;
                    }
                }
                changed = self.shutdown_rx.changed() => {
                    if changed.is_err() || *self.shutdown_rx.borrow() {
                        return Ok(());
                    }
                }
            }
        }
    }
}

fn is_ws_path(path: &str) -> bool {
    path == "/ws/v1" || path == "/api/v1/stream"
}

fn has_websocket_upgrade_header(req: &HttpRequest) -> bool {
    req.headers
        .get("upgrade")
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("websocket"))
}

fn authenticate_ws_request(state: &HubState, req: &HttpRequest) -> Option<HubAuthority> {
    let store = state.open_store().ok();
    state
        .auth
        .authenticate_request(store.as_deref(), &req.headers, &req.query)
}

async fn handle_connection(mut stream: TcpStream, state: HubState) -> io::Result<()> {
    let _ = stream.set_nodelay(true);
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 4096];

    let (req, consumed) = loop {
        match parse_http_request(&buf) {
            Ok(Some(parsed)) => break parsed,
            Ok(None) => {
                if buf.len() >= MAX_HTTP_REQUEST_BYTES {
                    let resp =
                        HttpResponse::json(400, json!({ "error": "HTTP request too large" }));
                    let _ = stream.write_all(&resp.to_http_bytes(false)).await;
                    let _ = stream.flush().await;
                    return Ok(());
                }
                let read_res =
                    tokio::time::timeout(HTTP_READ_TIMEOUT, stream.read(&mut chunk)).await;
                match read_res {
                    Ok(Ok(0)) => return Ok(()),
                    Ok(Ok(n)) => buf.extend_from_slice(&chunk[..n]),
                    Ok(Err(e)) => return Err(e),
                    Err(_) => return Ok(()),
                }
            }
            Err(e) => {
                let resp = HttpResponse::json(400, json!({ "error": e }));
                let _ = stream.write_all(&resp.to_http_bytes(false)).await;
                let _ = stream.flush().await;
                return Ok(());
            }
        }
    };

    if is_ws_path(&req.path) {
        let Some(authority) = authenticate_ws_request(&state, &req) else {
            let resp = HttpResponse::json(401, json!({ "error": "unauthorized" }));
            stream.write_all(&resp.to_http_bytes(false)).await?;
            stream.flush().await?;
            return Ok(());
        };

        if req.method != "GET" || !has_websocket_upgrade_header(&req) {
            let resp = HttpResponse::json(
                400,
                json!({ "error": "expected GET request with Upgrade: websocket header" }),
            );
            stream.write_all(&resp.to_http_bytes(false)).await?;
            stream.flush().await?;
            return Ok(());
        }

        let Some(sec_key) = req
            .headers
            .get("sec-websocket-key")
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        else {
            let resp = HttpResponse::json(400, json!({ "error": "missing Sec-WebSocket-Key" }));
            stream.write_all(&resp.to_http_bytes(false)).await?;
            stream.flush().await?;
            return Ok(());
        };

        let accept = compute_accept_key(sec_key);
        let leftover = buf[consumed..].to_vec();
        return handle_websocket_session(stream, state, authority, &accept, leftover).await;
    }

    let is_head = req.method == "HEAD";
    let state_for_http = state.clone();
    let resp = match tokio::task::spawn_blocking(move || handle_http_request(&state_for_http, &req))
        .await
    {
        Ok(r) => r,
        Err(e) => HttpResponse::json(500, json!({ "error": format!("internal task error: {e}") })),
    };

    stream.write_all(&resp.to_http_bytes(is_head)).await?;
    stream.flush().await?;
    Ok(())
}

async fn handle_websocket_session(
    mut stream: TcpStream,
    state: HubState,
    authority: HubAuthority,
    accept_key: &str,
    mut read_buf: Vec<u8>,
) -> io::Result<()> {
    let mut bcast_rx = state.subscribe();

    let handshake_resp = format!(
        "HTTP/1.1 101 Switching Protocols\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Accept: {accept_key}\r\n\r\n"
    );
    stream.write_all(handshake_resp.as_bytes()).await?;
    stream.flush().await?;

    let (mut reader, mut writer) = stream.into_split();

    let connected = HubWsFrame {
        channel: "system".to_string(),
        event: "connected".to_string(),
        data: json!({
            "authority": authority,
        }),
    };
    write_ws_json(&mut writer, &connected).await?;

    let (term_tx, mut term_rx) = mpsc::unbounded_channel::<HubWsFrame>();
    let mut terminal_mgr = TerminalManager::new(term_tx);
    let mut tmp = [0u8; 4096];

    let loop_res: io::Result<()> = async {
        loop {
            while let Some(frame) = try_parse_ws_frame(&mut read_buf)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?
            {
                let keep_open = handle_incoming_ws_frame(
                    frame,
                    &state,
                    &authority,
                    &mut terminal_mgr,
                    &mut writer,
                )
                .await?;
                if !keep_open {
                    return Ok(());
                }
            }

            tokio::select! {
                read_res = reader.read(&mut tmp) => {
                    match read_res {
                        Ok(0) => return Ok(()),
                        Ok(n) => {
                            read_buf.extend_from_slice(&tmp[..n]);
                        }
                        Err(e) => return Err(e),
                    }
                }
                bcast_res = bcast_rx.recv() => {
                    match bcast_res {
                        Ok(frame) => {
                            write_ws_json(&mut writer, &frame).await?;
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => return Ok(()),
                    }
                }
                Some(term_frame) = term_rx.recv() => {
                    write_ws_json(&mut writer, &term_frame).await?;
                }
            }
        }
    }
    .await;

    terminal_mgr.shutdown_all().await;
    loop_res
}

async fn handle_incoming_ws_frame(
    frame: WsFrame,
    state: &HubState,
    authority: &HubAuthority,
    terminal_mgr: &mut TerminalManager,
    writer: &mut OwnedWriteHalf,
) -> io::Result<bool> {
    match frame.opcode {
        WsOpcode::Ping => {
            write_ws_raw_frame(writer, WsOpcode::Pong, &frame.payload).await?;
            Ok(true)
        }
        WsOpcode::Pong => Ok(true),
        WsOpcode::Close => {
            let _ = write_ws_raw_frame(writer, WsOpcode::Close, &[]).await;
            Ok(false)
        }
        WsOpcode::Text | WsOpcode::Binary => {
            match serde_json::from_slice::<HubWsFrame>(&frame.payload) {
                Ok(ws_frame) => match ws_frame.channel.as_str() {
                    "system" => {
                        if ws_frame.event == "ping" {
                            let pong = HubWsFrame {
                                channel: "system".to_string(),
                                event: "pong".to_string(),
                                data: ws_frame.data,
                            };
                            write_ws_json(writer, &pong).await?;
                        }
                    }
                    "terminal" => {
                        terminal_mgr
                            .handle_frame(
                                authority,
                                state.config.enable_terminal,
                                &state.config.repo_root,
                                &ws_frame.event,
                                &ws_frame.data,
                            )
                            .await;
                    }
                    other => {
                        let err_frame = HubWsFrame {
                            channel: "system".to_string(),
                            event: "error".to_string(),
                            data: json!({
                                "status": 400,
                                "message": format!("unsupported channel `{other}`"),
                            }),
                        };
                        write_ws_json(writer, &err_frame).await?;
                    }
                },
                Err(e) => {
                    let err_frame = HubWsFrame {
                        channel: "system".to_string(),
                        event: "error".to_string(),
                        data: json!({
                            "status": 400,
                            "message": format!("invalid JSON WebSocket frame: {e}"),
                        }),
                    };
                    write_ws_json(writer, &err_frame).await?;
                }
            }
            Ok(true)
        }
        WsOpcode::Continuation => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unexpected continuation frame",
        )),
    }
}

async fn write_ws_json(writer: &mut OwnedWriteHalf, frame: &HubWsFrame) -> io::Result<()> {
    let bytes = serde_json::to_vec(frame)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    write_ws_raw_frame(writer, WsOpcode::Text, &bytes).await
}

async fn write_ws_raw_frame(
    writer: &mut OwnedWriteHalf,
    opcode: WsOpcode,
    payload: &[u8],
) -> io::Result<()> {
    let mut buf = Vec::with_capacity(10 + payload.len());
    buf.push(0x80 | (opcode as u8));
    if payload.len() < 126 {
        buf.push(payload.len() as u8);
    } else if payload.len() <= u16::MAX as usize {
        buf.push(126);
        buf.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    } else {
        buf.push(127);
        buf.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    }
    buf.extend_from_slice(payload);
    writer.write_all(&buf).await?;
    writer.flush().await
}

fn opcode_from_u8(b: u8) -> Result<WsOpcode, WsError> {
    match b {
        0x0 => Ok(WsOpcode::Continuation),
        0x1 => Ok(WsOpcode::Text),
        0x2 => Ok(WsOpcode::Binary),
        0x8 => Ok(WsOpcode::Close),
        0x9 => Ok(WsOpcode::Ping),
        0xA => Ok(WsOpcode::Pong),
        other => Err(WsError::Protocol(format!("unknown opcode 0x{other:x}"))),
    }
}

/// Non-destructively check `buf` for a complete RFC 6455 frame; if complete, drain and return it.
pub fn try_parse_ws_frame(buf: &mut Vec<u8>) -> Result<Option<WsFrame>, WsError> {
    if buf.len() < 2 {
        return Ok(None);
    }
    let opcode = opcode_from_u8(buf[0] & 0x0F)?;
    let masked = (buf[1] & 0x80) != 0;
    let len_byte = (buf[1] & 0x7F) as usize;

    let (payload_len, mut offset) = if len_byte < 126 {
        (len_byte, 2usize)
    } else if len_byte == 126 {
        if buf.len() < 4 {
            return Ok(None);
        }
        let ext = u16::from_be_bytes([buf[2], buf[3]]) as usize;
        (ext, 4usize)
    } else {
        if buf.len() < 10 {
            return Ok(None);
        }
        let mut arr = [0u8; 8];
        arr.copy_from_slice(&buf[2..10]);
        let ext = usize::try_from(u64::from_be_bytes(arr))
            .map_err(|_| WsError::Protocol("frame payload too large".into()))?;
        (ext, 10usize)
    };

    if payload_len > MAX_WS_PAYLOAD_BYTES {
        return Err(WsError::Protocol(format!(
            "frame payload {payload_len} exceeds maximum ({MAX_WS_PAYLOAD_BYTES})"
        )));
    }

    let mask_len = if masked { 4 } else { 0 };
    let total_frame_len = offset + mask_len + payload_len;
    if buf.len() < total_frame_len {
        return Ok(None);
    }

    let mut mask = [0u8; 4];
    if masked {
        mask.copy_from_slice(&buf[offset..offset + 4]);
        offset += 4;
    }

    let mut payload = buf[offset..offset + payload_len].to_vec();
    if masked {
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= mask[i & 3];
        }
    }

    buf.drain(..total_frame_len);
    Ok(Some(WsFrame { opcode, payload }))
}

/// Compute RFC 6455 `Sec-WebSocket-Accept` header value for `sec_key`.
pub fn compute_accept_key(sec_key: &str) -> String {
    let mut input = Vec::with_capacity(sec_key.len() + WS_GUID.len());
    input.extend_from_slice(sec_key.trim().as_bytes());
    input.extend_from_slice(WS_GUID.as_bytes());
    let digest = sha1_digest(&input);
    BASE64.encode(digest)
}

fn sha1_digest(data: &[u8]) -> [u8; 20] {
    let mut h0: u32 = 0x67452301;
    let mut h1: u32 = 0xEFCDAB89;
    let mut h2: u32 = 0x98BADCFE;
    let mut h3: u32 = 0x10325476;
    let mut h4: u32 = 0xC3D2E1F0;

    let bit_len = (data.len() as u64) * 8;
    let mut msg = data.to_vec();
    msg.push(0x80);
    while (msg.len() % 64) != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.as_chunks::<64>().0 {
        let mut w = [0u32; 80];
        for (i, word) in chunk.as_chunks::<4>().0.iter().enumerate() {
            w[i] = u32::from_be_bytes(*word);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }

        let mut a = h0;
        let mut b = h1;
        let mut c = h2;
        let mut d = h3;
        let mut e = h4;

        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A827999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1u32),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDCu32),
                _ => (b ^ c ^ d, 0xCA62C1D6u32),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }

        h0 = h0.wrapping_add(a);
        h1 = h1.wrapping_add(b);
        h2 = h2.wrapping_add(c);
        h3 = h3.wrapping_add(d);
        h4 = h4.wrapping_add(e);
    }

    let mut out = [0u8; 20];
    out[0..4].copy_from_slice(&h0.to_be_bytes());
    out[4..8].copy_from_slice(&h1.to_be_bytes());
    out[8..12].copy_from_slice(&h2.to_be_bytes());
    out[12..16].copy_from_slice(&h3.to_be_bytes());
    out[16..20].copy_from_slice(&h4.to_be_bytes());
    out
}

/// Synchronous RFC 6455 WebSocket client supporting custom HTTP headers (`Cookie`,
/// `Authorization`, `Tailscale-User-Login`, `X-Fl-Agent`) for `fl-hub` integration testing.
pub struct HubWsClient {
    tcp: StdTcpStream,
    rng: SystemRandom,
}

impl HubWsClient {
    pub fn connect(
        url: &str,
        extra_headers: &[(&str, &str)],
        timeout: Duration,
    ) -> Result<Self, WsError> {
        let rest = url
            .strip_prefix("ws://")
            .ok_or_else(|| WsError::InvalidUrl(url.to_string()))?;
        let (host_port, path) = match rest.find('/') {
            Some(idx) => (&rest[..idx], &rest[idx..]),
            None => (rest, "/"),
        };
        let addr = host_port
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::AddrNotAvailable, "no socket addr"))?;
        let mut tcp = StdTcpStream::connect_timeout(&addr, timeout)?;
        tcp.set_nodelay(true)?;
        tcp.set_read_timeout(Some(timeout))?;
        tcp.set_write_timeout(Some(timeout))?;

        let rng = SystemRandom::new();
        let mut key_bytes = [0u8; 16];
        rng.fill(&mut key_bytes)
            .map_err(|_| WsError::Handshake("RNG failure".into()))?;
        let sec_key = BASE64.encode(key_bytes);

        let mut req = format!(
            "GET {path} HTTP/1.1\r\n\
             Host: {host_port}\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: {sec_key}\r\n\
             Sec-WebSocket-Version: 13\r\n"
        );
        for (k, v) in extra_headers {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        req.push_str("\r\n");

        tcp.write_all(req.as_bytes())?;
        tcp.flush()?;

        let mut hdr_buf = Vec::with_capacity(512);
        let mut byte = [0u8; 1];
        while hdr_buf.len() < 16_384 {
            let n = tcp.read(&mut byte)?;
            if n == 0 {
                return Err(WsError::Handshake("unexpected EOF reading headers".into()));
            }
            hdr_buf.push(byte[0]);
            if hdr_buf.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        let hdr_str = String::from_utf8_lossy(&hdr_buf);
        let mut lines = hdr_str.lines();
        let status_line = lines.next().unwrap_or("");
        if !status_line.starts_with("HTTP/1.1 101") {
            return Err(WsError::Handshake(format!(
                "expected 101 Switching Protocols, got `{status_line}`"
            )));
        }
        let expected_accept = compute_accept_key(&sec_key);
        let mut accept_ok = false;
        for line in lines {
            if let Some((k, v)) = line.split_once(':')
                && k.trim().eq_ignore_ascii_case("sec-websocket-accept")
                && v.trim() == expected_accept
            {
                accept_ok = true;
            }
        }
        if !accept_ok {
            return Err(WsError::Handshake(
                "missing or mismatched Sec-WebSocket-Accept header".into(),
            ));
        }

        Ok(Self { tcp, rng })
    }

    pub fn set_read_timeout(&self, dur: Option<Duration>) -> io::Result<()> {
        self.tcp.set_read_timeout(dur)
    }

    pub fn send_frame(&mut self, frame: &HubWsFrame) -> Result<(), WsError> {
        let payload = serde_json::to_vec(frame)?;
        self.write_raw_frame(WsOpcode::Text, &payload)
    }

    pub fn recv_frame(&mut self) -> Result<HubWsFrame, WsError> {
        loop {
            let raw = self.read_raw_frame()?;
            match raw.opcode {
                WsOpcode::Text | WsOpcode::Binary => {
                    return Ok(serde_json::from_slice(&raw.payload)?);
                }
                WsOpcode::Ping => {
                    self.write_raw_frame(WsOpcode::Pong, &raw.payload)?;
                }
                WsOpcode::Pong => continue,
                WsOpcode::Close => {
                    let _ = self.write_raw_frame(WsOpcode::Close, &[]);
                    return Err(WsError::Closed);
                }
                WsOpcode::Continuation => {
                    return Err(WsError::Protocol("unexpected continuation frame".into()));
                }
            }
        }
    }

    pub fn close(&mut self) -> Result<(), WsError> {
        self.write_raw_frame(WsOpcode::Close, &[])
    }

    fn write_raw_frame(&mut self, opcode: WsOpcode, payload: &[u8]) -> Result<(), WsError> {
        let mut buf = Vec::with_capacity(14 + payload.len());
        buf.push(0x80 | (opcode as u8));
        if payload.len() < 126 {
            buf.push(0x80 | (payload.len() as u8));
        } else if payload.len() <= u16::MAX as usize {
            buf.push(0x80 | 126);
            buf.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        } else {
            buf.push(0x80 | 127);
            buf.extend_from_slice(&(payload.len() as u64).to_be_bytes());
        }
        let mut mask = [0u8; 4];
        self.rng
            .fill(&mut mask)
            .map_err(|_| WsError::Protocol("RNG failure".into()))?;
        buf.extend_from_slice(&mask);
        for (i, &b) in payload.iter().enumerate() {
            buf.push(b ^ mask[i & 3]);
        }
        self.tcp.write_all(&buf)?;
        self.tcp.flush()?;
        Ok(())
    }

    fn read_raw_frame(&mut self) -> Result<WsFrame, WsError> {
        let mut hdr = [0u8; 2];
        if let Err(e) = self.tcp.read_exact(&mut hdr) {
            if e.kind() == io::ErrorKind::UnexpectedEof {
                return Err(WsError::Closed);
            }
            return Err(WsError::Io(e));
        }
        let opcode = opcode_from_u8(hdr[0] & 0x0F)?;
        let masked = (hdr[1] & 0x80) != 0;
        let mut len = (hdr[1] & 0x7F) as usize;
        if len == 126 {
            let mut ext = [0u8; 2];
            self.tcp.read_exact(&mut ext)?;
            len = u16::from_be_bytes(ext) as usize;
        } else if len == 127 {
            let mut ext = [0u8; 8];
            self.tcp.read_exact(&mut ext)?;
            len = usize::try_from(u64::from_be_bytes(ext))
                .map_err(|_| WsError::Protocol("frame payload too large".into()))?;
        }
        let mut mask = [0u8; 4];
        if masked {
            self.tcp.read_exact(&mut mask)?;
        }
        let mut payload = vec![0u8; len];
        if len > 0 {
            self.tcp.read_exact(&mut payload)?;
        }
        if masked {
            for (i, b) in payload.iter_mut().enumerate() {
                *b ^= mask[i & 3];
            }
        }
        Ok(WsFrame { opcode, payload })
    }
}
