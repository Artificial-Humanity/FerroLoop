//! Runtime-port behavior over the authenticated host socket, with no live daemon.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use fl_core::vendor_adapter::{
    AdapterIdentityBinding, AdapterReadiness, RuntimePort, TeardownOutcome, VendorAdapterError,
};
use fl_host_proto::{
    ChildRecord, ChildState, ControllerLease, ExtensionLimits, ExtensionRefusal, ExtensionRequest,
    ExtensionResponse, HostConfig, HostEndpoint, HostInstance, HostListener, HostRequest,
    HostResponse, InputState, MAX_RAW_INPUT, RENDERED_PTY_FEATURE, RenderedScreen, SendBlock,
    SendOutcome, SessionEpoch, SessionId, SessionMode, SessionSummary, SpawnKind, StreamId,
};
use fl_wire::vendor_adapter::HostRuntimePort;

type Step = Box<dyn FnOnce(ExtensionRequest) -> ExtensionResponse + Send>;

fn with_host(sessions: Vec<SessionSummary>, steps: Vec<Step>, test: impl FnOnce(HostRuntimePort)) {
    with_endpoint_host(sessions, steps, |endpoint| {
        test(HostRuntimePort::with_endpoint(endpoint))
    });
}

/// Every accept/read is bounded, including the check for unexpected extra requests.
fn with_endpoint_host(
    sessions: Vec<SessionSummary>,
    steps: Vec<Step>,
    test: impl FnOnce(HostEndpoint),
) {
    let dir = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let endpoint = HostEndpoint::unix(dir.path().join("h.sock")).unwrap();
    let (ready_tx, ready_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let mut config = HostConfig::new(endpoint);
            config.frame_timeout = Duration::from_secs(2);
            let mut listener = HostListener::bind(config).await.unwrap();
            ready_tx.send(listener.endpoint().clone()).unwrap();
            let mut conn = tokio::time::timeout(Duration::from_secs(3), listener.accept())
                .await
                .expect("runtime port did not connect")
                .unwrap();
            let list: Step = Box::new(move |request| {
                assert_eq!(request, ExtensionRequest::ListSessions);
                ExtensionResponse::Sessions { records: sessions }
            });
            for step in std::iter::once(list).chain(steps) {
                let request = tokio::time::timeout(Duration::from_secs(3), conn.recv())
                    .await
                    .expect("runtime port omitted a request")
                    .unwrap();
                let HostRequest::Extended { id, request } = request else {
                    panic!("expected extension RPC");
                };
                conn.respond(&HostResponse::Extended {
                    id,
                    response: step(request),
                })
                .await
                .unwrap();
            }
            // An extra request here detects retries and calls made after a failed guard.
            let end = tokio::time::timeout(Duration::from_secs(3), conn.recv()).await;
            assert!(
                matches!(end, Ok(Err(_))),
                "unexpected request or unclosed connection: {end:?}"
            );
            listener.shutdown().await.unwrap();
        });
    });
    let endpoint = ready_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    let result = catch_unwind(AssertUnwindSafe(|| test(endpoint)));
    let joined = server.join();
    if let Err(panic) = result {
        resume_unwind(panic);
    }
    joined.unwrap();
}

fn binding(name: &str) -> AdapterIdentityBinding {
    // Neither generation nor process start ticks equals the host's epoch/time.
    AdapterIdentityBinding::new("ada", 900, name, Some((42, 987654)))
}

fn summary(name: &str, epoch: u64, mode: SessionMode, state: u8) -> SessionSummary {
    let session = SessionId::new(name).unwrap();
    let record = ChildRecord {
        session: session.clone(),
        session_epoch: SessionEpoch(epoch),
        kind: if mode == SessionMode::Pty {
            SpawnKind::Pty
        } else {
            SpawnKind::Pipe
        },
        pid: 42,
        input: InputState::Writable,
        started_at_ms: 123,
        exit: None,
        consumer_stalled: false,
    };
    SessionSummary {
        stream: StreamId {
            host: HostInstance::new([7; 16]),
            session,
            epoch: SessionEpoch(epoch),
        },
        mode,
        state: match state {
            0 => ChildState::Running(record),
            1 => ChildState::LeaderExited(record),
            _ => ChildState::CleanupComplete(record),
        },
        acked: 0,
        latest: 0,
        input: InputState::Writable,
    }
}

#[test]
fn readiness_maps_host_states_and_absence() {
    for (state, expected) in [
        (0, AdapterReadiness::ReadyIdle),
        (1, AdapterReadiness::Exiting),
        (2, AdapterReadiness::Exited),
    ] {
        with_host(
            vec![summary("ada.claude", 4, SessionMode::Pty, state)],
            vec![],
            |port| {
                assert_eq!(
                    port.session_readiness(&binding("ada:claude")).unwrap(),
                    expected
                );
            },
        );
    }
    with_host(vec![], vec![], |port| {
        assert_eq!(
            port.session_readiness(&binding("ada.claude")).unwrap(),
            AdapterReadiness::Exited
        );
    });
}

#[test]
fn readiness_selects_newest_exact_session_before_pid_guard() {
    let old = summary("ada.claude", 4, SessionMode::Pty, 0);
    let mut replaced = summary("ada.claude", 5, SessionMode::Pty, 0);
    if let ChildState::Running(record) = &mut replaced.state {
        record.pid = 43;
    }
    with_host(vec![replaced, old], vec![], |port| {
        assert_eq!(
            port.session_readiness(&binding("ada.claude")).unwrap(),
            AdapterReadiness::Exited
        );
    });
    with_host(
        vec![
            summary("ada.claude", 4, SessionMode::Pty, 0),
            summary("ada.claude", 5, SessionMode::Pty, 2),
        ],
        vec![],
        |port| {
            assert_eq!(
                port.session_readiness(&binding("ada.claude")).unwrap(),
                AdapterReadiness::Exited
            );
        },
    );
    with_host(
        vec![summary("ada.claude", 4, SessionMode::Pty, 0)],
        vec![],
        |port| {
            assert_eq!(
                port.session_readiness(&binding("ada")).unwrap(),
                AdapterReadiness::Exited
            );
        },
    );
}

fn assert_transport<T: std::fmt::Debug>(result: Result<T, VendorAdapterError>) {
    assert!(
        matches!(result, Err(VendorAdapterError::Transport { .. })),
        "{result:?}"
    );
}

fn claim(stream: StreamId, response: ExtensionResponse) -> Step {
    Box::new(move |request| {
        assert_eq!(request, ExtensionRequest::ClaimController { stream });
        response
    })
}

fn granted(stream: StreamId) -> Step {
    claim(
        stream,
        ExtensionResponse::Controller {
            lease: ControllerLease(812),
            acked: 0,
            latest: 0,
        },
    )
}

fn refused() -> ExtensionResponse {
    ExtensionResponse::Refused {
        code: ExtensionRefusal::StaleLease,
    }
}

#[test]
fn raw_write_uses_selected_stream_and_claimed_lease_without_changing_bytes() {
    let session = summary("ada.claude", 5, SessionMode::Pty, 0);
    let stream = session.stream.clone();
    let bytes = b"hello\0\x1b[200~\n";
    let steps: Vec<Step> = vec![
        granted(stream.clone()),
        Box::new(move |request| {
            let ExtensionRequest::Write {
                stream: actual,
                lease,
                bytes: actual_bytes,
            } = request
            else {
                panic!("raw Write required, got {request:?}");
            };
            assert_eq!(actual, stream);
            assert_eq!(lease, ControllerLease(812));
            assert_eq!(actual_bytes.as_slice(), bytes);
            ExtensionResponse::Input {
                outcome: SendOutcome::Written,
            }
        }),
    ];
    with_host(
        vec![summary("ada.claude", 4, SessionMode::Pty, 0), session],
        steps,
        |port| {
            port.write_bytes(&binding("ada:claude"), bytes).unwrap();
        },
    );
}

#[test]
fn raw_write_reports_blocked_ambiguous_and_refused_without_retry() {
    for response in [
        ExtensionResponse::Input {
            outcome: SendOutcome::NotWritten {
                reason: SendBlock::SessionFenced,
            },
        },
        ExtensionResponse::Input {
            outcome: SendOutcome::Ambiguous {
                reason: SendBlock::Timeout,
            },
        },
        refused(),
    ] {
        let session = summary("ada.claude", 5, SessionMode::Pty, 0);
        let stream = session.stream.clone();
        let steps: Vec<Step> = vec![
            granted(stream.clone()),
            Box::new(move |request| {
                assert!(
                    matches!(request, ExtensionRequest::Write { stream: actual, lease: ControllerLease(812), .. } if actual == stream)
                );
                response
            }),
        ];
        with_host(vec![session], steps, |port| {
            assert_transport(port.write_bytes(&binding("ada.claude"), b"x"))
        });
    }
    let session = summary("ada.claude", 5, SessionMode::Pty, 0);
    let steps = vec![claim(session.stream.clone(), refused())];
    with_host(vec![session], steps, |port| {
        assert_transport(port.write_bytes(&binding("ada.claude"), b"x"))
    });
}

#[test]
fn write_and_capture_refuse_missing_exited_and_replaced_bindings() {
    let mut wrong_pid = binding("ada.claude");
    wrong_pid.pid_birth = Some((43, 0));
    let fixtures = [
        (vec![], binding("ada.claude")),
        (
            vec![summary("ada.claude", 5, SessionMode::Pty, 1)],
            binding("ada.claude"),
        ),
        (
            vec![summary("ada.claude", 5, SessionMode::Pty, 2)],
            binding("ada.claude"),
        ),
        (
            vec![summary("ada.claude", 5, SessionMode::Pty, 0)],
            wrong_pid,
        ),
        (
            vec![summary("ada.claude", 5, SessionMode::Pty, 0)],
            binding("ada"),
        ),
    ];
    for (sessions, bound) in fixtures {
        with_host(sessions.clone(), vec![], |port| {
            assert_transport(port.write_bytes(&bound, b"x"))
        });
        // Retained exit output remains useful; capture only requires identity/PID guards.
        if sessions
            .first()
            .is_some_and(|session| !matches!(session.state, ChildState::Running(_)))
        {
            continue;
        }
        with_host(sessions, vec![], |port| {
            assert_transport(port.capture_output(&bound, 100))
        });
    }
}

fn capabilities(features: u64) -> Step {
    Box::new(move |request| {
        assert_eq!(request, ExtensionRequest::Capabilities);
        ExtensionResponse::Capabilities {
            host: HostInstance::new([7; 16]),
            features,
            limits: ExtensionLimits::default(),
        }
    })
}

fn raw_capture(limit: u32, text: &'static str) -> Step {
    Box::new(move |request| {
        assert_eq!(
            request,
            ExtensionRequest::CaptureOutput {
                session: SessionId::new("ada.claude").unwrap(),
                max_bytes: limit
            }
        );
        ExtensionResponse::CapturedOutput { text: text.into() }
    })
}

#[test]
fn pty_capture_prefers_rendered_screen_contents() {
    let session = summary("ada.claude", 5, SessionMode::Pty, 0);
    let stream = session.stream.clone();
    let steps: Vec<Step> = vec![
        capabilities(RENDERED_PTY_FEATURE),
        Box::new(move |request| {
            assert_eq!(
                request,
                ExtensionRequest::CaptureRendered {
                    stream: stream.clone()
                }
            );
            ExtensionResponse::RenderedCapture {
                screen: RenderedScreen {
                    schema: 1,
                    stream,
                    contents: "visible screen".into(),
                    cursor_row: 0,
                    cursor_col: 0,
                    rows: 24,
                    cols: 80,
                    generation: 6,
                    output_seq: 12,
                    output_bytes: 300,
                },
            }
        }),
    ];
    with_host(vec![session], steps, |port| {
        assert_eq!(
            port.capture_output(&binding("ada.claude"), 10).unwrap(),
            "visible screen"
        );
    });
}

#[test]
fn pty_capture_falls_back_for_unsupported_and_any_rendered_refusal() {
    let session = summary("ada.claude", 5, SessionMode::Pty, 0);
    with_host(
        vec![session.clone()],
        vec![capabilities(0), raw_capture(80, "raw unsupported")],
        |port| {
            assert_eq!(
                port.capture_output(&binding("ada.claude"), 80).unwrap(),
                "raw unsupported"
            );
        },
    );
    for code in [
        ExtensionRefusal::SnapshotNotReady,
        ExtensionRefusal::Io,
        ExtensionRefusal::WrongInstance,
    ] {
        let stream = session.stream.clone();
        let steps: Vec<Step> = vec![
            capabilities(RENDERED_PTY_FEATURE),
            Box::new(move |request| {
                assert_eq!(request, ExtensionRequest::CaptureRendered { stream });
                ExtensionResponse::Refused { code }
            }),
            raw_capture(81, "raw fallback"),
        ];
        with_host(vec![session.clone()], steps, |port| {
            assert_eq!(
                port.capture_output(&binding("ada.claude"), 81).unwrap(),
                "raw fallback"
            );
        });
    }
}

#[test]
fn pipe_capture_skips_rendering_and_caps_raw_limit_without_overflow() {
    for mode in [SessionMode::LegacyPipe, SessionMode::ReliablePipe] {
        for (max, expected) in [
            (0, 0),
            (31, 31),
            (MAX_RAW_INPUT, MAX_RAW_INPUT as u32),
            (usize::MAX, MAX_RAW_INPUT as u32),
        ] {
            let session = summary("ada.claude", 5, mode, 0);
            with_host(
                vec![session],
                vec![raw_capture(expected, "pipe bytes")],
                |port| {
                    let mut bound = binding("ada:claude");
                    bound.pid_birth = None;
                    assert_eq!(port.capture_output(&bound, max).unwrap(), "pipe bytes");
                },
            );
        }
    }
}

#[test]
fn stop_claims_controller_and_kills_running_or_exiting_session() {
    for state in [0, 1] {
        let session = summary("ada.claude", 5, SessionMode::Pty, state);
        let stream = session.stream.clone();
        let steps: Vec<Step> = vec![
            granted(stream.clone()),
            Box::new(move |request| {
                assert_eq!(
                    request,
                    ExtensionRequest::KillSession {
                        stream,
                        lease: ControllerLease(812)
                    }
                );
                ExtensionResponse::Controlled
            }),
        ];
        with_host(vec![session], steps, |port| {
            assert_eq!(
                port.stop_session(&binding("ada.claude")).unwrap(),
                TeardownOutcome::Completed
            );
        });
    }
}

#[test]
fn stop_is_idempotent_for_absent_cleaned_or_replaced_sessions() {
    for sessions in [vec![], vec![summary("ada.claude", 5, SessionMode::Pty, 2)]] {
        with_host(sessions, vec![], |port| {
            assert_eq!(
                port.stop_session(&binding("ada.claude")).unwrap(),
                TeardownOutcome::AlreadyAbsent
            );
        });
    }
    with_host(
        vec![summary("ada.claude", 5, SessionMode::Pty, 0)],
        vec![],
        |port| {
            let mut bound = binding("ada.claude");
            bound.pid_birth = Some((43, 987654));
            assert_eq!(
                port.stop_session(&bound).unwrap(),
                TeardownOutcome::AlreadyAbsent
            );
        },
    );
}

#[test]
fn stop_reports_controller_or_kill_errors_as_transport() {
    let session = summary("ada.claude", 5, SessionMode::Pty, 0);
    with_host(
        vec![session.clone()],
        vec![claim(session.stream.clone(), refused())],
        |port| {
            assert_transport(port.stop_session(&binding("ada.claude")));
        },
    );
    let stream = session.stream.clone();
    let steps: Vec<Step> = vec![
        granted(stream.clone()),
        Box::new(move |request| {
            assert_eq!(
                request,
                ExtensionRequest::KillSession {
                    stream,
                    lease: ControllerLease(812)
                }
            );
            refused()
        }),
    ];
    with_host(vec![session], steps, |port| {
        assert_transport(port.stop_session(&binding("ada.claude")))
    });
}

#[test]
fn invalid_paths_return_errors_and_unreachable_host_is_transport() {
    assert!(HostRuntimePort::with_socket_path("relative.sock").is_err());
    assert!(HostRuntimePort::with_socket_path("/parent/../socket").is_err());
    let dir = tempfile::tempdir().unwrap();
    let port = HostRuntimePort::with_socket_path(dir.path().join("absent.sock")).unwrap();
    let bound = binding("ada.claude");
    assert_transport(port.session_readiness(&bound));
    assert_transport(port.write_bytes(&bound, b"x"));
    assert_transport(port.capture_output(&bound, 1));
    assert_transport(port.stop_session(&bound));
}

#[test]
fn capture_can_read_retained_exit_output_and_reports_raw_refusals() {
    for state in [1, 2] {
        with_host(
            vec![summary("ada.claude", 5, SessionMode::ReliablePipe, state)],
            vec![raw_capture(100, "final output")],
            |port| {
                assert_eq!(
                    port.capture_output(&binding("ada.claude"), 100).unwrap(),
                    "final output"
                )
            },
        );
    }
    with_host(
        vec![summary("ada.claude", 5, SessionMode::ReliablePipe, 0)],
        vec![Box::new(|request| {
            assert!(matches!(request, ExtensionRequest::CaptureOutput { .. }));
            refused()
        })],
        |port| assert_transport(port.capture_output(&binding("ada.claude"), 100)),
    );
}

#[test]
fn default_port_rediscovers_endpoint_on_each_call_in_isolated_process() {
    with_endpoint_host(
        vec![summary("ada.claude", 5, SessionMode::Pty, 0)],
        vec![],
        |first| {
            with_endpoint_host(vec![], vec![], |second| {
                let child = std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "default_discovery_child",
                        "--nocapture",
                        "--test-threads=1",
                    ])
                    .env("FL_HOST_SOCK", first.socket_path())
                    .env("FL_WIRE_RUNTIME_DISCOVERY_SECOND", second.socket_path())
                    .output()
                    .unwrap();
                assert!(
                    child.status.success(),
                    "{}\n{}",
                    String::from_utf8_lossy(&child.stdout),
                    String::from_utf8_lossy(&child.stderr)
                );
            });
        },
    );
}

#[test]
fn default_discovery_child() {
    let Some(second) = std::env::var_os("FL_WIRE_RUNTIME_DISCOVERY_SECOND") else {
        return;
    };
    let port = HostRuntimePort::new();
    assert_eq!(
        port.session_readiness(&binding("ada.claude")).unwrap(),
        AdapterReadiness::ReadyIdle
    );
    // This test only mutates the environment in a subprocess running this sole test.
    // Its servers live in the parent process, so no client/server threads share this environment.
    unsafe { std::env::set_var("FL_HOST_SOCK", second) };
    assert_eq!(
        port.session_readiness(&binding("ada.claude")).unwrap(),
        AdapterReadiness::Exited
    );
    unsafe { std::env::set_var("FL_HOST_SOCK", "relative-invalid.sock") };
    assert_transport(port.session_readiness(&binding("ada.claude")));
}
