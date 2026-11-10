//! Sender persona resolution, live vendor route table, and passive route discovery (spec §3.1).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fl_core::agent::{AgentRole, AgentStatus};
use fl_core::at::At;
use fl_core::state::validate_slug;
use fl_core::store::AgentRegistry;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::dispatch_diagnostics::{
    MAX_REGISTRATION_DIAGNOSTICS, RegistrationConflictReason, RegistrationDiagnosticOutcome,
    RouteAdapter, RouteRegistrationDiagnostic, sanitize_generation_token,
};

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
    HostCli {
        vendor: String,
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

pub fn canonical_vendor_id(raw: &str) -> &str {
    let trimmed = raw.trim();
    if trimmed.eq_ignore_ascii_case("agy") || trimmed.eq_ignore_ascii_case("antigravity") {
        "antigravity"
    } else if trimmed.eq_ignore_ascii_case("claude") {
        "claude"
    } else if trimmed.eq_ignore_ascii_case("codex") {
        "codex"
    } else if trimmed.eq_ignore_ascii_case("muse") {
        "muse"
    } else if trimmed.eq_ignore_ascii_case("grok") {
        "grok"
    } else if trimmed.eq_ignore_ascii_case("lemonade") {
        "lemonade"
    } else if trimmed.eq_ignore_ascii_case("ollama") {
        "ollama"
    } else if trimmed.eq_ignore_ascii_case("host_cli") {
        "host_cli"
    } else if trimmed.eq_ignore_ascii_case("webhook") {
        "webhook"
    } else {
        trimmed
    }
}

fn is_known_hosted_vendor(vendor_lower: &str) -> bool {
    PERSONA_DERIVED_VENDORS.contains(&vendor_lower)
        || fl_core::lookup_vendor(vendor_lower).is_some()
}

/// Parse a `FL_HOSTED_ID` or hosted session identifier into `(canonical_vendor, normalized_persona)`.
///
/// Accepts `<vendor>:<persona>` (`"antigravity:feira"`, `"agy:feira"`, `"codex:fiona"`) as well as
/// `<Persona>.<vendor>` (`"Feira.antigravity"`) and `"muse.<persona>"` (`"muse.ada"`). Fails closed
/// (`Err`) on empty or malformed values.
pub fn parse_hosted_identity(raw: &str) -> Result<(String, String), &'static str> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("hosted identity is empty");
    }
    if trimmed.contains(':') && trimmed.contains('.') {
        return Err("hosted identity cannot mix ':' and '.' separators");
    }
    if let Some((vendor_raw, persona_raw)) = trimmed.split_once(':') {
        if persona_raw.contains(':') {
            return Err("malformed hosted identity");
        }
        let vendor_lower = vendor_raw.trim().to_ascii_lowercase();
        if vendor_lower.is_empty() || !is_known_hosted_vendor(&vendor_lower) {
            return Err("unknown vendor in hosted identity");
        }
        let persona = normalize_persona(persona_raw).ok_or("invalid persona in hosted identity")?;
        return Ok((canonical_vendor_id(&vendor_lower).to_string(), persona));
    }
    if let Some((first_raw, second_raw)) = trimmed.split_once('.') {
        if second_raw.contains('.') {
            return Err("malformed hosted identity");
        }
        if first_raw.trim().eq_ignore_ascii_case("muse") {
            let persona =
                normalize_persona(second_raw).ok_or("invalid persona in hosted identity")?;
            if persona == "muse" {
                return Err("invalid Muse persona in hosted identity");
            }
            return Ok(("muse".to_string(), persona));
        }
        let vendor_lower = second_raw.trim().to_ascii_lowercase();
        if vendor_lower.is_empty() || !is_known_hosted_vendor(&vendor_lower) {
            return Err("unknown vendor in hosted identity");
        }
        let persona = normalize_persona(first_raw).ok_or("invalid persona in hosted identity")?;
        return Ok((canonical_vendor_id(&vendor_lower).to_string(), persona));
    }
    Err("malformed hosted identity")
}

impl VendorRoute {
    pub fn canonical_vendor(&self) -> &str {
        match self {
            Self::Claude { .. } => "claude",
            Self::Agy { .. } => "antigravity",
            Self::Codex { .. } => "codex",
            Self::Muse { .. } => "muse",
            Self::Grok { .. } => "grok",
            Self::HostCli { vendor, .. } => canonical_vendor_id(vendor),
            Self::Webhook { .. } => "webhook",
        }
    }

    pub fn session_identity(&self) -> Option<(&'static str, &str)> {
        let (kind, raw_id) = match self {
            Self::Claude { session_id, .. } => ("claude", session_id.as_str()),
            Self::Agy {
                conversation_id, ..
            } => ("agy", conversation_id.as_str()),
            Self::Codex { thread_id, .. } => ("codex", thread_id.as_str()),
            Self::Muse { session_id, .. } => ("muse", session_id.as_str()),
            Self::Grok { session_id, .. } => ("grok", session_id.as_str()),
            Self::HostCli { session_id, .. } => ("host_cli", session_id.as_str()),
            Self::Webhook { .. } => return None,
        };
        let id = raw_id.trim();
        (!id.is_empty()).then_some((kind, id))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteEntry {
    pub persona: String,
    pub route: VendorRoute,
    pub updated_at: At,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_at: Option<At>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub explicit: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hosted_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hydration_generation: Option<String>,
}

impl RouteEntry {
    /// Nonzero source identity-evidence timestamp, or `None` when unknown (`None` or epoch `0`).
    pub fn identity_evidence_at(&self) -> Option<&At> {
        self.identity_at.as_ref().filter(|at| at.unix_millis() > 0)
    }
}

fn passive_identity_beats(challenger: &RouteEntry, incumbent: &RouteEntry) -> bool {
    challenger.identity_evidence_at() > incumbent.identity_evidence_at()
}

fn incoming_beats_incumbent(
    challenger: &RouteEntry,
    incumbent: &RouteEntry,
    same_persona: bool,
) -> bool {
    if challenger.explicit || incumbent.explicit {
        challenger.updated_at.unix_millis() > incumbent.updated_at.unix_millis()
    } else if same_persona
        && challenger.route.session_identity().is_some()
        && challenger.route.session_identity() == incumbent.route.session_identity()
    {
        (
            challenger.identity_evidence_at().is_some(),
            challenger.updated_at.unix_millis(),
        ) > (
            incumbent.identity_evidence_at().is_some(),
            incumbent.updated_at.unix_millis(),
        )
    } else if challenger.route.session_identity().is_none()
        && incumbent.route.session_identity().is_none()
    {
        challenger.updated_at.unix_millis() > incumbent.updated_at.unix_millis()
    } else {
        passive_identity_beats(challenger, incumbent)
    }
}

fn is_false(value: &bool) -> bool {
    !value
}

#[cfg(test)]
thread_local! {
    /// [`RouteTable::discover_routes`] calls made on this thread.
    pub(crate) static DISCOVERIES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// In-memory result for the latest passive Agy source check remembered for one persona.
/// `None` rejects the exact `entry`; `Some(file_at)` records its validated transcript age
/// so snapshot merges normalize pre-revalidation copies without clobbering newer reloads.
#[derive(Debug, Clone)]
struct PassiveAgyRevalidation {
    entry: RouteEntry,
    validated_identity_at: Option<At>,
    /// Orders only in-process validation results, never competing route identities.
    observed_at: Instant,
}

impl PassiveAgyRevalidation {
    fn normalize_or_reject(&self, entry: &mut RouteEntry) -> bool {
        if self.entry != *entry {
            return true;
        }
        let Some(file_at) = &self.validated_identity_at else {
            return false;
        };
        entry.identity_at = Some(file_at.clone());
        true
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RouteTable {
    pub entries: BTreeMap<String, RouteEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub registration_diagnostics: Vec<RouteRegistrationDiagnostic>,
    /// When [`RouteTable::discover_routes`] last scanned this host; `None` until it has. Kept in
    /// memory only (never in the routes file), so a fresh process starts due for a scan.
    #[serde(skip)]
    last_discovery: Option<Instant>,
    /// Latest exact passive Agy revalidation result per persona. Kept only in memory so a scan's
    /// invalidation or identity-age normalization survives snapshot reconciliation.
    #[serde(skip)]
    passive_agy_revalidation: BTreeMap<String, PassiveAgyRevalidation>,
}

#[derive(Debug, Clone)]
pub struct RegisterRouteCheckArgs {
    pub persona: String,
    pub route: VendorRoute,
    pub explicit: bool,
    pub replace: bool,
    pub hosted_id: Option<String>,
    pub hydration_generation: Option<String>,
    pub hosted_session_verified: bool,
    pub allow_host_probes: bool,
    pub at: At,
}

#[derive(Debug, Clone)]
pub struct RegisterRouteRefusal {
    pub message: String,
    pub diagnostic: Option<Box<RouteRegistrationDiagnostic>>,
}

/// Sidecar next to the route table. Rebuilt from hosted records that are still
/// `decommissioned`, so a re-spin of the same session stops suppressing it.
pub const DECOMMISSIONED_SESSIONS_FILE: &str = "decommissioned-sessions.json";

/// Where discovery reads ids of decommissioned sessions. A home override uses
/// `<home>/.cache/agent-bus/`; otherwise the file sits beside [`default_routes_path`].
pub fn decommissioned_sessions_path(home_override: Option<&Path>) -> PathBuf {
    match home_override {
        Some(home) => home
            .join(".cache")
            .join("agent-bus")
            .join(DECOMMISSIONED_SESSIONS_FILE),
        None => std::env::var_os("FL_WIRE_ROUTES")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| Some(default_routes_path()))
            .and_then(|p| p.parent().map(|dir| dir.join(DECOMMISSIONED_SESSIONS_FILE)))
            .unwrap_or_else(|| PathBuf::from("/tmp").join(DECOMMISSIONED_SESSIONS_FILE)),
    }
}

fn persistent_decommissioned_sessions_path() -> Option<PathBuf> {
    if std::env::var_os("FL_WIRE_ROUTES")
        .map(PathBuf::from)
        .is_some_and(|p| p.is_absolute())
        || std::env::var("FL_WIRE_DISABLE_HOST_DISCOVERY").as_deref() == Ok("1")
    {
        return None;
    }
    legacy_routes_path().and_then(|p| p.parent().map(|dir| dir.join(DECOMMISSIONED_SESSIONS_FILE)))
}

fn write_decommissioned_session_bytes(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    fs::write(&tmp, bytes)?;
    if let Err(err) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(err);
    }
    Ok(())
}

fn read_decommissioned_sessions_value_from(
    primary: &Path,
    fallback: Option<&Path>,
) -> Option<serde_json::Value> {
    if let Ok(raw) = fs::read_to_string(primary)
        && let Ok(val) = serde_json::from_str::<serde_json::Value>(&raw)
    {
        return Some(val);
    }
    if let Some(fallback) = fallback
        && let Ok(raw) = fs::read_to_string(fallback)
        && let Ok(val) = serde_json::from_str::<serde_json::Value>(&raw)
    {
        return Some(val);
    }
    None
}

fn read_decommissioned_sessions_value(home_override: Option<&Path>) -> Option<serde_json::Value> {
    let primary = decommissioned_sessions_path(home_override);
    let fallback = home_override
        .is_none()
        .then(persistent_decommissioned_sessions_path)
        .flatten();
    read_decommissioned_sessions_value_from(&primary, fallback.as_deref())
}

/// Ids in the decommission sidecar. An empty or unreadable file is an empty set.
/// Empty strings are ignored.
pub fn load_decommissioned_session_ids(home_override: Option<&Path>) -> BTreeSet<String> {
    let Some(value) = read_decommissioned_sessions_value(home_override) else {
        return BTreeSet::new();
    };
    value
        .get("session_ids")
        .and_then(|ids| ids.as_array())
        .into_iter()
        .flatten()
        .filter_map(|id| id.as_str())
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .collect()
}

/// Lowercase persona names marked retired in the decommission sidecar.
pub fn load_retired_personas(home_override: Option<&Path>) -> BTreeSet<String> {
    let Some(value) = read_decommissioned_sessions_value(home_override) else {
        return BTreeSet::new();
    };
    value
        .get("retired_personas")
        .and_then(|names| names.as_array())
        .into_iter()
        .flatten()
        .filter_map(|name| name.as_str())
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| name.to_ascii_lowercase())
        .collect()
}

/// Replace the decommission sidecar with `ids` (preserving any existing `retired_personas`).
/// Callers rebuild it from records that are still decommissioned rather than appending.
pub fn save_decommissioned_session_ids(
    home_override: Option<&Path>,
    ids: &BTreeSet<String>,
) -> std::io::Result<()> {
    let retired = load_retired_personas(home_override);
    save_decommissioned_tombstones(home_override, ids, &retired)
}

/// Replace the decommission sidecar with `ids` and `retired_personas`.
pub fn save_decommissioned_tombstones(
    home_override: Option<&Path>,
    ids: &BTreeSet<String>,
    retired_personas: &BTreeSet<String>,
) -> std::io::Result<()> {
    let path = decommissioned_sessions_path(home_override);
    let retired_lower: BTreeSet<String> = retired_personas
        .iter()
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    let payload = if retired_lower.is_empty() {
        serde_json::json!({ "session_ids": ids })
    } else {
        serde_json::json!({
            "session_ids": ids,
            "retired_personas": retired_lower,
        })
    };
    let bytes = serde_json::to_vec_pretty(&payload).map_err(std::io::Error::other)?;
    write_decommissioned_session_bytes(&path, &bytes)?;
    if home_override.is_none()
        && let Some(persistent) = persistent_decommissioned_sessions_path()
        && persistent != path
    {
        let _ = write_decommissioned_session_bytes(&persistent, &bytes);
    }
    Ok(())
}

/// Update `retired_personas` in the decommission sidecar while preserving existing `session_ids`.
pub fn sync_retired_personas_in_tombstones(
    home_override: Option<&Path>,
    retired_personas: &BTreeSet<String>,
) -> std::io::Result<()> {
    let ids = load_decommissioned_session_ids(home_override);
    save_decommissioned_tombstones(home_override, &ids, retired_personas)
}

/// Vendors whose `vendor:persona` slug is a hosted session name, not a native id.
const PERSONA_DERIVED_VENDORS: [&str; 9] = [
    "claude",
    "antigravity",
    "agy",
    "codex",
    "muse",
    "grok",
    "lemonade",
    "ollama",
    "host_cli",
];

/// Discovery placeholders `{prefix}-{persona}` when a route has no native id yet.
const PERSONA_DERIVED_PREFIXES: [&str; 8] = [
    "grok", "muse", "webhook", "agy", "codex", "lemonade", "ollama", "host_cli",
];

/// `true` for a hosted slug (`claude:ada`, `agy:ada`, `Ada.claude`, `muse.ada`) or a discovery
/// placeholder (`grok-ada`, `codex-ada`, `claude-ada-1234`). Those name the persona, not one
/// vendor conversation, so a tombstone of them would ban the name.
pub fn is_persona_derived_id(id: &str) -> bool {
    let id = id.trim();
    if id.is_empty() {
        return false;
    }
    if let Some((vendor, persona)) = id.split_once(':') {
        return PERSONA_DERIVED_VENDORS.contains(&vendor.to_ascii_lowercase().as_str())
            && validate_slug(persona).is_ok();
    }
    if let Some((first, second)) = id.split_once('.') {
        let first_lower = first.to_ascii_lowercase();
        let second_lower = second.to_ascii_lowercase();
        if first_lower == "muse" && validate_slug(&second_lower).is_ok() {
            return true;
        }
        if PERSONA_DERIVED_VENDORS.contains(&second_lower.as_str())
            && validate_slug(&first_lower).is_ok()
        {
            return true;
        }
    }
    let lower = id.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("claude-")
        && let Some((persona, pid)) = rest.rsplit_once('-')
        && !pid.is_empty()
        && pid.bytes().all(|b| b.is_ascii_digit())
        && validate_slug(persona).is_ok()
    {
        return true;
    }
    PERSONA_DERIVED_PREFIXES.iter().any(|prefix| {
        lower
            .strip_prefix(prefix)
            .and_then(|rest| rest.strip_prefix('-'))
            .is_some_and(|persona| validate_slug(persona).is_ok())
    })
}

/// A non-empty id that is a vendor's own session id, not a persona name.
pub fn is_native_session_id(id: &str) -> bool {
    let id = id.trim();
    !id.is_empty() && !is_persona_derived_id(id)
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
            VendorRoute::Claude {
                pid: dst_pid,
                session_id: dst_sid,
                socket_path: dst_sock,
                key_path: dst_key,
            },
            VendorRoute::Claude {
                pid: src_pid,
                session_id: src_sid,
                socket_path: src_sock,
                key_path: src_key,
            },
        ) if (*dst_pid > 0 && *dst_pid == *src_pid)
            || (!dst_sock.as_os_str().is_empty() && dst_sock == src_sock) =>
        {
            if dst_sid.trim().is_empty() && !src_sid.trim().is_empty() {
                *dst_sid = src_sid.clone();
            }
            if dst_key.as_ref().is_none_or(|p| !p.is_file())
                && src_key.as_ref().is_some_and(|p| p.is_file())
            {
                *dst_key = src_key.clone();
            }
        }
        (
            VendorRoute::Agy {
                conversation_id: dst_conv,
                ls_pid: dst_pid,
                ls_address: dst_addr,
                csrf_token: dst_csrf,
                agentapi_exe: dst_exe,
            },
            VendorRoute::Agy {
                conversation_id: src_conv,
                ls_pid: src_pid,
                ls_address: src_addr,
                csrf_token: src_csrf,
                agentapi_exe: src_exe,
            },
        ) => {
            let same_or_placeholder_conv = dst_conv == src_conv
                || !is_native_session_id(dst_conv)
                || !is_native_session_id(src_conv);
            let port_compatible = match (
                dst_addr.as_deref().and_then(agy_ls_port),
                src_addr.as_deref().and_then(agy_ls_port),
            ) {
                (Some(dp), Some(sp)) => dp == sp,
                _ => true,
            };
            let dst_has_live_pair = dst_addr.as_deref().is_some_and(agy_ls_address_listening)
                && dst_csrf.as_deref().is_some_and(|t| !t.trim().is_empty());
            let src_has_live_pair = src_addr.as_deref().is_some_and(agy_ls_address_listening)
                && src_csrf.as_deref().is_some_and(|t| !t.trim().is_empty());
            if same_or_placeholder_conv
                && port_compatible
                && !dst_has_live_pair
                && src_has_live_pair
            {
                *dst_addr = src_addr.clone();
                *dst_csrf = src_csrf.clone();
                if dst_pid.is_none() {
                    *dst_pid = *src_pid;
                }
            } else if same_or_placeholder_conv && dst_pid.is_none() {
                *dst_pid = *src_pid;
            }
            if dst_exe.as_ref().is_none_or(|path| !agy_exe_is_usable(path))
                && src_exe.as_ref().is_some_and(|path| agy_exe_is_usable(path))
            {
                *dst_exe = src_exe.clone();
            }
        }
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
        (
            VendorRoute::HostCli {
                cwd: dst_cwd,
                tmux_target: dst_target,
                tmux_pane: dst_pane,
                ..
            },
            VendorRoute::HostCli {
                cwd: src_cwd,
                tmux_target: src_target,
                tmux_pane: src_pane,
                ..
            },
        ) => {
            if dst_cwd.is_none() {
                *dst_cwd = src_cwd.clone();
            }
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

/// True when `/proc/{pid}` exists and its process state is not `'Z'` (zombie).
pub fn proc_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let proc_dir = Path::new("/proc").join(pid.to_string());
    if !proc_dir.exists() {
        return false;
    }
    if let Ok(stat) = fs::read_to_string(proc_dir.join("stat"))
        && let Some(rparen) = stat.rfind(')')
        && let Some(state) = stat[rparen + 1..].split_whitespace().next()
    {
        return state != "Z";
    }
    true
}

fn pid_alive(pid: u32) -> bool {
    proc_alive(pid)
}

pub fn host_probes_allowed(home_override: Option<&Path>) -> bool {
    home_override.is_none()
        && std::env::var("FL_WIRE_DISABLE_HOST_DISCOVERY").as_deref() != Ok("1")
        && crate::dispatcher::host_discovery_allowed()
}

fn agy_host_session_running(persona: Option<&str>) -> bool {
    let Some(norm) = persona.and_then(normalize_persona) else {
        return false;
    };
    let Ok(endpoint) = fl_host_proto::HostEndpoint::discover() else {
        return false;
    };
    let Ok(mut client) = fl_host_proto::BlockingHostClient::connect(&endpoint) else {
        return false;
    };
    let Ok(sessions) = client.list_sessions() else {
        return false;
    };
    let mut cap = norm.clone();
    if let Some(first) = cap.get_mut(0..1) {
        first.make_ascii_uppercase();
    }
    for target in [
        format!("{cap}:antigravity"),
        format!("{cap}:agy"),
        format!("antigravity:{norm}"),
        format!("agy:{norm}"),
    ] {
        if !crate::vendor_adapter::matching_live_host_sessions(&sessions, &target).is_empty() {
            return true;
        }
    }
    false
}

/// Passive liveness check for an explicit route.
///
/// Never connects to Unix sockets (`Webhook` / `Claude` sockets are only checked via
/// `symlink_metadata()`), and skips host TCP / `fl-host` / `tmux` probes whenever
/// `allow_host_probes` is `false`.
pub fn explicit_route_is_dead(
    route: &VendorRoute,
    persona: Option<&str>,
    allow_host_probes: bool,
) -> bool {
    match route {
        VendorRoute::Claude {
            pid, socket_path, ..
        } => {
            if *pid > 0 {
                !proc_alive(*pid)
            } else {
                !socket_path.as_os_str().is_empty() && socket_path.symlink_metadata().is_err()
            }
        }
        VendorRoute::Agy {
            ls_pid, ls_address, ..
        } => {
            if let Some(pid) = *ls_pid
                && pid > 0
            {
                !proc_alive(pid)
                    && (!allow_host_probes
                        || ls_address
                            .as_deref()
                            .filter(|a| !a.trim().is_empty())
                            .is_none_or(|addr| !agy_ls_address_listening(addr)))
            } else if allow_host_probes {
                let addr_dead = ls_address
                    .as_deref()
                    .map(str::trim)
                    .filter(|a| !a.is_empty())
                    .is_some_and(|addr| !agy_ls_address_listening(addr));
                addr_dead && !agy_host_session_running(persona)
            } else {
                false
            }
        }
        VendorRoute::Webhook { socket_path } => {
            !socket_path.as_os_str().is_empty() && socket_path.symlink_metadata().is_err()
        }
        VendorRoute::Muse {
            tmux_target,
            tmux_pane,
            ..
        }
        | VendorRoute::Grok {
            tmux_target,
            tmux_pane,
            ..
        }
        | VendorRoute::HostCli {
            tmux_target,
            tmux_pane,
            ..
        } => {
            if !allow_host_probes {
                return false;
            }
            let mut targets = [tmux_target.as_deref(), tmux_pane.as_deref()]
                .into_iter()
                .flatten()
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .peekable();
            if targets.peek().is_none() {
                return false;
            }
            !targets.any(crate::dispatcher::tmux_target_alive_passive)
        }
        VendorRoute::Codex { .. } => false,
    }
}

fn agy_route_accepts_shared_endpoint(
    route_pid: Option<u32>,
    shared_ls_pid: Option<u32>,
    check_proc: bool,
) -> bool {
    match route_pid {
        None | Some(0) => true,
        Some(pid) if check_proc && !pid_alive(pid) => true,
        Some(pid) => shared_ls_pid == Some(pid),
    }
}

/// Read one `/proc/<pid>/environ` blob. A language-server address is kept only
/// when its port is listening, a CSRF token is kept only as a pair with a live
/// listening language-server address from that same process, and an `agentapi`
/// path is kept only when the file is usable. Dead `bwrap` ports and unlinked
/// `.old` binaries are skipped so a later process can still supply the live values.
#[cfg(test)]
fn note_agy_proc_env(
    env_bytes: &[u8],
    shared_csrf: &mut Option<String>,
    shared_ls_addr: &mut Option<String>,
    shared_exe: &mut Option<PathBuf>,
) {
    let mut ignored_pid = None;
    note_agy_proc_env_with_pid(
        env_bytes,
        None,
        &mut ignored_pid,
        shared_csrf,
        shared_ls_addr,
        shared_exe,
    );
}

fn note_agy_proc_env_with_pid(
    env_bytes: &[u8],
    proc_pid: Option<u32>,
    shared_ls_pid: &mut Option<u32>,
    shared_csrf: &mut Option<String>,
    shared_ls_addr: &mut Option<String>,
    shared_exe: &mut Option<PathBuf>,
) {
    let mut proc_csrf: Option<&str> = None;
    let mut proc_ls_addr: Option<&str> = None;
    let mut proc_exe: Option<&str> = None;
    for kv in env_bytes.split(|&b| b == 0) {
        let Ok(kv_str) = std::str::from_utf8(kv) else {
            continue;
        };
        if proc_csrf.is_none()
            && let Some(val) = kv_str.strip_prefix("ANTIGRAVITY_CSRF_TOKEN=")
            && !val.trim().is_empty()
        {
            proc_csrf = Some(val);
        } else if proc_ls_addr.is_none()
            && let Some(val) = kv_str.strip_prefix("ANTIGRAVITY_LS_ADDRESS=")
            && !val.trim().is_empty()
            && agy_ls_address_listening(val)
        {
            proc_ls_addr = Some(val);
        } else if proc_exe.is_none()
            && let Some(val) = kv_str.strip_prefix("ANTIGRAVITY_AGENTAPI_EXE=")
            && agy_exe_is_usable(Path::new(val))
        {
            proc_exe = Some(val);
        }
    }
    if shared_exe.is_none()
        && let Some(exe) = proc_exe
    {
        *shared_exe = Some(PathBuf::from(exe));
    }
    if let Some(addr) = proc_ls_addr {
        if let Some(existing_addr) = shared_ls_addr.as_deref() {
            if agy_ls_port(existing_addr).is_some()
                && agy_ls_port(existing_addr) == agy_ls_port(addr)
            {
                if shared_ls_pid.is_none() {
                    *shared_ls_pid = proc_pid;
                }
                if shared_csrf.is_none()
                    && let Some(csrf) = proc_csrf
                {
                    *shared_csrf = Some(csrf.to_string());
                }
            }
        } else {
            *shared_ls_addr = Some(addr.to_string());
            *shared_ls_pid = proc_pid;
            if shared_csrf.is_none()
                && let Some(csrf) = proc_csrf
            {
                *shared_csrf = Some(csrf.to_string());
            }
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
        let legacy = if custom_path.is_none()
            && !primary.exists()
            && std::env::var("FL_WIRE_DISABLE_HOST_DISCOVERY").as_deref() != Ok("1")
        {
            legacy_routes_path()
        } else {
            None
        };
        Self::load_with_legacy(&primary, legacy.as_deref())
    }

    /// The table in `primary`, falling back to `legacy` (`~/.cache/agent-bus/routes.json`) only
    /// when `primary` does not exist or cannot be loaded (for example right after a reboot when
    /// `/run/user/$UID/fl-wire-routes.json` is absent). Once `primary` exists, stale entries in
    /// `legacy` are never merged on top of it.
    fn load_with_legacy(primary: &Path, legacy: Option<&Path>) -> Self {
        if let Some(table) = Self::load_file(primary) {
            return table;
        }
        legacy.and_then(Self::load_file).unwrap_or_default()
    }

    pub fn merge_newer(&mut self, other: Self) {
        // Apply source revalidation before freshness comparisons. Matching the whole pre-scan
        // entry preserves concurrent registrations, changed valid routes, and newer disk reloads.
        for (key, result) in other.passive_agy_revalidation {
            let current = self.passive_agy_revalidation.get(&key);
            // Keep confirmations even when this receiver missed the rejection, so replaying
            // that older rejection cannot revoke the successfully revalidated claim.
            if current.is_none_or(|old| result.observed_at > old.observed_at)
                && (result.validated_identity_at.is_none()
                    || current.is_none_or(|old| {
                        old.entry == result.entry
                            || old.validated_identity_at.is_some()
                            || self.entries.get(&key) == Some(&result.entry)
                    }))
            {
                self.passive_agy_revalidation.insert(key, result);
            }
        }
        self.entries.retain(|key, entry| {
            self.passive_agy_revalidation
                .get(key)
                .is_none_or(|result| result.normalize_or_reject(entry))
        });
        for (k, mut v) in other.entries {
            if self
                .passive_agy_revalidation
                .get(&k)
                .is_some_and(|result| !result.normalize_or_reject(&mut v))
            {
                continue;
            }
            if !v.explicit
                && v.route.session_identity().is_some_and(|identity| {
                    self.entries.iter().any(|(name, entry)| {
                        name != &k
                            && entry.explicit
                            && entry.route.session_identity() == Some(identity)
                    })
                })
            {
                continue;
            }
            if let Some(existing) = self.entries.get(&k)
                && existing.explicit
                && !v.explicit
            {
                if std::mem::discriminant(&existing.route) != std::mem::discriminant(&v.route) {
                    continue;
                }
                v.explicit = true;
                v.identity_at = None;
                if v.hosted_id.is_none() {
                    v.hosted_id = existing.hosted_id.clone();
                }
                if v.hydration_generation.is_none() {
                    v.hydration_generation = existing.hydration_generation.clone();
                }
            }
            if let Some((target_kind, target_id)) = v
                .route
                .session_identity()
                .map(|(kind, id)| (kind, id.to_string()))
            {
                let target_sid = (target_kind, target_id.as_str());
                let mut beaten_by_same_sid = false;
                for (name, existing) in &mut self.entries {
                    if existing.route.session_identity() == Some(target_sid)
                        && !incoming_beats_incumbent(&v, existing, name == &k)
                    {
                        preserve_muse_tmux_fields(&mut existing.route, &v.route);
                        if name == &k
                            && existing.explicit
                            && v.explicit
                            && std::mem::discriminant(&existing.route)
                                == std::mem::discriminant(&v.route)
                        {
                            if existing.hosted_id.is_none() {
                                existing.hosted_id = v.hosted_id.clone();
                            }
                            if existing.hydration_generation.is_none() {
                                existing.hydration_generation = v.hydration_generation.clone();
                            }
                        }
                        if name == &k && !existing.explicit && !v.explicit {
                            existing.identity_at = existing
                                .identity_evidence_at()
                                .max(v.identity_evidence_at())
                                .cloned();
                            restamp_if_newer(&mut existing.updated_at, &v.updated_at);
                        }
                        beaten_by_same_sid = true;
                    }
                }
                if beaten_by_same_sid {
                    continue;
                }
                if let Some(existing) = self.entries.get_mut(&k) {
                    if !incoming_beats_incumbent(&v, existing, true) {
                        preserve_muse_tmux_fields(&mut existing.route, &v.route);
                        if existing.explicit
                            && v.explicit
                            && std::mem::discriminant(&existing.route)
                                == std::mem::discriminant(&v.route)
                        {
                            if existing.hosted_id.is_none() {
                                existing.hosted_id = v.hosted_id.clone();
                            }
                            if existing.hydration_generation.is_none() {
                                existing.hydration_generation = v.hydration_generation.clone();
                            }
                        }
                        continue;
                    }
                    preserve_muse_tmux_fields(&mut v.route, &existing.route);
                    if existing.explicit
                        && v.explicit
                        && std::mem::discriminant(&existing.route)
                            == std::mem::discriminant(&v.route)
                    {
                        if v.hosted_id.is_none() {
                            v.hosted_id = existing.hosted_id.clone();
                        }
                        if v.hydration_generation.is_none() {
                            v.hydration_generation = existing.hydration_generation.clone();
                        }
                    }
                    if !existing.explicit
                        && !v.explicit
                        && existing.route.session_identity() == Some(target_sid)
                    {
                        v.identity_at = existing
                            .identity_evidence_at()
                            .max(v.identity_evidence_at())
                            .cloned();
                        restamp_if_newer(&mut v.updated_at, &existing.updated_at);
                    }
                }
                self.entries.retain(|existing_k, old| {
                    if old.route.session_identity() == Some(target_sid) {
                        preserve_muse_tmux_fields(&mut v.route, &old.route);
                        existing_k == &k
                    } else {
                        true
                    }
                });
                self.entries.insert(k, v);
            } else {
                match self.entries.get_mut(&k) {
                    Some(existing) if !incoming_beats_incumbent(&v, existing, true) => {
                        preserve_muse_tmux_fields(&mut existing.route, &v.route);
                        if existing.explicit
                            && v.explicit
                            && std::mem::discriminant(&existing.route)
                                == std::mem::discriminant(&v.route)
                        {
                            if existing.hosted_id.is_none() {
                                existing.hosted_id = v.hosted_id.clone();
                            }
                            if existing.hydration_generation.is_none() {
                                existing.hydration_generation = v.hydration_generation.clone();
                            }
                        }
                        if !existing.explicit && !v.explicit && existing.route == v.route {
                            if existing.identity_evidence_at().is_none() {
                                existing.identity_at = v.identity_evidence_at().cloned();
                            }
                            restamp_if_newer(&mut existing.updated_at, &v.updated_at);
                        }
                    }
                    Some(existing) => {
                        preserve_muse_tmux_fields(&mut v.route, &existing.route);
                        if existing.explicit
                            && v.explicit
                            && std::mem::discriminant(&existing.route)
                                == std::mem::discriminant(&v.route)
                        {
                            if v.hosted_id.is_none() {
                                v.hosted_id = existing.hosted_id.clone();
                            }
                            if v.hydration_generation.is_none() {
                                v.hydration_generation = existing.hydration_generation.clone();
                            }
                        }
                        self.entries.insert(k, v);
                    }
                    None => {
                        self.entries.insert(k, v);
                    }
                }
            }
        }
        for diag in other.registration_diagnostics {
            self.push_registration_diagnostic(diag);
        }
    }

    /// Fold the result of a dispatch back into the live table. `working` started as a clone of
    /// `base` and went through `discover_routes`, which both adds routes and prunes dead ones.
    /// A route that `base` held but `working` dropped is removed here too, unless the live table
    /// changed it meanwhile (a concurrent `RegisterRoute`), in which case the live entry wins.
    /// Everything else merges as in [`RouteTable::merge_newer`].
    pub fn merge_dispatch_result(&mut self, base: &Self, working: Self) {
        self.last_discovery = self.last_discovery.max(working.last_discovery);
        for (key, base_entry) in &base.entries {
            if !working.entries.contains_key(key) && self.entries.get(key) == Some(base_entry) {
                self.entries.remove(key);
            }
        }
        self.merge_newer(working);
    }

    /// Merge a daemon dispatch snapshot without restoring a route concurrently unregistered
    /// from the daemon. Hub scan reconciliation retains its existing merge semantics above.
    pub fn merge_daemon_dispatch_result(&mut self, base: &Self, mut working: Self) {
        // A scan made on the working copy counts for the live table too.
        self.last_discovery = self.last_discovery.max(working.last_discovery);
        // A concurrently changed persona can own either its old or new session. Protect both
        // identities against stale aliases as well as the changed persona key itself.
        let changed_sessions: BTreeSet<(&str, String)> = base
            .entries
            .iter()
            .chain(self.entries.iter())
            .filter(|(key, _)| base.entries.get(*key) != self.entries.get(*key))
            .filter_map(|(_, entry)| {
                entry
                    .route
                    .session_identity()
                    .map(|(kind, id)| (kind, id.to_owned()))
            })
            .collect();
        for (key, base_entry) in &base.entries {
            if !working.entries.contains_key(key) && self.entries.get(key) == Some(base_entry) {
                self.entries.remove(key);
            }
        }
        working.entries.retain(|key, entry| {
            self.entries.get(key) == base.entries.get(key)
                && !entry
                    .route
                    .session_identity()
                    .is_some_and(|(kind, id)| changed_sessions.contains(&(kind, id.to_owned())))
        });
        self.merge_newer(working);
    }

    fn load_file(path: &Path) -> Option<Self> {
        let raw = fs::read_to_string(path).ok()?;
        if let Ok(mut table) = serde_json::from_str::<RouteTable>(&raw) {
            table.registration_diagnostics = table
                .registration_diagnostics
                .into_iter()
                .map(RouteRegistrationDiagnostic::sanitized)
                .collect();
            if table.registration_diagnostics.len() > MAX_REGISTRATION_DIAGNOSTICS {
                let excess = table.registration_diagnostics.len() - MAX_REGISTRATION_DIAGNOSTICS;
                table.registration_diagnostics.drain(0..excess);
            }
            return Some(table);
        }
        // Also support flat map `{ "<persona>": RouteEntry }` or legacy agent-bus route map
        if let Ok(entries) = serde_json::from_str::<BTreeMap<String, RouteEntry>>(&raw) {
            return Some(RouteTable {
                entries,
                registration_diagnostics: Vec::new(),
                last_discovery: None,
                passive_agy_revalidation: BTreeMap::new(),
            });
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
                            identity_at: None,
                            explicit: false,
                            hosted_id: None,
                            hydration_generation: None,
                        },
                    );
                }
            }
            if !entries.is_empty() {
                return Some(RouteTable {
                    entries,
                    registration_diagnostics: Vec::new(),
                    last_discovery: None,
                    passive_agy_revalidation: BTreeMap::new(),
                });
            }
        }
        None
    }

    pub fn push_registration_diagnostic(&mut self, diagnostic: RouteRegistrationDiagnostic) {
        let diagnostic = diagnostic.sanitized();
        let duplicate = self.registration_diagnostics.iter().any(|existing| {
            existing.at == diagnostic.at
                && existing.persona == diagnostic.persona
                && existing.outcome == diagnostic.outcome
                && existing.reason == diagnostic.reason
                && existing.challenger_adapter == diagnostic.challenger_adapter
                && existing.replace == diagnostic.replace
        });
        if !duplicate {
            self.registration_diagnostics.push(diagnostic);
        }
        if self.registration_diagnostics.len() > MAX_REGISTRATION_DIAGNOSTICS {
            let excess = self.registration_diagnostics.len() - MAX_REGISTRATION_DIAGNOSTICS;
            self.registration_diagnostics.drain(0..excess);
        }
    }

    pub fn register_route_checked(
        &mut self,
        args: RegisterRouteCheckArgs,
    ) -> Result<Option<RouteRegistrationDiagnostic>, RegisterRouteRefusal> {
        let Some(norm) = normalize_persona(&args.persona) else {
            return Err(RegisterRouteRefusal {
                message: format!("invalid persona name `{}`", args.persona),
                diagnostic: None,
            });
        };
        if !args.explicit {
            self.upsert(&norm, args.route, args.at);
            return Ok(None);
        }
        let new_hosted_id = args
            .hosted_id
            .as_deref()
            .and_then(|raw| parse_hosted_identity(raw).ok())
            .map(|(v, p)| format!("{v}:{p}"));
        let new_generation = sanitize_generation_token(args.hydration_generation.as_deref());

        let mut clear_existing_meta = false;
        let mut recorded_diag: Option<RouteRegistrationDiagnostic> = None;

        if let Some(incumbent) = self.entries.get(&norm)
            && incumbent.explicit
        {
            let inc_hosted_id = incumbent
                .hosted_id
                .as_deref()
                .and_then(|raw| parse_hosted_identity(raw).ok())
                .map(|(v, p)| format!("{v}:{p}"));
            let inc_generation =
                sanitize_generation_token(incumbent.hydration_generation.as_deref());
            let is_codex_same_vendor = matches!(incumbent.route, VendorRoute::Codex { .. })
                && matches!(args.route, VendorRoute::Codex { .. });

            let conflict_reason =
                if incumbent.route.canonical_vendor() != args.route.canonical_vendor() {
                    Some(RegistrationConflictReason::VendorMismatch)
                } else if inc_generation.is_some()
                    && new_generation.is_some()
                    && inc_generation != new_generation
                {
                    Some(RegistrationConflictReason::HydrationGenerationMismatch)
                } else if (inc_hosted_id.is_some()
                    && new_hosted_id.is_some()
                    && inc_hosted_id != new_hosted_id)
                    || (inc_hosted_id.is_some() && new_hosted_id.is_none() && !is_codex_same_vendor)
                {
                    Some(RegistrationConflictReason::HostedIdentityMismatch)
                } else {
                    None
                };

            if let Some(reason) = conflict_reason {
                clear_existing_meta = true;
                if !explicit_route_is_dead(&incumbent.route, Some(&norm), args.allow_host_probes) {
                    let inc_vendor = incumbent.route.canonical_vendor().to_string();
                    let inc_adapter = RouteAdapter::from_route(&incumbent.route);
                    let chal_adapter = RouteAdapter::from_route(&args.route);
                    let outcome = if args.hosted_session_verified || args.replace {
                        RegistrationDiagnosticOutcome::Replaced
                    } else {
                        RegistrationDiagnosticOutcome::Refused
                    };
                    let diag = RouteRegistrationDiagnostic {
                        at: args.at.clone(),
                        persona: norm.clone(),
                        outcome,
                        reason,
                        incumbent_adapter: Some(inc_adapter),
                        challenger_adapter: chal_adapter,
                        incumbent_hosted_id: inc_hosted_id,
                        challenger_hosted_id: new_hosted_id.clone(),
                        incumbent_generation: inc_generation,
                        challenger_generation: new_generation.clone(),
                        replace: args.replace,
                    }
                    .sanitized();
                    self.push_registration_diagnostic(diag.clone());
                    if outcome == RegistrationDiagnosticOutcome::Refused {
                        return Err(RegisterRouteRefusal {
                            message: format!(
                                "refused route registration for `{norm}` ({reason:?}): live explicit `{inc_vendor}` route is owned by incumbent session; pass --replace to override"
                            ),
                            diagnostic: Some(Box::new(diag)),
                        });
                    }
                    recorded_diag = Some(diag);
                }
            }
        }

        self.upsert_explicit_with_meta(
            &norm,
            args.route,
            args.at,
            new_hosted_id,
            new_generation,
            clear_existing_meta,
        );
        Ok(recorded_diag)
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

    pub fn upsert(&mut self, persona: &str, route: VendorRoute, at: At) {
        let identity_at = (at.unix_millis() > 0).then(|| at.clone());
        self.upsert_inner(persona, route, at, identity_at, false);
    }

    /// Register a route selected by the caller rather than discovered by a passive scan.
    pub fn upsert_explicit(&mut self, persona: &str, route: VendorRoute, at: At) {
        self.upsert_inner_with_meta(persona, route, at, None, true, None, None, false);
    }

    pub fn upsert_explicit_with_meta(
        &mut self,
        persona: &str,
        route: VendorRoute,
        at: At,
        hosted_id: Option<String>,
        hydration_generation: Option<String>,
        clear_existing_meta: bool,
    ) {
        self.upsert_inner_with_meta(
            persona,
            route,
            at,
            None,
            true,
            hosted_id,
            hydration_generation,
            clear_existing_meta,
        );
    }

    fn upsert_passive(&mut self, persona: &str, route: VendorRoute, at: At) {
        let identity_at = (at.unix_millis() > 0).then(|| at.clone());
        self.upsert_inner(persona, route, at, identity_at, false);
    }

    fn upsert_passive_without_identity(&mut self, persona: &str, route: VendorRoute, at: At) {
        self.upsert_inner(persona, route, at, None, false);
    }

    fn upsert_inner(
        &mut self,
        persona: &str,
        route: VendorRoute,
        at: At,
        identity_at: Option<At>,
        requested_explicit: bool,
    ) {
        self.upsert_inner_with_meta(
            persona,
            route,
            at,
            identity_at,
            requested_explicit,
            None,
            None,
            false,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn upsert_inner_with_meta(
        &mut self,
        persona: &str,
        mut route: VendorRoute,
        at: At,
        identity_at: Option<At>,
        requested_explicit: bool,
        hosted_id: Option<String>,
        hydration_generation: Option<String>,
        clear_existing_meta: bool,
    ) {
        if let Some(norm) = normalize_persona(persona) {
            if !requested_explicit
                && route.session_identity().is_some_and(|identity| {
                    self.entries.iter().any(|(name, entry)| {
                        name != &norm
                            && entry.explicit
                            && entry.route.session_identity() == Some(identity)
                    })
                })
            {
                return;
            }
            let explicit = if let Some(existing) = self.entries.get(&norm) {
                if !requested_explicit
                    && existing.explicit
                    && std::mem::discriminant(&existing.route) != std::mem::discriminant(&route)
                {
                    return;
                }
                requested_explicit || existing.explicit
            } else {
                requested_explicit
            };
            let normalized_hosted_id = hosted_id
                .as_deref()
                .and_then(|raw| parse_hosted_identity(raw).ok())
                .map(|(v, p)| format!("{v}:{p}"));
            let normalized_generation = sanitize_generation_token(hydration_generation.as_deref());
            let (final_hosted_id, final_generation) = if !explicit {
                (None, None)
            } else if !clear_existing_meta
                && let Some(existing) = self.entries.get(&norm)
                && existing.explicit
                && std::mem::discriminant(&existing.route) == std::mem::discriminant(&route)
            {
                (
                    normalized_hosted_id.or_else(|| existing.hosted_id.clone()),
                    normalized_generation.or_else(|| existing.hydration_generation.clone()),
                )
            } else {
                (normalized_hosted_id, normalized_generation)
            };
            if let Some(existing) = self.entries.get(&norm) {
                preserve_muse_tmux_fields(&mut route, &existing.route);
            }
            if let Some((target_kind, target_id)) = route
                .session_identity()
                .map(|(kind, id)| (kind, id.to_string()))
            {
                let target_sid = (target_kind, target_id.as_str());
                self.entries.retain(|k, existing| {
                    if existing.route.session_identity() == Some(target_sid) {
                        preserve_muse_tmux_fields(&mut route, &existing.route);
                        k == &norm
                    } else {
                        true
                    }
                });
            }
            let identity_at = if explicit {
                None
            } else {
                identity_at.filter(|stamp| stamp.unix_millis() > 0)
            };
            self.entries.insert(
                norm.clone(),
                RouteEntry {
                    persona: norm,
                    route,
                    updated_at: at,
                    identity_at,
                    explicit,
                    hosted_id: final_hosted_id,
                    hydration_generation: final_generation,
                },
            );
        }
    }

    /// Record a file-discovered route at its own mtime.
    ///
    /// Passive sessions compete by [`RouteEntry::identity_evidence_at`]: newer known evidence
    /// wins, incumbent wins ties, and known evidence outranks unknown (`None`). `updated_ms == 0`
    /// stays unknown (`identity_at: None`) for passive routes and is never promoted to scan time.
    /// Explicit non-Antigravity incumbents retain `updated_at` ordering (`updated_ms == 0`
    /// falling back to `now`), while explicit Antigravity routes are never replaced passively.
    fn upsert_discovered(&mut self, persona: &str, route: VendorRoute, updated_ms: u64, now: &At) {
        let Some(norm) = normalize_persona(persona) else {
            return;
        };
        if let Some(incumbent) = self.entries.get(&norm)
            && incumbent.explicit
        {
            if matches!(incumbent.route, VendorRoute::Agy { .. })
                || std::mem::discriminant(&incumbent.route) != std::mem::discriminant(&route)
            {
                return;
            }
            let file_at = if updated_ms > 0 {
                At::from_unix_millis(updated_ms)
            } else {
                now.clone()
            };
            if incumbent.updated_at.unix_millis() > file_at.unix_millis() {
                return;
            }
            if let Some(target_sid) = route.session_identity() {
                let beaten_by_other = self.entries.iter().any(|(name, existing)| {
                    name != &norm
                        && existing.route.session_identity() == Some(target_sid)
                        && (existing.explicit
                            || existing.updated_at.unix_millis() > file_at.unix_millis())
                });
                if beaten_by_other {
                    return;
                }
            }
            self.upsert_inner(&norm, route, file_at, None, false);
            return;
        }
        let cand_identity = (updated_ms > 0).then(|| At::from_unix_millis(updated_ms));
        let cand_updated = cand_identity
            .clone()
            .unwrap_or_else(|| At::from_unix_millis(0));
        let target_sid = route.session_identity();
        if let Some(target_sid) = target_sid
            && self.entries.iter().any(|(name, existing)| {
                name != &norm
                    && existing.route.session_identity() == Some(target_sid)
                    && (existing.explicit
                        || cand_identity.as_ref() <= existing.identity_evidence_at())
            })
        {
            return;
        }
        if let Some(existing) = self.entries.get(&norm) {
            if target_sid.is_some() && existing.route.session_identity() == target_sid {
                if cand_identity.as_ref() < existing.identity_evidence_at() {
                    return;
                }
                let updated_at = existing.updated_at.clone().max(cand_updated);
                let identity_at = cand_identity.or_else(|| existing.identity_at.clone());
                self.upsert_inner(&norm, route, updated_at, identity_at, false);
                return;
            }
            if cand_identity.as_ref() <= existing.identity_evidence_at() {
                return;
            }
        }
        self.upsert_inner(&norm, route, cand_updated, cand_identity, false);
    }

    pub fn get(&self, persona: &str) -> Option<&RouteEntry> {
        let norm = normalize_persona(persona)?;
        self.entries.get(&norm)
    }

    /// `true` when `other` holds the same personas with the same routes, ignoring `updated_at`
    /// and `identity_at` (discovery restamps every live Claude route and refreshes active
    /// transcript mtimes in memory on each scan).
    pub fn same_routes(&self, other: &Self) -> bool {
        self.entries.len() == other.entries.len()
            && self
                .entries
                .iter()
                .all(|(k, e)| other.entries.get(k).is_some_and(|o| o.route == e.route))
    }

    /// Rescan the host's session files and report whether the route held for `persona` changed
    /// (appeared, vanished, or differs). A push that failed uses it to learn that the session was
    /// replaced since the last scan.
    pub fn refresh_persona(&mut self, persona: &str, home_override: Option<&Path>, at: At) -> bool {
        let before = self.get(persona).map(|e| e.route.clone());
        self.discover_routes(home_override, at);
        self.get(persona).map(|e| e.route.clone()) != before
    }

    /// `true` when no discovery has run yet, or the last one was more than `interval` ago.
    pub fn discovery_due(&self, interval: Duration) -> bool {
        self.last_discovery
            .is_none_or(|last| last.elapsed() > interval)
    }

    /// Test seam: pretend the last discovery ran at `at`.
    #[cfg(test)]
    pub(crate) fn set_last_discovery(&mut self, at: Option<Instant>) {
        self.last_discovery = at;
    }

    /// Passively discover Claude, Antigravity, Codex, Meta Muse, and xAI Grok routes from vendor session files.
    pub fn discover_routes(&mut self, home_override: Option<&Path>, at: At) {
        self.last_discovery = Some(Instant::now());
        #[cfg(test)]
        {
            DISCOVERIES.with(|n| n.set(n.get() + 1));
            // A unit test never scans the real home.
            if home_override.is_none() {
                return;
            }
        }
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
        self.drop_decommissioned_routes(home_override);
        let pre_existing: BTreeSet<String> = self
            .entries
            .keys()
            .map(|k| k.to_ascii_lowercase())
            .collect();
        let agy_candidates = self.revalidate_passive_agy_entries(&home);
        self.discover_claude_routes(&home, check_proc, &at);
        self.discover_agy_routes_with_candidates(&home, check_proc, &at, agy_candidates);
        self.discover_codex_routes(&home, &at);
        self.discover_muse_routes(&home, home_override.is_some(), &at);
        self.discover_grok_routes(&home, check_proc, &at);
        self.discover_scanner_routes(&home, &at);
        // Last, so a placeholder route for a live window cannot shadow a route a transcript
        // scan above found for the same persona. Muse keeps a persona that has both a Muse
        // window and a Grok window and no transcript route; Grok fills only what is left.
        self.discover_muse_windows(&home, home_override.is_some(), &at);
        if muse_tmux_discovery_enabled(&home, home_override.is_some()) {
            if let Some(windows) = list_tmux_windows() {
                self.attach_grok_tmux_placeholders(&windows, &at);
            }
            if crate::dispatcher::host_discovery_allowed() {
                self.discover_host_cli_sessions(&at);
            }
        }
        self.drop_decommissioned_and_retired_routes(home_override, &pre_existing);
    }

    /// Drop routes whose native session id was decommissioned.
    /// A webhook, an empty session id, and a persona-derived id are kept. A later
    /// route for the same persona with a different native id stays.
    fn drop_decommissioned_routes(&mut self, home_override: Option<&Path>) {
        let banned = load_decommissioned_session_ids(home_override);
        if banned.is_empty() {
            return;
        }
        self.entries
            .retain(|_persona, entry| !route_is_decommissioned(&entry.route, &banned));
    }

    /// Drop routes whose native session id was decommissioned, plus any passive route
    /// newly discovered during this scan for a retired persona that had no route before
    /// the scan.
    fn drop_decommissioned_and_retired_routes(
        &mut self,
        home_override: Option<&Path>,
        pre_existing: &BTreeSet<String>,
    ) {
        self.drop_decommissioned_routes(home_override);
        let retired = load_retired_personas(home_override);
        if retired.is_empty() {
            return;
        }
        self.entries.retain(|persona, entry| {
            entry.explicit
                || pre_existing.contains(&persona.to_ascii_lowercase())
                || !retired.contains(&persona.to_ascii_lowercase())
        });
    }

    /// Merge the routes of every scanner registered with
    /// [`register_wire_scanner`](crate::scanner::register_wire_scanner), after the built-in scans.
    ///
    /// A scanner writes a route only for a name the table does not hold, or to replace the very
    /// route that same scanner last produced for the name (see
    /// [`scanner_may_write`](crate::scanner::scanner_may_write)). It never replaces a route an
    /// agent registered, a built-in scan found or another scanner produced, and an earlier
    /// scanner's route wins over a later scanner's for the same name. A route already in the
    /// table that is newer than this pass is kept, as for the built-in scans. A scanner that
    /// panics is skipped for this pass.
    fn discover_scanner_routes(&mut self, home: &Path, at: &At) {
        let mut claimed: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for (vendor_id, scanner) in crate::scanner::registered_scanners() {
            let found =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| scanner.discover(home)));
            let Ok(found) = found else {
                eprintln!("fl-wire: route scanner `{vendor_id}` panicked; skipped for this pass");
                continue;
            };
            for (agent, route) in found {
                let Some(name) = normalize_persona(&agent) else {
                    continue;
                };
                if !crate::scanner::scanner_route_is_valid(&route) || claimed.contains(&name) {
                    continue;
                }
                let existing = self.entries.get(&name).map(|e| e.route.clone());
                if !crate::scanner::scanner_may_write(&vendor_id, &name, existing.as_ref()) {
                    continue;
                }
                claimed.insert(name.clone());
                if existing.as_ref() == Some(&route) {
                    continue;
                }
                if self
                    .entries
                    .get(&name)
                    .is_some_and(|existing| existing.updated_at > *at)
                {
                    continue;
                }
                self.upsert_passive_without_identity(&name, route.clone(), at.clone());
                if self.entries.get(&name).is_some_and(|e| e.route == route) {
                    crate::scanner::record_produced(&vendor_id, &name, &route);
                }
            }
        }
    }

    fn discover_claude_routes(&mut self, home: &Path, check_proc: bool, at: &At) {
        if check_proc {
            self.entries.retain(|_, entry| {
                entry.explicit
                    || match &entry.route {
                        VendorRoute::Claude { pid, .. } if *pid > 0 => {
                            Path::new(&format!("/proc/{pid}")).exists()
                        }
                        _ => true,
                    }
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
                && self.entries.get(&derived_key).is_some_and(|entry| {
                    !entry.explicit && matches!(entry.route, VendorRoute::Claude { .. })
                })
            {
                self.entries.remove(&derived_key);
            }
            let known = |persona: &str| self.entries.contains_key(persona);
            let Some(persona) =
                resolve_claude_session_persona(home, &val, Some(&self.entries), &known)
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
            self.upsert_passive(
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

    fn revalidate_passive_agy_entries(&mut self, home: &Path) -> Vec<(u64, String, String)> {
        let brain_dir = home.join(".gemini").join("antigravity-cli").join("brain");
        // Revalidate passive identity claims before endpoint repair or any early return.
        // A cached conversation ID is not evidence by itself: its bounded transcript head
        // must still identify that persona, with a known nonzero source timestamp.
        let mut candidates: Vec<(u64, String, String)> = Vec::new();
        let mut rejected_sources = BTreeMap::new();
        let mut absent_reason = "source missing";
        match fs::read_dir(&brain_dir) {
            Ok(read_dir) => {
                for entry in read_dir {
                    let Ok(entry) = entry else {
                        absent_reason = "conversation directory unreadable";
                        continue;
                    };
                    let conv_dir = entry.path();
                    let Some(conv_id) = conv_dir.file_name().and_then(|s| s.to_str()) else {
                        continue;
                    };
                    let transcript = conv_dir.join(".system_generated/logs/transcript.jsonl");
                    match passive_agy_source(&transcript) {
                        Ok((file_ms, persona)) => {
                            candidates.push((file_ms, conv_id.to_owned(), persona))
                        }
                        Err(reason) => {
                            rejected_sources.insert(conv_id.to_owned(), reason);
                        }
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => absent_reason = "source directory unreadable",
        }
        let validated: BTreeMap<&str, (u64, &str)> = candidates
            .iter()
            .map(|(file_ms, conversation_id, persona)| {
                (conversation_id.as_str(), (*file_ms, persona.as_str()))
            })
            .collect();
        let validation_at = Instant::now();
        // A later bounded scan can validate a previously rejected conversation again. Do not
        // let old in-memory rejection evidence suppress that now-valid identity.
        for (persona, result) in &mut self.passive_agy_revalidation {
            result.validated_identity_at = match &result.entry.route {
                VendorRoute::Agy {
                    conversation_id, ..
                } => validated
                    .get(conversation_id.as_str())
                    .filter(|&&(_, p)| p == persona.as_str())
                    .map(|&(file_ms, _)| At::from_unix_millis(file_ms)),
                _ => None,
            };
            result.observed_at = validation_at;
        }
        self.entries.retain(|persona, entry| {
            let keep = if entry.explicit {
                true
            } else {
                match &entry.route {
                    VendorRoute::Agy {
                        conversation_id, ..
                    } => {
                        if let Some(&(file_ms, valid_persona)) =
                            validated.get(conversation_id.as_str())
                            && valid_persona == persona.as_str()
                        {
                            let file_at = At::from_unix_millis(file_ms);
                            if entry.identity_at.as_ref() != Some(&file_at) {
                                self.passive_agy_revalidation.insert(
                                    persona.clone(),
                                    PassiveAgyRevalidation {
                                        entry: entry.clone(),
                                        validated_identity_at: Some(file_at.clone()),
                                        observed_at: validation_at,
                                    },
                                );
                                entry.identity_at = Some(file_at);
                            }
                            true
                        } else {
                            false
                        }
                    }
                    _ => true,
                }
            };
            if !keep {
                if let VendorRoute::Agy { conversation_id, .. } = &entry.route {
                    let reason = if validated.contains_key(conversation_id.as_str()) {
                        "persona mismatch"
                    } else {
                        rejected_sources.get(conversation_id).copied().unwrap_or(absent_reason)
                    };
                    // JSON escaping keeps untrusted IDs on one line. Never format the route:
                    // it contains endpoint and credential material unrelated to rejection.
                    let diagnostic = format!("fl-wire: rejected passive Antigravity claim {}", serde_json::json!({
                        "persona": persona, "conversation_id": conversation_id, "reason": reason,
                    }));
                    eprintln!("{diagnostic}");
                    #[cfg(test)]
                    tests::AGY_REJECTION_DIAGNOSTICS.with(|lines| lines.borrow_mut().push(diagnostic));
                }
                self.passive_agy_revalidation.insert(
                    persona.clone(),
                    PassiveAgyRevalidation {
                        entry: entry.clone(),
                        validated_identity_at: None,
                        observed_at: validation_at,
                    },
                );
            }
            keep
        });
        candidates
    }

    #[cfg(test)]
    fn discover_agy_routes(&mut self, home: &Path, check_proc: bool, at: &At) {
        let candidates = self.revalidate_passive_agy_entries(home);
        self.discover_agy_routes_with_candidates(home, check_proc, at, candidates);
    }

    fn discover_agy_routes_with_candidates(
        &mut self,
        home: &Path,
        check_proc: bool,
        at: &At,
        mut candidates: Vec<(u64, String, String)>,
    ) {
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
                    let replaced = agentapi_exe.is_some();
                    *agentapi_exe = Some(fallback.clone());
                    if replaced {
                        restamp_if_newer(&mut entry.updated_at, at);
                    }
                }
            }
            return;
        }
        let mut shared_ls_pid: Option<u32> = None;
        let mut shared_ls_addr: Option<String> = None;
        let mut shared_csrf: Option<String> = None;
        let mut shared_exe: Option<PathBuf> = None;

        // 1. Prefer live language-server HTTP port from the agy logs so host reboots immediately
        // override stale pre-reboot ports persisted in routes.json. `cli.log` is the newest log,
        // but a one-shot `agy` command (`agy models`) rotates it to a log of its own that shuts
        // its language server down within a second, while the persistent `agy remote-control
        // serve` daemon keeps writing to the log it opened. A log whose last listener was shut
        // down names no port, so the scan falls through to the logs next to it.
        'logs: for log in agy_log_candidates(&agy_dir) {
            let Ok(log_text) = fs::read_to_string(&log) else {
                continue;
            };
            for port in agy_live_http_ports(&log_text) {
                let candidate = format!("127.0.0.1:{port}");
                // Offline temp-home discovery does not probe listeners.
                // Host discovery keeps scanning until a port accepts.
                if !check_proc || agy_ls_address_listening(&candidate) {
                    shared_ls_addr = Some(candidate);
                    break 'logs;
                }
            }
        }

        // 2. Inspect live /proc/*/environ when host discovery is enabled.
        if check_proc
            && (shared_csrf.is_none()
                || shared_ls_addr.is_none()
                || shared_ls_pid.is_none()
                || shared_exe.is_none())
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
                let proc_pid = s.parse::<u32>().ok().filter(|&p| p > 0);
                let Ok(env_bytes) = fs::read(entry.path().join("environ")) else {
                    continue;
                };
                note_agy_proc_env_with_pid(
                    &env_bytes,
                    proc_pid,
                    &mut shared_ls_pid,
                    &mut shared_csrf,
                    &mut shared_ls_addr,
                    &mut shared_exe,
                );
                if shared_csrf.is_some()
                    && shared_ls_addr.is_some()
                    && shared_ls_pid.is_some()
                    && shared_exe.is_some()
                {
                    break;
                }
            }
        }

        // 3. Fall back to existing Agy entries ordered from newest updated_at to oldest.
        // Keep `(ls_address, csrf_token)` paired to the same endpoint so a `cli.log` port from one
        // process is never mixed with a CSRF token from a different session's language server.
        let mut agy_entries: Vec<&RouteEntry> = self
            .entries
            .values()
            .filter(|e| matches!(e.route, VendorRoute::Agy { .. }))
            .collect();
        agy_entries.sort_by_key(|e| std::cmp::Reverse(e.updated_at.unix_millis()));
        for entry in agy_entries {
            if let VendorRoute::Agy {
                ls_pid,
                ls_address,
                csrf_token,
                agentapi_exe,
                ..
            } = &entry.route
            {
                let live_pid = ls_pid.filter(|&pid| pid > 0 && (!check_proc || pid_alive(pid)));
                let live_addr = ls_address.as_ref().filter(|addr| {
                    !addr.trim().is_empty() && (!check_proc || agy_ls_address_listening(addr))
                });
                let valid_csrf = csrf_token.as_ref().filter(|t| !t.trim().is_empty());
                if shared_ls_addr.is_none() {
                    if let Some(addr) = live_addr {
                        shared_ls_addr = Some(addr.clone());
                        if shared_ls_pid.is_none() {
                            shared_ls_pid = live_pid;
                        }
                        if shared_csrf.is_none() {
                            shared_csrf = valid_csrf.cloned();
                        }
                    }
                } else if let (Some(current_addr), Some(entry_addr)) =
                    (shared_ls_addr.as_deref(), live_addr)
                    && agy_ls_port(current_addr).is_some()
                    && agy_ls_port(current_addr) == agy_ls_port(entry_addr)
                {
                    if shared_ls_pid.is_none() {
                        shared_ls_pid = live_pid;
                    }
                    if shared_csrf.is_none()
                        && let Some(csrf) = valid_csrf
                    {
                        shared_csrf = Some(csrf.clone());
                    }
                }
                if shared_exe.is_none()
                    && let Some(exe) = agentapi_exe.as_ref().filter(|path| agy_exe_is_usable(path))
                {
                    shared_exe = Some(exe.clone());
                }
            }
        }

        // 4. Propagate the live/newest shared ls_address and csrf_token across Agy routes that do
        // not already hold a live listening `(ls_address, csrf_token)` pair of their own and whose
        // `ls_pid` is absent, dead, or matches `shared_ls_pid`, so pre-reboot entries do not retain
        // stale language-server ports or CSRF tokens while live standalone `fl-host` Antigravity
        // sessions keep their own process-bound endpoint.
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
            let mut changed = false;
            if let VendorRoute::Agy {
                conversation_id,
                ls_pid,
                ls_address,
                csrf_token,
                agentapi_exe,
            } = &mut entry.route
            {
                let before = (ls_address.clone(), csrf_token.clone(), agentapi_exe.clone());
                let has_live_paired_endpoint =
                    ls_address.as_deref().is_some_and(agy_ls_address_listening)
                        && csrf_token
                            .as_deref()
                            .is_some_and(|token| !token.trim().is_empty());
                let accepts_shared =
                    agy_route_accepts_shared_endpoint(*ls_pid, shared_ls_pid, check_proc);
                if !has_live_paired_endpoint {
                    if accepts_shared && shared_ls_addr.is_some() {
                        *ls_address = shared_ls_addr.clone();
                    } else if check_proc
                        && ls_address
                            .as_deref()
                            .is_some_and(|addr| !agy_ls_address_listening(addr))
                    {
                        *ls_address = None;
                    }
                    if accepts_shared && shared_csrf.is_some() {
                        *csrf_token = shared_csrf.clone();
                    } else if before.0.as_deref().and_then(agy_ls_port)
                        != ls_address.as_deref().and_then(agy_ls_port)
                    {
                        *csrf_token = None;
                    }
                }
                if agentapi_exe
                    .as_ref()
                    .is_none_or(|path| !agy_exe_is_usable(path))
                {
                    *agentapi_exe = Some(resolved_shared.clone());
                }
                // Filling an exe the route never had is no correction: the legacy copy holds none
                // either, or a stale one a later scan replaces. Do not advance a placeholder's
                // `updated_at` before the brain scan compares it against transcript `file_ms`.
                changed = is_native_session_id(conversation_id)
                    && (before.0 != *ls_address
                        || before.1 != *csrf_token
                        || (before.2.is_some() && before.2 != *agentapi_exe));
            }
            // A corrected endpoint is a newer entry: `RouteTable::load` merges the legacy
            // agent-bus file in by `updated_at`, and an entry that kept its old stamp would lose
            // to a legacy copy still holding the stale pre-reboot endpoint. Nothing changed, no
            // restamp, so a steady-state scan stays a no-op for the callers that save on change.
            if changed {
                restamp_if_newer(&mut entry.updated_at, at);
            }
        }

        // Reuse the validated transcript claims, newest mtime first. Unknown timestamps
        // were excluded above so neither new claims nor placeholder upgrades use scan time.
        candidates.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        for (file_ms, conv_id, persona) in candidates {
            if !self.entries.values().any(|existing| {
                matches!(
                    &existing.route,
                    VendorRoute::Agy {
                        conversation_id: existing_id,
                        ..
                    } if existing_id == &conv_id
                )
            }) {
                let placeholder = match self.entries.get(&persona) {
                    Some(entry) => match &entry.route {
                        VendorRoute::Agy {
                            conversation_id,
                            ls_pid,
                            ls_address,
                            csrf_token,
                            agentapi_exe,
                        } if !is_native_session_id(conversation_id) => {
                            let placeholder_ms = if entry.explicit {
                                entry.updated_at.unix_millis()
                            } else {
                                entry
                                    .identity_evidence_at()
                                    .map_or(entry.updated_at.unix_millis(), At::unix_millis)
                            };
                            if file_ms < placeholder_ms {
                                continue;
                            }
                            Some((
                                *ls_pid,
                                ls_address.clone(),
                                csrf_token.clone(),
                                agentapi_exe.clone(),
                                entry.updated_at.clone(),
                                entry.explicit,
                            ))
                        }
                        _ if entry.explicit => continue,
                        _ => None,
                    },
                    None => None,
                };
                if let Some((
                    existing_pid,
                    existing_addr,
                    existing_csrf,
                    existing_exe,
                    mut upgraded_at,
                    explicit_placeholder,
                )) = placeholder
                {
                    let accepts_shared =
                        agy_route_accepts_shared_endpoint(existing_pid, shared_ls_pid, check_proc);
                    let route = VendorRoute::Agy {
                        conversation_id: conv_id,
                        ls_pid: existing_pid,
                        ls_address: existing_addr
                            .or_else(|| accepts_shared.then(|| shared_ls_addr.clone()).flatten()),
                        csrf_token: existing_csrf
                            .or_else(|| accepts_shared.then(|| shared_csrf.clone()).flatten()),
                        agentapi_exe: existing_exe.or_else(|| Some(resolved_shared.clone())),
                    };
                    let file_at = At::from_unix_millis(file_ms);
                    restamp_if_newer(&mut upgraded_at, &file_at);
                    self.upsert_inner(
                        &persona,
                        route,
                        upgraded_at,
                        Some(file_at),
                        explicit_placeholder,
                    );
                } else {
                    let accepts_shared =
                        agy_route_accepts_shared_endpoint(None, shared_ls_pid, check_proc);
                    let route = VendorRoute::Agy {
                        conversation_id: conv_id,
                        ls_pid: None,
                        ls_address: accepts_shared.then(|| shared_ls_addr.clone()).flatten(),
                        csrf_token: accepts_shared.then(|| shared_csrf.clone()).flatten(),
                        agentapi_exe: Some(resolved_shared.clone()),
                    };
                    self.upsert_discovered(&persona, route, file_ms, at);
                }
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
                // then the index mtime.
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
        self.discover_muse_routes_in(&dirs, None, at);
    }

    /// Ties Muse routes to the live tmux windows (see
    /// [`attach_muse_tmux_windows`](Self::attach_muse_tmux_windows)). Does nothing when tmux
    /// discovery is off or tmux's answer cannot be trusted.
    fn discover_muse_windows(&mut self, home: &Path, has_home_override: bool, at: &At) {
        if !muse_tmux_discovery_enabled(home, has_home_override) {
            return;
        }
        if let Some(windows) = list_tmux_windows() {
            self.attach_muse_tmux_windows(&windows, at);
        }
    }

    /// Muse routes from the transcripts under each of `sessions_dirs`, then (when `windows` is
    /// `Some`: the live tmux `session:window` pairs) the tmux targets of those routes.
    fn discover_muse_routes_in(
        &mut self,
        sessions_dirs: &[PathBuf],
        windows: Option<&[TmuxWindow]>,
        at: &At,
    ) {
        for sessions_dir in sessions_dirs {
            for transcript in muse_transcripts(sessions_dir) {
                let path = &transcript.path;
                let Some(persona) = scan_muse_transcript_for_persona(path, 50) else {
                    continue;
                };
                let (session_id, cwd) = match &transcript.dir_session_id {
                    // The session directory's name is the id; a row's own `id` is not.
                    Some(id) => (id.clone(), extract_session_id_and_cwd(path, id).1),
                    None => extract_session_id_and_cwd(path, &transcript.stem),
                };
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
                // A transcript replaces the placeholder route a live tmux window made before
                // the session had one, whatever the two timestamps say.
                let explicit_placeholder = self.entries.get(&persona).is_some_and(|entry| {
                    entry.explicit && matches!(&entry.route, VendorRoute::Muse { session_id, .. } if session_id.is_empty())
                });
                if !explicit_placeholder
                    && matches!(
                        self.entries.get(&persona),
                        Some(RouteEntry {
                            route: VendorRoute::Muse { session_id, .. },
                            ..
                        }) if session_id.is_empty()
                    )
                {
                    self.entries.remove(&persona);
                }
                let route = VendorRoute::Muse {
                    session_id,
                    cwd,
                    tmux_target: existing_target,
                    tmux_pane: existing_pane,
                };
                if explicit_placeholder {
                    let updated = mtime_ms(path);
                    let file_at = if updated > 0 {
                        At::from_unix_millis(updated)
                    } else {
                        at.clone()
                    };
                    self.upsert_passive(&persona, route, file_at);
                } else {
                    self.upsert_discovered(&persona, route, mtime_ms(path), at);
                }
            }
        }
        if let Some(windows) = windows {
            self.attach_muse_tmux_windows(windows, at);
        }
    }

    /// Tie Muse routes to the live tmux windows a Muse agent runs in. Two forms are recognised:
    /// `muse:<persona>` (a window of the shared `muse` session, the form `muse-sessions` and the
    /// lifecycle use) and `<Session>:muse` (a session named by the capitalised persona). When a
    /// persona has both, `muse:<persona>` is preferred. A window of the shared session whose name
    /// is not a valid persona (a window literally named `muse` included) is skipped, as is a
    /// `<Session>:muse` whose session is not.
    ///
    /// - A route's tmux target is kept exactly when that window is live (any listed
    ///   `session:window`); otherwise it is replaced by the persona's live window, or cleared
    ///   when there is none. So `muse:<persona>` is a legitimate stored target, and a stale one
    ///   goes when its window does. The caller passes a window list only when tmux answered, so
    ///   a tmux failure never reaches here and leaves every target alone.
    /// - A live window whose persona has no route at all gets a placeholder Muse route with an
    ///   empty session id and no cwd, so a prompt can reach Muse before its first transcript
    ///   exists. The route is only a tmux address: it never matches a session id, and a
    ///   delivery on it fails unless the window is alive (see `deliver_to_muse_route`).
    /// - A placeholder with no live window is dropped.
    fn attach_muse_tmux_windows(&mut self, windows: &[TmuxWindow], at: &At) {
        let mut shared: BTreeMap<String, String> = BTreeMap::new();
        let mut per_persona: BTreeMap<String, String> = BTreeMap::new();
        let mut live_targets: BTreeSet<String> = BTreeSet::new();
        for (session, window) in windows {
            live_targets.insert(format!("{session}:{window}"));
            // The shared session's windows are named by persona; any other session names the
            // persona and holds a window called `muse`.
            let (persona, found) = if session == "muse" {
                (normalize_claude_session_name(window), &mut shared)
            } else if window == "muse" {
                (normalize_claude_session_name(session), &mut per_persona)
            } else {
                continue;
            };
            let Some(persona) = persona.filter(|p| p != "muse") else {
                continue;
            };
            found
                .entry(persona)
                .or_insert_with(|| format!("{session}:{window}"));
        }
        let mut live: BTreeMap<String, String> = per_persona;
        live.extend(shared);
        let mut gone = Vec::new();
        for (persona, entry) in &mut self.entries {
            let VendorRoute::Muse {
                session_id,
                tmux_target,
                ..
            } = &mut entry.route
            else {
                continue;
            };
            if tmux_target
                .as_deref()
                .is_some_and(|t| live_targets.contains(t))
            {
                continue;
            }
            match live.get(persona) {
                Some(target) => *tmux_target = Some(target.clone()),
                None if session_id.is_empty() && !entry.explicit => gone.push(persona.clone()),
                None => *tmux_target = None,
            }
        }
        for persona in gone {
            self.entries.remove(&persona);
        }
        for (persona, target) in live {
            if !self.entries.contains_key(&persona) {
                self.upsert_passive_without_identity(
                    &persona,
                    VendorRoute::Muse {
                        session_id: String::new(),
                        cwd: None,
                        tmux_target: Some(target),
                        tmux_pane: None,
                    },
                    at.clone(),
                );
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
                if transcript_is_grok_build_subagent(&path) || grok_flat_file_is_headless(&path) {
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
            // A `grok -p` one-shot is a command, never an agent session.
            if grok_summary_is_headless(&summary_path) {
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
                self.upsert_passive(&cand.persona, route, at.clone());
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

    fn attach_grok_tmux_placeholders(&mut self, windows: &[TmuxWindow], at: &At) {
        for (session, window) in windows {
            if window != "grok" {
                continue;
            }
            let Some(persona) = normalize_claude_session_name(session) else {
                continue;
            };
            if persona == "grok" || self.entries.contains_key(&persona) {
                continue;
            }
            self.upsert_passive_without_identity(
                &persona,
                VendorRoute::Grok {
                    session_id: String::new(),
                    cwd: None,
                    tmux_target: Some(format!("{session}:grok")),
                    tmux_pane: None,
                },
                at.clone(),
            );
        }
    }

    fn discover_host_cli_sessions(&mut self, at: &At) {
        let Ok(endpoint) = fl_host_proto::HostEndpoint::discover() else {
            return;
        };
        let Ok(mut client) = fl_host_proto::BlockingHostClient::connect(&endpoint) else {
            return;
        };
        let Ok(sessions) = client.list_sessions() else {
            return;
        };
        for s in sessions {
            let fl_host_proto::ChildState::Running(rec) = &s.state else {
                continue;
            };
            let target = crate::vendor_adapter::host_session_id_to_unit(&s.stream.session);
            let pane = s.stream.session.as_str().to_string();
            let Some((session, window)) = target.split_once(':') else {
                continue;
            };
            let (persona, vendor) = if session.eq_ignore_ascii_case("muse") {
                (
                    normalize_persona(window).filter(|p| p != "muse"),
                    "muse".to_string(),
                )
            } else {
                (
                    normalize_claude_session_name(session),
                    window.trim().to_ascii_lowercase(),
                )
            };
            let Some(persona) = persona else {
                continue;
            };
            if vendor.is_empty() {
                continue;
            }
            if let Some(existing) = self.entries.get_mut(&persona) {
                match (&mut existing.route, vendor.as_str()) {
                    (
                        VendorRoute::Muse {
                            tmux_target,
                            tmux_pane,
                            ..
                        },
                        "muse",
                    )
                    | (
                        VendorRoute::Grok {
                            tmux_target,
                            tmux_pane,
                            ..
                        },
                        "grok",
                    ) => {
                        if tmux_target.is_none() {
                            *tmux_target = Some(target);
                        }
                        if tmux_pane.is_none() {
                            *tmux_pane = Some(pane);
                        }
                    }
                    (VendorRoute::Agy { ls_pid, .. }, "antigravity" | "agy")
                        if ls_pid
                            .is_none_or(|pid| !Path::new(&format!("/proc/{pid}")).exists()) =>
                    {
                        *ls_pid = Some(rec.pid);
                    }
                    _ => {}
                }
                continue;
            }
            let route = match vendor.as_str() {
                "muse" => VendorRoute::Muse {
                    session_id: String::new(),
                    cwd: None,
                    tmux_target: Some(target),
                    tmux_pane: Some(pane),
                },
                "grok" => VendorRoute::Grok {
                    session_id: String::new(),
                    cwd: None,
                    tmux_target: Some(target),
                    tmux_pane: Some(pane),
                },
                "antigravity" | "agy" => VendorRoute::Agy {
                    conversation_id: pane,
                    ls_pid: Some(rec.pid),
                    ls_address: None,
                    csrf_token: None,
                    agentapi_exe: None,
                },
                "claude" => VendorRoute::Claude {
                    pid: rec.pid,
                    session_id: pane,
                    socket_path: PathBuf::new(),
                    key_path: None,
                },
                "codex" => VendorRoute::Codex {
                    thread_id: pane,
                    cwd: None,
                },
                _ => VendorRoute::HostCli {
                    vendor,
                    session_id: pane.clone(),
                    cwd: None,
                    tmux_target: Some(target),
                    tmux_pane: Some(pane),
                },
            };
            self.upsert_passive_without_identity(&persona, route, at.clone());
        }
    }
}

/// `true` when `route` names a decommissioned native session id.
/// Webhook routes are not vendor sessions. An empty id and a persona-derived id
/// (`vendor:persona`, `grok-ada`, `claude-ada-1234`) match nothing.
fn route_is_decommissioned(route: &VendorRoute, banned: &BTreeSet<String>) -> bool {
    matches!(
        route.session_identity(),
        Some((_, id)) if is_native_session_id(id) && banned.contains(id)
    )
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

/// True when a parsed Grok summary (or session metadata object) marks a headless `grok -p`
/// one-shot. Grok writes `session_kind` at the top level of `summary.json` (interactive
/// sessions omit it; sub-agents carry `"subagent"`); `/info/session_kind` is read as well.
fn grok_value_is_headless(val: &serde_json::Value) -> bool {
    ["/session_kind", "/info/session_kind", "/sessionKind"]
        .iter()
        .filter_map(|ptr| val.pointer(ptr).and_then(|v| v.as_str()))
        .any(|kind| kind.trim().eq_ignore_ascii_case("headless"))
}

/// True when the Grok `summary.json` at `summary_path` says the session is headless
/// (`grok -p`). A missing or unreadable file is not headless: the summary may not exist yet at
/// `session_start`.
pub fn grok_summary_is_headless(summary_path: &Path) -> bool {
    fs::read_to_string(summary_path)
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .is_some_and(|val| grok_value_is_headless(&val))
}

/// Most bytes read from a flat-layout Grok session file per discovery pass.
const GROK_FLAT_FILE_READ_CAP: u64 = 1 << 20;

/// Flat-layout session file (`sessions/<id>.json` or `.jsonl`): the object itself, or the first
/// line of a JSONL file, carries the metadata.
fn grok_flat_file_is_headless(path: &Path) -> bool {
    use std::io::{BufRead, BufReader, Read};
    let Ok(file) = fs::File::open(path) else {
        return false;
    };
    let mut head = String::new();
    if BufReader::new(file.take(GROK_FLAT_FILE_READ_CAP))
        .read_line(&mut head)
        .is_err()
    {
        return false;
    }
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(head.trim())
        && grok_value_is_headless(&val)
    {
        return true;
    }
    // A pretty-printed `.json` object spans many lines. The read is capped like the first line:
    // a larger file is cut mid-object, fails to parse and counts as not headless.
    path.extension().and_then(|e| e.to_str()) == Some("json")
        && fs::File::open(path).is_ok_and(|file| {
            let mut raw = String::new();
            file.take(GROK_FLAT_FILE_READ_CAP)
                .read_to_string(&mut raw)
                .ok()
                .and_then(|_| serde_json::from_str::<serde_json::Value>(&raw).ok())
                .is_some_and(|val| grok_value_is_headless(&val))
        })
}

/// True when the Grok session `session_id` is headless. The summary is looked up next to
/// `transcript_path` when given, then under `~/.grok/sessions/<id>/` and
/// `~/.grok/sessions/*/<id>/`.
pub fn grok_session_is_headless(
    home: &Path,
    session_id: &str,
    transcript_path: Option<&Path>,
) -> bool {
    if let Some(dir) = transcript_path.and_then(Path::parent)
        && grok_summary_is_headless(&dir.join("summary.json"))
    {
        return true;
    }
    let sid = session_id.trim();
    // Reject ids that could climb out of the sessions tree.
    if sid.is_empty() || sid.contains(['/', '\\']) || sid == "." || sid == ".." {
        return false;
    }
    let sessions_dir = home.join(".grok").join("sessions");
    if grok_summary_is_headless(&sessions_dir.join(sid).join("summary.json")) {
        return true;
    }
    let Ok(read_dir) = fs::read_dir(&sessions_dir) else {
        return false;
    };
    read_dir.flatten().any(|entry| {
        let path = entry.path();
        path.is_dir() && grok_summary_is_headless(&path.join(sid).join("summary.json"))
    })
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

/// A Codex subagent rollout is not its own session. `source.subagent` is an object,
/// `thread_source` is `"subagent"`, or `parent_thread_id` is present.
fn codex_rollout_is_subagent(val: &serde_json::Value) -> bool {
    val.pointer("/payload/source/subagent")
        .is_some_and(serde_json::Value::is_object)
        || val
            .pointer("/payload/thread_source")
            .and_then(|v| v.as_str())
            == Some("subagent")
        || val.pointer("/payload/parent_thread_id").is_some()
}

fn extract_codex_rollout_meta(path: &Path) -> (String, Option<PathBuf>) {
    let fallback_stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let mut thread_id = String::new();
    let mut cwd = None;
    if let Some(text) = read_head_lines(path, 20) {
        for line in text.lines().take(20) {
            let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if codex_rollout_is_subagent(&val) {
                return (String::new(), None);
            }
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

/// A live tmux window: `(session name, window name)`.
type TmuxWindow = (String, String);

/// `true` when Muse discovery may ask tmux which windows are live: host discovery is on, and the
/// scan covers the host (no home override). A scan of the home override `home` asks only when a
/// test opts in with `FL_WIRE_TMUX_DISCOVERY=<home>` (so other homes scanned meanwhile are not
/// affected) and names a fake tmux in `FL_WIRE_TMUX_BIN`; it never reaches the real one.
fn muse_tmux_discovery_enabled(home: &Path, has_home_override: bool) -> bool {
    if std::env::var("FL_WIRE_DISABLE_HOST_DISCOVERY").as_deref() == Ok("1") {
        return false;
    }
    !has_home_override
        || (std::env::var_os("FL_WIRE_TMUX_DISCOVERY").is_some_and(|h| Path::new(&h) == home)
            && (std::env::var_os("FL_WIRE_TMUX_BIN").is_some()
                || std::env::var_os("FL_HOST_SOCK").is_some()))
}

/// How long [`list_tmux_windows`] waits for tmux.
const TMUX_LIST_TIMEOUT: Duration = Duration::from_secs(3);

/// Every live tmux window (`tmux list-windows -a`), or `None` when tmux's answer is not
/// trustworthy: it cannot be started, exits non-zero (no server running included) or takes more
/// than a few seconds. A caller must then leave what it knows about windows untouched; an empty
/// list means tmux answered that there are no windows. Uses the tmux binary the dispatcher uses.
fn list_tmux_windows() -> Option<Vec<TmuxWindow>> {
    list_tmux_windows_within(TMUX_LIST_TIMEOUT)
}

fn list_tmux_windows_within(timeout: Duration) -> Option<Vec<TmuxWindow>> {
    if crate::dispatcher::host_discovery_allowed()
        && let Ok(endpoint) = fl_host_proto::HostEndpoint::discover()
        && let Ok(mut client) = fl_host_proto::BlockingHostClient::connect(&endpoint)
        && let Ok(sessions) = client.list_sessions()
    {
        let lines: String = sessions
            .iter()
            .filter(|s| matches!(s.state, fl_host_proto::ChildState::Running(_)))
            .map(|s| crate::vendor_adapter::host_session_id_to_unit(&s.stream.session))
            .collect::<Vec<_>>()
            .join("\n");
        let windows = parse_tmux_windows(&lines);
        return Some(windows);
    }
    if std::env::var_os("FL_WIRE_TMUX_BIN").is_some() {
        return run_tmux_list_windows(timeout);
    }
    None
}

fn run_tmux_list_windows(timeout: Duration) -> Option<Vec<TmuxWindow>> {
    use std::io::Read;
    use std::process::{Command, Stdio};

    let mut child = Command::new(crate::dispatcher::tmux_bin())
        .args(["list-windows", "-a", "-F", "#{session_name}:#{window_name}"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // Drain stdout on its own thread, so a full pipe cannot hold tmux until the timeout.
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes);
        let _ = tx.send(bytes);
    });
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(_)) => return None,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let bytes = rx
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .ok()?;
    Some(parse_tmux_windows(&String::from_utf8_lossy(&bytes)))
}

/// `session:window` lines; tmux session names hold no `:`, so the first one splits the line.
fn parse_tmux_windows(text: &str) -> Vec<TmuxWindow> {
    text.lines()
        .filter_map(|line| line.trim().split_once(':'))
        .filter(|(session, window)| !session.is_empty() && !window.is_empty())
        .map(|(session, window)| (session.to_string(), window.to_string()))
        .collect()
}

/// A Muse transcript file found under a `sessions` directory.
struct MuseTranscript {
    path: PathBuf,
    /// File stem (the session id of the flat `<id>.jsonl` form).
    stem: String,
    /// The session directory's name, for the nested `.../<id>/session.jsonl` form.
    dir_session_id: Option<String>,
}

/// The most transcript files one Muse scan reads, and the deepest a file may lie below `sessions`
/// (`YYYY/MM/DD/<id>/session.jsonl` is 5).
const MUSE_MAX_TRANSCRIPTS: usize = 2000;
const MUSE_MAX_DEPTH: usize = 5;

/// Locate the transcript file on disk for a Muse session by explicit `session_id` or `persona`.
///
/// When a non-empty `session_id` is supplied, only an exact session-id match is returned; it does
/// not fall back to the newest transcript for `persona`, which may belong to another session. The
/// `persona` mtime fallback is used only when `session_id` is `None` or empty.
pub(crate) fn find_muse_transcript_path(
    home: &Path,
    session_id: Option<&str>,
    persona: Option<&str>,
) -> Option<PathBuf> {
    let mut dirs = vec![
        home.join(".local")
            .join("share")
            .join("muse")
            .join("sessions"),
    ];
    if std::env::var_os("HOME").is_some_and(|h| Path::new(&h) == home)
        && let Some(xdg) = std::env::var_os("XDG_DATA_HOME")
    {
        let xdg_dir = PathBuf::from(xdg).join("muse").join("sessions");
        if !dirs.contains(&xdg_dir) {
            dirs.push(xdg_dir);
        }
    }
    let mut transcripts: Vec<MuseTranscript> =
        dirs.iter().flat_map(|dir| muse_transcripts(dir)).collect();
    if transcripts.is_empty() {
        return None;
    }
    if let Some(sid) = session_id.map(str::trim).filter(|s| !s.is_empty()) {
        return transcripts
            .iter()
            .find(|t| {
                t.dir_session_id.as_deref() == Some(sid)
                    || t.stem == sid
                    || extract_session_id_and_cwd(&t.path, &t.stem).0 == sid
            })
            .map(|t| t.path.clone());
    }
    if let Some(norm_persona) = persona.and_then(normalize_persona) {
        transcripts.sort_by_key(|t| std::cmp::Reverse(mtime_ms(&t.path)));
        if let Some(found) = transcripts.iter().find(|t| {
            t.dir_session_id
                .as_deref()
                .and_then(normalize_persona)
                .as_deref()
                == Some(norm_persona.as_str())
                || normalize_persona(&t.stem).as_deref() == Some(norm_persona.as_str())
                || scan_muse_transcript_for_persona(&t.path, 50).as_deref()
                    == Some(norm_persona.as_str())
        }) {
            return Some(found.path.clone());
        }
    }
    None
}

/// Muse transcripts under `sessions_dir`, newest names first: the flat `<id>.jsonl` form and the
/// nested `YYYY/MM/DD/<id>/session.jsonl` form. Symlinks are not followed, dot-directories
/// (`.msp-view-*`) and anything below a `subagent` directory are skipped, and the scan is bounded
/// by [`MUSE_MAX_DEPTH`] and [`MUSE_MAX_TRANSCRIPTS`].
fn muse_transcripts(sessions_dir: &Path) -> Vec<MuseTranscript> {
    let mut found = Vec::new();
    let mut stack = vec![(sessions_dir.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(read_dir) = fs::read_dir(&dir) else {
            continue;
        };
        let mut entries: Vec<_> = read_dir.flatten().collect();
        entries.sort_by_key(|e| std::cmp::Reverse(e.file_name()));
        let mut subdirs = Vec::new();
        for entry in entries {
            // `file_type` does not follow a symlink: a link is neither a file nor a directory.
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let name = entry.file_name().to_string_lossy().to_string();
            let path = entry.path();
            if kind.is_dir() {
                if depth + 1 < MUSE_MAX_DEPTH
                    && !name.starts_with('.')
                    && !name.to_ascii_lowercase().contains("subagent")
                {
                    subdirs.push((path, depth + 1));
                }
            } else if kind.is_file() {
                let stem = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default()
                    .to_string();
                let transcript = if depth == 0 {
                    (path.extension().and_then(|s| s.to_str()) == Some("jsonl") && !stem.is_empty())
                        .then_some(None)
                } else {
                    (name == "session.jsonl")
                        .then(|| dir.file_name().and_then(|n| n.to_str()))
                        .flatten()
                        .filter(|id| !id.is_empty())
                        .map(|id| Some(id.to_string()))
                };
                if let Some(dir_session_id) = transcript {
                    found.push(MuseTranscript {
                        path,
                        stem,
                        dir_session_id,
                    });
                    if found.len() >= MUSE_MAX_TRANSCRIPTS {
                        return found;
                    }
                }
            }
        }
        // Largest name last, so the stack pops it first.
        stack.extend(subdirs.into_iter().rev());
    }
    found
}

fn extract_session_id_and_cwd(path: &Path, fallback_stem: &str) -> (String, Option<PathBuf>) {
    let mut session_id = fallback_stem.to_string();
    let mut cwd = None;
    if let Some(text) = read_head_lines(path, 20) {
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
        "host_cli" | "lemonade" | "ollama" => {
            let vendor = item
                .get("vendor")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(vendor)
                .to_ascii_lowercase();
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
            Some(VendorRoute::HostCli {
                vendor,
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

/// The first row of an Antigravity subagent transcript (`invoke_subagent`).
fn first_message_is_agy_subagent(text: &str) -> bool {
    for line in text.lines().take(50) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            return false;
        };
        if val.get("type").and_then(|v| v.as_str()) == Some("SYSTEM_MESSAGE") {
            let content = row_text(&val).unwrap_or_default();
            return content.contains("<SYSTEM_MESSAGE>");
        }
        return false;
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
        .or_else(|| {
            val.pointer("/payload/model_messages")
                .and_then(muse_user_messages_text)
        })
        .or_else(|| val.get("text").and_then(extract_text_from_json_content))
        .or_else(|| val.get("output").and_then(extract_text_from_json_content))
}

/// The text of the user-side messages in a Muse `runtime.user_intent.accepted` row's
/// `payload.model_messages` (a message with no `role` counts as the user's). A Muse session
/// records the prompt it was started with in this row.
fn muse_user_messages_text(messages: &serde_json::Value) -> Option<String> {
    let parts: Vec<String> = messages
        .as_array()?
        .iter()
        .filter(|m| {
            m.get("role")
                .and_then(|r| r.as_str())
                .is_none_or(|r| r.eq_ignore_ascii_case("user"))
        })
        .filter_map(|m| m.get("content").and_then(extract_text_from_json_content))
        .collect();
    (!parts.is_empty()).then(|| parts.join("\n"))
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
        Some(
            "tool_result"
                | "TOOL_RESULT"
                | "custom_tool_call_output"
                | "function_call_output"
                | "GENERIC"
                | "generic"
        )
    )
}

/// Retain the bounded scan's rejection reason without reading the source a second time.
fn passive_agy_source(path: &Path) -> Result<(u64, String), &'static str> {
    let metadata = fs::metadata(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            "source missing"
        } else {
            "source unreadable"
        }
    })?;
    if !metadata.is_file() {
        return Err("source unreadable (not a regular file)");
    }
    let file_ms = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0);
    if file_ms == 0 {
        return Err("source mtime is zero or unknown");
    }
    let text = read_head_lines(path, 50).ok_or("source unreadable")?;
    let persona =
        scan_text_for_persona(&text, 50).ok_or("no supported identity in bounded source head")?;
    Ok((file_ms, persona))
}

pub fn scan_transcript_for_persona(path: &Path, max_lines: usize) -> Option<String> {
    scan_text_for_persona(&read_head_lines(path, max_lines)?, max_lines)
}

/// The most bytes [`read_head_lines`] reads for a transcript head.
const TRANSCRIPT_HEAD_MAX_BYTES: u64 = 1 << 20;

/// The first `max_lines` lines of `path` (at most [`TRANSCRIPT_HEAD_MAX_BYTES`] of it), without
/// reading the rest of a transcript that may be many megabytes.
fn read_head_lines(path: &Path, max_lines: usize) -> Option<String> {
    use std::io::{BufRead, Read};

    let file = fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file.take(TRANSCRIPT_HEAD_MAX_BYTES));
    let mut text = String::new();
    let mut line = Vec::new();
    for _ in 0..max_lines {
        line.clear();
        if reader.read_until(b'\n', &mut line).ok()? == 0 {
            break;
        }
        text.push_str(&String::from_utf8_lossy(&line));
    }
    Some(text)
}

/// [`scan_transcript_for_persona`] for a Muse transcript: reads only the head of the file.
fn scan_muse_transcript_for_persona(path: &Path, max_lines: usize) -> Option<String> {
    scan_text_for_persona(&read_head_lines(path, max_lines)?, max_lines)
}

fn scan_text_for_persona(text: &str, max_lines: usize) -> Option<String> {
    if first_system_message_is_grok_subagent(text) || first_message_is_agy_subagent(text) {
        return None;
    }
    let mut latest_field: Option<String> = None;
    let mut latest_registered: Option<String> = None;
    let mut first_prompt: Option<String> = None;
    for line in text.lines().take(max_lines) {
        if line.trim().is_empty() {
            continue;
        }
        let candidate_text: String = if let Ok(val) =
            serde_json::from_str::<serde_json::Value>(line)
        {
            if val.get("isMeta").and_then(|v| v.as_bool()) == Some(true)
                || val.get("isSidechain").and_then(|v| v.as_bool()) == Some(true)
                || val.get("synthetic_reason").is_some()
            {
                continue;
            }
            let t = val.get("type").and_then(|v| v.as_str());
            let pt = val.pointer("/payload/type").and_then(|v| v.as_str());
            // Tool output includes repository source and test fixtures that name
            // other personas. Reject the whole row before reading even identity fields.
            if is_tool_output_type(t) || (t == Some("response_item") && is_tool_output_type(pt)) {
                continue;
            }
            for field in ["agentName", "customTitle", "persona"] {
                if let Some(s) = val.get(field).and_then(|v| v.as_str())
                    && let Some(norm) = normalize_claude_session_name(s)
                {
                    latest_field = Some(norm);
                }
            }
            if t == Some("response_item") {
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
            "agent-msg register ",
            "fl wire register ",
            "fl agent register ",
            "take the name of ",
            "you will be ",
            "you are ",
        ] {
            let is_reg = matches!(
                marker,
                "agent-msg register " | "fl wire register " | "fl agent register "
            );
            if !is_reg && first_prompt.is_some() {
                continue;
            }
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
                        | "and"
                        | "or"
                        | "to"
                        | "with"
                        | "for"
                        | "by"
                        | "from"
                        | "of"
                        | "as"
                        | "highly"
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
                    if is_reg {
                        latest_registered = Some(norm);
                    } else {
                        first_prompt = Some(norm);
                    }
                }
            }
        }
    }
    latest_field.or(latest_registered).or(first_prompt)
}

/// The Grok route for the environment's `GROK_SESSION_ID`, or `None` when that session is a
/// headless `grok -p` one-shot (its id must never reach the route table).
fn grok_env_route(home: Option<&Path>, session_id: String) -> Option<VendorRoute> {
    if home.is_some_and(|h| grok_session_is_headless(h, &session_id, None)) {
        return None;
    }
    Some(VendorRoute::Grok {
        session_id,
        cwd: std::env::current_dir().ok(),
        tmux_target: None,
        tmux_pane: std::env::var("TMUX_PANE")
            .ok()
            .filter(|s| !s.trim().is_empty()),
    })
}

/// Capture the current process's vendor environment route so `fl wire` / `agent-msg`
/// can register it in the route table automatically.
pub fn capture_current_env_route() -> Option<VendorRoute> {
    capture_env_route_from(&|k| std::env::var_os(k))
}

/// [`capture_current_env_route`] over an injected environment.
///
/// `ANTIGRAVITY_CONVERSATION_ID` is inherited by every subshell an Antigravity session starts,
/// so a caller there that registers a Codex, Muse or Grok agent names that session through its
/// own variable (`CODEX_THREAD_ID`, `MUSE_SESSION_ID`, `GROK_SESSION_ID`). Those explicit
/// session ids come before the inherited Antigravity one, so a real Agy session whose shell
/// also exports one of them resolves as that vendor. A `CLAUDE_SESSION_ID` without a
/// Claude messaging socket yields no Claude route, and it must not become an Antigravity one.
fn capture_env_route_from(
    get_env: &dyn Fn(&str) -> Option<std::ffi::OsString>,
) -> Option<VendorRoute> {
    let get_str = |k: &str| get_env(k).and_then(|v| v.into_string().ok());
    let set = |k: &str| get_str(k).filter(|v| !v.trim().is_empty());
    let tmux_pane = || get_str("TMUX_PANE").filter(|s| !s.trim().is_empty());
    if let Some(sock) = get_env("CLAUDE_CODE_MESSAGING_SOCKET") {
        let socket_path = PathBuf::from(sock);
        let pid = socket_path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or_else(std::process::id);
        let home = get_env("HOME").map(PathBuf::from);
        let session_id = set("CLAUDE_SESSION_ID")
            .or_else(|| {
                let sessions_dir = home.as_ref()?.join(".claude").join("sessions");
                let raw = fs::read_to_string(sessions_dir.join(format!("{pid}.json"))).ok()?;
                let val: serde_json::Value = serde_json::from_str(&raw).ok()?;
                if val
                    .get("messagingSocketPath")
                    .and_then(|v| v.as_str())
                    .map(Path::new)
                    != Some(socket_path.as_path())
                {
                    return None;
                }
                val.get("sessionId")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
            })
            .unwrap_or_default();
        let key_path = home.and_then(|h| {
            resolve_claude_key_path(&h.join(".claude").join("sessions"), pid, &socket_path)
        });
        return Some(VendorRoute::Claude {
            pid,
            session_id,
            socket_path,
            key_path,
        });
    }
    if let Some(thread_id) = set("CODEX_THREAD_ID") {
        return Some(VendorRoute::Codex {
            thread_id,
            cwd: std::env::current_dir().ok(),
        });
    }
    if let Some(session_id) = set("MUSE_SESSION_ID") {
        return Some(VendorRoute::Muse {
            session_id,
            cwd: std::env::current_dir().ok(),
            tmux_target: None,
            tmux_pane: tmux_pane(),
        });
    }
    if let Some(session_id) = set("GROK_SESSION_ID") {
        let home = get_env("HOME").map(PathBuf::from);
        // A headless `grok -p` session is not a route: fall through to the other probes.
        if let Some(route) = grok_env_route(home.as_deref(), session_id) {
            return Some(route);
        }
    }
    if set("CLAUDE_SESSION_ID").is_none()
        && let Some(conv_id) = set("ANTIGRAVITY_CONVERSATION_ID")
    {
        return Some(VendorRoute::Agy {
            conversation_id: conv_id,
            ls_pid: None,
            ls_address: get_str("ANTIGRAVITY_LS_ADDRESS"),
            csrf_token: get_str("ANTIGRAVITY_CSRF_TOKEN"),
            agentapi_exe: get_env("ANTIGRAVITY_AGENTAPI_EXE").map(PathBuf::from),
        });
    }
    if get_str("FL_WIRE_DISABLE_HOST_DISCOVERY").as_deref() != Some("1")
        && let Some(home) = get_env("HOME").map(PathBuf::from)
        && let Some((session_id, cwd, tmux_pane_found)) =
            resolve_grok_pid_session(&home, std::process::id())
    {
        return Some(VendorRoute::Grok {
            session_id,
            cwd: cwd.or_else(|| std::env::current_dir().ok()),
            tmux_target: None,
            tmux_pane: tmux_pane_found.or_else(tmux_pane),
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
    resolve_persona_traced(
        explicit,
        transcript_path,
        cwd,
        registry,
        load_routes,
        get_env,
    )
    .0
}

/// Where a resolved persona came from. A persona the caller named (flag, `AGENT_PERSONA`) or
/// that the hook session's own transcript declares is trusted to replace a route of another
/// vendor. Everything else is inferred from the surroundings: the parent-process tree,
/// inherited vendor session ids, the cwd.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersonaSource {
    Explicit,
    Env,
    Transcript,
    Inferred,
}

impl PersonaSource {
    /// True when the persona is named by the caller or by the session's own transcript.
    pub fn is_trusted(self) -> bool {
        !matches!(self, PersonaSource::Inferred)
    }
}

/// [`resolve_persona`] that also reports which source produced the persona.
pub fn resolve_persona_with_source(
    explicit: Option<&str>,
    transcript_path: Option<&Path>,
    cwd: &Path,
    registry: Option<&dyn AgentRegistry>,
    custom_routes_path: Option<&Path>,
) -> (String, PersonaSource) {
    resolve_persona_traced(
        explicit,
        transcript_path,
        cwd,
        registry,
        || RouteTable::load(custom_routes_path),
        &|k| std::env::var_os(k),
    )
}

fn resolve_persona_traced(
    explicit: Option<&str>,
    transcript_path: Option<&Path>,
    cwd: &Path,
    registry: Option<&dyn AgentRegistry>,
    load_routes: impl FnOnce() -> RouteTable,
    get_env: &dyn Fn(&str) -> Option<std::ffi::OsString>,
) -> (String, PersonaSource) {
    let get_str = |k: &str| get_env(k).and_then(|v| v.into_string().ok());
    if let Some(exp) = explicit
        && let Some(norm) = normalize_persona(exp)
    {
        return (norm, PersonaSource::Explicit);
    }

    for var in ["AGENT_PERSONA", "AGENT_NAME"] {
        if let Some(val) = get_str(var)
            && let Some(norm) = normalize_persona(&val)
        {
            return (norm, PersonaSource::Env);
        }
    }

    if let Some(tp) = transcript_path
        && let Some(norm) = scan_transcript_for_persona(tp, 50)
    {
        return (norm, PersonaSource::Transcript);
    }

    // Past this point the persona is inferred from the process tree, inherited session
    // ids or the cwd, never named by the caller.
    let inferred = |persona: String| (persona, PersonaSource::Inferred);

    if let Some(home) = get_env("HOME").map(PathBuf::from) {
        if let Some(sock) = get_env("CLAUDE_CODE_MESSAGING_SOCKET") {
            let sock_path = PathBuf::from(sock);
            if let Some(pid) = sock_path
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.parse::<u32>().ok())
                && let Some(norm) = resolve_claude_pid_persona(&home, pid, &|persona| {
                    registry.is_some_and(|reg| {
                        reg.list_agents(None, false).ok().is_some_and(|agents| {
                            agents.iter().any(|a| {
                                a.status == AgentStatus::Active
                                    && a.name_key.eq_ignore_ascii_case(persona)
                            })
                        })
                    })
                })
            {
                return inferred(norm);
            }
        }
        if let Some(norm) = resolve_grok_pid_persona(&home, std::process::id()) {
            return inferred(norm);
        }
    }

    let routes = load_routes();
    for (env_var, kind) in [
        ("CLAUDE_SESSION_ID", "claude"),
        ("CODEX_THREAD_ID", "codex"),
        ("MUSE_SESSION_ID", "muse"),
        ("GROK_SESSION_ID", "grok"),
        // Last: every subshell of an Antigravity session inherits this id, so an explicit
        // CLAUDE_SESSION_ID, CODEX_THREAD_ID, MUSE_SESSION_ID or GROK_SESSION_ID takes
        // precedence over it (same rule as `capture_env_route_from`).
        ("ANTIGRAVITY_CONVERSATION_ID", "agy"),
    ] {
        if let Some(sid) = get_str(env_var)
            && !sid.trim().is_empty()
        {
            let target = (kind, sid.trim());
            let registered_active: BTreeSet<String> = registry
                .and_then(|reg| reg.list_agents(None, false).ok())
                .map(|agents| {
                    agents
                        .into_iter()
                        .filter(|a| a.status == AgentStatus::Active)
                        .map(|a| a.name_key.to_ascii_lowercase())
                        .collect()
                })
                .unwrap_or_default();
            if let Some(entry) = routes
                .entries
                .values()
                .filter(|entry| entry.route.session_identity() == Some(target))
                .max_by_key(|entry| {
                    (
                        registered_active.contains(&entry.persona.to_ascii_lowercase()),
                        entry.updated_at.unix_millis(),
                    )
                })
            {
                return inferred(entry.persona.clone());
            }
        }
    }

    if let Some(reg) = registry
        && let Some(lead) = resolve_cwd_lead(cwd, reg)
    {
        return inferred(lead);
    }

    if let Some(persona) = resolve_cwd_persona_md(cwd) {
        return inferred(persona);
    }

    inferred("unknown".to_string())
}

/// Move `stamp` forward to `at`; a stamp never moves back.
fn restamp_if_newer(stamp: &mut At, at: &At) {
    if at.unix_millis() > stamp.unix_millis() {
        *stamp = at.clone();
    }
}

/// How many `log/cli-*.log` files [`agy_log_candidates`] offers besides `cli.log`.
const AGY_LOG_CANDIDATES: usize = 8;

/// The agy logs to scan for a language-server port, best first: `cli.log` (a symlink to the
/// newest log), then the newest `log/cli-*.log` files by modification time.
fn agy_log_candidates(agy_dir: &Path) -> Vec<PathBuf> {
    let mut logs = vec![agy_dir.join("cli.log")];
    let mut dated: Vec<(u64, PathBuf)> = fs::read_dir(agy_dir.join("log"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("cli-") && name.ends_with(".log"))
        })
        .map(|path| (mtime_ms(&path), path))
        .collect();
    dated.sort_by(|a, b| b.cmp(a));
    let head = fs::canonicalize(&logs[0]).ok();
    logs.extend(
        dated
            .into_iter()
            .map(|(_, path)| path)
            .filter(|path| head.is_none() || fs::canonicalize(path).ok() != head)
            .take(AGY_LOG_CANDIDATES),
    );
    logs
}

/// The language-server HTTP ports a log names, newest first, that were not shut down: ports
/// logged before the last `Language server shutting down` line belong to servers that exited,
/// so a log that ends in a shutdown yields none. HTTPS (gRPC) listeners are not the endpoint.
fn agy_live_http_ports(log_text: &str) -> Vec<String> {
    const LISTENING: &str = "Language server listening on random port at ";
    let mut ports = Vec::new();
    for line in log_text.lines() {
        if line.contains("Language server shutting down") {
            ports.clear();
        } else if !line.contains("for HTTPS")
            && let Some(idx) = line.find(LISTENING)
        {
            let port: String = line[idx + LISTENING.len()..]
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if !port.is_empty() {
                ports.push(port);
            }
        }
    }
    ports.reverse();
    ports
}

/// The persona a Claude `sessions/<pid>.json` belongs to. `known_personas` says whether a name is
/// a persona the caller already knows (a route-table key, a registered agent).
fn resolve_claude_session_persona(
    home: &Path,
    val: &serde_json::Value,
    existing_entries: Option<&BTreeMap<String, RouteEntry>>,
    known_personas: &dyn Fn(&str) -> bool,
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

    // A derived name (`felix-ui-86`, from the worktree directory) names no persona. The tmux
    // session the agent runs in does when it is one: `"tmux": "Felix:@5.%5"` is
    // `<session>:<window>.<pane>`.
    if name_source == Some("derived")
        && let Some(persona) = val
            .get("tmux")
            .and_then(|v| v.as_str())
            .and_then(|tmux| tmux.split(':').next())
            .and_then(normalize_claude_session_name)
        && known_personas(&persona)
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

fn resolve_claude_pid_persona(
    home: &Path,
    start_pid: u32,
    known_personas: &dyn Fn(&str) -> bool,
) -> Option<String> {
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
            && let Some(norm) = resolve_claude_session_persona(home, &val, None, known_personas)
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
                if grok_session_is_headless(home, &sid, None) {
                    return None;
                }
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
            if grok_summary_is_headless(&dir.join("summary.json")) {
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
                if !candidate
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_uppercase())
                {
                    continue;
                }
                let lower = candidate.to_ascii_lowercase();
                if matches!(
                    lower.as_str(),
                    "the"
                        | "a"
                        | "an"
                        | "highly"
                        | "resident"
                        | "senior"
                        | "working"
                        | "running"
                        | "interactive"
                ) {
                    continue;
                }
                if let Some(norm) = normalize_persona(&candidate) {
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
        // A virtual group (an area with no path) is not a directory: no cwd is "in" it.
        for area in areas.iter().filter(|a| !a.is_virtual()) {
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
    fn a_virtual_group_is_not_a_directory_so_its_lead_is_not_a_cwd_lead() {
        let store = MemStore::default();
        for (area, lead, project) in [
            ("council", "Cora", None),
            ("alpha", "Alma", Some("/work/alpha".to_string())),
        ] {
            store
                .upsert_area(AreaRecord {
                    area_key: area.into(),
                    name: area.into(),
                    path: project,
                    purpose: "p".into(),
                    updated_at: At::from_unix_millis(10),
                    by: "owner".into(),
                })
                .unwrap();
            store
                .register_agent(RegisterAgentArgs {
                    display_name: lead.into(),
                    email: None,
                    project: area.into(),
                    role: AgentRole::Lead,
                    extra_areas: vec![],
                    create_area: false,
                    replace: false,
                    at: At::from_unix_millis(11),
                    by: "owner".into(),
                })
                .unwrap();
        }
        // A directory that happens to be named like the group is not in it.
        assert_eq!(
            resolve_cwd_lead(Path::new("/elsewhere/council/src"), &store),
            None
        );
        // A directory-backed area is matched by name, as before.
        assert_eq!(
            resolve_cwd_lead(Path::new("/elsewhere/alpha/src"), &store).as_deref(),
            Some("alma")
        );
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

        let mut legacy_table =
            RouteTable::load_file(&legacy_path).expect("should parse legacy routes");
        let ferris_legacy = legacy_table.get("ferris").unwrap();
        let ferris_legacy_at = At::parse("2026-10-27T16:00:00.000Z").unwrap();
        assert_eq!(ferris_legacy.updated_at, ferris_legacy_at);
        assert_eq!(ferris_legacy.identity_at, None);

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

        // Before revalidation, a legacy passive entry without `identity_at` cannot outrank
        // a route with known identity evidence merely via `updated_at`.
        primary.merge_newer(legacy_table.clone());
        assert!(matches!(
            primary.get("ferris").map(|e| &e.route),
            Some(VendorRoute::Agy { conversation_id, .. }) if conversation_id == "conv-stale-subagent"
        ));

        agy_registration_fixture(
            tmp.path(),
            "conv-live-new",
            "Ferris",
            ferris_legacy_at.unix_millis(),
        );
        legacy_table.discover_agy_routes(
            tmp.path(),
            false,
            &At::parse("2026-10-27T16:05:00.000Z").unwrap(),
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
        agy_registration_fixture(home, "conv-kayla", "Kayla", 100);
        agy_registration_fixture(home, "conv-ferris", "Ferris", 100);
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
    fn a_passive_claude_scan_does_not_replace_an_explicit_codex_binding() {
        let home = TempDir::new().unwrap();
        let sessions = home.path().join(".claude/sessions");
        fs::create_dir_all(&sessions).unwrap();
        fs::write(sessions.join("4242.json"), serde_json::json!({
            "name":"ada", "sessionId":"old-claude", "messagingSocketPath":home.path().join("old.sock")
        }).to_string()).unwrap();
        let route = VendorRoute::Codex {
            thread_id: "new-codex".into(),
            cwd: None,
        };
        let mut table = RouteTable::default();
        table.upsert_explicit("ada", route.clone(), At::from_unix_millis(1));
        table.discover_routes(Some(home.path()), At::from_unix_millis(2));
        assert_eq!(table.get("ada").map(|entry| &entry.route), Some(&route));
    }

    #[test]
    fn same_vendor_discovery_refreshes_and_preserves_explicit_binding() {
        let mut table = RouteTable::default();
        table.upsert_explicit(
            "ada",
            VendorRoute::Codex {
                thread_id: "old".into(),
                cwd: None,
            },
            At::from_unix_millis(1),
        );
        table.upsert_discovered(
            "ada",
            VendorRoute::Codex {
                thread_id: "new".into(),
                cwd: None,
            },
            2,
            &At::from_unix_millis(2),
        );
        let entry = table.get("ada").unwrap();
        assert!(entry.explicit);
        assert!(matches!(&entry.route, VendorRoute::Codex { thread_id, .. } if thread_id == "new"));
        let encoded = serde_json::to_string(&table).unwrap();
        let restored: RouteTable = serde_json::from_str(&encoded).unwrap();
        assert!(restored.get("ada").unwrap().explicit);
    }

    #[test]
    fn legacy_route_without_explicit_marker_still_loads() {
        let raw = serde_json::json!({"entries":{"ada":{"persona":"ada","route":{"kind":"codex","thread_id":"old"},"updated_at":At::from_unix_millis(1)}}});
        let table: RouteTable = serde_json::from_value(raw).unwrap();
        assert!(!table.get("ada").unwrap().explicit);
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
    fn refresh_persona_reports_a_new_or_replaced_session_only_when_the_route_changed() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let sessions = home.join(".claude").join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        let write_session = |pid: u32| {
            fs::write(
                sessions.join(format!("{pid}.json")),
                serde_json::json!({
                    "name": "Wendy",
                    "nameSource": "user",
                    "sessionId": format!("sess-{pid}"),
                    "messagingSocketPath": home.join(format!("{pid}.sock")),
                })
                .to_string(),
            )
            .unwrap();
        };
        let at = |ms| At::from_unix_millis(ms);
        let mut table = RouteTable::default();

        assert!(
            !table.refresh_persona("wendy", Some(home), at(10)),
            "nothing to find yet"
        );
        write_session(111);
        assert!(
            table.refresh_persona("wendy", Some(home), at(20)),
            "a new session appears"
        );
        assert!(
            !table.refresh_persona("wendy", Some(home), at(30)),
            "same route: unchanged"
        );
        fs::remove_file(sessions.join("111.json")).unwrap();
        write_session(222);
        assert!(
            table.refresh_persona("wendy", Some(home), at(40)),
            "a replaced session"
        );
        assert!(matches!(
            table.get("wendy").map(|e| &e.route),
            Some(VendorRoute::Claude { pid: 222, .. })
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
    fn scan_rejects_generic_output_identity_text() {
        for kind in ["GENERIC", "generic"] {
            for content in [
                "agent-msg register Kayla",
                "diff --git a/example b/example\n+fl wire register Kayla --replace\n",
                "Showing lines 1-20 of example\nYou are Kayla",
                "Command output:\n\"Take the name of Kayla\"\n",
            ] {
                let row = serde_json::json!({"type": kind, "content": content});
                assert_eq!(scan_text_for_persona(&row.to_string(), 50), None);
            }
        }
    }

    #[test]
    fn scan_rejects_tool_output_identity_fields_before_reading_them() {
        for kind in [
            "GENERIC",
            "generic",
            "tool_result",
            "TOOL_RESULT",
            "custom_tool_call_output",
            "function_call_output",
        ] {
            for field in ["agentName", "customTitle", "persona"] {
                let mut output = serde_json::json!({"type": kind, "content": "completed"});
                output[field] = serde_json::json!("Kayla");
                assert_eq!(scan_text_for_persona(&output.to_string(), 50), None);

                let mut wrapped = serde_json::json!({
                    "type": "response_item",
                    "payload": output,
                });
                wrapped[field] = serde_json::json!("Kayla");
                assert_eq!(scan_text_for_persona(&wrapped.to_string(), 50), None);
            }
        }
    }

    #[test]
    fn scan_preserves_agy_bootstrap_and_command_input_despite_output_identity() {
        for legitimate in [
            serde_json::json!({
                "type": "USER_INPUT",
                "content": "Take the name of Fiona and take up residence as the assistant.",
            }),
            serde_json::json!({
                "type": "PLANNER_RESPONSE",
                "tool_calls": [{
                    "name": "run_command",
                    "args": {"CommandLine": "fl wire register Fiona --project ferroloop --role assistant --replace"},
                }],
            }),
        ] {
            let text = [
                legitimate.to_string(),
                serde_json::json!({
                    "type": "GENERIC",
                    "content": "diff --git a/example b/example\n+agent-msg register Kayla\n",
                    "persona": "Kayla",
                })
                .to_string(),
                serde_json::json!({
                    "type": "response_item",
                    "agentName": "Kayla",
                    "payload": {"type": "generic", "content": "You are Kayla"},
                })
                .to_string(),
            ]
            .join("\n");
            assert_eq!(scan_text_for_persona(&text, 50).as_deref(), Some("fiona"));
        }
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
        assert_eq!(scan_transcript_for_persona(&generic, 20), None);

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

    fn write_grok_summary(dir: &Path, sid: &str, extra: serde_json::Value) {
        fs::create_dir_all(dir).unwrap();
        let mut val = serde_json::json!({
            "info": {"id": sid, "cwd": "/tmp"},
            "session_summary": "Wendy registers as lead",
            "updated_at": "2026-10-29T03:00:00.000Z"
        });
        if let (Some(base), Some(add)) = (val.as_object_mut(), extra.as_object()) {
            for (k, v) in add {
                base.insert(k.clone(), v.clone());
            }
        }
        fs::write(dir.join("summary.json"), val.to_string()).unwrap();
    }

    fn grok_route_session(table: &RouteTable, persona: &str) -> Option<String> {
        match table.get(persona).map(|e| &e.route) {
            Some(VendorRoute::Grok { session_id, .. }) => Some(session_id.clone()),
            _ => None,
        }
    }

    #[test]
    fn headless_grok_summary_is_not_a_route_but_interactive_is() {
        let at = At::parse("2026-10-29T04:00:00.000Z").unwrap();
        // Nested (`sessions/<proj>/<sid>`), top-level and `/info/` spellings.
        for (layout, extra) in [
            ("nested", serde_json::json!({"session_kind": "headless"})),
            ("flat-dir", serde_json::json!({"session_kind": "Headless"})),
            (
                "info",
                serde_json::json!({"info": {"id": "sid-h", "cwd": "/tmp", "session_kind": "headless"}}),
            ),
        ] {
            let tmp = TempDir::new().unwrap();
            let home = tmp.path();
            let root = home.join(".grok").join("sessions");
            let dir = if layout == "flat-dir" {
                root.join("sid-h")
            } else {
                root.join("proj").join("sid-h")
            };
            write_grok_summary(&dir, "sid-h", extra);
            let mut table = RouteTable::default();
            table.discover_routes(Some(home), at.clone());
            assert!(
                table.get("wendy").is_none(),
                "{layout}: headless was routed"
            );

            // The same layout without `session_kind` is an interactive session.
            let tmp = TempDir::new().unwrap();
            let home = tmp.path();
            let root = home.join(".grok").join("sessions");
            let dir = if layout == "flat-dir" {
                root.join("sid-i")
            } else {
                root.join("proj").join("sid-i")
            };
            write_grok_summary(&dir, "sid-i", serde_json::json!({}));
            let mut table = RouteTable::default();
            table.discover_routes(Some(home), at.clone());
            assert_eq!(
                grok_route_session(&table, "wendy").as_deref(),
                Some("sid-i"),
                "{layout}: interactive session lost its route"
            );
        }
    }

    #[test]
    fn headless_grok_flat_files_are_not_routes() {
        let at = At::parse("2026-10-29T04:00:00.000Z").unwrap();
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let root = home.join(".grok").join("sessions");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("headless-1.jsonl"),
            "{\"sessionId\":\"headless-1\",\"agentName\":\"Gideon\",\"session_kind\":\"headless\"}\n",
        )
        .unwrap();
        fs::write(
            root.join("headless-2.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "sessionId": "headless-2", "agentName": "Hector", "session_kind": "headless"
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(
            root.join("live-1.jsonl"),
            "{\"sessionId\":\"live-1\",\"agentName\":\"Iris\"}\n",
        )
        .unwrap();
        let mut table = RouteTable::default();
        table.discover_routes(Some(home), at);
        assert!(table.get("gideon").is_none());
        assert!(table.get("hector").is_none());
        assert_eq!(
            grok_route_session(&table, "iris").as_deref(),
            Some("live-1")
        );
    }

    #[test]
    fn grok_session_is_headless_finds_the_summary_by_id_or_transcript() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let root = home.join(".grok").join("sessions");
        write_grok_summary(
            &root.join("proj").join("sid-h"),
            "sid-h",
            serde_json::json!({"session_kind": "headless"}),
        );
        write_grok_summary(
            &root.join("proj").join("sid-i"),
            "sid-i",
            serde_json::json!({}),
        );
        write_grok_summary(
            &root.join("sid-top"),
            "sid-top",
            serde_json::json!({"session_kind": "headless"}),
        );
        write_grok_summary(
            &root.join("proj").join("sid-sub"),
            "sid-sub",
            serde_json::json!({"session_kind": "subagent"}),
        );
        assert!(grok_session_is_headless(home, "sid-h", None));
        assert!(grok_session_is_headless(home, "sid-top", None));
        assert!(!grok_session_is_headless(home, "sid-i", None));
        assert!(!grok_session_is_headless(home, "sid-sub", None));
        assert!(!grok_session_is_headless(home, "missing", None));
        assert!(!grok_session_is_headless(home, "", None));
        assert!(!grok_session_is_headless(home, "../proj/sid-h", None));
        // Via the directory of the transcript path, even with an unknown id.
        let chat = root.join("proj").join("sid-h").join("chat_history.jsonl");
        assert!(grok_session_is_headless(home, "other", Some(&chat)));
        let chat = root.join("proj").join("sid-i").join("chat_history.jsonl");
        assert!(!grok_session_is_headless(home, "sid-i", Some(&chat)));
    }

    #[test]
    fn headless_grok_session_does_not_resolve_a_persona_from_the_process_tree() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let sid = "01a18599-c714-7700-85bb-e276825a98b3";
        let session = home.join(".grok").join("sessions").join("proj").join(sid);
        write_grok_summary(
            &session,
            sid,
            serde_json::json!({"session_kind": "headless"}),
        );
        fs::write(
            home.join(".grok").join("active_sessions.json"),
            serde_json::json!([{"session_id": sid, "pid": std::process::id(), "cwd": "/tmp"}])
                .to_string(),
        )
        .unwrap();
        assert!(resolve_grok_pid_persona(home, std::process::id()).is_none());
        // The same session without the marker resolves.
        write_grok_summary(&session, sid, serde_json::json!({}));
        assert_eq!(
            resolve_grok_pid_persona(home, std::process::id()).as_deref(),
            Some("wendy")
        );
    }

    #[test]
    fn headless_grok_session_is_not_resolved_or_captured_as_a_route() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let sid = "01a18599-c714-7700-85bb-e276825a98b3";
        let session = home.join(".grok").join("sessions").join("proj").join(sid);
        write_grok_summary(
            &session,
            sid,
            serde_json::json!({"session_kind": "headless"}),
        );
        fs::write(
            home.join(".grok").join("active_sessions.json"),
            serde_json::json!([{"session_id": sid, "pid": std::process::id(), "cwd": "/tmp"}])
                .to_string(),
        )
        .unwrap();
        // Process-tree path.
        assert!(resolve_grok_pid_session(home, std::process::id()).is_none());
        // `GROK_SESSION_ID` path.
        assert!(grok_env_route(Some(home), sid.to_string()).is_none());
        // The same session without the marker still resolves and is captured.
        write_grok_summary(&session, sid, serde_json::json!({}));
        let (got, _, _) = resolve_grok_pid_session(home, std::process::id()).unwrap();
        assert_eq!(got, sid);
        assert!(matches!(
            grok_env_route(Some(home), sid.to_string()),
            Some(VendorRoute::Grok { session_id, .. }) if session_id == sid
        ));
        // Without a home there is nothing to consult: the session is kept.
        assert!(grok_env_route(None, sid.to_string()).is_some());
    }

    #[test]
    fn flat_json_headless_check_reads_at_most_the_cap() {
        let tmp = TempDir::new().unwrap();
        let small = tmp.path().join("small.json");
        fs::write(
            &small,
            "{\n  \"id\": \"s\",\n  \"session_kind\": \"headless\"\n}\n",
        )
        .unwrap();
        assert!(grok_flat_file_is_headless(&small));
        // `session_kind` sits past the cap, so it is never seen.
        let big = tmp.path().join("big.json");
        let filler = "x".repeat((GROK_FLAT_FILE_READ_CAP as usize) + 1024);
        fs::write(
            &big,
            format!("{{\n  \"pad\": \"{filler}\",\n  \"session_kind\": \"headless\"\n}}\n"),
        )
        .unwrap();
        assert!(!grok_flat_file_is_headless(&big));
    }

    #[test]
    fn persona_source_distinguishes_named_from_inferred() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path();
        let none_env = |_: &str| None;
        let r = resolve_persona_traced(
            Some("Wendy"),
            None,
            cwd,
            None,
            RouteTable::default,
            &none_env,
        );
        assert_eq!(r, ("wendy".to_string(), PersonaSource::Explicit));
        let env = |k: &str| (k == "AGENT_PERSONA").then(|| std::ffi::OsString::from("Wesley"));
        let r = resolve_persona_traced(None, None, cwd, None, RouteTable::default, &env);
        assert_eq!(r, ("wesley".to_string(), PersonaSource::Env));
        let tp = cwd.join("t.jsonl");
        fs::write(
            &tp,
            "{\"type\":\"user\",\"content\":\"agent-msg register Katie\"}\n",
        )
        .unwrap();
        let r = resolve_persona_traced(None, Some(&tp), cwd, None, RouteTable::default, &none_env);
        assert_eq!(r, ("katie".to_string(), PersonaSource::Transcript));
        // Inherited vendor session id: inferred, not trusted.
        let mut routes = RouteTable::default();
        routes.upsert(
            "jarvis",
            VendorRoute::Agy {
                conversation_id: "conv-1".into(),
                ls_pid: None,
                ls_address: None,
                csrf_token: None,
                agentapi_exe: None,
            },
            At::from_unix_millis(1),
        );
        let env = |k: &str| {
            (k == "ANTIGRAVITY_CONVERSATION_ID").then(|| std::ffi::OsString::from("conv-1"))
        };
        let r = resolve_persona_traced(None, None, cwd, None, || routes, &env);
        assert_eq!(r, ("jarvis".to_string(), PersonaSource::Inferred));
        assert!(!r.1.is_trusted());
        assert!(PersonaSource::Transcript.is_trusted());
    }

    #[test]
    fn stale_historical_sessions_do_not_overwrite_a_newer_route() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let old_ms = At::parse("2026-09-16T10:40:52.000Z").unwrap().unix_millis();
        let live_at = At::parse("2026-10-29T01:42:59.000Z").unwrap();
        let agy_transcript =
            agy_registration_fixture(home, "conv-wesley", "Wesley", live_at.unix_millis());

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
            live_at.clone(),
        );
        table.discover_routes(Some(home), At::parse("2026-10-29T04:00:00.000Z").unwrap());

        assert!(
            matches!(
                table.get("wesley").map(|e| &e.route),
                Some(VendorRoute::Agy { conversation_id, .. }) if conversation_id == "conv-wesley"
            ),
            "stale codex, muse, and grok files must not replace the newer route"
        );

        // An explicit Agy registration at `live_at` also survives even if its transcript mtime is 100.
        set_mtime(&agy_transcript, 100);
        let mut explicit_table = RouteTable::default();
        explicit_table.upsert_explicit("wesley", agy_route("conv-wesley"), live_at);
        explicit_table.discover_routes(Some(home), At::parse("2026-10-29T04:00:00.000Z").unwrap());
        assert!(matches!(
            explicit_table.get("wesley").map(|e| &e.route),
            Some(VendorRoute::Agy { conversation_id, .. }) if conversation_id == "conv-wesley"
        ));
    }

    #[test]
    fn inactive_grok_summary_updated_at_does_not_replace_newer_route() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let old_ms = At::parse("2026-09-16T10:40:52.000Z").unwrap().unix_millis();
        let live_at = At::parse("2026-10-29T01:42:59.000Z").unwrap();
        let agy_transcript =
            agy_registration_fixture(home, "conv-wesley", "Wesley", live_at.unix_millis());
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

        set_mtime(&agy_transcript, 100);
        let mut explicit_table = RouteTable::default();
        explicit_table.upsert_explicit("wesley", agy_route("conv-wesley"), live_at);
        explicit_table.discover_routes(Some(home), At::parse("2026-10-29T06:00:00.000Z").unwrap());
        assert!(matches!(
            explicit_table.get("wesley").map(|e| &e.route),
            Some(VendorRoute::Agy { conversation_id, .. }) if conversation_id == "conv-wesley"
        ));
    }

    #[test]
    fn codex_index_updated_at_does_not_outstamp_older_rollout() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let old_ms = At::parse("2026-09-16T10:40:52.000Z").unwrap().unix_millis();
        let live_at = At::parse("2026-10-29T01:42:59.000Z").unwrap();
        let agy_transcript =
            agy_registration_fixture(home, "conv-wesley", "Wesley", live_at.unix_millis());
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

        set_mtime(&agy_transcript, 100);
        let mut explicit_table = RouteTable::default();
        explicit_table.upsert_explicit("wesley", agy_route("conv-wesley"), live_at);
        explicit_table.discover_routes(Some(home), At::parse("2026-10-29T06:00:00.000Z").unwrap());
        assert!(matches!(
            explicit_table.get("wesley").map(|e| &e.route),
            Some(VendorRoute::Agy { conversation_id, .. }) if conversation_id == "conv-wesley"
        ));
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
        assert!(
            csrf.is_none(),
            "csrf token from a process with a dead port must not be kept"
        );
        assert!(addr.is_none());
        assert!(exe.is_none());

        let live_env = format!(
            "ANTIGRAVITY_CSRF_TOKEN=csrf-live\0ANTIGRAVITY_LS_ADDRESS=127.0.0.1:{live_port}\0ANTIGRAVITY_AGENTAPI_EXE={}\0",
            good.display()
        );
        note_agy_proc_env(live_env.as_bytes(), &mut csrf, &mut addr, &mut exe);
        assert_eq!(csrf.as_deref(), Some("csrf-live"));
        assert_eq!(
            addr.as_deref(),
            Some(format!("127.0.0.1:{live_port}").as_str())
        );
        assert_eq!(exe.as_deref(), Some(good.as_path()));
    }

    #[test]
    fn note_agy_proc_env_never_mixes_cli_log_address_with_unpaired_csrf_token() {
        let live1 = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port1 = live1.local_addr().unwrap().port();
        let live2 = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port2 = live2.local_addr().unwrap().port();

        // Start with an unpaired address from cli.log.
        let mut addr = Some(format!("127.0.0.1:{port1}"));
        let mut csrf = None;
        let mut exe = None;

        // A process with only ANTIGRAVITY_CSRF_TOKEN (no ANTIGRAVITY_LS_ADDRESS) must not
        // attach its CSRF token to the unrelated cli.log address.
        note_agy_proc_env(
            b"ANTIGRAVITY_CSRF_TOKEN=orphan-csrf\0",
            &mut csrf,
            &mut addr,
            &mut exe,
        );
        assert!(csrf.is_none());
        assert_eq!(addr.as_deref(), Some(format!("127.0.0.1:{port1}").as_str()));

        // A process on a different port (port2) must not mix its CSRF token with port1.
        let other_port_env =
            format!("ANTIGRAVITY_LS_ADDRESS=127.0.0.1:{port2}\0ANTIGRAVITY_CSRF_TOKEN=csrf-p2\0");
        note_agy_proc_env(other_port_env.as_bytes(), &mut csrf, &mut addr, &mut exe);
        assert_eq!(addr.as_deref(), Some(format!("127.0.0.1:{port1}").as_str()));
        assert!(csrf.is_none());

        // A process whose ANTIGRAVITY_LS_ADDRESS matches port1 supplies its paired CSRF token.
        let matching_port_env =
            format!("ANTIGRAVITY_LS_ADDRESS=localhost:{port1}\0ANTIGRAVITY_CSRF_TOKEN=csrf-p1\0");
        note_agy_proc_env(matching_port_env.as_bytes(), &mut csrf, &mut addr, &mut exe);
        assert_eq!(addr.as_deref(), Some(format!("127.0.0.1:{port1}").as_str()));
        assert_eq!(csrf.as_deref(), Some("csrf-p1"));
    }

    #[test]
    fn discover_agy_routes_preserves_live_listening_route_pair_and_never_mixes_endpoints() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let agy_dir = home.join(".gemini").join("antigravity-cli");
        fs::create_dir_all(agy_dir.join("brain")).unwrap();

        let daemon_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let daemon_port = daemon_listener.local_addr().unwrap().port();
        let feira_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let feira_port = feira_listener.local_addr().unwrap().port();

        fs::write(
            agy_dir.join("cli.log"),
            format!("Language server listening on random port at {daemon_port} for HTTP\n"),
        )
        .unwrap();

        let feira_addr = format!("127.0.0.1:{feira_port}");
        let mut table = RouteTable::default();
        // Feira has her own standalone live language server and CSRF token.
        table.upsert_explicit(
            "feira",
            VendorRoute::Agy {
                conversation_id: "conv-feira".into(),
                ls_pid: Some(4242),
                ls_address: Some(feira_addr.clone()),
                csrf_token: Some("csrf-feira-live".into()),
                agentapi_exe: None,
            },
            At::from_unix_millis(200),
        );
        // Kayla has a dead port and stale CSRF token.
        table.upsert_explicit(
            "kayla",
            VendorRoute::Agy {
                conversation_id: "conv-kayla".into(),
                ls_pid: None,
                ls_address: Some("127.0.0.1:1".into()),
                csrf_token: Some("csrf-kayla-dead".into()),
                agentapi_exe: None,
            },
            At::from_unix_millis(100),
        );

        table.discover_agy_routes(home, false, &At::from_unix_millis(300));

        // Feira's live listening (ls_address, csrf_token) pair is preserved untouched.
        match &table.get("feira").expect("feira route").route {
            VendorRoute::Agy {
                ls_address,
                csrf_token,
                ..
            } => {
                assert_eq!(ls_address.as_deref(), Some(feira_addr.as_str()));
                assert_eq!(csrf_token.as_deref(), Some("csrf-feira-live"));
            }
            other => panic!("expected Agy route for feira, got {other:?}"),
        }

        // Kayla gets the cli.log daemon port, and must NOT steal Feira's CSRF token or retain
        // her own dead-port CSRF token.
        match &table.get("kayla").expect("kayla route").route {
            VendorRoute::Agy {
                ls_address,
                csrf_token,
                ..
            } => {
                assert_eq!(
                    ls_address.as_deref(),
                    Some(format!("127.0.0.1:{daemon_port}").as_str())
                );
                assert!(
                    csrf_token.is_none(),
                    "must not mix daemon cli.log port with another process's csrf_token, got {csrf_token:?}"
                );
            }
            other => panic!("expected Agy route for kayla, got {other:?}"),
        }

        // Upserting Feira with empty ls_address/csrf_token preserves her live listening pair.
        table.upsert(
            "feira",
            VendorRoute::Agy {
                conversation_id: "conv-feira".into(),
                ls_pid: None,
                ls_address: None,
                csrf_token: None,
                agentapi_exe: None,
            },
            At::from_unix_millis(400),
        );
        match &table.get("feira").expect("feira route after upsert").route {
            VendorRoute::Agy {
                ls_address,
                csrf_token,
                ls_pid,
                ..
            } => {
                assert_eq!(ls_address.as_deref(), Some(feira_addr.as_str()));
                assert_eq!(csrf_token.as_deref(), Some("csrf-feira-live"));
                assert_eq!(*ls_pid, Some(4242));
            }
            other => panic!("expected Agy route for feira, got {other:?}"),
        }
    }

    #[test]
    fn spinning_up_second_agy_session_does_not_rewrite_existing_agy_route_ls_address_or_csrf() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let agy_dir = home.join(".gemini").join("antigravity-cli");
        let brain_dir = agy_dir.join("brain");
        fs::create_dir_all(&brain_dir).unwrap();

        // Bind three independent local listeners:
        // 1. Remote-control daemon (33047 in live repro) with csrf `f5174f82`
        // 2. Feira's agy session (39643 in live repro) with csrf `6f09f521`
        // 3. Felix's newly spun-up agy session (42959 in live repro) with csrf `felix-csrf-42959`
        let daemon_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let daemon_port = daemon_listener.local_addr().unwrap().port();
        let daemon_addr = format!("127.0.0.1:{daemon_port}");

        let feira_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let feira_port = feira_listener.local_addr().unwrap().port();
        let feira_addr = format!("127.0.0.1:{feira_port}");

        let felix_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let felix_port = felix_listener.local_addr().unwrap().port();
        let felix_addr = format!("127.0.0.1:{felix_port}");

        let mut table = RouteTable::default();
        table.upsert_explicit(
            "jarvis",
            VendorRoute::Agy {
                conversation_id: "conv-jarvis-daemon".into(),
                ls_pid: Some(7701),
                ls_address: Some(daemon_addr.clone()),
                csrf_token: Some("f5174f82".into()),
                agentapi_exe: None,
            },
            At::from_unix_millis(100),
        );
        table.upsert_explicit(
            "feira",
            VendorRoute::Agy {
                conversation_id: "05729f07-2e25-43d9-a1af-90d5a5907788".into(),
                ls_pid: Some(2270049),
                ls_address: Some(feira_addr.clone()),
                csrf_token: Some("6f09f521".into()),
                agentapi_exe: None,
            },
            At::from_unix_millis(200),
        );

        // `fl agent spin-up --vendor antigravity --session Felix` registers an explicit
        // placeholder route for felix, and Felix's new agy process rotates `cli.log` to its
        // own port (42959) and creates its brain transcript.
        table.upsert_explicit(
            "felix",
            VendorRoute::Agy {
                conversation_id: "antigravity:felix".into(),
                ls_pid: Some(2667475),
                ls_address: None,
                csrf_token: None,
                agentapi_exe: None,
            },
            At::from_unix_millis(300),
        );
        fs::write(
            agy_dir.join("cli.log"),
            format!("Language server listening on random port at {felix_port} for HTTP\n"),
        )
        .unwrap();
        let felix_transcript = brain_dir
            .join("conv-felix-new")
            .join(".system_generated")
            .join("logs")
            .join("transcript.jsonl");
        fs::create_dir_all(felix_transcript.parent().unwrap()).unwrap();
        fs::write(
            &felix_transcript,
            "{\"type\":\"USER_INPUT\",\"content\":\"Take the name of Felix and take up residence in ferroloop/ as the web-developer.\"}\n",
        )
        .unwrap();
        set_mtime(&felix_transcript, 350);

        // Discovery runs before Felix executes `agent-msg register Felix`.
        table.discover_agy_routes(home, false, &At::from_unix_millis(400));

        // 1. Feira's ls_address and csrf_token must NOT be overwritten by Felix's cli.log port
        // or Jarvis's daemon csrf_token.
        match &table.get("feira").expect("feira route").route {
            VendorRoute::Agy {
                conversation_id,
                ls_pid,
                ls_address,
                csrf_token,
                ..
            } => {
                assert_eq!(conversation_id, "05729f07-2e25-43d9-a1af-90d5a5907788");
                assert_eq!(*ls_pid, Some(2270049));
                assert_eq!(ls_address.as_deref(), Some(feira_addr.as_str()));
                assert_eq!(csrf_token.as_deref(), Some("6f09f521"));
            }
            other => panic!("expected Agy route for feira, got {other:?}"),
        }

        // 2. Jarvis's daemon route must also remain untouched.
        match &table.get("jarvis").expect("jarvis route").route {
            VendorRoute::Agy {
                ls_address,
                csrf_token,
                ..
            } => {
                assert_eq!(ls_address.as_deref(), Some(daemon_addr.as_str()));
                assert_eq!(csrf_token.as_deref(), Some("f5174f82"));
            }
            other => panic!("expected Agy route for jarvis, got {other:?}"),
        }

        // 3. Felix's placeholder route is upgraded to `conv-felix-new` and preserves `ls_pid`,
        // without copying an unpaired cli.log address or Jarvis's `f5174f82` CSRF token onto a
        // route with a different live `ls_pid`.
        match &table.get("felix").expect("felix route").route {
            VendorRoute::Agy {
                conversation_id,
                ls_pid,
                ls_address,
                csrf_token,
                ..
            } => {
                assert_eq!(conversation_id, "conv-felix-new");
                assert_eq!(*ls_pid, Some(2667475));
                assert!(
                    ls_address.is_none(),
                    "felix must not inherit an unverified shared ls_address, got {ls_address:?}"
                );
                assert!(
                    csrf_token.is_none(),
                    "felix must not inherit jarvis's daemon csrf f5174f82 on a different port, got {csrf_token:?}"
                );
            }
            other => panic!("expected Agy route for felix, got {other:?}"),
        }

        // 4. Felix then runs `agent-msg register Felix`, recording his own paired endpoint,
        // and a subsequent discovery pass preserves all three sessions' distinct pairs.
        table.upsert_explicit(
            "felix",
            VendorRoute::Agy {
                conversation_id: "conv-felix-new".into(),
                ls_pid: None,
                ls_address: Some(felix_addr.clone()),
                csrf_token: Some("felix-csrf-42959".into()),
                agentapi_exe: None,
            },
            At::from_unix_millis(450),
        );
        table.discover_agy_routes(home, false, &At::from_unix_millis(500));

        for (persona, expected_addr, expected_csrf, expected_pid) in [
            ("jarvis", daemon_addr.as_str(), "f5174f82", Some(7701)),
            ("feira", feira_addr.as_str(), "6f09f521", Some(2270049)),
            (
                "felix",
                felix_addr.as_str(),
                "felix-csrf-42959",
                Some(2667475),
            ),
        ] {
            match &table.get(persona).expect(persona).route {
                VendorRoute::Agy {
                    ls_address,
                    csrf_token,
                    ls_pid,
                    ..
                } => {
                    assert_eq!(ls_address.as_deref(), Some(expected_addr), "{persona}");
                    assert_eq!(csrf_token.as_deref(), Some(expected_csrf), "{persona}");
                    assert_eq!(*ls_pid, expected_pid, "{persona}");
                }
                other => panic!("expected Agy route for {persona}, got {other:?}"),
            }
        }
    }

    #[test]
    fn discover_agy_routes_replaces_old_or_missing_agentapi_exe() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        agy_registration_fixture(home, "conv-old", "Oldie", 100);
        agy_registration_fixture(home, "conv-gone", "Gone", 100);
        agy_registration_fixture(home, "conv-kept", "Kept", 100);
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
        table.upsert_explicit(
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
        agy_registration_fixture(home, "conv-felicia", "Felicia", 100);
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

    fn stale_agy_route(conversation_id: &str, ls_address: &str, csrf: &str) -> VendorRoute {
        VendorRoute::Agy {
            conversation_id: conversation_id.into(),
            ls_pid: None,
            ls_address: Some(ls_address.into()),
            csrf_token: Some(csrf.into()),
            agentapi_exe: Some(PathBuf::from("/bin/sh")),
        }
    }

    fn agy_ls_address_of(table: &RouteTable, persona: &str) -> Option<String> {
        match &table.get(persona)?.route {
            VendorRoute::Agy { ls_address, .. } => ls_address.clone(),
            other => panic!("expected agy route, got {other:?}"),
        }
    }

    #[test]
    fn agy_discovery_restamps_a_corrected_route_so_the_legacy_file_cannot_clobber_it() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        agy_registration_fixture(&home, "conv-kayla", "Kayla", 100);
        let agy_dir = home.join(".gemini").join("antigravity-cli");
        fs::create_dir_all(agy_dir.join("brain")).unwrap();
        fs::write(
            agy_dir.join("cli.log"),
            "Language server listening on random port at 41547 for HTTP\n",
        )
        .unwrap();
        let primary = tmp.path().join("fl-wire-routes.json");
        let legacy = tmp.path().join("routes.json");
        let stale = "localhost:36313";
        let save_stale = |path: &Path, stamp_ms: u64| {
            let mut table = RouteTable::default();
            table.upsert(
                "kayla",
                stale_agy_route("conv-kayla", stale, "csrf"),
                At::from_unix_millis(stamp_ms),
            );
            table.save(Some(path)).unwrap();
        };
        // The legacy agent-bus copy carries a later stamp than the primary's, with the stale
        // pre-reboot endpoint.
        save_stale(&primary, 100);
        save_stale(&legacy, 999);
        {
            let mut leg = RouteTable::load(Some(&legacy));
            leg.upsert(
                "stale-legacy-only",
                stale_agy_route("conv-stale", stale, "csrf"),
                At::from_unix_millis(999),
            );
            leg.save(Some(&legacy)).unwrap();
        }

        let mut table = RouteTable::load_with_legacy(&primary, None);
        table.discover_routes(Some(&home), At::from_unix_millis(300));
        assert_eq!(
            agy_ls_address_of(&table, "kayla").as_deref(),
            Some("127.0.0.1:41547")
        );
        assert_eq!(table.get("kayla").unwrap().updated_at.unix_millis(), 300);
        table.save(Some(&primary)).unwrap();

        let reloaded = RouteTable::load_with_legacy(&primary, Some(&legacy));
        assert_eq!(
            agy_ls_address_of(&reloaded, "kayla").as_deref(),
            Some("127.0.0.1:41547"),
            "the stale legacy entry must not win when primary exists"
        );
        assert!(
            reloaded.get("stale-legacy-only").is_none(),
            "stale entries from legacy routes.json must not merge on top of an existing primary"
        );

        let missing_primary = tmp.path().join("missing-fl-wire-routes.json");
        let fallback = RouteTable::load_with_legacy(&missing_primary, Some(&legacy));
        assert!(
            fallback.get("stale-legacy-only").is_some(),
            "missing primary must still fall back to legacy routes.json"
        );

        // A scan that changes nothing leaves the stamp alone.
        table.discover_routes(Some(&home), At::from_unix_millis(400));
        assert_eq!(table.get("kayla").unwrap().updated_at.unix_millis(), 300);
    }

    #[test]
    fn agy_discovery_restamps_a_replaced_exe_but_not_a_filled_one() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        agy_registration_fixture(home, "c1", "Stale", 1);
        agy_registration_fixture(home, "c2", "Unset", 1);
        fs::create_dir_all(home.join(".gemini").join("antigravity-cli").join("brain")).unwrap();
        let route = |conv: &str, exe: Option<&str>| VendorRoute::Agy {
            conversation_id: conv.into(),
            ls_pid: None,
            ls_address: None,
            csrf_token: None,
            agentapi_exe: exe.map(PathBuf::from),
        };
        let mut table = RouteTable::default();
        table.upsert(
            "stale",
            route("c1", Some("/gone/agy.old")),
            At::from_unix_millis(1),
        );
        table.upsert("unset", route("c2", None), At::from_unix_millis(1));
        table.discover_agy_routes(home, false, &At::from_unix_millis(5));
        assert_eq!(table.get("stale").unwrap().updated_at.unix_millis(), 5);
        assert_eq!(table.get("unset").unwrap().updated_at.unix_millis(), 1);
    }

    #[test]
    fn agy_discovery_skips_a_one_shot_cli_log_that_shut_its_language_server_down() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        agy_registration_fixture(home, "conv-kayla", "Kayla", 1);
        let agy_dir = home.join(".gemini").join("antigravity-cli");
        let log_dir = agy_dir.join("log");
        fs::create_dir_all(agy_dir.join("brain")).unwrap();
        fs::create_dir_all(&log_dir).unwrap();
        // The persistent `agy remote-control serve` daemon's log.
        fs::write(
            log_dir.join("cli-daemon.log"),
            "Language server listening on random port at 44271 for HTTPS (gRPC)\n\
             Language server listening on random port at 41699 for HTTP\n",
        )
        .unwrap();
        // An `agy models` run: a listener that shuts down within a second. `cli.log` follows it.
        fs::write(
            log_dir.join("cli-oneshot.log"),
            "Language server listening on random port at 38000 for HTTP\n\
             Language server shutting down\n",
        )
        .unwrap();
        std::os::unix::fs::symlink(log_dir.join("cli-oneshot.log"), agy_dir.join("cli.log"))
            .unwrap();

        let at = At::from_unix_millis(10);
        let mut table = RouteTable::default();
        table.upsert(
            "kayla",
            stale_agy_route("conv-kayla", "localhost:36313", "csrf"),
            At::from_unix_millis(1),
        );
        table.discover_agy_routes(home, false, &at);
        assert_eq!(
            agy_ls_address_of(&table, "kayla").as_deref(),
            Some("127.0.0.1:41699")
        );
    }

    #[test]
    fn agy_live_http_ports_ends_at_the_last_shutdown() {
        let log = "Language server listening on random port at 1 for HTTP\n\
                   Language server shutting down\n\
                   Language server listening on random port at 2 for HTTP\n\
                   Language server listening on random port at 3 for HTTPS (gRPC)\n\
                   Language server listening on random port at 4 for HTTP\n";
        assert_eq!(agy_live_http_ports(log), ["4", "2"]);
        let shut = format!("{log}Language server shutting down\n");
        assert!(agy_live_http_ports(&shut).is_empty());
    }

    fn env_of<'a>(
        pairs: &'a [(&'a str, &'a str)],
    ) -> impl Fn(&str) -> Option<std::ffi::OsString> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(name, _)| *name == k)
                .map(|(_, v)| std::ffi::OsString::from(*v))
        }
    }

    #[test]
    fn an_inherited_antigravity_id_does_not_preempt_an_explicit_session_id() {
        let off = ("FL_WIRE_DISABLE_HOST_DISCOVERY", "1");
        let agy = ("ANTIGRAVITY_CONVERSATION_ID", "conv-inherited");
        // Alone, the Antigravity id is the caller's route.
        assert!(matches!(
            capture_env_route_from(&env_of(&[off, agy])),
            Some(VendorRoute::Agy { conversation_id, .. }) if conversation_id == "conv-inherited"
        ));
        assert!(matches!(
            capture_env_route_from(&env_of(&[off, agy, ("CODEX_THREAD_ID", "thread-7")])),
            Some(VendorRoute::Codex { thread_id, .. }) if thread_id == "thread-7"
        ));
        assert!(matches!(
            capture_env_route_from(&env_of(&[off, agy, ("MUSE_SESSION_ID", "muse-7")])),
            Some(VendorRoute::Muse { session_id, .. }) if session_id == "muse-7"
        ));
        assert!(matches!(
            capture_env_route_from(&env_of(&[off, agy, ("GROK_SESSION_ID", "grok-7")])),
            Some(VendorRoute::Grok { session_id, .. }) if session_id == "grok-7"
        ));
        // A Claude session id has no route without its socket, and is not an Antigravity one.
        assert!(
            capture_env_route_from(&env_of(&[off, agy, ("CLAUDE_SESSION_ID", "claude-7")]))
                .is_none()
        );
    }

    #[test]
    fn an_inherited_antigravity_id_does_not_preempt_an_explicit_session_id_for_the_persona() {
        let route = |route: VendorRoute| {
            let mut routes = RouteTable::default();
            routes.upsert(
                "jarvis",
                stale_agy_route("conv-1", "x:1", "c"),
                At::from_unix_millis(9),
            );
            routes.upsert("ada", route, At::from_unix_millis(1));
            routes
        };
        let routes = route(VendorRoute::Codex {
            thread_id: "thread-7".into(),
            cwd: None,
        });
        let env = env_of(&[
            ("ANTIGRAVITY_CONVERSATION_ID", "conv-1"),
            ("CODEX_THREAD_ID", "thread-7"),
        ]);
        let cwd = TempDir::new().unwrap();
        let r = resolve_persona_traced(None, None, cwd.path(), None, || routes, &env);
        assert_eq!(r, ("ada".to_string(), PersonaSource::Inferred));
    }

    #[test]
    fn a_derived_claude_session_name_resolves_through_its_tmux_session() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let sessions = home.join(".claude").join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        let sock = home.join("cc.sock");
        fs::write(&sock, "").unwrap();
        let session = |tmux: &str| {
            serde_json::json!({
                "pid": 1419570,
                "name": "felix-ui-86",
                "nameSource": "derived",
                "tmux": tmux,
                "sessionId": "felix-new-session",
                "messagingSocketPath": sock,
            })
        };
        let claude = |pid: u32, session_id: &str| VendorRoute::Claude {
            pid,
            session_id: session_id.into(),
            socket_path: sock.clone(),
            key_path: None,
        };
        let known = |persona: &str| persona == "felix";
        assert_eq!(
            resolve_claude_session_persona(home, &session("Felix:@5.%5"), None, &known).as_deref(),
            Some("felix")
        );
        // A tmux session that is no known persona names nothing, and a named session wins.
        assert_eq!(
            resolve_claude_session_persona(home, &session("main:@1.%1"), None, &known),
            None
        );
        let mut named = session("Felix:@5.%5");
        named["nameSource"] = "user".into();
        named["name"] = "ada".into();
        assert_eq!(
            resolve_claude_session_persona(home, &named, None, &known).as_deref(),
            Some("ada")
        );

        // Through discovery: the felix route follows the new session.
        fs::write(
            sessions.join("1419570.json"),
            session("Felix:@5.%5").to_string(),
        )
        .unwrap();
        let mut table = RouteTable::default();
        table.upsert(
            "felix",
            claude(1, "felix-old-session"),
            At::from_unix_millis(1),
        );
        table.discover_routes(Some(home), At::from_unix_millis(2));
        assert!(matches!(
            table.get("felix").map(|e| &e.route),
            Some(VendorRoute::Claude { pid: 1419570, session_id, .. }) if session_id == "felix-new-session"
        ));
        assert!(table.get("felix-ui-86").is_none());
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
        let route = |id: &str| VendorRoute::Codex {
            thread_id: id.into(),
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
            table.upsert(bad, route("t"), At::from_unix_millis(1));
        }
        assert!(table.entries.is_empty());
        table.upsert("  Mixed_Case-9  ", route("t1"), At::from_unix_millis(1));
        table.upsert(&"a".repeat(64), route("t2"), At::from_unix_millis(1));
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
    fn dispatch_scan_does_not_restore_concurrently_unregistered_route() {
        let mut base = RouteTable::default();
        base.upsert_explicit(
            "ada",
            VendorRoute::Codex {
                thread_id: "old".into(),
                cwd: None,
            },
            At::from_unix_millis(1),
        );
        let mut scanned = base.clone();
        scanned.upsert(
            "ada",
            VendorRoute::Codex {
                thread_id: "old".into(),
                cwd: None,
            },
            At::from_unix_millis(3),
        );
        let mut live = base.clone();
        live.entries.remove("ada");
        live.merge_daemon_dispatch_result(&base, scanned);
        assert!(live.get("ada").is_none());
    }

    #[test]
    fn hub_scan_can_merge_a_changed_route_after_its_prior_entry_was_removed() {
        let mut base = RouteTable::default();
        base.upsert(
            "ada",
            VendorRoute::Codex {
                thread_id: "old".into(),
                cwd: None,
            },
            At::from_unix_millis(1),
        );
        let mut scanned = base.clone();
        scanned.upsert(
            "ada",
            VendorRoute::Codex {
                thread_id: "new".into(),
                cwd: None,
            },
            At::from_unix_millis(3),
        );
        let mut live = base.clone();
        live.entries.remove("ada");
        live.merge_dispatch_result(&base, scanned);
        assert!(
            matches!(&live.get("ada").unwrap().route, VendorRoute::Codex { thread_id, .. } if thread_id == "new")
        );
    }

    #[test]
    fn dispatch_scan_does_not_replace_concurrent_explicit_registration() {
        let mut base = RouteTable::default();
        base.upsert_explicit(
            "ada",
            VendorRoute::Codex {
                thread_id: "old".into(),
                cwd: None,
            },
            At::from_unix_millis(1),
        );
        let mut scanned = base.clone();
        scanned.upsert(
            "ada",
            VendorRoute::Codex {
                thread_id: "old".into(),
                cwd: None,
            },
            At::from_unix_millis(3),
        );
        let mut live = base.clone();
        live.upsert_explicit(
            "ada",
            VendorRoute::Codex {
                thread_id: "new".into(),
                cwd: None,
            },
            At::from_unix_millis(2),
        );
        live.merge_daemon_dispatch_result(&base, scanned);
        assert!(
            matches!(&live.get("ada").unwrap().route, VendorRoute::Codex { thread_id, .. } if thread_id == "new")
        );
    }

    #[test]
    fn dispatch_scan_alias_cannot_evict_concurrent_registered_session() {
        let base = RouteTable::default();
        let mut scanned = base.clone();
        scanned.upsert(
            "old-alias",
            VendorRoute::Codex {
                thread_id: "same".into(),
                cwd: None,
            },
            At::from_unix_millis(3),
        );
        let mut live = base.clone();
        live.upsert_explicit(
            "ada",
            VendorRoute::Codex {
                thread_id: "same".into(),
                cwd: None,
            },
            At::from_unix_millis(2),
        );
        live.merge_daemon_dispatch_result(&base, scanned);
        assert!(live.get("ada").is_some());
        assert!(live.get("old-alias").is_none());
    }

    #[test]
    fn dispatch_scan_alias_does_not_restore_a_concurrently_removed_session() {
        let mut base = RouteTable::default();
        base.upsert(
            "ada",
            VendorRoute::Codex {
                thread_id: "old".into(),
                cwd: None,
            },
            At::from_unix_millis(1),
        );
        let mut scanned = base.clone();
        scanned.upsert(
            "old-alias",
            VendorRoute::Codex {
                thread_id: "old".into(),
                cwd: None,
            },
            At::from_unix_millis(3),
        );
        let mut live = base.clone();
        live.entries.remove("ada");
        live.merge_daemon_dispatch_result(&base, scanned);
        assert!(live.get("ada").is_none());
        assert!(live.get("old-alias").is_none());
    }

    /// The daemon dispatches on a copy of its table: a scan made on that copy must reach the live
    /// table, or every dispatch would find the live table due and rescan.
    #[test]
    fn merge_dispatch_result_carries_the_working_copys_discovery_time_back() {
        let interval = Duration::from_secs(60);
        let mut live = RouteTable::default();
        let base = live.clone();
        let mut working = base.clone();
        working.discover_routes(None, At::from_unix_millis(10));
        assert!(live.discovery_due(interval));
        live.merge_dispatch_result(&base, working);
        assert!(
            !live.discovery_due(interval),
            "the scan was not carried back"
        );
        // An older scan on the working copy does not move the live time back.
        let stale = Instant::now()
            .checked_sub(interval * 2)
            .expect("clock older than the interval");
        let mut old = live.clone();
        old.set_last_discovery(Some(stale));
        live.merge_dispatch_result(&base, old);
        assert!(!live.discovery_due(interval));
        // The time is never written to the routes file.
        let json = serde_json::to_string(&live).unwrap();
        assert!(!json.contains("last_discovery"), "{json}");
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

    // ---- Muse discovery: nested transcripts, tmux windows, placeholder routes ------------------

    const MUSE_SESSION_A: &str = "01a17d5b-482d-7ec0-a974-eaa56ef667a7";
    const MUSE_SESSION_B: &str = "01a17d5b-482d-7ec0-a974-eaa56ef667b8";

    /// The hydration prompt a lifecycle hands a Muse session (see `canonical_reforge_prompt`).
    fn muse_hydration_prompt(display: &str) -> String {
        format!(
            "Take the name of {display} and take up residence in ArtificialHumanity.io/ as the \
             assistant."
        )
    }

    /// A Muse transcript: the first row records the prompt Muse was started with.
    fn muse_transcript_rows(prompt: &str) -> String {
        [
            serde_json::json!({
                "schema_version": 1,
                "recorded_at": 1_793_104_670_899_152_i64,
                "payload_type": "runtime.user_intent.accepted",
                "payload": { "model_messages": [{ "role": "user", "content": prompt }] }
            }),
            serde_json::json!({
                "schema_version": 1,
                "recorded_at": 1_793_104_889_531_568_i64,
                "payload_type": "runtime.session",
                "payload": {
                    "kind": "run",
                    "event": { "kind": "assistant_message_committed", "text": "Residence taken." }
                }
            }),
        ]
        .map(|row| row.to_string())
        .join("\n")
    }

    fn muse_sessions_dir(home: &Path) -> PathBuf {
        home.join(".local/share/muse/sessions")
    }

    fn write_muse_transcript(dir: &Path, text: &str) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join("session.jsonl");
        fs::write(&path, text).unwrap();
        path
    }

    fn muse_route_of(table: &RouteTable, persona: &str) -> Option<VendorRoute> {
        table.get(persona).map(|e| e.route.clone())
    }

    fn muse_parts(route: &VendorRoute) -> (String, Option<String>, Option<PathBuf>) {
        match route {
            VendorRoute::Muse {
                session_id,
                tmux_target,
                cwd,
                ..
            } => (session_id.clone(), tmux_target.clone(), cwd.clone()),
            other => panic!("not a Muse route: {other:?}"),
        }
    }

    /// Serialises the tests that set or rely on `FL_WIRE_TMUX_BIN`; restores it on drop.
    struct TmuxEnv {
        _lock: std::sync::MutexGuard<'static, ()>,
        prev_bin: Option<std::ffi::OsString>,
        prev_disable: Option<std::ffi::OsString>,
        prev_opt_in: Option<std::ffi::OsString>,
    }

    impl TmuxEnv {
        /// Holds the lock with `FL_WIRE_TMUX_BIN`, `FL_WIRE_TMUX_DISCOVERY` and
        /// `FL_WIRE_DISABLE_HOST_DISCOVERY` unset.
        fn unset() -> Self {
            let lock = crate::dispatcher::TMUX_ENV_LOCK
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            let this = Self {
                _lock: lock,
                prev_bin: std::env::var_os("FL_WIRE_TMUX_BIN"),
                prev_disable: std::env::var_os("FL_WIRE_DISABLE_HOST_DISCOVERY"),
                prev_opt_in: std::env::var_os("FL_WIRE_TMUX_DISCOVERY"),
            };
            // SAFETY: the lock is held for the guard's life; drop restores both variables.
            unsafe {
                std::env::remove_var("FL_WIRE_TMUX_BIN");
                std::env::remove_var("FL_WIRE_TMUX_DISCOVERY");
                std::env::remove_var("FL_WIRE_DISABLE_HOST_DISCOVERY");
            }
            this
        }

        /// As [`TmuxEnv::unset`], then points `FL_WIRE_TMUX_BIN` at a fake `tmux` in `dir` whose
        /// `list-windows` prints `windows` (one `session:window` per line), for scans of `home`.
        fn fake(dir: &Path, home: &Path, windows: &[&str]) -> Self {
            let list = windows
                .iter()
                .map(|w| format!("echo '{w}'\n"))
                .collect::<String>();
            Self::fake_script(dir, home, &list)
        }

        /// As [`TmuxEnv::fake`], but `list-windows` runs the shell `list_body`.
        fn fake_script(dir: &Path, home: &Path, list_body: &str) -> Self {
            use std::os::unix::fs::PermissionsExt;

            let this = Self::unset();
            let bin = dir.join("fake-tmux.sh");
            fs::write(
                &bin,
                format!(
                    "#!/bin/sh\necho \"$*\" >> \"{}\"\nif [ \"$1\" = \"list-windows\" ]; then\n{list_body}fi\n",
                    dir.join("tmux.out").display()
                ),
            )
            .unwrap();
            fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
            // SAFETY: as in `unset`.
            unsafe {
                std::env::set_var("FL_WIRE_TMUX_BIN", &bin);
                std::env::set_var("FL_WIRE_TMUX_DISCOVERY", home);
            }
            this
        }
    }

    impl Drop for TmuxEnv {
        fn drop(&mut self) {
            // SAFETY: the lock is still held (fields drop after this).
            unsafe {
                match self.prev_bin.take() {
                    Some(v) => std::env::set_var("FL_WIRE_TMUX_BIN", v),
                    None => std::env::remove_var("FL_WIRE_TMUX_BIN"),
                }
                match self.prev_disable.take() {
                    Some(v) => std::env::set_var("FL_WIRE_DISABLE_HOST_DISCOVERY", v),
                    None => std::env::remove_var("FL_WIRE_DISABLE_HOST_DISCOVERY"),
                }
                match self.prev_opt_in.take() {
                    Some(v) => std::env::set_var("FL_WIRE_TMUX_DISCOVERY", v),
                    None => std::env::remove_var("FL_WIRE_TMUX_DISCOVERY"),
                }
            }
        }
    }

    fn windows(pairs: &[(&str, &str)]) -> Vec<TmuxWindow> {
        pairs
            .iter()
            .map(|(s, w)| (s.to_string(), w.to_string()))
            .collect()
    }

    #[test]
    fn muse_discovery_finds_a_nested_session_and_names_the_persona_from_the_hydration_prompt() {
        let tmp = TempDir::new().unwrap();
        let dir = muse_sessions_dir(tmp.path())
            .join("2026/10/27")
            .join(MUSE_SESSION_A);
        write_muse_transcript(&dir, &muse_transcript_rows(&muse_hydration_prompt("Kacey")));

        let mut table = RouteTable::default();
        table.discover_routes(Some(tmp.path()), At::from_unix_millis(5));

        let route = muse_route_of(&table, "kacey").expect("the nested transcript names kacey");
        let (session_id, tmux_target, _) = muse_parts(&route);
        assert_eq!(session_id, MUSE_SESSION_A, "the id is the directory name");
        assert_eq!(tmux_target, None, "a home override never asks tmux");
    }

    #[test]
    fn a_nested_muse_session_id_is_the_directory_not_a_row_id() {
        let tmp = TempDir::new().unwrap();
        let rows = format!(
            "{}\n{}",
            serde_json::json!({ "id": "evt-9", "session_id": "row-sid", "cwd": "/work/here" }),
            muse_transcript_rows(&muse_hydration_prompt("Kacey"))
        );
        write_muse_transcript(
            &muse_sessions_dir(tmp.path())
                .join("2026/10/27")
                .join(MUSE_SESSION_A),
            &rows,
        );
        let mut table = RouteTable::default();
        table.discover_routes(Some(tmp.path()), At::from_unix_millis(5));
        let (session_id, _, cwd) = muse_parts(&muse_route_of(&table, "kacey").unwrap());
        assert_eq!(session_id, MUSE_SESSION_A);
        assert_eq!(cwd, Some(PathBuf::from("/work/here")));
    }

    #[test]
    fn muse_discovery_ignores_subagent_dot_dir_symlink_and_too_deep_transcripts() {
        let tmp = TempDir::new().unwrap();
        let sessions = muse_sessions_dir(tmp.path());
        let day = sessions.join("2026/10/27");
        let other = |name: &str| muse_transcript_rows(&muse_hydration_prompt(name));
        write_muse_transcript(&day.join(MUSE_SESSION_A), &other("Kacey"));
        // Under a `subagent` directory, within the depth bound (so only its name excludes it).
        write_muse_transcript(&day.join("subagent").join(MUSE_SESSION_B), &other("Sub"));
        write_muse_transcript(
            &sessions.join("subagents").join(MUSE_SESSION_B),
            &other("Subs"),
        );
        // Under dot-directories, at the top and in the middle.
        write_muse_transcript(&sessions.join(".msp-view-1"), &other("Dotty"));
        write_muse_transcript(
            &day.join(".msp-view-2").join(MUSE_SESSION_B),
            &other("Hidden"),
        );
        // Too deep: six components below `sessions`.
        write_muse_transcript(
            &sessions.join("a/b/c/d/e").join(MUSE_SESSION_B),
            &other("Deep"),
        );
        // A linked directory is not followed.
        let outside = tmp.path().join("outside");
        write_muse_transcript(&outside.join(MUSE_SESSION_B), &other("Linky"));
        std::os::unix::fs::symlink(&outside, sessions.join("2026/10/28")).unwrap();
        // A transcript that is not `session.jsonl` is not one.
        fs::write(
            day.join(MUSE_SESSION_A).join("events.jsonl"),
            other("Events"),
        )
        .unwrap();

        let mut table = RouteTable::default();
        table.discover_routes(Some(tmp.path()), At::from_unix_millis(5));

        assert_eq!(
            table.entries.keys().cloned().collect::<Vec<_>>(),
            vec!["kacey".to_string()],
            "{:?}",
            table.entries
        );
    }

    #[test]
    fn muse_transcript_scan_is_bounded() {
        let tmp = TempDir::new().unwrap();
        let day = tmp.path().join("2026/10/27");
        for i in 0..2005 {
            write_muse_transcript(&day.join(format!("id-{i:05}")), "{}");
        }
        let found = muse_transcripts(tmp.path());
        assert_eq!(found.len(), 2000);
        // Newest names first: the cut drops the oldest.
        assert_eq!(found[0].dir_session_id.as_deref(), Some("id-02004"));
        assert!(
            found
                .iter()
                .all(|t| t.dir_session_id.as_deref() != Some("id-00004"))
        );
    }

    #[test]
    fn muse_discovery_still_reads_the_flat_form() {
        let tmp = TempDir::new().unwrap();
        let sessions = muse_sessions_dir(tmp.path());
        fs::create_dir_all(&sessions).unwrap();
        fs::write(
            sessions.join("flat-sess.jsonl"),
            "{\"session_id\":\"flat-sess\",\"agentName\":\"Mira\",\"cwd\":\"/tmp\"}\n",
        )
        .unwrap();
        let mut table = RouteTable::default();
        table.discover_routes(Some(tmp.path()), At::from_unix_millis(5));
        let (session_id, _, cwd) = muse_parts(&muse_route_of(&table, "mira").unwrap());
        assert_eq!(session_id, "flat-sess");
        assert_eq!(cwd, Some(PathBuf::from("/tmp")));
    }

    #[test]
    fn muse_tmux_target_is_set_only_when_the_window_is_live() {
        let tmp = TempDir::new().unwrap();
        let sessions = vec![muse_sessions_dir(tmp.path())];
        write_muse_transcript(
            &sessions[0].join("2026/10/27").join(MUSE_SESSION_A),
            &muse_transcript_rows(&muse_hydration_prompt("Kacey")),
        );
        let at = At::from_unix_millis(5);
        let target_for = |windows: Option<&[TmuxWindow]>| {
            let mut table = RouteTable::default();
            table.discover_muse_routes_in(&sessions, windows, &at);
            muse_parts(&muse_route_of(&table, "kacey").unwrap()).1
        };

        assert_eq!(target_for(None), None, "no tmux consulted");
        assert_eq!(target_for(Some(&windows(&[]))), None);
        assert_eq!(
            target_for(Some(&windows(&[("Kacey", "zsh"), ("Other", "muse")]))),
            None,
            "another window, or another session's muse window, is not hers"
        );
        assert_eq!(
            target_for(Some(&windows(&[("Kacey", "muse")]))).as_deref(),
            Some("Kacey:muse")
        );
        // A window of the shared `muse` session named by the persona is hers.
        assert_eq!(
            target_for(Some(&windows(&[("muse", "kacey")]))).as_deref(),
            Some("muse:kacey")
        );
        // Another persona's window of the shared session, a window literally named `muse` and
        // a name that is no persona are not hers.
        assert_eq!(
            target_for(Some(&windows(&[
                ("muse", "mira"),
                ("muse", "muse"),
                ("muse", "Bad Name"),
                ("muse", "artificial-humanity-kacey"),
            ]))),
            None
        );
        // With both forms live the shared session's window is preferred, whatever the order.
        for pairs in [
            [("Kacey", "muse"), ("muse", "kacey")],
            [("muse", "kacey"), ("Kacey", "muse")],
        ] {
            assert_eq!(
                target_for(Some(&windows(&pairs))).as_deref(),
                Some("muse:kacey"),
                "{pairs:?}"
            );
        }
    }

    #[test]
    fn a_muse_route_keeps_its_tmux_target_exactly_while_that_window_is_live() {
        let route = || {
            let mut table = RouteTable::default();
            table.upsert(
                "kacey",
                VendorRoute::Muse {
                    session_id: "s".into(),
                    cwd: None,
                    tmux_target: Some("Elsewhere:1".into()),
                    tmux_pane: None,
                },
                At::from_unix_millis(1),
            );
            table
        };
        let target_after = |pairs: &[(&str, &str)]| {
            let mut table = route();
            table.attach_muse_tmux_windows(&windows(pairs), &At::from_unix_millis(2));
            muse_parts(&muse_route_of(&table, "kacey").unwrap()).1
        };
        // Its window is live: kept, even though the persona has live windows of the two forms.
        assert_eq!(
            target_after(&[("Elsewhere", "1"), ("Kacey", "muse"), ("muse", "kacey")]).as_deref(),
            Some("Elsewhere:1")
        );
        // Its window is gone: replaced by the persona's live window (shared form first) ...
        assert_eq!(
            target_after(&[("Kacey", "muse")]).as_deref(),
            Some("Kacey:muse")
        );
        assert_eq!(
            target_after(&[("Kacey", "muse"), ("muse", "kacey")]).as_deref(),
            Some("muse:kacey")
        );
        // ... or cleared when she has none.
        assert_eq!(target_after(&[("Other", "muse"), ("muse", "mira")]), None);
        assert_eq!(target_after(&[]), None);
    }

    #[test]
    fn muse_tmux_discovery_needs_host_discovery_or_a_test_opt_in_for_that_home_with_a_fake_tmux() {
        let _env = TmuxEnv::unset();
        let home = Path::new("/scan/home");
        assert!(muse_tmux_discovery_enabled(home, false), "host scan");
        assert!(!muse_tmux_discovery_enabled(home, true), "home override");
        // SAFETY: TmuxEnv holds the lock and restores the variables.
        unsafe { std::env::set_var("FL_WIRE_TMUX_BIN", "/nonexistent/fake-tmux") };
        assert!(
            !muse_tmux_discovery_enabled(home, true),
            "a named tmux alone is no opt-in"
        );
        unsafe { std::env::set_var("FL_WIRE_TMUX_DISCOVERY", home) };
        assert!(muse_tmux_discovery_enabled(home, true), "opt-in");
        assert!(
            !muse_tmux_discovery_enabled(Path::new("/other/home"), true),
            "the opt-in names one home"
        );
        unsafe { std::env::remove_var("FL_WIRE_TMUX_BIN") };
        assert!(
            !muse_tmux_discovery_enabled(home, true),
            "opt-in needs a fake tmux"
        );
        unsafe { std::env::set_var("FL_WIRE_TMUX_BIN", "/nonexistent/fake-tmux") };
        unsafe { std::env::set_var("FL_WIRE_DISABLE_HOST_DISCOVERY", "1") };
        assert!(
            !muse_tmux_discovery_enabled(home, false),
            "host discovery off"
        );
        assert!(
            !muse_tmux_discovery_enabled(home, true),
            "host discovery off"
        );
    }

    #[test]
    fn a_home_override_scan_never_runs_tmux() {
        let tmp = TempDir::new().unwrap();
        let env = TmuxEnv::unset();
        // A tmux that would report a live window, reachable only through PATH: not used.
        let bin_dir = tmp.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let marker = tmp.path().join("tmux-ran");
        let tmux = bin_dir.join("tmux");
        fs::write(
            &tmux,
            format!(
                "#!/bin/sh\ntouch \"{}\"\necho 'Kacey:muse'\n",
                marker.display()
            ),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&tmux, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let prev_path = std::env::var_os("PATH").unwrap_or_default();
        let mut path = std::ffi::OsString::from(&bin_dir);
        path.push(":");
        path.push(&prev_path);
        // SAFETY: `env` holds the tmux env lock; PATH is restored below.
        unsafe { std::env::set_var("PATH", &path) };
        let home = tmp.path().join("home");
        let mut table = RouteTable::default();
        table.discover_routes(Some(&home), At::from_unix_millis(5));
        unsafe { std::env::set_var("PATH", prev_path) };
        drop(env);

        assert!(!marker.exists(), "tmux ran under a home override");
        assert!(table.entries.is_empty(), "{:?}", table.entries);
    }

    #[test]
    fn a_live_muse_window_makes_a_route_before_the_first_transcript() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let _env = TmuxEnv::fake(
            tmp.path(),
            &home,
            &[
                "Kacey:muse",
                "Kacey:logs",
                "Felix:zsh",
                "muse:muse",
                "Bad Name:muse",
                "artificial-humanity-x:muse",
                "0:zsh",
            ],
        );
        let mut table = RouteTable::default();
        table.discover_routes(Some(&home), At::from_unix_millis(5));

        assert_eq!(
            table.entries.keys().cloned().collect::<Vec<_>>(),
            vec!["kacey".to_string()],
            "{:?}",
            table.entries
        );
        let (session_id, target, cwd) = muse_parts(&muse_route_of(&table, "kacey").unwrap());
        assert_eq!(session_id, "", "no transcript yet");
        assert_eq!(target.as_deref(), Some("Kacey:muse"));
        assert_eq!(cwd, None);
        let log = fs::read_to_string(tmp.path().join("tmux.out")).unwrap();
        assert!(
            log.contains("list-windows -a -F #{session_name}:#{window_name}"),
            "{log}"
        );
    }

    #[test]
    fn a_live_window_of_the_shared_muse_session_makes_a_route_through_discovery() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let _env = TmuxEnv::fake(
            tmp.path(),
            &home,
            &[
                "muse:mira",
                "muse:muse",
                "muse:Bad Name",
                "Kacey:muse",
                "muse:kacey",
            ],
        );
        let mut table = RouteTable::default();
        table.discover_routes(Some(&home), At::from_unix_millis(5));
        let targets: Vec<(String, Option<String>)> = table
            .entries
            .keys()
            .map(|p| (p.clone(), muse_parts(&muse_route_of(&table, p).unwrap()).1))
            .collect();
        assert_eq!(
            targets,
            [
                ("kacey".to_string(), Some("muse:kacey".to_string())),
                ("mira".to_string(), Some("muse:mira".to_string())),
            ]
        );
    }

    #[test]
    fn a_muse_window_does_not_replace_a_route_the_persona_already_has() {
        let at = At::from_unix_millis(5);
        let mut table = RouteTable::default();
        table.upsert(
            "kacey",
            VendorRoute::Codex {
                thread_id: "t".into(),
                cwd: None,
            },
            At::from_unix_millis(1),
        );
        table.attach_muse_tmux_windows(&windows(&[("Kacey", "muse")]), &at);
        assert!(matches!(
            muse_route_of(&table, "kacey"),
            Some(VendorRoute::Codex { .. })
        ));
    }

    #[test]
    fn a_placeholder_muse_route_goes_with_its_window_and_gives_way_to_the_transcript() {
        let tmp = TempDir::new().unwrap();
        let sessions = vec![muse_sessions_dir(tmp.path())];
        let early = At::from_unix_millis(5);
        let mut table = RouteTable::default();
        table.discover_muse_routes_in(&sessions, Some(&windows(&[("Kacey", "muse")])), &early);
        assert_eq!(muse_parts(&muse_route_of(&table, "kacey").unwrap()).0, "");

        // The transcript appears. Its mtime (1) is older than the placeholder (5): still wins.
        let transcript = write_muse_transcript(
            &sessions[0].join("2026/10/27").join(MUSE_SESSION_A),
            &muse_transcript_rows(&muse_hydration_prompt("Kacey")),
        );
        set_mtime(&transcript, 1);
        table.discover_muse_routes_in(&sessions, Some(&windows(&[("Kacey", "muse")])), &early);
        let (session_id, target, _) = muse_parts(&muse_route_of(&table, "kacey").unwrap());
        assert_eq!(session_id, MUSE_SESSION_A);
        assert_eq!(
            target.as_deref(),
            Some("Kacey:muse"),
            "the window's target carries over"
        );

        // A placeholder whose window is gone is dropped; a route with a session id is kept.
        let mut table = RouteTable::default();
        table.discover_muse_routes_in(&[], Some(&windows(&[("Kacey", "muse")])), &early);
        table.discover_muse_routes_in(&[], Some(&windows(&[])), &early);
        assert!(table.entries.is_empty(), "{:?}", table.entries);
        let mut table = RouteTable::default();
        table.discover_muse_routes_in(&sessions, Some(&windows(&[])), &early);
        assert!(table.get("kacey").is_some());
    }

    #[test]
    fn explicit_muse_placeholder_remains_explicit_when_transcript_appears() {
        let tmp = TempDir::new().unwrap();
        let sessions = vec![muse_sessions_dir(tmp.path())];
        let at = At::from_unix_millis(5);
        let mut table = RouteTable::default();
        table.upsert_explicit(
            "kacey",
            VendorRoute::Muse {
                session_id: String::new(),
                cwd: None,
                tmux_target: Some("Kacey:muse".into()),
                tmux_pane: None,
            },
            at.clone(),
        );
        let transcript = write_muse_transcript(
            &sessions[0].join("2026/10/27").join(MUSE_SESSION_A),
            &muse_transcript_rows(&muse_hydration_prompt("Kacey")),
        );
        set_mtime(&transcript, 1);
        table.discover_muse_routes_in(&sessions, Some(&windows(&[("Kacey", "muse")])), &at);
        let entry = table.get("kacey").unwrap();
        assert!(entry.explicit);
        assert!(
            matches!(&entry.route, VendorRoute::Muse { session_id, .. } if session_id == MUSE_SESSION_A)
        );
    }

    #[test]
    fn passive_alias_cannot_evict_an_explicit_session() {
        let route = VendorRoute::Codex {
            thread_id: "same".into(),
            cwd: None,
        };
        let mut table = RouteTable::default();
        table.upsert_explicit("ada", route.clone(), At::from_unix_millis(1));
        table.upsert_discovered("old-alias", route.clone(), 2, &At::from_unix_millis(2));
        assert_eq!(table.get("ada").map(|entry| &entry.route), Some(&route));
        assert!(table.get("old-alias").is_none());

        let mut observed = RouteTable::default();
        observed.upsert("old-alias", route.clone(), At::from_unix_millis(3));
        table.merge_newer(observed);
        assert_eq!(table.get("ada").map(|entry| &entry.route), Some(&route));
        assert!(table.get("old-alias").is_none());
    }

    fn muse_route_with(session_id: &str, target: Option<&str>) -> VendorRoute {
        VendorRoute::Muse {
            session_id: session_id.into(),
            cwd: None,
            tmux_target: target.map(str::to_string),
            tmux_pane: Some("%3".into()),
        }
    }

    #[test]
    fn a_stored_shared_session_target_is_kept_while_its_window_is_live_and_cleared_when_not() {
        let table_with = |target: &str| {
            let mut table = RouteTable::default();
            table.upsert(
                "kacey",
                muse_route_with("s", Some(target)),
                At::from_unix_millis(1),
            );
            table
        };
        let target_after = |stored: &str, pairs: &[(&str, &str)]| {
            let mut table = table_with(stored);
            table.attach_muse_tmux_windows(&windows(pairs), &At::from_unix_millis(2));
            let route = muse_route_of(&table, "kacey").unwrap();
            assert!(
                matches!(&route, VendorRoute::Muse { tmux_pane: Some(p), .. } if p == "%3"),
                "the pane stays: {route:?}"
            );
            muse_parts(&route).1
        };
        // Live: kept (it is a legitimate target now, not a guess).
        assert_eq!(
            target_after("muse:kacey", &[("muse", "kacey")]).as_deref(),
            Some("muse:kacey")
        );
        // Neither form live: cleared.
        assert_eq!(target_after("muse:kacey", &[("Other", "muse")]), None);
        assert_eq!(target_after("muse:kacey", &[("muse", "mira")]), None);
        assert_eq!(target_after("muse:kacey", &[]), None);
        // Its own window is gone but the other form is live: moves to it.
        assert_eq!(
            target_after("muse:kacey", &[("Kacey", "muse")]).as_deref(),
            Some("Kacey:muse")
        );
        // The same holds for the per-persona form.
        assert_eq!(
            target_after("Kacey:muse", &[("Kacey", "muse")]).as_deref(),
            Some("Kacey:muse")
        );
        assert_eq!(target_after("Kacey:muse", &[("Other", "muse")]), None);
        assert_eq!(
            target_after("Kacey:muse", &[("muse", "kacey")]).as_deref(),
            Some("muse:kacey")
        );
    }

    #[test]
    fn a_live_window_of_either_form_makes_a_placeholder_route_preferring_the_shared_one() {
        let at = At::from_unix_millis(5);
        let mut table = RouteTable::default();
        table.attach_muse_tmux_windows(
            &windows(&[
                ("Kacey", "muse"),
                ("muse", "kacey"),
                ("Felix", "muse"),
                ("muse", "mira"),
                // Not personas, or not Muse windows.
                ("muse", "muse"),
                ("muse", "Bad Name"),
                ("muse", "artificial-humanity-x"),
                ("Bad Name", "muse"),
                ("Other", "zsh"),
            ]),
            &at,
        );
        let targets: Vec<(String, Option<String>)> = table
            .entries
            .keys()
            .map(|p| (p.clone(), muse_parts(&muse_route_of(&table, p).unwrap()).1))
            .collect();
        assert_eq!(
            targets,
            [
                ("felix".to_string(), Some("Felix:muse".to_string())),
                ("kacey".to_string(), Some("muse:kacey".to_string())),
                ("mira".to_string(), Some("muse:mira".to_string())),
            ]
        );
        for persona in ["felix", "kacey", "mira"] {
            let (session_id, _, cwd) = muse_parts(&muse_route_of(&table, persona).unwrap());
            assert_eq!((session_id.as_str(), cwd), ("", None), "{persona}");
        }
    }

    #[test]
    fn a_placeholder_for_a_shared_session_window_goes_with_its_window() {
        let at = At::from_unix_millis(5);
        let mut table = RouteTable::default();
        table.attach_muse_tmux_windows(&windows(&[("muse", "kacey")]), &at);
        assert!(table.get("kacey").is_some());
        table.attach_muse_tmux_windows(&windows(&[("muse", "mira")]), &at);
        assert!(table.get("kacey").is_none(), "{:?}", table.entries);
        assert!(table.get("mira").is_some());
    }

    #[test]
    fn a_live_grok_window_without_a_route_becomes_a_placeholder() {
        let at = At::from_unix_millis(5);
        let mut table = RouteTable::default();
        table.attach_grok_tmux_placeholders(
            &windows(&[("Felicia", "grok"), ("grok", "grok"), ("Ada", "claude")]),
            &at,
        );
        match &table.get("felicia").unwrap().route {
            VendorRoute::Grok {
                session_id,
                tmux_target,
                cwd,
                tmux_pane,
            } => {
                assert!(session_id.is_empty());
                assert_eq!(tmux_target.as_deref(), Some("Felicia:grok"));
                assert!(cwd.is_none());
                assert!(tmux_pane.is_none());
            }
            other => panic!("{other:?}"),
        }
        assert!(table.get("grok").is_none());
        assert!(table.get("ada").is_none());
        table.upsert(
            "felicia",
            VendorRoute::Grok {
                session_id: "gs-1".into(),
                cwd: None,
                tmux_target: Some("Felicia:grok".into()),
                tmux_pane: None,
            },
            at.clone(),
        );
        table.attach_grok_tmux_placeholders(&windows(&[("Felicia", "grok")]), &at);
        match &table.get("felicia").unwrap().route {
            VendorRoute::Grok { session_id, .. } => assert_eq!(session_id, "gs-1"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_live_muse_window_keeps_the_persona_when_a_grok_window_is_also_live() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let _tmux = TmuxEnv::fake(tmp.path(), &home, &["muse:felicia", "Felicia:grok"]);
        let mut table = RouteTable::default();
        table.discover_routes(Some(&home), At::from_unix_millis(9));
        assert!(
            matches!(
                table.get("felicia").map(|entry| &entry.route),
                Some(VendorRoute::Muse { .. })
            ),
            "{:?}",
            table.entries
        );
    }

    #[test]
    fn discover_routes_inserts_a_grok_placeholder_for_a_live_window() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let _tmux = TmuxEnv::fake(tmp.path(), &home, &["Felicia:grok"]);
        let mut table = RouteTable::default();
        table.discover_routes(Some(&home), At::from_unix_millis(9));
        match &table.get("felicia").unwrap().route {
            VendorRoute::Grok {
                session_id,
                tmux_target,
                ..
            } => {
                assert!(session_id.is_empty());
                assert_eq!(tmux_target.as_deref(), Some("Felicia:grok"));
            }
            other => panic!("{other:?}"),
        }
    }

    fn agy_route(conversation_id: &str) -> VendorRoute {
        VendorRoute::Agy {
            conversation_id: conversation_id.into(),
            ls_pid: None,
            ls_address: None,
            csrf_token: None,
            agentapi_exe: None,
        }
    }

    fn codex_route(thread_id: &str) -> VendorRoute {
        VendorRoute::Codex {
            thread_id: thread_id.into(),
            cwd: None,
        }
    }

    #[test]
    fn decommissioned_session_ids_are_dropped_on_discovery() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let at = At::from_unix_millis(1);
        let ids = std::collections::BTreeSet::from([
            "conv-1".to_string(),
            "antigravity:ada".to_string(),
            "codex:bob".to_string(),
            "grok:dee".to_string(),
        ]);
        save_decommissioned_session_ids(Some(home), &ids).unwrap();
        let mut table = RouteTable::default();
        table.upsert("ada", agy_route("conv-1"), at.clone());
        table.upsert("bob", codex_route("thread-bob"), at.clone());
        table.upsert(
            "cara",
            VendorRoute::Webhook {
                socket_path: home.join("c.sock"),
            },
            at.clone(),
        );
        table.upsert(
            "dee",
            VendorRoute::Grok {
                session_id: String::new(),
                cwd: None,
                tmux_target: Some("Dee:grok".into()),
                tmux_pane: None,
            },
            at.clone(),
        );
        table.drop_decommissioned_routes(Some(home));
        assert!(table.get("ada").is_none(), "native conversation id");
        assert!(
            table.get("bob").is_some(),
            "thread-bob is not banned by codex:bob"
        );
        assert!(table.get("cara").is_some(), "a webhook route stays");
        assert!(
            table.get("dee").is_some(),
            "an empty grok placeholder is not banned by grok:dee"
        );

        let ids = std::collections::BTreeSet::from(["thread-bob".to_string()]);
        save_decommissioned_session_ids(Some(home), &ids).unwrap();
        let mut table = RouteTable::default();
        table.upsert("bob", codex_route("thread-bob"), at.clone());
        table.upsert("eve", codex_route("thread-eve"), at.clone());
        table.drop_decommissioned_routes(Some(home));
        assert!(table.get("bob").is_none(), "thread-bob is banned");
        assert!(table.get("eve").is_some(), "a different thread stays");

        let mut table = RouteTable::default();
        table.upsert("bob", codex_route("thread-bob-2"), at);
        table.drop_decommissioned_routes(Some(home));
        assert!(
            table.get("bob").is_some(),
            "a different thread id for bob stays"
        );
    }

    #[test]
    fn a_reregistered_route_survives_discover_routes_and_the_old_native_id_does_not() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let transcript = agy_registration_fixture(home, "conv-new", "Ada", 5);
        fs::create_dir_all(home).unwrap();
        let banned = std::collections::BTreeSet::from([
            "conv-old".to_string(),
            "gs-old".to_string(),
            "antigravity:ada".to_string(),
            "grok:ada".to_string(),
        ]);
        save_decommissioned_session_ids(Some(home), &banned).unwrap();
        let at = At::from_unix_millis(5);
        let mut table = RouteTable::default();
        table.upsert("ada", agy_route("conv-new"), at.clone());
        table.discover_routes(Some(home), at.clone());
        match table.get("ada").map(|entry| &entry.route) {
            Some(VendorRoute::Agy {
                conversation_id, ..
            }) => assert_eq!(conversation_id, "conv-new"),
            other => panic!("new native id must stay deliverable: {other:?}"),
        }
        // The later Grok-only cases must not rediscover this Antigravity session.
        fs::remove_file(transcript).unwrap();

        table.upsert(
            "ada",
            VendorRoute::Grok {
                session_id: String::new(),
                cwd: None,
                tmux_target: Some("Ada:grok".into()),
                tmux_pane: None,
            },
            at.clone(),
        );
        table.discover_routes(Some(home), at.clone());
        assert!(
            matches!(
                table.get("ada").map(|entry| &entry.route),
                Some(VendorRoute::Grok { session_id, .. }) if session_id.is_empty()
            ),
            "an empty grok id is not banned by grok:ada"
        );

        table.upsert(
            "ada",
            VendorRoute::Grok {
                session_id: "gs-old".into(),
                cwd: None,
                tmux_target: Some("Ada:grok".into()),
                tmux_pane: None,
            },
            at.clone(),
        );
        table.discover_routes(Some(home), at.clone());
        assert!(table.get("ada").is_none(), "the old grok id stays banned");

        table.upsert("ada", agy_route("conv-old"), at.clone());
        table.discover_routes(Some(home), at);
        assert!(
            table.get("ada").is_none(),
            "the old conversation id stays banned"
        );
    }

    #[test]
    fn list_tmux_windows_answers_none_when_tmux_cannot_be_trusted() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        {
            let _env = TmuxEnv::fake_script(tmp.path(), &home, "echo 'Kacey:muse'\nexit 1\n");
            assert_eq!(
                list_tmux_windows_within(Duration::from_secs(5)),
                None,
                "a non-zero exit (no server running) is not an empty list"
            );
        }
        {
            let _env = TmuxEnv::fake_script(tmp.path(), &home, "exec sleep 5\n");
            let started = Instant::now();
            assert_eq!(list_tmux_windows_within(Duration::from_millis(200)), None);
            assert!(started.elapsed() < Duration::from_secs(3));
        }
        {
            let _env = TmuxEnv::unset();
            // SAFETY: `_env` holds the tmux env lock and restores the variable.
            unsafe { std::env::set_var("FL_WIRE_TMUX_BIN", tmp.path().join("missing-tmux")) };
            assert_eq!(list_tmux_windows_within(Duration::from_secs(1)), None);
        }
        {
            let _env = TmuxEnv::fake_script(tmp.path(), &home, "echo 'Kacey:muse'\n");
            assert_eq!(
                list_tmux_windows_within(Duration::from_secs(5)),
                Some(windows(&[("Kacey", "muse")]))
            );
        }
        {
            let _env = TmuxEnv::fake_script(tmp.path(), &home, ":\n");
            assert_eq!(
                list_tmux_windows_within(Duration::from_secs(5)),
                Some(Vec::new()),
                "an answer with no windows is trustworthy"
            );
        }
    }

    #[test]
    fn list_tmux_windows_reads_output_larger_than_a_pipe_without_waiting_for_the_timeout() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        // ~190 KB of output, far above a 64 KB pipe buffer.
        let body =
            "i=0\nwhile [ $i -lt 8000 ]; do echo \"Session$i:window-name-$i\"; i=$((i+1)); done\n";
        let _env = TmuxEnv::fake_script(tmp.path(), &home, body);
        let started = Instant::now();
        let found = list_tmux_windows_within(Duration::from_secs(3)).expect("tmux answered");
        assert_eq!(found.len(), 8000);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "stalled on a full pipe: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn an_untrustworthy_tmux_answer_leaves_muse_routes_untouched() {
        for body in ["exit 1\n", "exec sleep 10\n"] {
            let tmp = TempDir::new().unwrap();
            let home = tmp.path().join("home");
            fs::create_dir_all(&home).unwrap();
            let _env = TmuxEnv::fake_script(tmp.path(), &home, body);
            let mut table = RouteTable::default();
            table.upsert(
                "kacey",
                muse_route_with("", Some("Kacey:muse")),
                At::from_unix_millis(1),
            );
            table.upsert(
                "mira",
                muse_route_with("s", Some("muse:mira")),
                At::from_unix_millis(1),
            );
            let before = table.entries.clone();
            table.discover_routes(Some(&home), At::from_unix_millis(5));
            assert_eq!(
                table.entries, before,
                "placeholder dropped or target changed ({body:?})"
            );
        }
    }

    #[test]
    fn a_placeholder_muse_route_does_not_shadow_a_grok_transcript_route() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let grok_dir = home.join(".grok/sessions");
        fs::create_dir_all(&grok_dir).unwrap();
        let transcript = grok_dir.join("g1.jsonl");
        fs::write(
            &transcript,
            "{\"session_id\":\"g1\",\"agentName\":\"Kacey\",\"cwd\":\"/tmp\"}\n",
        )
        .unwrap();
        // Older than the scan, so a placeholder stamped at the scan time would beat it.
        set_mtime(&transcript, 1);
        let _env = TmuxEnv::fake(tmp.path(), &home, &["Kacey:muse", "Felix:muse"]);
        let mut table = RouteTable::default();
        table.discover_routes(Some(&home), At::from_unix_millis(5));
        assert!(
            matches!(
                muse_route_of(&table, "kacey"),
                Some(VendorRoute::Grok { .. })
            ),
            "{:?}",
            table.get("kacey")
        );
        // A persona with no route of its own still gets the placeholder.
        assert_eq!(muse_parts(&muse_route_of(&table, "felix").unwrap()).0, "");
    }

    #[test]
    fn muse_discovery_reads_only_the_head_of_a_transcript() {
        let tmp = TempDir::new().unwrap();
        let dir = muse_sessions_dir(tmp.path())
            .join("2026/10/27")
            .join(MUSE_SESSION_A);
        let path =
            write_muse_transcript(&dir, &muse_transcript_rows(&muse_hydration_prompt("Kacey")));
        {
            use std::io::Write;
            let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
            // Past the head: 4 MB of rows, then bytes that are not UTF-8 (a whole-file
            // `read_to_string` would refuse the file).
            let row = format!("\n{{\"pad\":\"{}\"}}", "x".repeat(990));
            for _ in 0..4000 {
                file.write_all(row.as_bytes()).unwrap();
            }
            file.write_all(b"\n\xff\xfe\n").unwrap();
        }
        assert!(fs::metadata(&path).unwrap().len() > 3_000_000);
        assert!(fs::read_to_string(&path).is_err());
        let mut table = RouteTable::default();
        table.discover_routes(Some(tmp.path()), At::from_unix_millis(5));
        let (session_id, _, _) = muse_parts(&muse_route_of(&table, "kacey").unwrap());
        assert_eq!(session_id, MUSE_SESSION_A);
    }

    #[test]
    fn a_codex_subagent_rollout_has_no_thread_id_and_a_scan_reads_only_the_head() {
        let tmp = TempDir::new().unwrap();
        let day = tmp.path().join(".codex/sessions/2026/11/05");
        fs::create_dir_all(&day).unwrap();
        let thread_id = "01a0a9ce-1111-7222-8333-ea40058ed587";
        let sub = day.join(format!("rollout-2026-11-05T00-00-00-{thread_id}.jsonl"));
        fs::write(
            &sub,
            serde_json::json!({
                "type": "session_meta",
                "payload": {
                    "id": thread_id,
                    "cwd": "/tmp/sub",
                    "thread_source": "subagent",
                    "parent_thread_id": "parent-thread",
                    "source": { "subagent": { "id": "child" } }
                }
            })
            .to_string()
                + "\n",
        )
        .unwrap();
        assert_eq!(extract_codex_rollout_meta(&sub).0, "");

        let source_only = day.join("source-only.jsonl");
        fs::write(
            &source_only,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"other\",\"source\":{\"subagent\":{}}}}\n",
        )
        .unwrap();
        assert_eq!(extract_codex_rollout_meta(&source_only).0, "");

        let parent_only = day.join("parent-only.jsonl");
        fs::write(
            &parent_only,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"parented\",\"parent_thread_id\":\"parent-thread\"}}\n",
        )
        .unwrap();
        assert_eq!(extract_codex_rollout_meta(&parent_only).0, "");

        let source_kind = day.join("thread-source-only.jsonl");
        fs::write(
            &source_kind,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"sourced\",\"thread_source\":\"subagent\"}}\n",
        )
        .unwrap();
        assert_eq!(extract_codex_rollout_meta(&source_kind).0, "");

        let head = day.join("head.jsonl");
        {
            use std::io::Write;
            let mut file = fs::File::create(&head).unwrap();
            writeln!(
                file,
                "{}",
                serde_json::json!({
                    "type": "user",
                    "content": "agent-msg register Freya"
                })
            )
            .unwrap();
            file.write_all(&vec![0xff; 2_000_000]).unwrap();
        }
        assert!(fs::read_to_string(&head).is_err());
        assert_eq!(
            scan_transcript_for_persona(&head, 50).as_deref(),
            Some("freya")
        );
    }

    #[test]
    fn parse_tmux_windows_splits_at_the_first_colon() {
        assert_eq!(
            parse_tmux_windows("Kacey:muse\nFelix:a:b\n\nnocolon\n:x\ny:\n"),
            windows(&[("Kacey", "muse"), ("Felix", "a:b")])
        );
    }

    #[test]
    fn resolve_cwd_persona_md_ignores_name_agnostic_prose_and_accepts_proper_names() {
        let tmp = TempDir::new().unwrap();
        let competitive_dir = tmp.path().join("kaggle-gemma4");
        fs::create_dir_all(&competitive_dir).unwrap();
        fs::write(
            competitive_dir.join("PERSONA.md"),
            "# Kaggle Persona\n\nYou are highly competitive and rigorous.\nYou are a resident engineer.\n",
        )
        .unwrap();
        assert_eq!(resolve_cwd_persona_md(&competitive_dir), None);

        let named_dir = tmp.path().join("ai-lab");
        fs::create_dir_all(&named_dir).unwrap();
        fs::write(
            named_dir.join("PERSONA.md"),
            "# Lab Manager\n\nYou are Jarvis, the lab manager for AI-Lab-0.\n",
        )
        .unwrap();
        assert_eq!(
            resolve_cwd_persona_md(&named_dir).as_deref(),
            Some("jarvis")
        );
    }

    #[test]
    fn scan_text_for_persona_prefers_later_registration_and_latest_field_over_early_typo() {
        let prompt_then_reg = [
            serde_json::json!({
                "type": "USER_INPUT",
                "content": "You are Kayce, take up residence in Kaggle-Gemma4."
            })
            .to_string(),
            serde_json::json!({
                "type": "PLANNER_RESPONSE",
                "tool_calls": [{
                    "name": "run_command",
                    "arguments": {"cmd": "agent-msg register Kacey --project Kaggle-Gemma4 --role journalist"}
                }]
            })
            .to_string(),
        ]
        .join("\n");
        assert_eq!(
            scan_text_for_persona(&prompt_then_reg, 50).as_deref(),
            Some("kacey")
        );

        let typo_then_rename = [
            serde_json::json!({"type": "custom-title", "customTitle": "Ferrisd"}).to_string(),
            serde_json::json!({"type": "user", "message": {"role": "user", "content": "You are Ferrisd"}}).to_string(),
            serde_json::json!({"type": "agent-name", "agentName": "Ferris"}).to_string(),
        ]
        .join("\n");
        assert_eq!(
            scan_text_for_persona(&typo_then_rename, 50).as_deref(),
            Some("ferris")
        );

        let prose_register_conjunction = serde_json::json!({
            "type": "USER_INPUT",
            "content": "Please take the name of Fenris and take up residence in FerroLoop. Register with fl agent register and agent-msg register, and check in with Ferris."
        })
        .to_string();
        assert_eq!(
            scan_text_for_persona(&prose_register_conjunction, 50).as_deref(),
            Some("fenris")
        );
    }

    #[test]
    fn discover_codex_routes_ignores_thread_name_and_title_in_session_index() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let codex_dir = home.join(".codex");
        fs::create_dir_all(&codex_dir).unwrap();
        fs::write(
            codex_dir.join("session_index.jsonl"),
            [
                serde_json::json!({
                    "id": "thread-update",
                    "thread_name": "Update",
                    "title": "Update",
                    "cwd": "/tmp"
                })
                .to_string(),
                serde_json::json!({
                    "id": "thread-kage",
                    "agentName": "Kage",
                    "thread_name": "Update",
                    "cwd": "/tmp"
                })
                .to_string(),
            ]
            .join("\n"),
        )
        .unwrap();

        let mut table = RouteTable::default();
        table.discover_routes(Some(home), At::from_unix_millis(100));
        assert!(table.get("update").is_none(), "{:?}", table.entries);
        assert!(matches!(
            table.get("kage").map(|e| &e.route),
            Some(VendorRoute::Codex { thread_id, .. }) if thread_id == "thread-kage"
        ));
    }

    #[test]
    fn route_table_deduplicates_session_ids_and_resolve_persona_prefers_registered_newest() {
        let agy = |cid: &str| VendorRoute::Agy {
            conversation_id: cid.into(),
            ls_pid: None,
            ls_address: None,
            csrf_token: None,
            agentapi_exe: None,
        };
        let mut table = RouteTable::default();
        table.upsert("highly", agy("conv-kayla"), At::from_unix_millis(100));
        table.upsert("kayla", agy("conv-kayla"), At::from_unix_millis(200));
        assert!(table.get("highly").is_none(), "stale alias must be evicted");
        assert!(table.get("kayla").is_some());

        // Merging an older alias with the same conversation_id must be ignored;
        // merging a newer canonical persona must evict the older alias.
        let mut stale_other = RouteTable::default();
        stale_other.upsert("highly", agy("conv-kayla"), At::from_unix_millis(150));
        stale_other.upsert("sonya", agy("conv-sonya"), At::from_unix_millis(300));
        table.upsert("github-44", agy("conv-sonya"), At::from_unix_millis(100));
        table.merge_newer(stale_other);
        assert!(table.get("highly").is_none());
        assert!(table.get("kayla").is_some());
        assert!(table.get("github-44").is_none());
        assert!(table.get("sonya").is_some());

        // When routes loaded from disk contain both an alphabetically earlier alias and the
        // canonical persona for the same session ID, resolve_persona_traced prefers the
        // registered active agent (and newest timestamp when neither is registered).
        let mut raw_routes = RouteTable::default();
        raw_routes.entries.insert(
            "highly".into(),
            RouteEntry {
                persona: "highly".into(),
                route: agy("conv-shared"),
                updated_at: At::from_unix_millis(500),
                identity_at: None,
                explicit: false,
                hosted_id: None,
                hydration_generation: None,
            },
        );
        raw_routes.entries.insert(
            "kayla".into(),
            RouteEntry {
                persona: "kayla".into(),
                route: agy("conv-shared"),
                updated_at: At::from_unix_millis(200),
                identity_at: None,
                explicit: false,
                hosted_id: None,
                hydration_generation: None,
            },
        );
        let store = MemStore::default();
        store
            .register_agent(RegisterAgentArgs {
                display_name: "Kayla".into(),
                email: None,
                project: "kaggle-gemma4".into(),
                role: AgentRole::Lead,
                extra_areas: vec![],
                create_area: true,
                replace: false,
                at: At::from_unix_millis(10),
                by: "owner".into(),
            })
            .unwrap();
        let env = |k: &str| {
            (k == "ANTIGRAVITY_CONVERSATION_ID").then(|| std::ffi::OsString::from("conv-shared"))
        };
        let tmp = TempDir::new().unwrap();
        let (resolved_reg, _) = resolve_persona_traced(
            None,
            None,
            tmp.path(),
            Some(&store),
            || raw_routes.clone(),
            &env,
        );
        assert_eq!(resolved_reg, "kayla");

        // Without registry, the newer updated_at wins over alphabetical order.
        raw_routes.entries.get_mut("kayla").unwrap().updated_at = At::from_unix_millis(600);
        let (resolved_newer, _) =
            resolve_persona_traced(None, None, tmp.path(), None, || raw_routes, &env);
        assert_eq!(resolved_newer, "kayla");
    }

    #[test]
    fn agy_subagent_transcript_starting_with_system_message_is_ignored() {
        let tmp = TempDir::new().unwrap();
        let transcript = tmp.path().join("transcript.jsonl");
        fs::write(
            &transcript,
            concat!(
                "{\"step_index\":0,\"type\":\"SYSTEM_MESSAGE\",\"content\":\"The following is a <SYSTEM_MESSAGE> not actually sent by the user.\\n\\n<SYSTEM_MESSAGE>\\nYou are a subagent.\\n</SYSTEM_MESSAGE>\"}\n",
                "{\"step_index\":1,\"type\":\"PLANNER_RESPONSE\",\"content\":\"agent-msg register Felicity\"}\n",
            ),
        )
        .unwrap();
        assert_eq!(scan_transcript_for_persona(&transcript, 50), None);
    }

    fn agy_registration_fixture(
        home: &Path,
        conversation_id: &str,
        persona: &str,
        ms: u64,
    ) -> PathBuf {
        let text = serde_json::json!({"type": "USER_INPUT", "content": format!("fl wire register {persona}")});
        agy_transcript_fixture(home, conversation_id, &text.to_string(), ms)
    }

    fn agy_transcript_fixture(home: &Path, conversation_id: &str, text: &str, ms: u64) -> PathBuf {
        let transcript = home
            .join(".gemini/antigravity-cli/brain")
            .join(conversation_id)
            .join(".system_generated/logs/transcript.jsonl");
        fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        fs::write(&transcript, text).unwrap();
        set_mtime(&transcript, ms);
        transcript
    }

    thread_local! {
        pub(super) static AGY_REJECTION_DIAGNOSTICS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    #[test]
    fn passive_agy_rejections_explain_source_failure_without_secrets() {
        for (source, reason) in [
            ("missing", "missing"),
            ("unreadable", "unreadable"),
            ("zero", "zero"),
            ("mismatch", "mismatch"),
            ("generic", "no supported identity"),
            ("valid", ""),
            ("explicit", ""),
        ] {
            let home = TempDir::new().unwrap();
            let conversation = "cached-conversation\nforged-line";
            let path = agy_registration_fixture(
                home.path(),
                conversation,
                if source == "mismatch" {
                    "Kacey"
                } else {
                    "Kayla"
                },
                100,
            );
            match source {
                "missing" => fs::remove_file(&path).unwrap(),
                "unreadable" => {
                    fs::remove_file(&path).unwrap();
                    fs::create_dir(&path).unwrap();
                }
                "zero" => set_mtime(&path, 0),
                "generic" | "explicit" => {
                    fs::write(&path, "{\"type\":\"GENERIC\",\"content\":\"secret-transcript fl wire register Kayla\"}").unwrap();
                    set_mtime(&path, 100);
                }
                _ => {}
            }
            let route = VendorRoute::Agy {
                conversation_id: conversation.into(),
                ls_pid: None,
                ls_address: Some("secret-endpoint".into()),
                csrf_token: Some("secret-csrf".into()),
                agentapi_exe: None,
            };
            let mut table = RouteTable::default();
            if source == "explicit" {
                table.upsert_explicit("kayla", route, At::from_unix_millis(9000));
            } else {
                table.upsert("kayla", route, At::from_unix_millis(9000));
            }
            AGY_REJECTION_DIAGNOSTICS.with(|lines| lines.borrow_mut().clear());
            table.discover_agy_routes(home.path(), false, &At::from_unix_millis(10000));
            let lines =
                AGY_REJECTION_DIAGNOSTICS.with(|lines| std::mem::take(&mut *lines.borrow_mut()));
            if reason.is_empty() {
                assert!(lines.is_empty(), "{source}: {lines:?}");
                continue;
            }
            assert_eq!(lines.len(), 1, "{source}");
            let line = &lines[0];
            let fields: serde_json::Value = serde_json::from_str(
                line.strip_prefix("fl-wire: rejected passive Antigravity claim ")
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(fields.as_object().unwrap().len(), 3);
            assert_eq!(fields["persona"], "kayla");
            assert_eq!(fields["conversation_id"], conversation);
            assert!(fields["reason"].as_str().unwrap().contains(reason));
            assert!(
                line.contains("kayla")
                    && line.contains("cached-conversation")
                    && line.contains(reason),
                "{source}: {line}"
            );
            assert_eq!(line.lines().count(), 1);
            for secret in ["secret-csrf", "secret-endpoint", "secret-transcript"] {
                assert!(!line.contains(secret));
            }
        }
    }

    #[test]
    fn passive_agy_cached_claims_require_current_bounded_source_evidence() {
        for source in [
            "generic",
            "mismatch",
            "missing",
            "unreadable",
            "zero-mtime",
            "subagent",
            "beyond-head",
        ] {
            let home = TempDir::new().unwrap();
            let valid = "{\"type\":\"USER_INPUT\",\"content\":\"fl wire register Kayla\"}\n";
            let text = match source {
                "generic" => "{\"type\":\"GENERIC\",\"content\":\"diff --git a/example b/example\\n+fl wire register Kayla\"}\n".to_string(),
                "mismatch" => "{\"type\":\"USER_INPUT\",\"content\":\"fl wire register Kacey\"}\n".to_string(),
                "subagent" => format!("{{\"type\":\"SYSTEM_MESSAGE\",\"content\":\"<SYSTEM_MESSAGE> subagent </SYSTEM_MESSAGE>\"}}\n{valid}"),
                "beyond-head" => format!("{}{valid}", "{\"type\":\"GENERIC\",\"content\":\"output\"}\n".repeat(50)),
                _ => valid.to_string(),
            };
            let transcript = agy_transcript_fixture(home.path(), "cached-conversation", &text, 100);
            match source {
                "missing" => fs::remove_file(&transcript).unwrap(),
                "unreadable" => {
                    fs::remove_file(&transcript).unwrap();
                    fs::create_dir(&transcript).unwrap();
                }
                "zero-mtime" => set_mtime(&transcript, 0),
                _ => {}
            }
            let mut table = RouteTable::default();
            table.upsert(
                "kayla",
                agy_route("cached-conversation"),
                At::from_unix_millis(9000),
            );
            table.discover_agy_routes(home.path(), false, &At::from_unix_millis(10000));
            assert!(
                table.get("kayla").is_none(),
                "invalid cached source {source}"
            );
            if source == "mismatch" {
                assert!(
                    matches!(table.get("kacey").map(|e| &e.route), Some(VendorRoute::Agy { conversation_id, .. }) if conversation_id == "cached-conversation")
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn passive_agy_unreadable_regular_transcript_does_not_validate_cached_claim() {
        use std::os::unix::fs::PermissionsExt;

        // Root can read mode-000 files, so permission denial cannot be exercised as root.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let home = TempDir::new().unwrap();
        let transcript =
            agy_registration_fixture(home.path(), "unreadable-conversation", "Kayla", 100);
        let permissions = fs::metadata(&transcript).unwrap().permissions();
        fs::set_permissions(&transcript, fs::Permissions::from_mode(0o0)).unwrap();
        assert!(fs::read_to_string(&transcript).is_err());
        assert_eq!(mtime_ms(&transcript), 100);
        let mut table = RouteTable::default();
        table.upsert(
            "kayla",
            agy_route("unreadable-conversation"),
            At::from_unix_millis(9000),
        );
        table.discover_agy_routes(home.path(), false, &At::from_unix_millis(10000));
        fs::set_permissions(&transcript, permissions).unwrap();
        assert!(table.get("kayla").is_none());
    }

    #[test]
    fn passive_agy_missing_brain_removes_concrete_and_placeholder_but_keeps_explicit() {
        let home = TempDir::new().unwrap();
        let mut table = RouteTable::default();
        table.upsert(
            "kayla",
            agy_route("cached-conversation"),
            At::from_unix_millis(9000),
        );
        table.upsert(
            "kacey",
            agy_route("antigravity:kacey"),
            At::from_unix_millis(9000),
        );
        table.upsert_explicit(
            "ferris",
            agy_route("explicit-conversation"),
            At::from_unix_millis(100),
        );
        table.upsert_explicit(
            "feira",
            agy_route("antigravity:feira"),
            At::from_unix_millis(100),
        );
        table.discover_agy_routes(home.path(), false, &At::from_unix_millis(10000));
        assert!(table.get("kayla").is_none());
        assert!(table.get("kacey").is_none());
        for persona in ["ferris", "feira"] {
            assert!(table.get(persona).unwrap().explicit);
        }
    }

    #[test]
    fn passive_agy_zero_mtime_is_never_scan_time_evidence() {
        let home = TempDir::new().unwrap();
        agy_transcript_fixture(
            home.path(),
            "zero-conversation",
            "{\"type\":\"USER_INPUT\",\"content\":\"fl wire register Kayla\"}\n",
            0,
        );
        let mut table = RouteTable::default();
        table.discover_agy_routes(home.path(), false, &At::from_unix_millis(9000));
        assert!(table.get("kayla").is_none());
        table.upsert_explicit(
            "kayla",
            agy_route("antigravity:kayla"),
            At::from_unix_millis(0),
        );
        table.discover_agy_routes(home.path(), false, &At::from_unix_millis(9000));
        let entry = table.get("kayla").unwrap();
        assert!(entry.explicit);
        assert!(
            matches!(&entry.route, VendorRoute::Agy { conversation_id, .. } if conversation_id == "antigravity:kayla")
        );
    }

    #[test]
    fn passive_agy_valid_cached_claim_is_retained_and_explicit_claims_ignore_bad_sources() {
        let home = TempDir::new().unwrap();
        agy_transcript_fixture(
            home.path(),
            "valid-conversation",
            "{\"type\":\"PLANNER_RESPONSE\",\"tool_calls\":[{\"name\":\"run_command\",\"args\":{\"CommandLine\":\"fl wire register Kayla\"}}]}\n",
            100,
        );
        agy_transcript_fixture(
            home.path(),
            "explicit-conversation",
            "{\"type\":\"GENERIC\",\"content\":\"fl wire register Kacey\"}\n",
            100,
        );
        let mut table = RouteTable::default();
        table.upsert(
            "kayla",
            agy_route("valid-conversation"),
            At::from_unix_millis(500),
        );
        table.upsert_explicit(
            "ferris",
            agy_route("explicit-conversation"),
            At::from_unix_millis(500),
        );
        table.discover_agy_routes(home.path(), false, &At::from_unix_millis(9000));
        assert_eq!(table.get("kayla").unwrap().updated_at.unix_millis(), 500);
        assert!(!table.get("kayla").unwrap().explicit);
        assert!(table.get("ferris").unwrap().explicit);
        assert!(table.get("kacey").is_none());
    }

    #[test]
    fn passive_agy_loaded_claims_are_revalidated_before_snapshot_merge() {
        for legacy in [false, true] {
            let tmp = TempDir::new().unwrap();
            let home = tmp.path().join("home");
            agy_transcript_fixture(
                &home,
                "quoted-conversation",
                "{\"type\":\"GENERIC\",\"content\":\"fl wire register Kayla\"}\n",
                100,
            );
            let primary = tmp.path().join("primary.json");
            let fallback = tmp.path().join("legacy.json");
            if legacy {
                fs::write(&fallback, serde_json::json!({"kayla":{"vendor":"agy","conversation_id":"quoted-conversation","updated_at":"2026-11-09T10:00:00Z"}}).to_string()).unwrap();
            } else {
                let mut saved = RouteTable::default();
                saved.upsert(
                    "kayla",
                    agy_route("quoted-conversation"),
                    At::from_unix_millis(9000),
                );
                saved.save(Some(&primary)).unwrap();
            }
            let mut loaded = RouteTable::load_with_legacy(&primary, Some(&fallback));
            assert!(loaded.get("kayla").is_some());
            loaded.discover_routes(Some(&home), At::from_unix_millis(10000));
            assert!(loaded.get("kayla").is_none(), "legacy={legacy}");
            loaded.save(Some(&primary)).unwrap();
            assert!(
                RouteTable::load_with_legacy(&primary, Some(&fallback))
                    .get("kayla")
                    .is_none()
            );
            let mut real = RouteTable::default();
            real.upsert(
                "kayla",
                codex_route("real-thread"),
                At::from_unix_millis(200),
            );
            real.merge_newer(loaded);
            assert_eq!(real.get("kayla").unwrap().route, codex_route("real-thread"));
        }
    }

    #[test]
    fn passive_agy_pruning_survives_dispatch_reconciliation_without_removing_explicit_registration()
    {
        for daemon in [false, true] {
            for concurrent_explicit in [false, true] {
                let home = TempDir::new().unwrap();
                agy_transcript_fixture(
                    home.path(),
                    "quoted-conversation",
                    "{\"type\":\"GENERIC\",\"content\":\"fl wire register Kayla\"}\n",
                    100,
                );
                let mut base = RouteTable::default();
                base.upsert(
                    "kayla",
                    agy_route("quoted-conversation"),
                    At::from_unix_millis(9000),
                );
                let mut working = base.clone();
                working.discover_routes(Some(home.path()), At::from_unix_millis(10000));
                let mut live = base.clone();
                if concurrent_explicit {
                    live.upsert_explicit(
                        "kayla",
                        codex_route("real-thread"),
                        At::from_unix_millis(9500),
                    );
                }
                if daemon {
                    live.merge_daemon_dispatch_result(&base, working);
                } else {
                    live.merge_dispatch_result(&base, working);
                }
                if concurrent_explicit {
                    let entry = live.get("kayla").unwrap();
                    assert!(entry.explicit);
                    assert_eq!(entry.route, codex_route("real-thread"));
                } else {
                    assert!(live.get("kayla").is_none(), "daemon={daemon}");
                }
            }
        }
    }

    #[test]
    fn passive_agy_invalid_claim_replacement_survives_all_merge_paths() {
        for merge in ["direct", "dispatch", "daemon"] {
            for concurrent in ["none", "explicit", "valid-newer"] {
                let home = TempDir::new().unwrap();
                agy_transcript_fixture(
                    home.path(),
                    "quoted-conversation",
                    "{\"type\":\"GENERIC\",\"content\":\"fl wire register Kayla\"}\n",
                    100,
                );
                agy_registration_fixture(home.path(), "real-conversation", "Kayla", 200);
                let mut base = RouteTable::default();
                base.upsert(
                    "kayla",
                    agy_route("quoted-conversation"),
                    At::from_unix_millis(9000),
                );
                let mut working = base.clone();
                working.discover_routes(Some(home.path()), At::from_unix_millis(10000));
                let replacement = working.get("kayla").unwrap().clone();
                assert_eq!(replacement.updated_at.unix_millis(), 200);
                let mut live = base.clone();
                match concurrent {
                    "explicit" => live.upsert_explicit(
                        "kayla",
                        agy_route("explicit-conversation"),
                        At::from_unix_millis(9500),
                    ),
                    "valid-newer" => live.upsert(
                        "kayla",
                        agy_route("valid-newer-conversation"),
                        At::from_unix_millis(9500),
                    ),
                    _ => {}
                }
                let concurrent_entry = live.get("kayla").unwrap().clone();
                match merge {
                    "direct" => live.merge_newer(working),
                    "dispatch" => live.merge_dispatch_result(&base, working),
                    "daemon" => live.merge_daemon_dispatch_result(&base, working),
                    _ => unreachable!(),
                }
                let expected = if concurrent == "none" {
                    &replacement
                } else {
                    &concurrent_entry
                };
                assert_eq!(live.get("kayla"), Some(expected), "{merge}, {concurrent}");
                // A stale snapshot must not put the exact rejected claim back.
                live.merge_newer(base.clone());
                assert_eq!(
                    live.get("kayla"),
                    Some(expected),
                    "replay: {merge}, {concurrent}"
                );
            }
        }
    }

    #[test]
    fn passive_agy_rejection_does_not_change_valid_route_freshness_policy() {
        let mut newer = RouteTable::default();
        newer.upsert(
            "kayla",
            agy_route("valid-newer"),
            At::from_unix_millis(9000),
        );
        let expected = newer.get("kayla").unwrap().clone();
        let mut older = RouteTable::default();
        older.upsert("kayla", agy_route("valid-older"), At::from_unix_millis(200));
        newer.merge_newer(older);
        assert_eq!(newer.get("kayla"), Some(&expected));
    }

    fn assert_passive_agy_confirmation_survives_replay_without_prior_rejection(fresh: bool) {
        let home = TempDir::new().unwrap();
        let mut old = RouteTable::default();
        let mut route = agy_route("revalidated-conversation");
        if let VendorRoute::Agy { agentapi_exe, .. } = &mut route {
            *agentapi_exe = Some(PathBuf::from("agy"));
        }
        old.upsert("kayla", route, At::from_unix_millis(9000));
        let mut rejected = old.clone();
        rejected.discover_agy_routes(home.path(), false, &At::from_unix_millis(10000));
        assert!(rejected.get("kayla").is_none());
        agy_registration_fixture(home.path(), "revalidated-conversation", "Kayla", 9000);
        let mut confirmed = rejected.clone();
        confirmed.discover_agy_routes(home.path(), false, &At::from_unix_millis(10001));
        assert_eq!(confirmed.get("kayla"), old.get("kayla"));

        for merge in ["direct", "dispatch", "daemon"] {
            let base = if fresh {
                RouteTable::default()
            } else {
                old.clone()
            };
            let mut live = base.clone();
            match merge {
                "direct" => live.merge_newer(confirmed.clone()),
                "dispatch" => live.merge_dispatch_result(&base, confirmed.clone()),
                "daemon" => live.merge_daemon_dispatch_result(&base, confirmed.clone()),
                _ => unreachable!(),
            }
            assert_eq!(live.get("kayla"), old.get("kayla"), "confirmation: {merge}");
            live.merge_newer(rejected.clone());
            assert_eq!(
                live.get("kayla"),
                old.get("kayla"),
                "stale rejection after confirmation: {merge}, fresh={fresh}"
            );
        }
    }

    #[test]
    fn passive_agy_confirmation_survives_replay_on_original_receiver() {
        assert_passive_agy_confirmation_survives_replay_without_prior_rejection(false);
    }

    #[test]
    fn passive_agy_confirmation_survives_replay_on_fresh_receiver() {
        assert_passive_agy_confirmation_survives_replay_without_prior_rejection(true);
    }

    #[test]
    fn passive_agy_rejection_is_transient_and_successful_revalidation_clears_it() {
        let home = TempDir::new().unwrap();
        let mut table = RouteTable::default();
        let mut route = agy_route("revalidated-conversation");
        if let VendorRoute::Agy { agentapi_exe, .. } = &mut route {
            *agentapi_exe = Some(PathBuf::from("agy"));
        }
        table.upsert("kayla", route, At::from_unix_millis(9000));
        let old = table.clone();
        table.discover_agy_routes(home.path(), false, &At::from_unix_millis(10000));
        assert!(table.get("kayla").is_none());
        let encoded = serde_json::to_value(&table).unwrap();
        assert_eq!(encoded, serde_json::json!({"entries": {}}));
        table.merge_newer(old.clone());
        assert!(table.get("kayla").is_none(), "rejected snapshot reappeared");

        let rejected = table.clone();
        agy_registration_fixture(home.path(), "revalidated-conversation", "Kayla", 9000);
        table.discover_agy_routes(home.path(), false, &At::from_unix_millis(10001));
        assert_eq!(table.get("kayla"), old.get("kayla"));
        for merge in ["direct", "dispatch", "daemon"] {
            let mut live = rejected.clone();
            match merge {
                "direct" => live.merge_newer(table.clone()),
                "dispatch" => live.merge_dispatch_result(&rejected, table.clone()),
                "daemon" => live.merge_daemon_dispatch_result(&rejected, table.clone()),
                _ => unreachable!(),
            }
            assert_eq!(live.get("kayla"), old.get("kayla"), "revalidation: {merge}");
            live.merge_newer(rejected.clone());
            assert_eq!(
                live.get("kayla"),
                old.get("kayla"),
                "stale rejection: {merge}"
            );
        }

        // A later scan must revoke that confirmation if the source vanishes, even when the
        // current snapshot has no route (for example after an unregister).
        let mut missing_again = table.clone();
        missing_again.entries.clear();
        fs::remove_file(home.path().join(".gemini/antigravity-cli/brain/revalidated-conversation/.system_generated/logs/transcript.jsonl")).unwrap();
        missing_again.discover_agy_routes(home.path(), false, &At::from_unix_millis(10002));
        missing_again.merge_newer(old.clone());
        assert!(
            missing_again.get("kayla").is_none(),
            "missing source reused an old confirmation"
        );
        agy_registration_fixture(home.path(), "revalidated-conversation", "Kayla", 9000);

        // An older successful validation may clear only its own rejected entry. A different
        // claim rejected in the meantime still must not be replayed from a stale snapshot.
        let mut changed = rejected;
        changed.upsert(
            "kayla",
            agy_route("different-rejected"),
            At::from_unix_millis(9500),
        );
        let stale = changed.clone();
        changed.discover_agy_routes(home.path(), false, &At::from_unix_millis(10002));
        changed.merge_newer(table);
        changed.merge_newer(stale);
        assert_eq!(changed.get("kayla"), old.get("kayla"));
    }

    #[test]
    fn discover_agy_routes_picks_newest_conversation_by_mtime_and_stamps_file_mtime() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let brain = home.join(".gemini").join("antigravity-cli").join("brain");
        let old_transcript = brain
            .join("aaa-older-conv")
            .join(".system_generated")
            .join("logs")
            .join("transcript.jsonl");
        let new_transcript = brain
            .join("zzz-newer-conv")
            .join(".system_generated")
            .join("logs")
            .join("transcript.jsonl");
        fs::create_dir_all(old_transcript.parent().unwrap()).unwrap();
        fs::create_dir_all(new_transcript.parent().unwrap()).unwrap();
        let line = "{\"type\":\"USER_INPUT\",\"content\":\"agent-msg register Ferris --project ferroloop --role lead\"}\n";
        fs::write(&old_transcript, line).unwrap();
        fs::write(&new_transcript, line).unwrap();
        set_mtime(&old_transcript, 1_000_000);
        set_mtime(&new_transcript, 2_000_000);

        let mut table = RouteTable::default();
        table.discover_routes(Some(home), At::from_unix_millis(9_000_000));
        let entry = table.get("ferris").expect("ferris route");
        assert_eq!(entry.updated_at.unix_millis(), 2_000_000);
        assert!(matches!(
            &entry.route,
            VendorRoute::Agy { conversation_id, .. } if conversation_id == "zzz-newer-conv"
        ));
    }

    #[test]
    fn retired_personas_in_decommission_tombstones_block_passive_rediscovery() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let brain_transcript = home
            .join(".gemini")
            .join("antigravity-cli")
            .join("brain")
            .join("conv-fiona-old")
            .join(".system_generated")
            .join("logs")
            .join("transcript.jsonl");
        fs::create_dir_all(brain_transcript.parent().unwrap()).unwrap();
        fs::write(
            &brain_transcript,
            "{\"type\":\"USER_INPUT\",\"content\":\"agent-msg register Fiona --project ferroloop --role assistant\"}\n",
        )
        .unwrap();
        set_mtime(&brain_transcript, 1_500_000);

        let ids = BTreeSet::new();
        let retired = BTreeSet::from(["fiona".to_string()]);
        save_decommissioned_tombstones(Some(home), &ids, &retired).unwrap();
        assert_eq!(load_retired_personas(Some(home)), retired);

        let mut table = RouteTable::default();
        table.discover_routes(Some(home), At::from_unix_millis(5_000_000));
        assert!(
            table.get("fiona").is_none(),
            "passive transcript discovery must skip retired personas"
        );

        table.upsert_explicit(
            "fiona",
            agy_route("conv-fiona-new"),
            At::from_unix_millis(6_000_000),
        );
        table.discover_routes(Some(home), At::from_unix_millis(7_000_000));
        assert!(
            table.get("fiona").is_some(),
            "explicit route registration for a retired persona stays"
        );
    }

    #[test]
    fn empty_or_corrupt_primary_decommission_sidecar_falls_back_to_persistent_copy() {
        let tmp = TempDir::new().unwrap();
        let primary = tmp.path().join("primary.json");
        let fallback = tmp.path().join("persistent.json");
        fs::write(
            &fallback,
            r#"{"session_ids":["conv-persisted"],"retired_personas":["fiona"]}"#,
        )
        .unwrap();

        // Empty primary falls back to persistent file.
        fs::write(&primary, "").unwrap();
        let val = read_decommissioned_sessions_value_from(&primary, Some(&fallback))
            .expect("fallback value");
        assert_eq!(val["session_ids"], serde_json::json!(["conv-persisted"]));
        assert_eq!(val["retired_personas"], serde_json::json!(["fiona"]));

        // Corrupt primary falls back to persistent file.
        fs::write(&primary, "{not-valid-json").unwrap();
        let val = read_decommissioned_sessions_value_from(&primary, Some(&fallback))
            .expect("fallback value");
        assert_eq!(val["session_ids"], serde_json::json!(["conv-persisted"]));

        // sync_retired_personas_in_tombstones preserves existing session_ids.
        let home = tmp.path().join("home");
        save_decommissioned_tombstones(
            Some(&home),
            &BTreeSet::from(["conv-keep".to_string()]),
            &BTreeSet::from(["fiona".to_string()]),
        )
        .unwrap();
        sync_retired_personas_in_tombstones(Some(&home), &BTreeSet::from(["ada".to_string()]))
            .unwrap();
        assert_eq!(
            load_decommissioned_session_ids(Some(&home)),
            BTreeSet::from(["conv-keep".to_string()])
        );
        assert_eq!(
            load_retired_personas(Some(&home)),
            BTreeSet::from(["ada".to_string()])
        );
    }

    #[test]
    fn capture_env_route_reads_claude_session_id_from_pid_json_and_upsert_preserves_it() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let sessions_dir = home.join(".claude").join("sessions");
        fs::create_dir_all(&sessions_dir).unwrap();
        let sock_path = sessions_dir.join("4242.sock");
        fs::write(&sock_path, "").unwrap();
        let key_file = sessions_dir.join("4242.abc.key");
        fs::write(&key_file, r#"{"peerToken":"tok-4242"}"#).unwrap();
        fs::write(
            sessions_dir.join("4242.json"),
            serde_json::json!({
                "pid": 4242,
                "sessionId": "claude-native-session-uuid",
                "messagingSocketPath": sock_path,
            })
            .to_string(),
        )
        .unwrap();

        let home_str = home.to_str().unwrap();
        let sock_str = sock_path.to_str().unwrap();
        for env_sid in [None, Some(""), Some("   ")] {
            let mut pairs = vec![
                ("HOME", home_str),
                ("CLAUDE_CODE_MESSAGING_SOCKET", sock_str),
            ];
            if let Some(s) = env_sid {
                pairs.push(("CLAUDE_SESSION_ID", s));
            }
            let route = capture_env_route_from(&env_of(&pairs)).expect("claude route");
            match route {
                VendorRoute::Claude {
                    pid,
                    session_id,
                    key_path,
                    ..
                } => {
                    assert_eq!(pid, 4242);
                    assert_eq!(session_id, "claude-native-session-uuid");
                    assert_eq!(key_path.as_deref(), Some(key_file.as_path()));
                }
                other => panic!("expected Claude route, got {other:?}"),
            }
        }

        // Upserting a Claude route with an empty session_id or missing key_path preserves the
        // existing non-empty session_id and valid key_path for the same PID.
        let mut table = RouteTable::default();
        table.upsert_explicit(
            "sonya",
            VendorRoute::Claude {
                pid: 4242,
                session_id: "claude-native-session-uuid".into(),
                socket_path: sock_path.clone(),
                key_path: Some(key_file.clone()),
            },
            At::from_unix_millis(100),
        );
        table.upsert_explicit(
            "sonya",
            VendorRoute::Claude {
                pid: 4242,
                session_id: String::new(),
                socket_path: sock_path.clone(),
                key_path: None,
            },
            At::from_unix_millis(200),
        );
        match &table.get("sonya").expect("sonya route").route {
            VendorRoute::Claude {
                pid,
                session_id,
                key_path,
                ..
            } => {
                assert_eq!(*pid, 4242);
                assert_eq!(session_id, "claude-native-session-uuid");
                assert_eq!(key_path.as_deref(), Some(key_file.as_path()));
            }
            other => panic!("expected Claude route, got {other:?}"),
        }

        // A `<pid>.json` whose `messagingSocketPath` does not match `socket_path` (e.g. from a
        // recycled PID) must be ignored.
        let other_sock = sessions_dir.join("9999.sock");
        fs::write(
            sessions_dir.join("4242.json"),
            serde_json::json!({
                "pid": 4242,
                "sessionId": "stale-recycled-session-uuid",
                "messagingSocketPath": other_sock,
            })
            .to_string(),
        )
        .unwrap();
        let mismatched_route = capture_env_route_from(&env_of(&[
            ("HOME", home_str),
            ("CLAUDE_CODE_MESSAGING_SOCKET", sock_str),
        ]))
        .expect("claude route");
        match mismatched_route {
            VendorRoute::Claude { session_id, .. } => {
                assert!(
                    session_id.is_empty(),
                    "mismatched messagingSocketPath must not attach stale sessionId, got {session_id:?}"
                );
            }
            other => panic!("expected Claude route, got {other:?}"),
        }

        // A legacy pid-0 Claude route with a different socket path must not copy its session_id
        // onto another Claude route.
        let mut pid0_table = RouteTable::default();
        pid0_table.upsert_explicit(
            "sonya",
            VendorRoute::Claude {
                pid: 0,
                session_id: "legacy-pid0-session".into(),
                socket_path: PathBuf::from("/tmp/claude-old.sock"),
                key_path: Some(key_file.clone()),
            },
            At::from_unix_millis(100),
        );
        pid0_table.upsert_explicit(
            "sonya",
            VendorRoute::Claude {
                pid: 5150,
                session_id: String::new(),
                socket_path: PathBuf::from("/tmp/claude-new-5150.sock"),
                key_path: None,
            },
            At::from_unix_millis(200),
        );
        match &pid0_table.get("sonya").expect("sonya route").route {
            VendorRoute::Claude {
                pid,
                session_id,
                key_path,
                ..
            } => {
                assert_eq!(*pid, 5150);
                assert!(
                    session_id.is_empty(),
                    "pid-0 route with different socket must not leak session_id, got {session_id:?}"
                );
                assert!(
                    key_path.is_none(),
                    "pid-0 route with different socket must not leak key_path, got {key_path:?}"
                );
            }
            other => panic!("expected Claude route, got {other:?}"),
        }
    }

    #[test]
    fn discover_agy_routes_does_not_copy_endpoint_across_different_live_ls_pids_or_downgrade_newer_placeholder()
     {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let agy_dir = home.join(".gemini").join("antigravity-cli");
        let brain_dir = agy_dir.join("brain");
        fs::create_dir_all(&brain_dir).unwrap();

        let feira_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let feira_port = feira_listener.local_addr().unwrap().port();
        let feira_addr = format!("127.0.0.1:{feira_port}");

        // `cli.log` still holds Feira's port while Felix's new process is spinning up.
        fs::write(
            agy_dir.join("cli.log"),
            format!("Language server listening on random port at {feira_port} for HTTP\n"),
        )
        .unwrap();

        // An older transcript for Felix from a previous session (`file_ms = 200`) exists on disk
        // before Felix's new spin-up placeholder (`updated_at = 300`).
        let old_felix_transcript = brain_dir
            .join("conv-felix-old")
            .join(".system_generated")
            .join("logs")
            .join("transcript.jsonl");
        fs::create_dir_all(old_felix_transcript.parent().unwrap()).unwrap();
        fs::write(
            &old_felix_transcript,
            "{\"type\":\"USER_INPUT\",\"content\":\"agent-msg register Felix --project ferroloop --role ui-developer\"}\n",
        )
        .unwrap();
        set_mtime(&old_felix_transcript, 200);

        let mut table = RouteTable::default();
        table.upsert_explicit(
            "feira",
            VendorRoute::Agy {
                conversation_id: "conv-feira-live".into(),
                ls_pid: Some(1111),
                ls_address: Some(feira_addr.clone()),
                csrf_token: Some("csrf-feira-1111".into()),
                agentapi_exe: None,
            },
            At::from_unix_millis(250),
        );
        table.upsert_explicit(
            "felix",
            VendorRoute::Agy {
                conversation_id: "antigravity:felix".into(),
                ls_pid: Some(2222),
                ls_address: None,
                csrf_token: None,
                agentapi_exe: None,
            },
            At::from_unix_millis(300),
        );

        // Pass 1: Felix's placeholder (`updated_at = 300`) is newer than `conv-felix-old`
        // (`file_ms = 200`), and Felix's live `ls_pid = Some(2222)` differs from Feira's
        // `ls_pid = Some(1111)`. Felix must keep his placeholder conversation_id and must not
        // inherit Feira's `(ls_address, csrf_token)`.
        table.discover_agy_routes(home, false, &At::from_unix_millis(350));
        let felix_entry = table.get("felix").expect("felix route after pass 1");
        assert!(felix_entry.explicit);
        assert_eq!(felix_entry.updated_at.unix_millis(), 300);
        match &felix_entry.route {
            VendorRoute::Agy {
                conversation_id,
                ls_pid,
                ls_address,
                csrf_token,
                ..
            } => {
                assert_eq!(conversation_id, "antigravity:felix");
                assert_eq!(*ls_pid, Some(2222));
                assert!(ls_address.is_none(), "got {ls_address:?}");
                assert!(csrf_token.is_none(), "got {csrf_token:?}");
            }
            other => panic!("expected Agy route for felix, got {other:?}"),
        }

        // Pass 2: Felix's new session writes its transcript at `file_ms = 400` (`>= 300`).
        // The placeholder upgrades to `conv-felix-new`, keeps `updated_at = 400` and `explicit`,
        // and still does not inherit Feira's `(ls_address, csrf_token)` from `ls_pid = 1111`.
        let new_felix_transcript = brain_dir
            .join("conv-felix-new")
            .join(".system_generated")
            .join("logs")
            .join("transcript.jsonl");
        fs::create_dir_all(new_felix_transcript.parent().unwrap()).unwrap();
        fs::write(
            &new_felix_transcript,
            "{\"type\":\"USER_INPUT\",\"content\":\"agent-msg register Felix --project ferroloop --role ui-developer\"}\n",
        )
        .unwrap();
        set_mtime(&new_felix_transcript, 400);

        table.discover_agy_routes(home, false, &At::from_unix_millis(450));
        let felix_upgraded = table.get("felix").expect("felix route after pass 2");
        assert!(felix_upgraded.explicit);
        assert_eq!(felix_upgraded.updated_at.unix_millis(), 400);
        match &felix_upgraded.route {
            VendorRoute::Agy {
                conversation_id,
                ls_pid,
                ls_address,
                csrf_token,
                ..
            } => {
                assert_eq!(conversation_id, "conv-felix-new");
                assert_eq!(*ls_pid, Some(2222));
                assert!(ls_address.is_none(), "got {ls_address:?}");
                assert!(csrf_token.is_none(), "got {csrf_token:?}");
            }
            other => panic!("expected Agy route for felix, got {other:?}"),
        }
    }

    fn codex_rollout_fixture(home: &Path, thread_id: &str, persona: &str, ms: u64) -> PathBuf {
        let day = home.join(".codex/sessions/2026/11/09");
        fs::create_dir_all(&day).unwrap();
        let rollout = day.join(format!("rollout-2026-11-09T00-00-00-{thread_id}.jsonl"));
        fs::write(
            &rollout,
            [
                serde_json::json!({
                    "type": "session_meta",
                    "payload": {
                        "id": thread_id,
                        "cwd": "/tmp/codex-fixture"
                    }
                })
                .to_string(),
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "user",
                        "content": [{"type": "input_text", "text": format!("fl wire register {persona}")}]
                    }
                })
                .to_string(),
            ]
            .join("\n"),
        )
        .unwrap();
        set_mtime(&rollout, ms);
        rollout
    }

    #[test]
    fn passive_agy_and_codex_compete_by_source_identity_age_across_order_and_repeated_scans() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let listener1 = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port1 = listener1.local_addr().unwrap().port();
        let listener2 = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port2 = listener2.local_addr().unwrap().port();
        let agy_dir = home.join(".gemini/antigravity-cli");
        fs::create_dir_all(&agy_dir).unwrap();
        fs::write(
            agy_dir.join("cli.log"),
            format!("Language server listening on random port at {port1} for HTTP\n"),
        )
        .unwrap();

        let agy_path = agy_registration_fixture(home, "conv-kayla-100", "Kayla", 100);
        let codex_tid = "01900000-0000-7000-8000-000000000200";
        codex_rollout_fixture(home, codex_tid, "Kayla", 200);

        // 1. Standard discover_routes order (Claude -> Agy -> Codex) at scan time 9000.
        let mut standard = RouteTable::default();
        standard.discover_routes(Some(home), At::from_unix_millis(9000));
        let entry = standard.get("kayla").expect("kayla route");
        assert!(
            matches!(&entry.route, VendorRoute::Codex { thread_id, .. } if thread_id == codex_tid),
            "expected Codex(200) to beat Agy(100), got {entry:?}"
        );
        assert_eq!(entry.identity_at, Some(At::from_unix_millis(200)));
        assert_eq!(entry.updated_at, At::from_unix_millis(200));

        // 2. Reversed discovery order (Codex -> Agy) at scan time 9000.
        let mut reversed = RouteTable::default();
        reversed.discover_codex_routes(home, &At::from_unix_millis(9000));
        reversed.discover_agy_routes(home, false, &At::from_unix_millis(9000));
        let rev_entry = reversed.get("kayla").expect("kayla route reversed");
        assert!(
            matches!(&rev_entry.route, VendorRoute::Codex { thread_id, .. } if thread_id == codex_tid),
            "expected Codex(200) to beat Agy(100) regardless of vendor scan order, got {rev_entry:?}"
        );
        assert_eq!(rev_entry.identity_at, Some(At::from_unix_millis(200)));

        // 3. Repeated scans at 9500 and 10000 (including cli.log endpoint rotation) never promote
        // Agy(100) over Codex(200).
        fs::write(
            agy_dir.join("cli.log"),
            format!("Language server listening on random port at {port2} for HTTP\n"),
        )
        .unwrap();
        for scan_ms in [9500_u64, 10000] {
            standard.discover_routes(Some(home), At::from_unix_millis(scan_ms));
            let after = standard.get("kayla").expect("kayla route after rescan");
            assert!(
                matches!(&after.route, VendorRoute::Codex { thread_id, .. } if thread_id == codex_tid),
                "scan at {scan_ms} must keep Codex(200), got {after:?}"
            );
            assert_eq!(after.identity_at, Some(At::from_unix_millis(200)));
        }

        // 4. Symmetric opposite: when Agy transcript is genuinely newer (300 > 200), Agy wins in
        // both standard and reversed discovery orders, with identity_at = 300.
        set_mtime(&agy_path, 300);
        let mut agy_newer_std = RouteTable::default();
        agy_newer_std.discover_routes(Some(home), At::from_unix_millis(11000));
        let std_winner = agy_newer_std.get("kayla").expect("std winner");
        assert!(
            matches!(&std_winner.route, VendorRoute::Agy { conversation_id, .. } if conversation_id == "conv-kayla-100")
        );
        assert_eq!(std_winner.identity_at, Some(At::from_unix_millis(300)));

        let mut agy_newer_rev = RouteTable::default();
        agy_newer_rev.discover_codex_routes(home, &At::from_unix_millis(11000));
        agy_newer_rev.discover_agy_routes(home, false, &At::from_unix_millis(11000));
        let rev_winner = agy_newer_rev.get("kayla").expect("rev winner");
        assert!(
            matches!(&rev_winner.route, VendorRoute::Agy { conversation_id, .. } if conversation_id == "conv-kayla-100")
        );
        assert_eq!(rev_winner.identity_at, Some(At::from_unix_millis(300)));
    }

    #[test]
    fn persisted_scan_stamped_passive_agy_revalidates_identity_age_and_loses_to_newer_codex() {
        for disk_shape in [
            "legacy-no-identity-at",
            "scan-stamped-identity-at",
            "legacy-agent-bus",
        ] {
            let tmp = TempDir::new().unwrap();
            let home = tmp.path().join("home");
            let agy_dir = home.join(".gemini/antigravity-cli");
            fs::create_dir_all(&agy_dir).unwrap();
            let bin_dir = home.join(".local/bin");
            fs::create_dir_all(&bin_dir).unwrap();
            let local_agy = bin_dir.join("agy");
            fs::write(&local_agy, b"#!/bin/sh\n").unwrap();
            let old_agy = bin_dir.join("agy.1700.old");
            fs::write(&old_agy, b"#!/bin/sh\n").unwrap();

            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let live_port = listener.local_addr().unwrap().port();
            fs::write(
                agy_dir.join("cli.log"),
                format!("Language server listening on random port at {live_port} for HTTP\n"),
            )
            .unwrap();
            agy_registration_fixture(&home, "conv-kayla-100", "Kayla", 100);

            let primary = tmp.path().join("fl-wire-routes.json");
            let fallback = tmp.path().join("legacy-routes.json");
            let write_persisted = || match disk_shape {
                "legacy-no-identity-at" => {
                    fs::write(
                        &primary,
                        serde_json::json!({
                            "entries": {
                                "kayla": {
                                    "persona": "kayla",
                                    "route": {
                                        "kind": "agy",
                                        "conversation_id": "conv-kayla-100",
                                        "ls_address": "127.0.0.1:1",
                                        "csrf_token": "stale-csrf",
                                        "agentapi_exe": old_agy,
                                    },
                                    "updated_at": At::from_unix_millis(9000),
                                }
                            }
                        })
                        .to_string(),
                    )
                    .unwrap();
                }
                "scan-stamped-identity-at" => {
                    fs::write(
                        &primary,
                        serde_json::json!({
                            "entries": {
                                "kayla": {
                                    "persona": "kayla",
                                    "route": {
                                        "kind": "agy",
                                        "conversation_id": "conv-kayla-100",
                                        "ls_address": "127.0.0.1:1",
                                        "csrf_token": "stale-csrf",
                                        "agentapi_exe": old_agy,
                                    },
                                    "updated_at": At::from_unix_millis(9000),
                                    "identity_at": At::from_unix_millis(9000),
                                }
                            }
                        })
                        .to_string(),
                    )
                    .unwrap();
                }
                "legacy-agent-bus" => {
                    let _ = fs::remove_file(&primary);
                    fs::write(
                        &fallback,
                        serde_json::json!({
                            "kayla": {
                                "vendor": "agy",
                                "conversation_id": "conv-kayla-100",
                                "ls_address": "127.0.0.1:1",
                                "csrf_token": "stale-csrf",
                                "agentapi_exe": old_agy,
                                "updated_at": At::from_unix_millis(9000),
                            }
                        })
                        .to_string(),
                    )
                    .unwrap();
                }
                _ => unreachable!(),
            };

            // Phase A (Agy alone): reload, repair endpoint + .old exe at scan 9500, verify
            // updated_at advances to 9500 while identity_at revalidates to 100 (never 9000/9500).
            write_persisted();
            let mut agy_only = RouteTable::load_with_legacy(&primary, Some(&fallback));
            agy_only.discover_routes(Some(&home), At::from_unix_millis(9500));
            let repaired = agy_only.get("kayla").expect("repaired agy route");
            assert_eq!(
                repaired.updated_at,
                At::from_unix_millis(9500),
                "{disk_shape}"
            );
            assert_eq!(
                repaired.identity_at,
                Some(At::from_unix_millis(100)),
                "{disk_shape}"
            );
            match &repaired.route {
                VendorRoute::Agy {
                    ls_address,
                    csrf_token,
                    agentapi_exe,
                    ..
                } => {
                    assert_eq!(
                        ls_address.as_deref(),
                        Some(format!("127.0.0.1:{live_port}").as_str()),
                        "{disk_shape}"
                    );
                    assert!(csrf_token.is_none(), "{disk_shape}");
                    assert_eq!(
                        agentapi_exe.as_deref(),
                        Some(local_agy.as_path()),
                        "{disk_shape}"
                    );
                }
                other => panic!("{disk_shape}: expected Agy, got {other:?}"),
            }

            // Phase B (Agy 100 + Codex 200): reload the scan-stamped 9000 Agy entry and scan at
            // 9500; endpoint/exe repair does not promote identity_at, so Codex(200) wins.
            let codex_tid = "01900000-0000-7000-8000-000000000202";
            codex_rollout_fixture(&home, codex_tid, "Kayla", 200);
            write_persisted();
            let mut competing = RouteTable::load_with_legacy(&primary, Some(&fallback));
            competing.discover_routes(Some(&home), At::from_unix_millis(9500));
            let winner = competing.get("kayla").expect("competing winner");
            assert!(
                matches!(&winner.route, VendorRoute::Codex { thread_id, .. } if thread_id == codex_tid),
                "{disk_shape}: expected Codex(200) to beat persisted Agy(9000 -> 100), got {winner:?}"
            );
            assert_eq!(winner.identity_at, Some(At::from_unix_millis(200)));
        }
    }

    #[test]
    fn same_session_passive_agy_endpoint_repair_propagates_without_promoting_identity_age() {
        for initial_identity in [None, Some(100_u64), Some(8000_u64)] {
            for merge_mode in ["direct", "dispatch", "daemon"] {
                let tmp = TempDir::new().unwrap();
                let home = tmp.path();
                let agy_dir = home.join(".gemini/antigravity-cli");
                fs::create_dir_all(&agy_dir).unwrap();
                let bin_dir = home.join(".local/bin");
                fs::create_dir_all(&bin_dir).unwrap();
                let local_agy = bin_dir.join("agy");
                fs::write(&local_agy, b"#!/bin/sh\n").unwrap();
                let old_agy = bin_dir.join("agy.99.old");
                fs::write(&old_agy, b"#!/bin/sh\n").unwrap();

                let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
                let port = listener.local_addr().unwrap().port();
                let live_addr = format!("127.0.0.1:{port}");
                fs::write(
                    agy_dir.join("cli.log"),
                    format!("Language server listening on random port at {port} for HTTP\n"),
                )
                .unwrap();
                agy_registration_fixture(home, "conv-kayla-1", "Kayla", 100);
                agy_registration_fixture(home, "conv-donor", "Ferris", 300);

                let mut base = RouteTable::default();
                base.entries.insert(
                    "kayla".into(),
                    RouteEntry {
                        persona: "kayla".into(),
                        route: VendorRoute::Agy {
                            conversation_id: "conv-kayla-1".into(),
                            ls_pid: None,
                            ls_address: Some("127.0.0.1:1".into()),
                            csrf_token: Some("stale-csrf".into()),
                            agentapi_exe: Some(old_agy),
                        },
                        updated_at: At::from_unix_millis(500),
                        identity_at: initial_identity.map(At::from_unix_millis),
                        explicit: false,
                        hosted_id: None,
                        hydration_generation: None,
                    },
                );
                base.upsert_explicit(
                    "ferris",
                    VendorRoute::Agy {
                        conversation_id: "conv-donor".into(),
                        ls_pid: None,
                        ls_address: Some(live_addr.clone()),
                        csrf_token: Some("fresh-csrf".into()),
                        agentapi_exe: Some(local_agy.clone()),
                    },
                    At::from_unix_millis(600),
                );

                let mut working = base.clone();
                working.discover_routes(Some(home), At::from_unix_millis(9000));
                let mut live = base.clone();
                match merge_mode {
                    "direct" => live.merge_newer(working),
                    "dispatch" => live.merge_dispatch_result(&base, working),
                    "daemon" => live.merge_daemon_dispatch_result(&base, working),
                    _ => unreachable!(),
                }

                let kayla = live.get("kayla").expect("kayla in live");
                assert_eq!(
                    kayla.updated_at,
                    At::from_unix_millis(9000),
                    "{merge_mode}, init={initial_identity:?}"
                );
                assert_eq!(
                    kayla.identity_at,
                    Some(At::from_unix_millis(100)),
                    "{merge_mode}, init={initial_identity:?}"
                );
                match &kayla.route {
                    VendorRoute::Agy {
                        conversation_id,
                        ls_address,
                        csrf_token,
                        agentapi_exe,
                        ..
                    } => {
                        assert_eq!(conversation_id, "conv-kayla-1");
                        assert_eq!(ls_address.as_deref(), Some(live_addr.as_str()));
                        assert_eq!(csrf_token.as_deref(), Some("fresh-csrf"));
                        assert_eq!(agentapi_exe.as_deref(), Some(local_agy.as_path()));
                    }
                    other => panic!("expected Agy route, got {other:?}"),
                }
            }
        }
    }

    #[test]
    fn dispatch_reconciliation_replaces_scan_stamped_passive_agy_with_newer_codex_and_keeps_concurrent_explicit()
     {
        for initial_identity in [None, Some(9000_u64)] {
            for merge_mode in ["direct", "dispatch", "daemon"] {
                for concurrent_explicit in [false, true] {
                    let tmp = TempDir::new().unwrap();
                    let home = tmp.path();
                    agy_registration_fixture(home, "conv-kayla-100", "Kayla", 100);
                    let codex_tid = "01900000-0000-7000-8000-000000000205";
                    codex_rollout_fixture(home, codex_tid, "Kayla", 200);

                    let mut base = RouteTable::default();
                    base.entries.insert(
                        "kayla".into(),
                        RouteEntry {
                            persona: "kayla".into(),
                            route: agy_route("conv-kayla-100"),
                            updated_at: At::from_unix_millis(9000),
                            identity_at: initial_identity.map(At::from_unix_millis),
                            explicit: false,
                            hosted_id: None,
                            hydration_generation: None,
                        },
                    );
                    let mut working = base.clone();
                    working.discover_routes(Some(home), At::from_unix_millis(9500));
                    assert!(matches!(
                        working.get("kayla").map(|e| &e.route),
                        Some(VendorRoute::Codex { thread_id, .. }) if thread_id == codex_tid
                    ));

                    let mut live = base.clone();
                    if concurrent_explicit {
                        live.upsert_explicit(
                            "kayla",
                            agy_route("conv-explicit-concurrent"),
                            At::from_unix_millis(9200),
                        );
                    }
                    match merge_mode {
                        "direct" => live.merge_newer(working),
                        "dispatch" => live.merge_dispatch_result(&base, working),
                        "daemon" => live.merge_daemon_dispatch_result(&base, working),
                        _ => unreachable!(),
                    }

                    if concurrent_explicit {
                        let kept = live.get("kayla").expect("concurrent explicit kept");
                        assert!(kept.explicit);
                        assert_eq!(kept.route, agy_route("conv-explicit-concurrent"));
                    } else {
                        let replaced = live.get("kayla").expect("codex won in live");
                        assert!(!replaced.explicit);
                        assert!(
                            matches!(&replaced.route, VendorRoute::Codex { thread_id, .. } if thread_id == codex_tid),
                            "init={initial_identity:?}, mode={merge_mode}: got {replaced:?}"
                        );
                        assert_eq!(replaced.identity_at, Some(At::from_unix_millis(200)));

                        // Replaying the pre-scan snapshot must not resurrect the 9000 Agy entry.
                        live.merge_newer(base.clone());
                        let after_replay = live.get("kayla").expect("codex after replay");
                        assert!(
                            matches!(&after_replay.route, VendorRoute::Codex { thread_id, .. } if thread_id == codex_tid),
                            "replay init={initial_identity:?}, mode={merge_mode}: got {after_replay:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn identity_evidence_tie_breaking_and_unknown_mtime_rules() {
        // 1. Equal known evidence (100 vs 100 across different sessions): incumbent wins ties in
        // both upsert_discovered and merge_newer (same-vendor, cross-vendor, and cross-persona).
        let mut up_tie = RouteTable::default();
        up_tie.upsert_discovered(
            "kayla",
            agy_route("conv-incumbent-100"),
            100,
            &At::from_unix_millis(9000),
        );
        up_tie.upsert_discovered(
            "kayla",
            codex_route("codex-challenger-100"),
            100,
            &At::from_unix_millis(9500),
        );
        assert_eq!(
            up_tie.get("kayla").unwrap().route,
            agy_route("conv-incumbent-100")
        );
        up_tie.upsert_discovered(
            "alias-kayla",
            agy_route("conv-incumbent-100"),
            100,
            &At::from_unix_millis(9500),
        );
        assert!(up_tie.get("alias-kayla").is_none());

        let mut merge_incumbent = RouteTable::default();
        merge_incumbent.entries.insert(
            "kayla".into(),
            RouteEntry {
                persona: "kayla".into(),
                route: agy_route("conv-incumbent-100"),
                updated_at: At::from_unix_millis(9000),
                identity_at: Some(At::from_unix_millis(100)),
                explicit: false,
                hosted_id: None,
                hydration_generation: None,
            },
        );
        merge_incumbent.entries.insert(
            "bystander".into(),
            RouteEntry {
                persona: "bystander".into(),
                route: codex_route("codex-challenger-100"),
                updated_at: At::from_unix_millis(50),
                identity_at: Some(At::from_unix_millis(50)),
                explicit: false,
                hosted_id: None,
                hydration_generation: None,
            },
        );
        let mut merge_challenger = RouteTable::default();
        merge_challenger.entries.insert(
            "kayla".into(),
            RouteEntry {
                persona: "kayla".into(),
                route: codex_route("codex-challenger-100"),
                updated_at: At::from_unix_millis(9500),
                identity_at: Some(At::from_unix_millis(100)),
                explicit: false,
                hosted_id: None,
                hydration_generation: None,
            },
        );
        merge_challenger.upsert(
            "alias-kayla",
            agy_route("conv-incumbent-100"),
            At::from_unix_millis(100),
        );
        merge_incumbent.merge_newer(merge_challenger);
        assert_eq!(
            merge_incumbent.get("kayla").unwrap().route,
            agy_route("conv-incumbent-100"),
            "same-persona passive tie must keep incumbent even when challenger has higher updated_at"
        );
        assert_eq!(
            merge_incumbent.get("bystander").unwrap().route,
            codex_route("codex-challenger-100"),
            "losing same-persona challenger must not evict bystander owning challenger's session"
        );
        assert!(merge_incumbent.get("alias-kayla").is_none());

        // `session_identity() == None` routes (e.g. scanner Webhook socket move): newer
        // `updated_at` (9500 > 9000) replaces the old socket across all merge paths, while an
        // equal-`updated_at` tie (9000 vs 9000) keeps the incumbent.
        for hook_identity in [None, Some(At::from_unix_millis(100))] {
            let hook_a_route = VendorRoute::Webhook {
                socket_path: PathBuf::from("/tmp/hook-a.sock"),
            };
            let hook_b_route = VendorRoute::Webhook {
                socket_path: PathBuf::from("/tmp/hook-b.sock"),
            };
            let mut base = RouteTable::default();
            base.entries.insert(
                "kayla".into(),
                RouteEntry {
                    persona: "kayla".into(),
                    route: hook_a_route.clone(),
                    updated_at: At::from_unix_millis(9000),
                    identity_at: hook_identity.clone(),
                    explicit: false,
                    hosted_id: None,
                    hydration_generation: None,
                },
            );
            let mut newer_hook = RouteTable::default();
            newer_hook.entries.insert(
                "kayla".into(),
                RouteEntry {
                    persona: "kayla".into(),
                    route: hook_b_route.clone(),
                    updated_at: At::from_unix_millis(9500),
                    identity_at: hook_identity.clone(),
                    explicit: false,
                    hosted_id: None,
                    hydration_generation: None,
                },
            );
            for merge_mode in ["direct", "dispatch", "daemon"] {
                let mut live = base.clone();
                match merge_mode {
                    "direct" => live.merge_newer(newer_hook.clone()),
                    "dispatch" => live.merge_dispatch_result(&base, newer_hook.clone()),
                    "daemon" => live.merge_daemon_dispatch_result(&base, newer_hook.clone()),
                    _ => unreachable!(),
                }
                assert_eq!(
                    live.get("kayla").unwrap().route,
                    hook_b_route,
                    "newer Webhook must win: mode={merge_mode}, hook_identity={hook_identity:?}"
                );
                assert_eq!(
                    live.get("kayla").unwrap().updated_at,
                    At::from_unix_millis(9500)
                );
            }

            let mut equal_hook = RouteTable::default();
            equal_hook.entries.insert(
                "kayla".into(),
                RouteEntry {
                    persona: "kayla".into(),
                    route: hook_b_route,
                    updated_at: At::from_unix_millis(9000),
                    identity_at: hook_identity.clone(),
                    explicit: false,
                    hosted_id: None,
                    hydration_generation: None,
                },
            );
            let mut tie_live = base.clone();
            tie_live.merge_newer(equal_hook);
            assert_eq!(
                tie_live.get("kayla").unwrap().route,
                hook_a_route,
                "equal-updated_at Webhook tie must keep incumbent: hook_identity={hook_identity:?}"
            );
        }

        // End-to-end scanner Webhook socket move (`sock1` at 9000 -> `sock2` at 9500) across
        // direct scan and hub/daemon reconciliation.
        {
            struct MovingWebhookScanner {
                expected_home: PathBuf,
                current_sock: std::sync::Mutex<PathBuf>,
            }
            impl crate::scanner::WireVendorScanner for MovingWebhookScanner {
                fn vendor_id(&self) -> &str {
                    "wscan-445-webhook-move"
                }
                fn discover(&self, home: &Path) -> BTreeMap<String, VendorRoute> {
                    if home == self.expected_home {
                        let sock = self.current_sock.lock().unwrap().clone();
                        BTreeMap::from([(
                            "kayla".to_string(),
                            VendorRoute::Webhook { socket_path: sock },
                        )])
                    } else {
                        BTreeMap::new()
                    }
                }
            }

            let tmp = TempDir::new().unwrap();
            let sock1 = tmp.path().join("sock1.sock");
            let sock2 = tmp.path().join("sock2.sock");
            let scanner = std::sync::Arc::new(MovingWebhookScanner {
                expected_home: tmp.path().to_path_buf(),
                current_sock: std::sync::Mutex::new(sock1.clone()),
            });
            crate::scanner::register_wire_scanner(scanner.clone()).unwrap();

            let mut base = RouteTable::default();
            base.discover_routes(Some(tmp.path()), At::from_unix_millis(9000));
            assert_eq!(
                base.get("kayla").map(|e| &e.route),
                Some(&VendorRoute::Webhook { socket_path: sock1 })
            );

            *scanner.current_sock.lock().unwrap() = sock2.clone();
            let mut working = base.clone();
            working.discover_routes(Some(tmp.path()), At::from_unix_millis(9500));
            let expected_sock2 = VendorRoute::Webhook { socket_path: sock2 };
            assert_eq!(
                working.get("kayla").map(|e| &e.route),
                Some(&expected_sock2)
            );

            for merge_mode in ["direct", "dispatch", "daemon"] {
                let mut live = base.clone();
                match merge_mode {
                    "direct" => live.merge_newer(working.clone()),
                    "dispatch" => live.merge_dispatch_result(&base, working.clone()),
                    "daemon" => live.merge_daemon_dispatch_result(&base, working.clone()),
                    _ => unreachable!(),
                }
                assert_eq!(
                    live.get("kayla").map(|e| &e.route),
                    Some(&expected_sock2),
                    "scanner Webhook socket move must propagate in mode={merge_mode}"
                );
            }
        }

        // 2. Unknown (0 mtime) is NEVER promoted to "now", and known (Some(100)) outranks unknown
        // (None / 0 mtime) in BOTH directions across upsert_discovered and merge_newer.
        let mut zero_first = RouteTable::default();
        zero_first.upsert_discovered(
            "kayla",
            codex_route("codex-zero"),
            0,
            &At::from_unix_millis(9000),
        );
        let zero_entry = zero_first.get("kayla").expect("zero-mtime route");
        assert_eq!(zero_entry.identity_at, None);
        assert_eq!(zero_entry.identity_evidence_at(), None);
        assert_eq!(
            zero_entry.updated_at,
            At::from_unix_millis(0),
            "zero mtime must never be promoted to scan time"
        );
        // Known challenger (100) beats unknown incumbent (None).
        zero_first.upsert_discovered(
            "kayla",
            codex_route("codex-known-100"),
            100,
            &At::from_unix_millis(9000),
        );
        assert_eq!(
            zero_first.get("kayla").unwrap().route,
            codex_route("codex-known-100")
        );
        assert_eq!(
            zero_first.get("kayla").unwrap().identity_at,
            Some(At::from_unix_millis(100))
        );
        // Unknown challenger (0 mtime) loses to known incumbent (100).
        zero_first.upsert_discovered(
            "kayla",
            codex_route("codex-zero-again"),
            0,
            &At::from_unix_millis(9500),
        );
        assert_eq!(
            zero_first.get("kayla").unwrap().route,
            codex_route("codex-known-100")
        );

        // Same known-vs-unknown symmetry in merge_newer (even when unknown has huge updated_at).
        let unknown_route_entry = |conv: &str| RouteEntry {
            persona: "kayla".into(),
            route: codex_route(conv),
            updated_at: At::from_unix_millis(99_000),
            identity_at: None,
            explicit: false,
            hosted_id: None,
            hydration_generation: None,
        };
        let mut merge_unknown_inc = RouteTable::default();
        merge_unknown_inc
            .entries
            .insert("kayla".into(), unknown_route_entry("unknown-inc"));
        let mut merge_known_chal = RouteTable::default();
        merge_known_chal.upsert("kayla", codex_route("known-100"), At::from_unix_millis(100));
        merge_unknown_inc.merge_newer(merge_known_chal.clone());
        assert_eq!(
            merge_unknown_inc.get("kayla").unwrap().route,
            codex_route("known-100")
        );

        let mut merge_unknown_chal = RouteTable::default();
        merge_unknown_chal
            .entries
            .insert("kayla".into(), unknown_route_entry("unknown-chal"));
        merge_known_chal.merge_newer(merge_unknown_chal);
        assert_eq!(
            merge_known_chal.get("kayla").unwrap().route,
            codex_route("known-100")
        );

        // 3. Equal unknown (None vs None across different sessions): incumbent wins ties in both
        // upsert_discovered and merge_newer.
        let mut unknown_tie = RouteTable::default();
        unknown_tie.upsert_discovered(
            "kayla",
            codex_route("unknown-first"),
            0,
            &At::from_unix_millis(9000),
        );
        unknown_tie.upsert_discovered(
            "kayla",
            codex_route("unknown-second"),
            0,
            &At::from_unix_millis(9500),
        );
        assert_eq!(
            unknown_tie.get("kayla").unwrap().route,
            codex_route("unknown-first")
        );

        let mut merge_none_a = RouteTable::default();
        merge_none_a
            .entries
            .insert("kayla".into(), unknown_route_entry("none-incumbent"));
        let mut merge_none_b = RouteTable::default();
        merge_none_b.entries.insert(
            "kayla".into(),
            RouteEntry {
                persona: "kayla".into(),
                route: codex_route("none-challenger"),
                updated_at: At::from_unix_millis(200_000),
                identity_at: None,
                explicit: false,
                hosted_id: None,
                hydration_generation: None,
            },
        );
        merge_none_a.merge_newer(merge_none_b);
        assert_eq!(
            merge_none_a.get("kayla").unwrap().route,
            codex_route("none-incumbent")
        );
    }

    #[test]
    fn explicit_routes_are_never_replaced_by_passive_agy_discovery_or_aliases() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        agy_registration_fixture(home, "conv-passive-500", "Ada", 500);

        // 1. Explicit cross-vendor route (updated_at: 100) is protected against newer passive
        // cross-vendor discovery and merge_newer.
        let mut cross = RouteTable::default();
        cross.upsert_explicit(
            "ada",
            codex_route("explicit-codex"),
            At::from_unix_millis(100),
        );
        cross.discover_routes(Some(home), At::from_unix_millis(9000));
        assert_eq!(
            cross.get("ada").unwrap().route,
            codex_route("explicit-codex")
        );
        assert!(cross.get("ada").unwrap().explicit);

        let mut passive_other = RouteTable::default();
        passive_other.upsert(
            "ada",
            agy_route("conv-passive-500"),
            At::from_unix_millis(500),
        );
        cross.merge_newer(passive_other);
        assert_eq!(
            cross.get("ada").unwrap().route,
            codex_route("explicit-codex")
        );

        // 2. Explicit Agy route is NEVER replaced or re-pointed by passive Agy discovery, even
        // when another Agy transcript for the same persona has a newer mtime than registration.
        let mut same = RouteTable::default();
        same.upsert_explicit(
            "ada",
            agy_route("explicit-agy-200"),
            At::from_unix_millis(200),
        );
        same.upsert_discovered(
            "ada",
            agy_route("conv-passive-500"),
            500,
            &At::from_unix_millis(9000),
        );
        assert_eq!(
            same.get("ada").unwrap().route,
            agy_route("explicit-agy-200")
        );
        same.discover_routes(Some(home), At::from_unix_millis(9000));
        assert!(
            matches!(
                &same.get("ada").unwrap().route,
                VendorRoute::Agy { conversation_id, .. } if conversation_id == "explicit-agy-200"
            ),
            "passive Agy discovery must never replace an explicit Agy route: got {:?}",
            same.get("ada")
        );
        assert!(same.get("ada").unwrap().explicit);
        assert_eq!(same.get("ada").unwrap().identity_at, None);

        // 3. Hosted reproduction: `upsert_explicit("feira", conv-hosted, 1000)` with `conv-hosted`
        // transcript at mtime 5000 and another `feira` Agy chat at mtime 3000 stays on `conv-hosted`.
        let hosted_tmp = TempDir::new().unwrap();
        let hosted_home = hosted_tmp.path();
        agy_registration_fixture(hosted_home, "conv-hosted", "Feira", 5000);
        agy_registration_fixture(hosted_home, "conv-other-chat", "Feira", 3000);
        let mut hosted = RouteTable::default();
        hosted.upsert_explicit(
            "feira",
            agy_route("conv-hosted"),
            At::from_unix_millis(1000),
        );
        hosted.discover_routes(Some(hosted_home), At::from_unix_millis(9000));
        let feira = hosted.get("feira").expect("feira explicit route");
        assert!(feira.explicit);
        assert!(
            matches!(
                &feira.route,
                VendorRoute::Agy { conversation_id, .. } if conversation_id == "conv-hosted"
            ),
            "got {feira:?}"
        );

        // 4. Explicit alias protection: passive discovery or merge for another persona with the
        // same session ID never evicts the explicit owner.
        hosted.upsert_discovered(
            "alias-feira",
            agy_route("conv-hosted"),
            9000,
            &At::from_unix_millis(9500),
        );
        assert!(hosted.get("alias-feira").is_none());
        let mut alias_table = RouteTable::default();
        alias_table.upsert(
            "alias-feira",
            agy_route("conv-hosted"),
            At::from_unix_millis(9000),
        );
        hosted.merge_newer(alias_table);
        assert!(hosted.get("alias-feira").is_none());
        assert!(hosted.get("feira").unwrap().explicit);
    }

    #[test]
    fn same_routes_ignores_same_session_identity_and_endpoint_timestamp_refreshes() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let agy_transcript = agy_registration_fixture(home, "conv-kayla", "Kayla", 100);
        let codex_tid = "01900000-0000-7000-8000-000000000301";
        let codex_rollout = codex_rollout_fixture(home, codex_tid, "Ferris", 100);

        let mut base = RouteTable::default();
        base.discover_routes(Some(home), At::from_unix_millis(1000));
        assert_eq!(
            base.get("kayla").unwrap().identity_at,
            Some(At::from_unix_millis(100))
        );
        assert_eq!(
            base.get("ferris").unwrap().identity_at,
            Some(At::from_unix_millis(100))
        );

        // Advancing active transcript mtimes on the same winning sessions refreshes `identity_at`
        // in memory while `same_routes` stays `true` (zero persistence churn).
        let mut working = base.clone();
        for (i, mtime) in [200_u64, 300, 400, 500, 600].into_iter().enumerate() {
            set_mtime(&agy_transcript, mtime);
            set_mtime(&codex_rollout, mtime);
            working.discover_routes(Some(home), At::from_unix_millis(2000 + i as u64 * 100));
            assert_eq!(
                working.get("kayla").unwrap().identity_at,
                Some(At::from_unix_millis(mtime))
            );
            assert_eq!(
                working.get("ferris").unwrap().identity_at,
                Some(At::from_unix_millis(mtime))
            );
            assert!(
                working.same_routes(&base),
                "same-session mtime={mtime} refresh must not look like a route change"
            );
        }

        // Changing the winning session for `kayla` (newer Codex rollout at 700 > 600) IS a route change.
        let kayla_codex = "01900000-0000-7000-8000-000000000302";
        codex_rollout_fixture(home, kayla_codex, "Kayla", 700);
        working.discover_routes(Some(home), At::from_unix_millis(3000));
        assert!(!working.same_routes(&base));
    }

    #[test]
    fn passive_agy_in_memory_validation_never_downgrades_newer_disk_reload_in_either_direction() {
        for table1_repaired_at in [None, Some(9500_u64)] {
            for table2_updated_ms in [500_u64, 9600_u64] {
                let tmp = TempDir::new().unwrap();
                let home = tmp.path().join("home");
                let routes_path = tmp.path().join("fl-wire-routes.json");
                let agy_dir = home.join(".gemini/antigravity-cli");
                fs::create_dir_all(&agy_dir).unwrap();
                let bin_dir = home.join(".local/bin");
                fs::create_dir_all(&bin_dir).unwrap();
                let local_agy = bin_dir.join("agy");
                fs::write(&local_agy, b"#!/bin/sh\n").unwrap();
                let old_agy = bin_dir.join("agy.1.old");
                fs::write(&old_agy, b"#!/bin/sh\n").unwrap();

                let transcript = agy_registration_fixture(&home, "conv-kayla", "Kayla", 100);

                // `table1` scans `conv-kayla` at t1 while mtime = 100.
                let mut table1 = RouteTable::default();
                if let Some(repaired_ms) = table1_repaired_at {
                    table1.entries.insert(
                        "kayla".into(),
                        RouteEntry {
                            persona: "kayla".into(),
                            route: VendorRoute::Agy {
                                conversation_id: "conv-kayla".into(),
                                ls_pid: None,
                                ls_address: None,
                                csrf_token: None,
                                agentapi_exe: Some(old_agy),
                            },
                            updated_at: At::from_unix_millis(9000),
                            identity_at: Some(At::from_unix_millis(9000)),
                            explicit: false,
                            hosted_id: None,
                            hydration_generation: None,
                        },
                    );
                    table1.discover_routes(Some(&home), At::from_unix_millis(repaired_ms));
                } else {
                    table1.discover_routes(Some(&home), At::from_unix_millis(9000));
                }
                assert_eq!(
                    table1.get("kayla").unwrap().identity_at,
                    Some(At::from_unix_millis(100))
                );

                // Later on disk, `conv-kayla` advances to mtime = 500 and is persisted to routes.json.
                // `table2` is loaded from disk, so `table2.passive_agy_revalidation` is empty.
                set_mtime(&transcript, 500);
                let mut writer = RouteTable::default();
                writer.discover_routes(Some(&home), At::from_unix_millis(9600));
                writer.get("kayla").expect("writer kayla");
                if let Some(entry) = writer.entries.get_mut("kayla") {
                    entry.updated_at = At::from_unix_millis(table2_updated_ms);
                }
                writer.save(Some(&routes_path)).unwrap();
                let table2 = RouteTable::load(Some(&routes_path));
                assert_eq!(
                    table2.get("kayla").unwrap().identity_at,
                    Some(At::from_unix_millis(500))
                );

                // Merge in both directions (and replay): `Some(500)` must always win over `Some(100)`.
                let mut forward = table1.clone();
                forward.merge_newer(table2.clone());
                assert_eq!(
                    forward.get("kayla").unwrap().identity_at,
                    Some(At::from_unix_millis(500)),
                    "table1.merge_newer(table2): repaired={table1_repaired_at:?}, t2_up={table2_updated_ms}"
                );

                let mut reverse = table2.clone();
                reverse.merge_newer(table1.clone());
                assert_eq!(
                    reverse.get("kayla").unwrap().identity_at,
                    Some(At::from_unix_millis(500)),
                    "table2.merge_newer(table1): repaired={table1_repaired_at:?}, t2_up={table2_updated_ms}"
                );
                reverse.merge_newer(table1.clone());
                assert_eq!(
                    reverse.get("kayla").unwrap().identity_at,
                    Some(At::from_unix_millis(500)),
                    "repeated table2.merge_newer(table1) must not downgrade Some(500)"
                );
            }
        }
    }

    #[test]
    fn passive_agy_revalidation_handles_claude_first_and_reversed_codex_order_across_reconciliation()
     {
        for initial_identity in [None, Some(9000_u64)] {
            for merge_mode in ["direct", "dispatch", "daemon"] {
                // 1. Claude-first during `discover_routes` (`8500 < 9000`): top-of-scan revalidation
                // normalizes `kayla` before `discover_claude_routes` replaces it, so `live`
                // reconciliation also downgrades `live`'s scan-stamped 9000 Agy to 100 and Claude(8500) wins.
                {
                    let tmp = TempDir::new().unwrap();
                    let home = tmp.path();
                    agy_registration_fixture(home, "conv-kayla-100", "Kayla", 100);
                    let claude_sessions = home.join(".claude/sessions");
                    fs::create_dir_all(&claude_sessions).unwrap();
                    let claude_sock = home.join("claude-kayla.sock");
                    fs::write(
                        claude_sessions.join("4242.json"),
                        serde_json::json!({
                            "name": "kayla",
                            "sessionId": "claude-kayla-8500",
                            "messagingSocketPath": claude_sock,
                        })
                        .to_string(),
                    )
                    .unwrap();

                    let mut base = RouteTable::default();
                    base.entries.insert(
                        "kayla".into(),
                        RouteEntry {
                            persona: "kayla".into(),
                            route: agy_route("conv-kayla-100"),
                            updated_at: At::from_unix_millis(9000),
                            identity_at: initial_identity.map(At::from_unix_millis),
                            explicit: false,
                            hosted_id: None,
                            hydration_generation: None,
                        },
                    );
                    let mut working = base.clone();
                    working.discover_routes(Some(home), At::from_unix_millis(8500));
                    assert!(matches!(
                        working.get("kayla").map(|e| &e.route),
                        Some(VendorRoute::Claude { session_id, .. }) if session_id == "claude-kayla-8500"
                    ));

                    let mut live = base.clone();
                    match merge_mode {
                        "direct" => live.merge_newer(working),
                        "dispatch" => live.merge_dispatch_result(&base, working),
                        "daemon" => live.merge_daemon_dispatch_result(&base, working),
                        _ => unreachable!(),
                    }
                    let winner = live.get("kayla").expect("claude winner");
                    assert!(
                        matches!(
                            &winner.route,
                            VendorRoute::Claude { session_id, .. } if session_id == "claude-kayla-8500"
                        ),
                        "claude-first init={initial_identity:?}, mode={merge_mode}: got {winner:?}"
                    );
                    assert_eq!(winner.identity_at, Some(At::from_unix_millis(8500)));

                    live.merge_newer(base.clone());
                    assert!(matches!(
                        live.get("kayla").map(|e| &e.route),
                        Some(VendorRoute::Claude { session_id, .. }) if session_id == "claude-kayla-8500"
                    ));
                }

                // 2. Reversed Codex -> Agy (`200 < 9000`) after the single top-of-scan revalidation
                // pass: Codex(200) beats Agy(100) in both `working` and `live`.
                {
                    let tmp = TempDir::new().unwrap();
                    let home = tmp.path();
                    agy_registration_fixture(home, "conv-kayla-100", "Kayla", 100);
                    let codex_tid = "01900000-0000-7000-8000-000000000208";
                    codex_rollout_fixture(home, codex_tid, "Kayla", 200);

                    let mut base = RouteTable::default();
                    base.entries.insert(
                        "kayla".into(),
                        RouteEntry {
                            persona: "kayla".into(),
                            route: agy_route("conv-kayla-100"),
                            updated_at: At::from_unix_millis(9000),
                            identity_at: initial_identity.map(At::from_unix_millis),
                            explicit: false,
                            hosted_id: None,
                            hydration_generation: None,
                        },
                    );
                    let mut working = base.clone();
                    let agy_candidates = working.revalidate_passive_agy_entries(home);
                    working.discover_codex_routes(home, &At::from_unix_millis(8500));
                    working.discover_agy_routes_with_candidates(
                        home,
                        false,
                        &At::from_unix_millis(8500),
                        agy_candidates,
                    );
                    assert!(
                        matches!(
                            working.get("kayla").map(|e| &e.route),
                            Some(VendorRoute::Codex { thread_id, .. }) if thread_id == codex_tid
                        ),
                        "reversed working init={initial_identity:?}: got {:?}",
                        working.get("kayla")
                    );

                    let mut live = base.clone();
                    match merge_mode {
                        "direct" => live.merge_newer(working),
                        "dispatch" => live.merge_dispatch_result(&base, working),
                        "daemon" => live.merge_daemon_dispatch_result(&base, working),
                        _ => unreachable!(),
                    }
                    let winner = live.get("kayla").expect("codex winner");
                    assert!(
                        matches!(
                            &winner.route,
                            VendorRoute::Codex { thread_id, .. } if thread_id == codex_tid
                        ),
                        "reversed live init={initial_identity:?}, mode={merge_mode}: got {winner:?}"
                    );
                    assert_eq!(winner.identity_at, Some(At::from_unix_millis(200)));

                    live.merge_newer(base.clone());
                    assert!(matches!(
                        live.get("kayla").map(|e| &e.route),
                        Some(VendorRoute::Codex { thread_id, .. }) if thread_id == codex_tid
                    ));
                }
            }
        }
    }

    #[test]
    fn same_session_passive_merge_preserves_repaired_endpoint_and_newest_identity_in_both_directions()
     {
        let repaired_route = VendorRoute::Agy {
            conversation_id: "conv-kayla".into(),
            ls_pid: Some(7777),
            ls_address: Some("127.0.0.1:42100".into()),
            csrf_token: Some("fresh-csrf".into()),
            agentapi_exe: Some(PathBuf::from("/tmp/agy")),
        };
        let stale_route = VendorRoute::Agy {
            conversation_id: "conv-kayla".into(),
            ls_pid: None,
            ls_address: Some("127.0.0.1:1".into()),
            csrf_token: Some("stale-csrf".into()),
            agentapi_exe: None,
        };

        let mut passive_repaired = RouteTable::default();
        passive_repaired.entries.insert(
            "kayla".into(),
            RouteEntry {
                persona: "kayla".into(),
                route: repaired_route.clone(),
                updated_at: At::from_unix_millis(9500),
                identity_at: Some(At::from_unix_millis(100)),
                explicit: false,
                hosted_id: None,
                hydration_generation: None,
            },
        );
        let mut newer_transcript = RouteTable::default();
        newer_transcript.entries.insert(
            "kayla".into(),
            RouteEntry {
                persona: "kayla".into(),
                route: stale_route,
                updated_at: At::from_unix_millis(500),
                identity_at: Some(At::from_unix_millis(500)),
                explicit: false,
                hosted_id: None,
                hydration_generation: None,
            },
        );

        let mut repaired_then_transcript = passive_repaired.clone();
        repaired_then_transcript.merge_newer(newer_transcript.clone());
        let a = repaired_then_transcript.get("kayla").expect("kayla a");
        assert_eq!(a.route, repaired_route);
        assert!(!a.explicit);
        assert_eq!(a.updated_at, At::from_unix_millis(9500));
        assert_eq!(a.identity_at, Some(At::from_unix_millis(500)));

        let mut transcript_then_repaired = newer_transcript;
        transcript_then_repaired.merge_newer(passive_repaired);
        let b = transcript_then_repaired.get("kayla").expect("kayla b");
        assert_eq!(b.route, repaired_route);
        assert!(!b.explicit);
        assert_eq!(b.updated_at, At::from_unix_millis(9500));
        assert_eq!(b.identity_at, Some(At::from_unix_millis(500)));
    }

    #[test]
    fn explicit_agy_endpoint_repair_never_repoints_to_another_agy_conversation() {
        for with_newer_transcript in [false, true] {
            let tmp = TempDir::new().unwrap();
            let home = tmp.path();
            let agy_dir = home.join(".gemini/antigravity-cli");
            fs::create_dir_all(&agy_dir).unwrap();
            let bin_dir = home.join(".local/bin");
            fs::create_dir_all(&bin_dir).unwrap();
            let local_agy = bin_dir.join("agy");
            fs::write(&local_agy, b"#!/bin/sh\n").unwrap();
            let old_agy = bin_dir.join("agy.15.old");
            fs::write(&old_agy, b"#!/bin/sh\n").unwrap();

            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let live_port = listener.local_addr().unwrap().port();
            let live_addr = format!("127.0.0.1:{live_port}");
            fs::write(
                agy_dir.join("cli.log"),
                format!("Language server listening on random port at {live_port} for HTTP\n"),
            )
            .unwrap();
            agy_registration_fixture(home, "conv-old", "Ada", 150);
            if with_newer_transcript {
                agy_registration_fixture(home, "conv-new", "Ada", 500);
            }

            let mut table = RouteTable::default();
            table.entries.insert(
                "ada".into(),
                RouteEntry {
                    persona: "ada".into(),
                    route: VendorRoute::Agy {
                        conversation_id: "conv-old".into(),
                        ls_pid: None,
                        ls_address: Some("127.0.0.1:1".into()),
                        csrf_token: Some("stale-csrf".into()),
                        agentapi_exe: Some(old_agy),
                    },
                    updated_at: At::from_unix_millis(200),
                    identity_at: None,
                    explicit: true,
                    hosted_id: None,
                    hydration_generation: None,
                },
            );
            table.discover_routes(Some(home), At::from_unix_millis(9000));
            let ada = table.get("ada").expect("ada route");
            assert!(ada.explicit);
            assert_eq!(ada.updated_at, At::from_unix_millis(9000));
            assert_eq!(ada.identity_at, None);
            match &ada.route {
                VendorRoute::Agy {
                    conversation_id,
                    ls_address,
                    agentapi_exe,
                    ..
                } => {
                    assert_eq!(
                        conversation_id, "conv-old",
                        "explicit Agy route must stay on conv-old (with_newer={with_newer_transcript})"
                    );
                    assert_eq!(ls_address.as_deref(), Some(live_addr.as_str()));
                    assert_eq!(agentapi_exe.as_deref(), Some(local_agy.as_path()));
                }
                other => panic!("expected Agy route, got {other:?}"),
            }
        }
    }

    #[test]
    fn explicit_non_agy_incumbent_repoints_on_zero_mtime_rollout_at_scan_time() {
        let t1 = "01900000-0000-7000-8000-000000000401";
        let t2 = "01900000-0000-7000-8000-000000000402";
        let now = At::from_unix_millis(9000);

        // 1. Direct `upsert_discovered` with `updated_ms == 0` falls back to `now` (9000) for an
        // explicit Codex incumbent, re-pointing `P` from `T1` (1000) to `T2` (9000).
        let mut direct = RouteTable::default();
        direct.upsert_explicit("parker", codex_route(t1), At::from_unix_millis(1000));
        direct.upsert_discovered("parker", codex_route(t2), 0, &now);
        let direct_entry = direct.get("parker").expect("parker route");
        assert!(direct_entry.explicit);
        assert_eq!(direct_entry.route, codex_route(t2));
        assert_eq!(direct_entry.updated_at, now);
        assert_eq!(direct_entry.identity_at, None);

        // 2. Full `discover_routes` (both zero-mtime rollout file and zero-mtime session_index)
        // plus reconciliation (`merge_newer`, `merge_dispatch_result`, `merge_daemon_dispatch_result`).
        for use_session_index in [false, true] {
            let tmp = TempDir::new().unwrap();
            let home = tmp.path();
            if use_session_index {
                let codex_dir = home.join(".codex");
                fs::create_dir_all(&codex_dir).unwrap();
                let index_path = codex_dir.join("session_index.jsonl");
                fs::write(
                    &index_path,
                    serde_json::json!({
                        "id": t2,
                        "agentName": "Parker",
                        "cwd": "/tmp"
                    })
                    .to_string(),
                )
                .unwrap();
                set_mtime(&index_path, 0);
            } else {
                codex_rollout_fixture(home, t2, "Parker", 0);
            }

            let mut base = RouteTable::default();
            base.upsert_explicit("parker", codex_route(t1), At::from_unix_millis(1000));
            let mut working = base.clone();
            working.discover_routes(Some(home), now.clone());

            let scanned = working.get("parker").expect("scanned parker");
            assert!(
                scanned.explicit,
                "use_session_index={use_session_index}: got {scanned:?}"
            );
            assert!(
                matches!(&scanned.route, VendorRoute::Codex { thread_id, .. } if thread_id == t2),
                "use_session_index={use_session_index}: got {scanned:?}"
            );
            assert_eq!(scanned.updated_at, now);
            assert_eq!(scanned.identity_at, None);

            for merge_mode in ["direct", "dispatch", "daemon"] {
                let mut live = base.clone();
                match merge_mode {
                    "direct" => live.merge_newer(working.clone()),
                    "dispatch" => live.merge_dispatch_result(&base, working.clone()),
                    "daemon" => live.merge_daemon_dispatch_result(&base, working.clone()),
                    _ => unreachable!(),
                }
                let merged = live.get("parker").expect("merged parker");
                assert!(merged.explicit, "mode={merge_mode}");
                assert!(
                    matches!(&merged.route, VendorRoute::Codex { thread_id, .. } if thread_id == t2),
                    "use_session_index={use_session_index}, mode={merge_mode}: got {merged:?}"
                );
                assert_eq!(merged.updated_at, now);
                assert_eq!(merged.identity_at, None);
            }
        }
    }

    #[test]
    fn explicit_non_agy_incumbent_respects_newer_passive_session_owner_across_scan_and_merge() {
        let t1 = "01900000-0000-7000-8000-000000000411";
        let t2 = "01900000-0000-7000-8000-000000000412";
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        codex_rollout_fixture(home, t2, "Parker", 5000);

        let mut base = RouteTable::default();
        base.upsert_explicit("parker", codex_route(t1), At::from_unix_millis(1000));
        base.entries.insert(
            "quinn".into(),
            RouteEntry {
                persona: "quinn".into(),
                route: codex_route(t2),
                updated_at: At::from_unix_millis(8000),
                identity_at: None,
                explicit: false,
                hosted_id: None,
                hydration_generation: None,
            },
        );

        let assert_unchanged = |table: &RouteTable, label: &str| {
            let p = table.get("parker").expect("parker present");
            assert!(p.explicit, "{label}: {p:?}");
            assert_eq!(p.route, codex_route(t1), "{label}: {p:?}");
            assert_eq!(p.updated_at, At::from_unix_millis(1000), "{label}: {p:?}");
            assert_eq!(p.identity_at, None, "{label}: {p:?}");

            let q = table.get("quinn").expect("quinn present");
            assert!(!q.explicit, "{label}: {q:?}");
            assert!(
                matches!(&q.route, VendorRoute::Codex { thread_id, .. } if thread_id == t2),
                "{label}: {q:?}"
            );
            assert_eq!(q.updated_at, At::from_unix_millis(8000), "{label}: {q:?}");
        };

        // 1. Direct `upsert_discovered`.
        let mut direct_upsert = base.clone();
        direct_upsert.upsert_discovered(
            "parker",
            codex_route(t2),
            5000,
            &At::from_unix_millis(9000),
        );
        assert_unchanged(&direct_upsert, "direct upsert_discovered");

        // 2. Direct `discover_routes` on `base`.
        let mut working = base.clone();
        working.discover_routes(Some(home), At::from_unix_millis(9000));
        assert_unchanged(&working, "direct discover_routes");

        // 3. Reconciling `working` into `live` across all merge entry points.
        for merge_mode in ["direct", "dispatch", "daemon"] {
            let mut live = base.clone();
            match merge_mode {
                "direct" => live.merge_newer(working.clone()),
                "dispatch" => live.merge_dispatch_result(&base, working.clone()),
                "daemon" => live.merge_daemon_dispatch_result(&base, working.clone()),
                _ => unreachable!(),
            }
            assert_unchanged(&live, merge_mode);
        }

        // 4. Merging an empty-base scan (`Parker -> T2` passive at 5000) into `base` also agrees.
        let mut fresh_scan = RouteTable::default();
        fresh_scan.discover_routes(Some(home), At::from_unix_millis(9000));
        let mut merged_from_fresh = base.clone();
        merged_from_fresh.merge_newer(fresh_scan);
        assert_unchanged(&merged_from_fresh, "merge_newer(fresh_scan)");
    }

    #[test]
    fn merge_newer_explicit_challenger_beats_passive_incumbent_with_identity_evidence() {
        // (a) Cross-persona on the same session `T1`: passive `Q` at 5000 (`identity_at: Some(5000)`)
        // loses `T1` to explicit `P` at 6000 (`identity_at: None`).
        let mut cross_persona = RouteTable::default();
        cross_persona.upsert("quinn", codex_route("t1"), At::from_unix_millis(5000));
        assert_eq!(
            cross_persona.get("quinn").unwrap().identity_at,
            Some(At::from_unix_millis(5000))
        );
        let mut other_a = RouteTable::default();
        other_a.upsert_explicit("parker", codex_route("t1"), At::from_unix_millis(6000));
        assert_eq!(other_a.get("parker").unwrap().identity_at, None);

        cross_persona.merge_newer(other_a);
        assert!(
            cross_persona.get("quinn").is_none(),
            "passive incumbent must be evicted by newer explicit owner"
        );
        let winner_a = cross_persona.get("parker").expect("parker won t1");
        assert!(winner_a.explicit);
        assert_eq!(winner_a.route, codex_route("t1"));
        assert_eq!(winner_a.updated_at, At::from_unix_millis(6000));
        assert_eq!(winner_a.identity_at, None);

        // (b) Same-persona across different sessions (`T1` -> `T2`, both same-vendor and cross-vendor):
        // passive `P` on `T1` at 5000 (`identity_at: Some(5000)`) moves to explicit `P` on `T2` at 6000.
        for challenger_route in [
            codex_route("t2"),
            agy_route("conv-t2"),
            VendorRoute::Webhook {
                socket_path: PathBuf::from("/tmp/explicit-t2.sock"),
            },
        ] {
            let mut same_persona = RouteTable::default();
            same_persona.upsert("parker", codex_route("t1"), At::from_unix_millis(5000));
            assert_eq!(
                same_persona.get("parker").unwrap().identity_at,
                Some(At::from_unix_millis(5000))
            );
            let mut other_b = RouteTable::default();
            other_b.upsert_explicit(
                "parker",
                challenger_route.clone(),
                At::from_unix_millis(6000),
            );
            assert_eq!(other_b.get("parker").unwrap().identity_at, None);

            same_persona.merge_newer(other_b);
            let winner_b = same_persona.get("parker").expect("parker moved to t2");
            assert!(winner_b.explicit);
            assert_eq!(winner_b.route, challenger_route);
            assert_eq!(winner_b.updated_at, At::from_unix_millis(6000));
            assert_eq!(winner_b.identity_at, None);
        }
    }

    #[test]
    fn canonical_vendor_and_parse_hosted_identity_normalize_and_fail_closed() {
        assert_eq!(agy_route("c1").canonical_vendor(), "antigravity");
        assert_eq!(codex_route("t1").canonical_vendor(), "codex");
        assert_eq!(
            VendorRoute::Claude {
                pid: 1,
                session_id: "s1".into(),
                socket_path: PathBuf::from("/tmp/c.sock"),
                key_path: None,
            }
            .canonical_vendor(),
            "claude"
        );
        assert_eq!(
            VendorRoute::Muse {
                session_id: "m1".into(),
                cwd: None,
                tmux_target: None,
                tmux_pane: None,
            }
            .canonical_vendor(),
            "muse"
        );
        assert_eq!(
            VendorRoute::Grok {
                session_id: "g1".into(),
                cwd: None,
                tmux_target: None,
                tmux_pane: None,
            }
            .canonical_vendor(),
            "grok"
        );
        assert_eq!(
            VendorRoute::HostCli {
                vendor: "AGY".into(),
                session_id: "h1".into(),
                cwd: None,
                tmux_target: None,
                tmux_pane: None,
            }
            .canonical_vendor(),
            "antigravity"
        );
        assert_eq!(
            VendorRoute::Webhook {
                socket_path: PathBuf::from("/tmp/w.sock"),
            }
            .canonical_vendor(),
            "webhook"
        );

        assert_eq!(
            parse_hosted_identity("antigravity:feira"),
            Ok(("antigravity".into(), "feira".into()))
        );
        assert_eq!(
            parse_hosted_identity("agy:Feira"),
            Ok(("antigravity".into(), "feira".into()))
        );
        assert_eq!(
            parse_hosted_identity("codex:fiona"),
            Ok(("codex".into(), "fiona".into()))
        );
        assert_eq!(
            parse_hosted_identity("Feira.antigravity"),
            Ok(("antigravity".into(), "feira".into()))
        );
        assert_eq!(
            parse_hosted_identity("Feira.agy"),
            Ok(("antigravity".into(), "feira".into()))
        );
        assert_eq!(
            parse_hosted_identity("muse.Ada"),
            Ok(("muse".into(), "ada".into()))
        );

        for invalid in [
            "",
            "   ",
            "feira",
            "unknown:feira",
            "feira.unknown",
            "claude:",
            ":feira",
            "claude:bad slug",
            "claude:a:b",
            "Feira.antigravity.extra",
            "claude:feira.agy",
            "muse.muse",
        ] {
            assert!(
                parse_hosted_identity(invalid).is_err(),
                "expected Err for malformed hosted identity `{invalid}`"
            );
        }
    }

    #[test]
    fn explicit_route_is_dead_passive_checks_and_zero_io_codex() {
        let mut child = std::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let dead_pid = child.id();
        let _ = child.wait();

        let tmp = TempDir::new().unwrap();
        let existing_sock = tmp.path().join("present.sock");
        fs::write(&existing_sock, b"").unwrap();
        let missing_sock = tmp.path().join("missing.sock");

        let dead_claude = VendorRoute::Claude {
            pid: dead_pid,
            session_id: "s-dead".into(),
            socket_path: existing_sock.clone(),
            key_path: None,
        };
        assert!(explicit_route_is_dead(&dead_claude, Some("feira"), false));

        let live_claude = VendorRoute::Claude {
            pid: std::process::id(),
            session_id: "s-live".into(),
            socket_path: missing_sock.clone(),
            key_path: None,
        };
        assert!(!explicit_route_is_dead(&live_claude, Some("feira"), false));

        let dead_webhook = VendorRoute::Webhook {
            socket_path: missing_sock,
        };
        assert!(explicit_route_is_dead(&dead_webhook, Some("feira"), false));
        let present_webhook = VendorRoute::Webhook {
            socket_path: existing_sock,
        };
        assert!(!explicit_route_is_dead(
            &present_webhook,
            Some("feira"),
            false
        ));

        let dead_agy = VendorRoute::Agy {
            conversation_id: "conv-dead".into(),
            ls_pid: Some(dead_pid),
            ls_address: None,
            csrf_token: None,
            agentapi_exe: None,
        };
        assert!(explicit_route_is_dead(&dead_agy, Some("feira"), false));

        let hermetic_agy = agy_route("conv-hermetic");
        assert!(!explicit_route_is_dead(&hermetic_agy, Some("feira"), false));

        assert!(!explicit_route_is_dead(
            &codex_route("SESSION_SECRET"),
            Some("held"),
            false
        ));
    }

    #[test]
    fn registration_guard_preserves_metadata_caps_diagnostics_and_guards_incumbent() {
        let mut table = RouteTable::default();
        let live_agy = VendorRoute::Agy {
            conversation_id: "conv-live".into(),
            ls_pid: Some(std::process::id()),
            ls_address: None,
            csrf_token: Some("SECRET_CSRF".into()),
            agentapi_exe: None,
        };
        table
            .register_route_checked(RegisterRouteCheckArgs {
                persona: "Feira".into(),
                route: live_agy.clone(),
                explicit: true,
                replace: true,
                hosted_id: Some("Feira.agy".into()),
                hydration_generation: Some("gen-1".into()),
                hosted_session_verified: true,
                allow_host_probes: false,
                at: At::from_unix_millis(1000),
            })
            .unwrap();
        let entry = table.get("feira").unwrap();
        assert_eq!(entry.hosted_id.as_deref(), Some("antigravity:feira"));
        assert_eq!(entry.hydration_generation.as_deref(), Some("gen-1"));

        // Same-vendor explicit refresh without metadata preserves hosted_id + hydration_generation.
        table.upsert_explicit(
            "feira",
            agy_route("conv-refreshed"),
            At::from_unix_millis(1100),
        );
        let refreshed = table.get("feira").unwrap();
        assert_eq!(refreshed.hosted_id.as_deref(), Some("antigravity:feira"));
        assert_eq!(refreshed.hydration_generation.as_deref(), Some("gen-1"));

        // Put back live_agy with live ls_pid so liveness check sees a live incumbent.
        table
            .register_route_checked(RegisterRouteCheckArgs {
                persona: "feira".into(),
                route: live_agy.clone(),
                explicit: true,
                replace: true,
                hosted_id: Some("antigravity:feira".into()),
                hydration_generation: Some("gen-1".into()),
                hosted_session_verified: true,
                allow_host_probes: false,
                at: At::from_unix_millis(1200),
            })
            .unwrap();

        // Cross-vendor unverified challenger without --replace is refused and recorded.
        let refusal = table
            .register_route_checked(RegisterRouteCheckArgs {
                persona: "feira".into(),
                route: VendorRoute::Claude {
                    pid: std::process::id(),
                    session_id: "spike-1".into(),
                    socket_path: PathBuf::from("/tmp/SECRET_SOCK.sock"),
                    key_path: Some(PathBuf::from("/tmp/SECRET_KEY")),
                },
                explicit: true,
                replace: false,
                hosted_id: None,
                hydration_generation: None,
                hosted_session_verified: false,
                allow_host_probes: false,
                at: At::from_unix_millis(1300),
            })
            .unwrap_err();
        assert_eq!(
            refusal.diagnostic.as_ref().map(|d| (d.outcome, d.reason)),
            Some((
                RegistrationDiagnosticOutcome::Refused,
                RegistrationConflictReason::VendorMismatch
            ))
        );
        assert_eq!(table.get("feira").unwrap().route, live_agy);

        // Secret-free serialization and same_routes invariance + 64-cap FIFO eviction.
        let baseline = {
            let mut b = RouteTable::default();
            b.upsert_explicit("feira", live_agy.clone(), At::from_unix_millis(1200));
            b
        };
        assert!(table.same_routes(&baseline));

        for idx in 0..70_u64 {
            table.push_registration_diagnostic(RouteRegistrationDiagnostic {
                at: At::from_unix_millis(2000 + idx),
                persona: "Feira".into(),
                outcome: RegistrationDiagnosticOutcome::Refused,
                reason: RegistrationConflictReason::ForeignHostedEnv,
                incumbent_adapter: Some(RouteAdapter::Agy),
                challenger_adapter: RouteAdapter::Claude,
                incumbent_hosted_id: Some("antigravity:feira".into()),
                challenger_hosted_id: Some("/tmp/not-a-hosted-id".into()),
                incumbent_generation: Some("gen-1".into()),
                challenger_generation: Some("bad token with spaces".into()),
                replace: false,
            });
        }
        assert_eq!(
            table.registration_diagnostics.len(),
            MAX_REGISTRATION_DIAGNOSTICS
        );
        assert_eq!(
            table.registration_diagnostics.first().unwrap().at,
            At::from_unix_millis(2000 + 6)
        );
        let diag_json = serde_json::to_string(&table.registration_diagnostics).unwrap();
        assert!(!diag_json.contains("SECRET_SOCK"));
        assert!(!diag_json.contains("SECRET_KEY"));
        assert!(!diag_json.contains("SECRET_CSRF"));
        assert!(!diag_json.contains("/tmp/not-a-hosted-id"));
        assert!(!diag_json.contains("bad token"));
    }
}
