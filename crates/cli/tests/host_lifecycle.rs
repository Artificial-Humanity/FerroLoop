//! Lifecycle regressions against a private real host, never the resident daemon.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use fl_host_proto::{
    BlockingHostClient, ChildState, HostEndpoint, SessionId, SpawnKind, SpawnSpec,
};
use serde_json::Value;

struct Fixture {
    root: tempfile::TempDir,
    daemon: Child,
    endpoint: HostEndpoint,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let endpoint = HostEndpoint::unix(root.path().join("h.sock")).unwrap();
        let stub = root.path().join("vendor");
        std::fs::write(&stub, "#!/bin/sh\nexec /bin/cat\n").unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o700)).unwrap();
        let daemon = Command::new(env!("CARGO_BIN_EXE_fl"))
            .env_clear()
            .env("HOME", root.path())
            .env("PATH", "/usr/bin:/bin")
            .args(["host", "serve", "--socket"])
            .arg(endpoint.socket_path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut f = Self {
            root,
            daemon,
            endpoint,
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while BlockingHostClient::connect(&f.endpoint).is_err() {
            assert!(f.daemon.try_wait().unwrap().is_none(), "host exited");
            assert!(Instant::now() < deadline, "host startup timeout");
            std::thread::sleep(Duration::from_millis(10));
        }
        f
    }
    fn rpc(&self) -> BlockingHostClient {
        BlockingHostClient::connect(&self.endpoint).unwrap()
    }
    fn command(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_fl"));
        c.env_clear()
            .current_dir(self.root.path())
            .env("HOME", self.root.path())
            .env("PATH", "/usr/bin:/bin")
            .env("XDG_RUNTIME_DIR", self.root.path())
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("XDG_DATA_HOME", self.root.path().join("data"))
            .env("FL_HOST_SOCK", self.endpoint.socket_path())
            .env("FL_WIRE_DISABLE_HOST_DISCOVERY", "1")
            .env("FL_WIRE_CLAUDE_BIN", self.root.path().join("vendor"))
            .env("FL_WIRE_AGY_BIN", self.root.path().join("vendor"))
            .args(["--db"])
            .arg(self.root.path().join("fl.fls"));
        c
    }
    fn json(&self, args: &[&str], no_spawn: bool) -> Value {
        let mut c = self.command();
        c.args(args);
        if no_spawn {
            c.env("FL_TEST_SPINUP_NO_SPAWN", "1");
        }
        let out = c.output().unwrap();
        assert!(
            out.status.success(),
            "args {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }
    fn spin(&self, vendor: &str, no_spawn: bool) -> Value {
        self.json(
            &[
                "agent",
                "spin-up",
                "ada",
                "--project",
                "FerroLoop",
                "--role",
                "assistant",
                "--vendor",
                vendor,
                "--by",
                "owner",
                "--json",
            ],
            no_spawn,
        )
    }
    fn teardown(&self, vendor: &str) -> Value {
        self.json(
            &[
                "agent",
                "decommission",
                &format!("{vendor}:ada"),
                "--mode",
                "teardown",
                "--by",
                "owner",
                "--json",
            ],
            false,
        )
    }
    fn raw_spawn(&self, id: &str, owner: Option<&str>) {
        let mut env = std::collections::BTreeMap::new();
        if let Some(owner) = owner {
            env.insert("FL_HOSTED_ID".into(), owner.into());
        }
        self.rpc()
            .spawn_pty_session(
                SpawnSpec {
                    session: SessionId::new(id).unwrap(),
                    kind: SpawnKind::Pty,
                    argv: vec!["/bin/cat".into()],
                    cwd: self.root.path().into(),
                    env_allow: env.keys().cloned().collect(),
                    env,
                },
                None,
            )
            .unwrap();
    }
    fn running(&self, id: &str) -> bool {
        self.rpc()
            .list_sessions()
            .unwrap()
            .iter()
            .any(|s| s.stream.session.as_str() == id && matches!(s.state, ChildState::Running(_)))
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Ok(mut rpc) = BlockingHostClient::connect(&self.endpoint)
            && let Ok(sessions) = rpc.list_sessions()
        {
            for s in sessions {
                if let Ok(grant) = rpc.claim_controller(s.stream.clone()) {
                    let _ = rpc.kill_session(s.stream, grant.lease);
                }
            }
        }
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

#[test]
fn stale_record_stops_owned_vendor_then_spawns_new_vendor() {
    let f = Fixture::new();
    f.spin("antigravity", true); // Recorded target, but runtime_spawned=false.
    f.raw_spawn("Ada.antigravity", Some("antigravity:ada"));
    let out = f.teardown("antigravity");
    assert_eq!(out["runtime_stopped"], true);
    assert!(!f.running("Ada.antigravity"));
    let out = f.spin("claude", false);
    assert_eq!(out["runtime_spawned"], true);
    assert!(f.running("Ada.claude"));
    assert_eq!(f.teardown("claude")["runtime_stopped"], true);
    assert!(!f.running("Ada.claude"));
}

#[test]
fn stale_record_preserves_foreign_or_unmarked_vendor() {
    for owner in [None, Some("foreign:ada")] {
        let f = Fixture::new();
        f.spin("antigravity", true);
        f.raw_spawn("Ada.antigravity", owner);
        assert_eq!(f.teardown("antigravity")["runtime_stopped"], false);
        assert!(f.running("Ada.antigravity"));
    }
}

#[test]
fn spawn_and_teardown_do_not_match_other_vendors_by_persona() {
    let f = Fixture::new();
    f.raw_spawn("Ada.antigravity", Some("antigravity:ada"));
    assert_eq!(f.spin("claude", false)["runtime_spawned"], true);
    assert!(f.running("Ada.claude"));
    assert_eq!(f.teardown("claude")["runtime_stopped"], true);
    assert!(f.running("Ada.antigravity"));
    assert!(!f.running("Ada.claude"));
}

#[test]
fn ownership_query_binds_owner_host_and_epoch_and_refuses_unmarked_sessions() {
    let f = Fixture::new();
    f.raw_spawn("Ada.antigravity", Some("antigravity:ada"));
    f.raw_spawn("Ada.claude", None);
    let mut rpc = f.rpc();
    let stream = rpc
        .list_sessions()
        .unwrap()
        .into_iter()
        .find(|s| s.stream.session.as_str() == "Ada.antigravity")
        .unwrap()
        .stream;
    assert!(
        rpc.check_session_owner(stream.clone(), "antigravity:ada")
            .unwrap()
    );
    assert!(
        !rpc.check_session_owner(stream.clone(), "claude:ada")
            .unwrap()
    );
    assert!(!rpc.check_session_owner(stream.clone(), "").unwrap());
    assert!(
        !rpc.check_session_owner(stream.clone(), &"x".repeat(129))
            .unwrap()
    );
    let mut stale = stream.clone();
    stale.epoch.0 += 1;
    assert!(!rpc.check_session_owner(stale, "antigravity:ada").unwrap());
    let mut stale = stream.clone();
    stale.host = fl_host_proto::HostInstance::new([0; 16]);
    assert!(!rpc.check_session_owner(stale, "antigravity:ada").unwrap());
    let unmarked = rpc
        .list_sessions()
        .unwrap()
        .into_iter()
        .find(|s| s.stream.session.as_str() == "Ada.claude")
        .unwrap()
        .stream;
    assert!(!rpc.check_session_owner(unmarked, "claude:ada").unwrap());
    let grant = rpc.claim_controller(stream.clone()).unwrap();
    rpc.kill_session(stream.clone(), grant.lease).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while rpc
        .list_sessions()
        .unwrap()
        .iter()
        .any(|s| s.stream == stream && !matches!(s.state, ChildState::CleanupComplete(_)))
    {
        assert!(Instant::now() < deadline, "cleanup deadline");
        std::thread::sleep(Duration::from_millis(10));
    }
    f.raw_spawn("Ada.antigravity", Some("replacement-owner"));
    assert!(!rpc.check_session_owner(stream, "antigravity:ada").unwrap());
}

#[tokio::test]
async fn ownership_query_never_sends_unknown_request_to_older_host() {
    use fl_host_proto::{
        ExtensionRequest as Req, ExtensionResponse as Resp, HostConfig, HostInstance, HostListener,
        HostRequest, HostResponse, SessionEpoch, StreamId,
    };
    let root = tempfile::tempdir().unwrap();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let endpoint = HostEndpoint::unix(root.path().join("old.sock")).unwrap();
    let mut cfg = HostConfig::new(endpoint.clone());
    cfg.minor = 1;
    let mut listener = HostListener::bind(cfg).await.unwrap();
    let server = tokio::spawn(async move {
        let mut conn = listener.accept().await.unwrap();
        let HostRequest::Extended {
            id,
            request: Req::Capabilities,
        } = conn.recv().await.unwrap()
        else {
            panic!("expected capability query");
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
        // Unsupported means no new request reaches this decoder, and no mutation occurs.
        assert!(
            tokio::time::timeout(Duration::from_secs(5), conn.recv())
                .await
                .unwrap()
                .is_err()
        );
    });
    tokio::task::spawn_blocking(move || {
        let mut client = BlockingHostClient::connect(&endpoint).unwrap();
        assert!(
            !client
                .check_session_owner(
                    StreamId {
                        host: HostInstance::new([1; 16]),
                        session: SessionId::new("Ada.claude").unwrap(),
                        epoch: SessionEpoch(1),
                    },
                    "claude:ada"
                )
                .unwrap()
        );
    })
    .await
    .unwrap();
    server.await.unwrap();
}

#[test]
fn recorded_spawn_does_not_kill_a_foreign_replacement() {
    let f = Fixture::new();
    f.spin("claude", true);
    let entries = f.json(&["state", "list", "--concern", "runtime", "--json"], true);
    let entry = entries
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["slug"] == "hosted-session-claude-ada")
        .unwrap();
    let mut record = entry["payload"]["runtime"]["value"].clone();
    record["runtime_spawned"] = true.into();
    f.json(
        &[
            "state",
            "runtime",
            "set",
            "--scope",
            "workspace",
            "--key",
            "hosted-session-claude-ada",
            "--value",
            &record.to_string(),
            "--by",
            "owner",
            "--json",
        ],
        true,
    );
    f.raw_spawn("Ada.claude", Some("foreign:ada"));
    assert_eq!(f.teardown("claude")["runtime_stopped"], false);
    assert!(f.running("Ada.claude"));
}

#[tokio::test]
async fn spawn_refuses_older_host_before_creating_an_unverifiable_child() {
    use fl_host_proto::{
        ExtensionRequest as Req, ExtensionResponse as Resp, HostConfig, HostInstance, HostListener,
        HostRequest, HostResponse,
    };
    let root = tempfile::tempdir().unwrap();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let endpoint = HostEndpoint::unix(root.path().join("old.sock")).unwrap();
    let mut cfg = HostConfig::new(endpoint.clone());
    cfg.minor = 1;
    let mut listener = HostListener::bind(cfg).await.unwrap();
    let server = tokio::spawn(async move {
        let mut conn = listener.accept().await.unwrap();
        let HostRequest::Extended {
            id,
            request: Req::Capabilities,
        } = conn.recv().await.unwrap()
        else {
            panic!("expected capability preflight");
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
        assert!(
            tokio::time::timeout(Duration::from_secs(5), conn.recv())
                .await
                .unwrap()
                .is_err()
        );
    });
    let out = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_fl"))
            .env_clear()
            .current_dir(root.path())
            .env("HOME", root.path())
            .env("PATH", "/usr/bin:/bin")
            .env("XDG_RUNTIME_DIR", root.path())
            .env("XDG_CONFIG_HOME", root.path().join("config"))
            .env("XDG_DATA_HOME", root.path().join("data"))
            .env("FL_WIRE_DISABLE_HOST_DISCOVERY", "1")
            .env("FL_HOST_SOCK", endpoint.socket_path())
            .arg("--db")
            .arg(root.path().join("fl.fls"))
            .args([
                "agent",
                "spin-up",
                "ada",
                "--project",
                "FerroLoop",
                "--role",
                "assistant",
                "--vendor",
                "claude",
                "--by",
                "owner",
                "--json",
            ])
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let response: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(response["runtime_spawned"], false);
    server.await.unwrap();
}
