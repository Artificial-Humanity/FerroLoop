//! Minimal RFC 6455 WebSocket framing and client/server upgrade over TCP (`ws://`) and TLS (`wss://`).

use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

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
}

impl WsStream {
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
        })
    }

    pub fn read_frame(&mut self) -> Result<WsFrame, WsError> {
        let mut hdr = [0u8; 2];
        if let Err(e) = self.transport.read_exact(&mut hdr) {
            if e.kind() == io::ErrorKind::UnexpectedEof {
                return Err(WsError::Closed);
            }
            return Err(WsError::Io(e));
        }
        let _fin = (hdr[0] & 0x80) != 0;
        let opcode = WsOpcode::from_u8(hdr[0] & 0x0F)?;
        let masked = (hdr[1] & 0x80) != 0;
        let mut len = (hdr[1] & 0x7F) as usize;

        if len == 126 {
            let mut ext = [0u8; 2];
            self.transport.read_exact(&mut ext)?;
            len = u16::from_be_bytes(ext) as usize;
        } else if len == 127 {
            let mut ext = [0u8; 8];
            self.transport.read_exact(&mut ext)?;
            len = usize::try_from(u64::from_be_bytes(ext))
                .map_err(|_| WsError::Protocol("frame payload too large".into()))?;
        }
        if len > MAX_FRAME_PAYLOAD {
            return Err(WsError::Protocol(format!(
                "frame payload {len} exceeds maximum ({MAX_FRAME_PAYLOAD})"
            )));
        }

        let mut mask = [0u8; 4];
        if masked {
            self.transport.read_exact(&mut mask)?;
        }
        let mut payload = vec![0u8; len];
        if len > 0 {
            self.transport.read_exact(&mut payload)?;
        }
        if masked {
            for (i, b) in payload.iter_mut().enumerate() {
                *b ^= mask[i & 3];
            }
        }
        Ok(WsFrame { opcode, payload })
    }

    pub fn write_frame(&mut self, opcode: WsOpcode, payload: &[u8]) -> Result<(), WsError> {
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
        let n = reader.read(&mut byte)?;
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
}
