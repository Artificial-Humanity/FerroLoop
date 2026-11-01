//! 5-Provider & Runtime Testing Matrix (Project Record 17).
//!
//! `GET /api/v1/runtimes/matrix` and `POST /api/v1/runtimes/probe` report three tiers:
//!
//! 1. subscription CLI runtimes (`claude`, `codex`, `grok`, `agy`, `muse`): binary discovery,
//!    a bounded `--version` probe, active route count and live session count;
//! 2. API-key providers held in the `FerroGate` vault: whether each provider secret is
//!    provisioned and unexpired. Secret values are never read, only metadata;
//! 3. local inference engines (Ollama, Lemonade) on loopback: reachability, latency and models.
//!
//! Every probe is bounded. The three tiers and every item inside them run concurrently, so the
//! worst case is one probe timeout, not the sum of all of them. Probing is owner only and goes
//! through a [`ProbeGate`]: one probe at a time, and a fresh result is served again.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, PoisonError, mpsc};
use std::time::{Duration, Instant};

use fl_exec::stamp;
use fl_gate::{MASTER_KEY_LEN, Vault, VaultSecretMetadata, resolve_vault_path};
use serde::Serialize;
use serde_json::{Value, json};

use crate::HubState;
use crate::api::{HttpRequest, HttpResponse};
use crate::auth::HubAuthority;
use crate::discovery::{
    DiscoveredSessionSummary, discover_route_sessions_without_model_info, snapshot_discovery_store,
};

/// Subscription CLI runtimes, in display order.
pub const CLI_RUNTIMES: [&str; 5] = ["claude", "codex", "grok", "agy", "muse"];
/// `(provider, vault secret name)` pairs.
pub const VAULT_PROVIDERS: [(&str, &str); 5] = [
    ("anthropic", "ANTHROPIC_API_KEY"),
    ("openai", "OPENAI_API_KEY"),
    ("xai", "XAI_API_KEY"),
    ("gemini", "GEMINI_API_KEY"),
    ("openrouter", "OPENROUTER_API_KEY"),
];

const VERSION_TIMEOUT: Duration = Duration::from_secs(2);
const LOCAL_TIMEOUT: Duration = Duration::from_millis(500);
const MAX_PROBE_OUTPUT: usize = 4096;
const MAX_LOCAL_BODY: usize = 256 * 1024;
const MAX_LOCAL_MODELS: usize = 100;
const MAX_TARGET_LEN: usize = 64;
/// A probe result younger than this is served again for the same target, so a burst of requests
/// spawns each CLI once.
const PROBE_MIN_INTERVAL: Duration = Duration::from_secs(2);

/// Loopback inference engine to probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalEngineSpec {
    pub name: String,
    pub port: u16,
    pub path: String,
    pub kind: LocalEngineKind,
}

/// Shape of the model-list response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalEngineKind {
    /// `{"models":[{"name":"..."}]}`
    Ollama,
    /// `{"data":[{"id":"..."}]}`
    OpenAiModels,
}

pub fn default_local_engines() -> Vec<LocalEngineSpec> {
    vec![
        LocalEngineSpec {
            name: "ollama".into(),
            port: 11434,
            path: "/api/tags".into(),
            kind: LocalEngineKind::Ollama,
        },
        LocalEngineSpec {
            name: "lemonade-8000".into(),
            port: 8000,
            path: "/v1/models".into(),
            kind: LocalEngineKind::OpenAiModels,
        },
        LocalEngineSpec {
            name: "lemonade-8097".into(),
            port: 8097,
            path: "/v1/models".into(),
            kind: LocalEngineKind::OpenAiModels,
        },
    ]
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CliRuntimeStatus {
    pub name: String,
    pub installed: bool,
    pub path: Option<String>,
    pub version: Option<String>,
    pub error: Option<String>,
    pub active_routes: usize,
    pub live_sessions: usize,
    pub probe_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VaultProviderStatus {
    pub provider: String,
    pub secret_name: String,
    pub provisioned: bool,
    pub expired: bool,
    /// `ready`, `expired` or `missing`.
    pub status: String,
    pub expires_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LocalEngineStatus {
    pub name: String,
    pub endpoint: String,
    pub reachable: bool,
    pub latency_ms: Option<u64>,
    pub models: Vec<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuntimeMatrix {
    pub generated_at_ms: u64,
    pub cli_runtimes: Vec<CliRuntimeStatus>,
    pub vault_providers: Vec<VaultProviderStatus>,
    pub local_engines: Vec<LocalEngineStatus>,
    /// Vault failure (for example an unreadable vault file). The tier is then empty of
    /// `provisioned` secrets rather than the whole matrix failing.
    pub vault_error: Option<String>,
}

/// Everything a probe reads from outside itself. Tests build this by hand.
pub struct ProbeInputs {
    pub path_env: String,
    pub home: Option<PathBuf>,
    pub sessions: Vec<DiscoveredSessionSummary>,
    pub vault: Result<Vec<VaultSecretMetadata>, String>,
    pub local_engines: Vec<LocalEngineSpec>,
    pub now_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tier {
    Cli,
    Vault,
    Local,
}

/// Which tiers and items a `target` filter selects.
struct Selection {
    cli: Vec<&'static str>,
    vault: Vec<(&'static str, &'static str)>,
    local: Vec<LocalEngineSpec>,
}

fn tier_for(target: &str) -> Option<Tier> {
    match target {
        "cli" | "cli-runtimes" => Some(Tier::Cli),
        "vault" | "vault-api-keys" => Some(Tier::Vault),
        "local" | "local-engines" => Some(Tier::Local),
        _ => None,
    }
}

fn select(target: Option<&str>, engines: &[LocalEngineSpec]) -> Result<Selection, String> {
    let all = Selection {
        cli: CLI_RUNTIMES.to_vec(),
        vault: VAULT_PROVIDERS.to_vec(),
        local: engines.to_vec(),
    };
    let Some(raw) = target else {
        return Ok(all);
    };
    let target = raw.trim().to_ascii_lowercase();
    if target.is_empty() {
        return Ok(all);
    }
    if target.len() > MAX_TARGET_LEN {
        return Err("`target` is too long".to_string());
    }
    let mut sel = Selection {
        cli: Vec::new(),
        vault: Vec::new(),
        local: Vec::new(),
    };
    if let Some(tier) = tier_for(&target) {
        match tier {
            Tier::Cli => sel.cli = all.cli,
            Tier::Vault => sel.vault = all.vault,
            Tier::Local => sel.local = all.local,
        }
        return Ok(sel);
    }
    sel.cli = all.cli.into_iter().filter(|n| *n == target).collect();
    sel.vault = all
        .vault
        .into_iter()
        .filter(|(p, s)| *p == target || s.eq_ignore_ascii_case(&target))
        .collect();
    sel.local = all.local.into_iter().filter(|e| e.name == target).collect();
    if sel.cli.is_empty() && sel.vault.is_empty() && sel.local.is_empty() {
        return Err(format!("unknown probe target `{target}`"));
    }
    Ok(sel)
}

/// Run the selected probes. Unknown `target` is an error; no target selects everything.
pub fn build_matrix(inputs: &ProbeInputs, target: Option<&str>) -> Result<RuntimeMatrix, String> {
    let sel = select(target, &inputs.local_engines)?;
    let (cli_runtimes, local_engines) = std::thread::scope(|scope| {
        let cli_handles: Vec<_> = sel
            .cli
            .iter()
            .map(|name| scope.spawn(move || probe_cli(name, inputs, VERSION_TIMEOUT)))
            .collect();
        let local_handles: Vec<_> = sel
            .local
            .iter()
            .map(|spec| scope.spawn(move || probe_local_engine(spec, LOCAL_TIMEOUT)))
            .collect();
        let cli: Vec<CliRuntimeStatus> = cli_handles
            .into_iter()
            .zip(sel.cli.iter())
            .map(|(h, name)| h.join().unwrap_or_else(|_| panicked_cli(name)))
            .collect();
        let local: Vec<LocalEngineStatus> = local_handles
            .into_iter()
            .zip(sel.local.iter())
            .map(|(h, spec)| h.join().unwrap_or_else(|_| panicked_local(spec)))
            .collect();
        (cli, local)
    });
    let (vault_providers, vault_error) = match &inputs.vault {
        Ok(meta) => (vault_statuses(&sel.vault, meta, inputs.now_ms), None),
        Err(err) => (
            vault_statuses(&sel.vault, &[], inputs.now_ms),
            Some(err.clone()),
        ),
    };
    Ok(RuntimeMatrix {
        generated_at_ms: inputs.now_ms,
        cli_runtimes,
        vault_providers,
        local_engines,
        vault_error,
    })
}

fn panicked_cli(name: &str) -> CliRuntimeStatus {
    CliRuntimeStatus {
        name: name.to_string(),
        installed: false,
        path: None,
        version: None,
        error: Some("probe panicked".to_string()),
        active_routes: 0,
        live_sessions: 0,
        probe_ms: 0,
    }
}

fn panicked_local(spec: &LocalEngineSpec) -> LocalEngineStatus {
    LocalEngineStatus {
        name: spec.name.clone(),
        endpoint: endpoint_of(spec),
        reachable: false,
        latency_ms: None,
        models: Vec::new(),
        error: Some("probe panicked".to_string()),
    }
}

// ---------------------------------------------------------------- tier 1: CLI runtimes

/// First executable `name` in `path_env` directories, then `<home>/.local/bin`.
pub fn find_binary(name: &str, path_env: &str, home: Option<&Path>) -> Option<PathBuf> {
    let from_path = path_env
        .split(':')
        .filter(|dir| !dir.is_empty())
        .map(|dir| Path::new(dir).join(name));
    let from_home = home.map(|h| h.join(".local").join("bin").join(name));
    from_path.chain(from_home).find(|p| is_executable_file(p))
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn probe_cli(name: &str, inputs: &ProbeInputs, timeout: Duration) -> CliRuntimeStatus {
    let started = Instant::now();
    let active_routes = inputs
        .sessions
        .iter()
        .filter(|s| s.adapter == name && s.source == "route")
        .count();
    let live_sessions = inputs
        .sessions
        .iter()
        .filter(|s| s.adapter == name && matches!(s.status.as_str(), "active" | "running"))
        .count();
    let mut status = CliRuntimeStatus {
        name: name.to_string(),
        installed: false,
        path: None,
        version: None,
        error: None,
        active_routes,
        live_sessions,
        probe_ms: 0,
    };
    match find_binary(name, &inputs.path_env, inputs.home.as_deref()) {
        None => status.error = Some(format!("`{name}` not found in PATH or ~/.local/bin")),
        Some(path) => {
            status.installed = true;
            status.path = Some(path.display().to_string());
            match run_version(&path, timeout) {
                Ok(version) => status.version = Some(version),
                Err(err) => status.error = Some(err),
            }
        }
    }
    status.probe_ms = elapsed_ms(started);
    status
}

/// `<path> --version` with a hard deadline. The child leads its own process group, and the whole
/// group is killed on timeout and after exit, so a wrapper script cannot leave a grandchild behind.
/// The group is only signalled while the leader is still unreaped (see [`exited_unreaped`]).
pub fn run_version(path: &Path, timeout: Duration) -> Result<String, String> {
    let mut cmd = Command::new(path);
    cmd.arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().map_err(|e| format!("spawn failed: {e}"))?;
    let pid = child.id();
    let out_rx = drain_pipe(child.stdout.take());
    let err_rx = drain_pipe(child.stderr.take());

    let deadline = Instant::now() + timeout;
    let exited = loop {
        match exited_unreaped(&mut child) {
            Ok(true) => break true,
            Ok(false) if Instant::now() >= deadline => break false,
            Ok(false) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => {
                // The state of the leader is unknown, so no group signal: it may be reaped.
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("wait failed: {e}"));
            }
        }
    };
    let exit = kill_group(pid, &mut child);
    if !exited {
        return Err(format!(
            "--version timed out after {}ms",
            timeout.as_millis()
        ));
    }
    let Ok(exit) = exit else {
        return Err("wait failed".to_string());
    };
    let stdout = out_rx
        .recv_timeout(Duration::from_millis(200))
        .unwrap_or_default();
    let stderr = err_rx
        .recv_timeout(Duration::from_millis(200))
        .unwrap_or_default();
    if !exit.success() {
        return Err(format!("--version exited with {exit}"));
    }
    let text = [stdout, stderr].iter().find_map(|s| {
        s.lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .map(str::to_string)
    });
    match text {
        Some(line) => Ok(line.chars().take(200).collect()),
        None => Err("--version printed nothing".to_string()),
    }
}

fn drain_pipe<R: Read + Send + 'static>(pipe: Option<R>) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(pipe) = pipe {
            let _ = pipe.take(MAX_PROBE_OUTPUT as u64).read_to_end(&mut buf);
        }
        let _ = tx.send(String::from_utf8_lossy(&buf).into_owned());
    });
    rx
}

/// Whether the child has exited, without reaping it. An exited but unreaped leader is a zombie
/// that still owns its pid, so `kill_group` can signal `-pid` knowing no other process holds that
/// id; once a leader is reaped (`try_wait`, `wait`) and its group is empty, the id can be reused.
#[cfg(unix)]
fn exited_unreaped(child: &mut std::process::Child) -> std::io::Result<bool> {
    // SAFETY: `waitid` with `WNOHANG | WNOWAIT` only fills `info` and leaves the child waitable;
    // an all-zero `siginfo_t` is a valid value to pass in. `si_pid` stays 0 while the child runs.
    unsafe {
        let mut info: libc::siginfo_t = std::mem::zeroed();
        let rc = libc::waitid(
            libc::P_PID,
            child.id() as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        );
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(info.si_pid() != 0)
    }
}

#[cfg(not(unix))]
fn exited_unreaped(child: &mut std::process::Child) -> std::io::Result<bool> {
    child.try_wait().map(|status| status.is_some())
}

/// SIGKILL the child's process group, then reap the child. Call it only while the leader is
/// unreaped, so `-pid` still names the child's own group.
fn kill_group(
    pid: u32,
    child: &mut std::process::Child,
) -> std::io::Result<std::process::ExitStatus> {
    #[cfg(unix)]
    if let Ok(pid) = i32::try_from(pid) {
        // SAFETY: plain `kill(2)` on a negative pid targets the process group the child created
        // with `process_group(0)`. No memory is touched; ESRCH is ignored.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
    child.wait()
}

// ---------------------------------------------------------------- tier 2: vault

/// Vault status per provider, from metadata only. Secret values are never decrypted.
pub fn vault_statuses(
    providers: &[(&str, &str)],
    metadata: &[VaultSecretMetadata],
    now_ms: u64,
) -> Vec<VaultProviderStatus> {
    let now = fl_core::At::from_unix_millis(now_ms).to_string();
    providers
        .iter()
        .map(|(provider, secret)| {
            let meta = metadata
                .iter()
                .find(|m| m.name.eq_ignore_ascii_case(secret));
            // Same lexicographic comparison the vault itself applies on read.
            let expired = meta
                .and_then(|m| m.expires_at.as_deref())
                .is_some_and(|exp| now.as_str() > exp);
            let status = match meta {
                None => "missing",
                Some(_) if expired => "expired",
                Some(_) => "ready",
            };
            VaultProviderStatus {
                provider: (*provider).to_string(),
                secret_name: (*secret).to_string(),
                provisioned: meta.is_some(),
                expired,
                status: status.to_string(),
                expires_at: meta.and_then(|m| m.expires_at.clone()),
            }
        })
        .collect()
}

// ---------------------------------------------------------------- tier 3: local engines

fn endpoint_of(spec: &LocalEngineSpec) -> String {
    format!("127.0.0.1:{}{}", spec.port, spec.path)
}

pub fn probe_local_engine(spec: &LocalEngineSpec, timeout: Duration) -> LocalEngineStatus {
    let started = Instant::now();
    let mut status = LocalEngineStatus {
        name: spec.name.clone(),
        endpoint: endpoint_of(spec),
        reachable: false,
        latency_ms: None,
        models: Vec::new(),
        error: None,
    };
    match loopback_get(spec.port, &spec.path, timeout) {
        Ok((code, body)) if (200..300).contains(&code) => {
            status.reachable = true;
            status.latency_ms = Some(elapsed_ms(started));
            status.models = parse_model_ids(spec.kind, &body);
        }
        Ok((code, _)) => {
            status.latency_ms = Some(elapsed_ms(started));
            status.error = Some(format!("HTTP {code}"));
        }
        Err(err) => status.error = Some(err),
    }
    status
}

/// HTTP/1.0 GET to `127.0.0.1:port`. HTTP/1.0 keeps servers from chunking the body.
/// The connect and the whole read share one `timeout` budget.
fn loopback_get(port: u16, path: &str, timeout: Duration) -> Result<(u16, String), String> {
    let deadline = Instant::now() + timeout;
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&addr, timeout).map_err(|e| match e.kind() {
        std::io::ErrorKind::ConnectionRefused => "connection refused".to_string(),
        std::io::ErrorKind::TimedOut => "connect timed out".to_string(),
        _ => format!("connect failed: {e}"),
    })?;
    stream.set_write_timeout(Some(timeout)).ok();
    let request = format!(
        "GET {path} HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|e| format!("write failed: {e}"))?;
    let mut raw = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err("read timed out".to_string());
        }
        stream.set_read_timeout(Some(left)).ok();
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&chunk[..n]);
                if raw.len() > MAX_LOCAL_BODY {
                    break;
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Err("read timed out".to_string());
            }
            Err(e) => return Err(format!("read failed: {e}")),
        }
    }
    parse_http_response(&raw)
}

fn parse_http_response(raw: &[u8]) -> Result<(u16, String), String> {
    let text = String::from_utf8_lossy(raw);
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let code = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or_else(|| "malformed HTTP response".to_string())?;
    Ok((code, body.to_string()))
}

fn parse_model_ids(kind: LocalEngineKind, body: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return Vec::new();
    };
    let (list, keys): (&str, &[&str]) = match kind {
        LocalEngineKind::Ollama => ("models", &["name", "model"]),
        LocalEngineKind::OpenAiModels => ("data", &["id"]),
    };
    value
        .get(list)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    keys.iter()
                        .find_map(|k| item.get(*k).and_then(Value::as_str))
                })
                .take(MAX_LOCAL_MODELS)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn elapsed_ms(since: Instant) -> u64 {
    u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX)
}

// ---------------------------------------------------------------- HTTP handlers

/// Serializes probes and remembers the last result. A probe spawns `<binary> --version` for each
/// CLI, so a second request while one runs gets `429` instead of a second set of children, and a
/// request within [`PROBE_MIN_INTERVAL`] of a finished probe of the same target gets that result.
#[derive(Debug, Default)]
pub struct ProbeGate {
    state: Mutex<GateState>,
}

#[derive(Debug, Default)]
struct GateState {
    running: bool,
    last: Option<CachedProbe>,
}

#[derive(Debug)]
struct CachedProbe {
    at: Instant,
    target: Option<String>,
    body: Value,
}

/// Clears `running` when the probe ends, including by panic.
struct Running<'a>(&'a ProbeGate);

impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.0.lock().running = false;
    }
}

impl ProbeGate {
    fn lock(&self) -> std::sync::MutexGuard<'_, GateState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Serve `target` from the last result, refuse while another probe runs, or run `probe` and
    /// remember its result. An error response is returned as it is and not remembered.
    fn run(
        &self,
        target: Option<&str>,
        probe: impl FnOnce() -> Result<Value, HttpResponse>,
    ) -> HttpResponse {
        // The same normalisation `select` applies, so `Vault` and `vault ` share a result.
        let key = target
            .map(|t| t.trim().to_ascii_lowercase())
            .filter(|t| !t.is_empty());
        {
            let mut state = self.lock();
            if let Some(last) = &state.last
                && last.target == key
                && last.at.elapsed() < PROBE_MIN_INTERVAL
            {
                return HttpResponse::json(200, last.body.clone());
            }
            if state.running {
                return HttpResponse::json(
                    429,
                    json!({ "error": "a probe is already running; retry shortly" }),
                )
                .with_header("Retry-After", "1");
            }
            state.running = true;
        }
        let _running = Running(self);
        match probe() {
            Ok(body) => {
                self.lock().last = Some(CachedProbe {
                    at: Instant::now(),
                    target: key,
                    body: body.clone(),
                });
                HttpResponse::json(200, body)
            }
            Err(resp) => resp,
        }
    }
}

/// Metadata of the vault's secrets, read without the master key: nothing is decrypted, so the
/// key file is neither read nor created, and a missing vault file reads as empty. (`Vault::open`
/// only needs a key to seal and open secret values.)
fn vault_metadata(home_override: Option<&Path>) -> Result<Vec<VaultSecretMetadata>, String> {
    Vault::open(resolve_vault_path(home_override), [0u8; MASTER_KEY_LEN])
        .map(|v| v.list())
        .map_err(|e| e.to_string())
}

pub(crate) fn gather_inputs(state: &HubState) -> Result<ProbeInputs, HttpResponse> {
    // Snapshot under the store guard, then drop it before any filesystem or network probing.
    let snapshot = match state.open_store() {
        Ok(store) => snapshot_discovery_store(&store),
        Err(_) => {
            return Err(HttpResponse::json(
                500,
                json!({ "error": "failed to open store: store unavailable" }),
            ));
        }
    };
    let home = state
        .config
        .home_override
        .clone()
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from));
    let sessions = discover_route_sessions_without_model_info(
        &snapshot,
        state.config.home_override.as_deref(),
    );
    let vault = vault_metadata(state.config.home_override.as_deref());
    // A hub with a `home_override` is sandboxed (its tests): it looks for the vendor CLIs only
    // under that home's `.local/bin`, never on the host's PATH, so a probe cannot run (and
    // write the state of) the real `claude`, `codex`, `grok`, `agy` or `muse` of whoever runs it.
    let path_env = if state.config.home_override.is_some() {
        String::new()
    } else {
        std::env::var("PATH").unwrap_or_default()
    };
    Ok(ProbeInputs {
        path_env,
        home,
        sessions,
        vault,
        local_engines: default_local_engines(),
        now_ms: stamp::now().unix_millis(),
    })
}

fn run_probe(state: &HubState, target: Option<&str>) -> HttpResponse {
    state.probe_gate.run(target, || {
        let inputs = gather_inputs(state)?;
        match build_matrix(&inputs, target) {
            Ok(matrix) => Ok(serde_json::to_value(&matrix).unwrap_or_else(|_| json!({}))),
            Err(error) => Err(HttpResponse::json(400, json!({ "error": error }))),
        }
    })
}

/// `GET /api/v1/runtimes/matrix`: probe every runtime. Owner only, like the POST: a probe spawns
/// `<binary> --version` for binaries found on the hub's PATH.
pub fn handle_get_matrix(state: &HubState, authority: &HubAuthority) -> HttpResponse {
    if !authority.is_owner() {
        return HttpResponse::json(403, json!({ "error": "owner authority required" }));
    }
    run_probe(state, None)
}

/// `POST /api/v1/runtimes/probe` with optional `{ "target": "<tier|runtime|provider|engine>" }`.
/// Owner only: a probe spawns `<binary> --version` for binaries found on the hub's PATH.
pub fn handle_post_probe(
    state: &HubState,
    authority: &HubAuthority,
    req: &HttpRequest,
) -> HttpResponse {
    if !authority.is_owner() {
        return HttpResponse::json(403, json!({ "error": "owner authority required" }));
    }
    let body: Value = if req.body.iter().all(|b| b.is_ascii_whitespace()) {
        json!({})
    } else {
        match serde_json::from_slice(&req.body) {
            Ok(v) => v,
            Err(e) => {
                return HttpResponse::json(
                    400,
                    json!({ "error": format!("invalid JSON body: {e}") }),
                );
            }
        }
    };
    let target = match body.get("target") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => {
            return HttpResponse::json(400, json!({ "error": "`target` must be a string" }));
        }
    };
    run_probe(state, target.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use tempfile::TempDir;

    fn meta(name: &str, expires_at: Option<&str>) -> VaultSecretMetadata {
        VaultSecretMetadata {
            name: name.to_string(),
            description: None,
            allowed_hosts: vec![],
            allowed_methods: vec![],
            allowed_agents: vec![],
            allow_private_network: false,
            created_at: "2026-01-01T00:00:00.000Z".into(),
            updated_at: "2026-01-01T00:00:00.000Z".into(),
            lease_ttl_secs: None,
            expires_at: expires_at.map(str::to_string),
        }
    }

    fn session(adapter: &str, source: &str, status: &str) -> DiscoveredSessionSummary {
        DiscoveredSessionSummary {
            id: format!("{adapter}-{source}-{status}"),
            source: source.into(),
            adapter: adapter.into(),
            persona: None,
            status: status.into(),
            updated_at: None,
            cwd: None,
            record_id: None,
            transcript_path: None,
            quota_usage: None,
            model: None,
            thinking_level: None,
            available_models: Vec::new(),
        }
    }

    #[cfg(unix)]
    fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// `run_version` on a script this test just wrote. Writing a script and executing it races
    /// with every other test thread that forks: a child forked while the script's write fd is
    /// still open holds that fd until its own exec, and `execve` of a file that any process has
    /// open for writing fails with ETXTBSY. The race lives in the fixture, so the retry lives
    /// here: only that exact spawn error is retried, bounded, and every other result is returned.
    #[cfg(unix)]
    fn run_fixture_version(path: &Path, timeout: Duration) -> Result<String, String> {
        let mut attempts = 0;
        loop {
            let result = run_version(path, timeout);
            let busy = matches!(&result, Err(e) if e.starts_with("spawn failed:") && e.contains("Text file busy"));
            attempts += 1;
            if !busy || attempts >= 200 {
                return result;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn inputs(path_env: &str, home: Option<&Path>) -> ProbeInputs {
        ProbeInputs {
            path_env: path_env.to_string(),
            home: home.map(Path::to_path_buf),
            sessions: Vec::new(),
            vault: Ok(Vec::new()),
            local_engines: Vec::new(),
            now_ms: 1_800_000_000_000,
        }
    }

    /// One-shot loopback server answering the first connection with `response`.
    fn serve_once(response: &'static str) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = s.read(&mut buf);
                let _ = s.write_all(response.as_bytes());
            }
        });
        port
    }

    fn spec(name: &str, port: u16, kind: LocalEngineKind) -> LocalEngineSpec {
        LocalEngineSpec {
            name: name.into(),
            port,
            path: "/v1/models".into(),
            kind,
        }
    }

    #[test]
    fn matrix_tables_match_the_spec() {
        assert_eq!(CLI_RUNTIMES, ["claude", "codex", "grok", "agy", "muse"]);
        let secrets: Vec<_> = VAULT_PROVIDERS.iter().map(|(_, s)| *s).collect();
        assert_eq!(
            secrets,
            [
                "ANTHROPIC_API_KEY",
                "OPENAI_API_KEY",
                "XAI_API_KEY",
                "GEMINI_API_KEY",
                "OPENROUTER_API_KEY"
            ]
        );
        let engines = default_local_engines();
        assert_eq!(engines.len(), 3);
        assert_eq!(endpoint_of(&engines[0]), "127.0.0.1:11434/api/tags");
        assert_eq!(endpoint_of(&engines[1]), "127.0.0.1:8000/v1/models");
        assert_eq!(endpoint_of(&engines[2]), "127.0.0.1:8097/v1/models");
    }

    #[test]
    fn vault_status_ready_missing_expired() {
        let metas = vec![
            meta("ANTHROPIC_API_KEY", None),
            meta("OPENAI_API_KEY", Some("2020-01-01T00:00:00.000Z")),
            meta("XAI_API_KEY", Some("2999-01-01T00:00:00.000Z")),
        ];
        let out = vault_statuses(&VAULT_PROVIDERS, &metas, 1_800_000_000_000);
        let by = |p: &str| out.iter().find(|s| s.provider == p).unwrap();
        assert_eq!(by("anthropic").status, "ready");
        assert!(by("anthropic").provisioned && !by("anthropic").expired);
        assert_eq!(by("openai").status, "expired");
        assert!(by("openai").provisioned && by("openai").expired);
        assert_eq!(by("xai").status, "ready");
        assert_eq!(by("gemini").status, "missing");
        assert!(!by("openrouter").provisioned);
    }

    #[test]
    fn vault_status_never_serializes_secret_material() {
        let out = vault_statuses(&VAULT_PROVIDERS, &[meta("OPENAI_API_KEY", None)], 0);
        let json = serde_json::to_string(&out).unwrap();
        assert!(!json.contains("value"));
        assert!(!json.contains("nonce"));
        assert!(!json.contains("ciphertext"));
    }

    #[test]
    fn find_binary_checks_path_then_home_local_bin() {
        let path_dir = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        let bin = home.path().join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        #[cfg(unix)]
        {
            write_script(path_dir.path(), "claude", "exit 0");
            write_script(&bin, "codex", "exit 0");
            write_script(&bin, "claude", "exit 0");
            let p = path_dir.path().to_str().unwrap();
            assert_eq!(
                find_binary("claude", p, Some(home.path())).unwrap(),
                path_dir.path().join("claude")
            );
            assert_eq!(
                find_binary("codex", p, Some(home.path())).unwrap(),
                bin.join("codex")
            );
            assert!(find_binary("grok", p, Some(home.path())).is_none());
        }
    }

    #[cfg(unix)]
    #[test]
    fn find_binary_skips_non_executable_files() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("agy"), "x").unwrap();
        assert!(find_binary("agy", dir.path().to_str().unwrap(), None).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn version_probe_reads_first_line() {
        let dir = TempDir::new().unwrap();
        let p = write_script(
            dir.path(),
            "claude",
            "echo '1.2.3 (Claude Code)'; echo second",
        );
        assert_eq!(
            run_fixture_version(&p, Duration::from_secs(2)).unwrap(),
            "1.2.3 (Claude Code)"
        );
    }

    #[cfg(unix)]
    #[test]
    fn version_probe_falls_back_to_stderr_and_reports_failure() {
        let dir = TempDir::new().unwrap();
        let p = write_script(dir.path(), "a", "echo v9 >&2");
        assert_eq!(
            run_fixture_version(&p, Duration::from_secs(2)).unwrap(),
            "v9"
        );
        let bad = write_script(dir.path(), "b", "exit 3");
        assert!(
            run_fixture_version(&bad, Duration::from_secs(2))
                .unwrap_err()
                .contains("exited")
        );
        let empty = write_script(dir.path(), "c", "exit 0");
        assert!(
            run_fixture_version(&empty, Duration::from_secs(2))
                .unwrap_err()
                .contains("nothing")
        );
    }

    #[cfg(unix)]
    #[test]
    fn version_probe_times_out_and_kills_the_process_group() {
        let dir = TempDir::new().unwrap();
        let marker = dir.path().join("grandchild-alive");
        // The grandchild would create the marker after 1s if it survived the group kill.
        let body = format!("(sleep 1; touch '{}') &\nsleep 30", marker.display());
        let p = write_script(dir.path(), "slow", &body);
        let started = Instant::now();
        let err = run_fixture_version(&p, Duration::from_millis(200)).unwrap_err();
        assert!(err.contains("timed out"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(2));
        std::thread::sleep(Duration::from_millis(1400));
        assert!(!marker.exists(), "grandchild survived the group kill");
    }

    #[cfg(unix)]
    #[test]
    fn version_probe_kills_a_grandchild_left_behind_by_a_normal_exit() {
        let dir = TempDir::new().unwrap();
        let marker = dir.path().join("grandchild-alive");
        // The script exits at once, but its background child would touch the marker after 1s.
        let body = format!("(sleep 1; touch '{}') &\necho v1", marker.display());
        let p = write_script(dir.path(), "wrapper", &body);
        assert_eq!(
            run_fixture_version(&p, Duration::from_secs(2)).unwrap(),
            "v1"
        );
        std::thread::sleep(Duration::from_millis(1400));
        assert!(!marker.exists(), "grandchild survived the group kill");
    }

    #[cfg(unix)]
    #[test]
    fn noticing_an_exit_does_not_reap_the_leader() {
        let mut child = Command::new("true").spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !exited_unreaped(&mut child).unwrap() {
            assert!(Instant::now() < deadline, "child never exited");
            std::thread::sleep(Duration::from_millis(5));
        }
        // Still waitable: its pid is not free for reuse until `wait` below.
        assert!(child.try_wait().unwrap().is_some());
    }

    #[test]
    fn the_probe_gate_refuses_a_concurrent_probe_and_reuses_a_fresh_result() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let gate = ProbeGate::default();
        let calls = AtomicUsize::new(0);
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        std::thread::scope(|scope| {
            let (gate, calls) = (&gate, &calls);
            let first = scope.spawn(move || {
                gate.run(Some("cli"), move || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    started_tx.send(()).unwrap();
                    let _ = release_rx.recv_timeout(Duration::from_secs(5));
                    Ok(json!({ "n": 1 }))
                })
            });
            started_rx.recv().unwrap();
            // A second probe while the first runs spawns nothing.
            let second = gate.run(Some("cli"), || {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(json!({ "n": 2 }))
            });
            assert_eq!(second.status, 429);
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            release_tx.send(()).unwrap();
            assert_eq!(first.join().unwrap().status, 200);
        });
        // A fresh result for the same target (however spelled) is served again.
        let again = gate.run(Some(" CLI "), || {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(json!({ "n": 3 }))
        });
        assert_eq!(again.status, 200);
        assert_eq!(again.json_value().unwrap(), json!({ "n": 1 }));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // Another target probes. An error is returned as it is and is not remembered.
        let other = gate.run(Some("vault"), || {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(HttpResponse::json(400, json!({ "error": "no" })))
        });
        assert_eq!(other.status, 400);
        let retry = gate.run(Some("vault"), || {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(json!({ "n": 4 }))
        });
        assert_eq!(retry.json_value().unwrap(), json!({ "n": 4 }));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn reading_vault_metadata_creates_neither_a_key_nor_a_vault_file() {
        let home = TempDir::new().unwrap();
        assert!(vault_metadata(Some(home.path())).unwrap().is_empty());
        assert!(!fl_gate::resolve_master_key_path(Some(home.path())).exists());
        assert!(!resolve_vault_path(Some(home.path())).exists());
    }

    #[cfg(unix)]
    #[test]
    fn cli_probe_counts_routes_and_sessions_per_adapter() {
        let dir = TempDir::new().unwrap();
        write_script(dir.path(), "claude", "echo 2.0.0");
        let mut inp = inputs(dir.path().to_str().unwrap(), None);
        inp.sessions = vec![
            session("claude", "route", "active"),
            session("claude", "attempt", "running"),
            session("claude", "attempt", "succeeded"),
            session("codex", "route", "active"),
        ];
        let claude = probe_cli("claude", &inp, Duration::from_secs(2));
        assert!(claude.installed);
        assert_eq!(claude.version.as_deref(), Some("2.0.0"));
        assert_eq!(claude.active_routes, 1);
        assert_eq!(claude.live_sessions, 2);
        let codex = probe_cli("codex", &inp, Duration::from_secs(2));
        assert!(!codex.installed);
        assert!(codex.error.as_deref().unwrap().contains("not found"));
        assert_eq!((codex.active_routes, codex.live_sessions), (1, 1));
    }

    #[test]
    fn local_probe_reads_ollama_and_openai_model_lists() {
        let port = serve_once(
            "HTTP/1.0 200 OK\r\nContent-Type: application/json\r\n\r\n{\"models\":[{\"name\":\"llama3:8b\"},{\"name\":\"qwen3\"}]}",
        );
        let st = probe_local_engine(
            &spec("ollama", port, LocalEngineKind::Ollama),
            LOCAL_TIMEOUT,
        );
        assert!(st.reachable, "{st:?}");
        assert!(st.latency_ms.is_some());
        assert_eq!(st.models, ["llama3:8b", "qwen3"]);

        let port = serve_once("HTTP/1.1 200 OK\r\n\r\n{\"data\":[{\"id\":\"Qwen3-8B\"}]}");
        let st = probe_local_engine(
            &spec("lemonade-8000", port, LocalEngineKind::OpenAiModels),
            LOCAL_TIMEOUT,
        );
        assert_eq!(st.models, ["Qwen3-8B"]);
    }

    #[test]
    fn local_probe_reports_http_errors_and_garbage_bodies() {
        let port = serve_once("HTTP/1.0 503 Service Unavailable\r\n\r\nno");
        let st = probe_local_engine(
            &spec("x", port, LocalEngineKind::OpenAiModels),
            LOCAL_TIMEOUT,
        );
        assert!(!st.reachable);
        assert_eq!(st.error.as_deref(), Some("HTTP 503"));

        let port = serve_once("HTTP/1.0 200 OK\r\n\r\nnot json");
        let st = probe_local_engine(
            &spec("x", port, LocalEngineKind::OpenAiModels),
            LOCAL_TIMEOUT,
        );
        assert!(st.reachable);
        assert!(st.models.is_empty());
    }

    #[test]
    fn local_probe_refused_port_is_unreachable() {
        let port = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let st = probe_local_engine(&spec("gone", port, LocalEngineKind::Ollama), LOCAL_TIMEOUT);
        assert!(!st.reachable);
        assert_eq!(st.latency_ms, None);
        assert!(st.error.is_some());
    }

    #[test]
    fn local_probe_times_out_on_a_silent_server() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let hold = std::thread::spawn(move || {
            let conn = listener.accept();
            std::thread::sleep(Duration::from_millis(900));
            drop(conn);
        });
        let started = Instant::now();
        let st = probe_local_engine(&spec("slow", port, LocalEngineKind::Ollama), LOCAL_TIMEOUT);
        assert!(!st.reachable);
        assert!(st.error.as_deref().unwrap().contains("timed out"), "{st:?}");
        assert!(started.elapsed() < Duration::from_millis(800));
        hold.join().unwrap();
    }

    #[test]
    fn model_ids_are_capped() {
        let items: Vec<String> = (0..300).map(|i| format!("{{\"id\":\"m{i}\"}}")).collect();
        let body = format!("{{\"data\":[{}]}}", items.join(","));
        assert_eq!(
            parse_model_ids(LocalEngineKind::OpenAiModels, &body).len(),
            MAX_LOCAL_MODELS
        );
    }

    #[test]
    fn target_filter_selects_tiers_and_items() {
        let engines = default_local_engines();
        let sel = select(Some("cli"), &engines).unwrap();
        assert_eq!((sel.cli.len(), sel.vault.len(), sel.local.len()), (5, 0, 0));
        let sel = select(Some(" Codex "), &engines).unwrap();
        assert_eq!(sel.cli, ["codex"]);
        assert!(sel.vault.is_empty() && sel.local.is_empty());
        let sel = select(Some("openai"), &engines).unwrap();
        assert_eq!(sel.vault, [("openai", "OPENAI_API_KEY")]);
        let sel = select(Some("lemonade-8097"), &engines).unwrap();
        assert_eq!(sel.local.len(), 1);
        let sel = select(Some(""), &engines).unwrap();
        assert_eq!((sel.cli.len(), sel.vault.len(), sel.local.len()), (5, 5, 3));
        assert!(select(Some("nope"), &engines).is_err());
        assert!(select(Some(&"x".repeat(200)), &engines).is_err());
    }

    #[test]
    fn build_matrix_filters_and_survives_vault_failure() {
        let mut inp = inputs("", None);
        inp.vault = Err("vault unreadable".into());
        let m = build_matrix(&inp, Some("vault")).unwrap();
        assert!(m.cli_runtimes.is_empty() && m.local_engines.is_empty());
        assert_eq!(m.vault_providers.len(), 5);
        assert!(m.vault_providers.iter().all(|p| !p.provisioned));
        assert_eq!(m.vault_error.as_deref(), Some("vault unreadable"));

        let port = serve_once("HTTP/1.0 200 OK\r\n\r\n{\"data\":[{\"id\":\"m\"}]}");
        inp.local_engines = vec![spec("lemonade-8000", port, LocalEngineKind::OpenAiModels)];
        let m = build_matrix(&inp, Some("lemonade-8000")).unwrap();
        assert_eq!(m.local_engines.len(), 1);
        assert!(m.local_engines[0].reachable);
        assert!(build_matrix(&inp, Some("bogus")).is_err());

        let full = build_matrix(&inputs("", None), None).unwrap();
        assert_eq!(full.cli_runtimes.len(), 5);
        assert_eq!(full.vault_providers.len(), 5);
    }
}
