//! Native vendor push dispatcher for Claude Code, Antigravity, Codex, and Webhook routes (spec §3.2).

use std::fs;
use std::io::{BufRead, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use fl_core::at::At;
use fl_core::ferrowire::{DeliveryReceipt, DeliveryStatus, WireMessage};
use fl_core::store::MessageStore;

use crate::identity::{RouteTable, VendorRoute, resolve_agy_exe, resolve_claude_key_path};

/// `clear` sends the vendor reset command only. `reforge` sends that command
/// and then the canonical hydration prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionResetMode {
    Clear,
    Reforge,
}

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
                std::env::var_os("HOME").map(PathBuf::from).and_then(|h| {
                    resolve_claude_key_path(&h.join(".claude").join("sessions"), *pid, socket_path)
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
            let title = format!("--title={}", format_push_header(msg));
            let delivered_body = if msg.sender_name.eq_ignore_ascii_case("owner")
                || msg.sender_name.eq_ignore_ascii_case("lmcfarlin")
            {
                if msg.body.starts_with("[Operator Prompt via FerroDeck") {
                    msg.body.clone()
                } else {
                    format!(
                        "[Operator Prompt via FerroDeck — from {}]\n\n{}",
                        msg.sender_name, msg.body
                    )
                }
            } else {
                format_push_message(msg)
            };
            deliver_to_agy_route(
                conversation_id,
                ls_address,
                csrf_token,
                agentapi_exe,
                &title,
                &delivered_body,
            )
        }
        VendorRoute::Codex { thread_id, cwd } => {
            let formatted = format_push_message(msg);
            deliver_to_codex_route(thread_id, cwd.as_ref(), &formatted)
        }
        VendorRoute::Muse {
            session_id,
            cwd,
            tmux_target,
            tmux_pane,
        } => {
            let formatted = format_push_message(msg);
            deliver_to_muse_route(
                session_id,
                cwd.as_ref(),
                tmux_target.as_deref(),
                tmux_pane.as_deref(),
                &formatted,
            )
        }
        VendorRoute::Grok {
            session_id,
            cwd,
            tmux_target,
            tmux_pane,
        } => {
            let formatted = format_push_message(msg);
            deliver_to_grok_route(
                session_id,
                cwd.as_ref(),
                tmux_target.as_deref(),
                tmux_pane.as_deref(),
                &formatted,
            )
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

fn run_status_with_etxtbsy_retry(cmd: &mut Command) -> std::io::Result<std::process::ExitStatus> {
    for attempt in 0..5 {
        match cmd.status() {
            Err(err) if err.raw_os_error() == Some(26) && attempt < 4 => {
                std::thread::sleep(Duration::from_millis(5));
            }
            other => return other,
        }
    }
    cmd.status()
}

fn deliver_to_codex_route(
    thread_id: &str,
    cwd: Option<&PathBuf>,
    text: &str,
) -> Result<(), String> {
    let mut bins = Vec::new();
    if let Some(custom) = std::env::var_os("FL_WIRE_CODEX_BIN") {
        bins.push(PathBuf::from(custom));
    } else {
        bins.push(PathBuf::from("codex"));
        if let Some(home) = std::env::var_os("HOME") {
            let local_bin = PathBuf::from(home).join(".local").join("bin").join("codex");
            if local_bin.is_file() {
                bins.push(local_bin);
            }
        }
    }

    let mut last_err = String::new();
    for bin in bins {
        let mut cmd = Command::new(&bin);
        cmd.args(["queue", "--thread", thread_id, "--message", text])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        if let Some(dir) = cwd
            && dir.exists()
        {
            cmd.current_dir(dir);
        }
        match run_status_with_etxtbsy_retry(&mut cmd) {
            Ok(status) if status.success() => return Ok(()),
            Ok(status) => {
                return Err(format!("codex queue exited with {status}"));
            }
            Err(e) => {
                last_err = format!("failed to spawn codex queue: {e}");
            }
        }
    }
    Err(last_err)
}

/// Deliver a human operator chat prompt from FerroDeck directly to a live vendor session
/// without creating a FerroWire `WireMessage` or inbox entry.
fn deliver_to_muse_route(
    session_id: &str,
    cwd: Option<&PathBuf>,
    tmux_target: Option<&str>,
    tmux_pane: Option<&str>,
    text: &str,
) -> Result<(), String> {
    let use_tmux = std::env::var_os("FL_WIRE_TMUX_BIN").is_some()
        || std::env::var_os("FL_WIRE_MUSE_BIN").is_none();
    if use_tmux {
        let mut candidates = Vec::new();
        if let Some(t) = tmux_target.map(str::trim).filter(|s| !s.is_empty()) {
            candidates.push(t.to_string());
        }
        if let Some(p) = tmux_pane.map(str::trim).filter(|s| !s.is_empty())
            && !candidates.iter().any(|c| c == p)
        {
            candidates.push(p.to_string());
        }
        for target in &candidates {
            if tmux_target_alive(target) {
                return deliver_via_tmux_paste(target, text);
            }
        }
    }

    let bin = std::env::var_os("FL_WIRE_MUSE_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("muse"));
    let mut cmd = Command::new(&bin);
    cmd.env("MUSE_NO_AUTO_UPDATE", "1");
    cmd.args(["exec", "--session-id", session_id, "--json", text])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if let Some(dir) = cwd
        && dir.exists()
    {
        cmd.current_dir(dir);
    }
    let status = run_status_with_etxtbsy_retry(&mut cmd)
        .map_err(|e| format!("failed to spawn {}: {e}", bin.display()))?;
    if !status.success() {
        return Err(format!("muse exec exited with {status}"));
    }
    Ok(())
}

fn deliver_to_grok_route(
    session_id: &str,
    cwd: Option<&PathBuf>,
    tmux_target: Option<&str>,
    tmux_pane: Option<&str>,
    text: &str,
) -> Result<(), String> {
    let use_tmux = std::env::var_os("FL_WIRE_TMUX_BIN").is_some()
        || std::env::var_os("FL_WIRE_GROK_BIN").is_none();
    if use_tmux {
        let mut candidates = Vec::new();
        if let Some(p) = tmux_pane.map(str::trim).filter(|s| !s.is_empty()) {
            candidates.push(p.to_string());
        }
        if let Some(t) = tmux_target.map(str::trim).filter(|s| !s.is_empty())
            && !candidates.iter().any(|c| c == t)
        {
            candidates.push(t.to_string());
        }
        for target in &candidates {
            if tmux_target_alive(target) {
                return deliver_via_tmux_paste(target, text);
            }
        }
    }

    let bin = std::env::var_os("FL_WIRE_GROK_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("grok"));
    let mut cmd = Command::new(&bin);
    cmd.args([
        "--no-auto-update",
        "--no-alt-screen",
        "--always-approve",
        "-s",
        session_id,
        "-p",
        text,
    ])
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null());
    if let Some(dir) = cwd
        && dir.exists()
    {
        cmd.current_dir(dir);
    }
    let status = run_status_with_etxtbsy_retry(&mut cmd)
        .map_err(|e| format!("failed to spawn {}: {e}", bin.display()))?;
    if !status.success() {
        return Err(format!("grok exited with {status}"));
    }
    Ok(())
}

fn deliver_to_agy_route(
    conversation_id: &str,
    ls_address: &Option<String>,
    csrf_token: &Option<String>,
    agentapi_exe: &Option<PathBuf>,
    title: &str,
    body: &str,
) -> Result<(), String> {
    let env_exe = std::env::var_os("ANTIGRAVITY_AGENTAPI_EXE").map(PathBuf::from);
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let exe = resolve_agy_exe(agentapi_exe.as_deref(), env_exe.as_deref(), home.as_deref());
    // Mock binaries (`FL_WIRE_AGY_BIN`) may target a port with no listener.
    // Otherwise a dead route port must not clobber a live language server.
    let trust_unverified = std::env::var_os("FL_WIRE_AGY_BIN").is_some();
    let accept = |addr: &str| {
        !addr.is_empty() && (trust_unverified || crate::identity::agy_ls_address_listening(addr))
    };
    let addr = ls_address
        .as_deref()
        .filter(|addr| accept(addr))
        .map(str::to_string)
        .or_else(|| {
            std::env::var("ANTIGRAVITY_LS_ADDRESS")
                .ok()
                .filter(|addr| accept(addr))
        })
        .unwrap_or_default();
    let csrf = csrf_token
        .clone()
        .or_else(|| std::env::var("ANTIGRAVITY_CSRF_TOKEN").ok())
        .unwrap_or_default();

    let mut cmd = Command::new(&exe);
    cmd.args(["agentapi", "send-message", title, conversation_id, body])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    cmd.env("ANTIGRAVITY_CONVERSATION_ID", conversation_id);
    cmd.env("ANTIGRAVITY_AGENTAPI_EXE", &exe);
    if !addr.is_empty() {
        cmd.env("ANTIGRAVITY_LS_ADDRESS", &addr);
    }
    if !csrf.is_empty() {
        cmd.env("ANTIGRAVITY_CSRF_TOKEN", &csrf);
    }
    let status = run_status_with_etxtbsy_retry(&mut cmd)
        .map_err(|e| format!("failed to spawn {}: {e}", exe.display()))?;
    if !status.success() {
        return Err(format!("agy send-message exited with {status}"));
    }
    Ok(())
}

/// `Ok(true)` after `RevertToCascadeStep` and transcript truncation.
/// `Ok(false)` when `FL_WIRE_AGY_BIN` is set and the live endpoint is incomplete,
/// so the caller sends `/clear` through the mock binary instead.
fn reset_agy_cascade_in_place(
    conversation_id: &str,
    ls_address: &Option<String>,
    csrf_token: &Option<String>,
) -> Result<bool, String> {
    let listening = agy_listening_address(ls_address);
    let csrf = agy_csrf_token(csrf_token);
    if let (Some(addr), Some(csrf)) = (&listening, csrf.as_deref()) {
        if csrf.chars().any(|ch| ch.is_control() || ch.is_whitespace()) {
            return Err("antigravity reset csrf token is not a single header value".into());
        }
        let port = crate::identity::agy_ls_port(addr)
            .ok_or_else(|| format!("antigravity reset could not parse a port from {addr}"))?;
        validate_agy_conversation_id(conversation_id)?;
        post_revert_to_cascade_step(port, csrf, conversation_id)?;
        truncate_agy_transcripts(conversation_id)?;
        return Ok(true);
    }
    if std::env::var_os("FL_WIRE_AGY_BIN").is_some() {
        return Ok(false);
    }
    if listening.is_none() {
        return Err(agy_address_error(ls_address));
    }
    Err("antigravity reset needs a csrf token (route csrf_token or ANTIGRAVITY_CSRF_TOKEN)".into())
}

fn agy_listening_address(route_address: &Option<String>) -> Option<String> {
    agy_address_candidates(route_address)
        .into_iter()
        .find(|addr| crate::identity::agy_ls_address_listening(addr))
}

fn agy_nonempty_address(route_address: &Option<String>) -> Option<String> {
    agy_address_candidates(route_address).into_iter().next()
}

fn agy_address_candidates(route_address: &Option<String>) -> Vec<String> {
    let mut candidates = Vec::new();
    if let Some(addr) = route_address
        .as_deref()
        .map(str::trim)
        .filter(|addr| !addr.is_empty())
    {
        candidates.push(addr.to_string());
    }
    if let Ok(addr) = std::env::var("ANTIGRAVITY_LS_ADDRESS") {
        let addr = addr.trim();
        if !addr.is_empty() && !candidates.iter().any(|seen| seen == addr) {
            candidates.push(addr.to_string());
        }
    }
    candidates
}

fn agy_address_error(route_address: &Option<String>) -> String {
    match agy_nonempty_address(route_address) {
        Some(addr) if crate::identity::agy_ls_port(&addr).is_none() => {
            format!("antigravity reset could not parse a port from {addr}")
        }
        Some(addr) => format!(
            "antigravity reset needs a listening language-server address; {addr} is not accepting connections on 127.0.0.1"
        ),
        None => "antigravity reset needs a listening language-server address (route ls_address or ANTIGRAVITY_LS_ADDRESS)".into(),
    }
}

fn agy_csrf_token(route_csrf: &Option<String>) -> Option<String> {
    route_csrf
        .as_deref()
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_string)
        .or_else(|| {
            std::env::var("ANTIGRAVITY_CSRF_TOKEN")
                .ok()
                .map(|token| token.trim().to_string())
                .filter(|token| !token.is_empty())
        })
}

fn validate_agy_conversation_id(conversation_id: &str) -> Result<(), String> {
    if conversation_id.is_empty()
        || conversation_id.contains(['/', '\\', '\0'])
        || conversation_id.contains("..")
        || conversation_id.trim() == "."
    {
        return Err(format!(
            "antigravity conversation id `{conversation_id}` is not a single directory name"
        ));
    }
    Ok(())
}

const DEFAULT_AGY_PLAN_MODEL: &str = "MODEL_PLACEHOLDER_M418";
const MAX_AGY_RPC_BODY_BYTES: usize = 256 * 1024;

fn revert_to_cascade_step_body(conversation_id: &str, plan_model: &str) -> String {
    serde_json::json!({
        "cascadeId": conversation_id,
        "stepIndex": 0,
        "conversationOnly": true,
        "overrideConfig": {
            "plannerConfig": {
                "planModel": plan_model,
                "requestedModel": {
                    "model": plan_model,
                },
            },
        },
    })
    .to_string()
}

fn read_agy_chunked_body<R: BufRead>(reader: &mut R, max_bytes: usize) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let mut size_line = String::new();
        match reader.read_line(&mut size_line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let hex = size_line.trim().split(';').next().unwrap_or("").trim();
        let Ok(chunk_len) = usize::from_str_radix(hex, 16) else {
            break;
        };
        if chunk_len == 0 {
            let mut trailer = String::new();
            let _ = reader.read_line(&mut trailer);
            break;
        }
        let remaining = max_bytes.saturating_sub(out.len());
        let to_read = chunk_len.min(remaining);
        if to_read == 0 {
            break;
        }
        let start = out.len();
        out.resize(start + to_read, 0);
        if reader.read_exact(&mut out[start..]).is_err() {
            out.truncate(start);
            break;
        }
        if chunk_len > to_read {
            break;
        }
        let mut crlf = [0u8; 2];
        if reader.read_exact(&mut crlf).is_err() {
            break;
        }
    }
    out
}

fn post_agy_connect_rpc(
    port: u16,
    csrf: &str,
    procedure: &str,
    body: &str,
    timeout: Duration,
) -> Result<(u16, String, String), String> {
    post_agy_connect_rpc_limited(port, csrf, procedure, body, timeout, MAX_AGY_RPC_BODY_BYTES)
}

/// POST a JSON body to the Antigravity language server on `127.0.0.1:port` and return
/// `(status code, status line, body)`. `timeout` bounds the connect and each read or write.
/// The response body is cut off at `max_body_bytes`.
pub fn post_agy_connect_rpc_limited(
    port: u16,
    csrf: &str,
    procedure: &str,
    body: &str,
    timeout: Duration,
    max_body_bytes: usize,
) -> Result<(u16, String, String), String> {
    let host = format!("127.0.0.1:{port}");
    let request = format!(
        "POST {procedure} HTTP/1.1\r\n\
         Host: {host}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         x-codeium-csrf-token: {csrf}\r\n\
         \r\n\
         {body}",
        body.len()
    );
    let target = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = TcpStream::connect_timeout(&target, timeout)
        .map_err(|err| format!("antigravity revert connect to {host} failed: {err}"))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|err| format!("antigravity revert read timeout failed: {err}"))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|err| format!("antigravity revert write timeout failed: {err}"))?;
    stream
        .write_all(request.as_bytes())
        .map_err(|err| format!("antigravity revert write to {host} failed: {err}"))?;
    let mut reader = std::io::BufReader::new(stream);
    let mut status = String::new();
    reader
        .read_line(&mut status)
        .map_err(|err| format!("antigravity revert read from {host} failed: {err}"))?;
    let mut content_length: Option<usize> = None;
    let mut is_chunked = false;
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                if line == "\r\n" || line == "\n" {
                    break;
                }
                if let Some((name, value)) = line.split_once(':') {
                    let name = name.trim();
                    let value = value.trim();
                    if name.eq_ignore_ascii_case("content-length") {
                        content_length = value.parse::<usize>().ok();
                    } else if name.eq_ignore_ascii_case("transfer-encoding")
                        && value.to_ascii_lowercase().contains("chunked")
                    {
                        is_chunked = true;
                    }
                }
            }
            Err(_) => break,
        }
    }
    let resp_bytes = if is_chunked {
        read_agy_chunked_body(&mut reader, max_body_bytes)
    } else {
        let max_body = content_length.unwrap_or(max_body_bytes).min(max_body_bytes);
        let mut buf = Vec::new();
        if max_body > 0 {
            let _ = reader.take(max_body as u64).read_to_end(&mut buf);
        }
        buf
    };
    let resp_body = String::from_utf8_lossy(&resp_bytes).into_owned();
    let code = status
        .split_whitespace()
        .nth(1)
        .and_then(|raw| raw.parse::<u16>().ok())
        .unwrap_or(0);
    Ok((code, status.trim().to_string(), resp_body))
}

fn fetch_agy_default_plan_model(port: u16, csrf: &str) -> Option<String> {
    let (code, _, body) = post_agy_connect_rpc(
        port,
        csrf,
        "/exa.language_server_pb.LanguageServerService/GetCascadeModelConfigData",
        "{}",
        Duration::from_secs(2),
    )
    .ok()?;
    if code != 200 {
        return None;
    }
    let val: serde_json::Value = serde_json::from_str(&body).ok()?;
    val.get("defaultOverrideModelConfig")?
        .get("modelOrAlias")?
        .get("model")?
        .as_str()
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(str::to_string)
}

fn post_revert_to_cascade_step(port: u16, csrf: &str, conversation_id: &str) -> Result<(), String> {
    let procedure = "/exa.language_server_pb.LanguageServerService/RevertToCascadeStep";
    let body = revert_to_cascade_step_body(conversation_id, DEFAULT_AGY_PLAN_MODEL);
    let (mut code, mut status_line, mut resp_body) =
        post_agy_connect_rpc(port, csrf, procedure, &body, Duration::from_secs(5))?;
    if code == 200 {
        return Ok(());
    }
    if resp_body.to_ascii_lowercase().contains("model")
        && let Some(model) =
            fetch_agy_default_plan_model(port, csrf).filter(|m| m != DEFAULT_AGY_PLAN_MODEL)
    {
        let retry_body = revert_to_cascade_step_body(conversation_id, &model);
        (code, status_line, resp_body) =
            post_agy_connect_rpc(port, csrf, procedure, &retry_body, Duration::from_secs(5))?;
        if code == 200 {
            return Ok(());
        }
    }
    let detail: String = resp_body.trim().chars().take(512).collect();
    if detail.is_empty() {
        Err(format!("antigravity revert returned {status_line}"))
    } else {
        Err(format!(
            "antigravity revert returned {status_line}: {detail}"
        ))
    }
}

fn truncate_agy_transcripts(conversation_id: &str) -> Result<(), String> {
    let Some(home) = std::env::var_os("HOME") else {
        return Ok(());
    };
    let logs = PathBuf::from(home)
        .join(".gemini")
        .join("antigravity-cli")
        .join("brain")
        .join(conversation_id)
        .join(".system_generated")
        .join("logs");
    for name in ["transcript.jsonl", "transcript_full.jsonl"] {
        let path = logs.join(name);
        if path.is_file() {
            truncate_agy_transcript_file(&path)?;
        }
    }
    Ok(())
}

fn truncate_agy_transcript_file(path: &Path) -> Result<(), String> {
    let raw = fs::read_to_string(path)
        .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
    let keep = match raw.lines().next() {
        Some(line) => {
            let step_zero = serde_json::from_str::<serde_json::Value>(line.trim())
                .ok()
                .and_then(|val| val.get("step_index").and_then(|step| step.as_u64()))
                == Some(0);
            if step_zero {
                format!("{line}\n")
            } else {
                String::new()
            }
        }
        None => String::new(),
    };
    fs::write(path, keep).map_err(|err| format!("failed to truncate {}: {err}", path.display()))
}

/// Reset a live vendor session. Tmux-backed Muse and Grok routes send `C-u`,
/// the slash command, and `Enter` through `tmux send-keys`. Codex and Grok use
/// `/new`. Claude, Muse, and Webhook use `/clear`.
///
/// Antigravity rewinds the open cascade with `RevertToCascadeStep`
/// (`stepIndex` 0, `conversationOnly` true) and keeps only the `step_index` 0
/// transcript line. A mock `FL_WIRE_AGY_BIN` without a listening address and
/// csrf token still sends `/clear`.
///
/// Reforge requires a non-empty prompt before any tmux or vendor dispatch.
/// It waits 2s after a tmux reset (skipped when `FL_WIRE_TMUX_BIN` is set)
/// and then delivers that prompt on the same route.
pub fn clear_or_reforge_session(
    route: &VendorRoute,
    mode: SessionResetMode,
    sender_name: &str,
    reforge_prompt: Option<&str>,
) -> Result<(), String> {
    let prompt = if mode == SessionResetMode::Reforge {
        Some(require_reforge_prompt(reforge_prompt)?)
    } else {
        None
    };
    if let VendorRoute::Agy {
        conversation_id,
        ls_address,
        csrf_token,
        ..
    } = route
        && reset_agy_cascade_in_place(conversation_id, ls_address, csrf_token)?
    {
        if let Some(prompt) = prompt {
            dispatch_operator_prompt_to_route(route, sender_name, prompt)?;
        }
        return Ok(());
    }
    let slash = session_reset_slash(route);
    if let Some(target) = alive_tmux_reset_target(route) {
        send_tmux_reset_keys(&target, slash)?;
        if let Some(prompt) = prompt {
            if std::env::var_os("FL_WIRE_TMUX_BIN").is_none() {
                std::thread::sleep(Duration::from_millis(2000));
            }
            dispatch_operator_prompt_to_route(route, sender_name, prompt)?;
        }
        return Ok(());
    }
    dispatch_operator_prompt_to_route(route, sender_name, slash)?;
    if let Some(prompt) = prompt {
        dispatch_operator_prompt_to_route(route, sender_name, prompt)?;
    }
    Ok(())
}

fn require_reforge_prompt(reforge_prompt: Option<&str>) -> Result<&str, String> {
    reforge_prompt
        .map(str::trim)
        .filter(|prompt| !prompt.is_empty())
        .ok_or_else(|| "missing reforge prompt".to_string())
}

fn session_reset_slash(route: &VendorRoute) -> &'static str {
    match route {
        VendorRoute::Codex { .. } | VendorRoute::Grok { .. } => "/new",
        VendorRoute::Claude { .. }
        | VendorRoute::Muse { .. }
        | VendorRoute::Agy { .. }
        | VendorRoute::Webhook { .. } => "/clear",
    }
}

fn alive_tmux_reset_target(route: &VendorRoute) -> Option<String> {
    let ordered: [&Option<String>; 2] = match route {
        VendorRoute::Grok {
            tmux_pane,
            tmux_target,
            ..
        } => [tmux_pane, tmux_target],
        VendorRoute::Muse {
            tmux_target,
            tmux_pane,
            ..
        } => [tmux_target, tmux_pane],
        VendorRoute::Claude { .. }
        | VendorRoute::Codex { .. }
        | VendorRoute::Agy { .. }
        | VendorRoute::Webhook { .. } => return None,
    };
    let mut seen = Vec::new();
    for slot in ordered {
        let Some(raw) = slot else {
            continue;
        };
        let target = raw.trim();
        if target.is_empty() || seen.iter().any(|seen| seen == target) {
            continue;
        }
        seen.push(target.to_string());
        if tmux_target_alive(target) {
            return Some(target.to_string());
        }
    }
    None
}

fn send_tmux_reset_keys(target: &str, slash: &str) -> Result<(), String> {
    let tmux = tmux_bin();
    for key in ["C-u", slash, "Enter"] {
        let status = Command::new(&tmux)
            .args(["send-keys", "-t", target, key])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_err(|e| format!("failed to spawn tmux send-keys: {e}"))?;
        if !status.success() {
            return Err(format!("tmux send-keys {key} exited with {status}"));
        }
    }
    Ok(())
}

/// Method label of [`InterruptPlan::TmuxKey`].
pub const INTERRUPT_METHOD_TMUX: &str = "tmux-send-keys";
/// Method label of [`InterruptPlan::AgyCancel`].
pub const INTERRUPT_METHOD_AGY: &str = "agy-cancel-cascade";

/// The one tmux key an interrupt sends. `Escape` stops the turn that is running in every
/// supported TUI (Claude Code, Codex, Grok and Muse all treat it as "stop generating") and
/// does nothing harmful to an idle prompt. `C-c` is NOT used: at an idle prompt it clears the
/// input, and a second one quits Claude Code and Codex, so a stray interrupt could kill the
/// session. One key per interrupt also avoids the double-`Escape` rewind menu in Claude Code.
const INTERRUPT_TMUX_KEY: &str = "Escape";

const AGY_CANCEL_PROCEDURE: &str =
    "/exa.language_server_pb.LanguageServerService/CancelCascadeInvocation";

/// How [`run_interrupt`] will interrupt one route. Building the plan touches nothing
/// outside this process, so a caller can plan and then decide whether to run it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InterruptPlan {
    /// Send `key` with `tmux send-keys` to the first live target of `targets`.
    TmuxKey {
        targets: Vec<String>,
        key: &'static str,
    },
    /// Call `CancelCascadeInvocation` on the Antigravity language server.
    AgyCancel {
        conversation_id: String,
        ls_address: Option<String>,
        csrf_token: Option<String>,
    },
}

impl InterruptPlan {
    /// Stable label for the way the interrupt travels (`tmux-send-keys`, `agy-cancel-cascade`).
    pub fn method(&self) -> &'static str {
        match self {
            Self::TmuxKey { .. } => INTERRUPT_METHOD_TMUX,
            Self::AgyCancel { .. } => INTERRUPT_METHOD_AGY,
        }
    }
}

/// Plan an interrupt for `route`. Claude and Codex routes carry no tmux target, so a caller
/// that knows the pane the CLI runs in (a hosted session record) passes it as `tmux_fallback`;
/// Grok and Muse try their own pane and target first. A route with no way to interrupt it
/// (a webhook route, or a tmux vendor with no target at all) is an error.
pub fn plan_interrupt(
    route: &VendorRoute,
    tmux_fallback: Option<&str>,
) -> Result<InterruptPlan, String> {
    let own: [&Option<String>; 2] = match route {
        VendorRoute::Grok {
            tmux_pane,
            tmux_target,
            ..
        } => [tmux_pane, tmux_target],
        VendorRoute::Muse {
            tmux_target,
            tmux_pane,
            ..
        } => [tmux_target, tmux_pane],
        VendorRoute::Claude { .. } | VendorRoute::Codex { .. } => [&None, &None],
        VendorRoute::Agy {
            conversation_id,
            ls_address,
            csrf_token,
            ..
        } => {
            validate_agy_conversation_id(conversation_id)?;
            return Ok(InterruptPlan::AgyCancel {
                conversation_id: conversation_id.clone(),
                ls_address: ls_address.clone(),
                csrf_token: csrf_token.clone(),
            });
        }
        VendorRoute::Webhook { .. } => {
            return Err("a webhook route cannot be interrupted".to_string());
        }
    };
    let mut targets: Vec<String> = Vec::new();
    for raw in own
        .into_iter()
        .map(|slot| slot.as_deref())
        .chain(std::iter::once(tmux_fallback))
    {
        let Some(target) = raw.map(str::trim).filter(|t| !t.is_empty()) else {
            continue;
        };
        if !targets.iter().any(|seen| seen == target) {
            targets.push(target.to_string());
        }
    }
    if targets.is_empty() {
        return Err("the route has no tmux target to send an interrupt to".to_string());
    }
    Ok(InterruptPlan::TmuxKey {
        targets,
        key: INTERRUPT_TMUX_KEY,
    })
}

/// Run `plan`: the real `tmux send-keys`, or the real language-server call.
pub fn run_interrupt(plan: &InterruptPlan) -> Result<(), String> {
    match plan {
        InterruptPlan::TmuxKey { targets, key } => {
            let target = targets
                .iter()
                .find(|t| tmux_target_alive(t))
                .ok_or_else(|| "no live tmux target for the session".to_string())?;
            let status = Command::new(tmux_bin())
                .args(["send-keys", "-t", target, key])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map_err(|e| format!("failed to spawn tmux send-keys: {e}"))?;
            if !status.success() {
                return Err(format!("tmux send-keys {key} exited with {status}"));
            }
            Ok(())
        }
        InterruptPlan::AgyCancel {
            conversation_id,
            ls_address,
            csrf_token,
        } => {
            let Some(addr) = agy_listening_address(ls_address) else {
                return Err(agy_address_error(ls_address).replace("reset", "interrupt"));
            };
            let csrf = agy_csrf_token(csrf_token).ok_or_else(|| {
                "antigravity interrupt needs a csrf token (route csrf_token or ANTIGRAVITY_CSRF_TOKEN)"
                    .to_string()
            })?;
            if csrf.chars().any(|ch| ch.is_control() || ch.is_whitespace()) {
                return Err("antigravity interrupt csrf token is not a single header value".into());
            }
            let port = crate::identity::agy_ls_port(&addr).ok_or_else(|| {
                format!("antigravity interrupt could not parse a port from {addr}")
            })?;
            let body = serde_json::json!({ "cascadeId": conversation_id }).to_string();
            let (code, status_line, resp_body) = post_agy_connect_rpc(
                port,
                &csrf,
                AGY_CANCEL_PROCEDURE,
                &body,
                Duration::from_secs(5),
            )?;
            if code == 200 {
                return Ok(());
            }
            let detail: String = resp_body.trim().chars().take(512).collect();
            if detail.is_empty() {
                Err(format!("antigravity interrupt returned {status_line}"))
            } else {
                Err(format!(
                    "antigravity interrupt returned {status_line}: {detail}"
                ))
            }
        }
    }
}

fn tmux_bin() -> PathBuf {
    std::env::var_os("FL_WIRE_TMUX_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("tmux"))
}

fn tmux_target_alive(target: &str) -> bool {
    let Ok(out) = Command::new(tmux_bin())
        .args(["display-message", "-p", "-t", target, "#{pane_id}"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
    else {
        return false;
    };
    out.status.success() && !String::from_utf8_lossy(&out.stdout).trim().is_empty()
}

fn deliver_via_tmux_paste(target: &str, text: &str) -> Result<(), String> {
    let tmux = tmux_bin();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let buf_name = format!("fl-wire-muse-{}-{nanos}", std::process::id());
    let payload = format!("{}\n", text.trim());

    let mut load_child = Command::new(&tmux)
        .args(["load-buffer", "-b", &buf_name, "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("failed to spawn tmux load-buffer: {e}"))?;
    if let Some(mut stdin) = load_child.stdin.take() {
        stdin
            .write_all(payload.as_bytes())
            .map_err(|e| format!("failed to write to tmux load-buffer stdin: {e}"))?;
    }
    let load_status = load_child
        .wait()
        .map_err(|e| format!("failed waiting on tmux load-buffer: {e}"))?;
    if !load_status.success() {
        return Err(format!("tmux load-buffer exited with {load_status}"));
    }

    let paste_status = Command::new(&tmux)
        .args(["paste-buffer", "-d", "-b", &buf_name, "-t", target])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|e| format!("failed to spawn tmux paste-buffer: {e}"))?;
    if !paste_status.success() {
        let _ = Command::new(&tmux)
            .args(["delete-buffer", "-b", &buf_name])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        return Err(format!("tmux paste-buffer exited with {paste_status}"));
    }

    let skip_sleep = std::env::var_os("FL_WIRE_TMUX_BIN").is_some();
    if !skip_sleep {
        std::thread::sleep(Duration::from_millis(350));
    }

    let enter_status = Command::new(&tmux)
        .args(["send-keys", "-t", target, "Enter"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|e| format!("failed to spawn tmux send-keys: {e}"))?;
    if !enter_status.success() {
        return Err(format!("tmux send-keys exited with {enter_status}"));
    }

    if !skip_sleep {
        std::thread::sleep(Duration::from_millis(250));
    }
    if let Ok(cap) = Command::new(&tmux)
        .args(["capture-pane", "-p", "-t", target, "-S", "-8"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        && cap.status.success()
        && String::from_utf8_lossy(&cap.stdout).contains("[Pasted Content")
    {
        let _ = Command::new(&tmux)
            .args(["send-keys", "-t", target, "Enter"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }

    Ok(())
}

/// `true` when `sender_name` names the owner (`owner`, or the owner's login `lmcfarlin`).
///
/// This is the gate that lets an operator prompt arrive as direct terminal input. The match is
/// exact (case-insensitive, no trimming) so a padded name never passes. A caller that builds a
/// sender name from anything a peer can influence must refuse these names first: see
/// `HubAuthority::actor_name` in `fl-hub`.
pub fn is_owner_sender(sender_name: &str) -> bool {
    sender_name.eq_ignore_ascii_case("owner") || sender_name.eq_ignore_ascii_case("lmcfarlin")
}

/// The tmux pane id (`%<n>`) of a Claude session, read from the `tmux` field of its session key
/// file (for example `"Felicia:@12.%12"`), when that pane is alive.
///
/// Only a pane id is accepted. A session or window name is not: `tmux -t <name>` resolves to
/// whichever pane is active there, which need not be the pane Claude runs in.
fn claude_live_tmux_pane(key_json: &serde_json::Value) -> Option<String> {
    let raw = key_json.get("tmux")?.as_str()?.trim();
    let pane = raw.rsplit('.').next()?;
    let digits = pane.strip_prefix('%')?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    tmux_target_alive(pane).then(|| pane.to_string())
}

/// The body pasted into a terminal for an operator prompt: the FerroDeck prefix, unless the body
/// already carries it or is a slash command (a prefix would stop it being one).
fn prefixed_operator_body(sender_name: &str, body: &str) -> String {
    let trimmed = body.trim_start();
    if body.starts_with("[Operator Prompt via FerroDeck") || trimmed.starts_with('/') {
        body.to_string()
    } else {
        format!("[Operator Prompt via FerroDeck — from {sender_name}]\n\n{body}")
    }
}

pub fn dispatch_operator_prompt_to_route(
    route: &VendorRoute,
    sender_name: &str,
    body: &str,
) -> Result<(), String> {
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
                std::env::var_os("HOME").map(PathBuf::from).and_then(|h| {
                    resolve_claude_key_path(&h.join(".claude").join("sessions"), *pid, socket_path)
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
            // The owner's own chat prompt is delivered as direct terminal input when the
            // session runs in a live tmux pane. The peer-session socket below labels every
            // message "came from another Claude session", which is wrong for the owner. Only
            // an owner sender may take this path: it must stay out of reach of any caller
            // that is merely authenticated as a peer.
            if is_owner_sender(sender_name)
                && let Some(pane) = claude_live_tmux_pane(&key_json)
            {
                return deliver_via_tmux_paste(&pane, &prefixed_operator_body(sender_name, body));
            }
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
                "from": sender_name,
                "priority": "next",
                "message": {
                    "role": "user",
                    "content": body,
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
            let title = "--title=[Operator Prompt via FerroDeck]";
            // `/clear` and `/new` must reach Antigravity as the reset command.
            // Every other body, including the reforge prompt, keeps the prefix.
            let trimmed = body.trim();
            let delivered_body = if matches!(trimmed, "/clear" | "/new") {
                trimmed.to_string()
            } else if body.starts_with("[Operator Prompt via FerroDeck") {
                body.to_string()
            } else {
                format!("[Operator Prompt via FerroDeck — from {sender_name}]\n\n{body}")
            };
            deliver_to_agy_route(
                conversation_id,
                ls_address,
                csrf_token,
                agentapi_exe,
                title,
                &delivered_body,
            )
        }
        VendorRoute::Codex { thread_id, cwd } => {
            deliver_to_codex_route(thread_id, cwd.as_ref(), body)
        }
        VendorRoute::Muse {
            session_id,
            cwd,
            tmux_target,
            tmux_pane,
        } => deliver_to_muse_route(
            session_id,
            cwd.as_ref(),
            tmux_target.as_deref(),
            tmux_pane.as_deref(),
            body,
        ),
        VendorRoute::Grok {
            session_id,
            cwd,
            tmux_target,
            tmux_pane,
        } => deliver_to_grok_route(
            session_id,
            cwd.as_ref(),
            tmux_target.as_deref(),
            tmux_pane.as_deref(),
            body,
        ),
        VendorRoute::Webhook { socket_path } => {
            let mut stream = UnixStream::connect(socket_path).map_err(|e| {
                format!(
                    "failed to connect to webhook socket {}: {e}",
                    socket_path.display()
                )
            })?;
            let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
            let mut line = serde_json::to_string(&serde_json::json!({
                "kind": "operator_prompt",
                "sender_name": sender_name,
                "body": body,
            }))
            .map_err(|e| format!("serialize operator prompt: {e}"))?;
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
    use std::sync::Mutex;
    use tempfile::TempDir;

    /// `FL_WIRE_TMUX_BIN` is process-global. Tests that point it at a fake tmux
    /// must hold this lock for the whole test, including the earlier vendor
    /// dispatch that treats a set tmux bin as permission to use tmux.
    static TMUX_ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Antigravity reset reads `FL_WIRE_AGY_BIN`, `ANTIGRAVITY_LS_ADDRESS`, and
    /// `ANTIGRAVITY_CSRF_TOKEN`. Tests that change those variables hold this lock
    /// for the whole test so a mock binary cannot hide a live revert, and a live
    /// revert cannot run against the host language server.
    static AGY_ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn validate_agy_conversation_id_rejects_dot_and_nul() {
        assert!(validate_agy_conversation_id(".").is_err());
        assert!(validate_agy_conversation_id("a\0b").is_err());
    }

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

        let updated = dispatch_message_for_store(
            &store,
            &mut routes,
            Some(tmp.path()),
            &msg,
            At::from_unix_millis(20),
        );
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
        assert!(
            user["message"]["content"]
                .as_str()
                .unwrap()
                .contains("hello sonya")
        );
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

        let _tmux_lock = TMUX_ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
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
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"{}\"\n",
                grok_out.display()
            ),
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
                tmux_target: None,
                tmux_pane: None,
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

        let tmux_out = tmp.path().join("tmux.out");
        let tmux_stdin_out = tmp.path().join("tmux.stdin");
        let tmux_bin = tmp.path().join("fake-tmux.sh");
        fs::write(
            &tmux_bin,
            format!(
                "#!/bin/sh\necho \"$*\" >> \"{}\"\nif [ \"$1\" = \"display-message\" ]; then\n  echo \"%9\"\nelif [ \"$1\" = \"load-buffer\" ]; then\n  cat > \"{}\"\nelif [ \"$1\" = \"capture-pane\" ]; then\n  echo \"[Pasted Content 28 chars]\"\nfi\n",
                tmux_out.display(),
                tmux_stdin_out.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&tmux_bin, fs::Permissions::from_mode(0o755)).unwrap();
        }
        unsafe {
            std::env::set_var("FL_WIRE_TMUX_BIN", &tmux_bin);
        }
        dispatch_operator_prompt_to_route(
            &VendorRoute::Muse {
                session_id: "ms-42".into(),
                cwd: Some(tmp.path().to_path_buf()),
                tmux_target: Some("muse:felicia".into()),
                tmux_pane: Some("%9".into()),
            },
            "owner",
            "Hello Felicia via tmux",
        )
        .unwrap();
        unsafe {
            std::env::remove_var("FL_WIRE_TMUX_BIN");
        }
        let tmux_cmds = fs::read_to_string(&tmux_out).unwrap();
        assert!(tmux_cmds.contains("display-message -p -t muse:felicia #{pane_id}"));
        assert!(tmux_cmds.contains("load-buffer -b fl-wire-muse-"));
        assert!(tmux_cmds.contains("-t muse:felicia"));
        assert!(tmux_cmds.contains("send-keys -t muse:felicia Enter"));
        let pasted_stdin = fs::read_to_string(&tmux_stdin_out).unwrap();
        assert_eq!(pasted_stdin.trim(), "Hello Felicia via tmux");

        dispatch_to_route(
            &VendorRoute::Grok {
                session_id: "gs-99".into(),
                cwd: Some(tmp.path().to_path_buf()),
                tmux_target: None,
                tmux_pane: None,
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

    #[test]
    fn operator_prompt_dispatch_sends_direct_frames_without_wire_headers() {
        let tmp = TempDir::new().unwrap();
        let claude_sock = tmp.path().join("claude-op.sock");
        let key_path = tmp.path().join("0.key");
        fs::write(&key_path, "{\"peerToken\":\"tok-op\"}").unwrap();

        let claude_listener = UnixListener::bind(&claude_sock).unwrap();
        let claude_handle = std::thread::spawn(move || {
            let (stream, _) = claude_listener.accept().unwrap();
            let reader = std::io::BufReader::new(stream);
            reader
                .lines()
                .take(2)
                .map(|l| l.unwrap())
                .collect::<Vec<_>>()
        });

        dispatch_operator_prompt_to_route(
            &VendorRoute::Claude {
                pid: 0,
                session_id: "s-op".into(),
                socket_path: claude_sock,
                key_path: Some(key_path),
            },
            "owner",
            "Check the queue status",
        )
        .unwrap();

        let claude_lines = claude_handle.join().unwrap();
        let auth: serde_json::Value = serde_json::from_str(&claude_lines[0]).unwrap();
        assert_eq!(auth["type"], "auth");
        assert_eq!(auth["token"], "tok-op");
        let user: serde_json::Value = serde_json::from_str(&claude_lines[1]).unwrap();
        assert_eq!(user["type"], "user");
        assert_eq!(user["from"], "owner");
        assert_eq!(user["priority"], "next");
        assert_eq!(user["message"]["role"], "user");
        assert_eq!(user["message"]["content"], "Check the queue status");

        let webhook_sock = tmp.path().join("webhook-op.sock");
        let webhook_listener = UnixListener::bind(&webhook_sock).unwrap();
        let webhook_handle = std::thread::spawn(move || {
            let (stream, _) = webhook_listener.accept().unwrap();
            let reader = std::io::BufReader::new(stream);
            reader.lines().next().unwrap().unwrap()
        });

        dispatch_operator_prompt_to_route(
            &VendorRoute::Webhook {
                socket_path: webhook_sock,
            },
            "lmcfarlin",
            "Inspect the UI",
        )
        .unwrap();

        let wh_line = webhook_handle.join().unwrap();
        let wh_json: serde_json::Value = serde_json::from_str(&wh_line).unwrap();
        assert_eq!(wh_json["kind"], "operator_prompt");
        assert_eq!(wh_json["sender_name"], "lmcfarlin");
        assert_eq!(wh_json["body"], "Inspect the UI");
    }

    /// A fake tmux that logs its arguments, keeps `load-buffer` stdin, and answers
    /// `display-message` with a pane id when `alive`, or fails when not.
    fn write_fake_tmux(dir: &Path, alive: bool) -> (PathBuf, PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;

        let out = dir.join("tmux.out");
        let stdin_out = dir.join("tmux.stdin");
        let bin = dir.join("fake-tmux.sh");
        let display = if alive { "echo \"%12\"" } else { "exit 1" };
        fs::write(
            &bin,
            format!(
                "#!/bin/sh\necho \"$*\" >> \"{}\"\nif [ \"$1\" = \"display-message\" ]; then\n  {display}\nelif [ \"$1\" = \"load-buffer\" ]; then\n  cat > \"{}\"\nfi\n",
                out.display(),
                stdin_out.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        (bin, out, stdin_out)
    }

    /// A Claude route whose mock socket records whether anything connected. The session key
    /// file carries `tmux_field` (the raw `tmux` value) when it is `Some`.
    fn claude_route_with_mock_socket(
        dir: &Path,
        tmux_field: Option<&str>,
    ) -> (VendorRoute, UnixListener) {
        let sock_path = dir.join("claude-tmux.sock");
        let key_path = dir.join("0.key");
        let key = match tmux_field {
            Some(t) => serde_json::json!({ "peerToken": "tok-t", "tmux": t }),
            None => serde_json::json!({ "peerToken": "tok-t" }),
        };
        fs::write(&key_path, key.to_string()).unwrap();
        let listener = UnixListener::bind(&sock_path).unwrap();
        listener.set_nonblocking(true).unwrap();
        (
            VendorRoute::Claude {
                pid: 0,
                session_id: "s-tmux".into(),
                socket_path: sock_path,
                key_path: Some(key_path),
            },
            listener,
        )
    }

    fn socket_frames(listener: &UnixListener) -> Option<Vec<String>> {
        let (stream, _) = listener.accept().ok()?;
        stream.set_nonblocking(false).unwrap();
        let reader = std::io::BufReader::new(stream);
        Some(reader.lines().take(2).map(|l| l.unwrap()).collect())
    }

    #[test]
    fn claude_owner_prompt_with_live_tmux_pane_is_pasted_not_sent_to_the_peer_socket() {
        let _tmux_lock = TMUX_ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        let tmp = TempDir::new().unwrap();
        let (tmux_bin, tmux_out, tmux_stdin) = write_fake_tmux(tmp.path(), true);
        let (route, listener) = claude_route_with_mock_socket(tmp.path(), Some("Felicity:@12.%12"));
        let _env = EnvGuard::set("FL_WIRE_TMUX_BIN", &tmux_bin);

        for sender in ["owner", "lmcfarlin", "Owner"] {
            let _ = fs::remove_file(&tmux_out);
            dispatch_operator_prompt_to_route(&route, sender, "Check the queue status").unwrap();
            let cmds = fs::read_to_string(&tmux_out).unwrap();
            assert!(
                cmds.contains("display-message -p -t %12 #{pane_id}"),
                "{cmds}"
            );
            assert!(cmds.contains("paste-buffer -d -b"), "{cmds}");
            assert!(cmds.contains("-t %12"), "{cmds}");
            assert!(cmds.contains("send-keys -t %12 Enter"), "{cmds}");
            assert_eq!(
                fs::read_to_string(&tmux_stdin).unwrap().trim(),
                format!(
                    "[Operator Prompt via FerroDeck — from {sender}]\n\nCheck the queue status"
                )
            );
        }
        assert!(
            socket_frames(&listener).is_none(),
            "the peer socket must receive nothing"
        );
    }

    #[test]
    fn claude_owner_slash_command_is_pasted_without_the_prefix() {
        let _tmux_lock = TMUX_ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        let tmp = TempDir::new().unwrap();
        let (tmux_bin, _tmux_out, tmux_stdin) = write_fake_tmux(tmp.path(), true);
        let (route, listener) = claude_route_with_mock_socket(tmp.path(), Some("Felicity:@12.%12"));
        let _env = EnvGuard::set("FL_WIRE_TMUX_BIN", &tmux_bin);

        dispatch_operator_prompt_to_route(&route, "owner", "/compact").unwrap();
        assert_eq!(fs::read_to_string(&tmux_stdin).unwrap().trim(), "/compact");
        assert!(socket_frames(&listener).is_none());
    }

    #[test]
    fn claude_owner_prompt_without_a_usable_tmux_pane_uses_the_socket() {
        let _tmux_lock = TMUX_ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        let tmp = TempDir::new().unwrap();

        // No `tmux` field at all.
        let (tmux_bin, tmux_out, _) = write_fake_tmux(tmp.path(), true);
        let _env = EnvGuard::set("FL_WIRE_TMUX_BIN", &tmux_bin);
        let (route, listener) = claude_route_with_mock_socket(tmp.path(), None);
        dispatch_operator_prompt_to_route(&route, "owner", "no field").unwrap();
        let frames = socket_frames(&listener).expect("socket path");
        let user: serde_json::Value = serde_json::from_str(&frames[1]).unwrap();
        assert_eq!(user["message"]["content"], "no field");
        assert!(!tmux_out.exists(), "tmux must not be used without a field");
        drop(listener);
        let _ = fs::remove_file(tmp.path().join("claude-tmux.sock"));

        // A `tmux` field naming a session, not a pane id: never used as a target.
        let (route, listener) = claude_route_with_mock_socket(tmp.path(), Some("Felicity:@12"));
        dispatch_operator_prompt_to_route(&route, "owner", "no pane id").unwrap();
        assert!(socket_frames(&listener).is_some());
        assert!(!tmux_out.exists());
        drop(listener);
        let _ = fs::remove_file(tmp.path().join("claude-tmux.sock"));

        // A pane that is gone.
        let (dead_bin, dead_out, _) = {
            let dead_dir = tmp.path().join("dead");
            fs::create_dir(&dead_dir).unwrap();
            write_fake_tmux(&dead_dir, false)
        };
        let _dead_env = EnvGuard::set("FL_WIRE_TMUX_BIN", &dead_bin);
        let (route, listener) = claude_route_with_mock_socket(tmp.path(), Some("Felicity:@12.%12"));
        dispatch_operator_prompt_to_route(&route, "owner", "dead pane").unwrap();
        let frames = socket_frames(&listener).expect("socket path");
        let user: serde_json::Value = serde_json::from_str(&frames[1]).unwrap();
        assert_eq!(user["message"]["content"], "dead pane");
        let cmds = fs::read_to_string(&dead_out).unwrap();
        assert!(cmds.contains("display-message"), "{cmds}");
        assert!(!cmds.contains("paste-buffer"), "{cmds}");
    }

    #[test]
    fn claude_non_owner_prompt_with_live_tmux_pane_uses_the_socket_not_tmux() {
        let _tmux_lock = TMUX_ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        let tmp = TempDir::new().unwrap();
        let (tmux_bin, tmux_out, _) = write_fake_tmux(tmp.path(), true);
        let _env = EnvGuard::set("FL_WIRE_TMUX_BIN", &tmux_bin);

        for sender in ["peer", "ferris", "jarvis", " owner", "owner2", "system"] {
            let (route, listener) =
                claude_route_with_mock_socket(tmp.path(), Some("Felicity:@12.%12"));
            dispatch_operator_prompt_to_route(&route, sender, "from a peer").unwrap();
            let frames = socket_frames(&listener).expect("socket path");
            let user: serde_json::Value = serde_json::from_str(&frames[1]).unwrap();
            assert_eq!(user["from"], sender);
            assert_eq!(user["message"]["content"], "from a peer");
            assert!(!tmux_out.exists(), "sender `{sender}` must not reach tmux");
            drop(listener);
            let _ = fs::remove_file(tmp.path().join("claude-tmux.sock"));
        }
    }

    #[test]
    fn claude_tmux_pane_is_read_only_from_a_pane_id() {
        let _tmux_lock = TMUX_ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        let tmp = TempDir::new().unwrap();
        let (tmux_bin, _, _) = write_fake_tmux(tmp.path(), true);
        let _env = EnvGuard::set("FL_WIRE_TMUX_BIN", &tmux_bin);
        let pane = |v: serde_json::Value| claude_live_tmux_pane(&serde_json::json!({ "tmux": v }));
        assert_eq!(pane("Felicity:@12.%12".into()).as_deref(), Some("%12"));
        assert_eq!(pane("%7".into()).as_deref(), Some("%7"));
        assert_eq!(pane("Felicity".into()), None);
        assert_eq!(pane("Felicity:@12".into()), None);
        assert_eq!(pane("Felicity:@12.%".into()), None);
        assert_eq!(pane("Felicity:@12.%1;x".into()), None);
        assert_eq!(pane(serde_json::json!(12)), None);
        assert_eq!(claude_live_tmux_pane(&serde_json::json!({})), None);
    }

    #[test]
    fn is_owner_sender_matches_only_the_exact_owner_names() {
        assert!(is_owner_sender("owner"));
        assert!(is_owner_sender("OWNER"));
        assert!(is_owner_sender("lmcfarlin"));
        assert!(!is_owner_sender(" owner"));
        assert!(!is_owner_sender("owner "));
        assert!(!is_owner_sender("owner2"));
        assert!(!is_owner_sender("peer"));
        assert!(!is_owner_sender(""));
    }

    #[test]
    fn clear_or_reforge_tmux_sends_reset_keys_then_prompt() {
        use std::os::unix::fs::PermissionsExt;

        let _tmux_lock = TMUX_ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        let tmp = TempDir::new().unwrap();
        let tmux_out = tmp.path().join("tmux.out");
        let tmux_stdin = tmp.path().join("tmux.stdin");
        let tmux_bin = tmp.path().join("fake-tmux.sh");
        fs::write(
            &tmux_bin,
            format!(
                "#!/bin/sh\necho \"$*\" >> \"{}\"\nif [ \"$1\" = \"display-message\" ]; then\n  echo \"%9\"\nelif [ \"$1\" = \"load-buffer\" ]; then\n  cat > \"{}\"\nfi\n",
                tmux_out.display(),
                tmux_stdin.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&tmux_bin, fs::Permissions::from_mode(0o755)).unwrap();

        let grok = VendorRoute::Grok {
            session_id: "gs-1".into(),
            cwd: None,
            tmux_target: Some("grok:felicia".into()),
            tmux_pane: Some("%9".into()),
        };
        let muse = VendorRoute::Muse {
            session_id: "ms-1".into(),
            cwd: None,
            tmux_target: Some("muse:felicia".into()),
            tmux_pane: None,
        };

        // SAFETY: this test is the only fl-wire test that keeps FL_WIRE_TMUX_BIN set
        // for clear/reforge. The guard restores the previous value on drop.
        let _env = EnvGuard::set("FL_WIRE_TMUX_BIN", &tmux_bin);
        clear_or_reforge_session(&grok, SessionResetMode::Clear, "owner", None).unwrap();
        clear_or_reforge_session(
            &muse,
            SessionResetMode::Reforge,
            "owner",
            Some("reforge muse now"),
        )
        .unwrap();
        drop(_env);

        let log = fs::read_to_string(&tmux_out).unwrap();
        assert!(log.contains("send-keys -t %9 C-u"));
        assert!(log.contains("send-keys -t %9 /new"));
        assert!(log.contains("send-keys -t %9 Enter"));
        assert!(log.contains("send-keys -t muse:felicia C-u"));
        assert!(log.contains("send-keys -t muse:felicia /clear"));
        assert!(log.contains("send-keys -t muse:felicia Enter"));
        assert!(log.contains("load-buffer -b fl-wire-muse-"));
        let pasted = fs::read_to_string(&tmux_stdin).unwrap();
        assert_eq!(pasted.trim(), "reforge muse now");
        let reset_at = log.find("send-keys -t muse:felicia /clear").unwrap();
        let paste_at = log.find("load-buffer -b fl-wire-muse-").unwrap();
        assert!(reset_at < paste_at);
    }

    #[test]
    fn plan_interrupt_picks_targets_per_vendor_and_refuses_unreachable_routes() {
        let grok = VendorRoute::Grok {
            session_id: "gs".into(),
            cwd: None,
            tmux_target: Some("grok:felicia".into()),
            tmux_pane: Some("%9".into()),
        };
        assert_eq!(
            plan_interrupt(&grok, Some("Felicia:grok")).unwrap(),
            InterruptPlan::TmuxKey {
                targets: vec!["%9".into(), "grok:felicia".into(), "Felicia:grok".into()],
                key: "Escape",
            }
        );
        let muse = VendorRoute::Muse {
            session_id: "ms".into(),
            cwd: None,
            tmux_target: Some("muse:felicia".into()),
            tmux_pane: Some("muse:felicia".into()),
        };
        assert_eq!(
            plan_interrupt(&muse, None).unwrap(),
            InterruptPlan::TmuxKey {
                targets: vec!["muse:felicia".into()],
                key: "Escape",
            }
        );
        let claude = VendorRoute::Claude {
            pid: 0,
            session_id: "cs".into(),
            socket_path: PathBuf::from("/nonexistent.sock"),
            key_path: None,
        };
        assert!(plan_interrupt(&claude, None).is_err());
        assert!(plan_interrupt(&claude, Some("   ")).is_err());
        let plan = plan_interrupt(&claude, Some("Felicia:claude")).unwrap();
        assert_eq!(plan.method(), INTERRUPT_METHOD_TMUX);
        let codex = VendorRoute::Codex {
            thread_id: "t".into(),
            cwd: None,
        };
        assert!(plan_interrupt(&codex, None).is_err());
        let webhook = VendorRoute::Webhook {
            socket_path: PathBuf::from("/nonexistent.sock"),
        };
        assert!(plan_interrupt(&webhook, Some("x:y")).is_err());
        let bad_agy = VendorRoute::Agy {
            conversation_id: "../etc".into(),
            ls_pid: None,
            ls_address: None,
            csrf_token: None,
            agentapi_exe: None,
        };
        assert!(plan_interrupt(&bad_agy, None).is_err());
        let agy = VendorRoute::Agy {
            conversation_id: "conv-1".into(),
            ls_pid: None,
            ls_address: None,
            csrf_token: None,
            agentapi_exe: None,
        };
        assert_eq!(
            plan_interrupt(&agy, None).unwrap().method(),
            INTERRUPT_METHOD_AGY
        );
    }

    #[test]
    fn run_interrupt_sends_one_escape_to_the_first_live_tmux_target() {
        use std::os::unix::fs::PermissionsExt;

        let _tmux_lock = TMUX_ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        let tmp = TempDir::new().unwrap();
        let tmux_out = tmp.path().join("tmux.out");
        let tmux_bin = tmp.path().join("fake-tmux.sh");
        // `display-message` succeeds only for the live pane `%9`.
        fs::write(
            &tmux_bin,
            format!(
                "#!/bin/sh\necho \"$*\" >> \"{}\"\nif [ \"$1\" = \"display-message\" ]; then\n  if [ \"$4\" = \"%9\" ]; then echo \"%9\"; else exit 1; fi\nfi\n",
                tmux_out.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&tmux_bin, fs::Permissions::from_mode(0o755)).unwrap();

        let _env = EnvGuard::set("FL_WIRE_TMUX_BIN", &tmux_bin);
        let plan = InterruptPlan::TmuxKey {
            targets: vec!["dead:pane".into(), "%9".into(), "other:pane".into()],
            key: "Escape",
        };
        run_interrupt(&plan).unwrap();
        let none_alive = InterruptPlan::TmuxKey {
            targets: vec!["dead:pane".into()],
            key: "Escape",
        };
        assert!(run_interrupt(&none_alive).is_err());
        drop(_env);

        let log = fs::read_to_string(&tmux_out).unwrap();
        let sends: Vec<&str> = log.lines().filter(|l| l.starts_with("send-keys")).collect();
        assert_eq!(sends, vec!["send-keys -t %9 Escape"], "{log}");
        assert!(!log.contains("other:pane"), "{log}");
        assert!(!log.contains("C-c"), "{log}");
    }

    #[test]
    fn run_interrupt_calls_cancel_cascade_on_a_local_mock_language_server() {
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (req_tx, req_rx) = std::sync::mpsc::channel::<String>();
        std::thread::spawn(move || {
            // The first connection is `agy_ls_address_listening`'s probe; it sends nothing.
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                stream
                    .set_read_timeout(Some(Duration::from_millis(500)))
                    .unwrap();
                let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                let mut headers = String::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        break;
                    }
                    let end = line == "\r\n";
                    headers.push_str(&line);
                    if end {
                        break;
                    }
                }
                if headers.is_empty() {
                    continue;
                }
                let len = headers
                    .lines()
                    .find_map(|l| {
                        l.split_once(':').and_then(|(n, v)| {
                            n.eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse::<usize>().ok())
                                .flatten()
                        })
                    })
                    .unwrap_or(0);
                let mut body = vec![0u8; len];
                reader.read_exact(&mut body).unwrap();
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                    )
                    .unwrap();
                req_tx
                    .send(format!("{headers}{}", String::from_utf8_lossy(&body)))
                    .unwrap();
                break;
            }
        });

        let plan = InterruptPlan::AgyCancel {
            conversation_id: "conv-1".into(),
            ls_address: Some(format!("127.0.0.1:{port}")),
            csrf_token: Some("csrf-test".into()),
        };
        run_interrupt(&plan).unwrap();
        let captured = req_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(captured.starts_with(
            "POST /exa.language_server_pb.LanguageServerService/CancelCascadeInvocation HTTP/1.1\r\n"
        ));
        assert!(captured.contains("x-codeium-csrf-token: csrf-test\r\n"));
        let payload: serde_json::Value =
            serde_json::from_str(captured.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(payload, serde_json::json!({ "cascadeId": "conv-1" }));

        // A language server that is not running, or a missing token, is an error.
        let no_token = InterruptPlan::AgyCancel {
            conversation_id: "conv-1".into(),
            ls_address: Some(format!("127.0.0.1:{port}")),
            csrf_token: None,
        };
        let _guard = AGY_ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        let _no_env = EnvGuard::remove("ANTIGRAVITY_CSRF_TOKEN");
        let _no_addr = EnvGuard::remove("ANTIGRAVITY_LS_ADDRESS");
        assert!(run_interrupt(&no_token).is_err());
    }

    #[test]
    fn clear_or_reforge_codex_sends_new_then_prompt() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = TempDir::new().unwrap();
        let out = tmp.path().join("codex.out");
        let bin = tmp.path().join("fake-codex.sh");
        fs::write(
            &bin,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" >> \"{}\"\n",
                out.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();

        // SAFETY: restores FL_WIRE_CODEX_BIN when the guard drops.
        let _env = EnvGuard::set("FL_WIRE_CODEX_BIN", &bin);
        clear_or_reforge_session(
            &VendorRoute::Codex {
                thread_id: "thread-7".into(),
                cwd: None,
            },
            SessionResetMode::Reforge,
            "owner",
            Some("reforge codex"),
        )
        .unwrap();
        drop(_env);

        let log = fs::read_to_string(&out).unwrap();
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(lines[0], "queue");
        assert_eq!(lines[1], "--thread");
        assert_eq!(lines[2], "thread-7");
        assert_eq!(lines[3], "--message");
        assert_eq!(lines[4], "/new");
        assert_eq!(lines[5], "queue");
        assert_eq!(lines[9], "reforge codex");
    }

    #[test]
    fn clear_or_reforge_agy_sends_raw_slash_then_prefixed_prompt() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = TempDir::new().unwrap();
        let out = tmp.path().join("agy-args.txt");
        let bin = tmp.path().join("fake-agy.sh");
        fs::write(
            &bin,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" >> \"{}\"\nprintf '%s\\n' '--CALL--' >> \"{}\"\n",
                out.display(),
                out.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();

        let _agy_lock = AGY_ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        // SAFETY: restores FL_WIRE_AGY_BIN and the Antigravity endpoint variables.
        // A mock binary with no csrf token keeps the `/clear` script path.
        let _bin_env = EnvGuard::set("FL_WIRE_AGY_BIN", &bin);
        let _ls_env = EnvGuard::remove("ANTIGRAVITY_LS_ADDRESS");
        let _csrf_env = EnvGuard::remove("ANTIGRAVITY_CSRF_TOKEN");
        clear_or_reforge_session(
            &VendorRoute::Agy {
                conversation_id: "conv-felicia".into(),
                ls_pid: None,
                ls_address: None,
                csrf_token: None,
                agentapi_exe: Some(bin),
            },
            SessionResetMode::Reforge,
            "owner",
            Some("reforge agy now"),
        )
        .unwrap();
        drop(_csrf_env);
        drop(_ls_env);
        drop(_bin_env);
        drop(_agy_lock);

        let log = fs::read_to_string(&out).unwrap();
        let mut calls = log.split("--CALL--");
        let first = calls.next().expect("slash invocation");
        let second = calls.next().expect("reforge invocation");
        let first_args: Vec<&str> = first.lines().filter(|line| !line.is_empty()).collect();
        assert_eq!(
            first_args.last().copied(),
            Some("/clear"),
            "slash must be delivered raw, got {first}"
        );
        assert!(
            !first.contains("[Operator Prompt via FerroDeck — from"),
            "reset slash must not be wrapped: {first}"
        );
        let slash_at = log.find("\n/clear\n").expect("raw /clear");
        let prefix_at = log
            .find("[Operator Prompt via FerroDeck — from owner]")
            .expect("operator prefix");
        assert!(slash_at < prefix_at);
        assert!(second.contains("reforge agy now"), "{second}");
    }

    #[test]
    fn deliver_to_agy_skips_dead_ls_address_unless_mock_bin() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = TempDir::new().unwrap();
        let out = tmp.path().join("agy-env.txt");
        let bin = tmp.path().join("fake-agy.sh");
        fs::write(
            &bin,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$ANTIGRAVITY_LS_ADDRESS\" >> \"{}\"\n",
                out.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();

        let _agy_lock = AGY_ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        let ls_address = Some("127.0.0.1:1".to_string());
        let exe = Some(bin.clone());
        deliver_to_agy_route("conv-felicia", &ls_address, &None, &exe, "title", "body").unwrap();

        let recorded = fs::read_to_string(&out).unwrap();
        assert!(
            recorded.lines().all(|line| line != "127.0.0.1:1"),
            "dead route address must not be forwarded, got {recorded:?}"
        );

        // SAFETY: restores FL_WIRE_AGY_BIN when the guard drops. A set mock
        // binary may forward a language-server address that is not listening.
        let _env = EnvGuard::set("FL_WIRE_AGY_BIN", &bin);
        deliver_to_agy_route("conv-felicia", &ls_address, &None, &exe, "title", "body").unwrap();
        drop(_env);

        let recorded = fs::read_to_string(&out).unwrap();
        assert_eq!(recorded.lines().last(), Some("127.0.0.1:1"));
        drop(_agy_lock);
    }

    #[test]
    fn clear_or_reforge_rejects_blank_reforge_prompt_before_dispatch() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = TempDir::new().unwrap();
        let out = tmp.path().join("agy-args.txt");
        let bin = tmp.path().join("fake-agy.sh");
        fs::write(
            &bin,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" >> \"{}\"\n",
                out.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();

        let route = VendorRoute::Agy {
            conversation_id: "conv-felicia".into(),
            ls_pid: None,
            ls_address: None,
            csrf_token: None,
            agentapi_exe: Some(bin),
        };
        let missing = clear_or_reforge_session(&route, SessionResetMode::Reforge, "owner", None);
        assert_eq!(missing.unwrap_err(), "missing reforge prompt");
        let blank =
            clear_or_reforge_session(&route, SessionResetMode::Reforge, "owner", Some("  "));
        assert_eq!(blank.unwrap_err(), "missing reforge prompt");
        let body = fs::read_to_string(&out).unwrap_or_default();
        assert!(
            body.is_empty(),
            "vendor binary must not run when the reforge prompt is missing, got {body:?}"
        );
    }

    #[test]
    fn clear_or_reforge_agy_reverts_cascade_and_truncates_transcripts() {
        use std::io::{BufRead, Read, Write};
        use std::net::TcpListener;
        use std::os::unix::fs::PermissionsExt;
        use std::sync::mpsc;

        let _agy_lock = AGY_ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        let _ls_env = EnvGuard::remove("ANTIGRAVITY_LS_ADDRESS");
        let _csrf_env = EnvGuard::remove("ANTIGRAVITY_CSRF_TOKEN");
        let _bin_env = EnvGuard::remove("FL_WIRE_AGY_BIN");

        let missing = VendorRoute::Agy {
            conversation_id: "conv-missing".into(),
            ls_pid: None,
            ls_address: Some("not-a-port".into()),
            csrf_token: Some("csrf-test".into()),
            agentapi_exe: None,
        };
        let parse_err =
            clear_or_reforge_session(&missing, SessionResetMode::Clear, "owner", None).unwrap_err();
        assert!(parse_err.contains("could not parse a port"), "{parse_err}");

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let no_csrf = VendorRoute::Agy {
            conversation_id: "conv-no-csrf".into(),
            ls_pid: None,
            ls_address: Some(format!("127.0.0.1:{port}")),
            csrf_token: None,
            agentapi_exe: None,
        };
        let csrf_err =
            clear_or_reforge_session(&no_csrf, SessionResetMode::Clear, "owner", None).unwrap_err();
        assert!(csrf_err.contains("csrf token"), "{csrf_err}");

        let (status_tx, status_rx) = mpsc::channel::<(u16, String)>();
        let (req_tx, req_rx) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            while let Ok((code, resp_body)) = status_rx.recv() {
                // `agy_ls_address_listening` connects and drops before the POST.
                // Skip that probe and answer the next request that carries headers.
                loop {
                    let (mut stream, _) = listener.accept().unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_millis(200)))
                        .unwrap();
                    let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                    let mut headers = String::new();
                    let mut saw_request = false;
                    loop {
                        let mut line = String::new();
                        match reader.read_line(&mut line) {
                            Ok(0) => break,
                            Ok(_) => {
                                saw_request = true;
                                headers.push_str(&line);
                                if line == "\r\n" {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    if !saw_request || !headers.contains("\r\n\r\n") {
                        continue;
                    }
                    let len = headers
                        .lines()
                        .find_map(|line| {
                            line.split_once(':').and_then(|(name, value)| {
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().ok())
                                    .flatten()
                            })
                        })
                        .unwrap_or(0);
                    let mut body = vec![0u8; len];
                    if reader.read_exact(&mut body).is_err() {
                        continue;
                    }
                    let captured = format!("{headers}{}", String::from_utf8_lossy(&body));
                    let reason = if code == 200 { "OK" } else { "ERR" };
                    let response = if let Some(chunk_payload) = resp_body.strip_prefix("CHUNKED:") {
                        format!(
                            "HTTP/1.1 {code} {reason}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{chunk_payload}\r\n0\r\n\r\n",
                            chunk_payload.len()
                        )
                    } else {
                        format!(
                            "HTTP/1.1 {code} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{resp_body}",
                            resp_body.len()
                        )
                    };
                    stream
                        .set_write_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    stream.write_all(response.as_bytes()).unwrap();
                    req_tx.send(captured).unwrap();
                    break;
                }
            }
        });

        let home = std::env::var("HOME").expect("HOME");
        let conversation_id = format!(
            "fl-wire-revert-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let logs = PathBuf::from(&home)
            .join(".gemini")
            .join("antigravity-cli")
            .join("brain")
            .join(&conversation_id)
            .join(".system_generated")
            .join("logs");
        fs::create_dir_all(&logs).unwrap();
        struct RemoveDir(PathBuf);
        impl Drop for RemoveDir {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = RemoveDir(logs.parent().unwrap().parent().unwrap().to_path_buf());
        let transcript = logs.join("transcript.jsonl");
        let full = logs.join("transcript_full.jsonl");
        let step_zero = "{\"step_index\":0,\"type\":\"USER_INPUT\",\"content\":\"keep me\"}\n";
        let write_logs = || {
            fs::write(
                &transcript,
                format!("{step_zero}{{\"step_index\":4,\"type\":\"PLANNER_RESPONSE\",\"content\":\"old\"}}\n"),
            )
            .unwrap();
            fs::write(
                &full,
                "{\"step_index\":4,\"type\":\"PLANNER_RESPONSE\",\"content\":\"drop\"}\n",
            )
            .unwrap();
        };
        write_logs();

        let tmp = TempDir::new().unwrap();
        let out = tmp.path().join("agy-args.txt");
        let bin = tmp.path().join("fake-agy.sh");
        fs::write(
            &bin,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" >> \"{}\"\n",
                out.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        let route = VendorRoute::Agy {
            conversation_id: conversation_id.clone(),
            ls_pid: None,
            ls_address: Some(format!("127.0.0.1:{port}")),
            csrf_token: Some("csrf-test".into()),
            agentapi_exe: Some(bin),
        };

        status_tx.send((200, "{}".into())).unwrap();
        clear_or_reforge_session(&route, SessionResetMode::Clear, "owner", None).unwrap();
        let captured = req_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(captured.starts_with(
            "POST /exa.language_server_pb.LanguageServerService/RevertToCascadeStep HTTP/1.1\r\n"
        ));
        assert!(captured.contains("Content-Type: application/json\r\n"));
        assert!(captured.contains("x-codeium-csrf-token: csrf-test\r\n"));
        let body = captured.split("\r\n\r\n").nth(1).expect("request body");
        let payload: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(payload["cascadeId"], conversation_id);
        assert_eq!(payload["stepIndex"], 0);
        assert_eq!(payload["conversationOnly"], true);
        assert_eq!(
            payload["overrideConfig"]["plannerConfig"]["planModel"],
            "MODEL_PLACEHOLDER_M418"
        );
        assert_eq!(
            payload["overrideConfig"]["plannerConfig"]["requestedModel"]["model"],
            "MODEL_PLACEHOLDER_M418"
        );
        assert_eq!(fs::read_to_string(&transcript).unwrap(), step_zero);
        assert_eq!(fs::read_to_string(&full).unwrap(), "");
        assert!(!out.exists() || fs::read_to_string(&out).unwrap().is_empty());

        write_logs();
        status_tx
            .send((
                500,
                "{\"code\":\"unknown\",\"message\":\"plan model not specified\"}".into(),
            ))
            .unwrap();
        let padding = "x".repeat(6000);
        status_tx
            .send((
                200,
                format!(
                    "CHUNKED:{{\"clientModelConfigs\":[\"{padding}\"],\"defaultOverrideModelConfig\":{{\"modelOrAlias\":{{\"model\":\"MODEL_PLACEHOLDER_M417\"}}}}}}"
                ),
            ))
            .unwrap();
        status_tx.send((200, "{}".into())).unwrap();
        clear_or_reforge_session(&route, SessionResetMode::Clear, "owner", None).unwrap();
        let _first = req_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let model_cfg_req = req_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(model_cfg_req.starts_with(
            "POST /exa.language_server_pb.LanguageServerService/GetCascadeModelConfigData HTTP/1.1\r\n"
        ));
        let retry_req = req_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let retry_body = retry_req.split("\r\n\r\n").nth(1).expect("retry body");
        let retry_payload: serde_json::Value = serde_json::from_str(retry_body).unwrap();
        assert_eq!(
            retry_payload["overrideConfig"]["plannerConfig"]["planModel"],
            "MODEL_PLACEHOLDER_M417"
        );
        assert_eq!(fs::read_to_string(&transcript).unwrap(), step_zero);

        write_logs();
        status_tx.send((200, "{}".into())).unwrap();
        clear_or_reforge_session(
            &route,
            SessionResetMode::Reforge,
            "owner",
            Some("reforge agy now"),
        )
        .unwrap();
        let captured = req_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(captured.contains("\"stepIndex\":0"));
        assert_eq!(fs::read_to_string(&transcript).unwrap(), step_zero);
        let prompt_log = fs::read_to_string(&out).unwrap();
        assert!(prompt_log.contains("reforge agy now"), "{prompt_log}");
        assert!(!prompt_log.contains("\n/clear\n"), "{prompt_log}");

        write_logs();
        let before = fs::read_to_string(&transcript).unwrap();
        status_tx
            .send((
                500,
                "{\"code\":\"internal\",\"message\":\"database locked\"}".into(),
            ))
            .unwrap();
        let failed =
            clear_or_reforge_session(&route, SessionResetMode::Clear, "owner", None).unwrap_err();
        assert!(failed.contains("500"), "{failed}");
        assert!(failed.contains("database locked"), "{failed}");
        assert_eq!(fs::read_to_string(&transcript).unwrap(), before);
        drop(_bin_env);
        drop(_csrf_env);
        drop(_ls_env);
        drop(_agy_lock);
    }

    struct EnvGuard {
        key: &'static str,
        prev: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &std::path::Path) -> Self {
            let prev = std::env::var_os(key);
            // SAFETY: the guard restores the previous value on drop. Callers hold
            // the matching env lock for the whole test.
            unsafe {
                std::env::set_var(key, value);
            }
            Self { key, prev }
        }

        fn remove(key: &'static str) -> Self {
            let prev = std::env::var_os(key);
            // SAFETY: the guard restores the previous value on drop. Callers hold
            // the matching env lock for the whole test.
            unsafe {
                std::env::remove_var(key);
            }
            Self { key, prev }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                match self.prev.take() {
                    Some(value) => std::env::set_var(self.key, value),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }
}
