//! Authenticated transport, endpoint ownership, and platform policy tests.
#[cfg(unix)]
use std::path::Path;
use std::path::PathBuf;
#[cfg(unix)]
use std::sync::{Arc, Mutex};
#[cfg(unix)]
use std::time::{Duration, Instant};

#[cfg(unix)]
use fl_host_proto::{
    AsyncHostClient, BlockingHostClient, ConnectOptions, Deadlines, FrameKind, HANDSHAKE_TIMEOUT,
    HandshakeParams, HostListener, HostRequest, HostResponse, MAX_UNAUTHENTICATED, PROTO_MAJOR,
    PROTO_MINOR, RequestId, client_hello_frame, decode_challenge, read_frame_async,
    server_handshake, supports_message, write_frame_async,
};
use fl_host_proto::{
    AuthToken, BindDisposition, FirstInstance, HostConfig, HostEndpoint, HostError, InstanceLedger,
    MutexFastPath, NamedPipeClientOptions, NamedPipeServerOptions, PeerQuery, TokenFileCreate,
    WinPeerLookup, auth_transcript, client_proof, server_proof, validate_unix_peer,
    verify_client_proof, verify_server_proof,
};

#[cfg(unix)]
fn short_dir() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "flh{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&path).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    path
}

#[cfg(unix)]
struct Tmp(PathBuf);
#[cfg(unix)]
impl Tmp {
    fn new() -> Self {
        Self(short_dir())
    }
    fn path(&self) -> &Path {
        &self.0
    }
}
#[cfg(unix)]
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(unix)]
fn endpoint(dir: &Path) -> HostEndpoint {
    HostEndpoint::unix(dir.join("h.sock")).unwrap()
}

#[cfg(unix)]
fn config(dir: &Path) -> HostConfig {
    let mut config = HostConfig::new(endpoint(dir));
    config.daemon_version = "fl-host-version-unique-zz".into();
    config.handshake_timeout = Duration::from_millis(800);
    config.frame_timeout = Duration::from_millis(800);
    config.write_timeout = Duration::from_secs(2);
    config
}

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::symlink_metadata(path)
        .unwrap()
        .permissions()
        .mode()
        & 0o777
}

#[cfg(unix)]
fn deadlines() -> Deadlines {
    Deadlines {
        handshake: Duration::from_secs(2),
        frame: Duration::from_secs(2),
        write: Duration::from_secs(2),
    }
}

#[test]
fn client_refuses_foreign_uid_server() {
    assert!(validate_unix_peer(PeerQuery::Uid(1000), 1000).is_ok());
    assert!(validate_unix_peer(PeerQuery::Uid(0), 0).is_ok());
    assert!(matches!(
        validate_unix_peer(PeerQuery::Uid(0), 1000),
        Err(HostError::ForeignPeer { peer: 0, own: 1000 })
    ));
    assert!(matches!(
        validate_unix_peer(PeerQuery::Uid(1), 0),
        Err(HostError::ForeignPeer { peer: 1, own: 0 })
    ));
    assert!(matches!(
        validate_unix_peer(PeerQuery::Error, 1000),
        Err(HostError::PeerIdentityUnavailable)
    ));
    assert!(matches!(
        validate_unix_peer(PeerQuery::Unsupported, 0),
        Err(HostError::PeerIdentityUnavailable)
    ));
    #[cfg(unix)]
    {
        assert!(fl_host_proto::interpret_ucred(&[0, 1, 2, 3]).is_err());
        let mut cred = vec![0u8; std::mem::size_of::<libc::ucred>()];
        let uid = 7u32;
        // pid, uid, gid are the first three native-endian u32 fields.
        cred[4..8].copy_from_slice(&uid.to_ne_bytes());
        assert_eq!(fl_host_proto::interpret_ucred(&cred).unwrap(), 7);
    }
}

#[test]
fn mutual_nonce_proofs_reject_replay_and_reflection() {
    let token = [9u8; 32];
    let ns = [1u8; 32];
    let nc = [2u8; 32];
    let identity = b"/tmp/flh.sock";
    let transcript = auth_transcript(identity, 1, 0, &ns, &nc);
    let client = client_proof(&token, &transcript);
    let server = server_proof(&token, &transcript);
    assert_ne!(client, server);
    assert!(verify_client_proof(&token, &transcript, &client));
    assert!(verify_server_proof(&token, &transcript, &server));
    assert!(!verify_client_proof(&token, &transcript, &server));
    assert!(!verify_server_proof(&token, &transcript, &client));
    let replayed = auth_transcript(identity, 1, 0, &[3u8; 32], &nc);
    assert!(!verify_client_proof(&token, &replayed, &client));
    assert!(fl_host_proto::reject_equal_nonces(&ns, &ns).is_err());
    assert!(fl_host_proto::reject_equal_nonces(&ns, &nc).is_ok());
    assert_ne!(&transcript[..20], b"fl-store/1/client");
}

#[test]
fn token_debug_is_redacted() {
    let token = AuthToken::new([0xab; 32]);
    let shown = format!("{token:?} {token}");
    assert!(!shown.to_ascii_lowercase().contains("abab"));
    assert!(shown.contains("redacted"));
    let endpoint = HostEndpoint::unix("/tmp/unique-endpoint-name.sock").unwrap();
    assert!(!format!("{endpoint:?}").contains("unique-endpoint-name"));
    let config = HostConfig::new(endpoint);
    let shown = format!("{config:?}");
    assert!(!shown.contains("unique-endpoint-name"));
    assert!(!shown.to_ascii_lowercase().contains("abab"));
}

#[test]
fn lock_token_registry_epochs_derive_from_endpoint() {
    let left = HostEndpoint::unix("/tmp/a.sock").unwrap();
    let right = HostEndpoint::unix("/tmp/b.sock").unwrap();
    assert_eq!(left.lock_path(), PathBuf::from("/tmp/a.sock.lock"));
    assert_eq!(left.token_path(), PathBuf::from("/tmp/a.sock.token"));
    assert_eq!(left.state_path(), PathBuf::from("/tmp/a.sock.state.json"));
    assert_ne!(left.lock_path(), right.lock_path());
    assert_ne!(left.token_path(), right.token_path());
    assert_ne!(left.state_path(), right.state_path());
    let folded = HostEndpoint::windows("Foo", "S-1-5-18").unwrap();
    let lower = HostEndpoint::windows("foo", "S-1-5-18").unwrap();
    let other = HostEndpoint::windows("bar", "S-1-5-18").unwrap();
    assert_eq!(folded.token_file_name(), lower.token_file_name());
    assert_eq!(folded.mutex_name(), lower.mutex_name());
    assert_eq!(folded.state_file_name(), lower.state_file_name());
    assert_ne!(folded.mutex_name(), other.mutex_name());
    assert!(folded.mutex_name().unwrap().starts_with("Local\\fl-host-"));
    assert!(HostEndpoint::windows("..", "S-1-5-18").is_err());
    assert!(HostEndpoint::windows("", "S-1-5-18").is_err());
    assert!(HostEndpoint::windows("a/b", "S-1-5-18").is_err());
    assert!(HostEndpoint::windows(&"a".repeat(201), "S-1-5-18").is_err());
}

#[test]
fn windows_peer_identity_lookup_unavailable_refused() {
    assert!(validate_win_peer_ok(WinPeerLookup::SidMatch));
    assert!(matches!(
        fl_host_proto::validate_win_peer(WinPeerLookup::SidMismatch),
        Err(HostError::ForeignIdentity)
    ));
    for lookup in [WinPeerLookup::AccessDenied, WinPeerLookup::Failed] {
        assert!(
            matches!(
                fl_host_proto::validate_win_peer(lookup),
                Err(HostError::PeerIdentityUnavailable)
            ),
            "elevated and non-elevated lookup failures stay refused"
        );
    }
}

fn validate_win_peer_ok(lookup: WinPeerLookup) -> bool {
    fl_host_proto::validate_win_peer(lookup).is_ok()
}

#[test]
fn windows_standby_instance_no_gap() {
    let mut ledger = InstanceLedger::first();
    assert!(ledger.handoff().is_err());
    assert!(ledger.alive() >= 1);
    ledger.precreate_standby();
    assert!(ledger.alive() >= 2);
    ledger.handoff().unwrap();
    assert!(ledger.alive() >= 1);
    ledger.shutdown();
    assert_eq!(ledger.alive(), 0);
}

#[test]
fn windows_first_instance_is_authoritative_across_mutex_names() {
    let denied =
        fl_host_proto::decide_first_instance(FirstInstance::AccessDenied, MutexFastPath::Free);
    assert!(matches!(denied.error, Some(HostError::AlreadyRunning)));
    assert!(!denied.touch_token);
    let other_mutex = fl_host_proto::decide_first_instance(
        FirstInstance::AccessDenied,
        MutexFastPath::HeldElsewhere,
    );
    assert!(!other_mutex.touch_token);
    assert!(matches!(other_mutex.error, Some(HostError::AlreadyRunning)));
    let created =
        fl_host_proto::decide_first_instance(FirstInstance::Created, MutexFastPath::HeldElsewhere);
    assert!(created.proceed);
    assert!(created.error.is_none());
    let _ = BindDisposition::default();
}

#[test]
fn windows_pipe_dacl_single_ace() {
    let sid = "S-1-5-21-1";
    let sddl = fl_host_proto::pipe_dacl_sddl(sid);
    assert!(fl_host_proto::dacl_is_single_sid(&sddl, sid));
    assert!(!fl_host_proto::dacl_is_single_sid("D:(A;;GA;;;WD)", sid));
    assert!(!fl_host_proto::dacl_is_single_sid("D:P(A;;GA;;;AU)", sid));
    assert!(!fl_host_proto::dacl_is_single_sid("D:P(A;;GA;;;NU)", sid));
    assert!(!fl_host_proto::dacl_is_single_sid(
        &format!("D:(A;;GA;;;{sid})"),
        sid
    ));
}

#[test]
fn windows_token_dacl_applies_at_creation() {
    let create = TokenFileCreate::protected();
    assert!(create.disposition_create_new);
    assert!(create.dacl_in_security_attributes);
    assert!(!create.inherit_handle);
    assert!(!create.post_write_acl_fixup);
}

#[test]
fn windows_pipe_options_are_local_and_non_inheritable() {
    let server = NamedPipeServerOptions::authenticated_local(true);
    assert!(server.byte_mode);
    assert!(server.reject_remote_clients);
    assert!(!server.inherit_handle);
    assert!(server.first_pipe_instance);
    let standby = NamedPipeServerOptions::authenticated_local(false);
    assert!(!standby.first_pipe_instance);
    assert!(standby.reject_remote_clients);
    let client = NamedPipeClientOptions::identification();
    assert!(client.sqos_identification);
    assert!(!client.inherit_handle);
}

#[cfg(unix)]
mod unix_transport {
    use super::*;
    use std::io::Read;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn same_user_handshake_succeeds() {
        let dir = Tmp::new();
        let listener = HostListener::bind(config(dir.path())).await.unwrap();
        let endpoint = listener.endpoint().clone();
        assert_eq!(mode(endpoint.socket_path().parent().unwrap()), 0o700);
        assert_eq!(mode(endpoint.socket_path()), 0o600);
        assert_eq!(mode(&endpoint.token_path()), 0o600);
        assert_eq!(mode(&endpoint.lock_path()), 0o600);
        let server = tokio::spawn(async move {
            let mut listener = listener;
            for _ in 0..2 {
                let mut conn = listener.accept().await.unwrap();
                let request = conn.recv().await.unwrap();
                let id = request.request_id();
                conn.respond(&HostResponse::Listed {
                    id,
                    children: vec![],
                })
                .await
                .unwrap();
            }
            listener
        });
        let mut client = AsyncHostClient::connect(&endpoint).await.unwrap();
        let response = client
            .request(HostRequest::List { id: RequestId(1) })
            .await
            .unwrap();
        assert_eq!(response.request_id(), RequestId(1));
        let blocking = tokio::task::spawn_blocking(move || {
            let mut client = BlockingHostClient::connect(&endpoint).unwrap();
            client.request(HostRequest::List { id: RequestId(2) })
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(blocking.request_id(), RequestId(2));
        server.await.unwrap().shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn wrong_token_fails() {
        let dir = Tmp::new();
        let mut listener = HostListener::bind(config(dir.path())).await.unwrap();
        let endpoint = listener.endpoint().clone();
        let original = std::fs::read(endpoint.token_path()).unwrap();
        let wrong = [0x5au8; 32];
        std::fs::write(endpoint.token_path(), wrong).unwrap();
        let err = AsyncHostClient::connect(&endpoint).await.unwrap_err();
        let shown = err.to_string();
        assert!(matches!(err, HostError::Auth), "{shown}");
        assert!(!shown.contains("5a5a"));
        assert_eq!(listener.handshake_attempts(), 1);
        std::fs::write(endpoint.token_path(), &original).unwrap();
        let accept = tokio::spawn(async move { listener.accept().await });
        AsyncHostClient::connect(&endpoint).await.unwrap();
        accept.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn wrong_proto_major_fails_without_restart() {
        let dir = Tmp::new();
        let mut listener = HostListener::bind(config(dir.path())).await.unwrap();
        let endpoint = listener.endpoint().clone();
        let inode = {
            use std::os::unix::fs::MetadataExt;
            std::fs::symlink_metadata(endpoint.socket_path())
                .unwrap()
                .ino()
        };
        let options = ConnectOptions {
            major: PROTO_MAJOR + 9,
            ..ConnectOptions::default()
        };
        let err = AsyncHostClient::connect_with(&endpoint, options)
            .await
            .unwrap_err();
        match err {
            HostError::IncompatibleProtocol { daemon_version } => {
                assert_eq!(daemon_version, "fl-host-version-unique-zz");
            }
            other => panic!("{other:?}"),
        }
        let inode_after = {
            use std::os::unix::fs::MetadataExt;
            std::fs::symlink_metadata(endpoint.socket_path())
                .unwrap()
                .ino()
        };
        assert_eq!(inode, inode_after);
        let accept = tokio::spawn(async move { listener.accept().await });
        AsyncHostClient::connect(&endpoint).await.unwrap();
        accept.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn newer_minor_client_negotiates_down() {
        let dir = Tmp::new();
        let mut config = config(dir.path());
        config.minor = 0;
        let mut listener = HostListener::bind(config).await.unwrap();
        let endpoint = listener.endpoint().clone();
        let accept = tokio::spawn(async move { listener.accept().await });
        let options = ConnectOptions {
            minor: PROTO_MINOR + 5,
            ..ConnectOptions::default()
        };
        let client = AsyncHostClient::connect_with(&endpoint, options)
            .await
            .unwrap();
        assert_eq!(client.negotiated(), (PROTO_MAJOR, 0));
        assert!(!client.allows("resize"));
        assert!(client.allows("send"));
        assert!(!supports_message(0, "resize"));
        assert!(supports_message(1, "resize"));
        accept.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn bounded_restart_token_race() {
        let dir = Tmp::new();
        let listener = HostListener::bind(config(dir.path())).await.unwrap();
        let endpoint = listener.endpoint().clone();
        let control = listener.token_control();
        let once = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let flag = Arc::clone(&once);
        let rotate = control.clone();
        let options = ConnectOptions {
            pre_proof: Some(Arc::new(move || {
                if flag.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                    rotate.rotate().unwrap();
                }
            })),
            ..ConnectOptions::default()
        };
        let client = AsyncHostClient::connect_with(&endpoint, options)
            .await
            .unwrap();
        assert_eq!(listener.handshake_attempts(), 2);
        assert_eq!(listener.authenticated_requests(), 0);
        drop(client);

        let always = control.clone();
        let options = ConnectOptions {
            pre_proof: Some(Arc::new(move || {
                always.rotate().unwrap();
            })),
            ..ConnectOptions::default()
        };
        let before = listener.handshake_attempts();
        let err = AsyncHostClient::connect_with(&endpoint, options)
            .await
            .unwrap_err();
        assert!(matches!(err, HostError::Auth), "{err}");
        assert_eq!(listener.handshake_attempts(), before + 2);
        assert_eq!(listener.authenticated_requests(), 0);

        let wrong = [0x11u8; 32];
        std::fs::write(endpoint.token_path(), wrong).unwrap();
        let before = listener.handshake_attempts();
        let err = AsyncHostClient::connect(&endpoint).await.unwrap_err();
        assert!(matches!(err, HostError::Auth), "{err}");
        assert_eq!(listener.handshake_attempts(), before + 1);

        listener.testing_set_garbage_banner(true);
        std::fs::write(endpoint.token_path(), control.current_bytes()).unwrap();
        let before = listener.handshake_attempts();
        let err = AsyncHostClient::connect(&endpoint).await.unwrap_err();
        assert!(matches!(err, HostError::Protocol(_)), "{err}");
        assert_eq!(listener.handshake_attempts(), before + 1);
        assert_eq!(listener.authenticated_requests(), 0);
    }

    #[tokio::test]
    async fn token_never_sent_in_hello() {
        let token = AuthToken::new([0xa5; 32]);
        let identity = b"/tmp/flh-record.sock";
        let capture = Arc::new(Mutex::new(Vec::new()));
        let (client, server) = tokio::io::duplex(64 * 1024);
        let params = HandshakeParams {
            identity,
            token: &token,
            deadlines: deadlines(),
            daemon_version: "fl-host-version-unique-zz",
            major: PROTO_MAJOR,
            minor: PROTO_MINOR,
            capture_writes: Some(Arc::clone(&capture)),
        };
        let server = server_handshake(server, params);
        let client = fl_host_proto::client_handshake(
            client,
            identity,
            &token,
            PROTO_MAJOR,
            PROTO_MINOR,
            deadlines(),
            Some(Arc::clone(&capture)),
        );
        let (server, client) = tokio::join!(server, client);
        server.unwrap();
        client.unwrap();
        let bytes = capture.lock().unwrap().clone();
        assert!(
            bytes.windows(32).all(|window| window != token.as_bytes()),
            "token bytes crossed the wire"
        );
    }

    #[tokio::test]
    async fn version_disclosed_only_after_auth() {
        let token = AuthToken::new([3u8; 32]);
        let wrong = AuthToken::new([4u8; 32]);
        let identity = b"/tmp/flh-version.sock";
        let capture = Arc::new(Mutex::new(Vec::new()));
        let (client, server) = tokio::io::duplex(64 * 1024);
        let params = HandshakeParams {
            identity,
            token: &token,
            deadlines: deadlines(),
            daemon_version: "fl-host-version-unique-zz",
            major: PROTO_MAJOR,
            minor: PROTO_MINOR,
            capture_writes: Some(Arc::clone(&capture)),
        };
        let server = server_handshake(server, params);
        let client = fl_host_proto::client_handshake(
            client,
            identity,
            &wrong,
            PROTO_MAJOR,
            PROTO_MINOR,
            deadlines(),
            None,
        );
        let (server, client) = tokio::join!(server, client);
        assert!(matches!(server, Err(HostError::Auth)));
        assert!(matches!(client, Err(HostError::Auth)));
        let bytes = capture.lock().unwrap().clone();
        let version = b"fl-host-version-unique-zz";
        assert!(bytes.windows(version.len()).all(|window| window != version));
    }

    #[tokio::test]
    async fn request_before_auth_or_second_hello_refused() {
        let dir = Tmp::new();
        let listener = HostListener::bind(config(dir.path())).await.unwrap();
        let endpoint = listener.endpoint().clone();
        let path = endpoint.socket_path().to_path_buf();
        let mut raw = tokio::net::UnixStream::connect(&path).await.unwrap();
        let request =
            fl_host_proto::encode_request(&HostRequest::List { id: RequestId(1) }).unwrap();
        let _ =
            write_frame_async(&mut raw, &request, Instant::now() + Duration::from_secs(1)).await;
        let mut buf = [0u8; 8];
        let _ = tokio::time::timeout(Duration::from_millis(800), raw.read(&mut buf)).await;
        drop(raw);
        let mut options_listener = listener;
        let accept = tokio::spawn(async move { options_listener.accept().await });
        let client = AsyncHostClient::connect(&endpoint).await.unwrap();
        let mut conn = accept.await.unwrap().unwrap();
        let mut stream = client.into_stream();
        let hello = client_hello_frame(&[9u8; 32], PROTO_MAJOR, PROTO_MINOR, &[0u8; 32]);
        write_frame_async(&mut stream, &hello, Instant::now() + Duration::from_secs(1))
            .await
            .unwrap();
        let err = conn.recv().await.unwrap_err();
        assert!(matches!(err, HostError::Protocol(_)), "{err}");
    }

    #[tokio::test]
    async fn stalled_handshake_expires() {
        let dir = Tmp::new();
        let mut config = config(dir.path());
        config.handshake_timeout = Duration::from_millis(300);
        let mut listener = HostListener::bind(config).await.unwrap();
        let endpoint = listener.endpoint().clone();
        assert_eq!(HANDSHAKE_TIMEOUT, Duration::from_secs(5));
        let stalled = tokio::net::UnixStream::connect(endpoint.socket_path())
            .await
            .unwrap();
        let started = Instant::now();
        let limit = Duration::from_millis(1500);
        let err = loop {
            if let Some(err) = listener.handshake_error() {
                break err;
            }
            if started.elapsed() > limit {
                panic!("stalled peer was not refused within the handshake deadline");
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        };
        assert!(
            matches!(err, HostError::FrameTimeout),
            "configured deadline must surface the timeout, got {err:?}"
        );
        assert!(
            started.elapsed() >= Duration::from_millis(200),
            "failed before the deadline: {:?}",
            started.elapsed()
        );
        assert!(started.elapsed() < limit);
        drop(stalled);
        let accept = tokio::spawn(async move { listener.accept().await });
        AsyncHostClient::connect(&endpoint).await.unwrap();
        accept.await.unwrap().unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unauth_frame_cap_and_connection_cap() {
        assert_eq!(MAX_UNAUTHENTICATED, 32);
        let dir = Tmp::new();
        let mut config = config(dir.path());
        config.handshake_timeout = Duration::from_millis(500);
        config.max_unauthenticated = MAX_UNAUTHENTICATED;
        let listener = HostListener::bind(config).await.unwrap();
        let path = listener.endpoint().socket_path().to_path_buf();
        let mut held = Vec::new();
        for _ in 0..MAX_UNAUTHENTICATED {
            let mut stream = std::os::unix::net::UnixStream::connect(&path).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut byte = [0u8; 1];
            stream.read_exact(&mut byte).unwrap();
            held.push(stream);
        }
        let mut extra = std::os::unix::net::UnixStream::connect(&path).unwrap();
        extra
            .set_read_timeout(Some(Duration::from_millis(400)))
            .unwrap();
        let started = Instant::now();
        let mut buf = [0u8; 8];
        let read = std::io::Read::read(&mut extra, &mut buf);
        assert!(
            started.elapsed() < Duration::from_millis(400),
            "cap must not wait for handshake"
        );
        match read {
            Ok(0) => {}
            Err(_) => {}
            Ok(n) => panic!("refused connection still received {n} bytes"),
        }
        drop(held);
        drop(extra);
        // Oversized pre-auth length is refused before the payload is read.
        let mut huge = std::os::unix::net::UnixStream::connect(&path).unwrap();
        let len: u32 = 1 + fl_host_proto::MAX_UNAUTH_PAYLOAD as u32 + 1;
        use std::io::Write;
        huge.write_all(&len.to_be_bytes()).unwrap();
        huge.set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let started = Instant::now();
        let _ = std::io::Read::read(&mut huge, &mut buf);
        assert!(started.elapsed() < Duration::from_millis(200));
        drop(listener);
    }

    #[tokio::test]
    async fn partial_frame_expires() {
        let dir = Tmp::new();
        let mut config = config(dir.path());
        config.frame_timeout = Duration::from_millis(300);
        let mut listener = HostListener::bind(config).await.unwrap();
        let endpoint = listener.endpoint().clone();
        assert_eq!(fl_host_proto::FRAME_READ_TIMEOUT, Duration::from_secs(10));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let mut conn = listener.accept().await.unwrap();
            let _ = started_tx.send(());
            let started = Instant::now();
            let result = conn.recv().await;
            (started.elapsed(), result)
        });
        let client = AsyncHostClient::connect(&endpoint).await.unwrap();
        started_rx.await.unwrap();
        let mut stream = client.into_stream();
        stream.write_all(&[0x11]).await.unwrap();
        let (elapsed, result) = server.await.unwrap();
        assert!(elapsed >= Duration::from_millis(250), "{elapsed:?}");
        assert!(matches!(result, Err(HostError::FrameTimeout)), "{result:?}");
    }

    #[tokio::test]
    async fn idle_connection_has_no_frame_deadline() {
        let dir = Tmp::new();
        let mut config = config(dir.path());
        config.frame_timeout = Duration::from_millis(200);
        let mut listener = HostListener::bind(config).await.unwrap();
        let endpoint = listener.endpoint().clone();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let mut conn = listener.accept().await.unwrap();
            let _ = ready_tx.send(());
            let request = conn.recv().await.unwrap();
            conn.respond(&HostResponse::Listed {
                id: request.request_id(),
                children: vec![],
            })
            .await
            .unwrap();
        });
        let mut client = AsyncHostClient::connect(&endpoint).await.unwrap();
        ready_rx.await.unwrap();
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(700)).await;
        client
            .request(HostRequest::List { id: RequestId(1) })
            .await
            .unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn occupied_non_socket_is_preserved() {
        let dir = Tmp::new();
        let path = dir.path().join("h.sock");
        std::fs::write(&path, b"keep-me").unwrap();
        let err = HostListener::bind(config(dir.path())).await.unwrap_err();
        assert!(matches!(err, HostError::NotASocket), "{err}");
        assert_eq!(std::fs::read(&path).unwrap(), b"keep-me");
    }

    #[tokio::test]
    async fn second_daemon_cannot_replace_endpoint() {
        let dir = Tmp::new();
        let mut listener = HostListener::bind(config(dir.path())).await.unwrap();
        let endpoint = listener.endpoint().clone();
        let token = std::fs::read(endpoint.token_path()).unwrap();
        let err = HostListener::bind(config(dir.path())).await.unwrap_err();
        assert!(
            matches!(err, HostError::EndpointBusy | HostError::AlreadyRunning),
            "{err}"
        );
        assert_eq!(std::fs::read(endpoint.token_path()).unwrap(), token);
        let accept = tokio::spawn(async move { listener.accept().await });
        AsyncHostClient::connect(&endpoint).await.unwrap();
        accept.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn stale_socket_from_dead_daemon_is_replaced() {
        let dir = Tmp::new();
        let path = dir.path().join("h.sock");
        {
            let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        }
        assert!(path.exists());
        let mut listener = HostListener::bind(config(dir.path())).await.unwrap();
        let endpoint = listener.endpoint().clone();
        let accept = tokio::spawn(async move { listener.accept().await });
        AsyncHostClient::connect(&endpoint).await.unwrap();
        accept.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn parent_dir_wrong_mode_or_owner_refused() {
        let dir = Tmp::new();
        let parent = dir.path().join("bad");
        std::fs::create_dir(&parent).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut config = config(dir.path());
        config.endpoint = HostEndpoint::unix(parent.join("h.sock")).unwrap();
        let err = HostListener::bind(config).await.unwrap_err();
        assert!(matches!(err, HostError::ParentDenied), "{err}");
        assert_eq!(mode(&parent), 0o755);
        assert!(!parent.join("h.sock").exists());
        assert!(fl_host_proto::private_parent_ok(1000, 1000, 0o700, true).is_ok());
        assert!(fl_host_proto::private_parent_ok(0, 1000, 0o700, true).is_err());
        assert!(fl_host_proto::private_parent_ok(1000, 1000, 0o755, true).is_err());
        assert!(fl_host_proto::private_parent_ok(1000, 1000, 0o700, false).is_err());
        if unsafe { libc::geteuid() } != 0 {
            let endpoint = HostEndpoint::unix("/usr/flh-proto-not-created.sock").unwrap();
            let mut config = HostConfig::new(endpoint);
            config.daemon_version = "x".into();
            let err = HostListener::bind(config).await.unwrap_err();
            assert!(
                matches!(err, HostError::ParentDenied | HostError::RenameableAncestor),
                "{err}"
            );
            assert!(!Path::new("/usr/flh-proto-not-created.sock").exists());
        }
    }

    #[tokio::test]
    async fn renameable_ancestor_refused_before_bind_or_connect() {
        let dir = Tmp::new();
        let open = dir.path().join("open");
        std::fs::create_dir(&open).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).unwrap();
        let private = open.join("p");
        std::fs::create_dir(&private).unwrap();
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700)).unwrap();
        let sock = private.join("h.sock");
        let mut config = HostConfig::new(HostEndpoint::unix(&sock).unwrap());
        config.daemon_version = "x".into();
        let err = HostListener::bind(config).await.unwrap_err();
        assert!(matches!(err, HostError::RenameableAncestor), "{err}");
        assert!(!sock.exists());
        let _listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let err = AsyncHostClient::connect(&HostEndpoint::unix(&sock).unwrap())
            .await
            .unwrap_err();
        assert!(matches!(err, HostError::RenameableAncestor), "{err}");
    }

    #[tokio::test]
    async fn trusted_sticky_temp_ancestry_allowed() {
        let dir = Tmp::new();
        let listener = HostListener::bind(config(dir.path())).await.unwrap();
        listener.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn pathname_swap_cannot_bypass_peer_and_token_auth() {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::atomic::{AtomicBool, Ordering};
        let root = Tmp::new();
        let a_dir = root.path().join("a");
        let b_dir = root.path().join("b");
        let hidden = root.path().join("a.hidden");
        for dir in [&a_dir, &b_dir] {
            std::fs::create_dir(dir).unwrap();
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let listener_a = HostListener::bind(config(&a_dir)).await.unwrap();
        let listener_b = HostListener::bind(config(&b_dir)).await.unwrap();
        let endpoint_a = listener_a.endpoint().clone();
        AsyncHostClient::connect(&endpoint_a).await.unwrap();
        let did_swap = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&did_swap);
        let from = a_dir.clone();
        let onto_hidden = hidden.clone();
        let other = b_dir.clone();
        let onto = a_dir.clone();
        let options = ConnectOptions {
            pre_connect: Some(Arc::new(move || {
                if flag.swap(true, Ordering::SeqCst) {
                    return;
                }
                // Rename the resolved directory entry before connect(2).
                std::fs::rename(&from, &onto_hidden).unwrap();
                std::fs::rename(&other, &onto).unwrap();
            })),
            ..ConnectOptions::default()
        };
        let err = AsyncHostClient::connect_with(&endpoint_a, options)
            .await
            .unwrap_err();
        assert!(did_swap.load(Ordering::SeqCst));
        assert!(hidden.join("h.sock").exists());
        assert!(a_dir.join("h.sock").exists());
        assert!(matches!(err, HostError::Auth), "{err}");
        // Directory-fd pinning does not cover this rename. Peer uid and the
        // token proof fail closed. The race is not claimed to be impossible.
        assert!(validate_unix_peer(PeerQuery::Uid(0), 1000).is_err());
        drop(listener_a);
        drop(listener_b);
    }

    #[tokio::test]
    async fn symlink_or_foreign_endpoint_state_refused() {
        let dir = Tmp::new();
        let sock = dir.path().join("h.sock");
        let token = PathBuf::from(format!("{}.token", sock.display()));
        std::os::unix::fs::symlink("elsewhere", &token).unwrap();
        std::fs::write(dir.path().join("elsewhere"), b"secret").unwrap();
        let err = HostListener::bind(config(dir.path())).await.unwrap_err();
        assert!(matches!(err, HostError::EndpointStateDenied), "{err}");
        assert!(token.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(
            std::fs::read(dir.path().join("elsewhere")).unwrap(),
            b"secret"
        );
        std::fs::remove_file(&token).unwrap();
        std::fs::write(&token, [0u8; 32]).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = HostListener::bind(config(dir.path())).await.unwrap_err();
        assert!(matches!(err, HostError::EndpointStateDenied), "{err}");
        assert_eq!(mode(&token), 0o644);
    }

    #[tokio::test]
    async fn shutdown_preserves_replaced_token() {
        let dir = Tmp::new();
        let listener = HostListener::bind(config(dir.path())).await.unwrap();
        let endpoint = listener.endpoint().clone();
        let replacement = [0x3cu8; 32];
        std::fs::write(endpoint.token_path(), replacement).unwrap();
        let socket = endpoint.socket_path().to_path_buf();
        listener.shutdown().await.unwrap();
        assert_eq!(std::fs::read(endpoint.token_path()).unwrap(), replacement);
        assert!(!socket.exists());

        let listener = HostListener::bind(config(dir.path())).await.unwrap();
        let endpoint = listener.endpoint().clone();
        let token = endpoint.token_path();
        listener.shutdown().await.unwrap();
        assert!(!token.exists());
    }

    #[tokio::test]
    async fn replayed_hello_is_refused_on_a_new_challenge() {
        let token = AuthToken::new([6u8; 32]);
        let identity = b"/tmp/flh-replay.sock";
        let capture = Arc::new(Mutex::new(Vec::new()));
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let params = HandshakeParams {
            identity,
            token: &token,
            deadlines: deadlines(),
            daemon_version: "v",
            major: PROTO_MAJOR,
            minor: PROTO_MINOR,
            capture_writes: Some(Arc::clone(&capture)),
        };
        let server = server_handshake(server_io, params);
        let client = fl_host_proto::client_handshake(
            client_io,
            identity,
            &token,
            PROTO_MAJOR,
            PROTO_MINOR,
            deadlines(),
            Some(Arc::clone(&capture)),
        );
        let (server, client) = tokio::join!(server, client);
        server.unwrap();
        client.unwrap();
        let recorded = capture.lock().unwrap().clone();
        let (left, right) = tokio::io::duplex(64 * 1024);
        let params = HandshakeParams {
            identity,
            token: &token,
            deadlines: deadlines(),
            daemon_version: "v",
            major: PROTO_MAJOR,
            minor: PROTO_MINOR,
            capture_writes: None,
        };
        let server = server_handshake(right, params);
        let replay = async move {
            let mut left = left;
            let _challenge =
                read_frame_async(&mut left, Instant::now() + Duration::from_secs(2), true)
                    .await
                    .unwrap();
            use tokio::io::AsyncWriteExt;
            left.write_all(&recorded).await.unwrap();
            Ok::<_, HostError>(())
        };
        let (server, replay) = tokio::join!(server, replay);
        assert!(matches!(
            server,
            Err(HostError::Auth | HostError::Protocol(_))
        ));
        let _ = replay;
        let _ = FrameKind::Hello;
        let _ = decode_challenge;
    }
}
