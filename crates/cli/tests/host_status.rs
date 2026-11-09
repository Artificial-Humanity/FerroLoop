//! Query a real isolated `fl host serve`: telemetry must describe that binary.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use fl_host_proto::{BlockingHostClient, HostEndpoint};

struct Daemon(Child);
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn host_serve_reports_its_cli_build_through_rpc_and_status() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let socket = dir.path().join("host.sock");
    let endpoint = HostEndpoint::unix(&socket).unwrap();
    let bin = env!("CARGO_BIN_EXE_fl");
    let version = Command::new(bin)
        .args(["version", "--json"])
        .output()
        .unwrap();
    assert!(version.status.success());
    let version: serde_json::Value = serde_json::from_slice(&version.stdout).unwrap();
    let expected = serde_json::json!({
        "version": version["version"], "commit": version["commit"],
        "dirty": version["dirty"], "build_time_unix": version["build_time_unix"],
    });
    let mut daemon = Daemon(
        Command::new(bin)
            .args(["host", "serve", "--socket"])
            .arg(&socket)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut client = loop {
        if let Ok(client) = BlockingHostClient::connect(&endpoint) {
            break client;
        }
        assert!(daemon.0.try_wait().unwrap().is_none(), "daemon exited");
        assert!(Instant::now() < deadline, "daemon startup deadline");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(
        serde_json::to_value(client.host_status().unwrap().unwrap()).unwrap(),
        expected
    );
    let output = Command::new(bin)
        .args(["host", "status", "--json"])
        .env("FL_HOST_SOCK", &socket)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let status: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(status["build_identity"], "available");
    assert_eq!(status["build"], expected);
    assert_eq!(status["pty_retention_diagnostics"], "available");
    assert_eq!(status["pty_retention"]["completed_pty_limit"], 8);
    assert_eq!(status["pty_retention"]["projection_reserved_bytes"], 0);
    assert!(
        status["pty_retention"]["projection_limit_bytes"]
            .as_u64()
            .unwrap()
            > 590_000_000
    );
    let output = Command::new(bin)
        .args(["host", "status"])
        .env("FL_HOST_SOCK", &socket)
        .output()
        .unwrap();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains(version["commit"].as_str().unwrap()));
    assert!(text.contains("build time (unix):"));
    assert!(text.contains("projection reservation:"));
    assert!(text.contains("completed PTYs: 0 / 8"));
}

#[tokio::test]
async fn old_host_status_explicitly_reports_unavailable() {
    use fl_host_proto::{
        ExtensionRequest as Req, ExtensionResponse as Resp, HostConfig, HostInstance, HostListener,
        HostRequest, HostResponse,
    };
    for minor in [0, 1] {
        for json in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
            let socket = dir.path().join("old.sock");
            let mut cfg = HostConfig::new(HostEndpoint::unix(&socket).unwrap());
            cfg.minor = minor;
            let mut listener = HostListener::bind(cfg).await.unwrap();
            let server = tokio::spawn(async move {
                let mut conn = listener.accept().await.unwrap();
                for _ in 0..if minor > 0 { 2 } else { 0 } {
                    let HostRequest::Extended {
                        id,
                        request: Req::Capabilities,
                    } = conn.recv().await.unwrap()
                    else {
                        panic!("expected capabilities")
                    };
                    conn.respond(&HostResponse::Extended {
                        id,
                        response: Resp::Capabilities {
                            host: HostInstance::new([1; 16]),
                            features: fl_host_proto::RELIABLE_FEATURE,
                            limits: Default::default(),
                        },
                    })
                    .await
                    .unwrap();
                }
                let response = match conn.recv().await.unwrap() {
                    HostRequest::Extended {
                        id,
                        request: Req::ListSessions,
                    } if minor > 0 => HostResponse::Extended {
                        id,
                        response: Resp::Sessions { records: vec![] },
                    },
                    HostRequest::List { id } if minor == 0 => HostResponse::Listed {
                        id,
                        children: vec![],
                    },
                    other => panic!("unexpected old-host request {other:?}"),
                };
                conn.respond(&response).await.unwrap();
            });
            let output = tokio::task::spawn_blocking(move || {
                let mut command = Command::new(env!("CARGO_BIN_EXE_fl"));
                command.args(["host", "status"]).env("FL_HOST_SOCK", socket);
                if json {
                    command.arg("--json");
                }
                command.output().unwrap()
            })
            .await
            .unwrap();
            assert!(
                output.status.success(),
                "minor {minor}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            if json {
                let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
                assert_eq!(value["build_identity"], "unavailable");
                assert!(value["build"].is_null());
                assert_eq!(value["pty_retention_diagnostics"], "unavailable");
                assert!(value["pty_retention"].is_null());
            } else {
                assert!(
                    String::from_utf8(output.stdout)
                        .unwrap()
                        .contains("build identity: unavailable")
                );
            }
            server.await.unwrap();
        }
    }
}
