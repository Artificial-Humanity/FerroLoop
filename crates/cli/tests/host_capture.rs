//! Opt-in capture through an isolated real `fl host serve`; no live daemon access.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use fl_host_proto::{BlockingHostClient, HostEndpoint, SessionId, SpawnKind, SpawnSpec, StreamId};

struct Fixture {
    root: tempfile::TempDir,
    daemon: Child,
    endpoint: HostEndpoint,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let endpoint = HostEndpoint::unix(root.path().join("host.sock")).unwrap();
        let daemon = Command::new(env!("CARGO_BIN_EXE_fl"))
            .args(["host", "serve", "--socket"])
            .arg(endpoint.socket_path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut fixture = Self {
            root,
            daemon,
            endpoint,
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if BlockingHostClient::connect(&fixture.endpoint).is_ok() {
                return fixture;
            }
            assert!(
                fixture.daemon.try_wait().unwrap().is_none(),
                "daemon exited"
            );
            assert!(Instant::now() < deadline, "daemon startup deadline");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn rpc(&self) -> BlockingHostClient {
        BlockingHostClient::connect(&self.endpoint).unwrap()
    }
    fn capture(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_fl"))
            .args(["host", "capture", "capture-test"])
            .args(args)
            .env("FL_HOST_SOCK", self.endpoint.socket_path())
            .output()
            .unwrap()
    }
    fn spawn(&self, pty: bool) -> StreamId {
        let mut rpc = self.rpc();
        // A finite producer parks on an input gate, preserving exact output for all captures.
        let spec = SpawnSpec {
            session: SessionId::new("capture-test").unwrap(),
            kind: if pty { SpawnKind::Pty } else { SpawnKind::Pipe },
            argv: vec![
                "/bin/sh".into(),
                "-c".into(),
                "printf '\\033[2J\\033[Hold\\r\\033[32mclean\\033[0m'; read line".into(),
            ],
            cwd: self.root.path().to_owned(),
            env: Default::default(),
            env_allow: Default::default(),
        };
        let (stream, _) = if pty {
            rpc.spawn_pty_session(spec, None)
        } else {
            rpc.spawn_reliable_session(spec)
        }
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if rpc
                .capture_output(stream.session.clone(), 65536)
                .unwrap()
                .ends_with("clean\x1b[0m")
            {
                break;
            }
            assert!(Instant::now() < deadline, "child output deadline");
            std::thread::sleep(Duration::from_millis(5));
        }
        stream
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        // Kill every contained session before stopping the daemon, including on assertion failure.
        if let Ok(mut rpc) = BlockingHostClient::connect(&self.endpoint)
            && let Ok(sessions) = rpc.list_sessions()
        {
            for session in sessions {
                if let Ok(grant) = rpc.claim_controller(session.stream.clone()) {
                    let _ = rpc.kill_session(session.stream, grant.lease);
                }
            }
        }
        // Let the daemon finish contained-child reaping before its fallback kill.
        unsafe {
            libc::kill(self.daemon.id() as libc::pid_t, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while self.daemon.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}
fn success(output: Output) -> Vec<u8> {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

// Catches accidentally switching the default to a screen or applying byte truncation differently.
#[test]
fn default_and_explicit_raw_preserve_bytes_and_limits() {
    let f = Fixture::new();
    let stream = f.spawn(true);
    let expected = f
        .rpc()
        .capture_output(stream.session, 16384)
        .unwrap()
        .into_bytes();
    assert_eq!(success(f.capture(&[])), expected);
    assert_eq!(success(f.capture(&["--raw"])), expected);
    assert_eq!(success(f.capture(&["--max-bytes", "6"])), b"an\x1b[0m");
    assert_eq!(
        success(f.capture(&["--raw", "--max-bytes", "6"])),
        b"an\x1b[0m"
    );
}

// Catches ANSI leakage, omitted snapshot fields and name/epoch identity substitution.
#[test]
fn rendered_text_and_json_preserve_coherent_snapshot() {
    let f = Fixture::new();
    let stream = f.spawn(true);
    assert_eq!(success(f.capture(&["--rendered"])), b"clean");
    let snapshot: fl_host_proto::RenderedScreen =
        serde_json::from_slice(&success(f.capture(&["--rendered", "--json"]))).unwrap();
    assert_eq!(snapshot, f.rpc().capture_rendered(stream.clone()).unwrap());
    assert_eq!(snapshot.stream, stream);
    assert_eq!(
        (
            snapshot.schema,
            snapshot.rows,
            snapshot.cols,
            snapshot.cursor_row,
            snapshot.cursor_col
        ),
        (1, 40, 120, 0, 5)
    );
    assert!(snapshot.generation > 0);
    assert!(snapshot.output_seq > 0);
    assert_eq!(snapshot.output_bytes, 25);
}

// Catches silently downgrading pipe sessions to raw output.
#[test]
fn rendered_pipe_refusal_has_no_raw_fallback() {
    let f = Fixture::new();
    f.spawn(false);
    let output = f.capture(&["--rendered"]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("WrongMode"));
}

// Catches accepting incoherent flag combinations before endpoint discovery.
#[test]
fn invalid_capture_flags_fail_before_connecting() {
    for flags in [
        vec!["--json"],
        vec!["--raw", "--rendered"],
        vec!["--raw", "--json"],
        vec!["--rendered", "--max-bytes", "16384"],
    ] {
        let root = tempfile::tempdir().unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_fl"))
            .args(["host", "capture", "capture-test"])
            .args(&flags)
            .env("FL_HOST_SOCK", root.path().join("absent.sock"))
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(2),
            "flags {flags:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stdout.is_empty());
    }
}

// Catches sending CaptureRendered to an older decoder or falling back to CaptureOutput.
#[tokio::test]
async fn older_daemon_is_refused_without_unknown_capture_request() {
    use fl_host_proto::{
        ChildRecord, ChildState, ExtensionRequest as Req, ExtensionResponse as Resp, HostConfig,
        HostInstance, HostListener, HostRequest, HostResponse, InputState, SessionEpoch,
        SessionMode, SessionSummary,
    };
    let root = tempfile::tempdir().unwrap();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let socket = root.path().join("old.sock");
    let mut cfg = HostConfig::new(HostEndpoint::unix(&socket).unwrap());
    cfg.minor = 1;
    let mut listener = HostListener::bind(cfg).await.unwrap();
    let server = tokio::spawn(async move {
        let mut conn = listener.accept().await.unwrap();
        let host = HostInstance::new([1; 16]);
        loop {
            let request = tokio::time::timeout(Duration::from_secs(5), conn.recv())
                .await
                .expect("CLI request deadline");
            let (id, request) = match request {
                Ok(HostRequest::Extended { id, request }) => (id, request),
                Err(_) => break,
                Ok(other) => panic!("older daemon received unexpected request: {other:?}"),
            };
            let response = match request {
                Req::Capabilities => Resp::Capabilities {
                    host: host.clone(),
                    features: fl_host_proto::RELIABLE_FEATURE,
                    limits: Default::default(),
                },
                Req::ListSessions => Resp::Sessions {
                    records: vec![SessionSummary {
                        stream: StreamId {
                            host: host.clone(),
                            session: SessionId::new("capture-test").unwrap(),
                            epoch: SessionEpoch(1),
                        },
                        mode: SessionMode::Pty,
                        state: ChildState::Running(ChildRecord {
                            session: SessionId::new("capture-test").unwrap(),
                            session_epoch: SessionEpoch(1),
                            kind: SpawnKind::Pty,
                            pid: 1,
                            input: InputState::Writable,
                            started_at_ms: 1,
                            exit: None,
                            consumer_stalled: false,
                        }),
                        acked: 0,
                        latest: 1,
                        input: InputState::Writable,
                    }],
                },
                other => panic!("older daemon received unknown capture request: {other:?}"),
            };
            conn.respond(&HostResponse::Extended { id, response })
                .await
                .unwrap();
        }
    });
    let output = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_fl"))
            .args(["host", "capture", "capture-test", "--rendered"])
            .env("FL_HOST_SOCK", socket)
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .to_ascii_lowercase()
            .contains("unsupported"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    server.await.unwrap();
}
