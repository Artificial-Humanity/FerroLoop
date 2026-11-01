//! Spec §6.2: every served connection authenticates before any operation,
//! and every method needs a scope.
//!
//! Unix: `SO_PEERCRED` (the peer's uid is the server's), and a PSK when the
//! bind configures one. TCP (`ws://`): the mutual PSK challenge-response, or
//! a bearer token in the upgrade's `Authorization` header.

use super::auth::{self, Psk, Scope};
use super::server::{self, read_frame, write_frame};
use super::ws;
use super::{ConnectOptions, FerroStore, FerroStoreServer, Inner, ServerAuth};
use fl_core::StoreError;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::net::{SocketAddr, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

/// The PSK every test server and client shares: 32 bytes.
pub(super) const TEST_PSK: &[u8] = b"0123456789abcdef0123456789abcdef";
/// The name test servers prove themselves under.
pub(super) const TEST_SERVER_ID: &str = "test-store";

/// The server auth of a test TCP bind: the test PSK.
pub(super) fn psk_auth() -> ServerAuth {
    ServerAuth::new()
        .with_psk(TEST_SERVER_ID, TEST_PSK)
        .unwrap()
}

/// The client options that match [`psk_auth`].
pub(super) fn psk_options() -> ConnectOptions {
    ConnectOptions::psk(TEST_SERVER_ID, TEST_PSK)
}

/// A client of the `ws://` server at `addr` under the test PSK.
pub(super) fn ws_client(addr: SocketAddr) -> FerroStore {
    FerroStore::connect_with(&format!("ws://{addr}"), &psk_options()).unwrap()
}

fn remote(store: &FerroStore) -> &server::RemoteClient {
    match &store.inner {
        Inner::Remote(remote) => remote,
        _ => panic!("not a remote handle"),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unhex32(text: &str) -> [u8; 32] {
    assert_eq!(text.len(), 64, "{text}");
    let mut out = [0u8; 32];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&text[2 * i..2 * i + 2], 16).unwrap();
    }
    out
}

/// A raw `ws://` connection the test drives frame by frame.
struct RawWs {
    stream: TcpStream,
}

impl RawWs {
    fn connect(addr: SocketAddr, bearer: Option<&str>) -> Self {
        let stream = ws::connect(&addr.to_string(), bearer).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        Self { stream }
    }

    fn send(&mut self, value: &Value) {
        let bytes = serde_json::to_vec(value).unwrap();
        ws::write_frame(&mut self.stream, true, 0x1, &bytes).unwrap();
    }

    /// The next JSON frame; `None` once the server closed.
    fn recv(&mut self) -> Option<Value> {
        match ws::read_event(&mut self.stream, true) {
            Ok(ws::WsEvent::Data(payload)) => Some(serde_json::from_slice(&payload).unwrap()),
            Ok(ws::WsEvent::Ping(_)) => self.recv(),
            Ok(ws::WsEvent::Close) | Err(_) => None,
        }
    }

    /// The server's challenge nonce.
    fn challenge(&mut self) -> [u8; 32] {
        let frame = self.recv().expect("a challenge");
        assert_eq!(frame["auth"], "challenge", "{frame}");
        assert_eq!(frame["v"], 1, "{frame}");
        unhex32(frame["ns"].as_str().unwrap())
    }

    /// Send a PSK hello; the server's answer, `None` when it closed.
    fn hello(&mut self, nc: &[u8; 32], proof: &[u8]) -> Option<Value> {
        self.send(&json!({"auth": "psk", "nc": hex(nc), "proof": hex(proof)}));
        self.recv()
    }

    /// After a refusal: the failure frame, then the close.
    fn refused(&mut self, first: Option<Value>) {
        assert_eq!(
            first,
            Some(json!({"auth": "failed", "error": "authentication failed"}))
        );
        assert_eq!(self.recv(), None, "the server closes after refusing");
    }
}

/// A served test store over TCP under `auth`.
fn serve_tcp(store: FerroStore, auth: ServerAuth) -> FerroStoreServer {
    FerroStoreServer::bind_tcp("127.0.0.1:0", store, auth).unwrap()
}

/// A 0600 token file in `dir` holding `lines`.
fn token_file(dir: &Path, lines: &str) -> PathBuf {
    let path = dir.join("tokens");
    std::fs::write(&path, lines).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    path
}

fn sha256_hex(token: &str) -> String {
    hex(&Sha256::digest(token.as_bytes()))
}

const READ_TOKEN: &str = "read-token-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const WRITE_TOKEN: &str = "write-token-BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";
const ADMIN_TOKEN: &str = "admin-token-CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC";

fn three_tokens(dir: &Path) -> PathBuf {
    token_file(
        dir,
        &format!(
            "# sha256 scope label\n{} read reader\n\n{} write writer bot\n{} admin ops\n",
            sha256_hex(READ_TOKEN),
            sha256_hex(WRITE_TOKEN),
            sha256_hex(ADMIN_TOKEN)
        ),
    )
}

fn token_client(addr: SocketAddr, token: &str) -> Result<FerroStore, StoreError> {
    FerroStore::connect_with(&format!("ws://{addr}"), &ConnectOptions::token(token))
}

fn needs(err: &StoreError, method: &str, scope: &str) {
    let message = err.to_string();
    assert!(
        message.contains(&format!("`{method}` needs the `{scope}` scope")),
        "{message}"
    );
}

// ---- Unix: SO_PEERCRED ---------------------------------------------------------

#[test]
fn the_peer_uid_decision_accepts_the_server_uid_alone() {
    assert!(auth::peer_allowed(Some(1000), 1000));
    assert!(!auth::peer_allowed(Some(1001), 1000));
    assert!(!auth::peer_allowed(Some(0), 1000), "root is another uid");
    assert!(
        !auth::peer_allowed(None, 1000),
        "unknown credentials refuse"
    );

    // A real connection reports this process's uid.
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("p.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    let client = UnixStream::connect(&sock).unwrap();
    let (accepted, _) = listener.accept().unwrap();
    // SAFETY: plain syscall without arguments.
    let own = unsafe { libc::geteuid() };
    assert_eq!(auth::peer_uid(&accepted).unwrap(), own);
    drop(client);
}

#[test]
fn a_unix_peer_of_the_same_uid_is_served_and_another_uid_is_refused_unread() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("s.sock");
    let server = FerroStoreServer::bind_unix(&sock, FerroStore::in_memory()).unwrap();
    let uri = format!("unix://{}", sock.display());

    let same = FerroStore::connect_uri(&uri).unwrap();
    same.put_doc("docs", "a", &json!(1)).unwrap();
    assert_eq!(same.get_doc("docs", "a").unwrap(), Some(json!(1)));
    drop(same);

    // The server now sees every peer as another uid.
    // SAFETY: plain syscall without arguments.
    let other = unsafe { libc::geteuid() }.wrapping_add(1);
    server.set_peer_uid(Some(other));
    let before = server.dispatched();
    let mut raw = UnixStream::connect(&sock).unwrap();
    raw.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    // A request written at once is never read.
    write_frame(
        &mut raw,
        &json!({"id": 1, "method": "put_doc", "params": {"collection": "docs", "id": "b", "doc": 2}}),
    )
    .unwrap();
    let (frame, _) = read_frame(&mut raw).unwrap();
    assert_eq!(
        frame,
        json!({"auth": "failed", "error": "authentication failed"})
    );
    assert!(read_frame(&mut raw).is_err(), "closed after the refusal");
    assert_eq!(server.dispatched(), before, "nothing ran");
    assert_eq!(server.store().get_doc("docs", "b").unwrap(), None);
    assert!(
        auth::warnings()
            .iter()
            .any(|warning| warning.contains(&format!("uid {other}"))),
        "{:?}",
        auth::warnings()
    );

    let err = FerroStore::connect_uri(&uri).err().unwrap();
    assert!(err.to_string().contains(auth::AUTH_FAILED), "{err}");
    server.set_peer_uid(None);
    FerroStore::connect_uri(&uri).unwrap();
}

#[test]
fn a_unix_bind_with_a_psk_requires_it_as_well_as_the_uid() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("s.sock");
    let _server =
        FerroStoreServer::bind_unix_with(&sock, FerroStore::in_memory(), psk_auth()).unwrap();
    let uri = format!("unix://{}", sock.display());

    let err = FerroStore::connect_uri(&uri).err().unwrap();
    assert!(err.to_string().contains("requires authentication"), "{err}");
    let wrong = ConnectOptions::psk(
        TEST_SERVER_ID,
        b"another-32-byte-pre-shared-key!!".as_slice(),
    );
    let err = FerroStore::connect_with(&uri, &wrong).err().unwrap();
    assert!(err.to_string().contains(auth::AUTH_FAILED), "{err}");

    let client = FerroStore::connect_with(&uri, &psk_options()).unwrap();
    client.put_doc("docs", "a", &json!(1)).unwrap();
    assert_eq!(client.get_doc("docs", "a").unwrap(), Some(json!(1)));

    // A token file has no meaning on a Unix socket.
    let tokens = three_tokens(dir.path());
    let err = FerroStoreServer::bind_unix_with(
        dir.path().join("t.sock"),
        FerroStore::in_memory(),
        ServerAuth::new().with_token_file(&tokens).unwrap(),
    )
    .err()
    .unwrap();
    assert!(err.to_string().contains("ws://"), "{err}");
}

#[test]
fn a_psk_client_refuses_a_unix_server_that_proves_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("s.sock");
    let _server = FerroStoreServer::bind_unix(&sock, FerroStore::in_memory()).unwrap();
    let err = FerroStore::connect_with(&format!("unix://{}", sock.display()), &psk_options())
        .err()
        .unwrap();
    assert!(err.to_string().contains("did not prove"), "{err}");
}

// ---- PSK challenge-response ------------------------------------------------------

#[test]
fn the_psk_handshake_serves_a_client_with_the_psk() {
    let server = serve_tcp(FerroStore::in_memory(), psk_auth());
    let client = ws_client(server.local_addr());
    client.put_doc("docs", "a", &json!({"n": 1})).unwrap();
    assert_eq!(client.get_doc("docs", "a").unwrap(), Some(json!({"n": 1})));
    // PSK connections hold the admin scope.
    remote(&client).call("checkpoint", json!({})).unwrap();
    remote(&client).call("compact", json!({})).unwrap();
}

#[test]
fn the_server_refuses_a_wrong_psk_and_a_wrong_server_id() {
    let server = serve_tcp(FerroStore::in_memory(), psk_auth());
    let url = format!("ws://{}", server.local_addr());
    let before = server.dispatched();
    for options in [
        ConnectOptions::psk(
            TEST_SERVER_ID,
            b"another-32-byte-pre-shared-key!!".as_slice(),
        ),
        ConnectOptions::psk("some-other-store", TEST_PSK),
        ConnectOptions::new(),
    ] {
        let err = FerroStore::connect_with(&url, &options).err().unwrap();
        let message = err.to_string();
        assert!(
            message.contains(auth::AUTH_FAILED) || message.contains("requires authentication"),
            "{message}"
        );
    }
    assert_eq!(server.dispatched(), before);
    // A PSK shorter than 32 bytes is refused before any connection.
    assert!(
        ServerAuth::new()
            .with_psk(TEST_SERVER_ID, b"short")
            .is_err()
    );
    assert!(ServerAuth::new().with_psk("", TEST_PSK).is_err());
}

/// How a fake server answers a client's hello, given the nonce it issued.
type Answer = dyn Fn(&Value, &[u8; 32]) -> Value + Send;

/// A fake Unix server that answers the challenge-response with `answer`,
/// given the client's hello and the nonce it issued.
fn fake_server(
    answer: impl Fn(&Value, &[u8; 32]) -> Value + Send + 'static,
) -> (String, thread::JoinHandle<()>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("fake.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let ns = [7u8; 32];
        write_frame(
            &mut stream,
            &json!({"auth": "challenge", "v": 1, "ns": hex(&ns)}),
        )
        .unwrap();
        let (hello, _) = read_frame(&mut stream).unwrap();
        write_frame(&mut stream, &answer(&hello, &ns)).unwrap();
        // Anything the client sends after a refusal would be read here.
        let _ = read_frame(&mut stream);
    });
    (format!("unix://{}", sock.display()), handle, dir)
}

#[test]
fn the_client_refuses_a_server_that_does_not_hold_the_psk() {
    let wrong = Psk::new(TEST_SERVER_ID, b"another-32-byte-pre-shared-key!!").unwrap();
    let answers: Vec<Box<Answer>> = vec![
        // Proof under another PSK.
        Box::new(move |hello, ns| {
            let nc = unhex32(hello["nc"].as_str().unwrap());
            json!({"auth": "ok", "scope": "admin", "proof": hex(&wrong.server_proof(&nc, ns))})
        }),
        // No proof at all.
        Box::new(|_, _| json!({"auth": "ok", "scope": "admin"})),
        // The client's own proof, reflected.
        Box::new(|hello, _| json!({"auth": "ok", "scope": "admin", "proof": hello["proof"]})),
    ];
    for answer in answers {
        let (uri, handle, _dir) = fake_server(answer);
        let err = FerroStore::connect_with(&uri, &psk_options())
            .err()
            .unwrap();
        assert!(
            err.to_string().contains("did not prove"),
            "the client accepted a server without the PSK: {err}"
        );
        handle.join().unwrap();
    }
    // The control: the right proof is accepted by the same fake server.
    let right = Psk::new(TEST_SERVER_ID, TEST_PSK).unwrap();
    let (uri, _handle, _dir) = fake_server(move |hello, ns| {
        let nc = unhex32(hello["nc"].as_str().unwrap());
        json!({"auth": "ok", "scope": "admin", "proof": hex(&right.server_proof(&nc, ns))})
    });
    FerroStore::connect_with(&uri, &psk_options()).unwrap();
}

/// The FerroWire W-1 lesson: a proof made in one direction never passes in
/// the other, and no proof outlives the connection whose nonce it covers.
#[test]
fn psk_handshake_is_not_open_to_a_reflection_or_a_replay() {
    let server = serve_tcp(FerroStore::in_memory(), psk_auth());
    let addr = server.local_addr();
    let psk = Psk::new(TEST_SERVER_ID, TEST_PSK).unwrap();
    let before = server.dispatched();

    // An honest connection, recorded.
    let mut honest = RawWs::connect(addr, None);
    let ns_a = honest.challenge();
    let nc_a = [0x11u8; 32];
    let client_a = psk.client_proof(&ns_a, &nc_a);
    let ok = honest.hello(&nc_a, &client_a).expect("an answer");
    assert_eq!(ok["auth"], "ok", "{ok}");
    assert_eq!(ok["scope"], "admin", "{ok}");
    let server_a = unhex32(ok["proof"].as_str().unwrap());
    assert!(psk.verify_server(&nc_a, &ns_a, &server_a));

    // Direction separation: the server's proof is no client proof for the
    // transposed transcript (a server that had issued `nc_a`), and the
    // client's proof is no server proof for it.
    assert!(!psk.verify_client(&nc_a, &ns_a, &server_a));
    assert!(!psk.verify_server(&ns_a, &nc_a, &client_a));

    // The server's proof, presented as a client proof over its own nonce.
    let mut reflect = RawWs::connect(addr, None);
    let ns_b = reflect.challenge();
    let answer = reflect.hello(&ns_a, &server_a);
    reflect.refused(answer);

    // The server's challenge echoed back as the client nonce.
    let mut echo = RawWs::connect(addr, None);
    let ns_c = echo.challenge();
    let answer = echo.hello(&ns_c, &psk.client_proof(&ns_c, &ns_c));
    echo.refused(answer);

    // A captured client hello replayed on a new connection: a fresh `ns`.
    let mut replay = RawWs::connect(addr, None);
    let ns_d = replay.challenge();
    assert_ne!(ns_d, ns_a, "every connection gets a fresh nonce");
    assert_ne!(ns_d, ns_b);
    let answer = replay.hello(&nc_a, &client_a);
    replay.refused(answer);

    assert_eq!(server.dispatched(), before, "nothing ran");
}

// ---- bearer tokens and scopes --------------------------------------------------

#[test]
fn a_bearer_token_gets_its_scope_and_no_more() {
    let dir = tempfile::tempdir().unwrap();
    let tokens = three_tokens(dir.path());
    let file = dir.path().join("s.fls");
    let server = serve_tcp(
        FerroStore::open(&file).unwrap(),
        ServerAuth::new().with_token_file(&tokens).unwrap(),
    );
    let addr = server.local_addr();

    let admin = token_client(addr, ADMIN_TOKEN).unwrap();
    admin.put_doc("docs", "a", &json!(1)).unwrap();

    // read: reads, but no commit (not even an empty one), put or checkpoint.
    let reader = token_client(addr, READ_TOKEN).unwrap();
    assert_eq!(reader.get_doc("docs", "a").unwrap(), Some(json!(1)));
    let empty_commit = json!({"reads": [], "ops": []});
    let err = remote(&reader)
        .call("commit_txn", empty_commit.clone())
        .unwrap_err();
    needs(&err, "commit_txn", "write");
    let err = reader.put_doc("docs", "b", &json!(2)).unwrap_err();
    needs(&err, "commit_txn", "write");
    let err = remote(&reader)
        .call(
            "put_doc",
            json!({"collection": "docs", "id": "b", "doc": 2}),
        )
        .unwrap_err();
    needs(&err, "put_doc", "write");
    let err = remote(&reader).call("checkpoint", json!({})).unwrap_err();
    needs(&err, "checkpoint", "admin");
    // The connection stays open after a refusal.
    assert_eq!(reader.get_doc("docs", "b").unwrap(), None);
    let reads_mq = reader.subscribe_mq("t");
    drop(reads_mq);

    // write: writes, but no compact, checkpoint or sync apply.
    let writer = token_client(addr, WRITE_TOKEN).unwrap();
    let committed = remote(&writer).call("commit_txn", empty_commit).unwrap();
    assert_eq!(committed["committed"], true, "{committed}");
    writer.put_doc("docs", "b", &json!(2)).unwrap();
    writer.publish_mq("t", "k", &json!({})).unwrap();
    let err = remote(&writer).call("compact", json!({})).unwrap_err();
    needs(&err, "compact", "admin");
    let err = remote(&writer).call("checkpoint", json!({})).unwrap_err();
    needs(&err, "checkpoint", "admin");
    let err = remote(&writer)
        .call("apply_sync_batches", json!({"batches": []}))
        .unwrap_err();
    needs(&err, "apply_sync_batches", "admin");
    assert_eq!(writer.get_doc("docs", "b").unwrap(), Some(json!(2)));

    // admin: all of it.
    remote(&admin).call("checkpoint", json!({})).unwrap();
    remote(&admin).call("compact", json!({})).unwrap();
    assert_eq!(admin.get_doc("docs", "b").unwrap(), Some(json!(2)));

    // An unknown token is refused at connect.
    let err = token_client(addr, "no-such-token").err().unwrap();
    assert!(err.to_string().contains(auth::AUTH_FAILED), "{err}");
    // A PSK client of a token-only server is refused too.
    let err = FerroStore::connect_with(&format!("ws://{addr}"), &psk_options())
        .err()
        .unwrap();
    assert!(err.to_string().contains(auth::AUTH_FAILED), "{err}");
}

#[test]
fn a_secret_file_others_can_read_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let tokens = three_tokens(dir.path());
    for mode in [0o644, 0o640, 0o604, 0o660] {
        std::fs::set_permissions(&tokens, std::fs::Permissions::from_mode(mode)).unwrap();
        let err = ServerAuth::new().with_token_file(&tokens).unwrap_err();
        assert!(err.to_string().contains("chmod 600"), "{mode:o}: {err}");
    }
    std::fs::set_permissions(&tokens, std::fs::Permissions::from_mode(0o600)).unwrap();
    ServerAuth::new().with_token_file(&tokens).unwrap();

    let psk = dir.path().join("psk");
    std::fs::write(&psk, [TEST_PSK, b"\n"].concat()).unwrap();
    std::fs::set_permissions(&psk, std::fs::Permissions::from_mode(0o644)).unwrap();
    let err = ServerAuth::new()
        .with_psk_file(TEST_SERVER_ID, &psk)
        .unwrap_err();
    assert!(err.to_string().contains("chmod 600"), "{err}");
    std::fs::set_permissions(&psk, std::fs::Permissions::from_mode(0o600)).unwrap();
    let server = serve_tcp(
        FerroStore::in_memory(),
        ServerAuth::new()
            .with_psk_file(TEST_SERVER_ID, &psk)
            .unwrap(),
    );
    // The client reads the same file, under the same rule.
    let url = format!("ws://{}", server.local_addr());
    FerroStore::connect_with(&url, &ConnectOptions::psk_file(TEST_SERVER_ID, &psk)).unwrap();
    std::fs::set_permissions(&psk, std::fs::Permissions::from_mode(0o640)).unwrap();
    let err = FerroStore::connect_with(&url, &ConnectOptions::psk_file(TEST_SERVER_ID, &psk))
        .err()
        .unwrap();
    assert!(err.to_string().contains("chmod 600"), "{err}");
}

#[test]
fn a_token_file_with_a_bad_line_is_refused_by_line_number() {
    let dir = tempfile::tempdir().unwrap();
    for (text, line) in [
        (format!("{} superuser x\n", sha256_hex("a")), 1),
        (
            format!("# c\n{} read\n{} read\n", sha256_hex("a"), sha256_hex("a")),
            3,
        ),
        ("abc read\n".to_string(), 1),
        (format!("\n{}\n", sha256_hex("a")), 2),
        (format!("{} read\n", sha256_hex("a").to_uppercase()), 1),
    ] {
        let path = token_file(dir.path(), &text);
        let err = ServerAuth::new().with_token_file(&path).unwrap_err();
        assert!(
            err.to_string().contains(&format!("line {line}")),
            "{text:?}: {err}"
        );
    }
    let empty = token_file(dir.path(), "# nothing\n");
    assert!(
        ServerAuth::new().with_token_file(&empty).is_err(),
        "no tokens"
    );
}

#[test]
fn the_uri_names_secret_files_and_never_a_secret() {
    let dir = tempfile::tempdir().unwrap();
    let tokens = three_tokens(dir.path());
    let token = dir.path().join("my-token");
    std::fs::write(&token, format!("{WRITE_TOKEN}\n")).unwrap();
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
    let psk = dir.path().join("psk");
    std::fs::write(&psk, TEST_PSK).unwrap();
    std::fs::set_permissions(&psk, std::fs::Permissions::from_mode(0o600)).unwrap();

    let by_token = serve_tcp(
        FerroStore::in_memory(),
        ServerAuth::new().with_token_file(&tokens).unwrap(),
    );
    let by_psk = serve_tcp(FerroStore::in_memory(), psk_auth());

    let client = FerroStore::connect_uri(&format!(
        "ws://{}/store?token_file={}",
        by_token.local_addr(),
        token.display()
    ))
    .unwrap();
    client.put_doc("docs", "a", &json!(1)).unwrap();
    let client = FerroStore::connect_uri(&format!(
        "ws://{}?psk_file={}&server_id={TEST_SERVER_ID}",
        by_psk.local_addr(),
        psk.display()
    ))
    .unwrap();
    client.put_doc("docs", "a", &json!(1)).unwrap();

    for query in [
        format!("token={WRITE_TOKEN}"),
        "psk=0123456789abcdef0123456789abcdef&server_id=x".to_string(),
    ] {
        let err = FerroStore::connect_uri(&format!("ws://{}?{query}", by_psk.local_addr()))
            .err()
            .unwrap();
        let message = err.to_string();
        assert!(message.contains("never"), "{message}");
        assert!(
            !message.contains(WRITE_TOKEN),
            "the error repeats the secret"
        );
    }
    for (query, needle) in [
        (format!("psk_file={}", psk.display()), "server_id"),
        ("colour=blue".to_string(), "colour"),
        (
            format!(
                "token_file={}&psk_file={}&server_id=x",
                token.display(),
                psk.display()
            ),
            "one",
        ),
    ] {
        let err = FerroStore::connect_uri(&format!("ws://{}?{query}", by_psk.local_addr()))
            .err()
            .unwrap();
        assert!(err.to_string().contains(needle), "{query}: {err}");
    }
    // Options and a URI that both name a credential are refused.
    let err = FerroStore::connect_with(
        &format!(
            "ws://{}?token_file={}",
            by_token.local_addr(),
            token.display()
        ),
        &psk_options(),
    )
    .err()
    .unwrap();
    assert!(err.to_string().contains("twice"), "{err}");
    // An in-process store takes no credential.
    let err = FerroStore::connect_with("mem://", &psk_options())
        .err()
        .unwrap();
    assert!(err.to_string().contains("unix:// and ws://"), "{err}");
    // A bearer token is for ws:// alone.
    let err = FerroStore::connect_with(
        &format!("unix://{}", dir.path().join("none.sock").display()),
        &ConnectOptions::token(WRITE_TOKEN),
    )
    .err()
    .unwrap();
    assert!(err.to_string().contains("ws://"), "{err}");
    // A token that would break the header is refused before it is sent.
    let err = token_client(by_token.local_addr(), "bad\r\nX-Injected: 1")
        .err()
        .unwrap();
    assert!(err.to_string().contains("token"), "{err}");
    assert!(!format!("{:?}", ConnectOptions::token(WRITE_TOKEN)).contains(WRITE_TOKEN));
    assert!(!format!("{:?}", psk_options()).contains("0123456789abcdef"));
}

// ---- the single table of scopes -----------------------------------------------

#[test]
fn every_method_the_server_dispatches_is_classified_once() {
    let source = include_str!("server.rs");
    let start = source.find("\nfn dispatch(").expect("fn dispatch");
    let body = &source[start..];
    let end = body.find("\n        _ => ").expect("the dispatch fallback");
    let mut dispatched = BTreeSet::new();
    for line in body[..end].lines() {
        if let Some(rest) = line.strip_prefix("        \"")
            && let Some((name, tail)) = rest.split_once('"')
            && tail.starts_with(" =>")
        {
            dispatched.insert(name);
        }
    }
    // `subscribe_mq` is answered by the connection loop, not by `dispatch`.
    assert!(source.contains("rule.name == \"subscribe_mq\""));
    dispatched.insert("subscribe_mq");
    assert!(
        dispatched.len() >= 29,
        "only {} methods found",
        dispatched.len()
    );

    let classified: BTreeSet<&str> = auth::METHODS.iter().map(|rule| rule.name).collect();
    assert_eq!(
        classified.len(),
        auth::METHODS.len(),
        "a method is classified twice"
    );
    assert_eq!(dispatched, classified);

    // The spec's fixed points.
    let scope = |name: &str| auth::method_rule(name).unwrap().scope;
    assert_eq!(scope("commit_txn"), Scope::Write);
    assert_eq!(scope("open_session"), Scope::Read);
    assert_eq!(scope("txn_read"), Scope::Read);
    assert_eq!(scope("checkpoint"), Scope::Admin);
    assert_eq!(scope("compact"), Scope::Admin);
    assert_eq!(scope("apply_sync_batches"), Scope::Admin);
    // Every method that keeps an exactly-once reply writes.
    for rule in auth::METHODS {
        assert!(!rule.writes || rule.scope >= Scope::Write, "{}", rule.name);
    }
    assert!(Scope::Admin.covers(Scope::Write) && Scope::Write.covers(Scope::Read));
    assert!(!Scope::Read.covers(Scope::Write) && !Scope::Write.covers(Scope::Admin));
}

// ---- nothing before authentication ---------------------------------------------

#[test]
fn no_request_runs_before_the_connection_authenticates() {
    let server = serve_tcp(FerroStore::in_memory(), psk_auth());
    let addr = server.local_addr();
    let before = server.dispatched();
    for request in [
        json!({"id": 1, "method": "open_session", "params": {}}),
        json!({"id": 1, "method": "get_doc", "params": {"collection": "docs", "id": "a"}}),
        json!({"id": 1, "method": "put_doc", "params": {"collection": "docs", "id": "a", "doc": 1}}),
    ] {
        // The request as the very first frame, then as the answer to the
        // challenge. The counter is checked before the frames: a server
        // that ran the request has already counted it when it replies.
        for read_challenge_first in [false, true] {
            let mut raw = RawWs::connect(addr, None);
            if read_challenge_first {
                raw.challenge();
            }
            raw.send(&request);
            let mut frames = Vec::new();
            while let Some(frame) = raw.recv() {
                frames.push(frame);
            }
            assert_eq!(
                server.dispatched(),
                before,
                "{request} ran before authentication: {frames:?}"
            );
            let refusal = json!({"auth": "failed", "error": "authentication failed"});
            if read_challenge_first {
                assert_eq!(frames, vec![refusal], "{request}");
            } else {
                assert_eq!(frames.len(), 2, "{frames:?}");
                assert_eq!(frames[0]["auth"], "challenge", "{frames:?}");
                assert_eq!(frames[1], refusal, "{request}");
            }
        }
    }
    assert_eq!(server.open_sessions(), 0);
    assert_eq!(server.store().get_doc("docs", "a").unwrap(), None);
}

#[test]
fn a_failed_authentication_closes_after_the_fixed_delay_and_names_no_check() {
    let dir = tempfile::tempdir().unwrap();
    let tokens = three_tokens(dir.path());
    let server = serve_tcp(
        FerroStore::in_memory(),
        psk_auth().with_token_file(&tokens).unwrap(),
    );
    let addr = server.local_addr();
    let url = format!("ws://{addr}");
    let mut messages = BTreeSet::new();

    let started = Instant::now();
    let wrong = ConnectOptions::psk(
        TEST_SERVER_ID,
        b"another-32-byte-pre-shared-key!!".as_slice(),
    );
    messages.insert(
        FerroStore::connect_with(&url, &wrong)
            .err()
            .unwrap()
            .to_string(),
    );
    assert!(started.elapsed() >= auth::AUTH_FAIL_DELAY, "wrong PSK");

    let started = Instant::now();
    messages.insert(token_client(addr, "unknown").err().unwrap().to_string());
    assert!(started.elapsed() >= auth::AUTH_FAIL_DELAY, "unknown token");

    let mut raw = RawWs::connect(addr, None);
    raw.challenge();
    let started = Instant::now();
    raw.send(&json!({"auth": "psk", "nc": "zz"}));
    let answer = raw.recv();
    assert!(
        started.elapsed() >= auth::AUTH_FAIL_DELAY,
        "malformed hello"
    );
    raw.refused(answer);

    assert_eq!(messages.len(), 1, "the errors differ: {messages:?}");
    assert!(auth::AUTH_FAIL_DELAY >= Duration::from_millis(200));
}

#[test]
fn bind_tcp_without_authentication_is_an_error() {
    let err = FerroStoreServer::bind_tcp("127.0.0.1:0", FerroStore::in_memory(), ServerAuth::new())
        .err()
        .unwrap();
    assert!(err.to_string().contains("authentication"), "{err}");
}
