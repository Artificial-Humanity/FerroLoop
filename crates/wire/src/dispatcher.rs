//! Native vendor push dispatcher for Claude Code, Antigravity, Codex, and Webhook routes (spec §3.2).

use std::fs;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use fl_core::at::At;
use fl_core::ferrowire::{DeliveryReceipt, DeliveryStatus, WireMessage};
use fl_core::store::MessageStore;

use crate::identity::{RouteTable, VendorRoute, resolve_claude_key_path};

/// Maximum automatic push delivery attempts before leaving a message `Pending` for explicit poll.
pub const MAX_DELIVER: u32 = 5;

pub fn format_push_header(msg: &WireMessage) -> String {
    format!(
        "[fl-wire {}/{}] from {} (id: {}, thread: {}): {}",
        msg.kind.as_wire(),
        msg.urgency.as_wire(),
        msg.sender_name,
        msg.id,
        msg.thread_id,
        msg.subject
    )
}

pub fn format_push_message(msg: &WireMessage) -> String {
    format!("{}\n\n{}", format_push_header(msg), msg.body)
}

pub fn dispatch_to_route(route: &VendorRoute, msg: &WireMessage) -> Result<(), String> {
    match route {
        VendorRoute::Claude {
            pid,
            socket_path,
            key_path,
            ..
        } => {
            if *pid != 0 && !Path::new(&format!("/proc/{pid}")).exists() {
                return Err(format!("claude pid {pid} is no longer running"));
            }
            let resolved_key = key_path.clone().or_else(|| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .and_then(|h| {
                        resolve_claude_key_path(
                            &h.join(".claude").join("sessions"),
                            *pid,
                            socket_path,
                        )
                    })
            });
            let Some(kpath) = resolved_key else {
                return Err(format!(
                    "missing claude session key file for pid {pid} ({})",
                    socket_path.display()
                ));
            };
            let key_raw = fs::read_to_string(&kpath)
                .map_err(|e| format!("failed to read {}: {e}", kpath.display()))?;
            let key_json: serde_json::Value = serde_json::from_str(&key_raw)
                .map_err(|e| format!("invalid JSON in {}: {e}", kpath.display()))?;
            let peer_token = key_json
                .get("peerToken")
                .and_then(|v| v.as_str())
                .ok_or_else(|| format!("missing `peerToken` in {}", kpath.display()))?;

            let mut stream = UnixStream::connect(socket_path).map_err(|e| {
                format!(
                    "failed to connect to claude socket {}: {e}",
                    socket_path.display()
                )
            })?;
            let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));

            let auth_frame = serde_json::json!({
                "type": "auth",
                "token": peer_token,
            });
            let user_frame = serde_json::json!({
                "type": "user",
                "from": msg.sender_name,
                "priority": "next",
                "message": {
                    "role": "user",
                    "content": format_push_message(msg),
                }
            });
            let payload = format!("{auth_frame}\n{user_frame}\n");
            stream
                .write_all(payload.as_bytes())
                .and_then(|()| stream.flush())
                .map_err(|e| format!("failed to write to claude socket: {e}"))?;
            Ok(())
        }
        VendorRoute::Agy {
            conversation_id,
            ls_address,
            csrf_token,
            agentapi_exe,
            ..
        } => {
            let exe = agentapi_exe
                .clone()
                .or_else(|| std::env::var_os("ANTIGRAVITY_AGENTAPI_EXE").map(PathBuf::from))
                .or_else(|| {
                    std::env::var_os("HOME")
                        .map(PathBuf::from)
                        .map(|h| h.join(".local").join("bin").join("agy"))
                })
                .unwrap_or_else(|| PathBuf::from("agy"));
            let addr = ls_address
                .clone()
                .or_else(|| std::env::var("ANTIGRAVITY_LS_ADDRESS").ok())
                .unwrap_or_default();
            let csrf = csrf_token
                .clone()
                .or_else(|| std::env::var("ANTIGRAVITY_CSRF_TOKEN").ok())
                .unwrap_or_default();

            let title = format!("--title={}", format_push_header(msg));
            let mut cmd = Command::new(&exe);
            cmd.args(["agentapi", "send-message", &title, conversation_id, &msg.body]);
            cmd.env("ANTIGRAVITY_CONVERSATION_ID", conversation_id);
            cmd.env("ANTIGRAVITY_AGENTAPI_EXE", &exe);
            if !addr.is_empty() {
                cmd.env("ANTIGRAVITY_LS_ADDRESS", &addr);
            }
            if !csrf.is_empty() {
                cmd.env("ANTIGRAVITY_CSRF_TOKEN", &csrf);
            }
            let status = cmd
                .status()
                .map_err(|e| format!("failed to spawn {}: {e}", exe.display()))?;
            if !status.success() {
                return Err(format!("agy send-message exited with {status}"));
            }
            Ok(())
        }
        VendorRoute::Codex { thread_id, cwd } => {
            let mut cmd = Command::new("codex");
            cmd.args([
                "queue",
                "--thread",
                thread_id,
                "--message",
                &format_push_message(msg),
            ]);
            if let Some(dir) = cwd {
                cmd.current_dir(dir);
            }
            let status = cmd
                .status()
                .map_err(|e| format!("failed to spawn codex queue: {e}"))?;
            if !status.success() {
                return Err(format!("codex queue exited with {status}"));
            }
            Ok(())
        }
        VendorRoute::Muse { session_id, cwd } => {
            let bin = std::env::var_os("FL_WIRE_MUSE_BIN")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("muse"));
            let formatted = format_push_message(msg);
            let mut cmd = Command::new(&bin);
            cmd.env("MUSE_NO_AUTO_UPDATE", "1");
            cmd.args(["exec", "--session-id", session_id, "--json", &formatted]);
            if let Some(dir) = cwd
                && dir.exists()
            {
                cmd.current_dir(dir);
            }
            let status = cmd
                .status()
                .map_err(|e| format!("failed to spawn {}: {e}", bin.display()))?;
            if !status.success() {
                return Err(format!("muse exec exited with {status}"));
            }
            Ok(())
        }
        VendorRoute::Grok { session_id, cwd } => {
            let bin = std::env::var_os("FL_WIRE_GROK_BIN")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("grok"));
            let formatted = format_push_message(msg);
            let mut cmd = Command::new(&bin);
            cmd.args([
                "--no-auto-update",
                "--no-alt-screen",
                "--always-approve",
                "-s",
                session_id,
                "-p",
                &formatted,
            ]);
            if let Some(dir) = cwd
                && dir.exists()
            {
                cmd.current_dir(dir);
            }
            let status = cmd
                .status()
                .map_err(|e| format!("failed to spawn {}: {e}", bin.display()))?;
            if !status.success() {
                return Err(format!("grok exited with {status}"));
            }
            Ok(())
        }
        VendorRoute::Webhook { socket_path } => {
            let mut stream = UnixStream::connect(socket_path).map_err(|e| {
                format!(
                    "failed to connect to webhook socket {}: {e}",
                    socket_path.display()
                )
            })?;
            let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
            let mut line =
                serde_json::to_string(msg).map_err(|e| format!("serialize message: {e}"))?;
            line.push('\n');
            stream
                .write_all(line.as_bytes())
                .and_then(|()| stream.flush())
                .map_err(|e| format!("failed to write to webhook socket: {e}"))?;
            Ok(())
        }
    }
}

/// Attempt push delivery for all pending recipients of `msg` on this host,
/// updating `store` receipts (`Acked` on verified push, `Pending` with `attempts + 1` on failure)
/// and returning any modified `DeliveryReceipt`s.
pub fn dispatch_message_for_store<S: MessageStore>(
    store: &S,
    routes: &mut RouteTable,
    home_override: Option<&Path>,
    msg: &WireMessage,
    now: At,
) -> Vec<DeliveryReceipt> {
    routes.discover_routes(home_override, now.clone());
    let Ok(receipts) = store.list_wire_receipts(&msg.id) else {
        return Vec::new();
    };
    let mut updated = Vec::new();
    for rcpt in receipts {
        if matches!(rcpt.status, DeliveryStatus::Acked | DeliveryStatus::Expired) {
            continue;
        }
        if rcpt.attempts >= MAX_DELIVER {
            continue;
        }
        let Some(entry) = routes.get(&rcpt.recipient_name).cloned() else {
            continue;
        };
        let target_status = match dispatch_to_route(&entry.route, msg) {
            Ok(()) => DeliveryStatus::Acked,
            Err(_) => DeliveryStatus::Pending,
        };
        if let Ok(Some(new_rcpt)) = store.update_wire_receipt(
            &msg.id,
            &rcpt.recipient_name,
            target_status,
            true,
            now.clone(),
        ) {
            updated.push(new_rcpt);
        }
    }
    updated
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::ferrowire::{MessageKind, MessageUrgency, SendWireArgs, WireAddress};
    use fl_core::mem::MemStore;
    use std::io::BufRead;
    use std::os::unix::net::UnixListener;
    use tempfile::TempDir;

    #[test]
    fn claude_socket_dispatch_sends_auth_and_user_frame_and_acks_receipt() {
        let tmp = TempDir::new().unwrap();
        let sock_path = tmp.path().join("claude.sock");
        let key_path = tmp.path().join("0.key");
        fs::write(&key_path, "{\"peerToken\":\"tok-xyz\"}").unwrap();

        let listener = UnixListener::bind(&sock_path).unwrap();
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let reader = std::io::BufReader::new(stream);
            reader
                .lines()
                .take(2)
                .map(|l| l.unwrap())
                .collect::<Vec<_>>()
        });

        let store = MemStore::default();
        let (msg, _) = store
            .send_wire(SendWireArgs {
                id: None,
                sender_id: None,
                sender_name: "jarvis".into(),
                sender_host: None,
                to: WireAddress::AgentName("sonya".into()),
                kind: MessageKind::Mail,
                urgency: MessageUrgency::Urgent,
                durable: false,
                subject: "ping".into(),
                body: "hello sonya".into(),
                reply_to: None,
                thread_id: None,
                deadline_at: None,
                at: At::from_unix_millis(10),
            })
            .unwrap();

        let mut routes = RouteTable::default();
        routes.upsert(
            "sonya",
            VendorRoute::Claude {
                pid: 0,
                session_id: "s1".into(),
                socket_path: sock_path,
                key_path: Some(key_path),
            },
            At::from_unix_millis(10),
        );

        let updated =
            dispatch_message_for_store(&store, &mut routes, Some(tmp.path()), &msg, At::from_unix_millis(20));
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0].status, DeliveryStatus::Acked);
        assert_eq!(updated[0].attempts, 1);

        let lines = handle.join().unwrap();
        let auth: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(auth["type"], "auth");
        assert_eq!(auth["token"], "tok-xyz");
        let user: serde_json::Value = serde_json::from_str(&lines[1]).unwrap();
        assert_eq!(user["type"], "user");
        assert_eq!(user["from"], "jarvis");
        assert!(user["message"]["content"]
            .as_str()
            .unwrap()
            .contains("hello sonya"));
    }

    #[test]
    fn failed_push_increments_attempts_and_stops_at_max_deliver() {
        let tmp = TempDir::new().unwrap();
        let dead_sock = tmp.path().join("dead.sock");

        let store = MemStore::default();
        let (msg, _) = store
            .send_wire(SendWireArgs {
                id: None,
                sender_id: None,
                sender_name: "jarvis".into(),
                sender_host: None,
                to: WireAddress::AgentName("kayla".into()),
                kind: MessageKind::Mail,
                urgency: MessageUrgency::Normal,
                durable: false,
                subject: "retry test".into(),
                body: "body".into(),
                reply_to: None,
                thread_id: None,
                deadline_at: None,
                at: At::from_unix_millis(10),
            })
            .unwrap();

        let mut routes = RouteTable::default();
        routes.upsert(
            "kayla",
            VendorRoute::Webhook {
                socket_path: dead_sock,
            },
            At::from_unix_millis(10),
        );

        for i in 1..=7 {
            let _ = dispatch_message_for_store(
                &store,
                &mut routes,
                Some(tmp.path()),
                &msg,
                At::from_unix_millis(10 + i),
            );
        }

        let rcpts = store.list_wire_receipts(&msg.id).unwrap();
        assert_eq!(rcpts[0].status, DeliveryStatus::Pending);
        assert_eq!(rcpts[0].attempts, MAX_DELIVER);
    }

    #[test]
    fn muse_and_grok_dispatch_invokes_configured_binaries_with_expected_args() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = TempDir::new().unwrap();
        let muse_out = tmp.path().join("muse-args.txt");
        let muse_bin = tmp.path().join("mock-muse.sh");
        fs::write(
            &muse_bin,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$MUSE_NO_AUTO_UPDATE\" \"$@\" > \"{}\"\n",
                muse_out.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&muse_bin, fs::Permissions::from_mode(0o755)).unwrap();

        let grok_out = tmp.path().join("grok-args.txt");
        let grok_bin = tmp.path().join("mock-grok.sh");
        fs::write(
            &grok_bin,
            format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > \"{}\"\n", grok_out.display()),
        )
        .unwrap();
        fs::set_permissions(&grok_bin, fs::Permissions::from_mode(0o755)).unwrap();

        // SAFETY: only this test in fl-wire reads or writes FL_WIRE_MUSE_BIN / FL_WIRE_GROK_BIN.
        unsafe {
            std::env::set_var("FL_WIRE_MUSE_BIN", &muse_bin);
            std::env::set_var("FL_WIRE_GROK_BIN", &grok_bin);
        }

        let store = MemStore::default();
        let (msg, _) = store
            .send_wire(SendWireArgs {
                id: None,
                sender_id: None,
                sender_name: "jarvis".into(),
                sender_host: None,
                to: WireAddress::AgentName("mira".into()),
                kind: MessageKind::Mail,
                urgency: MessageUrgency::Normal,
                durable: false,
                subject: "muse check".into(),
                body: "hello muse and grok".into(),
                reply_to: None,
                thread_id: None,
                deadline_at: None,
                at: At::from_unix_millis(10),
            })
            .unwrap();

        dispatch_to_route(
            &VendorRoute::Muse {
                session_id: "ms-42".into(),
                cwd: Some(tmp.path().to_path_buf()),
            },
            &msg,
        )
        .unwrap();
        let muse_lines: Vec<String> = fs::read_to_string(&muse_out)
            .unwrap()
            .lines()
            .map(ToOwned::to_owned)
            .collect();
        assert_eq!(muse_lines[0], "1");
        assert_eq!(muse_lines[1], "exec");
        assert_eq!(muse_lines[2], "--session-id");
        assert_eq!(muse_lines[3], "ms-42");
        assert_eq!(muse_lines[4], "--json");
        assert!(muse_lines[5].contains("muse check"));

        dispatch_to_route(
            &VendorRoute::Grok {
                session_id: "gs-99".into(),
                cwd: Some(tmp.path().to_path_buf()),
            },
            &msg,
        )
        .unwrap();
        let grok_lines: Vec<String> = fs::read_to_string(&grok_out)
            .unwrap()
            .lines()
            .map(ToOwned::to_owned)
            .collect();
        assert_eq!(grok_lines[0], "--no-auto-update");
        assert_eq!(grok_lines[1], "--no-alt-screen");
        assert_eq!(grok_lines[2], "--always-approve");
        assert_eq!(grok_lines[3], "-s");
        assert_eq!(grok_lines[4], "gs-99");
        assert_eq!(grok_lines[5], "-p");
        assert!(grok_lines[6].contains("muse check"));
    }
}
