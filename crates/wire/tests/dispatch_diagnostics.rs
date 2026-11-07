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
