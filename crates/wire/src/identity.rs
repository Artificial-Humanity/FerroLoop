//! Sender persona resolution, live vendor route table, and passive route discovery (spec §3.1).

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

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
    },
    Grok {
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<PathBuf>,
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

impl RouteTable {
    pub fn load(custom_path: Option<&Path>) -> Self {
        let primary = custom_path
            .map(Path::to_path_buf)
            .unwrap_or_else(default_routes_path);
        let mut table = Self::load_file(&primary).unwrap_or_default();
        if custom_path.is_none()
            && std::env::var("FL_WIRE_DISABLE_HOST_DISCOVERY").as_deref() != Ok("1")
            && let Some(legacy) = legacy_routes_path()
            && let Some(legacy_table) = Self::load_file(&legacy)
        {
            for (k, v) in legacy_table.entries {
                table.entries.entry(k).or_insert(v);
            }
        }
        table
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
                    entries.insert(
                        norm.clone(),
                        RouteEntry {
                            persona: norm,
                            route,
                            updated_at: At::from_unix_millis(0),
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
        let target = custom_path
            .map(Path::to_path_buf)
            .unwrap_or_else(default_routes_path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = target.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        {
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = f.set_permissions(fs::Permissions::from_mode(0o600));
            }
            f.write_all(&bytes)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &target)?;
        Ok(())
    }

    pub fn upsert(&mut self, persona: &str, route: VendorRoute, at: At) {
        if let Some(norm) = normalize_persona(persona) {
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
        self.discover_grok_routes(&home, &at);
    }

    fn discover_claude_routes(&mut self, home: &Path, check_proc: bool, at: &At) {
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
            let Some(name) = val.get("name").and_then(|v| v.as_str()) else {
                continue;
            };
            let Some(persona) = normalize_claude_session_name(name) else {
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
            return;
        }
        let mut shared_ls_addr: Option<String> = None;
        let mut shared_csrf: Option<String> = None;
        let mut shared_exe: Option<PathBuf> = None;

        for entry in self.entries.values() {
            if let VendorRoute::Agy {
                ls_address,
                csrf_token,
                agentapi_exe,
                ..
            } = &entry.route
            {
                if shared_ls_addr.is_none() {
                    shared_ls_addr = ls_address.clone();
                }
                if shared_csrf.is_none() {
                    shared_csrf = csrf_token.clone();
                }
                if shared_exe.is_none() {
                    shared_exe = agentapi_exe.clone();
                }
            }
        }

        if shared_ls_addr.is_none()
            && let Ok(log_text) = fs::read_to_string(agy_dir.join("cli.log"))
        {
            for line in log_text.lines().rev() {
                if let Some(idx) = line.find("Language server listening on random port at ") {
                    let rest = &line[idx + "Language server listening on random port at ".len()..];
                    let port: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
                    if !port.is_empty() {
                        shared_ls_addr = Some(format!("127.0.0.1:{port}"));
                        break;
                    }
                }
            }
        }

        if check_proc
            && (shared_csrf.is_none() || shared_ls_addr.is_none())
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
                for kv in env_bytes.split(|&b| b == 0) {
                    if let Ok(kv_str) = std::str::from_utf8(kv) {
                        if shared_csrf.is_none()
                            && let Some(val) = kv_str.strip_prefix("ANTIGRAVITY_CSRF_TOKEN=")
                            && !val.is_empty()
                        {
                            shared_csrf = Some(val.to_string());
                        } else if shared_ls_addr.is_none()
                            && let Some(val) = kv_str.strip_prefix("ANTIGRAVITY_LS_ADDRESS=")
                            && !val.is_empty()
                        {
                            shared_ls_addr = Some(val.to_string());
                        } else if shared_exe.is_none()
                            && let Some(val) = kv_str.strip_prefix("ANTIGRAVITY_AGENTAPI_EXE=")
                            && !val.is_empty()
                        {
                            shared_exe = Some(PathBuf::from(val));
                        }
                    }
                }
                if shared_csrf.is_some() && shared_ls_addr.is_some() {
                    break;
                }
            }
        }

        // Backfill shared ls_address / csrf_token onto existing Agy routes if missing
        for entry in self.entries.values_mut() {
            if let VendorRoute::Agy {
                ls_address,
                csrf_token,
                agentapi_exe,
                ..
            } = &mut entry.route
            {
                if ls_address.is_none() {
                    *ls_address = shared_ls_addr.clone();
                }
                if csrf_token.is_none() {
                    *csrf_token = shared_csrf.clone();
                }
                if agentapi_exe.is_none() {
                    *agentapi_exe = shared_exe.clone();
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
            {
                self.upsert(
                    &persona,
                    VendorRoute::Agy {
                        conversation_id: conv_id.to_string(),
                        ls_pid: None,
                        ls_address: shared_ls_addr.clone(),
                        csrf_token: shared_csrf.clone(),
                        agentapi_exe: shared_exe.clone(),
                    },
                    at.clone(),
                );
            }
        }
    }

    fn discover_codex_routes(&mut self, home: &Path, at: &At) {
        let index_path = home.join(".codex").join("session_index.jsonl");
        let Ok(text) = fs::read_to_string(&index_path) else {
            return;
        };
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
                "exit" | "quit" | "help" | "clear" | "status" | "compact" | "unknown" | "default"
            ) {
                continue;
            }
            let cwd = val.get("cwd").and_then(|v| v.as_str()).map(PathBuf::from);
            self.upsert(
                &persona,
                VendorRoute::Codex {
                    thread_id: thread_id.to_string(),
                    cwd,
                },
                at.clone(),
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
        if !has_home_override
            && let Some(xdg) = std::env::var_os("XDG_DATA_HOME")
        {
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
                self.upsert(
                    &persona,
                    VendorRoute::Muse { session_id, cwd },
                    at.clone(),
                );
            }
        }
    }

    fn discover_grok_routes(&mut self, home: &Path, at: &At) {
        let sessions_dir = home.join(".grok").join("sessions");
        let Ok(read_dir) = fs::read_dir(&sessions_dir) else {
            return;
        };
        for entry in read_dir.flatten() {
            let path = entry.path();
            let ext = path.extension().and_then(|s| s.to_str());
            if !matches!(ext, Some("json") | Some("jsonl")) {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let Some(persona) = scan_transcript_for_persona(&path, 50) else {
                continue;
            };
            let (session_id, cwd) = extract_session_id_and_cwd(&path, stem);
            self.upsert(
                &persona,
                VendorRoute::Grok { session_id, cwd },
                at.clone(),
            );
        }
    }
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
            let thread_id = item.get("thread_id")?.as_str()?.to_string();
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
            Some(VendorRoute::Muse { session_id, cwd })
        }
        "grok" => {
            let session_id = item
                .get("session_id")
                .or_else(|| item.get("sessionId"))?
                .as_str()?
                .to_string();
            let cwd = item.get("cwd").and_then(|v| v.as_str()).map(PathBuf::from);
            Some(VendorRoute::Grok { session_id, cwd })
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
    let prefix = format!("{pid}.");
    if let Ok(read_dir) = fs::read_dir(sessions_dir) {
        for entry in read_dir.flatten() {
            let p = entry.path();
            if let Some(name) = p.file_name().and_then(|s| s.to_str())
                && name.starts_with(&prefix)
                && name.ends_with(".key")
            {
                return Some(p);
            }
        }
    }
    None
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

pub fn scan_transcript_for_persona(path: &Path, max_lines: usize) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    for line in text.lines().take(max_lines) {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
            for field in ["agentName", "customTitle", "persona"] {
                if let Some(s) = val.get(field).and_then(|v| v.as_str())
                    && let Some(norm) = normalize_claude_session_name(s)
                {
                    return Some(norm);
                }
            }
        }
        let lower = line.to_ascii_lowercase();
        for marker in ["take the name of ", "you will be ", "take up residence in "] {
            if let Some(idx) = lower.find(marker) {
                let after = &line[idx + marker.len()..];
                let token: String = after
                    .trim_start_matches(['"', '\'', '`', ' '])
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                    .collect();
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
        let key_path = std::env::var_os("HOME")
            .map(PathBuf::from)
            .and_then(|h| resolve_claude_key_path(&h.join(".claude").join("sessions"), pid, &socket_path));
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
        });
    }
    if let Ok(session_id) = std::env::var("GROK_SESSION_ID")
        && !session_id.trim().is_empty()
    {
        return Some(VendorRoute::Grok {
            session_id,
            cwd: std::env::current_dir().ok(),
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
    if let Some(exp) = explicit
        && let Some(norm) = normalize_persona(exp)
    {
        return norm;
    }

    for var in ["AGENT_PERSONA", "AGENT_NAME"] {
        if let Ok(val) = std::env::var(var)
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

    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from)
        && let Some(sock) = std::env::var_os("CLAUDE_CODE_MESSAGING_SOCKET")
    {
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

    let routes = RouteTable::load(custom_routes_path);
    if let Ok(claude_sid) = std::env::var("CLAUDE_SESSION_ID")
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
    if let Ok(agy_cid) = std::env::var("ANTIGRAVITY_CONVERSATION_ID")
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
    if let Ok(codex_tid) = std::env::var("CODEX_THREAD_ID")
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
    if let Ok(muse_sid) = std::env::var("MUSE_SESSION_ID")
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
    if let Ok(grok_sid) = std::env::var("GROK_SESSION_ID")
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
            && let Some(name) = val.get("name").and_then(|v| v.as_str())
            && let Some(norm) = normalize_claude_session_name(name)
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
    use tempfile::TempDir;

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

        // 1. Explicit flag wins
        assert_eq!(
            resolve_persona(Some("Jarvis"), None, &proj_dir, Some(&store), None),
            "jarvis"
        );

        // 2. Transcript scan works when explicit is None
        let transcript = tmp.path().join("transcript.jsonl");
        fs::write(
            &transcript,
            "{\"type\":\"meta\",\"agentName\":\"Kayla\"}\n",
        )
        .unwrap();
        assert_eq!(
            resolve_persona(
                None,
                Some(&transcript),
                &proj_dir,
                Some(&store),
                Some(&tmp.path().join("routes.json")),
            ),
            "kayla"
        );

        // 3. CWD project lead fallback works when neither explicit nor transcript matches
        assert_eq!(
            resolve_persona(
                None,
                None,
                &proj_dir,
                Some(&store),
                Some(&tmp.path().join("routes.json")),
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
            "{\"customTitle\":\"Kacey\"}\n",
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
        let muse_dir = home.join(".local").join("share").join("muse").join("sessions");
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
            Some(VendorRoute::Claude { pid: 4242, key_path: Some(_), .. })
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
}
