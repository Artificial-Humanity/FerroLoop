//! Cross-boundary WebSocket/TLS peer protocol, mutual HMAC-SHA256 PSK handshake,
//! and catch-up sync framing (spec §3.4).

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use fl_core::agent::RegistryEvent;
use fl_core::ferrowire::{DeliveryReceipt, WireMessage};
use ring::hmac;
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};

use crate::ws::{WsError, WsStream};

/// Version 2 binds each handshake MAC to the role of the side that produced it (see [`PeerRole`]).
pub const WIRE_PROTOCOL_VERSION: u32 = 2;
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
pub const PEER_PING_INTERVAL: Duration = Duration::from_secs(15);
pub const PEER_READ_TIMEOUT: Duration = Duration::from_secs(45);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PeerFrame {
    Challenge {
        protocol_version: u32,
        node_id: String,
        host: String,
        nonce: String,
    },
    Auth {
        mac: String,
        last_msg_seq: u64,
        last_rcpt_seq: u64,
        last_agent_seq: u64,
    },
    SyncBatch {
        registry_events: Vec<RegistryEvent>,
        messages: Vec<WireMessage>,
        receipts: Vec<DeliveryReceipt>,
    },
    Publish {
        message: WireMessage,
        receipts: Vec<DeliveryReceipt>,
    },
    ReceiptUpdate {
        receipt: DeliveryReceipt,
    },
    RegistryUpdate {
        event: RegistryEvent,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerHandshakeOutcome {
    pub remote_node_id: String,
    pub remote_host: String,
    pub remote_last_msg_seq: u64,
    pub remote_last_rcpt_seq: u64,
    pub remote_last_agent_seq: u64,
}

/// Which end of the TCP connection produced a handshake MAC.
///
/// Both ends run the same handshake, so without a role in the MAC input a peer could ask the
/// victim to sign a challenge on one connection and replay that answer on another (a reflection
/// attack that needs no PSK). A MAC made by a `Server` never verifies as a `Client` MAC.
///
/// Residual (not fixed here): the role tag stops an off-path attacker who opens two connections.
/// It does not stop an attacker ON THE PATH of a plaintext `ws://` connection the victim dials
/// while the victim also listens: the attacker can pick its server nonce equal to the victim's
/// listener nonce and relay frames between the two connections. The handshake also gives no
/// channel binding and the frames after it carry no MAC, so a plaintext path attacker can hijack
/// an established session regardless. Use `wss://` for any non-loopback peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerRole {
    Client,
    Server,
}

impl PeerRole {
    fn tag(self) -> &'static [u8] {
        match self {
            Self::Client => b"client:",
            Self::Server => b"server:",
        }
    }

    fn opposite(self) -> Self {
        match self {
            Self::Client => Self::Server,
            Self::Server => Self::Client,
        }
    }
}

fn build_mac_input(
    prover: PeerRole,
    verifier_nonce: &[u8],
    prover_nonce: &[u8],
    prover_host: &str,
) -> Vec<u8> {
    let mut msg =
        Vec::with_capacity(18 + verifier_nonce.len() + prover_nonce.len() + prover_host.len());
    msg.extend_from_slice(b"fl-wire-v2:");
    msg.extend_from_slice(prover.tag());
    msg.extend_from_slice(verifier_nonce);
    msg.extend_from_slice(prover_nonce);
    msg.extend_from_slice(prover_host.as_bytes());
    msg
}

/// MAC proving that the `prover` end holds `token`, over the nonce the verifier issued, the
/// nonce the prover issued and the prover's host name.
pub fn compute_psk_mac(
    token: &[u8],
    prover: PeerRole,
    verifier_nonce: &[u8],
    prover_nonce: &[u8],
    prover_host: &str,
) -> String {
    let key = hmac::Key::new(hmac::HMAC_SHA256, token);
    let input = build_mac_input(prover, verifier_nonce, prover_nonce, prover_host);
    let tag = hmac::sign(&key, &input);
    BASE64.encode(tag.as_ref())
}

pub fn verify_psk_mac(
    token: &[u8],
    prover: PeerRole,
    verifier_nonce: &[u8],
    prover_nonce: &[u8],
    prover_host: &str,
    mac_b64: &str,
) -> bool {
    let Ok(mac_bytes) = BASE64.decode(mac_b64.trim()) else {
        return false;
    };
    let key = hmac::Key::new(hmac::HMAC_SHA256, token);
    let input = build_mac_input(prover, verifier_nonce, prover_nonce, prover_host);
    hmac::verify(&key, &input, &mac_bytes).is_ok()
}

/// Execute the mutual 32-byte nonce + HMAC-SHA256 PSK handshake on `ws` (spec §3.4).
pub fn handshake_peer(
    ws: &mut WsStream,
    local_node_id: &str,
    local_host: &str,
    psk: &[u8],
    last_msg_seq: u64,
    last_rcpt_seq: u64,
    last_agent_seq: u64,
) -> Result<PeerHandshakeOutcome, WsError> {
    ws.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    ws.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;

    let rng = SystemRandom::new();
    let mut local_nonce = [0u8; 32];
    rng.fill(&mut local_nonce)
        .map_err(|_| WsError::Handshake("RNG failure".into()))?;
    let local_nonce_b64 = BASE64.encode(local_nonce);

    ws.send_json(&PeerFrame::Challenge {
        protocol_version: WIRE_PROTOCOL_VERSION,
        node_id: local_node_id.to_string(),
        host: local_host.to_string(),
        nonce: local_nonce_b64,
    })?;

    let challenge: PeerFrame = ws.recv_json()?;
    let PeerFrame::Challenge {
        protocol_version,
        node_id: remote_node_id,
        host: remote_host,
        nonce: remote_nonce_b64,
    } = challenge
    else {
        let _ = ws.close();
        return Err(WsError::Handshake("expected PeerFrame::Challenge".into()));
    };

    if protocol_version != WIRE_PROTOCOL_VERSION {
        let _ = ws.close();
        return Err(WsError::Handshake(format!(
            "unsupported wire protocol version {protocol_version}"
        )));
    }
    let remote_nonce = BASE64
        .decode(remote_nonce_b64.trim())
        .map_err(|_| WsError::Handshake("invalid base64 remote nonce".into()))?;
    if remote_nonce.len() != 32 {
        let _ = ws.close();
        return Err(WsError::Handshake("remote nonce must be 32 bytes".into()));
    }

    let local_role = if ws.is_client() {
        PeerRole::Client
    } else {
        PeerRole::Server
    };
    let local_mac = compute_psk_mac(psk, local_role, &remote_nonce, &local_nonce, local_host);
    ws.send_json(&PeerFrame::Auth {
        mac: local_mac,
        last_msg_seq,
        last_rcpt_seq,
        last_agent_seq,
    })?;

    let auth: PeerFrame = ws.recv_json()?;
    let PeerFrame::Auth {
        mac: remote_mac,
        last_msg_seq: remote_last_msg_seq,
        last_rcpt_seq: remote_last_rcpt_seq,
        last_agent_seq: remote_last_agent_seq,
    } = auth
    else {
        let _ = ws.close();
        return Err(WsError::Handshake("expected PeerFrame::Auth".into()));
    };

    // The remote proved itself over the nonce we issued, its own nonce and its host, in the
    // role opposite to ours.
    if !verify_psk_mac(
        psk,
        local_role.opposite(),
        &local_nonce,
        &remote_nonce,
        &remote_host,
        &remote_mac,
    ) {
        let _ = ws.close();
        return Err(WsError::Handshake(
            "PSK HMAC-SHA256 authentication failed".into(),
        ));
    }

    ws.set_read_timeout(Some(PEER_READ_TIMEOUT))?;
    Ok(PeerHandshakeOutcome {
        remote_node_id,
        remote_host,
        remote_last_msg_seq,
        remote_last_rcpt_seq,
        remote_last_agent_seq,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn mutual_psk_handshake_succeeds_for_matching_token_and_rejects_mismatch() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        // 1. Matching PSK
        let srv = std::thread::spawn(move || {
            let (tcp, _) = listener.accept().unwrap();
            let mut ws = WsStream::accept(tcp, Duration::from_secs(2)).unwrap();
            handshake_peer(&mut ws, "node-b", "host-b", b"shared-secret", 10, 20, 5).unwrap()
        });

        let mut cli =
            WsStream::connect(&format!("ws://{addr}/wire"), Duration::from_secs(2)).unwrap();
        let out_a =
            handshake_peer(&mut cli, "node-a", "host-a", b"shared-secret", 1, 2, 3).unwrap();
        let out_b = srv.join().unwrap();

        assert_eq!(out_a.remote_node_id, "node-b");
        assert_eq!(out_a.remote_last_msg_seq, 10);
        assert_eq!(out_b.remote_node_id, "node-a");
        assert_eq!(out_b.remote_last_msg_seq, 1);

        // 2. Mismatched PSK fails authentication
        let listener2 = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr2 = listener2.local_addr().unwrap();
        let srv2 = std::thread::spawn(move || {
            let (tcp, _) = listener2.accept().unwrap();
            let mut ws = WsStream::accept(tcp, Duration::from_secs(2)).unwrap();
            handshake_peer(&mut ws, "node-b", "host-b", b"secret-1", 0, 0, 0)
        });
        let mut cli2 =
            WsStream::connect(&format!("ws://{addr2}/wire"), Duration::from_secs(2)).unwrap();
        let res_a = handshake_peer(&mut cli2, "node-a", "host-a", b"wrong-secret", 0, 0, 0);
        let res_b = srv2.join().unwrap();
        assert!(res_a.is_err() || res_b.is_err());
    }

    /// Audit W-1: the handshake MAC carried no role binding, so a server that signs its own
    /// challenge response on one connection can be replayed against itself on another. The
    /// attacker needs no PSK: it asks the victim to answer a challenge built from the nonce the
    /// victim issued on a second connection, then presents that answer back as its own.
    #[test]
    fn psk_handshake_is_not_open_to_a_reflection_attack() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let victim = std::thread::spawn(move || {
            let mut handles = Vec::new();
            for _ in 0..2 {
                let (tcp, _) = listener.accept().unwrap();
                handles.push(std::thread::spawn(move || {
                    let mut ws = WsStream::accept(tcp, Duration::from_secs(2)).unwrap();
                    handshake_peer(&mut ws, "victim", "victim-host", b"real-secret", 0, 0, 0)
                        .map(|_| ())
                }));
            }
            handles
                .into_iter()
                .map(|h| h.join().unwrap())
                .collect::<Vec<_>>()
        });

        let url = format!("ws://{addr}/wire");
        let mut oracle = WsStream::connect(&url, Duration::from_secs(2)).unwrap();
        let mut target = WsStream::connect(&url, Duration::from_secs(2)).unwrap();
        let PeerFrame::Challenge {
            nonce: oracle_nonce,
            ..
        } = oracle.recv_json::<PeerFrame>().unwrap()
        else {
            panic!("expected a challenge on the oracle connection");
        };
        let PeerFrame::Challenge {
            nonce: target_nonce,
            ..
        } = target.recv_json::<PeerFrame>().unwrap()
        else {
            panic!("expected a challenge on the target connection");
        };

        // Oracle connection: claim the nonce the victim issued on the target connection.
        oracle
            .send_json(&PeerFrame::Challenge {
                protocol_version: WIRE_PROTOCOL_VERSION,
                node_id: "attacker".into(),
                host: "attacker-host".into(),
                nonce: target_nonce,
            })
            .unwrap();
        let PeerFrame::Auth { mac, .. } = oracle.recv_json::<PeerFrame>().unwrap() else {
            panic!("expected the victim's Auth on the oracle connection");
        };
        drop(oracle);

        // Target connection: present the oracle's nonce, the victim's own host, and its MAC.
        target
            .send_json(&PeerFrame::Challenge {
                protocol_version: WIRE_PROTOCOL_VERSION,
                node_id: "attacker".into(),
                host: "victim-host".into(),
                nonce: oracle_nonce,
            })
            .unwrap();
        let _victim_auth = target.recv_json::<PeerFrame>().unwrap();
        target
            .send_json(&PeerFrame::Auth {
                mac,
                last_msg_seq: 0,
                last_rcpt_seq: 0,
                last_agent_seq: 0,
            })
            .unwrap();

        let results = victim.join().unwrap();
        assert!(
            results[1].is_err(),
            "reflected MAC authenticated an attacker that does not hold the PSK"
        );
    }
}
