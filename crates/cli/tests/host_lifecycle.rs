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
    for owner in [Some("foreign:ada"), None] {
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
        f.raw_spawn("Ada.claude", owner);
        let out = f
            .command()
            .args([
                "agent",
                "decommission",
                "claude:ada",
                "--mode",
                "teardown",
                "--by",
                "owner",
                "--json",
            ])
            .output()
            .unwrap();
        assert!(!out.status.success());
        let error = String::from_utf8_lossy(&out.stderr);
        assert!(
            error.contains("Ada.claude") && error.contains("fl host kill"),
            "{error}"
        );
        assert!(f.running("Ada.claude"));
        let entries = f.json(&["state", "list", "--concern", "runtime", "--json"], true);
        let value = &entries
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["slug"] == "hosted-session-claude-ada")
            .unwrap()["payload"]["runtime"]["value"];
        assert_eq!(value["status"], "running");
    }
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
        let out = Command::new(env!("CARGO_BIN_EXE_fl"))
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
            .unwrap();
        use fl_core::store::AgentRegistry;
        let store = fl_store::open_store(&root.path().join("fl.fls")).unwrap();
        assert!(store.agent_by_name("ada").unwrap().is_none());
        assert!(
            fl_hub::lifecycle::list_hosted_sessions(&store)
                .unwrap()
                .is_empty()
        );
        out
    })
    .await
    .unwrap();
    assert!(!out.status.success());
    let error = String::from_utf8_lossy(&out.stderr);
    assert!(error.contains("SESSION_OWNER_FEATURE"), "{error}");
    assert!(
        error.contains("upgrade and restart ferroloop-host.service"),
        "{error}"
    );
    server.await.unwrap();
}

/// An old daemon that knows reliable sessions but has no ownership extension. Any
/// unknown request fails the test, and every kill must use the exact listed stream.
async fn old_host(
    endpoint: HostEndpoint,
    sessions: Vec<fl_host_proto::SessionSummary>,
) -> (
    tokio::task::JoinHandle<()>,
    std::sync::Arc<std::sync::Mutex<Vec<fl_host_proto::StreamId>>>,
) {
    use fl_host_proto::{
        ControllerLease, ExtensionRequest as Req, ExtensionResponse as Resp, HostConfig,
        HostInstance, HostListener, HostRequest, HostResponse,
    };
    let mut cfg = HostConfig::new(endpoint);
    cfg.minor = 1;
    let listener = HostListener::bind(cfg).await.unwrap();
    let sessions = std::sync::Arc::new(std::sync::Mutex::new(sessions));
    let killed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = killed.clone();
    let task = tokio::spawn(async move {
        let mut listener = listener;
        while let Ok(mut conn) = listener.accept().await {
            let sessions = sessions.clone();
            let killed = killed.clone();
            tokio::spawn(async move {
                while let Ok(HostRequest::Extended { id, request }) = conn.recv().await {
                    let response = match request {
                        Req::Capabilities => Resp::Capabilities {
                            host: HostInstance::new([1; 16]),
                            features: fl_host_proto::RELIABLE_FEATURE,
                            limits: Default::default(),
                        },
                        Req::ListSessions => Resp::Sessions {
                            records: sessions.lock().unwrap().clone(),
                        },
                        Req::ClaimController { stream } => {
                            assert!(sessions.lock().unwrap().iter().any(|s| s.stream == stream));
                            Resp::Controller {
                                lease: ControllerLease(1),
                                acked: 0,
                                latest: 0,
                            }
                        }
                        Req::KillSession { stream, lease } => {
                            assert_eq!(lease, ControllerLease(1));
                            let mut sessions = sessions.lock().unwrap();
                            assert!(sessions.iter().any(|s| s.stream == stream));
                            sessions.retain(|s| s.stream != stream);
                            killed.lock().unwrap().push(stream);
                            Resp::Controlled
                        }
                        unexpected => panic!("old host received {unexpected:?}"),
                    };
                    conn.respond(&HostResponse::Extended { id, response })
                        .await
                        .unwrap();
                }
            });
        }
    });
    (task, seen)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn old_host_stops_only_exact_recorded_unit_and_refuses_unproven_adoption() {
    for recorded in [true, false] {
        let (f, summaries) = tokio::task::spawn_blocking(move || {
            let f = Fixture::new();
            f.spin("claude", !recorded);
            if !recorded {
                f.raw_spawn("Ada.claude", None);
            }
            f.raw_spawn("Ada.antigravity", None);
            let summaries = f.rpc().list_sessions().unwrap();
            (f, summaries)
        })
        .await
        .unwrap();
        let endpoint = HostEndpoint::unix(f.root.path().join("old.sock")).unwrap();
        let wanted = summaries
            .iter()
            .find(|s| s.stream.session.as_str() == "Ada.claude")
            .unwrap()
            .stream
            .clone();
        let (server, killed) = old_host(endpoint.clone(), summaries).await;
        tokio::task::spawn_blocking(move || {
            let before = f.json(&["state", "list", "--scope", "workspace", "--json"], true);
            let out = f
                .command()
                .env("FL_HOST_SOCK", endpoint.socket_path())
                .args([
                    "agent",
                    "decommission",
                    "claude:ada",
                    "--mode",
                    "teardown",
                    "--by",
                    "owner",
                    "--json",
                ])
                .output()
                .unwrap();
            if recorded {
                assert!(
                    out.status.success(),
                    "{}",
                    String::from_utf8_lossy(&out.stderr)
                );
                let json: Value = serde_json::from_slice(&out.stdout).unwrap();
                assert_eq!(json["runtime_stopped"], true);
                assert_eq!(*killed.lock().unwrap(), vec![wanted]);
            } else {
                assert!(!out.status.success());
                assert!(String::from_utf8_lossy(&out.stderr).contains("SESSION_OWNER_FEATURE"));
                assert!(killed.lock().unwrap().is_empty());
                assert_eq!(
                    before,
                    f.json(&["state", "list", "--scope", "workspace", "--json"], true)
                );
            }
        })
        .await
        .unwrap();
        server.abort();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hub_reports_old_host_refusal_without_creating_session_or_agent() {
    let root = tempfile::tempdir().unwrap();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let endpoint = HostEndpoint::unix(root.path().join("old.sock")).unwrap();
    let (server, killed) = old_host(endpoint.clone(), Vec::new()).await;
    let out = tokio::task::spawn_blocking(move || {
        Command::new(std::env::current_exe().unwrap())
            .env_clear()
            .env("HOME", root.path())
            .env("PATH", "/usr/bin:/bin")
            .env("FL_HOST_SOCK", endpoint.socket_path())
            .env("F397_HUB_CHILD", root.path())
            .args(["--exact", "hub_old_host_refusal_child", "--nocapture"])
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(killed.lock().unwrap().is_empty());
    server.abort();
}

#[test]
fn hub_old_host_refusal_child() {
    use fl_core::store::AgentRegistry;
    let Some(root) = std::env::var_os("F397_HUB_CHILD").map(std::path::PathBuf::from) else {
        return;
    };
    let path = root.join("hub.fls");
    let state = fl_hub::HubState::new(fl_hub::HubServerConfig {
        store_path: path.clone(),
        repo_root: root.clone(),
        home_override: Some(root.clone()),
        owner_token: Some("test-owner".into()),
        ..Default::default()
    });
    let req = fl_hub::HttpRequest::new("POST", "/api/v1/sessions/spin-up")
        .with_header("authorization", "Bearer test-owner")
        .with_json_body(&serde_json::json!({"persona":"ada", "project":"FerroLoop", "role":"assistant", "vendor":"claude"}));
    let response = fl_hub::handle_http_request(&state, &req);
    assert_eq!(
        response.status,
        409,
        "{}",
        String::from_utf8_lossy(&response.body)
    );
    assert!(
        String::from_utf8_lossy(&response.body)
            .contains("upgrade and restart ferroloop-host.service")
    );
    let store = fl_store::open_store(&path).unwrap();
    assert!(store.agent_by_name("ada").unwrap().is_none());
    assert!(
        fl_hub::lifecycle::list_hosted_sessions(&store)
            .unwrap()
            .is_empty()
    );
    use fl_hub::lifecycle::{self, Caller, LifecycleEnv, SpinUpRequest};
    let mut env = LifecycleEnv::new(Some(&root));
    env.spawn_runtime = false;
    let seed = SpinUpRequest {
        persona: "ada".into(),
        project: "FerroLoop".into(),
        role: "assistant".into(),
        vendor: "claude".into(),
        ..Default::default()
    };
    lifecycle::spin_up(&store, &env, &seed, "owner", Caller::Owner).unwrap();
    let before = lifecycle::list_hosted_sessions(&store).unwrap();
    for (path, body) in [
        (
            "/api/v1/hosted-sessions/claude:ada/restart",
            serde_json::json!({}),
        ),
        (
            "/api/v1/sessions/claude:ada/remodel",
            serde_json::json!({"vendor":"codex", "mode":"execute"}),
        ),
    ] {
        let req = fl_hub::HttpRequest::new("POST", path)
            .with_header("authorization", "Bearer test-owner")
            .with_json_body(&body);
        let response = fl_hub::handle_http_request(&state, &req);
        assert_eq!(
            response.status,
            409,
            "{}",
            String::from_utf8_lossy(&response.body)
        );
        assert!(String::from_utf8_lossy(&response.body).contains("SESSION_OWNER_FEATURE"));
        assert_eq!(before, lifecycle::list_hosted_sessions(&store).unwrap());
    }
    use lifecycle::SessionRuntime;
    for id in [String::new(), "x".repeat(129)] {
        let error = lifecycle::HostRuntime.preflight_spawn(&id).unwrap_err();
        assert_eq!(error.status(), 400);
        assert!(error.to_string().contains("1..=128 bytes"));
    }
    assert_eq!(before, lifecycle::list_hosted_sessions(&store).unwrap());
}

// Each HTTP request runs in a subprocess so its host endpoint cannot affect parallel tests.
#[test]
fn hub_refuses_remodel_of_foreign_or_unmarked_replacement_before_any_effect() {
    for owner in [Some("foreign:ada"), None] {
        for hosted in [true, false] {
            let f = Fixture::new();
            f.spin("claude", true);
            f.raw_spawn("Ada.claude", owner);
            let sessions = serde_json::to_value(f.rpc().list_sessions().unwrap()).unwrap();
            let out = Command::new(std::env::current_exe().unwrap())
                .env_clear()
                .env("HOME", f.root.path())
                .env("PATH", "/usr/bin:/bin")
                .env("FL_HOST_SOCK", f.endpoint.socket_path())
                .env("FL_WIRE_DISABLE_HOST_DISCOVERY", "1")
                .env("F397_REMODEL_CHILD", f.root.path())
                .env("F397_REMODEL_HOSTED", hosted.to_string())
                .args(["--exact", "hub_remodel_owner_refusal_child", "--nocapture"])
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "owner={owner:?}, hosted={hosted}: {}\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            // The original epoch remains running, and no destination or restore was spawned.
            assert_eq!(
                sessions,
                serde_json::to_value(f.rpc().list_sessions().unwrap()).unwrap()
            );
        }
    }
}

#[test]
fn hub_remodel_owner_refusal_child() {
    use fl_core::store::{AgentRegistry, StateStore};
    use fl_hub::lifecycle;
    let Some(root) = std::env::var_os("F397_REMODEL_CHILD").map(std::path::PathBuf::from) else {
        return;
    };
    let hosted: bool = std::env::var("F397_REMODEL_HOSTED")
        .unwrap()
        .parse()
        .unwrap();
    let path = root.join("fl.fls");
    let store = fl_store::open_store(&path).unwrap();
    let mut record = lifecycle::list_hosted_sessions(&store).unwrap().remove(0);
    // The record describes the prior owned child, which has since been replaced.
    record.runtime_spawned = true;
    lifecycle::persist_hosted_session(&store, &record, "owner").unwrap();
    let routes_path = root.join(".cache/agent-bus/routes.json");
    let mut routes = fl_wire::identity::RouteTable::default();
    routes.upsert(
        "ada",
        fl_wire::identity::VendorRoute::Webhook {
            socket_path: root.join("none.sock"),
        },
        fl_core::At::from_unix_millis(1),
    );
    routes.save(Some(&routes_path)).unwrap();
    let before_routes = std::fs::read(&routes_path).unwrap();
    let before_state = store.list_state(None, None).unwrap();
    let before_agents = store.list_agents(None, true).unwrap();
    let before_registry = store.registry_events_after(0).unwrap();
    let state = fl_hub::HubState::new(fl_hub::HubServerConfig {
        store_path: path,
        repo_root: root.clone(),
        home_override: Some(root),
        owner_token: Some("test-owner".into()),
        ..Default::default()
    });
    let req = fl_hub::HttpRequest::new("POST", "/api/v1/sessions/claude:ada/remodel")
        .with_header("authorization", "Bearer test-owner")
        .with_json_body(&serde_json::json!({"vendor":"codex", "mode":"execute", "hosted":hosted}));
    let response = fl_hub::handle_http_request(&state, &req);
    assert_eq!(
        response.status,
        409,
        "{}",
        String::from_utf8_lossy(&response.body)
    );
    let error = String::from_utf8_lossy(&response.body);
    assert!(
        error.contains("Ada.claude") && error.contains("fl host kill"),
        "{error}"
    );
    // Includes hosted records and the durable remodel handoff: no rollback/restore revisions.
    assert_eq!(before_state, store.list_state(None, None).unwrap());
    assert_eq!(before_agents, store.list_agents(None, true).unwrap());
    assert_eq!(before_registry, store.registry_events_after(0).unwrap());
    assert_eq!(before_routes, std::fs::read(&routes_path).unwrap());
}

/// A running vendor can expose its native identity through a child before it registers a route.
#[cfg(target_os = "linux")]
#[test]
fn agy_teardown_recovers_missing_route_identity_and_paired_endpoint() {
    agy_recovery_case(None);
}

#[cfg(target_os = "linux")]
#[test]
fn agy_teardown_recovers_bare_persona_route_identity() {
    agy_recovery_case(Some(("ada", true)));
}

#[cfg(target_os = "linux")]
#[test]
fn agy_teardown_recovers_address_without_token() {
    agy_recovery_case(Some(("4df33135-e9a4-4f85-adfd-3b09dc339628", false)));
}

#[cfg(target_os = "linux")]
#[test]
fn agy_teardown_never_sends_bare_persona_alongside_cached_uuid() {
    agy_recovery_case(Some(("4df33135-e9a4-4f85-adfd-3b09dc339628", true)));
}

#[cfg(target_os = "linux")]
fn agy_recovery_case(route: Option<(&str, bool)>) {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    let f = Fixture::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let cid = "4df33135-e9a4-4f85-adfd-3b09dc339628";
    std::fs::write(f.root.path().join("vendor"), format!(
        "#!/bin/sh\nANTIGRAVITY_CONVERSATION_ID={cid} ANTIGRAVITY_LS_ADDRESS={addr} ANTIGRAVITY_CSRF_TOKEN=fixture-token /bin/sleep 60 &\nwait\n"
    )).unwrap();
    f.spin("antigravity", false);
    assert!(!f.root.path().join(".cache/agent-bus/routes.json").exists());
    if let Some((route_id, token)) = route {
        let mut routes = fl_wire::identity::RouteTable::default();
        routes.upsert(
            "ada",
            fl_wire::identity::VendorRoute::Agy {
                conversation_id: route_id.into(),
                ls_pid: None,
                ls_address: Some(addr.to_string()),
                csrf_token: token.then(|| "fixture-token".into()),
                agentapi_exe: None,
            },
            fl_core::At::from_unix_millis(1),
        );
        routes
            .save(Some(&f.root.path().join(".cache/agent-bus/routes.json")))
            .unwrap();
        // Legacy records may contain a bare persona alongside a valid cached UUID.
        let store = fl_store::open_store(&f.root.path().join("fl.fls")).unwrap();
        let mut record = fl_hub::lifecycle::list_hosted_sessions(&store)
            .unwrap()
            .remove(0);
        record.vendor_session_id = Some("ada".into());
        fl_hub::lifecycle::persist_hosted_session(&store, &record, "owner").unwrap();
    }
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(4);
        let mut requests = Vec::new();
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    let mut request = Vec::new();
                    let mut buf = [0; 1024];
                    loop {
                        let n = stream.read(&mut buf).unwrap();
                        assert!(n > 0 && request.len() < 8192);
                        request.extend_from_slice(&buf[..n]);
                        if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                            let headers = String::from_utf8_lossy(&request[..end]);
                            let len: usize = headers
                                .lines()
                                .find_map(|line| {
                                    let (name, value) = line.split_once(':')?;
                                    name.eq_ignore_ascii_case("content-length")
                                        .then(|| value.trim().parse().unwrap())
                                })
                                .unwrap();
                            if request.len() >= end + 4 + len {
                                break;
                            }
                        }
                    }
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                        )
                        .unwrap();
                    requests.push(String::from_utf8(request).unwrap());
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if done_rx.try_recv().is_ok() || Instant::now() >= deadline {
                        return requests;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(e) => panic!("mock server: {e}"),
            }
        }
    });
    let out = f.teardown("antigravity");
    done_tx.send(()).unwrap();
    let requests = server.join().unwrap();
    assert_eq!(out["runtime_stopped"], true);
    assert_eq!(
        requests.len(),
        1,
        "only the verified native cascade may be deleted: {requests:?}"
    );
    let request = &requests[0];
    assert!(request.contains("/DeleteCascadeTrajectory"));
    assert!(
        request
            .to_ascii_lowercase()
            .contains("x-codeium-csrf-token: fixture-token")
    );
    let body: serde_json::Value =
        serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(body["cascadeId"], cid);
    let entries = f.json(&["state", "list", "--concern", "runtime", "--json"], true);
    let record = &entries
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["slug"] == "hosted-session-antigravity-ada")
        .unwrap()["payload"]["runtime"]["value"];
    assert!(
        record["tombstone_ids"]
            .as_array()
            .unwrap()
            .iter()
            .any(|id| id == cid),
        "recovered identity must survive teardown in durable tombstones"
    );
}

/// Host IPC discovery uses the same total budget and rejects replacement streams and owners.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agy_cleanup_resolver_rechecks_host_owner_epoch_and_deadline() {
    use fl_host_proto::{
        ExtensionRequest as Req, ExtensionResponse as Resp, HostConfig, HostListener, HostRequest,
        HostResponse,
    };
    use fl_wire::vendor_adapter::antigravity_session::{
        AgyResolutionError, AgySessionRequest, resolve_agy_session,
    };
    for mode in ["foreign", "epoch", "pid", "stall"] {
        let (f, summary) = tokio::task::spawn_blocking(|| {
            let f = Fixture::new();
            f.raw_spawn("Ada.antigravity", Some("antigravity:ada"));
            let summary = f
                .rpc()
                .list_sessions()
                .unwrap()
                .into_iter()
                .find(|s| s.stream.session.as_str() == "Ada.antigravity")
                .unwrap();
            (f, summary)
        })
        .await
        .unwrap();
        let endpoint = HostEndpoint::unix(f.root.path().join("probe.sock")).unwrap();
        let listener = HostListener::bind(HostConfig::new(endpoint.clone()))
            .await
            .unwrap();
        let server = tokio::spawn(async move {
            let mut listener = listener;
            let mut conn = listener.accept().await.unwrap();
            let mut lists = 0;
            while let Ok(HostRequest::Extended { id, request }) = conn.recv().await {
                if mode == "stall" {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    break;
                }
                let response = match request {
                    Req::Capabilities => Resp::Capabilities {
                        host: summary.stream.host.clone(),
                        features: fl_host_proto::RELIABLE_FEATURE
                            | fl_host_proto::SESSION_OWNER_FEATURE,
                        limits: Default::default(),
                    },
                    Req::ListSessions => {
                        lists += 1;
                        let mut changed = summary.clone();
                        if lists > 1 {
                            if mode == "epoch" {
                                changed.stream.epoch.0 += 1;
                            }
                            if mode == "pid"
                                && let ChildState::Running(ref mut child) = changed.state
                            {
                                child.pid += 1;
                            }
                        }
                        Resp::Sessions {
                            records: vec![changed],
                        }
                    }
                    Req::CheckSessionOwner { .. } => Resp::SessionOwner {
                        matches: mode != "foreign",
                    },
                    unexpected => panic!("unexpected resolver request: {unexpected:?}"),
                };
                if conn
                    .respond(&HostResponse::Extended { id, response })
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        let elapsed = Instant::now();
        let result = tokio::task::spawn_blocking(move || {
            resolve_agy_session(
                &AgySessionRequest {
                    hosted_id: "antigravity:ada",
                    host_target: "Ada.antigravity",
                    known_cascade_id: Some("4df33135-e9a4-4f85-adfd-3b09dc339628"),
                    cached_route: None,
                    allow_host_probe: true,
                    host_endpoint: Some(&endpoint),
                },
                Instant::now() + Duration::from_millis(if mode == "stall" { 150 } else { 2000 }),
            )
        })
        .await
        .unwrap();
        let elapsed = elapsed.elapsed();
        server.abort();
        let _ = server.await;
        tokio::task::spawn_blocking(move || drop(f)).await.unwrap();
        assert_eq!(
            result,
            Err(if mode == "stall" {
                AgyResolutionError::Limit
            } else {
                AgyResolutionError::Host
            }),
            "{mode}"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "host stall must not use default 10s frame timeout"
        );
    }
}
