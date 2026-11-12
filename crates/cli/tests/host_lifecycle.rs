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

/// These children have the prompt in argv but cannot consume it until the test releases them.
/// A dialog waits for input; delayed startup waits for a separate initialization gate.
fn hydration_waiting_child_stays_unconfirmed(dialog: bool) {
    let f = Fixture::new();
    let gate = if dialog {
        "printf 'Trust this workspace? [y/N]\\n'; read answer; [ \"$answer\" = y ] || exit 1"
    } else {
        "printf 'Initializing vendor runtime...\\n'; while [ ! -f \"$root/release\" ]; do sleep 0.01; done"
    };
    std::fs::write(
        f.root.path().join("vendor"),
        format!(
            "#!/bin/sh\nroot=$(dirname \"$0\")\nprintf 'launch\\n' >> \"$root/handoffs\"\nprintf '%s\\n' \"$@\" > \"$root/arguments\"\n{gate}\nprintf 'consumed\\n' > \"$root/consumed\"\nexec /bin/cat\n"
        ),
    ).unwrap();
    let out = f.spin("claude", false);
    let deadline = Instant::now() + Duration::from_secs(5);
    let marker = if dialog {
        "Trust this workspace?"
    } else {
        "Initializing vendor runtime..."
    };
    loop {
        let screen = f
            .rpc()
            .capture_output(SessionId::new("Ada.claude").unwrap(), 65536)
            .unwrap();
        if screen.contains(marker) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "child did not reach its gate: {screen:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(f.running("Ada.claude"));
    assert!(
        !f.root.path().join("consumed").exists(),
        "child must still be blocked"
    );
    assert_eq!(
        std::fs::read_to_string(f.root.path().join("handoffs")).unwrap(),
        "launch\n"
    );
    assert!(
        std::fs::read_to_string(f.root.path().join("arguments"))
            .unwrap()
            .contains(out["prompt"].as_str().unwrap())
    );
    assert_eq!(out["hydration_dispatched"], true);
    assert_eq!(out["hydration_status"], "unconfirmed");
    if dialog {
        let mut rpc = f.rpc();
        let stream = rpc
            .list_sessions()
            .unwrap()
            .into_iter()
            .find(|s| s.stream.session.as_str() == "Ada.claude")
            .unwrap()
            .stream;
        let grant = rpc.claim_controller(stream.clone()).unwrap();
        rpc.send_line(stream, grant.lease, "y".into()).unwrap();
    } else {
        std::fs::write(f.root.path().join("release"), "ready").unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while !f.root.path().join("consumed").exists() {
        assert!(
            Instant::now() < deadline,
            "child did not finish initialization"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let records = f.json(&["state", "list", "--concern", "runtime", "--json"], false);
    let record = records
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["slug"] == "hosted-session-claude-ada")
        .unwrap();
    assert_eq!(
        record["payload"]["runtime"]["value"]["hydration_status"],
        "unconfirmed"
    );
    assert_eq!(
        std::fs::read_to_string(f.root.path().join("handoffs")).unwrap(),
        "launch\n"
    );
}

#[test]
fn hydration_status_live_dialog_is_unconfirmed_without_a_second_handoff() {
    hydration_waiting_child_stays_unconfirmed(true);
}

#[test]
fn hydration_status_delayed_start_remains_unconfirmed_after_initialization() {
    hydration_waiting_child_stays_unconfirmed(false);
}

#[test]
fn claude_resume_and_child_env_isolation_prevent_session_start_hook_from_hijacking_foreign_persona()
{
    let f = Fixture::new();
    let fl_bin = env!("CARGO_BIN_EXE_fl");
    let routes_path = f.root.path().join(".cache/agent-bus/routes.json");
    let ada_sock = f.root.path().join("42424.sock");
    let db_path = f.root.path().join("fl.fls");
    std::fs::write(
        f.root.path().join("vendor"),
        format!(
            "#!/bin/sh\n\
             root=$(dirname \"$0\")\n\
             printf '%s\\n' \"$@\" > \"$root/arguments\"\n\
             printf 'AGENT_PERSONA=%s\\nAGENT_NAME=%s\\nFL_HOSTED_ID=%s\\nAGENT_HYDRATION_GENERATION=%s\\n' \\\n\
               \"${{AGENT_PERSONA:-<unset>}}\" \"${{AGENT_NAME:-<unset>}}\" \"${{FL_HOSTED_ID:-<unset>}}\" \"${{AGENT_HYDRATION_GENERATION:-<unset>}}\" \\\n\
               > \"$root/child_env\"\n\
             case \" $* \" in\n\
               *\" --resume \"*)\n\
                 if [ -f \"$root/hook_payload.json\" ]; then\n\
                   FL_WIRE_ROUTES=\"{}\" CLAUDE_CODE_MESSAGING_SOCKET=\"{}\" \"{}\" --db \"{}\" wire hook claude < \"$root/hook_payload.json\" > \"$root/hook_stdout\" 2> \"$root/hook_stderr\"\n\
                   echo $? > \"$root/hook_status\"\n\
                 fi\n\
                 ;;\n\
             esac\n\
             exec /bin/cat\n",
            routes_path.display(),
            ada_sock.display(),
            fl_bin,
            db_path.display()
        ),
    )
    .unwrap();

    // 1. Spin up both `feira` (antigravity, no_spawn) and `ada` (claude, spawned on private fl-host
    //    while parent caller environment is polluted with foreign `feira` identity vars).
    f.json(
        &[
            "agent",
            "spin-up",
            "feira",
            "--project",
            "FerroLoop",
            "--role",
            "lead",
            "--vendor",
            "antigravity",
            "--by",
            "owner",
            "--json",
        ],
        true,
    );
    let mut spin_cmd = f.command();
    spin_cmd
        .env("AGENT_PERSONA", "feira")
        .env("AGENT_NAME", "feira")
        .env("FL_HOSTED_ID", "antigravity:feira")
        .env(
            "AGENT_HYDRATION_GENERATION",
            "01900000-0000-7000-8000-000000000099",
        )
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
        ]);
    let spin_out = spin_cmd.output().unwrap();
    assert!(
        spin_out.status.success(),
        "spin-up failed: {}",
        String::from_utf8_lossy(&spin_out.stderr)
    );
    let spun_ada: Value = serde_json::from_slice(&spin_out.stdout).unwrap();
    assert_eq!(spun_ada["runtime_spawned"], true);
    let ada_gen = spun_ada["hydration_generation"]
        .as_str()
        .expect("ada hydration_generation")
        .to_string();
    assert_ne!(
        ada_gen, "01900000-0000-7000-8000-000000000099",
        "spin_up must mint a fresh hydration_generation for ada"
    );

    let child_env_path = f.root.path().join("child_env");
    let wait_for_file = |path: &std::path::Path| {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !path.exists() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {}",
                path.display()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    wait_for_file(&child_env_path);
    let spin_child_env = std::fs::read_to_string(&child_env_path).unwrap();
    assert_eq!(
        spin_child_env,
        format!(
            "AGENT_PERSONA=ada\nAGENT_NAME=<unset>\nFL_HOSTED_ID=claude:ada\nAGENT_HYDRATION_GENERATION={ada_gen}\n"
        ),
        "spawned child process on spin_up must receive ada's identity and never polluted parent feira vars"
    );

    let ada_sid = {
        let store = fl_store::open_store(&db_path).unwrap();
        let rec = fl_hub::lifecycle::find_hosted_session(&store, &["claude:ada"])
            .unwrap()
            .unwrap();
        let sid = rec.vendor_session_id.clone().unwrap();
        assert!(
            fl_hub::lifecycle::record_hydration_ack(
                &store,
                "ada",
                true,
                Some(&ada_gen),
                Some("claude:ada"),
            )
            .unwrap()
        );
        sid
    };

    // Write active transcript `<ada_sid>.jsonl` under `<home>/.claude/projects/ferroloop/`.
    let proj_dir = f.root.path().join(".claude/projects/ferroloop");
    std::fs::create_dir_all(&proj_dir).unwrap();
    std::fs::write(
        proj_dir.join(format!("{ada_sid}.jsonl")),
        format!(
            "{{\"type\":\"user\",\"sessionId\":\"{ada_sid}\",\"uuid\":\"u1\",\"message\":{{\"role\":\"user\",\"content\":\"hi\"}}}}\n\
             {{\"type\":\"assistant\",\"sessionId\":\"{ada_sid}\",\"uuid\":\"a1\",\"message\":{{\"model\":\"claude-opus-5-5\",\"role\":\"assistant\",\"stop_reason\":\"end_turn\",\"usage\":{{\"input_tokens\":2,\"cache_creation_input_tokens\":10,\"cache_read_input_tokens\":100}},\"content\":[{{\"type\":\"text\",\"text\":\"ready\"}}]}}}}\n\
             {{\"type\":\"bridge-session\",\"sessionId\":\"{ada_sid}\",\"bridgeSessionId\":\"cse_01SP4ortBQU3hQGNi4NtM8pN\"}}\n"
        ),
    )
    .unwrap();

    // Seed `feira`'s route in routes.json and write hook_payload.json before restarting `ada`.
    let feira_route = fl_wire::identity::VendorRoute::Agy {
        conversation_id: "019a0000-0000-7000-8000-000000000111".into(),
        agentapi_exe: None,
        ls_pid: None,
        ls_address: Some("127.0.0.1:42100".into()),
        csrf_token: Some("tok".into()),
    };
    let mut routes = fl_wire::identity::RouteTable::default();
    routes.upsert(
        "feira",
        feira_route.clone(),
        fl_core::At::from_unix_millis(100),
    );
    routes.save(Some(&routes_path)).unwrap();

    let hook_payload = serde_json::json!({
        "hook_event_name": "SessionStart",
        "source": "resume",
        "session_id": ada_sid,
        "cwd": f.root.path(),
    });
    std::fs::write(
        f.root.path().join("hook_payload.json"),
        hook_payload.to_string(),
    )
    .unwrap();

    // 2. Kill `Ada.claude` on private fl-host and restart `claude:ada` via `restart_hosted_session`
    //    while parent caller environment is polluted with `feira` identity vars ->
    //    must launch with `--resume <ada_sid>`, preserve `ada_sid` and `ada_gen`, and pass clean
    //    `ada` identity env vars to the spawned child process.
    {
        let mut rpc = f.rpc();
        let stream = rpc
            .list_sessions()
            .unwrap()
            .into_iter()
            .find(|s| s.stream.session.as_str() == "Ada.claude")
            .unwrap()
            .stream;
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
    }
    let _ = std::fs::remove_file(&child_env_path);

    let out = Command::new(std::env::current_exe().unwrap())
        .env_clear()
        .env("HOME", f.root.path())
        .env("PATH", "/usr/bin:/bin")
        .env("XDG_RUNTIME_DIR", f.root.path())
        .env("XDG_CONFIG_HOME", f.root.path().join("config"))
        .env("XDG_DATA_HOME", f.root.path().join("data"))
        .env("FL_HOST_SOCK", f.endpoint.socket_path())
        .env("FL_WIRE_DISABLE_HOST_DISCOVERY", "1")
        .env("FL_WIRE_CLAUDE_BIN", f.root.path().join("vendor"))
        .env("F433_RESTART_CHILD", f.root.path())
        .env("AGENT_NAME", "feira")
        .env("AGENT_PERSONA", "feira")
        .env("FL_HOSTED_ID", "antigravity:feira")
        .env(
            "AGENT_HYDRATION_GENERATION",
            "01900000-0000-7000-8000-000000000099",
        )
        .args(["--exact", "hub_restart_claude_resume_child", "--nocapture"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(f.running("Ada.claude"));

    let hook_status_path = f.root.path().join("hook_status");
    wait_for_file(&child_env_path);
    wait_for_file(&hook_status_path);

    let restart_child_env = std::fs::read_to_string(&child_env_path).unwrap();
    assert_eq!(
        restart_child_env,
        format!(
            "AGENT_PERSONA=ada\nAGENT_NAME=<unset>\nFL_HOSTED_ID=claude:ada\nAGENT_HYDRATION_GENERATION={ada_gen}\n"
        ),
        "spawned child process on restart_hosted_session must receive ada's identity and never polluted parent feira vars"
    );
    assert_eq!(
        std::fs::read_to_string(&hook_status_path).unwrap().trim(),
        "0",
        "SessionStart:resume hook inside spawned child failed: {}",
        std::fs::read_to_string(f.root.path().join("hook_stderr")).unwrap_or_default()
    );

    let args_after = std::fs::read_to_string(f.root.path().join("arguments")).unwrap();
    let arg_lines: Vec<&str> = args_after.lines().collect();
    assert!(
        arg_lines
            .windows(2)
            .any(|w| w == ["--resume", ada_sid.as_str()]),
        "expected --resume {ada_sid} in spawned arguments: {arg_lines:?}"
    );
    assert!(
        !arg_lines.contains(&"--session-id"),
        "--resume spawn must not pass --session-id: {arg_lines:?}"
    );

    // Prove `routes.json` updated `ada` to `<ada_sid>` and left `feira`'s route untouched!
    let after_routes = fl_wire::identity::RouteTable::load(Some(&routes_path));
    assert_eq!(
        after_routes.get("feira").map(|e| &e.route),
        Some(&feira_route),
        "feira's route must never be hijacked by ada's resumed session hook"
    );
    match after_routes.get("ada").map(|e| &e.route) {
        Some(fl_wire::identity::VendorRoute::Claude { session_id, .. }) => {
            assert_eq!(session_id, &ada_sid);
        }
        other => panic!("expected Claude route for ada with {ada_sid}, got {other:?}"),
    }
}

#[test]
fn hub_restart_claude_resume_child() {
    use fl_hub::lifecycle;
    let Some(root) = std::env::var_os("F433_RESTART_CHILD").map(std::path::PathBuf::from) else {
        return;
    };
    let store = fl_store::open_store(&root.join("fl.fls")).unwrap();
    let mut env = lifecycle::LifecycleEnv::new(Some(&root));
    env.spawn_runtime = true;
    let restarted = lifecycle::restart_hosted_session(&store, &env, "claude:ada", "owner").unwrap();
    assert!(restarted.record.runtime_spawned);
    assert_eq!(
        restarted.record.hydration_status,
        lifecycle::HydrationStatus::Confirmed
    );
}

/// Exercise actual private fl-host launch and a live-shaped hydration command for every adapter.
/// Codex tools instead run under a separate app-server fixture, just like its remote TUI.
#[test]
#[cfg(target_os = "linux")]
fn real_host_live_vendor_hydration_register_confirms_and_routes() {
    use fl_wire::identity::RouteTable;
    use std::os::unix::process::CommandExt;

    for (vendor, bin_name, bin_env, session_env) in [
        (
            "claude",
            "claude",
            "FL_WIRE_CLAUDE_BIN",
            "CLAUDE_CODE_MESSAGING_SOCKET",
        ),
        ("codex", "codex", "FL_WIRE_CODEX_BIN", "CODEX_THREAD_ID"),
        ("muse", "muse", "FL_WIRE_MUSE_BIN", "MUSE_SESSION_ID"),
        (
            "antigravity",
            "agy",
            "FL_WIRE_AGY_BIN",
            "ANTIGRAVITY_CONVERSATION_ID",
        ),
        ("grok", "grok", "FL_WIRE_GROK_BIN", "GROK_SESSION_ID"),
    ] {
        let f = Fixture::new();
        let root = f.root.path();
        let binary = root.join(bin_name);
        let routes = root.join("routes.json");
        let db = root.join("fl.fls");
        let script = format!(
            r##"#!/bin/sh
root='{root}'
if [ '{vendor}' = codex ] && [ "$1" != --fixture-app-server ]; then /bin/cat; exit; fi
while [ ! -f "$root/go" ]; do sleep 0.05; done
export HOME="$root" XDG_RUNTIME_DIR="$root" XDG_CONFIG_HOME="$root/config" XDG_DATA_HOME="$root/data"
export FL_HOST_SOCK="$root/h.sock" FL_WIRE_ROUTES="$root/routes.json" FL_WIRE_DISABLE_HOST_DISCOVERY=1
export AGENT_PERSONA=ada AGENT_HYDRATION_GENERATION="$(cat "$root/go")"
unset FL_HOSTED_ID CLAUDE_CODE_MESSAGING_SOCKET CLAUDE_SESSION_ID CODEX_THREAD_ID MUSE_SESSION_ID GROK_SESSION_ID ANTIGRAVITY_CONVERSATION_ID ANTIGRAVITY_AGENT
if [ '{vendor}' = claude ]; then export CLAUDE_CODE_MESSAGING_SOCKET="$root/$$.sock"; else export {session_env}=live-session-id; fi
'{fl}' --db "$root/fl.fls" wire register Ada --project FerroLoop --role assistant --replace >"$root/register.out" 2>"$root/register.err"
echo "$?" >"$root/register.status"
exec /bin/cat
"##,
            root = root.display(),
            fl = env!("CARGO_BIN_EXE_fl")
        );
        std::fs::write(&binary, script).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        if vendor == "codex" {
            let codex_home = root.join("codex-home");
            std::fs::create_dir_all(&codex_home).unwrap();
            std::fs::write(
                codex_home.join("config.toml"),
                format!(
                    "[projects.\"{}\"]\ntrust_level = \"trusted\"\n",
                    root.display()
                ),
            )
            .unwrap();
        }
        let mut spin = f.command();
        spin.env(bin_env, &binary)
            .env("CODEX_HOME", root.join("codex-home"))
            .env("FL_WIRE_ROUTES", &routes)
            .args([
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
            ]);
        let output = spin.output().unwrap();
        assert!(
            output.status.success(),
            "{vendor} spin-up: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let out: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(out["runtime_spawned"], true, "{vendor}");
        let generation = out["hydration_generation"].as_str().unwrap();
        let records = f.json(&["state", "list", "--concern", "runtime", "--json"], false);
        let slug = format!("hosted-session-{vendor}-ada");
        let record = records
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["slug"] == slug)
            .unwrap();
        let pid = record["payload"]["runtime"]["value"]["pid"]
            .as_u64()
            .unwrap() as u32;
        assert!(
            fl_wire::identity::proc_is_vendor_cli(pid),
            "{vendor}: recorded PID {pid} is not the launched CLI"
        );
        let mut app_server = (vendor == "codex").then(|| {
            Command::new(&binary)
                .arg("--fixture-app-server")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .process_group(0)
                .spawn()
                .unwrap()
        });
        std::fs::write(root.join("go"), generation).unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        while !root.join("register.status").exists() {
            assert!(
                Instant::now() < deadline,
                "{vendor}: hydration register timed out"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let status = std::fs::read_to_string(root.join("register.status")).unwrap();
        if let Some(ref mut server) = app_server {
            unsafe {
                libc::kill(-(server.id() as i32), libc::SIGKILL);
            }
            let _ = server.wait();
        }
        assert_eq!(
            status.trim(),
            "0",
            "{vendor}: {}",
            std::fs::read_to_string(root.join("register.err")).unwrap_or_default()
        );
        let store = fl_store::open_store(&db).unwrap();
        let session = fl_hub::lifecycle::find_hosted_session(&store, &[&format!("{vendor}:ada")])
            .unwrap()
            .unwrap();
        assert_eq!(
            session.hydration_status,
            fl_hub::lifecycle::HydrationStatus::Confirmed,
            "{vendor}"
        );
        let table = RouteTable::load(Some(&routes));
        assert_eq!(table.get("ada").unwrap().route.canonical_vendor(), vendor);
    }
}
