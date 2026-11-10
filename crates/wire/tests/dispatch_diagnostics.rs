//! Real vendor-dispatch boundary checks, isolated in subprocesses with fixture-only paths.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use fl_core::at::At;
use fl_core::ferrowire::{DeliveryStatus, MessageKind, MessageUrgency, SendWireArgs, WireAddress};
use fl_core::mem::MemStore;
use fl_core::store::MessageStore;
use fl_wire::dispatcher::dispatch_message_for_store;
use fl_wire::identity::{RouteTable, VendorRoute};
use tempfile::TempDir;

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"))
}

fn executable(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn fake_transport_route_refresh_preserves_legacy_dispatch_decisions() {
    for changed in [true, false] {
        let fixture = TempDir::new().unwrap();
        let home = fixture.path();
        let sessions = home.join(".local/share/muse/sessions");
        fs::create_dir_all(&sessions).unwrap();
        let transcript = sessions.join("session.jsonl");
        let call_log = home.join("calls");
        let before = serde_json::json!({
            "session_id": "before", "agentName": "diagnostic-recipient", "cwd": home,
        });
        let after = serde_json::json!({
            "session_id": "after", "agentName": "diagnostic-recipient", "cwd": home,
        });
        fs::write(&transcript, format!("{before}\n")).unwrap();
        let muse = home.join("fake-muse");
        executable(
            &muse,
            &format!(
                "#!/bin/sh\nset -eu\nprintf '%s\\n' \"$3\" >> {}\nif [ \"$3\" = before ]; then\n  {}\n  exit 1\nfi\n[ \"$3\" = after ]\n",
                shell_quote(&call_log),
                if changed {
                    // Copy a JSON fixture rather than interpolate its contents into shell code.
                    let replacement = home.join("replacement.jsonl");
                    fs::write(&replacement, format!("{after}\n")).unwrap();
                    format!(
                        "/bin/cp {} {}",
                        shell_quote(&replacement),
                        shell_quote(&transcript)
                    )
                } else {
                    ":".into()
                },
            ),
        );
        let tmux = home.join("fake-tmux");
        executable(&tmux, "#!/bin/sh\nexit 1\n");

        // The parent never mutates its environment. Each child has one test and a clean,
        // fixture-only environment, so it cannot race other tests' TMUX_ENV_LOCK guards.
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "fake_transport_fixture",
                "--ignored",
                "--nocapture",
            ])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", home)
            .env("XDG_CACHE_HOME", home.join(".cache"))
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env("FL_WIRE_MUSE_BIN", &muse)
            .env("FL_WIRE_TMUX_BIN", &tmux)
            .env("FL_WIRE_DISABLE_HOST_DISCOVERY", "1")
            .env("DIAGNOSTIC_FIXTURE_HOME", home)
            .env(
                "DIAGNOSTIC_FIXTURE_CHANGED",
                if changed { "1" } else { "0" },
            )
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "changed={changed}\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            fs::read_to_string(&call_log).unwrap(),
            if changed {
                "before\nafter\n"
            } else {
                "before\n"
            }
        );
    }
}

#[test]
#[ignore = "only run in the explicitly isolated fixture subprocess"]
fn fake_transport_fixture() {
    let home = std::env::var_os("DIAGNOSTIC_FIXTURE_HOME").expect("fixture subprocess only");
    let home = Path::new(&home);
    assert_eq!(std::env::var_os("HOME").unwrap(), home);
    let changed = std::env::var("DIAGNOSTIC_FIXTURE_CHANGED").unwrap() == "1";
    let store = MemStore::default();
    let (msg, initial) = store
        .send_wire(SendWireArgs {
            id: None,
            sender_id: None,
            sender_name: "synthetic-sender".into(),
            sender_host: None,
            to: WireAddress::AgentName("diagnostic-recipient".into()),
            kind: MessageKind::Mail,
            urgency: MessageUrgency::Normal,
            durable: false,
            subject: "fixture".into(),
            body: "fixture body".into(),
            reply_to: None,
            thread_id: None,
            deadline_at: None,
            at: At::from_unix_millis(10),
        })
        .unwrap();
    assert_eq!(initial.len(), 1);
    let mut routes = RouteTable::default();
    let updated = dispatch_message_for_store(
        &store,
        &mut routes,
        Some(home),
        &msg,
        At::from_unix_millis(20),
    );
    assert_eq!(updated.len(), 1);
    assert_eq!(
        updated[0].attempts, 1,
        "refresh must remain one receipt attempt"
    );
    assert_eq!(
        updated[0].status,
        if changed {
            DeliveryStatus::Acked
        } else {
            DeliveryStatus::Pending
        }
    );
    assert_eq!(store.list_wire_receipts(&msg.id).unwrap(), updated);
    assert!(matches!(&routes.get("diagnostic-recipient").unwrap().route,
        VendorRoute::Muse { session_id, .. } if session_id == if changed { "after" } else { "before" }));
}

use fl_core::agent::{AgentRecord, AreaRecord, RegisterAgentArgs, RegistryEvent};
use fl_core::ferrowire::{
    DeliveryReceipt, ThreadId, WireFilter, WireImportReport, WireInboxItem, WireMessage,
};
use fl_core::ids::{AgentId, MessageId};
use fl_core::store::{AgentRegistry, StoreError};
use fl_store::RedbStore;
use fl_wire::daemon::{DaemonConfig, LocalClient, LocalRequest, LocalResponse, spawn_daemon};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap()
}

#[derive(Clone, Copy, Default)]
enum Fault {
    #[default]
    Forward,
    Error,
}

struct FaultStore {
    inner: RedbStore,
    gets: Mutex<VecDeque<Fault>>,
    reads: Mutex<VecDeque<Fault>>,
    writes: Mutex<VecDeque<Fault>>,
    calls: Mutex<Vec<&'static str>>,
}

const SECRET_FAULT: &str = "ERROR_SECRET /private/token panic=PRIVATE_PAYLOAD";
impl FaultStore {
    fn fault(&self, queue: &Mutex<VecDeque<Fault>>) -> Fault {
        lock(queue).pop_front().unwrap_or_default()
    }
}
impl MessageStore for FaultStore {
    fn send_wire(
        &self,
        args: SendWireArgs,
    ) -> Result<(WireMessage, Vec<DeliveryReceipt>), StoreError> {
        lock(&self.calls).push("send");
        self.inner.send_wire(args)
    }
    fn get_wire_message(&self, id: &MessageId) -> Result<Option<WireMessage>, StoreError> {
        lock(&self.calls).push("get");
        match self.fault(&self.gets) {
            Fault::Error => Err(StoreError::Backend(SECRET_FAULT.into())),
            Fault::Forward => self.inner.get_wire_message(id),
        }
    }
    fn list_wire_receipts(&self, id: &MessageId) -> Result<Vec<DeliveryReceipt>, StoreError> {
        lock(&self.calls).push("receipts");
        match self.fault(&self.reads) {
            Fault::Error => Err(StoreError::Backend(SECRET_FAULT.into())),
            Fault::Forward => self.inner.list_wire_receipts(id),
        }
    }
    fn update_wire_receipt(
        &self,
        id: &MessageId,
        recipient: &str,
        status: DeliveryStatus,
        increment: bool,
        at: At,
    ) -> Result<Option<DeliveryReceipt>, StoreError> {
        lock(&self.calls).push("update");
        match self.fault(&self.writes) {
            Fault::Error => Err(StoreError::Backend(SECRET_FAULT.into())),
            Fault::Forward => self
                .inner
                .update_wire_receipt(id, recipient, status, increment, at),
        }
    }
    fn list_wire_inbox(&self, filter: &WireFilter) -> Result<Vec<WireInboxItem>, StoreError> {
        lock(&self.calls).push("inbox");
        self.inner.list_wire_inbox(filter)
    }
    fn list_wire_thread(&self, id: &ThreadId) -> Result<Vec<WireMessage>, StoreError> {
        lock(&self.calls).push("thread");
        self.inner.list_wire_thread(id)
    }
    fn list_wire_replies(&self, id: &MessageId) -> Result<Vec<WireMessage>, StoreError> {
        lock(&self.calls).push("replies");
        self.inner.list_wire_replies(id)
    }
    fn wire_messages_after(&self, seq: u64) -> Result<Vec<(u64, WireMessage)>, StoreError> {
        lock(&self.calls).push("messages_after");
        self.inner.wire_messages_after(seq)
    }
    fn wire_receipts_after(&self, seq: u64) -> Result<Vec<(u64, DeliveryReceipt)>, StoreError> {
        lock(&self.calls).push("receipts_after");
        self.inner.wire_receipts_after(seq)
    }
    fn import_wire_batch(
        &self,
        messages: &[WireMessage],
        receipts: &[DeliveryReceipt],
    ) -> Result<WireImportReport, StoreError> {
        lock(&self.calls).push("import");
        self.inner.import_wire_batch(messages, receipts)
    }
    fn prune_wire(&self, before: At) -> Result<usize, StoreError> {
        lock(&self.calls).push("prune");
        self.inner.prune_wire(before)
    }
}
impl AgentRegistry for FaultStore {
    fn upsert_area(&self, area: AreaRecord) -> Result<AreaRecord, StoreError> {
        self.inner.upsert_area(area)
    }
    fn get_area(&self, key: &str) -> Result<Option<AreaRecord>, StoreError> {
        self.inner.get_area(key)
    }
    fn list_areas(&self) -> Result<Vec<AreaRecord>, StoreError> {
        self.inner.list_areas()
    }
    fn delete_area(&self, key: &str, at: At, by: &str) -> Result<bool, StoreError> {
        self.inner.delete_area(key, at, by)
    }
    fn register_agent(&self, args: RegisterAgentArgs) -> Result<AgentRecord, StoreError> {
        self.inner.register_agent(args)
    }
    fn retire_agent(&self, id: &AgentId, at: At, by: &str) -> Result<AgentRecord, StoreError> {
        self.inner.retire_agent(id, at, by)
    }
    fn get_agent(&self, id: &AgentId) -> Result<Option<AgentRecord>, StoreError> {
        self.inner.get_agent(id)
    }
    fn agent_by_name(&self, key: &str) -> Result<Option<AgentRecord>, StoreError> {
        self.inner.agent_by_name(key)
    }
    fn list_agents(
        &self,
        project: Option<&str>,
        retired: bool,
    ) -> Result<Vec<AgentRecord>, StoreError> {
        self.inner.list_agents(project, retired)
    }
    fn registry_events_after(&self, seq: u64) -> Result<Vec<(u64, RegistryEvent)>, StoreError> {
        self.inner.registry_events_after(seq)
    }
    fn import_registry_events(&self, events: &[RegistryEvent]) -> Result<(), StoreError> {
        self.inner.import_registry_events(events)
    }
}

#[test]
fn successful_push_then_store_fault_exposes_status_without_resend() {
    for fault in ["write", "final_read"] {
        let fixture = TempDir::new().unwrap();
        let home = fixture.path();
        let muse = home.join("fake-muse");
        executable(
            &muse,
            &format!(
                "#!/bin/sh\nset -eu\nprintf 'push\\n' >> {}\n",
                shell_quote(&home.join("calls"))
            ),
        );
        let tmux = home.join("fake-tmux");
        executable(&tmux, "#!/bin/sh\nexit 1\n");
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "daemon_store_fault_fixture",
                "--ignored",
                "--nocapture",
            ])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", home)
            .env("XDG_RUNTIME_DIR", home)
            .env("XDG_CACHE_HOME", home.join("cache"))
            .env("XDG_CONFIG_HOME", home.join("config"))
            .env("XDG_DATA_HOME", home.join("data"))
            .env("FL_WIRE_ROUTES", home.join("routes.json"))
            .env("FL_WIRE_SOCKET", home.join("wire.sock"))
            .env("FL_WIRE_MUSE_BIN", muse)
            .env("FL_WIRE_TMUX_BIN", tmux)
            .env("FL_WIRE_DISABLE_HOST_DISCOVERY", "1")
            .env("DIAGNOSTIC_FIXTURE_HOME", home)
            .env("DIAGNOSTIC_STORE_FAULT", fault)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{fault}\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(fs::read_to_string(home.join("calls")).unwrap(), "push\n");
        let stderr = String::from_utf8(output.stderr).unwrap();
        let warning = stderr
            .lines()
            .find(|line| line.contains("dispatch hold: "))
            .unwrap();
        let (_, row) = warning.split_once("dispatch hold: ").unwrap();
        let row: serde_json::Value = serde_json::from_str(row).unwrap();
        assert_eq!(row["route_observations"][0]["outcome"], "returned_ok");
        for secret in [
            "ERROR_SECRET",
            "PRIVATE_PAYLOAD",
            "BODY_SECRET",
            "SUBJECT_SECRET",
            "SESSION_SECRET",
            "/private/token",
        ] {
            assert!(!warning.contains(secret), "{warning}");
        }
    }
}

struct FixtureClient(std::path::PathBuf);
impl FixtureClient {
    fn request(&mut self, request: &LocalRequest) -> Result<LocalResponse, String> {
        // Local RPC serves one request per connection.
        LocalClient::connect(&self.0, Duration::from_secs(3))
            .unwrap()
            .request(request)
    }
}

fn status(client: &mut FixtureClient) -> serde_json::Value {
    let LocalResponse::Status(status) = client
        .request(&LocalRequest::Status {
            include_codex_queues: false,
        })
        .unwrap()
    else {
        panic!("expected Status")
    };
    serde_json::to_value(status).unwrap()
}

#[test]
#[ignore = "only run in the explicitly isolated fixture subprocess"]
fn daemon_store_fault_fixture() {
    let home = std::env::var_os("DIAGNOSTIC_FIXTURE_HOME").expect("fixture subprocess only");
    let home = Path::new(&home);
    let fault = std::env::var("DIAGNOSTIC_STORE_FAULT").unwrap();
    let store = Arc::new(FaultStore {
        inner: RedbStore::open(&home.join("fault.redb")).unwrap(),
        gets: Mutex::new(VecDeque::new()),
        reads: Mutex::new(VecDeque::new()),
        writes: Mutex::new(VecDeque::new()),
        calls: Mutex::new(Vec::new()),
    });
    if fault == "write" {
        lock(&store.writes).push_back(Fault::Error);
    } else {
        lock(&store.reads).extend([Fault::Forward, Fault::Error]);
    }
    let mut daemon = spawn_daemon(
        Arc::clone(&store),
        DaemonConfig {
            node_id: "fixture-node".into(),
            host: "fixture-host".into(),
            socket_path: home.join("wire.sock"),
            routes_path: Some(home.join("routes.json")),
            home_override: Some(home.to_path_buf()),
            listen_addr: None,
            peers: vec![],
            psk: vec![],
        },
    )
    .unwrap();
    let mut client = FixtureClient(daemon.socket_path().to_path_buf());
    let initial = status(&mut client);
    assert_eq!(
        initial["dispatch_diagnostics"]["records"],
        serde_json::json!([])
    );
    assert!(matches!(
        client
            .request(&LocalRequest::RegisterRoute {
                persona: "diagnostic-recipient".into(),
                explicit: true,
                route: VendorRoute::Muse {
                    session_id: "SESSION_SECRET".into(),
                    cwd: Some(home.to_path_buf()),
                    tmux_target: None,
                    tmux_pane: None
                },
            })
            .unwrap(),
        LocalResponse::RouteRegistered
    ));
    let args = || SendWireArgs {
        id: None,
        sender_id: None,
        sender_name: "synthetic-sender".into(),
        sender_host: None,
        to: WireAddress::AgentName("diagnostic-recipient".into()),
        kind: MessageKind::Mail,
        urgency: MessageUrgency::Normal,
        durable: false,
        subject: "SUBJECT_SECRET".into(),
        body: "BODY_SECRET".into(),
        reply_to: None,
        thread_id: None,
        deadline_at: None,
        at: At::from_unix_millis(10),
    };
    let LocalResponse::Sent { message, .. } = client
        .request(&LocalRequest::Send { args: args() })
        .unwrap()
    else {
        panic!("expected sent")
    };
    let deadline = Instant::now() + Duration::from_secs(3);
    let first = loop {
        let value = status(&mut client);
        if value["dispatch_diagnostics"]["records"]
            .as_array()
            .unwrap()
            .len()
            == 1
        {
            break value["dispatch_diagnostics"].clone();
        }
        assert!(Instant::now() < deadline, "no fence observed");
        std::thread::sleep(Duration::from_millis(10));
    };
    let json = first.to_string();
    for secret in [
        "ERROR_SECRET",
        "PRIVATE_PAYLOAD",
        "BODY_SECRET",
        "SUBJECT_SECRET",
        "SESSION_SECRET",
        "/private/token",
    ] {
        assert!(!json.contains(secret), "{json}");
    }
    let row = &first["records"][0];
    assert_eq!(
        row["first_reason"],
        if fault == "write" {
            "receipt_write_error"
        } else {
            "final_receipt_read_error"
        }
    );
    assert_eq!(row["route_dispatch_calls"], 1);
    assert_eq!(row["route_observations"][0]["outcome"], "returned_ok");
    assert!(row["registration_generation"].is_null());
    let LocalResponse::Sent {
        message: later,
        receipts,
    } = client
        .request(&LocalRequest::Send { args: args() })
        .unwrap()
    else {
        panic!("expected sent")
    };
    assert_eq!(receipts[0].attempts, 0);
    assert_eq!(
        store.inner.list_wire_receipts(&later.id).unwrap()[0].attempts,
        0
    );
    client
        .request(&LocalRequest::Ack {
            message_ids: vec![message.id],
            recipient: "diagnostic-recipient".into(),
        })
        .unwrap();
    let before = lock(&store.calls).clone();
    for _ in 0..5 {
        assert_eq!(status(&mut client)["dispatch_diagnostics"], first);
    }
    assert_eq!(*lock(&store.calls), before);
    assert_eq!(fs::read_to_string(home.join("calls")).unwrap(), "push\n");
    daemon.stop();
}
