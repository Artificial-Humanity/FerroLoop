//! Sender persona resolution, live vendor route table, and passive route discovery (spec §3.1).

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use fl_core::agent::{AgentRole, AgentStatus};
use fl_core::at::At;
use fl_core::state::validate_slug;
use fl_core::store::AgentRegistry;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VendorRoute {
    Claude {
        pid: u32,
        session_id: String,
        socket_path: PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key_path: Option<PathBuf>,
    },
    Agy {
        conversation_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ls_pid: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ls_address: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        csrf_token: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agentapi_exe: Option<PathBuf>,
    },
    Codex {
        thread_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<PathBuf>,
    },
    Muse {
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<PathBuf>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tmux_target: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tmux_pane: Option<String>,
    },
    Grok {
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<PathBuf>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tmux_target: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tmux_pane: Option<String>,
    },
    /// Used by deterministic integration tests to capture pushed deliveries without vendor binaries.
    Webhook { socket_path: PathBuf },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteEntry {
    pub persona: String,
    pub route: VendorRoute,
    pub updated_at: At,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RouteTable {
    pub entries: BTreeMap<String, RouteEntry>,
}

pub fn default_routes_path() -> PathBuf {
    if let Some(rt) = std::env::var_os("XDG_RUNTIME_DIR") {
        let p = PathBuf::from(rt);
        if p.is_absolute() {
            return p.join("fl-wire-routes.json");
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let p = PathBuf::from(home);
        if p.is_absolute() {
            return p.join(".cache").join("fl").join("wire-routes.json");
        }
    }
    PathBuf::from("/tmp/fl-wire-routes.json")
}

pub fn legacy_routes_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let p = PathBuf::from(home);
    if p.is_absolute() {
        Some(p.join(".cache").join("agent-bus").join("routes.json"))
    } else {
        None
    }
}

fn preserve_muse_tmux_fields(target_route: &mut VendorRoute, fallback_route: &VendorRoute) {
    match (target_route, fallback_route) {
        (
            VendorRoute::Muse {
                tmux_target: dst_target,
                tmux_pane: dst_pane,
                ..
            },
            VendorRoute::Muse {
                tmux_target: src_target,
                tmux_pane: src_pane,
                ..
            },
        )
        | (
            VendorRoute::Grok {
                tmux_target: dst_target,
                tmux_pane: dst_pane,
                ..
            },
            VendorRoute::Grok {
                tmux_target: src_target,
                tmux_pane: src_pane,
                ..
            },
        ) => {
            if dst_target.is_none() {
                *dst_target = src_target.clone();
            }
            if dst_pane.is_none() {
                *dst_pane = src_pane.clone();
            }
        }
        _ => {}
    }
}

/// An Antigravity binary is usable when it is a real file and not an
/// `agy.<timestamp>.old` leftover from an in-place auto-update.
pub fn agy_exe_is_usable(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    !name.ends_with(".old") && path.is_file()
}

/// Pick an Antigravity `agentapi` binary.
///
/// `configured` (the route) wins when it is usable. Then `env_exe`
/// (`ANTIGRAVITY_AGENTAPI_EXE`) when that path is usable. Then
/// `$HOME/.local/bin/agy` when that file exists. Otherwise the bare `agy`
/// command name, which the caller resolves through `PATH`.
pub fn resolve_agy_exe(
    configured: Option<&Path>,
    env_exe: Option<&Path>,
    home: Option<&Path>,
) -> PathBuf {
    if let Some(path) = configured.filter(|path| agy_exe_is_usable(path)) {
        return path.to_path_buf();
    }
    if let Some(path) = env_exe.filter(|path| agy_exe_is_usable(path)) {
        return path.to_path_buf();
    }
    if let Some(home) = home {
        let local = home.join(".local").join("bin").join("agy");
        if local.is_file() {
            return local;
        }
    }
    PathBuf::from("agy")
}

/// Port of a language-server address (`host:port`, `[v6]:port`, or a bare port).
///
/// Every dial goes to `127.0.0.1`, so only an address that names this machine is usable:
/// an empty host, `127.0.0.1`, `localhost` or `[::1]`. Any other host is `None`, and the
/// route's CSRF token never goes to whatever listens on that port locally.
pub fn agy_ls_port(addr: &str) -> Option<u16> {
    let addr = addr.trim();
    if addr.is_empty() {
        return None;
    }
    let (host, port_src) = if let Some(rest) = addr.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        (format!("[{host}]"), after.strip_prefix(':')?)
    } else if let Some((host, port)) = addr.rsplit_once(':') {
        (host.to_string(), port)
    } else {
        (String::new(), addr)
    };
    let host = host.trim().to_ascii_lowercase();
    if !matches!(host.as_str(), "" | "127.0.0.1" | "localhost" | "[::1]") {
        return None;
    }
    let digits: String = port_src
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

/// True when `addr`'s TCP port accepts a connection on `127.0.0.1` within 50ms.
pub fn agy_ls_address_listening(addr: &str) -> bool {
    let Some(port) = agy_ls_port(addr) else {
        return false;
    };
    let target = SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port));
    TcpStream::connect_timeout(&target, Duration::from_millis(50)).is_ok()
}

/// Read one `/proc/<pid>/environ` blob. A language-server address is kept only
/// when its port is listening, and an `agentapi` path is kept only when the
/// file is usable. Dead `bwrap` ports and unlinked `.old` binaries are skipped
/// so a later process can still supply the live values.
fn note_agy_proc_env(
    env_bytes: &[u8],
    shared_csrf: &mut Option<String>,
    shared_ls_addr: &mut Option<String>,
    shared_exe: &mut Option<PathBuf>,
) {
    for kv in env_bytes.split(|&b| b == 0) {
        let Ok(kv_str) = std::str::from_utf8(kv) else {
            continue;
        };
        if shared_csrf.is_none()
            && let Some(val) = kv_str.strip_prefix("ANTIGRAVITY_CSRF_TOKEN=")
            && !val.is_empty()
        {
            *shared_csrf = Some(val.to_string());
        } else if shared_ls_addr.is_none()
            && let Some(val) = kv_str.strip_prefix("ANTIGRAVITY_LS_ADDRESS=")
            && !val.is_empty()
            && agy_ls_address_listening(val)
        {
            *shared_ls_addr = Some(val.to_string());
        } else if shared_exe.is_none()
            && let Some(val) = kv_str.strip_prefix("ANTIGRAVITY_AGENTAPI_EXE=")
            && agy_exe_is_usable(Path::new(val))
        {
            *shared_exe = Some(PathBuf::from(val));
        }
    }
}

/// Test code must never touch the machine's real route table: `RouteTable::load(None)` reads
/// the default path and, unless host discovery is disabled, `~/.cache/agent-bus/routes.json`
/// (what live agent sessions registered), and `RouteTable::save(None)` overwrites the default
/// path. A test that reaches either would pass or fail depending on which agents run on the
/// machine, and could dispatch to a real session. When fl-wire's own unit tests are built
/// (`cfg(test)` on this crate) a `None` path therefore panics, so those tests must hand the
/// table a path in their own temp directory. The guard does not cover other crates' tests:
/// fl-hub and fl-cli tests compile fl-wire without `cfg(test)`, so there a `None` path still
/// reaches the real route table.
#[cfg(test)]
fn refuse_default_route_path(custom_path: Option<&Path>, operation: &str) {
    assert!(
        custom_path.is_some(),
        "fl-wire test code called RouteTable::{operation}(None): pass an explicit route table path \
         in a temp directory, never the machine's real route table"
    );
}

impl RouteTable {
    pub fn load(custom_path: Option<&Path>) -> Self {
        #[cfg(test)]
        refuse_default_route_path(custom_path, "load");
        let primary = custom_path
            .map(Path::to_path_buf)
            .unwrap_or_else(default_routes_path);
        let mut table = Self::load_file(&primary).unwrap_or_default();
        if custom_path.is_none()
            && std::env::var("FL_WIRE_DISABLE_HOST_DISCOVERY").as_deref() != Ok("1")
            && let Some(legacy) = legacy_routes_path()
            && let Some(legacy_table) = Self::load_file(&legacy)
        {
            table.merge_newer(legacy_table);
        }
        table
    }

    pub fn merge_newer(&mut self, other: Self) {
        for (k, mut v) in other.entries {
            match self.entries.get_mut(&k) {
                Some(existing)
                    if existing.updated_at.unix_millis() >= v.updated_at.unix_millis() =>
                {
                    preserve_muse_tmux_fields(&mut existing.route, &v.route);
                }
                Some(existing) => {
                    preserve_muse_tmux_fields(&mut v.route, &existing.route);
                    self.entries.insert(k, v);
                }
                None => {
                    self.entries.insert(k, v);
                }
            }
        }
    }

    /// Fold the result of a dispatch back into the live table. `working` started as a clone of
    /// `base` and went through `discover_routes`, which both adds routes and prunes dead ones.
    /// A route that `base` held but `working` dropped is removed here too, unless the live table
    /// changed it meanwhile (a concurrent `RegisterRoute`), in which case the live entry wins.
    /// Everything else merges as in [`RouteTable::merge_newer`].
    pub fn merge_dispatch_result(&mut self, base: &Self, working: Self) {
        for (key, base_entry) in &base.entries {
            if !working.entries.contains_key(key) && self.entries.get(key) == Some(base_entry) {
                self.entries.remove(key);
            }
        }
        self.merge_newer(working);
    }

    fn load_file(path: &Path) -> Option<Self> {
        let raw = fs::read_to_string(path).ok()?;
        if let Ok(table) = serde_json::from_str::<RouteTable>(&raw) {
            return Some(table);
        }
        // Also support flat map `{ "<persona>": RouteEntry }` or legacy agent-bus route map
        if let Ok(entries) = serde_json::from_str::<BTreeMap<String, RouteEntry>>(&raw) {
            return Some(RouteTable { entries });
        }
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&raw)
            && let Some(obj) = val.as_object()
        {
            let mut entries = BTreeMap::new();
            for (persona, item) in obj {
                let Some(norm) = normalize_persona(persona) else {
                    continue;
                };
                if let Some(route) = parse_legacy_route_item(item) {
                    let updated_at = item
                        .get("updated_at")
                        .and_then(|v| v.as_str())
                        .and_then(parse_flexible_at)
                        .unwrap_or_else(|| At::from_unix_millis(0));
                    entries.insert(
                        norm.clone(),
                        RouteEntry {
                            persona: norm,
                            route,
                            updated_at,
                        },
                    );
                }
            }
            if !entries.is_empty() {
                return Some(RouteTable { entries });
            }
        }
        None
    }

    pub fn save(&self, custom_path: Option<&Path>) -> std::io::Result<()> {
        #[cfg(test)]
        refuse_default_route_path(custom_path, "save");
        let target = custom_path
            .map(Path::to_path_buf)
            .unwrap_or_else(default_routes_path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        // A name of our own per call: two savers (the daemon and the hub, or two threads) must
        // not truncate and rename each other's half-written file.
        static SAVE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SAVE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = target.with_extension(format!("json.tmp.{}.{seq}", std::process::id()));
        let bytes = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        let written = (|| -> std::io::Result<()> {
            let mut opts = fs::OpenOptions::new();
            opts.write(true).create_new(true);
            // The table holds CSRF tokens: the file is created private, not chmod-ed afterwards.
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let mut f = opts.open(&tmp)?;
            f.write_all(&bytes)?;
            f.sync_all()
        })();
        if let Err(err) = written {
            let _ = fs::remove_file(&tmp);
            return Err(err);
        }
        if let Err(err) = fs::rename(&tmp, &target) {
            let _ = fs::remove_file(&tmp);
            return Err(err);
        }
        Ok(())
    }

    pub fn upsert(&mut self, persona: &str, mut route: VendorRoute, at: At) {
        if let Some(norm) = normalize_persona(persona) {
            if let Some(existing) = self.entries.get(&norm) {
                preserve_muse_tmux_fields(&mut route, &existing.route);
            }
            self.entries.insert(
                norm.clone(),
                RouteEntry {
                    persona: norm,
                    route,
                    updated_at: at,
                },
            );
        }
    }

    /// Record a file-discovered route at its own mtime.
    ///
    /// A newer route already in the table (for example a live Antigravity
    /// session) is left in place. `updated_ms == 0` falls back to `now`.
    fn upsert_discovered(&mut self, persona: &str, route: VendorRoute, updated_ms: u64, now: &At) {
        let file_at = if updated_ms > 0 {
            At::from_unix_millis(updated_ms)
        } else {
            now.clone()
        };
        if let Some(norm) = normalize_persona(persona)
            && self
                .entries
                .get(&norm)
                .is_some_and(|existing| existing.updated_at.unix_millis() > file_at.unix_millis())
        {
            return;
        }
        self.upsert(persona, route, file_at);
    }

    pub fn get(&self, persona: &str) -> Option<&RouteEntry> {
        let norm = normalize_persona(persona)?;
        self.entries.get(&norm)
    }

    /// Passively discover Claude, Antigravity, Codex, Meta Muse, and xAI Grok routes from vendor session files.
    pub fn discover_routes(&mut self, home_override: Option<&Path>, at: At) {
        if home_override.is_none()
            && std::env::var("FL_WIRE_DISABLE_HOST_DISCOVERY").as_deref() == Ok("1")
        {
            return;
        }
        let home = home_override
            .map(Path::to_path_buf)
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from));
        let Some(home) = home else {
            return;
        };
        let check_proc = home_override.is_none()
            && std::env::var("FL_WIRE_DISABLE_HOST_DISCOVERY").as_deref() != Ok("1");
        self.discover_claude_routes(&home, check_proc, &at);
        self.discover_agy_routes(&home, check_proc, &at);
        self.discover_codex_routes(&home, &at);
        self.discover_muse_routes(&home, home_override.is_some(), &at);
        self.discover_grok_routes(&home, check_proc, &at);
    }

    fn discover_claude_routes(&mut self, home: &Path, check_proc: bool, at: &At) {
        if check_proc {
            self.entries.retain(|_, entry| match &entry.route {
                VendorRoute::Claude { pid, .. } if *pid > 0 => {
                    Path::new(&format!("/proc/{pid}")).exists()
                }
                _ => true,
            });
        }
        let sessions_dir = home.join(".claude").join("sessions");
        let Ok(read_dir) = fs::read_dir(&sessions_dir) else {
            return;
        };
        for entry in read_dir.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let Ok(pid) = stem.parse::<u32>() else {
                continue;
            };
            if check_proc && !Path::new(&format!("/proc/{pid}")).exists() {
                continue;
            }
            let Ok(raw) = fs::read_to_string(&path) else {
                continue;
            };
            let Ok(val) = serde_json::from_str::<serde_json::Value>(&raw) else {
                continue;
            };
            if val.get("nameSource").and_then(|v| v.as_str()) == Some("derived")
                && let Some(derived_key) = val
                    .get("name")
                    .and_then(|v| v.as_str())
                    .and_then(normalize_persona)
                && matches!(
                    self.entries.get(&derived_key).map(|e| &e.route),
                    Some(VendorRoute::Claude { .. })
                )
            {
                self.entries.remove(&derived_key);
            }
            let Some(persona) = resolve_claude_session_persona(home, &val, Some(&self.entries))
            else {
                continue;
            };
            let Some(sock_str) = val.get("messagingSocketPath").and_then(|v| v.as_str()) else {
                continue;
            };
            let socket_path = PathBuf::from(sock_str);
            let session_id = val
                .get("sessionId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let key_path = resolve_claude_key_path(&sessions_dir, pid, &socket_path);
            self.upsert(
                &persona,
                VendorRoute::Claude {
                    pid,
                    session_id,
                    socket_path,
                    key_path,
                },
                at.clone(),
            );
        }
    }

    fn discover_agy_routes(&mut self, home: &Path, check_proc: bool, at: &At) {
        let agy_dir = home.join(".gemini").join("antigravity-cli");
        let brain_dir = agy_dir.join("brain");
        if !brain_dir.is_dir() {
            // No transcript to scan. Still replace a missing or `.old` exe with
            // `$HOME/.local/bin/agy` when that file exists, otherwise `agy`.
            // cli.log and /proc stay unused on this path.
            let fallback = resolve_agy_exe(None, None, Some(home));
            for entry in self.entries.values_mut() {
                if let VendorRoute::Agy { agentapi_exe, .. } = &mut entry.route
                    && agentapi_exe
                        .as_ref()
                        .is_none_or(|path| !agy_exe_is_usable(path))
                {
                    *agentapi_exe = Some(fallback.clone());
                }
            }
            return;
        }
        let mut shared_ls_addr: Option<String> = None;
        let mut shared_csrf: Option<String> = None;
        let mut shared_exe: Option<PathBuf> = None;

        // 1. Prefer live language-server HTTP port from cli.log so host reboots immediately
        // override stale pre-reboot ports persisted in routes.json.
        if let Ok(log_text) = fs::read_to_string(agy_dir.join("cli.log")) {
            for line in log_text.lines().rev() {
                if line.contains("for HTTPS") {
                    continue;
                }
                if let Some(idx) = line.find("Language server listening on random port at ") {
                    let rest = &line[idx + "Language server listening on random port at ".len()..];
                    let port: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
                    if !port.is_empty() {
                        let candidate = format!("127.0.0.1:{port}");
                        // Offline temp-home discovery does not probe listeners.
                        // Host discovery keeps scanning until a port accepts.
                        if !check_proc || agy_ls_address_listening(&candidate) {
                            shared_ls_addr = Some(candidate);
                            break;
                        }
                    }
                }
            }
        }

        // 2. Inspect live /proc/*/environ when host discovery is enabled.
        if check_proc
            && (shared_csrf.is_none() || shared_ls_addr.is_none() || shared_exe.is_none())
            && let Ok(proc_dir) = fs::read_dir("/proc")
        {
            for entry in proc_dir.flatten() {
                let name = entry.file_name();
                let Some(s) = name.to_str() else {
                    continue;
                };
                if !s.chars().all(|c| c.is_ascii_digit()) {
                    continue;
                }
                let Ok(env_bytes) = fs::read(entry.path().join("environ")) else {
                    continue;
                };
                note_agy_proc_env(
                    &env_bytes,
                    &mut shared_csrf,
                    &mut shared_ls_addr,
                    &mut shared_exe,
                );
                if shared_csrf.is_some() && shared_ls_addr.is_some() && shared_exe.is_some() {
                    break;
                }
            }
        }

        // 3. Fall back to existing Agy entries ordered from newest updated_at to oldest.
        let mut agy_entries: Vec<&RouteEntry> = self
            .entries
            .values()
            .filter(|e| matches!(e.route, VendorRoute::Agy { .. }))
            .collect();
        agy_entries.sort_by_key(|e| std::cmp::Reverse(e.updated_at.unix_millis()));
        for entry in agy_entries {
            if let VendorRoute::Agy {
                ls_address,
                csrf_token,
                agentapi_exe,
                ..
            } = &entry.route
            {
                if shared_ls_addr.is_none()
                    && let Some(addr) = ls_address
                        .as_ref()
                        .filter(|addr| !check_proc || agy_ls_address_listening(addr))
                {
                    shared_ls_addr = Some(addr.clone());
                }
                if shared_csrf.is_none() {
                    shared_csrf = csrf_token.clone();
                }
                if shared_exe.is_none()
                    && let Some(exe) = agentapi_exe.as_ref().filter(|path| agy_exe_is_usable(path))
                {
                    shared_exe = Some(exe.clone());
                }
            }
        }

        // 4. Propagate the live/newest shared ls_address and csrf_token across all Agy routes
        // so pre-reboot entries do not retain stale language-server ports or CSRF tokens.
        // Replace a missing or `.old` agentapi binary. A usable exe already on the route stays.
        // Host `ANTIGRAVITY_AGENTAPI_EXE` is consulted only for live `/proc` discovery so a
        // temp-home unit test does not pick up the machine's binary.
        let env_exe = if check_proc {
            std::env::var_os("ANTIGRAVITY_AGENTAPI_EXE").map(PathBuf::from)
        } else {
            None
        };
        let resolved_shared =
            resolve_agy_exe(shared_exe.as_deref(), env_exe.as_deref(), Some(home));
        for entry in self.entries.values_mut() {
            if let VendorRoute::Agy {
                ls_address,
                csrf_token,
                agentapi_exe,
                ..
            } = &mut entry.route
            {
                if shared_ls_addr.is_some() {
                    *ls_address = shared_ls_addr.clone();
                } else if check_proc
                    && ls_address
                        .as_deref()
                        .is_some_and(|addr| !agy_ls_address_listening(addr))
                {
                    *ls_address = None;
                }
                if shared_csrf.is_some() {
                    *csrf_token = shared_csrf.clone();
                }
                if agentapi_exe
                    .as_ref()
                    .is_none_or(|path| !agy_exe_is_usable(path))
                {
                    *agentapi_exe = Some(resolved_shared.clone());
                }
            }
        }

        // Scan brain/*/transcript.jsonl for persona registrations
        let Ok(read_dir) = fs::read_dir(&brain_dir) else {
            return;
        };
        for entry in read_dir.flatten() {
            let conv_dir = entry.path();
            let Some(conv_id) = conv_dir.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            let transcript = conv_dir
                .join(".system_generated")
                .join("logs")
                .join("transcript.jsonl");
            if let Some(persona) = scan_transcript_for_persona(&transcript, 50)
                && !self.entries.contains_key(&persona)
                && !self.entries.values().any(|existing| {
                    matches!(
                        &existing.route,
                        VendorRoute::Agy {
                            conversation_id: existing_id,
                            ..
                        } if existing_id == conv_id
                    )
                })
            {
                self.upsert(
                    &persona,
                    VendorRoute::Agy {
                        conversation_id: conv_id.to_string(),
                        ls_pid: None,
                        ls_address: shared_ls_addr.clone(),
                        csrf_token: shared_csrf.clone(),
                        agentapi_exe: Some(resolved_shared.clone()),
                    },
                    at.clone(),
                );
            }
        }
    }

    fn discover_codex_routes(&mut self, home: &Path, at: &At) {
        let codex_dir = home.join(".codex");
        let mut rollout_files = Vec::new();
        collect_codex_rollout_files(&codex_dir.join("sessions"), 0, &mut rollout_files);
        // Index `updated_at` and the index file mtime move when the index is rewritten.
        // The rollout file mtime is the session age the later pass also uses.
        let mut rollout_mtime_by_thread: BTreeMap<String, u64> = BTreeMap::new();
        for path in &rollout_files {
            let (thread_id, _) = extract_codex_rollout_meta(path);
            if thread_id.is_empty() {
                continue;
            }
            let file_ms = mtime_ms(path);
            if file_ms == 0 {
                continue;
            }
            rollout_mtime_by_thread
                .entry(thread_id)
                .and_modify(|prev| {
                    if file_ms > *prev {
                        *prev = file_ms;
                    }
                })
                .or_insert(file_ms);
        }

        let index_path = codex_dir.join("session_index.jsonl");
        if let Ok(text) = fs::read_to_string(&index_path) {
            let index_ms = mtime_ms(&index_path);
            for line in text.lines() {
                let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
                    continue;
                };
                let Some(thread_id) = val
                    .get("id")
                    .or_else(|| val.get("thread_id"))
                    .and_then(|v| v.as_str())
                else {
                    continue;
                };
                let persona_raw = val
                    .get("agentName")
                    .or_else(|| val.get("persona"))
                    .or_else(|| val.get("thread_name"))
                    .or_else(|| val.get("title"))
                    .and_then(|v| v.as_str());
                let Some(persona) = persona_raw.and_then(normalize_persona) else {
                    continue;
                };
                if matches!(
                    persona.as_str(),
                    "exit"
                        | "quit"
                        | "help"
                        | "clear"
                        | "status"
                        | "compact"
                        | "unknown"
                        | "default"
                ) {
                    continue;
                }
                let cwd = val.get("cwd").and_then(|v| v.as_str()).map(PathBuf::from);
                let line_updated_ms = val
                    .get("updated_at")
                    .and_then(|v| v.as_str())
                    .and_then(parse_flexible_at)
                    .map(|stamp| stamp.unix_millis())
                    .filter(|ms| *ms > 0);
                // Prefer the matching rollout mtime. Fall back to the line's `updated_at`,
                // then the index mtime. Zero means `upsert_discovered` may use `at`.
                let updated_ms = rollout_mtime_by_thread
                    .get(thread_id)
                    .copied()
                    .filter(|ms| *ms > 0)
                    .or(line_updated_ms)
                    .unwrap_or(index_ms);
                self.upsert_discovered(
                    &persona,
                    VendorRoute::Codex {
                        thread_id: thread_id.to_string(),
                        cwd,
                    },
                    updated_ms,
                    at,
                );
            }
        }
        let mut candidates: Vec<(u64, String, String, Option<PathBuf>)> = Vec::new();
        for path in rollout_files {
            let Some(persona) = scan_transcript_for_persona(&path, 120) else {
                continue;
            };
            let (thread_id, cwd) = extract_codex_rollout_meta(&path);
            if thread_id.is_empty() {
                continue;
            }
            let updated_ms = fs::metadata(&path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            candidates.push((updated_ms, persona, thread_id, cwd));
        }
        candidates.sort_by_key(|c| c.0);
        for (updated_ms, persona, thread_id, cwd) in candidates {
            self.upsert_discovered(
                &persona,
                VendorRoute::Codex { thread_id, cwd },
                updated_ms,
                at,
            );
        }
    }

    fn discover_muse_routes(&mut self, home: &Path, has_home_override: bool, at: &At) {
        let mut dirs = vec![
            home.join(".local")
                .join("share")
                .join("muse")
                .join("sessions"),
        ];
        if !has_home_override && let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
            let xdg_dir = PathBuf::from(xdg).join("muse").join("sessions");
            if !dirs.contains(&xdg_dir) {
                dirs.push(xdg_dir);
            }
        }
        for sessions_dir in dirs {
            let Ok(read_dir) = fs::read_dir(&sessions_dir) else {
                continue;
            };
            for entry in read_dir.flatten() {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                    continue;
                }
                let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                let Some(persona) = scan_transcript_for_persona(&path, 50) else {
                    continue;
                };
                let (session_id, cwd) = extract_session_id_and_cwd(&path, stem);
                let (existing_target, existing_pane) = match self.entries.get(&persona) {
                    Some(RouteEntry {
                        route:
                            VendorRoute::Muse {
                                tmux_target,
                                tmux_pane,
                                ..
                            },
                        ..
                    }) => (tmux_target.clone(), tmux_pane.clone()),
                    _ => (None, None),
                };
                self.upsert_discovered(
                    &persona,
                    VendorRoute::Muse {
                        session_id,
                        cwd,
                        tmux_target: existing_target,
                        tmux_pane: existing_pane,
                    },
                    mtime_ms(&path),
                    at,
                );
            }
        }
        if !has_home_override
            && std::env::var("FL_WIRE_DISABLE_HOST_DISCOVERY").as_deref() != Ok("1")
        {
            for (persona, entry) in &mut self.entries {
                if let VendorRoute::Muse { tmux_target, .. } = &mut entry.route
                    && tmux_target.is_none()
                {
                    *tmux_target = Some(format!("muse:{persona}"));
                }
            }
        }
    }

    fn discover_grok_routes(&mut self, home: &Path, check_proc: bool, at: &At) {
        let grok_dir = home.join(".grok");
        let sessions_dir = grok_dir.join("sessions");

        let mut active_sessions: BTreeMap<String, GrokActiveSession> = BTreeMap::new();
        if let Ok(raw_active) = fs::read_to_string(grok_dir.join("active_sessions.json"))
            && let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(&raw_active)
        {
            for item in arr {
                let Some(sid) = item
                    .get("session_id")
                    .or_else(|| item.get("sessionId"))
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                else {
                    continue;
                };
                let pid = item.get("pid").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                if check_proc && pid > 0 && !Path::new(&format!("/proc/{pid}")).exists() {
                    continue;
                }
                let cwd = item
                    .get("cwd")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(PathBuf::from);
                let (env_persona, tmux_pane) = if check_proc && pid > 0 {
                    read_grok_proc_env(pid)
                } else {
                    (None, None)
                };
                active_sessions.insert(
                    sid.to_string(),
                    GrokActiveSession {
                        cwd,
                        env_persona,
                        tmux_pane,
                    },
                );
            }
        }

        let mut nested_dirs: Vec<PathBuf> = Vec::new();
        if let Ok(read_dir) = fs::read_dir(&sessions_dir) {
            for entry in read_dir.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    if path.join("chat_history.jsonl").is_file()
                        || path.join("summary.json").is_file()
                    {
                        nested_dirs.push(path);
                    } else if let Ok(sub_dir) = fs::read_dir(&path) {
                        for sub_entry in sub_dir.flatten() {
                            let sub_path = sub_entry.path();
                            if sub_path.is_dir()
                                && (sub_path.join("chat_history.jsonl").is_file()
                                    || sub_path.join("summary.json").is_file())
                            {
                                nested_dirs.push(sub_path);
                            }
                        }
                    }
                    continue;
                }
                let ext = path.extension().and_then(|s| s.to_str());
                if !matches!(ext, Some("json") | Some("jsonl")) {
                    continue;
                }
                let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                if transcript_is_grok_build_subagent(&path) {
                    continue;
                }
                let Some(persona) = scan_transcript_for_persona(&path, 50) else {
                    continue;
                };
                let (session_id, cwd) = extract_session_id_and_cwd(&path, stem);
                let (existing_target, existing_pane) = match self.entries.get(&persona) {
                    Some(RouteEntry {
                        route:
                            VendorRoute::Grok {
                                tmux_target,
                                tmux_pane,
                                ..
                            },
                        ..
                    }) => (tmux_target.clone(), tmux_pane.clone()),
                    _ => (None, None),
                };
                self.upsert_discovered(
                    &persona,
                    VendorRoute::Grok {
                        session_id,
                        cwd,
                        tmux_target: existing_target,
                        tmux_pane: existing_pane,
                    },
                    mtime_ms(&path),
                    at,
                );
            }
        }

        let mut candidates: Vec<GrokCandidate> = Vec::new();

        for dir in nested_dirs {
            let Some(dir_sid) = dir.file_name().and_then(|s| s.to_str()).map(str::trim) else {
                continue;
            };
            if dir_sid.is_empty() {
                continue;
            }
            let chat_path = dir.join("chat_history.jsonl");
            let summary_path = dir.join("summary.json");
            if chat_path.is_file() && transcript_is_grok_build_subagent(&chat_path) {
                continue;
            }
            let (summary_sid, summary_cwd, summary_updated_ms, summary_persona) =
                read_grok_summary_metadata(&summary_path);
            let session_id = summary_sid.unwrap_or_else(|| dir_sid.to_string());
            let active_meta = active_sessions.get(&session_id);
            let is_active = active_meta.is_some();
            let cwd = active_meta.and_then(|m| m.cwd.clone()).or(summary_cwd);
            let tmux_pane = active_meta.and_then(|m| m.tmux_pane.clone());
            // Active sessions upsert at wall-clock `at`. Inactive ones must not use
            // summary.json `updated_at` / `last_active_at` / `created_at`: a rewrite of
            // that file is not the chat transcript's age.
            let updated_ms = if is_active {
                summary_updated_ms.unwrap_or_else(|| mtime_ms(&chat_path))
            } else if chat_path.is_file() {
                mtime_ms(&chat_path)
            } else {
                mtime_ms(&summary_path)
            };

            let persona = active_meta
                .and_then(|m| m.env_persona.clone())
                .or_else(|| {
                    chat_path
                        .is_file()
                        .then(|| scan_transcript_for_persona(&chat_path, 120))
                        .flatten()
                })
                .or(summary_persona)
                .or_else(|| {
                    self.entries.values().find_map(|e| match &e.route {
                        VendorRoute::Grok {
                            session_id: existing_sid,
                            ..
                        } if existing_sid == &session_id => Some(e.persona.clone()),
                        _ => None,
                    })
                });
            let Some(persona) = persona else {
                continue;
            };
            candidates.push(GrokCandidate {
                is_active,
                updated_ms,
                persona,
                session_id,
                cwd,
                tmux_pane,
            });
        }

        // Sort ascending by `(is_active, updated_ms)` so active and newer sessions overwrite older ones.
        candidates.sort_by(|a, b| {
            a.is_active
                .cmp(&b.is_active)
                .then_with(|| a.updated_ms.cmp(&b.updated_ms))
        });
        for cand in candidates {
            let (existing_target, existing_pane) = match self.entries.get(&cand.persona) {
                Some(RouteEntry {
                    route:
                        VendorRoute::Grok {
                            tmux_target,
                            tmux_pane,
                            ..
                        },
                    ..
                }) => (tmux_target.clone(), tmux_pane.clone()),
                _ => (None, None),
            };
            let route = VendorRoute::Grok {
                session_id: cand.session_id,
                cwd: cand.cwd,
                tmux_target: existing_target,
                tmux_pane: cand.tmux_pane.or(existing_pane),
            };
            if cand.is_active {
                self.upsert(&cand.persona, route, at.clone());
            } else {
                self.upsert_discovered(&cand.persona, route, cand.updated_ms, at);
            }
        }

        if check_proc {
            for (persona, entry) in &mut self.entries {
                if let VendorRoute::Grok { tmux_target, .. } = &mut entry.route
                    && tmux_target.is_none()
                {
                    let mut cap = persona.clone();
                    if let Some(first) = cap.get_mut(0..1) {
                        first.make_ascii_uppercase();
                    }
                    *tmux_target = Some(format!("{cap}:grok"));
                }
            }
        }
    }
}

struct GrokActiveSession {
    cwd: Option<PathBuf>,
    env_persona: Option<String>,
    tmux_pane: Option<String>,
}

struct GrokCandidate {
    is_active: bool,
    updated_ms: u64,
    persona: String,
    session_id: String,
    cwd: Option<PathBuf>,
    tmux_pane: Option<String>,
}

fn read_grok_proc_env(pid: u32) -> (Option<String>, Option<String>) {
    let Ok(bytes) = fs::read(format!("/proc/{pid}/environ")) else {
        return (None, None);
    };
    let mut env_persona = None;
    let mut tmux_pane = None;
    for entry in bytes.split(|&b| b == 0) {
        let Ok(kv) = std::str::from_utf8(entry) else {
            continue;
        };
        if let Some(val) = kv
            .strip_prefix("AGENT_PERSONA=")
            .or_else(|| kv.strip_prefix("AGENT_NAME="))
        {
            if let Some(norm) = normalize_persona(val) {
                env_persona = Some(norm);
            }
        } else if let Some(pane) = kv.strip_prefix("TMUX_PANE=") {
            let trimmed = pane.trim();
            if !trimmed.is_empty() {
                tmux_pane = Some(trimmed.to_string());
            }
        }
    }
    (env_persona, tmux_pane)
}

fn read_grok_summary_metadata(
    summary_path: &Path,
) -> (Option<String>, Option<PathBuf>, Option<u64>, Option<String>) {
    let Ok(raw) = fs::read_to_string(summary_path) else {
        return (None, None, None, None);
    };
    let Ok(val) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return (None, None, None, None);
    };
    let sid = val
        .pointer("/info/id")
        .or_else(|| val.get("id"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned);
    let cwd = val
        .pointer("/info/cwd")
        .or_else(|| val.get("cwd"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);
    let updated_ms = val
        .get("updated_at")
        .or_else(|| val.get("last_active_at"))
        .or_else(|| val.get("created_at"))
        .and_then(|v| v.as_str())
        .and_then(parse_flexible_at)
        .map(|a| a.unix_millis());

    let mut persona = None;
    for field in ["session_summary", "generated_title", "last_turn_summary"] {
        if let Some(text) = val.get(field).and_then(|v| v.as_str()) {
            let trimmed = text.trim();
            let first_word: String = trimmed
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                .collect();
            let rest = trimmed[first_word.len()..].to_ascii_lowercase();
            if first_word
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_uppercase())
                && (rest.starts_with(" registers ")
                    || rest.starts_with(" registered")
                    || rest.starts_with(" online"))
                && let Some(norm) = normalize_claude_session_name(&first_word)
            {
                persona = Some(norm);
                break;
            }
        }
    }
    (sid, cwd, updated_ms, persona)
}

fn collect_codex_rollout_files(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > 4 || !dir.is_dir() {
        return;
    }
    let Ok(read_dir) = fs::read_dir(dir) else {
        return;
    };
    for entry in read_dir.flatten() {
        let p = entry.path();
        if p.is_dir() {
            collect_codex_rollout_files(&p, depth + 1, out);
        } else if p.extension().and_then(|s| s.to_str()) == Some("jsonl") {
            out.push(p);
        }
    }
}

fn extract_codex_rollout_meta(path: &Path) -> (String, Option<PathBuf>) {
    let fallback_stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let mut thread_id = String::new();
    let mut cwd = None;
    if let Ok(text) = fs::read_to_string(path) {
        for line in text.lines().take(20) {
            let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let is_response_item =
                val.get("type").and_then(|v| v.as_str()) == Some("response_item");
            if thread_id.is_empty()
                && !is_response_item
                && let Some(sid) = val
                    .pointer("/payload/session_id")
                    .or_else(|| val.pointer("/payload/id"))
                    .or_else(|| val.get("session_id"))
                    .or_else(|| val.get("id"))
                    .and_then(|v| v.as_str())
                && !sid.is_empty()
            {
                thread_id = sid.to_string();
            }
            if cwd.is_none()
                && let Some(dir) = val
                    .pointer("/payload/cwd")
                    .or_else(|| val.get("cwd"))
                    .and_then(|v| v.as_str())
                && !dir.is_empty()
            {
                cwd = Some(PathBuf::from(dir));
            }
            if !thread_id.is_empty() && cwd.is_some() {
                break;
            }
        }
    }
    if thread_id.is_empty() {
        if let Some(rest) = fallback_stem.strip_prefix("rollout-")
            && let Some(tail) = rest.get(rest.len().saturating_sub(36)..)
            && tail.len() == 36
        {
            thread_id = tail.to_string();
        } else {
            thread_id = fallback_stem.to_string();
        }
    }
    (thread_id, cwd)
}

fn extract_session_id_and_cwd(path: &Path, fallback_stem: &str) -> (String, Option<PathBuf>) {
    let mut session_id = fallback_stem.to_string();
    let mut cwd = None;
    if let Ok(text) = fs::read_to_string(path) {
        for line in text.lines().take(20) {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
                if let Some(sid) = val
                    .get("session_id")
                    .or_else(|| val.get("sessionId"))
                    .or_else(|| val.get("id"))
                    .and_then(|v| v.as_str())
                    && !sid.is_empty()
                {
                    session_id = sid.to_string();
                }
                if cwd.is_none()
                    && let Some(dir) = val.get("cwd").and_then(|v| v.as_str())
                    && !dir.is_empty()
                {
                    cwd = Some(PathBuf::from(dir));
                }
            }
        }
    }
    (session_id, cwd)
}

fn parse_flexible_at(raw: &str) -> Option<At> {
    let trimmed = raw.trim();
    if let Ok(at) = At::parse(trimmed) {
        return Some(at);
    }
    let without_tz = trimmed
        .strip_suffix('Z')
        .or_else(|| trimmed.strip_suffix('z'))
        .or_else(|| trimmed.strip_suffix("+00:00"))
        .or_else(|| trimmed.strip_suffix("-00:00"))
        .unwrap_or(trimmed);
    if without_tz.len() < 19 {
        return None;
    }
    let date_time = &without_tz[..19];
    let rest = &without_tz[19..];
    let millis_str = if rest.is_empty() {
        "000".to_string()
    } else {
        let frac = rest.strip_prefix('.')?;
        let digits: String = frac.chars().take_while(|c| c.is_ascii_digit()).collect();
        match digits.len() {
            0 => "000".to_string(),
            1 => format!("{digits}00"),
            2 => format!("{digits}0"),
            _ => digits[..3].to_string(),
        }
    };
    At::parse(&format!("{date_time}.{millis_str}Z")).ok()
}

fn parse_legacy_route_item(item: &serde_json::Value) -> Option<VendorRoute> {
    let vendor = item
        .get("vendor")
        .or_else(|| item.get("kind"))
        .and_then(|v| v.as_str())?;
    match vendor {
        "claude" => {
            let pid = item.get("pid").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let session_id = item
                .get("session_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let socket_path = PathBuf::from(item.get("socket_path")?.as_str()?);
            let key_path = item
                .get("key_path")
                .and_then(|v| v.as_str())
                .map(PathBuf::from);
            Some(VendorRoute::Claude {
                pid,
                session_id,
                socket_path,
                key_path,
            })
        }
        "agy" | "antigravity" => {
            let conversation_id = item.get("conversation_id")?.as_str()?.to_string();
            let ls_pid = item
                .get("ls_pid")
                .and_then(|v| v.as_u64())
                .map(|n| n as u32);
            let ls_address = item
                .get("ls_address")
                .and_then(|v| v.as_str())
                .map(ToOwned::to_owned);
            let csrf_token = item
                .get("csrf_token")
                .and_then(|v| v.as_str())
                .map(ToOwned::to_owned);
            let agentapi_exe = item
                .get("agentapi_exe")
                .and_then(|v| v.as_str())
                .map(PathBuf::from);
            Some(VendorRoute::Agy {
                conversation_id,
                ls_pid,
                ls_address,
                csrf_token,
                agentapi_exe,
            })
        }
        "codex" => {
            let thread_id = item
                .get("thread_id")
                .or_else(|| item.get("session_id"))?
                .as_str()?
                .to_string();
            let cwd = item.get("cwd").and_then(|v| v.as_str()).map(PathBuf::from);
            Some(VendorRoute::Codex { thread_id, cwd })
        }
        "muse" => {
            let session_id = item
                .get("session_id")
                .or_else(|| item.get("sessionId"))?
                .as_str()?
                .to_string();
            let cwd = item.get("cwd").and_then(|v| v.as_str()).map(PathBuf::from);
            let tmux_target = item
                .get("tmux_target")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToOwned::to_owned);
            let tmux_pane = item
                .get("tmux_pane")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToOwned::to_owned);
            Some(VendorRoute::Muse {
                session_id,
                cwd,
                tmux_target,
                tmux_pane,
            })
        }
        "grok" => {
            let session_id = item
                .get("session_id")
                .or_else(|| item.get("sessionId"))?
                .as_str()?
                .to_string();
            let cwd = item.get("cwd").and_then(|v| v.as_str()).map(PathBuf::from);
            let tmux_target = item
                .get("tmux_target")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToOwned::to_owned);
            let tmux_pane = item
                .get("tmux_pane")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToOwned::to_owned);
            Some(VendorRoute::Grok {
                session_id,
                cwd,
                tmux_target,
                tmux_pane,
            })
        }
        "webhook" => {
            let socket_path = PathBuf::from(item.get("socket_path")?.as_str()?);
            Some(VendorRoute::Webhook { socket_path })
        }
        _ => None,
    }
}

pub fn resolve_claude_key_path(
    sessions_dir: &Path,
    pid: u32,
    socket_path: &Path,
) -> Option<PathBuf> {
    let canonical = fs::canonicalize(socket_path).unwrap_or_else(|_| socket_path.to_path_buf());
    let mut hasher = Sha256::new();
    hasher.update(canonical.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(&mut hex, "{b:02x}");
    }
    let exact = sessions_dir.join(format!("{pid}.{hex}.key"));
    if exact.exists() {
        return Some(exact);
    }
    // Fallback: any `<pid>.*.key` in `sessions_dir`
    // `read_dir` order is unspecified; take the lexicographically first match so the same
    // directory always resolves to the same key file.
    let prefix = format!("{pid}.");
    let mut matches: Vec<PathBuf> = fs::read_dir(sessions_dir)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|p| {
            p.file_name()
                .and_then(|s| s.to_str())
                .is_some_and(|name| name.starts_with(&prefix) && name.ends_with(".key"))
        })
        .collect();
    matches.sort();
    matches.into_iter().next()
}

pub fn normalize_persona(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    validate_slug(trimmed).ok()
}

pub fn normalize_claude_session_name(raw: &str) -> Option<String> {
    let norm = normalize_persona(raw)?;
    if norm.starts_with("artificial-humanity-") {
        return None;
    }
    Some(norm)
}

fn extract_text_from_json_content(val: &serde_json::Value) -> Option<String> {
    if let Some(s) = val.as_str() {
        return Some(s.to_string());
    }
    if let Some(arr) = val.as_array() {
        let mut parts = Vec::new();
        for item in arr {
            if let Some(s) = item.as_str() {
                if !s.is_empty() {
                    parts.push(s.to_string());
                }
            } else if let Some(obj) = item.as_object() {
                let item_type = obj.get("type").and_then(|v| v.as_str());
                if matches!(item_type, None | Some("text" | "input_text"))
                    && let Some(t) = obj
                        .get("text")
                        .or_else(|| obj.get("content"))
                        .and_then(|v| v.as_str())
                    && !t.is_empty()
                {
                    parts.push(t.to_string());
                }
            }
        }
        if !parts.is_empty() {
            return Some(parts.join("\n"));
        }
    }
    None
}

fn mtime_ms(path: &Path) -> u64 {
    fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn transcript_is_grok_build_subagent(path: &Path) -> bool {
    let Ok(text) = fs::read_to_string(path) else {
        return false;
    };
    first_system_message_is_grok_subagent(&text)
}

/// The first `system` row of a Grok Build subagent session.
fn first_system_message_is_grok_subagent(text: &str) -> bool {
    for line in text.lines().take(200) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        let kind = val
            .get("type")
            .and_then(|v| v.as_str())
            .or_else(|| val.get("role").and_then(|v| v.as_str()));
        if !matches!(kind, Some("system" | "SYSTEM")) {
            continue;
        }
        let content = row_text(&val).unwrap_or_default();
        return content.contains("You are a Grok Build subagent");
    }
    false
}

fn row_text(val: &serde_json::Value) -> Option<String> {
    val.get("content")
        .and_then(extract_text_from_json_content)
        .or_else(|| {
            val.pointer("/message/content")
                .and_then(extract_text_from_json_content)
        })
        .or_else(|| {
            val.pointer("/payload/content")
                .and_then(extract_text_from_json_content)
        })
        .or_else(|| {
            val.pointer("/payload/body")
                .and_then(extract_text_from_json_content)
        })
        .or_else(|| val.get("text").and_then(extract_text_from_json_content))
        .or_else(|| val.get("output").and_then(extract_text_from_json_content))
}

fn push_json_text(parts: &mut Vec<String>, val: &serde_json::Value) {
    match val {
        serde_json::Value::String(text) => {
            if !text.is_empty() {
                parts.push(text.clone());
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                push_json_text(parts, item);
            }
        }
        serde_json::Value::Object(map) => {
            for child in map.values() {
                push_json_text(parts, child);
            }
        }
        _ => {}
    }
}

fn tool_call_argument_text(val: &serde_json::Value) -> String {
    let mut parts = Vec::new();
    for calls in [
        val.get("tool_calls"),
        val.get("toolCalls"),
        val.pointer("/message/tool_calls"),
        val.pointer("/payload/tool_calls"),
    ]
    .into_iter()
    .flatten()
    .filter_map(|v| v.as_array())
    {
        for call in calls {
            for key in ["arguments", "args", "input", "command"] {
                if let Some(arg) = call.get(key) {
                    push_json_text(&mut parts, arg);
                }
                if let Some(arg) = call.pointer(&format!("/function/{key}")) {
                    push_json_text(&mut parts, arg);
                }
            }
        }
    }
    parts.join("\n")
}

fn response_item_tool_input_text(val: &serde_json::Value) -> String {
    let mut parts = Vec::new();
    if let Some(payload) = val.get("payload") {
        for key in ["input", "arguments", "args", "command"] {
            if let Some(arg) = payload.get(key) {
                push_json_text(&mut parts, arg);
            }
        }
    }
    parts.join("\n")
}

fn is_tool_output_type(kind: Option<&str>) -> bool {
    matches!(
        kind,
        Some("tool_result" | "TOOL_RESULT" | "custom_tool_call_output" | "function_call_output")
    )
}

pub fn scan_transcript_for_persona(path: &Path, max_lines: usize) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    if first_system_message_is_grok_subagent(&text) {
        return None;
    }
    for line in text.lines().take(max_lines) {
        if line.trim().is_empty() {
            continue;
        }
        let candidate_text: String =
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
                for field in ["agentName", "customTitle", "persona"] {
                    if let Some(s) = val.get(field).and_then(|v| v.as_str())
                        && let Some(norm) = normalize_claude_session_name(s)
                    {
                        return Some(norm);
                    }
                }
                if val.get("isMeta").and_then(|v| v.as_bool()) == Some(true)
                    || val.get("isSidechain").and_then(|v| v.as_bool()) == Some(true)
                    || val.get("synthetic_reason").is_some()
                {
                    continue;
                }
                let t = val.get("type").and_then(|v| v.as_str());
                // Tool output includes repository source and test fixtures that name
                // other personas. Do not read those rows.
                if is_tool_output_type(t) {
                    continue;
                }
                if t == Some("response_item") {
                    let pt = val.pointer("/payload/type").and_then(|v| v.as_str());
                    if is_tool_output_type(pt) {
                        continue;
                    }
                    if matches!(pt, Some("custom_tool_call" | "function_call")) {
                        let args = response_item_tool_input_text(&val);
                        if args.is_empty() {
                            continue;
                        }
                        args
                    } else if pt == Some("message")
                        && val.pointer("/payload/role").and_then(|v| v.as_str()) == Some("user")
                        && let Some(c) = val
                            .pointer("/payload/content")
                            .and_then(extract_text_from_json_content)
                    {
                        let trimmed_c = c.trim_start();
                        if trimmed_c.starts_with("# AGENTS.md instructions for ")
                            || trimmed_c.starts_with("<environment_context>")
                            || trimmed_c.starts_with("<INSTRUCTIONS>")
                            || trimmed_c.starts_with("<permissions instructions>")
                            || trimmed_c.starts_with("<user_shell_command>")
                        {
                            continue;
                        }
                        c
                    } else {
                        continue;
                    }
                } else if matches!(t, Some("GENERIC" | "generic")) {
                    if line.contains("Showing lines ") {
                        continue;
                    }
                    let Some(s) = row_text(&val) else {
                        continue;
                    };
                    s
                } else if matches!(
                    t,
                    Some("assistant" | "PLANNER_RESPONSE" | "planner_response")
                ) {
                    let mut parts = Vec::new();
                    if let Some(s) = row_text(&val) {
                        parts.push(s);
                    }
                    let args = tool_call_argument_text(&val);
                    if !args.is_empty() {
                        parts.push(args);
                    }
                    if parts.is_empty() {
                        continue;
                    }
                    parts.join("\n")
                } else {
                    if !matches!(t, None | Some("USER_INPUT" | "user" | "human")) {
                        continue;
                    }
                    let Some(s) = row_text(&val) else {
                        continue;
                    };
                    s
                }
            } else {
                line.to_string()
            };
        let lower = candidate_text.to_ascii_lowercase();
        for marker in [
            "take the name of ",
            "agent-msg register ",
            "fl wire register ",
            "you will be ",
            "you are ",
        ] {
            if let Some(idx) = lower.find(marker) {
                let after = &candidate_text[idx + marker.len()..];
                let token: String = after
                    .trim_start_matches(['"', '\'', '`', ' ', '\\'])
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                    .collect();
                if matches!(marker, "you will be " | "you are ")
                    && !token.chars().next().is_some_and(|c| c.is_ascii_uppercase())
                {
                    continue;
                }
                let token_lower = token.to_ascii_lowercase();
                if matches!(
                    token_lower.as_str(),
                    "a" | "an"
                        | "the"
                        | "now"
                        | "in"
                        | "on"
                        | "at"
                        | "our"
                        | "my"
                        | "notified"
                        | "interactive"
                        | "antigravity"
                        | "claude"
                        | "codex"
                        | "muse"
                        | "grok"
                        | "working"
                        | "running"
                        | "powered"
                        | "taking"
                        | "hearing"
                        | "assistant"
                        | "lead"
                        | "scribe"
                        | "resident"
                        | "login"
                        | "pair"
                        | "connected"
                        | "not"
                        | "replacing"
                ) {
                    continue;
                }
                if let Some(norm) = normalize_claude_session_name(&token) {
                    return Some(norm);
                }
            }
        }
    }
    None
}

/// Capture the current process's vendor environment route so `fl wire` / `agent-msg`
/// can register it in the route table automatically.
pub fn capture_current_env_route() -> Option<VendorRoute> {
    if let Some(sock) = std::env::var_os("CLAUDE_CODE_MESSAGING_SOCKET") {
        let socket_path = PathBuf::from(sock);
        let pid = socket_path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or_else(std::process::id);
        let session_id = std::env::var("CLAUDE_SESSION_ID").unwrap_or_default();
        let key_path = std::env::var_os("HOME").map(PathBuf::from).and_then(|h| {
            resolve_claude_key_path(&h.join(".claude").join("sessions"), pid, &socket_path)
        });
        return Some(VendorRoute::Claude {
            pid,
            session_id,
            socket_path,
            key_path,
        });
    }
    if let Ok(conv_id) = std::env::var("ANTIGRAVITY_CONVERSATION_ID")
        && !conv_id.trim().is_empty()
    {
        return Some(VendorRoute::Agy {
            conversation_id: conv_id,
            ls_pid: None,
            ls_address: std::env::var("ANTIGRAVITY_LS_ADDRESS").ok(),
            csrf_token: std::env::var("ANTIGRAVITY_CSRF_TOKEN").ok(),
            agentapi_exe: std::env::var_os("ANTIGRAVITY_AGENTAPI_EXE").map(PathBuf::from),
        });
    }
    if let Ok(thread_id) = std::env::var("CODEX_THREAD_ID")
        && !thread_id.trim().is_empty()
    {
        return Some(VendorRoute::Codex {
            thread_id,
            cwd: std::env::current_dir().ok(),
        });
    }
    if let Ok(session_id) = std::env::var("MUSE_SESSION_ID")
        && !session_id.trim().is_empty()
    {
        return Some(VendorRoute::Muse {
            session_id,
            cwd: std::env::current_dir().ok(),
            tmux_target: None,
            tmux_pane: std::env::var("TMUX_PANE")
                .ok()
                .filter(|s| !s.trim().is_empty()),
        });
    }
    if let Ok(session_id) = std::env::var("GROK_SESSION_ID")
        && !session_id.trim().is_empty()
    {
        return Some(VendorRoute::Grok {
            session_id,
            cwd: std::env::current_dir().ok(),
            tmux_target: None,
            tmux_pane: std::env::var("TMUX_PANE")
                .ok()
                .filter(|s| !s.trim().is_empty()),
        });
    }
    if std::env::var("FL_WIRE_DISABLE_HOST_DISCOVERY").as_deref() != Ok("1")
        && let Some(home) = std::env::var_os("HOME").map(PathBuf::from)
        && let Some((session_id, cwd, tmux_pane)) =
            resolve_grok_pid_session(&home, std::process::id())
    {
        return Some(VendorRoute::Grok {
            session_id,
            cwd: cwd.or_else(|| std::env::current_dir().ok()),
            tmux_target: None,
            tmux_pane: tmux_pane.or_else(|| {
                std::env::var("TMUX_PANE")
                    .ok()
                    .filter(|s| !s.trim().is_empty())
            }),
        });
    }
    None
}

pub fn detect_current_route() -> Option<VendorRoute> {
    capture_current_env_route()
}

/// Resolve the caller's persona name in strict priority order (spec §3.1):
/// 1. Explicit flag/argument (`explicit`)
/// 2. `AGENT_PERSONA` or `AGENT_NAME` environment variable
/// 3. Vendor transcript / session / route-cache inspection
/// 4. CWD ancestor project lead lookup in `AgentRegistry`
/// 5. `"unknown"`
pub fn resolve_persona(
    explicit: Option<&str>,
    transcript_path: Option<&Path>,
    cwd: &Path,
    registry: Option<&dyn AgentRegistry>,
    custom_routes_path: Option<&Path>,
) -> String {
    resolve_persona_with_sources(
        explicit,
        transcript_path,
        cwd,
        registry,
        || RouteTable::load(custom_routes_path),
        &|k| std::env::var_os(k),
    )
}

/// [`resolve_persona`] with its ambient sources injected: `load_routes`
/// loads the route table (invoked only at the vendor route-matching
/// stage, so earlier hits never touch disk) and `get_env` supplies every
/// environment lookup (`AGENT_PERSONA`, vendor session ids, and the
/// `HOME` + socket pair behind the Claude pid/process-tree probe). Tests
/// pass a loader returning an empty table and a `get_env` that answers
/// `None`, so a live agent session on the host can never leak into the
/// result.
fn resolve_persona_with_sources(
    explicit: Option<&str>,
    transcript_path: Option<&Path>,
    cwd: &Path,
    registry: Option<&dyn AgentRegistry>,
    load_routes: impl FnOnce() -> RouteTable,
    get_env: &dyn Fn(&str) -> Option<std::ffi::OsString>,
) -> String {
    let get_str = |k: &str| get_env(k).and_then(|v| v.into_string().ok());
    if let Some(exp) = explicit
        && let Some(norm) = normalize_persona(exp)
    {
        return norm;
    }

    for var in ["AGENT_PERSONA", "AGENT_NAME"] {
        if let Some(val) = get_str(var)
            && let Some(norm) = normalize_persona(&val)
        {
            return norm;
        }
    }

    if let Some(tp) = transcript_path
        && let Some(norm) = scan_transcript_for_persona(tp, 50)
    {
        return norm;
    }

    if let Some(home) = get_env("HOME").map(PathBuf::from) {
        if let Some(sock) = get_env("CLAUDE_CODE_MESSAGING_SOCKET") {
            let sock_path = PathBuf::from(sock);
            if let Some(pid) = sock_path
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.parse::<u32>().ok())
                && let Some(norm) = resolve_claude_pid_persona(&home, pid)
            {
                return norm;
            }
        }
        if let Some(norm) = resolve_grok_pid_persona(&home, std::process::id()) {
            return norm;
        }
    }

    let routes = load_routes();
    if let Some(claude_sid) = get_str("CLAUDE_SESSION_ID")
        && !claude_sid.is_empty()
    {
        for entry in routes.entries.values() {
            if let VendorRoute::Claude { session_id, .. } = &entry.route
                && session_id == &claude_sid
            {
                return entry.persona.clone();
            }
        }
    }
    if let Some(agy_cid) = get_str("ANTIGRAVITY_CONVERSATION_ID")
        && !agy_cid.is_empty()
    {
        for entry in routes.entries.values() {
            if let VendorRoute::Agy {
                conversation_id, ..
            } = &entry.route
                && conversation_id == &agy_cid
            {
                return entry.persona.clone();
            }
        }
    }
    if let Some(codex_tid) = get_str("CODEX_THREAD_ID")
        && !codex_tid.is_empty()
    {
        for entry in routes.entries.values() {
            if let VendorRoute::Codex { thread_id, .. } = &entry.route
                && thread_id == &codex_tid
            {
                return entry.persona.clone();
            }
        }
    }
    if let Some(muse_sid) = get_str("MUSE_SESSION_ID")
        && !muse_sid.is_empty()
    {
        for entry in routes.entries.values() {
            if let VendorRoute::Muse { session_id, .. } = &entry.route
                && session_id == &muse_sid
            {
                return entry.persona.clone();
            }
        }
    }
    if let Some(grok_sid) = get_str("GROK_SESSION_ID")
        && !grok_sid.is_empty()
    {
        for entry in routes.entries.values() {
            if let VendorRoute::Grok { session_id, .. } = &entry.route
                && session_id == &grok_sid
            {
                return entry.persona.clone();
            }
        }
    }

    if let Some(reg) = registry
        && let Some(lead) = resolve_cwd_lead(cwd, reg)
    {
        return lead;
    }

    if let Some(persona) = resolve_cwd_persona_md(cwd) {
        return persona;
    }

    "unknown".to_string()
}

fn resolve_claude_session_persona(
    home: &Path,
    val: &serde_json::Value,
    existing_entries: Option<&BTreeMap<String, RouteEntry>>,
) -> Option<String> {
    let name_source = val.get("nameSource").and_then(|v| v.as_str());
    let derived_name = if name_source == Some("derived") {
        val.get("name")
            .and_then(|v| v.as_str())
            .and_then(normalize_persona)
    } else {
        None
    };
    if name_source != Some("derived")
        && let Some(name) = val.get("name").and_then(|v| v.as_str())
        && let Some(persona) = normalize_claude_session_name(name)
    {
        return Some(persona);
    }

    let sid = val
        .get("sessionId")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())?;

    if let Some(entries) = existing_entries {
        for entry in entries.values() {
            if Some(&entry.persona) == derived_name.as_ref() {
                continue;
            }
            if let VendorRoute::Claude { session_id, .. } = &entry.route
                && session_id == sid
            {
                return Some(entry.persona.clone());
            }
        }
    }

    let projects_dir = home.join(".claude").join("projects");
    let target_fname = format!("{sid}.jsonl");
    if let Ok(proj_dirs) = fs::read_dir(&projects_dir) {
        for pentry in proj_dirs.flatten() {
            let candidate = pentry.path().join(&target_fname);
            if candidate.exists()
                && let Some(norm) = scan_transcript_for_persona(&candidate, 100)
            {
                return Some(norm);
            }
        }
    }
    None
}

fn resolve_claude_pid_persona(home: &Path, start_pid: u32) -> Option<String> {
    let sessions_dir = home.join(".claude").join("sessions");
    let projects_dir = home.join(".claude").join("projects");
    let mut pid = start_pid;
    for _ in 0..8 {
        if pid <= 1 {
            break;
        }
        let session_file = sessions_dir.join(format!("{pid}.json"));
        if let Ok(raw) = fs::read_to_string(&session_file)
            && let Ok(val) = serde_json::from_str::<serde_json::Value>(&raw)
            && let Some(norm) = resolve_claude_session_persona(home, &val, None)
        {
            return Some(norm);
        }
        if let Ok(cmdline_bytes) = fs::read(format!("/proc/{pid}/cmdline")) {
            let args: Vec<&str> = cmdline_bytes
                .split(|&b| b == 0)
                .filter_map(|s| std::str::from_utf8(s).ok())
                .collect();
            for win in args.windows(2) {
                if (win[0] == "--session-id" || win[0] == "--resume") && !win[1].is_empty() {
                    let target_fname = format!("{}.jsonl", win[1]);
                    if let Ok(proj_dirs) = fs::read_dir(&projects_dir) {
                        for pentry in proj_dirs.flatten() {
                            let candidate = pentry.path().join(&target_fname);
                            if candidate.exists()
                                && let Some(norm) = scan_transcript_for_persona(&candidate, 50)
                            {
                                return Some(norm);
                            }
                        }
                    }
                }
            }
        }
        pid = parent_pid_of(pid)?;
    }
    None
}

fn resolve_grok_pid_session(
    home: &Path,
    start_pid: u32,
) -> Option<(String, Option<PathBuf>, Option<String>)> {
    let active_path = home.join(".grok").join("active_sessions.json");
    let raw = fs::read_to_string(active_path).ok()?;
    let arr = serde_json::from_str::<Vec<serde_json::Value>>(&raw).ok()?;
    let mut pid = start_pid;
    for _ in 0..8 {
        if pid <= 1 {
            break;
        }
        for item in &arr {
            let item_pid = item.get("pid").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            if item_pid == pid {
                let sid = item
                    .get("session_id")
                    .or_else(|| item.get("sessionId"))
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())?
                    .to_string();
                let cwd = item
                    .get("cwd")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(PathBuf::from);
                let (_env_persona, tmux_pane) = read_grok_proc_env(pid);
                return Some((sid, cwd, tmux_pane));
            }
        }
        pid = parent_pid_of(pid)?;
    }
    None
}

fn resolve_grok_pid_persona(home: &Path, start_pid: u32) -> Option<String> {
    let (sid, _cwd, _pane) = resolve_grok_pid_session(home, start_pid)?;
    let sessions_dir = home.join(".grok").join("sessions");
    let Ok(read_dir) = fs::read_dir(&sessions_dir) else {
        return None;
    };
    for entry in read_dir.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let candidate_dir = if path.file_name().and_then(|s| s.to_str()) == Some(sid.as_str()) {
            Some(path)
        } else {
            let nested = path.join(&sid);
            nested.is_dir().then_some(nested)
        };
        if let Some(dir) = candidate_dir {
            let chat_path = dir.join("chat_history.jsonl");
            if chat_path.is_file() && transcript_is_grok_build_subagent(&chat_path) {
                return None;
            }
            if chat_path.is_file()
                && let Some(norm) = scan_transcript_for_persona(&chat_path, 120)
            {
                return Some(norm);
            }
            let summary_path = dir.join("summary.json");
            let (_s, _c, _u, summary_persona) = read_grok_summary_metadata(&summary_path);
            if summary_persona.is_some() {
                return summary_persona;
            }
        }
    }
    None
}

fn resolve_cwd_persona_md(cwd: &Path) -> Option<String> {
    for ancestor in cwd.ancestors() {
        let p = ancestor.join("PERSONA.md");
        let Ok(text) = fs::read_to_string(&p) else {
            continue;
        };
        for line in text.lines() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("You are ") {
                let candidate: String = rest
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                    .collect();
                if !candidate.eq_ignore_ascii_case("the")
                    && !candidate.eq_ignore_ascii_case("a")
                    && !candidate.eq_ignore_ascii_case("an")
                    && let Some(norm) = normalize_persona(&candidate)
                {
                    return Some(norm);
                }
            }
        }
    }
    None
}

fn parent_pid_of(pid: u32) -> Option<u32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rparen = stat.rfind(')')?;
    let after = stat.get(rparen + 2..)?;
    let mut parts = after.split_whitespace();
    let _state = parts.next()?;
    let ppid = parts.next()?.parse::<u32>().ok()?;
    Some(ppid)
}

fn resolve_cwd_lead(cwd: &Path, registry: &dyn AgentRegistry) -> Option<String> {
    let areas = registry.list_areas().ok()?;
    let agents = registry.list_agents(None, false).ok()?;
    for ancestor in cwd.ancestors() {
        let Some(base) = ancestor.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let norm_base = base.to_ascii_lowercase();
        for area in &areas {
            let repo_match = area.path.as_ref().is_some_and(|rp| {
                let trimmed = rp.trim_end_matches('/');
                trimmed.eq_ignore_ascii_case(base)
            });
            if (area.area_key == norm_base || repo_match)
                && let Some(lead) = agents.iter().find(|a| {
                    a.status == AgentStatus::Active
                        && a.role == AgentRole::Lead
                        && a.project.eq_ignore_ascii_case(&area.area_key)
                })
            {
                return Some(lead.name_key.clone());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::agent::{AreaRecord, RegisterAgentArgs};
    use fl_core::mem::MemStore;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tempfile::TempDir;

    #[test]
    #[should_panic(expected = "never the machine's real route table")]
    fn test_code_cannot_load_the_default_route_table() {
        let _ = RouteTable::load(None);
    }

    #[test]
    #[should_panic(expected = "never the machine's real route table")]
    fn test_code_cannot_save_to_the_default_route_table() {
        let _ = RouteTable::default().save(None);
    }

    #[test]
    fn resolve_persona_respects_priority_and_cwd_lead_fallback() {
        let tmp = TempDir::new().unwrap();
        let store = MemStore::default();
        store
            .upsert_area(
                AreaRecord::new(
                    "FerroLoop",
                    Some("FerroLoop/".into()),
                    "Agent orchestration",
                    At::from_unix_millis(10),
                    "owner",
                )
                .unwrap(),
            )
            .unwrap();
        store
            .register_agent(RegisterAgentArgs {
                display_name: "Ferris".into(),
                email: None,
                project: "ferroloop".into(),
                role: AgentRole::Lead,
                extra_areas: vec![],
                create_area: false,
                replace: false,
                at: At::from_unix_millis(11),
                by: "owner".into(),
            })
            .unwrap();

        let proj_dir = tmp.path().join("FerroLoop").join("crates");
        fs::create_dir_all(&proj_dir).unwrap();

        // Isolated sources: no env, no host `HOME`/process probe, no routes.
        // A live agent session on the test host (its `AGENT_PERSONA`, vendor
        // session ids, or route cache) must never change these results.
        let no_env = |_: &str| -> Option<std::ffi::OsString> { None };

        // 1. Explicit flag wins
        assert_eq!(
            resolve_persona_with_sources(
                Some("Jarvis"),
                None,
                &proj_dir,
                Some(&store),
                RouteTable::default,
                &no_env,
            ),
            "jarvis"
        );

        // 2. An injected `AGENT_PERSONA` outranks the transcript and CWD lead
        let persona_env = |k: &str| -> Option<std::ffi::OsString> {
            (k == "AGENT_PERSONA").then(|| "Felix".into())
        };
        assert_eq!(
            resolve_persona_with_sources(
                None,
                None,
                &proj_dir,
                Some(&store),
                RouteTable::default,
                &persona_env,
            ),
            "felix"
        );

        // 3. Transcript scan works when explicit is None
        let transcript = tmp.path().join("transcript.jsonl");
        fs::write(&transcript, "{\"type\":\"meta\",\"agentName\":\"Kayla\"}\n").unwrap();
        assert_eq!(
            resolve_persona_with_sources(
                None,
                Some(&transcript),
                &proj_dir,
                Some(&store),
                RouteTable::default,
                &no_env,
            ),
            "kayla"
        );

        // 4. CWD project lead fallback works when neither explicit nor transcript matches
        assert_eq!(
            resolve_persona_with_sources(
                None,
                None,
                &proj_dir,
                Some(&store),
                RouteTable::default,
                &no_env,
            ),
            "ferris"
        );
    }

    #[test]
    fn route_table_discovers_claude_agy_and_codex_sessions() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();

        // Create mock Claude session + key file
        let claude_sessions = home.join(".claude").join("sessions");
        fs::create_dir_all(&claude_sessions).unwrap();
        let sock_path = home.join("cc.sock");
        fs::write(&sock_path, "").unwrap();
        fs::write(
            claude_sessions.join("4242.json"),
            serde_json::json!({
                "name": "Sonya",
                "sessionId": "sess-123",
                "messagingSocketPath": sock_path,
            })
            .to_string(),
        )
        .unwrap();
        fs::write(
            claude_sessions.join("4242.anyhash.key"),
            "{\"peerToken\":\"secret-tok\"}",
        )
        .unwrap();
        // Auto-generated default session name should be ignored
        fs::write(
            claude_sessions.join("4243.json"),
            serde_json::json!({
                "name": "artificial-humanity-1234",
                "sessionId": "sess-ignored",
                "messagingSocketPath": sock_path,
            })
            .to_string(),
        )
        .unwrap();

        // Create mock Antigravity cli.log + brain transcript
        let agy_dir = home.join(".gemini").join("antigravity-cli");
        let conv_logs = agy_dir
            .join("brain")
            .join("conv-abc")
            .join(".system_generated")
            .join("logs");
        fs::create_dir_all(&conv_logs).unwrap();
        fs::write(
            agy_dir.join("cli.log"),
            "2026-10-24 Language server listening on random port at 54321 for HTTP\n",
        )
        .unwrap();
        fs::write(
            conv_logs.join("transcript.jsonl"),
            "{\"type\":\"USER_INPUT\",\"content\":\"You will take up residence in ArtificialHumanity.io. You will take the name of Kacey.\"}\n",
        )
        .unwrap();

        // Create mock Codex session_index.jsonl
        let codex_dir = home.join(".codex");
        fs::create_dir_all(&codex_dir).unwrap();
        fs::write(
            codex_dir.join("session_index.jsonl"),
            "{\"id\":\"thread-99\",\"agentName\":\"Kage\",\"cwd\":\"/tmp\"}\n",
        )
        .unwrap();

        // Create mock Meta Muse session file
        let muse_dir = home
            .join(".local")
            .join("share")
            .join("muse")
            .join("sessions");
        fs::create_dir_all(&muse_dir).unwrap();
        fs::write(
            muse_dir.join("muse-sess-1.jsonl"),
            "{\"session_id\":\"muse-sess-1\",\"agentName\":\"Mira\",\"cwd\":\"/tmp\"}\n",
        )
        .unwrap();

        // Create mock xAI Grok session file
        let grok_dir = home.join(".grok").join("sessions");
        fs::create_dir_all(&grok_dir).unwrap();
        fs::write(
            grok_dir.join("grok-sess-1.jsonl"),
            "{\"sessionId\":\"grok-sess-1\",\"agentName\":\"Gideon\",\"cwd\":\"/tmp\"}\n",
        )
        .unwrap();

        let mut table = RouteTable::default();
        table.discover_routes(Some(home), At::from_unix_millis(100));

        assert!(matches!(
            table.get("sonya").map(|e| &e.route),
            Some(VendorRoute::Claude {
                pid: 4242,
                key_path: Some(_),
                ..
            })
        ));
        assert!(table.get("artificial-humanity-1234").is_none());
        assert!(matches!(
            table.get("kacey").map(|e| &e.route),
            Some(VendorRoute::Agy {
                conversation_id,
                ls_address: Some(addr),
                ..
            }) if conversation_id == "conv-abc" && addr == "127.0.0.1:54321"
        ));
        assert!(matches!(
            table.get("kage").map(|e| &e.route),
            Some(VendorRoute::Codex { thread_id, .. }) if thread_id == "thread-99"
        ));
        assert!(matches!(
            table.get("mira").map(|e| &e.route),
            Some(VendorRoute::Muse { session_id, .. }) if session_id == "muse-sess-1"
        ));
        assert!(matches!(
            table.get("gideon").map(|e| &e.route),
            Some(VendorRoute::Grok { session_id, .. }) if session_id == "grok-sess-1"
        ));
    }

    #[test]
    fn legacy_routes_json_preserves_updated_at_and_newer_legacy_wins_over_older_primary() {
        let tmp = TempDir::new().unwrap();
        let legacy_path = tmp.path().join("legacy-routes.json");
        fs::write(
            &legacy_path,
            serde_json::json!({
                "ferris": {
                    "vendor": "agy",
                    "conversation_id": "conv-live-new",
                    "updated_at": "2026-10-27T16:00:00Z"
                },
                "sonya": {
                    "vendor": "claude",
                    "pid": 1234,
                    "session_id": "sess-old",
                    "socket_path": "/tmp/sonya-old.sock",
                    "updated_at": "2026-10-20T00:00:00Z"
                }
            })
            .to_string(),
        )
        .unwrap();

        let legacy_table = RouteTable::load_file(&legacy_path).expect("should parse legacy routes");
        let ferris_legacy = legacy_table.get("ferris").unwrap();
        assert_eq!(
            ferris_legacy.updated_at,
            At::parse("2026-10-27T16:00:00.000Z").unwrap()
        );

        let mut primary = RouteTable::default();
        primary.upsert(
            "ferris",
            VendorRoute::Agy {
                conversation_id: "conv-stale-subagent".into(),
                ls_pid: None,
                ls_address: None,
                csrf_token: None,
                agentapi_exe: None,
            },
            At::parse("2026-10-25T12:00:00.000Z").unwrap(),
        );
        primary.upsert(
            "sonya",
            VendorRoute::Claude {
                pid: 5678,
                session_id: "sess-newer".into(),
                socket_path: PathBuf::from("/tmp/sonya-newer.sock"),
                key_path: None,
            },
            At::parse("2026-10-27T15:00:00.000Z").unwrap(),
        );

        primary.merge_newer(legacy_table);

        assert!(matches!(
            primary.get("ferris").map(|e| &e.route),
            Some(VendorRoute::Agy { conversation_id, .. }) if conversation_id == "conv-live-new"
        ));
        assert!(matches!(
            primary.get("sonya").map(|e| &e.route),
            Some(VendorRoute::Claude { session_id, .. }) if session_id == "sess-newer"
        ));
    }

    #[test]
    fn discover_agy_routes_overwrites_stale_pre_reboot_ls_address_and_csrf_token() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let agy_dir = home.join(".gemini").join("antigravity-cli");
        fs::create_dir_all(agy_dir.join("brain")).unwrap();
        fs::write(
            agy_dir.join("cli.log"),
            "I1028 17:44:33.477185 107 server.go:766] Language server listening on random port at 45143 for HTTPS (gRPC)\n\
             I1028 17:44:33.477218 107 server.go:774] Language server listening on random port at 41547 for HTTP\n",
        )
        .unwrap();

        let mut table = RouteTable::default();
        // Pre-reboot stale entry for kayla (alphabetically earlier or older timestamp)
        table.upsert(
            "kayla",
            VendorRoute::Agy {
                conversation_id: "conv-kayla".into(),
                ls_pid: None,
                ls_address: Some("localhost:36313".into()),
                csrf_token: Some("stale-csrf-pre-reboot".into()),
                agentapi_exe: None,
            },
            At::parse("2026-10-28T15:00:00.000Z").unwrap(),
        );
        // Post-reboot refreshed entry for ferris with newer updated_at and new csrf_token
        table.upsert(
            "ferris",
            VendorRoute::Agy {
                conversation_id: "conv-ferris".into(),
                ls_pid: None,
                ls_address: Some("localhost:41547".into()),
                csrf_token: Some("fresh-csrf-post-reboot".into()),
                agentapi_exe: None,
            },
            At::parse("2026-10-28T17:50:00.000Z").unwrap(),
        );

        table.discover_routes(Some(home), At::parse("2026-10-28T17:55:00.000Z").unwrap());

        assert!(matches!(
            table.get("kayla").map(|e| &e.route),
            Some(VendorRoute::Agy {
                ls_address: Some(addr),
                csrf_token: Some(csrf),
                ..
            }) if addr == "127.0.0.1:41547" && csrf == "fresh-csrf-post-reboot"
        ));
    }

    #[test]
    fn discover_claude_routes_ignores_derived_name_and_resolves_from_project_transcript() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();

        let claude_sessions = home.join(".claude").join("sessions");
        fs::create_dir_all(&claude_sessions).unwrap();
        let sock_path = home.join("cc.sock");
        fs::write(&sock_path, "").unwrap();
        let key_path = claude_sessions.join("1419565.deadbeef.key");
        fs::write(&key_path, r#"{"peerToken":"tok-login"}"#).unwrap();

        // 1. Derived session from `claude login` with no agent transcript should be ignored
        fs::write(
            claude_sessions.join("1419565.json"),
            serde_json::json!({
                "pid": 1419565,
                "name": "lmcfarlin-e6",
                "nameSource": "derived",
                "sessionId": "2d86ba53-a2a1-41e0-8ee9-88dc0acbb997",
                "messagingSocketPath": sock_path,
            })
            .to_string(),
        )
        .unwrap();

        // 2. Derived session with a matching ~/.claude/projects/<proj>/<sessionId>.jsonl transcript resolves persona
        let key_path_wendy = claude_sessions.join("1419566.deadbeef.key");
        fs::write(&key_path_wendy, r#"{"peerToken":"tok-wendy"}"#).unwrap();
        fs::write(
            claude_sessions.join("1419566.json"),
            serde_json::json!({
                "pid": 1419566,
                "name": "artificialhumanity-a1",
                "nameSource": "derived",
                "sessionId": "wendy-session-uuid",
                "messagingSocketPath": sock_path,
            })
            .to_string(),
        )
        .unwrap();
        let proj_dir = home
            .join(".claude")
            .join("projects")
            .join("-home-lmcfarlin-Projects-Artificial-Humanity-ArtificialHumanity-io");
        fs::create_dir_all(&proj_dir).unwrap();
        // Login transcript with Claude Code attachment containing "You will be notified when it exits"
        fs::write(
            proj_dir.join("2d86ba53-a2a1-41e0-8ee9-88dc0acbb997.jsonl"),
            r#"{"type":"attachment","attachment":{"type":"prompt_snapshot","systemPrompt":["You will be notified when it exits — do not poll its output file while waiting."]}}"#,
        )
        .unwrap();
        fs::write(
            proj_dir.join("wendy-session-uuid.jsonl"),
            concat!(
                r#"{"type":"attachment","attachment":{"type":"prompt_snapshot","systemPrompt":["You will be notified when it exits."]}}"#,
                "\n",
                r#"{"type":"user","message":{"role":"user","content":"You are Wendy and you are an assistant on the Website project."}}"#
            ),
        )
        .unwrap();

        let mut table = RouteTable::default();
        // Pre-populate stale derived route key to verify it is evicted and not reused
        table.upsert(
            "lmcfarlin-e6",
            VendorRoute::Claude {
                pid: 1419565,
                session_id: "2d86ba53-a2a1-41e0-8ee9-88dc0acbb997".into(),
                socket_path: sock_path.clone(),
                key_path: Some(key_path.clone()),
            },
            At::parse("2026-10-29T00:48:00.000Z").unwrap(),
        );
        table.discover_routes(Some(home), At::parse("2026-10-29T00:50:00.000Z").unwrap());

        assert!(
            table.get("lmcfarlin-e6").is_none(),
            "derived session without transcript must not register as a persona"
        );
        assert!(
            table.get("notified").is_none(),
            "attachment prompt_snapshot 'You will be notified' must not resolve as a persona"
        );
        assert!(matches!(
            table.get("wendy").map(|e| &e.route),
            Some(VendorRoute::Claude {
                pid: 1419566,
                session_id,
                ..
            }) if session_id == "wendy-session-uuid"
        ));
    }

    #[test]
    fn discover_grok_routes_resolves_nested_session_dir_and_active_sessions_json() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();

        let grok_dir = home.join(".grok");
        let nested_session_dir = grok_dir
            .join("sessions")
            .join("%2Fhome%2Flmcfarlin%2FProjects%2FArtificial-Humanity")
            .join("01a18538-6f1a-79c2-99d2-8e8540366c72");
        fs::create_dir_all(&nested_session_dir).unwrap();

        fs::write(
            grok_dir.join("active_sessions.json"),
            serde_json::json!([
                {
                    "session_id": "01a18538-6f1a-79c2-99d2-8e8540366c72",
                    "pid": 1535066,
                    "cwd": "/home/lmcfarlin/Projects/Artificial-Humanity",
                    "opened_at": "2026-10-29T01:13:50.916892837Z"
                }
            ])
            .to_string(),
        )
        .unwrap();

        fs::write(
            nested_session_dir.join("chat_history.jsonl"),
            [
                serde_json::json!({
                    "type": "system",
                    "content": "You are Grok 4.7 released by xAI."
                })
                .to_string(),
                serde_json::json!({
                    "type": "user",
                    "synthetic_reason": "system_reminder",
                    "content": [{"type": "text", "text": "<system-reminder>\nYou are interactive.\n</system-reminder>"}]
                })
                .to_string(),
                serde_json::json!({
                    "type": "user",
                    "prompt_index": 0,
                    "content": [{"type": "text", "text": "<user_query>\nYou are Wendy. Please read agent instruction files in front of you.\n</user_query>"}]
                })
                .to_string(),
            ]
            .join("\n"),
        )
        .unwrap();

        fs::write(
            nested_session_dir.join("summary.json"),
            serde_json::json!({
                "info": {
                    "id": "01a18538-6f1a-79c2-99d2-8e8540366c72",
                    "cwd": "/home/lmcfarlin/Projects/Artificial-Humanity"
                },
                "session_summary": "Wendy registers as Website assistant to Wesley",
                "updated_at": "2026-10-29T01:18:31.323100755Z",
                "current_model_id": "grok-4.7",
                "context_window": 500000
            })
            .to_string(),
        )
        .unwrap();

        let mut table = RouteTable::default();
        table.discover_routes(Some(home), At::parse("2026-10-29T01:20:00.000Z").unwrap());

        assert!(matches!(
            table.get("wendy").map(|e| &e.route),
            Some(VendorRoute::Grok {
                session_id,
                cwd: Some(cwd),
                ..
            }) if session_id == "01a18538-6f1a-79c2-99d2-8e8540366c72"
                && cwd == Path::new("/home/lmcfarlin/Projects/Artificial-Humanity")
        ));
    }

    #[test]
    fn discover_codex_routes_resolves_nested_rollout_transcript() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();

        let codex_dir = home.join(".codex");
        let day_dir = codex_dir
            .join("sessions")
            .join("2026")
            .join("10")
            .join("29");
        fs::create_dir_all(&day_dir).unwrap();

        fs::write(
            codex_dir.join("session_index.jsonl"),
            r#"{"id":"01a18550-1b11-7882-82b6-ea40058ed587","thread_name":"Report to Kayla","updated_at":"2026-10-29T01:40:39.621480658Z"}"#,
        )
        .unwrap();

        let rollout_path =
            day_dir.join("rollout-2026-10-29T01-39-41-01a18550-1b11-7882-82b6-ea40058ed587.jsonl");
        fs::write(
            &rollout_path,
            [
                serde_json::json!({
                    "timestamp": "2026-10-29T01:39:41.000Z",
                    "type": "session_meta",
                    "payload": {
                        "id": "01a18550-1b11-7882-82b6-ea40058ed587",
                        "cwd": "/home/lmcfarlin/Projects/Artificial-Humanity"
                    }
                })
                .to_string(),
                serde_json::json!({
                    "timestamp": "2026-10-29T01:39:41.100Z",
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "user",
                        "content": [{"type": "input_text", "text": "# AGENTS.md instructions for /home/lmcfarlin/Projects/Artificial-Humanity\n\nTake the name of <Name>"}]
                    }
                })
                .to_string(),
                serde_json::json!({
                    "timestamp": "2026-10-29T01:40:14.794Z",
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "user",
                        "content": [{"type": "input_text", "text": "You are Katie. Please review the agent instructions in front of you and note the agent registration process."}]
                    }
                })
                .to_string(),
            ]
            .join("\n"),
        )
        .unwrap();

        let mut table = RouteTable::default();
        table.discover_routes(Some(home), At::parse("2026-10-29T01:45:00.000Z").unwrap());

        assert!(matches!(
            table.get("katie").map(|e| &e.route),
            Some(VendorRoute::Codex {
                thread_id,
                cwd: Some(cwd),
            }) if thread_id == "01a18550-1b11-7882-82b6-ea40058ed587"
                && cwd == Path::new("/home/lmcfarlin/Projects/Artificial-Humanity")
        ));
    }

    fn set_mtime(path: &Path, ms: u64) {
        let file = fs::OpenOptions::new().write(true).open(path).unwrap();
        let when = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(ms);
        file.set_modified(when).unwrap();
    }

    #[test]
    fn scan_ignores_tool_output_subagents_and_read_file_dumps() {
        let tmp = TempDir::new().unwrap();
        let sub = tmp.path().join("sub.jsonl");
        fs::write(
            &sub,
            [
                serde_json::json!({
                    "type": "system",
                    "content": "You are a Grok Build subagent. Read the file and report."
                })
                .to_string(),
                serde_json::json!({
                    "type": "tool_result",
                    "content": "AGENT_PERSONA=katie\nYou are Katie"
                })
                .to_string(),
            ]
            .join("\n"),
        )
        .unwrap();
        assert!(scan_transcript_for_persona(&sub, 50).is_none());

        let tools = tmp.path().join("tools.jsonl");
        fs::write(
            &tools,
            [
                serde_json::json!({
                    "type": "tool_result",
                    "content": "agent-msg register Katie <katie@example.com>"
                })
                .to_string(),
                serde_json::json!({
                    "type": "custom_tool_call_output",
                    "output": "You are Penelope"
                })
                .to_string(),
                serde_json::json!({
                    "type": "GENERIC",
                    "content": "Showing lines 1-20 of discovery.rs\nAGENT_PERSONA=katie"
                })
                .to_string(),
            ]
            .join("\n"),
        )
        .unwrap();
        assert!(scan_transcript_for_persona(&tools, 50).is_none());

        let generic = tmp.path().join("generic.jsonl");
        fs::write(
            &generic,
            serde_json::json!({
                "type": "GENERIC",
                "content": "agent-msg register Wesley"
            })
            .to_string(),
        )
        .unwrap();
        assert_eq!(
            scan_transcript_for_persona(&generic, 20).as_deref(),
            Some("wesley")
        );

        let assistant = tmp.path().join("assistant.jsonl");
        fs::write(
            &assistant,
            serde_json::json!({
                "type": "assistant",
                "tool_calls": [{
                    "function": {"arguments": "agent-msg register Penelope"}
                }]
            })
            .to_string(),
        )
        .unwrap();
        assert_eq!(
            scan_transcript_for_persona(&assistant, 20).as_deref(),
            Some("penelope")
        );

        let planner = tmp.path().join("planner.jsonl");
        fs::write(
            &planner,
            serde_json::json!({
                "type": "PLANNER_RESPONSE",
                "content": "You are Sonya. Continue the review."
            })
            .to_string(),
        )
        .unwrap();
        assert_eq!(
            scan_transcript_for_persona(&planner, 20).as_deref(),
            Some("sonya")
        );

        let planner_cmd = tmp.path().join("planner-cmd.jsonl");
        fs::write(
            &planner_cmd,
            serde_json::json!({
                "type": "PLANNER_RESPONSE",
                "tool_calls": [{
                    "name": "run_command",
                    "arguments": {"cmd": "agent-msg register Katie"}
                }]
            })
            .to_string(),
        )
        .unwrap();
        assert_eq!(
            scan_transcript_for_persona(&planner_cmd, 20).as_deref(),
            Some("katie")
        );

        let codex_call = tmp.path().join("codex-call.jsonl");
        fs::write(
            &codex_call,
            serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "custom_tool_call",
                    "name": "exec",
                    "input": "agent-msg register Wesley"
                }
            })
            .to_string(),
        )
        .unwrap();
        assert_eq!(
            scan_transcript_for_persona(&codex_call, 20).as_deref(),
            Some("wesley")
        );

        let codex_fn = tmp.path().join("codex-fn.jsonl");
        fs::write(
            &codex_fn,
            serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "function_call",
                    "name": "shell",
                    "input": "agent-msg register Penelope"
                }
            })
            .to_string(),
        )
        .unwrap();
        assert_eq!(
            scan_transcript_for_persona(&codex_fn, 20).as_deref(),
            Some("penelope")
        );

        let codex_out = tmp.path().join("codex-out.jsonl");
        fs::write(
            &codex_out,
            serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "custom_tool_call_output",
                    "output": "agent-msg register Katie"
                }
            })
            .to_string(),
        )
        .unwrap();
        assert!(scan_transcript_for_persona(&codex_out, 20).is_none());
    }

    #[test]
    fn grok_build_subagent_session_is_not_a_route() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let session = home
            .join(".grok")
            .join("sessions")
            .join("proj")
            .join("01a18599-c714-7700-85bb-e276825a98b2");
        fs::create_dir_all(&session).unwrap();
        fs::write(
            session.join("chat_history.jsonl"),
            [
                serde_json::json!({
                    "type": "system",
                    "content": "You are a Grok Build subagent. Inspect the diff."
                })
                .to_string(),
                serde_json::json!({
                    "type": "tool_result",
                    "content": "fn test(){ let _ = \"AGENT_PERSONA=katie\"; }"
                })
                .to_string(),
            ]
            .join("\n"),
        )
        .unwrap();
        fs::write(
            session.join("summary.json"),
            serde_json::json!({
                "info": {"id": "01a18599-c714-7700-85bb-e276825a98b2"},
                "session_summary": "Katie registers from a tool result",
                "updated_at": "2026-10-29T03:00:00.000Z"
            })
            .to_string(),
        )
        .unwrap();
        let mut table = RouteTable::default();
        table.discover_routes(Some(home), At::parse("2026-10-29T04:00:00.000Z").unwrap());
        assert!(table.get("katie").is_none());
    }

    #[test]
    fn stale_historical_sessions_do_not_overwrite_a_newer_route() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let old_ms = At::parse("2026-09-16T10:40:52.000Z").unwrap().unix_millis();
        let live_at = At::parse("2026-10-29T01:42:59.000Z").unwrap();

        let day = home
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("09")
            .join("16");
        fs::create_dir_all(&day).unwrap();
        let rollout = day.join("rollout-2026-09-16T10-40-52-01a0a9ce-old.jsonl");
        fs::write(
            &rollout,
            serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "You are Wesley. Old Codex session."}]
                }
            })
            .to_string(),
        )
        .unwrap();
        set_mtime(&rollout, old_ms);

        let muse_dir = home
            .join(".local")
            .join("share")
            .join("muse")
            .join("sessions");
        fs::create_dir_all(&muse_dir).unwrap();
        let muse = muse_dir.join("old-muse.jsonl");
        fs::write(
            &muse,
            serde_json::json!({
                "type": "user",
                "content": "You are Wesley. Old Muse session."
            })
            .to_string(),
        )
        .unwrap();
        set_mtime(&muse, old_ms);

        let grok = home
            .join(".grok")
            .join("sessions")
            .join("proj")
            .join("old-grok-session");
        fs::create_dir_all(&grok).unwrap();
        let chat = grok.join("chat_history.jsonl");
        fs::write(
            &chat,
            [
                serde_json::json!({"type":"system","content":"You are Grok 4.7 released by xAI."})
                    .to_string(),
                serde_json::json!({"type":"user","content":"You are Wesley. Old Grok session."})
                    .to_string(),
            ]
            .join("\n"),
        )
        .unwrap();
        set_mtime(&chat, old_ms);

        let mut table = RouteTable::default();
        table.upsert(
            "wesley",
            VendorRoute::Agy {
                conversation_id: "conv-wesley".into(),
                ls_pid: None,
                ls_address: None,
                csrf_token: None,
                agentapi_exe: None,
            },
            live_at,
        );
        table.discover_routes(Some(home), At::parse("2026-10-29T04:00:00.000Z").unwrap());

        assert!(
            matches!(
                table.get("wesley").map(|e| &e.route),
                Some(VendorRoute::Agy { conversation_id, .. }) if conversation_id == "conv-wesley"
            ),
            "stale codex, muse, and grok files must not replace the newer route"
        );
    }

    #[test]
    fn inactive_grok_summary_updated_at_does_not_replace_newer_route() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let old_ms = At::parse("2026-09-16T10:40:52.000Z").unwrap().unix_millis();
        let live_at = At::parse("2026-10-29T01:42:59.000Z").unwrap();
        let summary_ms = At::parse("2026-10-29T05:00:00.000Z").unwrap().unix_millis();

        let grok = home
            .join(".grok")
            .join("sessions")
            .join("proj")
            .join("old-grok-session");
        fs::create_dir_all(&grok).unwrap();
        let chat = grok.join("chat_history.jsonl");
        fs::write(
            &chat,
            [
                serde_json::json!({"type":"system","content":"You are Grok 4.7 released by xAI."})
                    .to_string(),
                serde_json::json!({"type":"user","content":"You are Wesley. Old Grok session."})
                    .to_string(),
            ]
            .join("\n"),
        )
        .unwrap();
        let summary = grok.join("summary.json");
        fs::write(
            &summary,
            serde_json::json!({
                "info": {"id": "old-grok-session"},
                "session_summary": "Wesley registers from a rewritten summary",
                "updated_at": "2026-10-29T05:00:00.000Z"
            })
            .to_string(),
        )
        .unwrap();
        set_mtime(&summary, summary_ms);
        set_mtime(&chat, old_ms);

        let mut table = RouteTable::default();
        table.upsert(
            "wesley",
            VendorRoute::Agy {
                conversation_id: "conv-wesley".into(),
                ls_pid: None,
                ls_address: None,
                csrf_token: None,
                agentapi_exe: None,
            },
            live_at.clone(),
        );
        table.discover_routes(Some(home), At::parse("2026-10-29T06:00:00.000Z").unwrap());

        let entry = table.get("wesley").expect("existing route");
        assert!(
            matches!(
                &entry.route,
                VendorRoute::Agy { conversation_id, .. } if conversation_id == "conv-wesley"
            ),
            "inactive grok chat mtime must not lose to summary.json updated_at"
        );
        assert_eq!(entry.updated_at, live_at);
    }

    #[test]
    fn codex_index_updated_at_does_not_outstamp_older_rollout() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let old_ms = At::parse("2026-09-16T10:40:52.000Z").unwrap().unix_millis();
        let live_at = At::parse("2026-10-29T01:42:59.000Z").unwrap();
        let thread_id = "01a0a9ce-1111-7222-8333-ea40058ed587";

        let day = home
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("09")
            .join("16");
        fs::create_dir_all(&day).unwrap();
        let rollout = day.join(format!("rollout-2026-09-16T10-40-52-{thread_id}.jsonl"));
        fs::write(
            &rollout,
            [
                serde_json::json!({
                    "type": "session_meta",
                    "payload": {
                        "id": thread_id,
                        "cwd": "/tmp/old-codex"
                    }
                })
                .to_string(),
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "user",
                        "content": [{"type": "input_text", "text": "You are Wesley. Old Codex session."}]
                    }
                })
                .to_string(),
            ]
            .join("\n"),
        )
        .unwrap();
        set_mtime(&rollout, old_ms);

        let index = home.join(".codex").join("session_index.jsonl");
        fs::write(
            &index,
            serde_json::json!({
                "id": thread_id,
                "agentName": "Wesley",
                "updated_at": "2026-10-29T05:00:00.000Z",
                "cwd": "/tmp/fresh-index"
            })
            .to_string()
                + "\n",
        )
        .unwrap();
        set_mtime(
            &index,
            At::parse("2026-10-29T05:00:01.000Z").unwrap().unix_millis(),
        );

        let mut table = RouteTable::default();
        table.upsert(
            "wesley",
            VendorRoute::Agy {
                conversation_id: "conv-wesley".into(),
                ls_pid: None,
                ls_address: None,
                csrf_token: None,
                agentapi_exe: None,
            },
            live_at.clone(),
        );
        table.discover_routes(Some(home), At::parse("2026-10-29T06:00:00.000Z").unwrap());

        let entry = table.get("wesley").expect("existing route");
        assert!(
            matches!(
                &entry.route,
                VendorRoute::Agy { conversation_id, .. } if conversation_id == "conv-wesley"
            ),
            "index updated_at newer than the route must not beat an older rollout mtime"
        );
        assert_eq!(entry.updated_at, live_at);
    }

    #[test]
    fn resolve_grok_pid_persona_skips_subagent_summary() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let sid = "01a18599-c714-7700-85bb-e276825a98b2";
        let fake_pid = 4_000_000_001_u32;
        let session = home.join(".grok").join("sessions").join("proj").join(sid);
        fs::create_dir_all(&session).unwrap();
        fs::write(
            home.join(".grok").join("active_sessions.json"),
            serde_json::json!([{
                "session_id": sid,
                "pid": fake_pid,
                "cwd": "/tmp/proj"
            }])
            .to_string(),
        )
        .unwrap();
        fs::write(
            session.join("chat_history.jsonl"),
            serde_json::json!({
                "type": "system",
                "content": "You are a Grok Build subagent. Inspect the diff."
            })
            .to_string(),
        )
        .unwrap();
        fs::write(
            session.join("summary.json"),
            serde_json::json!({
                "info": {"id": sid},
                "session_summary": "Katie registers from a subagent summary",
                "updated_at": "2026-10-29T03:00:00.000Z"
            })
            .to_string(),
        )
        .unwrap();

        assert!(resolve_grok_pid_persona(home, fake_pid).is_none());
    }

    #[test]
    fn resolve_agy_exe_rejects_old_and_missing_paths() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let bin_dir = home.join(".local").join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let local = bin_dir.join("agy");
        fs::write(&local, b"#!/bin/sh\n").unwrap();
        let old = bin_dir.join("agy.1710000000.old");
        fs::write(&old, b"#!/bin/sh\n").unwrap();
        let good = bin_dir.join("agy-good");
        fs::write(&good, b"#!/bin/sh\n").unwrap();
        let missing = bin_dir.join("agy-missing");

        assert_eq!(resolve_agy_exe(Some(&old), Some(&old), Some(home)), local);
        assert_eq!(resolve_agy_exe(Some(&missing), None, Some(home)), local);
        assert_eq!(resolve_agy_exe(Some(&good), Some(&old), Some(home)), good);
        assert_eq!(
            resolve_agy_exe(Some(&old), Some(&old), None),
            PathBuf::from("agy")
        );
        assert!(!agy_exe_is_usable(&old));
        assert!(!agy_exe_is_usable(&missing));
        assert!(agy_exe_is_usable(&good));
    }

    #[test]
    fn agy_ls_address_listening_requires_an_open_port() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(agy_ls_address_listening(&format!("127.0.0.1:{port}")));
        assert!(agy_ls_address_listening(&format!("localhost:{port}")));
        // Port 1 is not an ephemeral bind target, so a parallel test cannot steal it.
        assert!(!agy_ls_address_listening("127.0.0.1:1"));
        assert!(!agy_ls_address_listening("not-a-port"));
        drop(listener);
    }

    #[test]
    fn agy_ls_port_accepts_only_addresses_that_name_this_machine() {
        for (addr, port) in [
            ("127.0.0.1:8080", 8080),
            ("localhost:8080", 8080),
            ("LOCALHOST:8080", 8080),
            ("[::1]:8080", 8080),
            (":8080", 8080),
            ("8080", 8080),
            ("  127.0.0.1:41234 ", 41234),
        ] {
            assert_eq!(agy_ls_port(addr), Some(port), "{addr}");
        }
        for addr in [
            "10.0.0.5:8080",
            "example.com:8080",
            "0.0.0.0:8080",
            "127.0.0.2:8080",
            "[fe80::1]:8080",
            "[2001:db8::1]:8080",
            "::1:8080",
            "http://localhost:8080",
            "evil.localhost:8080",
            "",
            "localhost",
            "127.0.0.1:",
            "not-a-port",
        ] {
            assert_eq!(agy_ls_port(addr), None, "{addr}");
        }
    }

    #[test]
    fn agy_ls_address_listening_ignores_a_remote_host_on_a_local_open_port() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(agy_ls_address_listening(&format!("127.0.0.1:{port}")));
        assert!(!agy_ls_address_listening(&format!("10.0.0.5:{port}")));
        assert!(!agy_ls_address_listening(&format!("example.com:{port}")));
        drop(listener);
    }

    #[test]
    fn note_agy_proc_env_skips_dead_ports_and_old_binaries() {
        let tmp = TempDir::new().unwrap();
        let live = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let live_port = live.local_addr().unwrap().port();
        // Port 1 is closed and is not recycled by other tests that bind port 0.
        let dead_port = 1u16;

        let old = tmp.path().join("agy.9.old");
        fs::write(&old, b"old").unwrap();
        let good = tmp.path().join("agy-bin");
        fs::write(&good, b"good").unwrap();

        let mut csrf = None;
        let mut addr = None;
        let mut exe = None;
        let dead_env = format!(
            "ANTIGRAVITY_CSRF_TOKEN=csrf-1\0ANTIGRAVITY_LS_ADDRESS=127.0.0.1:{dead_port}\0ANTIGRAVITY_AGENTAPI_EXE={}\0",
            old.display()
        );
        note_agy_proc_env(dead_env.as_bytes(), &mut csrf, &mut addr, &mut exe);
        assert_eq!(csrf.as_deref(), Some("csrf-1"));
        assert!(addr.is_none());
        assert!(exe.is_none());

        let live_env = format!(
            "ANTIGRAVITY_LS_ADDRESS=127.0.0.1:{live_port}\0ANTIGRAVITY_AGENTAPI_EXE={}\0",
            good.display()
        );
        note_agy_proc_env(live_env.as_bytes(), &mut csrf, &mut addr, &mut exe);
        assert_eq!(
            addr.as_deref(),
            Some(format!("127.0.0.1:{live_port}").as_str())
        );
        assert_eq!(exe.as_deref(), Some(good.as_path()));
    }

    #[test]
    fn discover_agy_routes_replaces_old_or_missing_agentapi_exe() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let agy_dir = home.join(".gemini").join("antigravity-cli");
        fs::create_dir_all(agy_dir.join("brain")).unwrap();
        let bin_dir = home.join(".local").join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let local = bin_dir.join("agy");
        fs::write(&local, b"#!/bin/sh\n").unwrap();
        let old = bin_dir.join("agy.42.old");
        fs::write(&old, b"#!/bin/sh\n").unwrap();
        let missing = bin_dir.join("agy-missing");
        let good = bin_dir.join("agy-good");
        fs::write(&good, b"#!/bin/sh\n").unwrap();

        let at = At::parse("2026-10-29T12:00:00.000Z").unwrap();
        let mut stale = RouteTable::default();
        stale.upsert(
            "oldie",
            VendorRoute::Agy {
                conversation_id: "conv-old".into(),
                ls_pid: None,
                ls_address: None,
                csrf_token: None,
                agentapi_exe: Some(old),
            },
            at.clone(),
        );
        stale.upsert(
            "gone",
            VendorRoute::Agy {
                conversation_id: "conv-gone".into(),
                ls_pid: None,
                ls_address: None,
                csrf_token: None,
                agentapi_exe: Some(missing),
            },
            at.clone(),
        );
        stale.discover_routes(Some(home), At::parse("2026-10-29T12:05:00.000Z").unwrap());
        let exe_of = |table: &RouteTable, persona: &str| match &table.get(persona).unwrap().route {
            VendorRoute::Agy { agentapi_exe, .. } => agentapi_exe.clone(),
            other => panic!("expected agy route, got {other:?}"),
        };
        assert_eq!(exe_of(&stale, "oldie"), Some(local.clone()));
        assert_eq!(exe_of(&stale, "gone"), Some(local));

        let mut kept = RouteTable::default();
        kept.upsert(
            "kept",
            VendorRoute::Agy {
                conversation_id: "conv-kept".into(),
                ls_pid: None,
                ls_address: None,
                csrf_token: None,
                agentapi_exe: Some(good.clone()),
            },
            at,
        );
        kept.discover_routes(Some(home), At::parse("2026-10-29T12:05:00.000Z").unwrap());
        assert_eq!(exe_of(&kept, "kept"), Some(good));
    }

    #[test]
    fn discover_agy_routes_repairs_old_exe_when_brain_missing() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        assert!(
            !home
                .join(".gemini")
                .join("antigravity-cli")
                .join("brain")
                .exists()
        );
        let bin_dir = home.join(".local").join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let local = bin_dir.join("agy");
        fs::write(&local, b"#!/bin/sh\n").unwrap();
        let old = bin_dir.join("agy.1710000000.old");
        fs::write(&old, b"#!/bin/sh\n").unwrap();

        let mut table = RouteTable::default();
        table.upsert(
            "felicia",
            VendorRoute::Agy {
                conversation_id: "conv-felicia".into(),
                ls_pid: None,
                ls_address: None,
                csrf_token: None,
                agentapi_exe: Some(old),
            },
            At::parse("2026-10-29T12:00:00.000Z").unwrap(),
        );
        table.discover_routes(Some(home), At::parse("2026-10-29T12:05:00.000Z").unwrap());

        match &table.get("felicia").unwrap().route {
            VendorRoute::Agy { agentapi_exe, .. } => {
                assert_eq!(agentapi_exe.as_deref(), Some(local.as_path()));
            }
            other => panic!("expected agy route, got {other:?}"),
        }
    }

    #[test]
    fn discover_agy_routes_ignores_dead_cli_log_port() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let live_port = listener.local_addr().unwrap().port();
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let agy_dir = home.join(".gemini").join("antigravity-cli");
        fs::create_dir_all(agy_dir.join("brain")).unwrap();
        // Reverse scan sees port 1 first. The HTTPS line stays skipped. The
        // earlier HTTP line is the bound listener, so Step 1 wins over /proc.
        fs::write(
            agy_dir.join("cli.log"),
            format!(
                "Language server listening on random port at {live_port} for HTTP\n\
                 Language server listening on random port at {live_port} for HTTPS\n\
                 Language server listening on random port at 1 for HTTP\n"
            ),
        )
        .unwrap();

        let at = At::parse("2026-10-29T18:00:00.000Z").unwrap();
        let mut table = RouteTable::default();
        table.upsert(
            "felicia",
            VendorRoute::Agy {
                conversation_id: "conv-felicia".into(),
                ls_pid: None,
                ls_address: Some("localhost:35867".into()),
                csrf_token: None,
                agentapi_exe: None,
            },
            at.clone(),
        );
        table.discover_agy_routes(home, true, &at);

        match &table.get("felicia").unwrap().route {
            VendorRoute::Agy { ls_address, .. } => {
                assert_eq!(
                    ls_address.as_deref(),
                    Some(format!("127.0.0.1:{live_port}").as_str())
                );
            }
            other => panic!("expected agy route, got {other:?}"),
        }
        drop(listener);
    }

    #[test]
    fn saved_route_table_is_private_round_trips_and_leaves_no_temp_files() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("routes.json");
        let mut table = RouteTable::default();
        table.upsert(
            "Kayla",
            VendorRoute::Agy {
                conversation_id: "c1".into(),
                ls_pid: None,
                ls_address: Some("127.0.0.1:1".into()),
                csrf_token: Some("super-secret-csrf".into()),
                agentapi_exe: None,
            },
            At::from_unix_millis(5),
        );
        table.save(Some(&path)).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the table holds CSRF tokens");
        let back = RouteTable::load(Some(&path));
        assert!(back.get("kayla").is_some());
        let leftovers: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "routes.json")
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
    }

    /// Audit I-1: every save used the same `routes.json.tmp`, so concurrent savers truncated each
    /// other's file and could rename a half-written one into place.
    #[test]
    fn concurrent_saves_never_leave_a_corrupt_route_table() {
        let tmp = TempDir::new().unwrap();
        let path = Arc::new(tmp.path().join("routes.json"));
        let handles: Vec<_> = (0..8)
            .map(|t| {
                let path = Arc::clone(&path);
                std::thread::spawn(move || {
                    let mut table = RouteTable::default();
                    for i in 0..20 {
                        table.upsert(
                            &format!("agent-{t}-{i}"),
                            VendorRoute::Codex {
                                thread_id: format!("thread-{t}-{i}"),
                                cwd: None,
                            },
                            At::from_unix_millis(1 + i),
                        );
                    }
                    for _ in 0..40 {
                        table.save(Some(&path)).unwrap();
                        let raw = fs::read_to_string(&*path).unwrap();
                        serde_json::from_str::<RouteTable>(&raw)
                            .expect("a reader saw a torn route table");
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn route_table_load_survives_garbage_and_missing_files() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("routes.json");
        assert!(RouteTable::load(Some(&path)).entries.is_empty());
        for junk in [
            "",
            "{",
            "[]",
            "null",
            "{\"entries\": 5}",
            "\u{feff}{}",
            "{\"x\": {}}",
        ] {
            fs::write(&path, junk).unwrap();
            assert!(
                RouteTable::load(Some(&path)).entries.is_empty(),
                "{junk:?} produced routes"
            );
        }
    }

    #[test]
    fn upsert_ignores_names_that_are_not_valid_personas() {
        let mut table = RouteTable::default();
        let route = || VendorRoute::Codex {
            thread_id: "t".into(),
            cwd: None,
        };
        for bad in [
            "",
            "   ",
            "1abc",
            "has space",
            "a/b",
            "../etc",
            "x".repeat(65).as_str(),
        ] {
            table.upsert(bad, route(), At::from_unix_millis(1));
        }
        assert!(table.entries.is_empty());
        table.upsert("  Mixed_Case-9  ", route(), At::from_unix_millis(1));
        table.upsert(&"a".repeat(64), route(), At::from_unix_millis(1));
        assert!(table.get("mixed_case-9").is_some());
        assert!(table.get("MIXED_CASE-9").is_some());
        assert_eq!(table.entries.len(), 2);
        assert!(table.get("nobody").is_none());
        assert!(table.get("").is_none());
    }

    #[test]
    fn merge_newer_keeps_the_newer_entry_and_ties_keep_the_existing_one() {
        let codex = |t: &str| VendorRoute::Codex {
            thread_id: t.into(),
            cwd: None,
        };
        let mut a = RouteTable::default();
        a.upsert("old", codex("a-old"), At::from_unix_millis(10));
        a.upsert("newer-in-a", codex("a-new"), At::from_unix_millis(50));
        a.upsert("tie", codex("a-tie"), At::from_unix_millis(30));
        let mut b = RouteTable::default();
        b.upsert("old", codex("b-old"), At::from_unix_millis(20));
        b.upsert("newer-in-a", codex("b-new"), At::from_unix_millis(40));
        b.upsert("tie", codex("b-tie"), At::from_unix_millis(30));
        b.upsert("only-b", codex("b-only"), At::from_unix_millis(1));
        a.merge_newer(b);
        let thread = |t: &RouteTable, k: &str| match &t.get(k).unwrap().route {
            VendorRoute::Codex { thread_id, .. } => thread_id.clone(),
            other => panic!("{other:?}"),
        };
        assert_eq!(thread(&a, "old"), "b-old");
        assert_eq!(thread(&a, "newer-in-a"), "a-new");
        assert_eq!(thread(&a, "tie"), "a-tie");
        assert_eq!(thread(&a, "only-b"), "b-only");
    }

    #[test]
    fn merge_dispatch_result_carries_pruned_routes_back_unless_changed_meanwhile() {
        let codex = |t: &str| VendorRoute::Codex {
            thread_id: t.into(),
            cwd: None,
        };
        let mut live = RouteTable::default();
        live.upsert("dead", codex("d"), At::from_unix_millis(10));
        live.upsert("changed", codex("c1"), At::from_unix_millis(10));
        live.upsert("kept", codex("k"), At::from_unix_millis(10));
        let base = live.clone();
        // The dispatch pruned "dead" and "changed" and discovered "found".
        let mut working = base.clone();
        working.entries.remove("dead");
        working.entries.remove("changed");
        working.upsert("found", codex("f"), At::from_unix_millis(20));
        // Meanwhile a RegisterRoute rewrote "changed" in the live table.
        live.upsert("changed", codex("c2"), At::from_unix_millis(15));
        live.merge_dispatch_result(&base, working);
        assert!(live.get("dead").is_none(), "pruned route must be removed");
        assert!(live.get("kept").is_some());
        assert!(live.get("found").is_some());
        assert!(
            live.get("changed").is_some(),
            "a route re-registered during the dispatch survives"
        );
    }

    #[test]
    fn a_newer_muse_route_keeps_the_tmux_target_the_older_one_knew() {
        let muse = |tmux: Option<&str>| VendorRoute::Muse {
            session_id: "s".into(),
            cwd: None,
            tmux_target: tmux.map(str::to_string),
            tmux_pane: tmux.map(|t| format!("{t}.%1")),
        };
        let mut table = RouteTable::default();
        table.upsert("muse-a", muse(Some("sess:1")), At::from_unix_millis(10));
        table.upsert("muse-a", muse(None), At::from_unix_millis(20));
        match &table.get("muse-a").unwrap().route {
            VendorRoute::Muse {
                tmux_target,
                tmux_pane,
                ..
            } => {
                assert_eq!(tmux_target.as_deref(), Some("sess:1"));
                assert_eq!(tmux_pane.as_deref(), Some("sess:1.%1"));
            }
            other => panic!("{other:?}"),
        }
        // Different vendors never share fields.
        table.upsert(
            "muse-a",
            VendorRoute::Codex {
                thread_id: "t".into(),
                cwd: None,
            },
            At::from_unix_millis(30),
        );
        assert!(matches!(
            table.get("muse-a").unwrap().route,
            VendorRoute::Codex { .. }
        ));
    }

    #[test]
    fn claude_key_lookup_matches_the_exact_pid_prefix_and_is_deterministic() {
        let tmp = TempDir::new().unwrap();
        let sock = tmp.path().join("claude.sock");
        // `12.` must not match pid 123's file, and the lexicographically first match wins.
        for name in [
            "123.aaa.key",
            "12.zzz.key",
            "12.bbb.key",
            "12.bbb.txt",
            "x12.q.key",
        ] {
            fs::write(tmp.path().join(name), "{}").unwrap();
        }
        for _ in 0..5 {
            let found = resolve_claude_key_path(tmp.path(), 12, &sock).unwrap();
            assert_eq!(found.file_name().unwrap(), "12.bbb.key");
        }
        assert!(resolve_claude_key_path(tmp.path(), 1, &sock).is_none());
        assert!(resolve_claude_key_path(&tmp.path().join("missing"), 12, &sock).is_none());
    }

    #[test]
    fn normalize_claude_session_name_rejects_the_artificial_humanity_prefix() {
        assert_eq!(
            normalize_claude_session_name(" Felicia "),
            Some("felicia".into())
        );
        assert_eq!(normalize_claude_session_name("artificial-humanity-x"), None);
        assert_eq!(normalize_claude_session_name("Artificial-Humanity-X"), None);
        assert_eq!(normalize_claude_session_name(""), None);
    }
}
