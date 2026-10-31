//! Minimal RFC 6455 WebSocket framing and client/server upgrade over TCP (`ws://`) and TLS (`wss://`).

use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use ring::rand::{SecureRandom, SystemRandom};
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};
use rustls_pki_types::ServerName;
use serde::Serialize;
use serde::de::DeserializeOwned;

const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
const MAX_FRAME_PAYLOAD: usize = 16 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum WsError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("TLS error: {0}")]
    Tls(#[from] rustls::Error),
    #[error("invalid WebSocket URL `{0}`")]
    InvalidUrl(String),
    #[error("WebSocket handshake failed: {0}")]
    Handshake(String),
    #[error("WebSocket protocol error: {0}")]
    Protocol(String),
    #[error("WebSocket closed")]
    Closed,
    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WsOpcode {
    Continuation = 0x0,
    Text = 0x1,
    Binary = 0x2,
    Close = 0x8,
    Ping = 0x9,
    Pong = 0xA,
}

impl WsOpcode {
    fn from_u8(b: u8) -> Result<Self, WsError> {
        match b {
            0x0 => Ok(Self::Continuation),
            0x1 => Ok(Self::Text),
            0x2 => Ok(Self::Binary),
            0x8 => Ok(Self::Close),
            0x9 => Ok(Self::Ping),
            0xA => Ok(Self::Pong),
            other => Err(WsError::Protocol(format!("unknown opcode 0x{other:x}"))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WsFrame {
    pub opcode: WsOpcode,
    pub payload: Vec<u8>,
}

enum Transport {
    Plain(TcpStream),
    Tls(Box<StreamOwned<ClientConnection, TcpStream>>),
}

impl Read for Transport {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Plain(s) => s.read(buf),
            Self::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Transport {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Plain(s) => s.write(buf),
            Self::Tls(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Plain(s) => s.flush(),
            Self::Tls(s) => s.flush(),
        }
    }
}

pub struct WsStream {
    transport: Transport,
    is_client: bool,
    rng: SystemRandom,
    /// Bytes read from the transport that do not yet form a whole frame. A read that times out
    /// part-way through a frame leaves them here, so the next `read_frame` resumes the frame
    /// instead of treating its middle as a new header.
    rbuf: Vec<u8>,
    /// When the last whole frame (of any opcode, pongs included) arrived.
    last_rx: Instant,
}

impl WsStream {
    /// `true` when this end opened the connection (and so masks the frames it sends).
    pub fn is_client(&self) -> bool {
        self.is_client
    }

    pub fn set_read_timeout(&self, dur: Option<Duration>) -> io::Result<()> {
        match &self.transport {
            Transport::Plain(s) => s.set_read_timeout(dur),
            Transport::Tls(s) => s.get_ref().set_read_timeout(dur),
        }
    }

    pub fn set_write_timeout(&self, dur: Option<Duration>) -> io::Result<()> {
        match &self.transport {
            Transport::Plain(s) => s.set_write_timeout(dur),
            Transport::Tls(s) => s.get_ref().set_write_timeout(dur),
        }
    }

    /// Connect to a `ws://host:port[/path]` or `wss://host:port[/path]` endpoint.
    pub fn connect(url: &str, timeout: Duration) -> Result<Self, WsError> {
        // The URL is written into the request line and `Host` header: a CR, LF or space would
        // let it add headers of its own.
        if url.bytes().any(|b| b.is_ascii_control() || b == b' ') {
            return Err(WsError::InvalidUrl(url.escape_debug().to_string()));
        }
        let (tls, rest) = if let Some(r) = url.strip_prefix("wss://") {
            (true, r)
        } else if let Some(r) = url.strip_prefix("ws://") {
            (false, r)
        } else {
            return Err(WsError::InvalidUrl(url.to_string()));
        };

        let (host_port, path) = match rest.find('/') {
            Some(idx) => (&rest[..idx], &rest[idx..]),
            None => (rest, "/"),
        };
        if host_port.is_empty() {
            return Err(WsError::InvalidUrl(url.to_string()));
        }
        let default_port = if tls { 443 } else { 80 };
        let (host, port) = split_host_port(host_port, default_port)
            .ok_or_else(|| WsError::InvalidUrl(url.to_string()))?;

        let addr = format!("{host}:{port}")
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::AddrNotAvailable,
                    format!("could not resolve {host}:{port}"),
                )
            })?;
        let tcp = TcpStream::connect_timeout(&addr, timeout)?;
        tcp.set_nodelay(true)?;
        tcp.set_read_timeout(Some(timeout))?;
        tcp.set_write_timeout(Some(timeout))?;

        let transport = if tls {
            let mut root_store = RootCertStore::empty();
            root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let config = ClientConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()?
            .with_root_certificates(root_store)
            .with_no_client_auth();
            let server_name = ServerName::try_from(host.to_string())
                .map_err(|_| WsError::InvalidUrl(url.to_string()))?;
            let conn = ClientConnection::new(Arc::new(config), server_name)?;
            Transport::Tls(Box::new(StreamOwned::new(conn, tcp)))
        } else {
            Transport::Plain(tcp)
        };

        let rng = SystemRandom::new();
        let mut key_bytes = [0u8; 16];
        rng.fill(&mut key_bytes)
            .map_err(|_| WsError::Handshake("RNG failure".into()))?;
        let sec_key = BASE64.encode(key_bytes);

        let mut ws = Self {
            transport,
            is_client: true,
            rng,
            rbuf: Vec::new(),
            last_rx: Instant::now(),
        };

        let req = format!(
            "GET {path} HTTP/1.1\r\n\
             Host: {host_port}\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: {sec_key}\r\n\
             Sec-WebSocket-Version: 13\r\n\r\n"
        );
        ws.transport.write_all(req.as_bytes())?;
        ws.transport.flush()?;

        let headers = read_http_headers(&mut ws.transport)?;
        let mut lines = headers.lines();
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

        Ok(ws)
    }

    /// Accept an inbound TCP stream and complete the server-side RFC 6455 upgrade handshake.
    pub fn accept(tcp: TcpStream, timeout: Duration) -> Result<Self, WsError> {
        tcp.set_nodelay(true)?;
        tcp.set_read_timeout(Some(timeout))?;
        tcp.set_write_timeout(Some(timeout))?;
        let mut transport = Transport::Plain(tcp);
        let headers = read_http_headers(&mut transport)?;
        let mut sec_key: Option<String> = None;
        for line in headers.lines().skip(1) {
            if let Some((k, v)) = line.split_once(':')
                && k.trim().eq_ignore_ascii_case("sec-websocket-key")
            {
                sec_key = Some(v.trim().to_string());
            }
        }
        let Some(sec_key) = sec_key else {
            return Err(WsError::Handshake(
                "client upgrade missing Sec-WebSocket-Key".into(),
            ));
        };
        let accept = compute_accept_key(&sec_key);
        let resp = format!(
            "HTTP/1.1 101 Switching Protocols\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Accept: {accept}\r\n\r\n"
        );
        transport.write_all(resp.as_bytes())?;
        transport.flush()?;

        Ok(Self {
            transport,
            is_client: false,
            rng: SystemRandom::new(),
            rbuf: Vec::new(),
            last_rx: Instant::now(),
        })
    }

    /// How long ago the last whole frame (a pong included) arrived.
    pub fn idle_for(&self) -> Duration {
        self.last_rx.elapsed()
    }

    /// Read one frame. A read timeout or `WouldBlock` can surface part-way through a frame; the
    /// bytes already read are kept, and calling `read_frame` again resumes the same frame.
    pub fn read_frame(&mut self) -> Result<WsFrame, WsError> {
        loop {
            if let Some((frame, used)) = parse_frame(&self.rbuf, self.is_client)? {
                self.rbuf.drain(..used);
                self.last_rx = Instant::now();
                return Ok(frame);
            }
            let mut chunk = [0u8; 8192];
            let n = match self.transport.read(&mut chunk) {
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(WsError::Io(e)),
            };
            if n == 0 {
                return Err(if self.rbuf.is_empty() {
                    WsError::Closed
                } else {
                    WsError::Io(io::ErrorKind::UnexpectedEof.into())
                });
            }
            self.rbuf.extend_from_slice(&chunk[..n]);
        }
    }

    pub fn write_frame(&mut self, opcode: WsOpcode, payload: &[u8]) -> Result<(), WsError> {
        if payload.len() > MAX_FRAME_PAYLOAD {
            return Err(WsError::Protocol(format!(
                "frame payload {} exceeds maximum ({MAX_FRAME_PAYLOAD})",
                payload.len()
            )));
        }
        let mut buf = Vec::with_capacity(14 + payload.len());
        buf.push(0x80 | (opcode as u8));
        let mask_bit = if self.is_client { 0x80 } else { 0x00 };
        if payload.len() < 126 {
            buf.push(mask_bit | (payload.len() as u8));
        } else if payload.len() <= u16::MAX as usize {
            buf.push(mask_bit | 126);
            buf.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        } else {
            buf.push(mask_bit | 127);
            buf.extend_from_slice(&(payload.len() as u64).to_be_bytes());
        }
        if self.is_client {
            let mut mask = [0u8; 4];
            self.rng
                .fill(&mut mask)
                .map_err(|_| WsError::Protocol("RNG failure".into()))?;
            buf.extend_from_slice(&mask);
            for (i, &b) in payload.iter().enumerate() {
                buf.push(b ^ mask[i & 3]);
            }
        } else {
            buf.extend_from_slice(payload);
        }
        self.transport.write_all(&buf)?;
        self.transport.flush()?;
        Ok(())
    }

    pub fn send_json<T: Serialize>(&mut self, value: &T) -> Result<(), WsError> {
        let bytes = serde_json::to_vec(value)?;
        self.write_frame(WsOpcode::Text, &bytes)
    }

    pub fn recv_json<T: DeserializeOwned>(&mut self) -> Result<T, WsError> {
        loop {
            let frame = self.read_frame()?;
            match frame.opcode {
                WsOpcode::Text | WsOpcode::Binary => {
                    return Ok(serde_json::from_slice(&frame.payload)?);
                }
                WsOpcode::Ping => {
                    self.write_frame(WsOpcode::Pong, &frame.payload)?;
                }
                WsOpcode::Pong => continue,
                WsOpcode::Close => {
                    let _ = self.write_frame(WsOpcode::Close, &[]);
                    return Err(WsError::Closed);
                }
                WsOpcode::Continuation => {
                    return Err(WsError::Protocol(
                        "unexpected fragmented continuation frame".into(),
                    ));
                }
            }
        }
    }

    pub fn send_ping(&mut self, payload: &[u8]) -> Result<(), WsError> {
        self.write_frame(WsOpcode::Ping, payload)
    }

    pub fn close(&mut self) -> Result<(), WsError> {
        self.write_frame(WsOpcode::Close, &[])
    }
}

/// Parse the frame at the start of `buf`: `Ok(None)` when more bytes are needed, else the frame
/// and how many bytes it used. Everything that can be refused from the header alone is refused
/// before the payload is waited for.
fn parse_frame(buf: &[u8], is_client: bool) -> Result<Option<(WsFrame, usize)>, WsError> {
    if buf.len() < 2 {
        return Ok(None);
    }
    if buf[0] & 0x70 != 0 {
        return Err(WsError::Protocol(
            "reserved bits set without a negotiated extension".into(),
        ));
    }
    let fin = (buf[0] & 0x80) != 0;
    let opcode = WsOpcode::from_u8(buf[0] & 0x0F)?;
    let is_control = (opcode as u8) & 0x8 != 0;
    let masked = (buf[1] & 0x80) != 0;
    // RFC 6455 5.1: a client must mask every frame, a server must not.
    if masked == is_client {
        return Err(WsError::Protocol(if masked {
            "server frame is masked".into()
        } else {
            "client frame is not masked".into()
        }));
    }
    let mut pos = 2usize;
    let mut len = (buf[1] & 0x7F) as usize;
    if len == 126 {
        if buf.len() < pos + 2 {
            return Ok(None);
        }
        len = u16::from_be_bytes([buf[pos], buf[pos + 1]]) as usize;
        pos += 2;
    } else if len == 127 {
        if buf.len() < pos + 8 {
            return Ok(None);
        }
        let mut ext = [0u8; 8];
        ext.copy_from_slice(&buf[pos..pos + 8]);
        len = usize::try_from(u64::from_be_bytes(ext))
            .map_err(|_| WsError::Protocol("frame payload too large".into()))?;
        pos += 8;
    }
    if is_control && (!fin || len > 125) {
        return Err(WsError::Protocol(
            "control frame is fragmented or longer than 125 bytes".into(),
        ));
    }
    if !is_control && !fin {
        return Err(WsError::Protocol(
            "fragmented messages are not supported".into(),
        ));
    }
    if len > MAX_FRAME_PAYLOAD {
        return Err(WsError::Protocol(format!(
            "frame payload {len} exceeds maximum ({MAX_FRAME_PAYLOAD})"
        )));
    }
    let mut mask = [0u8; 4];
    if masked {
        if buf.len() < pos + 4 {
            return Ok(None);
        }
        mask.copy_from_slice(&buf[pos..pos + 4]);
        pos += 4;
    }
    if buf.len() < pos + len {
        return Ok(None);
    }
    let mut payload = buf[pos..pos + len].to_vec();
    if masked {
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= mask[i & 3];
        }
    }
    Ok(Some((WsFrame { opcode, payload }, pos + len)))
}

fn split_host_port(host_port: &str, default_port: u16) -> Option<(&str, u16)> {
    if let Some(rest) = host_port.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        if after.is_empty() {
            return Some((host, default_port));
        }
        let port_str = after.strip_prefix(':')?;
        let port = port_str.parse::<u16>().ok()?;
        return Some((host, port));
    }
    if let Some((h, p)) = host_port.rsplit_once(':') {
        let port = p.parse::<u16>().ok()?;
        Some((h, port))
    } else {
        Some((host_port, default_port))
    }
}

fn read_http_headers<R: Read>(reader: &mut R) -> Result<String, WsError> {
    let mut buf = Vec::with_capacity(512);
    let mut byte = [0u8; 1];
    while buf.len() < 16_384 {
        // A signal (a child's SIGCHLD is enough) interrupts a read on a socket with a receive
        // timeout and Linux does not restart it; no byte was consumed, so read again.
        let n = match reader.read(&mut byte) {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        };
        if n == 0 {
            return Err(WsError::Handshake("unexpected EOF reading headers".into()));
        }
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n\r\n") {
            return String::from_utf8(buf)
                .map_err(|e| WsError::Handshake(format!("non-UTF8 HTTP headers: {e}")));
        }
    }
    Err(WsError::Handshake("HTTP headers exceeded 16 KB".into()))
}

fn compute_accept_key(sec_key: &str) -> String {
    let mut input = Vec::with_capacity(sec_key.len() + WS_GUID.len());
    input.extend_from_slice(sec_key.trim().as_bytes());
    input.extend_from_slice(WS_GUID.as_bytes());
    let digest = sha1_digest(&input);
    BASE64.encode(digest)
}

/// Compact RFC 3174 SHA-1 implementation used solely for RFC 6455 `Sec-WebSocket-Accept`.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// Yields `Interrupted` before every real byte, as a signal-hit socket read does.
    struct InterruptedThenBytes {
        data: Vec<u8>,
        pos: usize,
        pending_interrupt: bool,
    }

    impl Read for InterruptedThenBytes {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.pending_interrupt {
                self.pending_interrupt = false;
                return Err(io::Error::from(io::ErrorKind::Interrupted));
            }
            self.pending_interrupt = true;
            let Some(&b) = self.data.get(self.pos) else {
                return Ok(0);
            };
            buf[0] = b;
            self.pos += 1;
            Ok(1)
        }
    }

    #[test]
    fn header_reads_survive_interrupted_syscalls() {
        let raw = b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\r\n";
        let mut reader = InterruptedThenBytes {
            data: raw.to_vec(),
            pos: 0,
            pending_interrupt: true,
        };
        let headers = read_http_headers(&mut reader).expect("Interrupted is retried");
        assert_eq!(headers.as_bytes(), raw);
    }

    #[test]
    fn rfc6455_accept_key_matches_spec_test_vector() {
        assert_eq!(
            compute_accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn websocket_client_and_server_exchange_json_and_ping_pong() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let server = std::thread::spawn(move || {
            let (tcp, _) = listener.accept().unwrap();
            let mut ws = WsStream::accept(tcp, Duration::from_secs(2)).unwrap();
            let val: serde_json::Value = ws.recv_json().unwrap();
            assert_eq!(val["hello"], "world");
            ws.send_ping(b"ping-1").unwrap();
            ws.send_json(&serde_json::json!({"ack": true})).unwrap();
        });

        let mut client =
            WsStream::connect(&format!("ws://{addr}/wire"), Duration::from_secs(2)).unwrap();
        client
            .send_json(&serde_json::json!({"hello": "world"}))
            .unwrap();
        let reply: serde_json::Value = client.recv_json().unwrap();
        assert_eq!(reply["ack"], true);
        server.join().unwrap();
    }

    /// Raw client-to-server bytes for tests: a masked frame with a zero mask key.
    fn masked_frame(first: u8, payload: &[u8]) -> Vec<u8> {
        let mut f = vec![first];
        if payload.len() < 126 {
            f.push(0x80 | payload.len() as u8);
        } else {
            f.push(0x80 | 126);
            f.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        }
        f.extend_from_slice(&[0, 0, 0, 0]);
        f.extend_from_slice(payload);
        f
    }

    /// Open a server-side `WsStream` whose peer is a raw TCP client that has finished the upgrade.
    fn server_with_raw_client() -> (WsStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let raw = std::thread::spawn(move || {
            let mut tcp = TcpStream::connect(addr).unwrap();
            tcp.write_all(
                b"GET / HTTP/1.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
                  Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n",
            )
            .unwrap();
            let mut resp = [0u8; 1];
            let mut seen = Vec::new();
            while !seen.ends_with(b"\r\n\r\n") {
                tcp.read_exact(&mut resp).unwrap();
                seen.push(resp[0]);
            }
            tcp
        });
        let (tcp, _) = listener.accept().unwrap();
        let ws = WsStream::accept(tcp, Duration::from_secs(2)).unwrap();
        (ws, raw.join().unwrap())
    }

    /// Audit W-2: a read timeout in the middle of a frame used to discard the bytes already read,
    /// so the daemon's 50 ms poll desynchronised the stream on any frame that straddled it.
    #[test]
    fn frame_split_across_a_read_timeout_is_resumed_not_lost() {
        let (mut ws, mut raw) = server_with_raw_client();
        ws.set_read_timeout(Some(Duration::from_millis(40)))
            .unwrap();
        let body = br#"{"hello":"split"}"#;
        let frame = masked_frame(0x81, body);
        // Header, then the first few payload bytes, then a pause longer than the read timeout.
        raw.write_all(&frame[..8]).unwrap();
        raw.flush().unwrap();
        let mut timed_out = false;
        let mut got: Option<serde_json::Value> = None;
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut sent_rest = false;
        while got.is_none() && Instant::now() < deadline {
            match ws.recv_json::<serde_json::Value>() {
                Ok(v) => got = Some(v),
                Err(WsError::Io(e))
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    timed_out = true;
                    if !sent_rest {
                        raw.write_all(&frame[8..]).unwrap();
                        sent_rest = true;
                    }
                }
                Err(e) => panic!("stream desynchronised after a mid-frame timeout: {e}"),
            }
        }
        assert!(timed_out, "the test never hit a mid-frame timeout");
        assert_eq!(got.unwrap()["hello"], "split");
    }

    #[test]
    fn two_frames_in_one_segment_are_both_delivered_in_order() {
        let (mut ws, mut raw) = server_with_raw_client();
        let mut both = masked_frame(0x81, br#"{"n":1}"#);
        both.extend(masked_frame(0x81, br#"{"n":2}"#));
        raw.write_all(&both).unwrap();
        let a: serde_json::Value = ws.recv_json().unwrap();
        let b: serde_json::Value = ws.recv_json().unwrap();
        assert_eq!((a["n"].as_i64(), b["n"].as_i64()), (Some(1), Some(2)));
    }

    #[test]
    fn oversize_length_header_is_refused_before_the_payload_arrives() {
        let (mut ws, mut raw) = server_with_raw_client();
        let mut hdr = vec![0x82, 0x80 | 127];
        hdr.extend_from_slice(&((MAX_FRAME_PAYLOAD as u64) + 1).to_be_bytes());
        hdr.extend_from_slice(&[0; 4]);
        raw.write_all(&hdr).unwrap();
        assert!(matches!(ws.read_frame(), Err(WsError::Protocol(_))));
        // A length whose top bit is set must not wrap into something small.
        let (mut ws, mut raw) = server_with_raw_client();
        let mut hdr = vec![0x82, 0x80 | 127];
        hdr.extend_from_slice(&u64::MAX.to_be_bytes());
        hdr.extend_from_slice(&[0; 4]);
        raw.write_all(&hdr).unwrap();
        assert!(matches!(ws.read_frame(), Err(WsError::Protocol(_))));
    }

    #[test]
    fn malformed_frames_are_protocol_errors() {
        // (label, bytes) — each is a client frame that RFC 6455 forbids or this server rejects.
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("reserved bit set", masked_frame(0xC1, b"{}")),
            ("fragmented text", masked_frame(0x01, b"{}")),
            ("fragmented ping", masked_frame(0x09, b"")),
            ("oversize ping", masked_frame(0x89, &[b'x'; 126])),
            ("unknown opcode", masked_frame(0x83, b"{}")),
            ("unmasked client frame", vec![0x81, 0x02, b'{', b'}']),
        ];
        for (label, bytes) in cases {
            let (mut ws, mut raw) = server_with_raw_client();
            raw.write_all(&bytes).unwrap();
            assert!(
                matches!(ws.read_frame(), Err(WsError::Protocol(_))),
                "{label} was accepted"
            );
        }
    }

    #[test]
    fn peer_closing_mid_frame_is_an_error_and_a_clean_close_is_closed() {
        let (mut ws, mut raw) = server_with_raw_client();
        raw.write_all(&masked_frame(0x81, br#"{"a":1}"#)[..5])
            .unwrap();
        drop(raw);
        assert!(matches!(ws.read_frame(), Err(WsError::Io(_))));
        let (mut ws, raw) = server_with_raw_client();
        drop(raw);
        assert!(matches!(ws.read_frame(), Err(WsError::Closed)));
    }

    #[test]
    fn empty_and_126_byte_payloads_round_trip() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (tcp, _) = listener.accept().unwrap();
            let mut ws = WsStream::accept(tcp, Duration::from_secs(2)).unwrap();
            let empty = ws.read_frame().unwrap();
            let edge = ws.read_frame().unwrap();
            let big = ws.read_frame().unwrap();
            (empty.payload.len(), edge.payload.len(), big.payload.len())
        });
        let mut client =
            WsStream::connect(&format!("ws://{addr}/wire"), Duration::from_secs(2)).unwrap();
        client.write_frame(WsOpcode::Binary, &[]).unwrap();
        client.write_frame(WsOpcode::Binary, &[7; 125]).unwrap();
        client.write_frame(WsOpcode::Binary, &[7; 65_536]).unwrap();
        assert_eq!(server.join().unwrap(), (0, 125, 65_536));
        assert!(matches!(
            client.write_frame(WsOpcode::Binary, &vec![0; MAX_FRAME_PAYLOAD + 1]),
            Err(WsError::Protocol(_))
        ));
    }

    #[test]
    fn connect_rejects_urls_that_could_inject_request_headers() {
        for url in [
            "ws://127.0.0.1:1/x\r\nX-Evil: 1",
            "ws://127.0.0.1:1/a b",
            "ws://127.0.0.1:1/\0",
        ] {
            assert!(
                matches!(
                    WsStream::connect(url, Duration::from_millis(50)),
                    Err(WsError::InvalidUrl(_))
                ),
                "{url:?}"
            );
        }
    }

    #[test]
    fn host_port_split_handles_bracketed_ipv6_and_bad_ports() {
        assert_eq!(split_host_port("[::1]:9", 80), Some(("::1", 9)));
        assert_eq!(split_host_port("[::1]", 80), Some(("::1", 80)));
        assert_eq!(split_host_port("[::1", 80), None);
        assert_eq!(split_host_port("[::1]x", 80), None);
        assert_eq!(split_host_port("host:70000", 80), None);
        assert_eq!(split_host_port("host:", 80), None);
        assert_eq!(split_host_port("host", 443), Some(("host", 443)));
    }

    #[test]
    fn http_header_reader_stops_at_16_kib() {
        let mut huge = vec![b'a'; 20_000];
        huge.extend_from_slice(b"\r\n\r\n");
        assert!(read_http_headers(&mut huge.as_slice()).is_err());
        let mut eof = &b"GET / HTTP/1.1\r\n"[..];
        assert!(read_http_headers(&mut eof).is_err());
    }
}
