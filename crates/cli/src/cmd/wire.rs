//! `fl wire` CLI subcommands, `agent-msg` compatibility shim, and `fl wire hook claude` (spec §5.2, §5.3).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use fl_core::agent::{RegisterAgentArgs, reports_to};
use fl_core::at::At;
use fl_core::ferrowire::{
    DeliveryReceipt, DeliveryStatus, MessageKind, MessageUrgency, SendWireArgs, WireAddress,
    WireFilter, WireInboxItem, WireMessage, parse_role_selector,
};
use fl_core::ids::MessageId;
use fl_core::iri::Iri;
use fl_core::state::StateScope;
use fl_core::store::{AgentRegistry, MessageStore, StoreError};
use fl_wire::daemon::{
    DEFAULT_LOCAL_RPC_TIMEOUT, DaemonConfig, LocalClient, LocalEvent, LocalRequest, LocalResponse,
    default_socket_path, spawn_daemon,
};
use fl_wire::dispatcher::dispatch_message_for_store;
use fl_wire::identity::{
    PersonaSource, RouteTable, VendorRoute, capture_current_env_route, grok_session_is_headless,
    resolve_claude_key_path, resolve_persona_with_source, scan_transcript_for_persona,
};
use serde_json::{Value, json};

use crate::cmd::agent::{now_at, preserve_on_replace, resolve_agent};
use crate::cmd::state::StateEnv;

pub const DEFAULT_RETENTION_DAYS: u64 = 30;

/// The prune cutoff: `days` before `now_ms`. `days` comes from the config or
/// the command line, so the multiplication saturates: a wrapped product would
/// put the cutoff near `now` and prune everything acknowledged.
fn retention_cutoff_ms(now_ms: u64, days: u64) -> u64 {
    now_ms.saturating_sub(days.saturating_mul(86_400_000))
}

pub fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[derive(Subcommand)]
pub enum WireCmd {
    /// Send a message to a persona, AgentId URN, or group selector (`@all`, `@council`, `@<group>`, `@area:<slug>`, `@role:<role>`, `@area:<slug>/<role>`).
    Send {
        to: String,
        subject: String,
        body: String,
        #[arg(long, default_value = "mail")]
        kind: String,
        #[arg(long)]
        urgent: bool,
        #[arg(long)]
        durable: bool,
        #[arg(long)]
        thread: Option<String>,
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Broadcast a message to all active agents (`@all`), excluding the sender.
    Broadcast {
        subject: String,
        body: String,
        #[arg(long)]
        urgent: bool,
        #[arg(long)]
        durable: bool,
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Send a durable `Ask` message and optionally wait up to `--timeout <secs>` for a correlated `Reply`.
    Ask {
        to: String,
        subject: String,
        body: String,
        #[arg(long)]
        timeout: Option<u64>,
        #[arg(long)]
        urgent: bool,
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Reply to a message by `MessageId` (`urn:uuid:...`), inheriting its thread and correlation ID.
    Reply {
        message_id: String,
        body: String,
        #[arg(long)]
        subject: Option<String>,
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Read inbox messages for the resolved caller (`unread_only = !all`).
    Read {
        #[arg(long)]
        ack: bool,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        thread: Option<String>,
        #[arg(long = "for")]
        for_agent: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Acknowledge one or more messages by `MessageId`.
    Ack {
        #[arg(required = true)]
        message_ids: Vec<String>,
        #[arg(long = "for")]
        for_agent: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Register the caller's live vendor route and optionally wait for an unread message.
    Wait {
        #[arg(long)]
        ack: bool,
        #[arg(long)]
        block: bool,
        #[arg(long)]
        timeout: Option<u64>,
        #[arg(long = "for")]
        for_agent: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Prune acknowledged or expired transient messages older than `--older-than-days`.
    Prune {
        #[arg(long)]
        older_than_days: Option<u64>,
        #[arg(long)]
        json: bool,
    },
    /// Report local daemon, peer, and route status.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Run the local Unix-socket and WebSocket peer relay daemon.
    Daemon {
        #[arg(long)]
        listen: Option<String>,
        #[arg(long = "peer")]
        peers: Vec<String>,
        #[arg(long)]
        socket: Option<PathBuf>,
        #[arg(long)]
        token_file: Option<PathBuf>,
    },
    /// Vendor lifecycle hooks (`fl wire hook claude|muse|grok`).
    Hook {
        #[command(subcommand)]
        cmd: HookCmd,
    },
    /// Run the stdio MCP server (`fl state mcp` alias).
    Mcp,
}

#[derive(Subcommand)]
pub enum HookCmd {
    /// Handle Claude Code / Codex lifecycle hook (`Stop`, `SessionStart`, `UserPromptSubmit`, `PostToolUse`).
    Claude,
    /// Handle Meta Muse lifecycle hook (`Stop`, `SessionStart`, `UserPromptSubmit`, `PostToolUse`).
    Muse,
    /// Handle xAI Grok lifecycle hook (`Stop`, `SessionStart`, `UserPromptSubmit`, `PostToolUse`).
    Grok,
}

/// CLI parser matching `AI-Lab-AMD/scripts/agent-bus/agent_bus/cli.py` for `agent-msg`.
#[derive(Parser)]
#[command(name = "agent-msg", about = "FerroWire agent-msg compatibility CLI")]
pub struct AgentMsgCli {
    #[arg(long, global = true)]
    pub server: Option<String>,
    #[arg(long, global = true)]
    pub from: Option<String>,
    #[arg(long, global = true)]
    pub db: Option<PathBuf>,
    #[command(subcommand)]
    pub cmd: AgentMsgCmd,
}

#[derive(Subcommand)]
pub enum AgentMsgCmd {
    Send {
        recipient: String,
        subject: String,
        body: String,
    },
    Broadcast {
        subject: String,
        body: String,
    },
    Read {
        /// Accepted for compatibility; unread-only is the default.
        #[arg(long)]
        unread_only: bool,
        /// Include messages already acknowledged.
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        ack: bool,
    },
    Ack {
        message_id: String,
    },
    Wait {
        #[arg(long)]
        timeout: Option<u64>,
        #[arg(long)]
        ack: bool,
        #[arg(long)]
        block: bool,
    },
    Listen,
    Register {
        name: String,
        #[arg(long)]
        project: String,
        #[arg(long, default_value = "assistant")]
        role: String,
        #[arg(long = "extra-areas")]
        extra_areas: Option<String>,
        #[arg(long)]
        replace: bool,
    },
    Unregister {
        name: String,
    },
    Roster {
        #[arg(long)]
        json: bool,
    },
    /// Run `agent-hydrate` with `args`, so `agent-msg hydrate <Name>` keeps working.
    Hydrate {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

/// The `agent-hydrate` program: the one on `PATH`, else `~/.local/bin/agent-hydrate`.
fn hydrate_program() -> PathBuf {
    let on_path = std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| dir.join("agent-hydrate").is_file())
    });
    if on_path {
        return PathBuf::from("agent-hydrate");
    }
    std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join(".local/bin/agent-hydrate"))
        .unwrap_or_else(|| PathBuf::from("agent-hydrate"))
}

pub fn resolve_socket_path(env: &StateEnv<'_>, explicit: Option<&Path>) -> PathBuf {
    if let Some(p) = explicit {
        return p.to_path_buf();
    }
    if let Some(s) = std::env::var_os("FL_WIRE_SOCKET") {
        let p = PathBuf::from(s);
        if p.is_absolute() {
            return p;
        }
    }
    if let Some(p) = env.cfg.wire.as_ref().and_then(|w| w.socket.clone()) {
        return p;
    }
    if let Some(dir) = env
        .explicit_db
        .and_then(|p| p.parent())
        .filter(|d| !d.as_os_str().is_empty())
    {
        return dir.join("fl-wire.sock");
    }
    default_socket_path()
}

pub fn resolve_routes_path() -> Option<PathBuf> {
    std::env::var_os("FL_WIRE_ROUTES")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

pub fn resolve_routes_path_for(env: &StateEnv<'_>) -> Option<PathBuf> {
    resolve_routes_path().or_else(|| {
        env.explicit_db
            .and_then(|p| p.parent())
            .filter(|d| !d.as_os_str().is_empty())
            .map(|d| d.join("fl-wire-routes.json"))
    })
}

pub fn resolve_caller(
    env: &StateEnv<'_>,
    explicit: Option<&str>,
    transcript: Option<&Path>,
) -> String {
    resolve_caller_with_source(env, explicit, transcript).0
}

/// [`resolve_caller`] that also reports where the persona came from, so a caller about to
/// register a route can tell a named persona from one inferred out of the surroundings.
pub fn resolve_caller_with_source(
    env: &StateEnv<'_>,
    explicit: Option<&str>,
    transcript: Option<&Path>,
) -> (String, PersonaSource) {
    let store = env.open_for_scope(Some(&StateScope::Workspace)).ok();
    let reg = store.as_ref().map(|s| s as &dyn AgentRegistry);
    let rpath = resolve_routes_path_for(env);
    let (persona, source) =
        resolve_persona_with_source(explicit, transcript, env.cwd, reg, rpath.as_deref());
    let has_env_persona_override =
        std::env::var_os("AGENT_PERSONA").is_some() || std::env::var_os("AGENT_NAME").is_some();
    let allow_route_capture = env.explicit_db.is_none() || resolve_routes_path().is_some();
    if explicit.is_none()
        && !has_env_persona_override
        && allow_route_capture
        && persona != "unknown"
        && let Some(route) = capture_current_env_route()
    {
        record_caller_route_checked(env, &persona, route, source);
    }
    (persona, source)
}

fn record_caller_route_with_provenance(
    env: &StateEnv<'_>,
    persona: &str,
    route: VendorRoute,
    explicit: bool,
) -> Result<(), String> {
    let prune_roster = explicit && !matches!(route, VendorRoute::Muse { .. });
    let sock = resolve_socket_path(env, None);
    if let Ok(mut client) = LocalClient::connect(&sock, DEFAULT_LOCAL_RPC_TIMEOUT) {
        let response = client.request(&LocalRequest::RegisterRoute {
            persona: persona.to_string(),
            route,
            explicit,
        })?;
        if !matches!(response, LocalResponse::RouteRegistered) {
            return Err(format!("unexpected daemon response: {response:?}"));
        }
    } else {
        let rpath = resolve_routes_path_for(env);
        let mut table = RouteTable::load(rpath.as_deref());
        if explicit {
            table.upsert_explicit(persona, route, now_at());
        } else {
            table.upsert(persona, route, now_at());
        }
        table
            .save(rpath.as_deref())
            .map_err(|error| error.to_string())?;
    }
    if prune_roster {
        prune_muse_roster(persona).map_err(|error| {
            format!("route registered but Muse boot roster could not be updated: {error}")
        })?;
    }
    Ok(())
}

/// Remove just this persona from the external Muse restore list, preserving other entries.
fn prune_muse_roster(persona: &str) -> std::io::Result<()> {
    use std::io::Write;
    let Some(home) = std::env::var_os("HOME") else {
        return Ok(());
    };
    let path = PathBuf::from(home).join(".config/muse/active-sessions.json");
    let raw = match std::fs::read(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let mut value: Value = serde_json::from_slice(&raw).map_err(std::io::Error::other)?;
    let object = value.as_object_mut().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Muse roster must be a JSON object",
        )
    })?;
    let before = object.len();
    object.retain(|name, _| !name.eq_ignore_ascii_case(persona.trim()));
    if object.len() == before {
        return Ok(());
    }
    let mut temp = tempfile::NamedTempFile::new_in(path.parent().expect("roster has parent"))?;
    if let Ok(metadata) = std::fs::metadata(&path) {
        temp.as_file().set_permissions(metadata.permissions())?;
    }
    serde_json::to_writer_pretty(&mut temp, &value).map_err(std::io::Error::other)?;
    writeln!(temp)?;
    temp.persist(&path).map_err(|error| error.error)?;
    Ok(())
}

pub(crate) fn remove_caller_route(env: &StateEnv<'_>, persona: &str) -> Result<(), String> {
    let sock = resolve_socket_path(env, None);
    let paths: Vec<PathBuf> = match resolve_routes_path_for(env) {
        Some(path) => vec![path],
        None => std::iter::once(fl_wire::identity::default_routes_path())
            .chain(fl_wire::identity::legacy_routes_path())
            .collect(),
    };
    remove_route_at(&sock, &paths, persona)
}

pub(crate) fn sync_store_retired_personas<S: AgentRegistry>(store: &S) {
    #[cfg(test)]
    if resolve_routes_path().is_none() {
        return;
    }
    let mut active_names = std::collections::BTreeSet::new();
    let mut retired_candidates = std::collections::BTreeSet::new();
    for agent in store.list_agents(None, true).unwrap_or_default() {
        let key = agent.name_key.to_ascii_lowercase();
        match agent.status {
            fl_core::agent::AgentStatus::Active => {
                active_names.insert(key);
            }
            fl_core::agent::AgentStatus::Retired => {
                retired_candidates.insert(key);
            }
        }
    }
    let retired: std::collections::BTreeSet<String> = retired_candidates
        .difference(&active_names)
        .cloned()
        .collect();
    let _ = fl_wire::identity::sync_retired_personas_in_tombstones(None, &retired);
}

pub(crate) fn evict_persona_route(persona: &str) {
    #[cfg(test)]
    if resolve_routes_path().is_none() {
        return;
    }
    let sock = std::env::var_os("FL_WIRE_SOCKET")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| {
            crate::config::load(crate::config::path().as_deref())
                .ok()
                .and_then(|cfg| cfg.wire)
                .and_then(|w| w.socket)
        })
        .unwrap_or_else(default_socket_path);
    let paths: Vec<PathBuf> = match resolve_routes_path() {
        Some(path) => vec![path],
        None => std::iter::once(fl_wire::identity::default_routes_path())
            .chain(fl_wire::identity::legacy_routes_path())
            .collect(),
    };
    let _ = remove_route_at(&sock, &paths, persona);
}

fn remove_route_at(sock: &Path, paths: &[PathBuf], persona: &str) -> Result<(), String> {
    let mut daemon_err = None;
    if let Ok(mut client) = LocalClient::connect(sock, DEFAULT_LOCAL_RPC_TIMEOUT) {
        match client.request(&LocalRequest::UnregisterRoute {
            persona: persona.into(),
        }) {
            Ok(LocalResponse::RouteUnregistered) => {}
            Ok(other) => daemon_err = Some(format!("unexpected daemon response: {other:?}")),
            Err(e) => daemon_err = Some(e),
        }
    }
    for path in paths {
        if !path.is_file() {
            continue;
        }
        let mut routes = RouteTable::load(Some(path));
        if routes.entries.remove(persona).is_some() {
            routes.save(Some(path)).map_err(|error| error.to_string())?;
        }
    }
    if let Some(err) = daemon_err {
        return Err(err);
    }
    Ok(())
}

/// May `new` replace `existing` as the route of a persona resolved from `source`?
///
/// A persona the caller named (flag, `AGENT_PERSONA`) or that the session's own transcript
/// declares may replace any route: that is a legitimate switch of vendor. A persona inferred
/// from the surroundings (the parent-process tree, an inherited session id, the cwd) may
/// refresh a route of the same vendor but never replace a route of a different vendor: a
/// child process such as `grok -p` run from an Antigravity shell would otherwise take over
/// its parent's route.
pub fn route_replacement_allowed(
    existing: Option<&VendorRoute>,
    new: &VendorRoute,
    source: PersonaSource,
) -> bool {
    source.is_trusted()
        || existing.is_none_or(|old| std::mem::discriminant(old) == std::mem::discriminant(new))
}

/// True when the process behind `route` is demonstrably gone. Only what the route carries
/// counts: a Claude `pid`, an Antigravity `ls_pid`. A route with no liveness signal is never
/// dead, so it stays protected.
pub fn route_is_dead(route: &VendorRoute, proc_exists: &dyn Fn(u32) -> bool) -> bool {
    match route {
        VendorRoute::Claude { pid, .. } => !proc_exists(*pid),
        VendorRoute::Agy {
            ls_pid: Some(pid), ..
        } => !proc_exists(*pid),
        _ => false,
    }
}

fn route_vendor_name(route: &VendorRoute) -> &'static str {
    match route {
        VendorRoute::Claude { .. } => "Claude",
        VendorRoute::Agy { .. } => "Antigravity",
        VendorRoute::Codex { .. } => "Codex",
        VendorRoute::Muse { .. } => "Muse",
        VendorRoute::Grok { .. } => "Grok",
        VendorRoute::Webhook { .. } => "Webhook",
    }
}

/// [`route_replacement_allowed`] plus the dead-route escape: an inferred persona may replace a
/// route of another vendor when the old route's process is gone. `Err` carries the one-line
/// diagnostic to show when the replacement is refused.
pub fn route_replacement_verdict(
    persona: &str,
    existing: Option<&VendorRoute>,
    new: &VendorRoute,
    source: PersonaSource,
    proc_exists: &dyn Fn(u32) -> bool,
) -> Result<(), String> {
    if route_replacement_allowed(existing, new, source) {
        return Ok(());
    }
    if let Some(old) = existing {
        if route_is_dead(old, proc_exists) {
            return Ok(());
        }
        return Err(format!(
            "fl wire: not replacing {persona}'s {} route from an inferred {} session; \
             set AGENT_PERSONA={persona} or run `agent-msg register` to switch",
            route_vendor_name(old),
            route_vendor_name(new),
        ));
    }
    Ok(())
}

fn process_exists(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// The route currently on file for `persona`: the daemon's table when it answers, else the
/// routes file.
fn existing_route_for(env: &StateEnv<'_>, persona: &str) -> Option<VendorRoute> {
    let sock = resolve_socket_path(env, None);
    if let Ok(mut client) = LocalClient::connect(&sock, DEFAULT_LOCAL_RPC_TIMEOUT)
        && let Ok(LocalResponse::Status(st)) = client.request(&LocalRequest::Status)
    {
        return st
            .routes
            .into_iter()
            .find(|r| r.persona.eq_ignore_ascii_case(persona))
            .map(|r| r.route);
    }
    let rpath = resolve_routes_path_for(env);
    RouteTable::load(rpath.as_deref())
        .get(persona)
        .map(|e| e.route.clone())
}

/// [`record_caller_route`] behind the provenance guard of [`route_replacement_allowed`].
/// Returns `false`, leaving the route table alone, when the guard refuses.
pub fn record_caller_route_checked(
    env: &StateEnv<'_>,
    persona: &str,
    route: VendorRoute,
    source: PersonaSource,
) -> bool {
    // A trusted source replaces anything, so the table is read only for an inferred persona.
    if !source.is_trusted() {
        let existing = existing_route_for(env, persona);
        if let Err(note) =
            route_replacement_verdict(persona, existing.as_ref(), &route, source, &process_exists)
        {
            // stderr only: the hook's stdout must stay valid JSON.
            eprintln!("{note}");
            return false;
        }
    }
    if let Err(error) = record_caller_route_with_provenance(
        env,
        persona,
        route,
        // A session's own transcript is as explicit as AGENT_PERSONA. Codex sessions share
        // one app-server, so the process environment names whoever started the daemon.
        // The transcript has to be able to take its own thread id back from that alias.
        matches!(
            source,
            PersonaSource::Explicit | PersonaSource::Env | PersonaSource::Transcript
        ),
    ) {
        eprintln!("fl wire: could not register route: {error}");
        return false;
    }
    true
}

pub fn parse_message_id(raw: &str) -> Result<MessageId, String> {
    let trimmed = raw.trim();
    if trimmed.starts_with("urn:uuid:") {
        let iri = Iri::parse(trimmed).map_err(|e| format!("invalid message ID `{raw}`: {e}"))?;
        Ok(MessageId(iri))
    } else {
        let with_prefix = format!("urn:uuid:{trimmed}");
        let iri =
            Iri::parse(&with_prefix).map_err(|e| format!("invalid message ID `{raw}`: {e}"))?;
        Ok(MessageId(iri))
    }
}

pub fn execute_send(
    env: &StateEnv<'_>,
    mut args: SendWireArgs,
) -> Result<(WireMessage, Vec<DeliveryReceipt>), String> {
    let message_id = args
        .id
        .get_or_insert_with(|| {
            MessageId(
                Iri::parse(&format!("urn:uuid:{}", uuid::Uuid::now_v7()))
                    .expect("a formatted UUID is a valid IRI"),
            )
        })
        .clone();
    let sock = resolve_socket_path(env, None);
    if let Ok(mut client) = LocalClient::connect(&sock, DEFAULT_LOCAL_RPC_TIMEOUT) {
        let uncertain = |error: String| {
            format!(
                "message {message_id} may already be delivered; check its status before retrying: {error}"
            )
        };
        match client
            .request(&LocalRequest::Send { args })
            .map_err(uncertain)?
        {
            LocalResponse::Sent { message, receipts } => return Ok((message, receipts)),
            other => return Err(uncertain(format!("unexpected daemon response: {other:?}"))),
        }
    }
    let store = env.open_for_scope(Some(&StateScope::Workspace))?;
    let (msg, receipts) = store.send_wire(args).map_err(|e| e.to_string())?;
    let rpath = resolve_routes_path_for(env);
    let mut routes = RouteTable::load(rpath.as_deref());
    let _ = dispatch_message_for_store(&store, &mut routes, None, &msg, now_at());
    let latest_receipts = store.list_wire_receipts(&msg.id).unwrap_or(receipts);
    Ok((msg, latest_receipts))
}

pub fn execute_read(
    env: &StateEnv<'_>,
    filter: WireFilter,
    ack: bool,
) -> Result<Vec<WireInboxItem>, String> {
    let sock = resolve_socket_path(env, None);
    if let Ok(mut client) = LocalClient::connect(&sock, DEFAULT_LOCAL_RPC_TIMEOUT) {
        match client.request(&LocalRequest::Read { filter, ack })? {
            LocalResponse::Inbox { items } => return Ok(items),
            other => return Err(format!("unexpected daemon response: {other:?}")),
        }
    }
    let store = env.open_for_scope(Some(&StateScope::Workspace))?;
    let mut items = store.list_wire_inbox(&filter).map_err(|e| e.to_string())?;
    if ack && let Some(ref rec) = filter.recipient {
        let now = now_at();
        for item in &mut items {
            if item.receipt.status != DeliveryStatus::Acked
                && let Ok(Some(updated)) = store.update_wire_receipt(
                    &item.message.id,
                    rec,
                    DeliveryStatus::Acked,
                    false,
                    now.clone(),
                )
            {
                item.receipt = updated;
            }
        }
    }
    Ok(items)
}

pub fn execute_ack(
    env: &StateEnv<'_>,
    message_ids: Vec<MessageId>,
    recipient: String,
) -> Result<Vec<DeliveryReceipt>, String> {
    let sock = resolve_socket_path(env, None);
    if let Ok(mut client) = LocalClient::connect(&sock, DEFAULT_LOCAL_RPC_TIMEOUT) {
        match client.request(&LocalRequest::Ack {
            message_ids,
            recipient,
        })? {
            LocalResponse::Acked { receipts } => return Ok(receipts),
            other => return Err(format!("unexpected daemon response: {other:?}")),
        }
    }
    let store = env.open_for_scope(Some(&StateScope::Workspace))?;
    let now = now_at();
    let mut out = Vec::new();
    for mid in message_ids {
        if let Some(rcpt) = store
            .update_wire_receipt(&mid, &recipient, DeliveryStatus::Acked, false, now.clone())
            .map_err(|e| e.to_string())?
        {
            out.push(rcpt);
        }
    }
    Ok(out)
}

pub fn execute_reply(
    env: &StateEnv<'_>,
    parent_id: &MessageId,
    subject_override: Option<String>,
    body: String,
    sender_name: String,
) -> Result<(WireMessage, Vec<DeliveryReceipt>), String> {
    let parent = {
        let store = env.open_for_scope(Some(&StateScope::Workspace))?;
        store
            .get_wire_message(parent_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no such wire message: {parent_id}"))?
    };
    let subject = match subject_override {
        Some(s) => s,
        None => {
            if parent.subject.starts_with("Re: ") || parent.subject.starts_with("re: ") {
                parent.subject.clone()
            } else {
                format!("Re: {}", parent.subject)
            }
        }
    };
    let to = match parent.sender_id {
        Some(id) => WireAddress::AgentId(id),
        None => WireAddress::AgentName(parent.sender_name.clone()),
    };
    execute_send(
        env,
        SendWireArgs {
            id: None,
            sender_id: None,
            sender_name,
            sender_host: None,
            to,
            kind: MessageKind::Reply,
            urgency: MessageUrgency::Normal,
            durable: parent.durable,
            subject,
            body,
            reply_to: Some(parent.id),
            thread_id: Some(parent.thread_id),
            deadline_at: None,
            at: now_at(),
        },
    )
}

pub fn wait_for_reply(
    env: &StateEnv<'_>,
    ask_id: &MessageId,
    recipient: &str,
    timeout: Duration,
) -> Result<Option<WireMessage>, String> {
    let sock = resolve_socket_path(env, None);
    let deadline = Instant::now() + timeout;

    if let Ok(mut sub_client) = LocalClient::connect(&sock, DEFAULT_LOCAL_RPC_TIMEOUT)
        && sub_client
            .subscribe(Some(recipient.to_string()), Some(ask_id.clone()))
            .is_ok()
    {
        // Check if reply already arrived before subscribe completed
        if let Some(reply) = poll_store_for_reply(env, ask_id)? {
            return Ok(Some(reply));
        }
        while Instant::now() < deadline {
            let rem = deadline.saturating_duration_since(Instant::now());
            if let Some(LocalEvent::Message { message }) = sub_client.wait_for_event(rem)?
                && message.correlation_id.as_ref() == Some(ask_id)
            {
                return Ok(Some(message));
            }
        }
        return Ok(None);
    }

    // Store polling fallback when daemon is not running
    loop {
        if let Some(reply) = poll_store_for_reply(env, ask_id)? {
            return Ok(Some(reply));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn poll_store_for_reply(
    env: &StateEnv<'_>,
    ask_id: &MessageId,
) -> Result<Option<WireMessage>, String> {
    let store = env.open_for_scope(Some(&StateScope::Workspace))?;
    let mut replies = store.list_wire_replies(ask_id).map_err(|e| e.to_string())?;
    Ok(replies.pop())
}

pub fn run(env: &StateEnv<'_>, cmd: WireCmd) -> Result<i32, (i32, String)> {
    match cmd {
        WireCmd::Send {
            to,
            subject,
            body,
            kind,
            urgent,
            durable,
            thread,
            from,
            json,
        } => {
            let parsed_to = WireAddress::parse(&to).map_err(|e| (1, e.to_string()))?;
            let parsed_kind = MessageKind::from_wire(&kind).ok_or_else(|| {
                (
                    1,
                    format!(
                        "unknown message kind `{kind}`; expected one of: {}",
                        MessageKind::wire_values()
                    ),
                )
            })?;
            let thread_id = thread
                .as_deref()
                .map(parse_message_id)
                .transpose()
                .map_err(|e| (1, e))?;
            let sender_name = resolve_caller(env, from.as_deref(), None);
            let urgency = if urgent {
                MessageUrgency::Urgent
            } else {
                MessageUrgency::Normal
            };
            let (msg, receipts) = execute_send(
                env,
                SendWireArgs {
                    id: None,
                    sender_id: None,
                    sender_name,
                    sender_host: None,
                    to: parsed_to,
                    kind: parsed_kind,
                    urgency,
                    durable,
                    subject,
                    body,
                    reply_to: None,
                    thread_id,
                    deadline_at: None,
                    at: now_at(),
                },
            )
            .map_err(|e| (1, e))?;
            print_sent(&msg, &receipts, json);
            Ok(0)
        }
        WireCmd::Broadcast {
            subject,
            body,
            urgent,
            durable,
            from,
            json,
        } => {
            let sender_name = resolve_caller(env, from.as_deref(), None);
            let urgency = if urgent {
                MessageUrgency::Urgent
            } else {
                MessageUrgency::Normal
            };
            let (msg, receipts) = execute_send(
                env,
                SendWireArgs {
                    id: None,
                    sender_id: None,
                    sender_name,
                    sender_host: None,
                    to: WireAddress::All,
                    kind: MessageKind::Mail,
                    urgency,
                    durable,
                    subject,
                    body,
                    reply_to: None,
                    thread_id: None,
                    deadline_at: None,
                    at: now_at(),
                },
            )
            .map_err(|e| (1, e))?;
            print_sent(&msg, &receipts, json);
            Ok(0)
        }
        WireCmd::Ask {
            to,
            subject,
            body,
            timeout,
            urgent,
            from,
            json,
        } => {
            let parsed_to = WireAddress::parse(&to).map_err(|e| (1, e.to_string()))?;
            let sender_name = resolve_caller(env, from.as_deref(), None);
            let urgency = if urgent {
                MessageUrgency::Urgent
            } else {
                MessageUrgency::Normal
            };
            let now_ms = now_millis();
            let now = At::from_unix_millis(now_ms);
            let deadline_at = timeout.map(|secs| At::from_unix_millis(now_ms + secs * 1000));
            let (ask_msg, receipts) = execute_send(
                env,
                SendWireArgs {
                    id: None,
                    sender_id: None,
                    sender_name: sender_name.clone(),
                    sender_host: None,
                    to: parsed_to,
                    kind: MessageKind::Ask,
                    urgency,
                    durable: true,
                    subject,
                    body,
                    reply_to: None,
                    thread_id: None,
                    deadline_at,
                    at: now,
                },
            )
            .map_err(|e| (1, e))?;

            let Some(secs) = timeout else {
                print_sent(&ask_msg, &receipts, json);
                return Ok(0);
            };

            match wait_for_reply(env, &ask_msg.id, &sender_name, Duration::from_secs(secs))
                .map_err(|e| (1, e))?
            {
                Some(reply) => {
                    if json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&json!({
                                "ask": ask_msg,
                                "reply": reply,
                            }))
                            .unwrap()
                        );
                    } else {
                        println!(
                            "[{}] from {}: {}\n{}",
                            reply.id, reply.sender_name, reply.subject, reply.body
                        );
                    }
                    Ok(0)
                }
                None => Err((
                    124,
                    format!(
                        "timed out after {secs}s waiting for reply to {}",
                        ask_msg.id
                    ),
                )),
            }
        }
        WireCmd::Reply {
            message_id,
            body,
            subject,
            from,
            json,
        } => {
            let parent_id = parse_message_id(&message_id).map_err(|e| (1, e))?;
            let sender_name = resolve_caller(env, from.as_deref(), None);
            let (msg, receipts) =
                execute_reply(env, &parent_id, subject, body, sender_name).map_err(|e| (1, e))?;
            print_sent(&msg, &receipts, json);
            Ok(0)
        }
        WireCmd::Read {
            ack,
            all,
            thread,
            for_agent,
            json,
        } => {
            let recipient = resolve_caller(env, for_agent.as_deref(), None);
            let thread_id = thread
                .as_deref()
                .map(parse_message_id)
                .transpose()
                .map_err(|e| (1, e))?;
            let items = execute_read(
                env,
                WireFilter {
                    recipient: Some(recipient),
                    unread_only: !all,
                    thread_id,
                    correlation_id: None,
                    limit: None,
                },
                ack,
            )
            .map_err(|e| (1, e))?;
            print_inbox_items(&items, json);
            Ok(0)
        }
        WireCmd::Ack {
            message_ids,
            for_agent,
            json,
        } => {
            let recipient = resolve_caller(env, for_agent.as_deref(), None);
            let mut ids = Vec::with_capacity(message_ids.len());
            for raw in &message_ids {
                ids.push(parse_message_id(raw).map_err(|e| (1, e))?);
            }
            let receipts = execute_ack(env, ids, recipient).map_err(|e| (1, e))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&receipts).unwrap());
            } else {
                for r in receipts {
                    println!(
                        "acked\t{}\t{}\t{}",
                        r.message_id,
                        r.recipient_name,
                        r.status.as_wire()
                    );
                }
            }
            Ok(0)
        }
        WireCmd::Wait {
            ack,
            block,
            timeout,
            for_agent,
            json,
        } => {
            let persona = resolve_caller(env, for_agent.as_deref(), None);
            let items = execute_read(
                env,
                WireFilter {
                    recipient: Some(persona.clone()),
                    unread_only: true,
                    thread_id: None,
                    correlation_id: None,
                    limit: None,
                },
                ack,
            )
            .map_err(|e| (1, e))?;

            if !items.is_empty() {
                print_inbox_items(&items, json);
                return Ok(0);
            }

            if !block && timeout.is_none() {
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&json!({
                            "status": "dispatcher_active",
                            "persona": persona,
                        }))
                        .unwrap()
                    );
                } else {
                    println!(
                        "Central FerroWire dispatcher is active for '{persona}'. No background wait process is needed."
                    );
                }
                return Ok(0);
            }

            let wait_dur = Duration::from_secs(timeout.unwrap_or(300));
            let deadline = Instant::now() + wait_dur;
            let sock = resolve_socket_path(env, None);

            if let Ok(mut sub) = LocalClient::connect(&sock, DEFAULT_LOCAL_RPC_TIMEOUT)
                && sub.subscribe(Some(persona.clone()), None).is_ok()
            {
                while Instant::now() < deadline {
                    let rem = deadline.saturating_duration_since(Instant::now());
                    if let Some(LocalEvent::Message { .. }) =
                        sub.wait_for_event(rem).map_err(|e| (1, e))?
                    {
                        let arrived = execute_read(
                            env,
                            WireFilter {
                                recipient: Some(persona),
                                unread_only: true,
                                thread_id: None,
                                correlation_id: None,
                                limit: None,
                            },
                            ack,
                        )
                        .map_err(|e| (1, e))?;
                        print_inbox_items(&arrived, json);
                        return Ok(0);
                    }
                }
                return Ok(2);
            }

            while Instant::now() < deadline {
                let arrived = execute_read(
                    env,
                    WireFilter {
                        recipient: Some(persona.clone()),
                        unread_only: true,
                        thread_id: None,
                        correlation_id: None,
                        limit: None,
                    },
                    ack,
                )
                .map_err(|e| (1, e))?;
                if !arrived.is_empty() {
                    print_inbox_items(&arrived, json);
                    return Ok(0);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok(2)
        }
        WireCmd::Prune {
            older_than_days,
            json,
        } => {
            let days = older_than_days
                .or_else(|| env.cfg.wire.as_ref().and_then(|w| w.retention_days))
                .unwrap_or(DEFAULT_RETENTION_DAYS);
            let cutoff_ms = retention_cutoff_ms(now_millis(), days);
            let before = At::from_unix_millis(cutoff_ms);
            let store = env
                .open_for_scope(Some(&StateScope::Workspace))
                .map_err(|e| (1, e))?;
            let pruned = store.prune_wire(before).map_err(|e| (1, e.to_string()))?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "pruned": pruned,
                        "older_than_days": days,
                    }))
                    .unwrap()
                );
            } else {
                println!("pruned {pruned} transient wire messages older than {days}d");
            }
            Ok(0)
        }
        WireCmd::Status { json } => {
            let sock = resolve_socket_path(env, None);
            if let Ok(mut client) = LocalClient::connect(&sock, DEFAULT_LOCAL_RPC_TIMEOUT)
                && let Ok(LocalResponse::Status(st)) = client.request(&LocalRequest::Status)
            {
                if json {
                    println!("{}", serde_json::to_string_pretty(&st).unwrap());
                } else {
                    println!(
                        "daemon=active\tnode={}\thost={}\tsocket={}\tpeers={}\troutes={}\t{}",
                        st.node_id,
                        st.host,
                        st.socket_path.display(),
                        st.connected_peers.len(),
                        st.routes.len(),
                        dispatch_diagnostics_summary(st.dispatch_diagnostics.as_ref())
                    );
                }
                return Ok(0);
            }
            let rpath = resolve_routes_path_for(env);
            let routes = RouteTable::load(rpath.as_deref());
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "daemon": "inactive",
                        "dispatch_diagnostics": null,
                        "socket_path": sock,
                        "routes": routes.entries.values().collect::<Vec<_>>(),
                    }))
                    .unwrap()
                );
            } else {
                println!(
                    "daemon=inactive\tsocket={}\troutes={}\tdispatch_diagnostics=unavailable",
                    sock.display(),
                    routes.entries.len()
                );
            }
            Ok(0)
        }
        WireCmd::Daemon {
            listen,
            peers,
            socket,
            token_file,
        } => {
            let sock = resolve_socket_path(env, socket.as_deref());
            let listen_addr =
                listen.or_else(|| env.cfg.wire.as_ref().and_then(|w| w.listen.clone()));
            let mut peer_list = peers;
            if peer_list.is_empty()
                && let Some(cfg_peers) = env.cfg.wire.as_ref().map(|w| &w.peers)
            {
                peer_list = cfg_peers.clone();
            }
            let tfile =
                token_file.or_else(|| env.cfg.wire.as_ref().and_then(|w| w.token_file.clone()));
            let psk = if let Some(tf) = tfile {
                std::fs::read_to_string(&tf)
                    .map_err(|e| (1, format!("could not read {}: {e}", tf.display())))?
                    .trim()
                    .as_bytes()
                    .to_vec()
            } else if let Ok(tok) = std::env::var("FL_WIRE_TOKEN") {
                tok.trim().as_bytes().to_vec()
            } else {
                Vec::new()
            };

            let store = env
                .open_for_scope(Some(&StateScope::Workspace))
                .map_err(|e| (1, e))?;
            let host = std::env::var("HOSTNAME")
                .or_else(|_| std::fs::read_to_string("/etc/hostname").map(|s| s.trim().to_string()))
                .unwrap_or_else(|_| "localhost".into());
            let handle = spawn_daemon(
                Arc::new(store),
                DaemonConfig {
                    node_id: host.clone(),
                    host,
                    socket_path: sock,
                    routes_path: resolve_routes_path_for(env),
                    home_override: None,
                    listen_addr,
                    peers: peer_list,
                    psk,
                },
            )
            .map_err(|e| (1, e.to_string()))?;

            // Keep foreground daemon alive until Shutdown request
            while handle.socket_path().exists() {
                std::thread::sleep(Duration::from_millis(100));
            }
            Ok(0)
        }
        WireCmd::Hook { cmd } => match cmd {
            HookCmd::Claude => run_vendor_hook(env, Some("claude")),
            HookCmd::Muse => run_vendor_hook(env, Some("muse")),
            HookCmd::Grok => run_vendor_hook(env, Some("grok")),
        },
        WireCmd::Mcp => crate::cmd::state_mcp::run(env)
            .map(|()| 0)
            .map_err(|e| (1, e)),
    }
}

pub fn run_claude_hook(env: &StateEnv<'_>) -> Result<i32, (i32, String)> {
    run_vendor_hook(env, None)
}

pub fn run_vendor_hook(
    env: &StateEnv<'_>,
    explicit_vendor: Option<&str>,
) -> Result<i32, (i32, String)> {
    let mut raw = String::new();
    std::io::stdin()
        .read_to_string(&mut raw)
        .map_err(|e| (1, e.to_string()))?;
    let out = vendor_hook_response(env, explicit_vendor, &raw)?;
    println!("{}", serde_json::to_string(&out).unwrap());
    Ok(0)
}

/// The hook's JSON answer for the payload `raw`, registering the session's route on the way.
fn vendor_hook_response(
    env: &StateEnv<'_>,
    explicit_vendor: Option<&str>,
    raw: &str,
) -> Result<Value, (i32, String)> {
    if raw.trim().is_empty() {
        return Ok(json!({}));
    }
    let payload: Value = serde_json::from_str(raw).unwrap_or_else(|_| json!({}));
    if payload
        .get("stop_hook_active")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Ok(json!({}));
    }
    let env_grok_event = std::env::var("GROK_HOOK_EVENT").ok();
    let event_name = payload
        .get("hook_event_name")
        .or_else(|| payload.get("hookEventName"))
        .or_else(|| payload.get("event"))
        .and_then(Value::as_str)
        .or(env_grok_event.as_deref())
        .unwrap_or("Stop");
    let transcript_path = payload
        .get("transcript_path")
        .and_then(Value::as_str)
        .map(PathBuf::from);
    let hook_cwd = payload
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| env.cwd.to_path_buf());
    let session_id = payload
        .get("session_id")
        .or_else(|| payload.get("sessionId"))
        .or_else(|| payload.get("thread_id"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let hook_env = StateEnv {
        explicit_db: env.explicit_db,
        default_store: env.default_store,
        cfg: env.cfg,
        cwd: &hook_cwd,
    };

    // Classify the vendor first: a headless Grok session must be turned away before the
    // persona lookup, which can record a route of its own.
    let vendor_str = explicit_vendor.or_else(|| payload.get("vendor").and_then(Value::as_str));
    let tp_str = transcript_path
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let is_muse = vendor_str == Some("muse")
        || tp_str.contains(".muse")
        || tp_str.contains("/muse/")
        || payload.get("event").is_some();
    let is_grok = vendor_str == Some("grok")
        || tp_str.contains(".grok")
        || std::env::var_os("GROK_HOOK_EVENT").is_some()
        || payload.get("hookEventName").is_some();
    let is_codex = vendor_str == Some("codex") || tp_str.contains(".codex");

    // `grok -p` is a one-shot command, not an agent. It fires the same hooks as an interactive
    // session but must never touch the route table or read the inbox. Its summary may not
    // exist yet at `session_start`; the provenance guard below covers that case.
    if is_grok
        && let Some(home) = std::env::var_os("HOME").map(PathBuf::from)
        && grok_session_is_headless(&home, &session_id, transcript_path.as_deref())
    {
        return Ok(json!({}));
    }

    // Codex 0.166 shares one app-server daemon. That process keeps AGENT_PERSONA from
    // whichever session started it, so the hook environment is not the session that fired.
    // Never fall back to the process environment for Codex: resolve from the transcript or
    // an existing Codex route for `session_id`.
    let (persona, persona_source) = if is_codex {
        if let Some(found) = transcript_path
            .as_deref()
            .and_then(|tp| scan_transcript_for_persona(tp, 50))
        {
            (found, PersonaSource::Transcript)
        } else if !session_id.is_empty()
            && let Some(entry) = RouteTable::load(resolve_routes_path_for(&hook_env).as_deref())
                .entries
                .values()
                .find(|entry| {
                    matches!(
                        &entry.route,
                        VendorRoute::Codex { thread_id, .. } if thread_id == &session_id
                    )
                })
        {
            (entry.persona.clone(), PersonaSource::Transcript)
        } else {
            return Ok(json!({}));
        }
    } else {
        resolve_caller_with_source(&hook_env, None, transcript_path.as_deref())
    };
    if persona == "unknown" {
        return Ok(json!({}));
    }

    // Register route for Muse, Grok, Codex, or Claude

    let route = if is_muse && !session_id.is_empty() {
        Some(VendorRoute::Muse {
            session_id,
            cwd: Some(hook_cwd.clone()),
            // The tmux window is not guessed here: route discovery ties the route to the live
            // `muse:<persona>` or `<Session>:muse` window (see
            // `RouteTable::attach_muse_tmux_windows`).
            tmux_target: None,
            tmux_pane: std::env::var("TMUX_PANE")
                .ok()
                .filter(|s| !s.trim().is_empty()),
        })
    } else if is_grok && !session_id.is_empty() {
        let mut cap = persona.clone();
        if let Some(first) = cap.get_mut(0..1) {
            first.make_ascii_uppercase();
        }
        Some(VendorRoute::Grok {
            session_id,
            cwd: Some(hook_cwd.clone()),
            tmux_target: Some(format!("{cap}:grok")),
            tmux_pane: std::env::var("TMUX_PANE")
                .ok()
                .filter(|s| !s.trim().is_empty()),
        })
    } else if is_codex && !session_id.is_empty() {
        Some(VendorRoute::Codex {
            thread_id: session_id,
            cwd: Some(hook_cwd.clone()),
        })
    } else if let Some(sock) = std::env::var_os("CLAUDE_CODE_MESSAGING_SOCKET") {
        let socket_path = PathBuf::from(sock);
        let pid = socket_path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or_else(std::process::id);
        let key_path = std::env::var_os("HOME").map(PathBuf::from).and_then(|h| {
            resolve_claude_key_path(&h.join(".claude").join("sessions"), pid, &socket_path)
        });
        Some(VendorRoute::Claude {
            pid,
            session_id,
            socket_path,
            key_path,
        })
    } else {
        None
    };
    // A persona inferred from the process tree must not take a route over from another vendor.
    if let Some(route) = route
        && !record_caller_route_checked(&hook_env, &persona, route, persona_source)
    {
        return Ok(json!({}));
    }

    let items = execute_read(
        &hook_env,
        WireFilter {
            recipient: Some(persona.clone()),
            unread_only: true,
            thread_id: None,
            correlation_id: None,
            limit: None,
        },
        true,
    )
    .unwrap_or_default();

    if items.is_empty() {
        return Ok(json!({}));
    }

    let mut formatted = Vec::with_capacity(items.len());
    for item in &items {
        formatted.push(format!(
            "[fl-wire {}/{}] from {} (id: {}): {}\n{}",
            item.message.kind.as_wire(),
            item.message.urgency.as_wire(),
            item.message.sender_name,
            item.message.id,
            item.message.subject,
            item.message.body
        ));
    }
    let combined = formatted.join("\n\n---\n\n");

    let out = match event_name {
        "Stop" => json!({
            "decision": "block",
            "reason": format!("Incoming FerroWire message(s) for '{persona}':\n\n{combined}"),
        }),
        "UserPromptSubmit" | "PostToolUse" | "SessionStart" => json!({
            "hookSpecificOutput": {
                "hookEventName": event_name,
                "additionalContext": format!("Incoming FerroWire message(s) for '{persona}':\n\n{combined}"),
            }
        }),
        _ => json!({}),
    };
    Ok(out)
}

pub fn run_agent_msg(env: &StateEnv<'_>, cli: AgentMsgCli) -> Result<i32, (i32, String)> {
    let from = cli.from;
    match cli.cmd {
        AgentMsgCmd::Send {
            recipient,
            subject,
            body,
        } => run(
            env,
            WireCmd::Send {
                to: recipient,
                subject,
                body,
                kind: "mail".into(),
                urgent: false,
                durable: false,
                thread: None,
                from,
                json: false,
            },
        ),
        AgentMsgCmd::Broadcast { subject, body } => run(
            env,
            WireCmd::Broadcast {
                subject,
                body,
                urgent: false,
                durable: false,
                from,
                json: false,
            },
        ),
        AgentMsgCmd::Read {
            unread_only,
            all,
            json,
            ack,
        } => run(
            env,
            WireCmd::Read {
                ack,
                all: all && !unread_only,
                thread: None,
                for_agent: from,
                json,
            },
        ),
        AgentMsgCmd::Ack { message_id } => run(
            env,
            WireCmd::Ack {
                message_ids: vec![message_id],
                for_agent: from,
                json: false,
            },
        ),
        AgentMsgCmd::Wait {
            timeout,
            ack,
            block,
        } => run(
            env,
            WireCmd::Wait {
                ack,
                block,
                timeout,
                for_agent: from,
                json: false,
            },
        ),
        AgentMsgCmd::Listen => run_listen_loop(env, from.as_deref()),
        AgentMsgCmd::Register {
            name,
            project,
            role,
            extra_areas,
            replace,
        } => {
            let parsed_role = parse_role_selector(&role).map_err(|e| (1, e.to_string()))?;
            let extras: Vec<String> = extra_areas
                .as_deref()
                .map(|csv| {
                    csv.split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(ToOwned::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            let by = name.trim().to_ascii_lowercase();
            let store = env
                .open_for_scope(Some(&StateScope::Workspace))
                .map_err(|e| (1, e))?;
            let existing = if replace {
                store.agent_by_name(&by).map_err(|e| (1, e.to_string()))?
            } else {
                None
            };
            let (parsed_role, extras) = preserve_on_replace(
                existing.as_ref(),
                replace,
                parsed_role,
                extras,
                extra_areas.is_some(),
            );
            match store.register_agent(RegisterAgentArgs {
                display_name: name,
                email: None,
                project,
                role: parsed_role,
                extra_areas: extras,
                create_area: true,
                replace,
                at: now_at(),
                by,
            }) {
                Ok(rec) => {
                    sync_store_retired_personas(&store);
                    if let Some(route) = capture_current_env_route() {
                        record_caller_route_with_provenance(env, &rec.name_key, route, true)
                            .map_err(|error| {
                                (
                                    1,
                                    format!("agent registered but route update failed: {error}"),
                                )
                            })?;
                    }
                    let all_active = store.list_agents(None, false).unwrap_or_default();
                    let mgr = reports_to(&rec, &all_active);
                    println!(
                        "Registered {} ({}) as {} in {} (reports to {})",
                        rec.display_name,
                        rec.id,
                        rec.role.as_wire(),
                        rec.project,
                        mgr
                    );
                    Ok(0)
                }
                Err(e @ (StoreError::AgentCollision { .. } | StoreError::LeadTaken { .. })) => {
                    Err((2, e.to_string()))
                }
                Err(e) => Err((1, e.to_string())),
            }
        }
        AgentMsgCmd::Unregister { name } => {
            let store = env
                .open_for_scope(Some(&StateScope::Workspace))
                .map_err(|e| (1, e))?;
            let target = resolve_agent(&store, &name).map_err(|e| (1, e))?;
            let by = target.name_key.clone();
            let retired = store
                .retire_agent(&target.id, now_at(), &by)
                .map_err(|e| (1, e.to_string()))?;
            sync_store_retired_personas(&store);
            remove_caller_route(env, &by)
                .map_err(|e| (1, format!("agent retired but route removal failed: {e}")))?;
            prune_muse_roster(&by).map_err(|e| {
                (
                    1,
                    format!("agent retired but Muse roster removal failed: {e}"),
                )
            })?;
            println!("Retired {} ({})", retired.display_name, retired.id);
            Ok(0)
        }
        AgentMsgCmd::Hydrate { args } => {
            let status = std::process::Command::new(hydrate_program())
                .args(&args)
                .status()
                .map_err(|e| (1, format!("failed to run agent-hydrate: {e}")))?;
            Ok(status.code().unwrap_or(1))
        }
        AgentMsgCmd::Roster { json } => {
            let store = env
                .open_for_scope(Some(&StateScope::Workspace))
                .map_err(|e| (1, e))?;
            crate::cmd::agent::run(
                &store,
                crate::cmd::agent::AgentCmd::Roster {
                    project: None,
                    include_retired: false,
                    render: None,
                    json,
                },
            )
            .map(|()| 0)
        }
    }
}

fn run_listen_loop(env: &StateEnv<'_>, explicit_from: Option<&str>) -> Result<i32, (i32, String)> {
    let persona = resolve_caller(env, explicit_from, None);
    let sock = resolve_socket_path(env, None);
    if let Ok(mut sub) = LocalClient::connect(&sock, DEFAULT_LOCAL_RPC_TIMEOUT)
        && sub.subscribe(Some(persona.clone()), None).is_ok()
    {
        loop {
            if let Some(LocalEvent::Message { message }) = sub
                .wait_for_event(Duration::from_secs(60))
                .map_err(|e| (1, e))?
            {
                println!(
                    "[{}] {} -> {}: {}\n{}",
                    message.id, message.sender_name, persona, message.subject, message.body
                );
            }
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    loop {
        let items = execute_read(
            env,
            WireFilter {
                recipient: Some(persona.clone()),
                unread_only: true,
                thread_id: None,
                correlation_id: None,
                limit: None,
            },
            false,
        )
        .map_err(|e| (1, e))?;
        for item in items {
            if seen.insert(item.message.id.clone()) {
                print_inbox_items(&[item], false);
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn print_sent(msg: &WireMessage, receipts: &[DeliveryReceipt], json: bool) {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "message": msg,
                "receipts": receipts,
            }))
            .unwrap()
        );
    } else {
        let rcpt_list = receipts
            .iter()
            .map(|r| format!("{}:{}", r.recipient_name, r.status.as_wire()))
            .collect::<Vec<_>>()
            .join(",");
        println!(
            "sent\t{}\tthread={}\tto={}\treceipts=[{}]",
            msg.id, msg.thread_id, msg.to, rcpt_list
        );
    }
}

fn print_inbox_items(items: &[WireInboxItem], json: bool) {
    if json {
        let legacy_items: Vec<Value> = items.iter().map(inbox_item_to_json).collect();
        println!("{}", serde_json::to_string_pretty(&legacy_items).unwrap());
    } else {
        for item in items {
            println!(
                "[{}] {} -> {} ({}/{} status={}): {}\n{}",
                item.message.id,
                item.message.sender_name,
                item.receipt.recipient_name,
                item.message.kind.as_wire(),
                item.message.urgency.as_wire(),
                item.receipt.status.as_wire(),
                item.message.subject,
                item.message.body
            );
        }
    }
}

pub fn inbox_item_to_json(item: &WireInboxItem) -> Value {
    json!({
        "id": item.message.id,
        "thread_id": item.message.thread_id,
        "reply_to": item.message.reply_to,
        "correlation_id": item.message.correlation_id,
        "sender": item.message.sender_name,
        "to": item.message.to,
        "recipient": item.receipt.recipient_name,
        "kind": item.message.kind,
        "urgency": item.message.urgency,
        "durable": item.message.durable,
        "timestamp": item.message.created_at,
        "subject": item.message.subject,
        "body": item.message.body,
        "status": item.receipt.status,
    })
}

fn dispatch_diagnostics_summary(
    diagnostics: Option<&fl_wire::dispatch_diagnostics::DispatchDiagnostics>,
) -> String {
    let Some(diagnostics) = diagnostics else {
        return "dispatch_diagnostics=unavailable".into();
    };
    let mut reasons = std::collections::BTreeMap::<String, usize>::new();
    for row in &diagnostics.records {
        // Derive labels from the wire enum rather than duplicate its vocabulary.
        let label = serde_json::to_value(row.decisive_reason).unwrap();
        *reasons
            .entry(label.as_str().unwrap().to_owned())
            .or_default() += 1;
    }
    let reasons = reasons
        .into_iter()
        .map(|(reason, count)| format!("{reason}={count}"))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "dispatch_diagnostics=available\tholds={}\tglobal_capacity_exhausted={}\tomitted_evidence={}\tdiagnostics_partial={}\tomitted_evidence_saturated={}\tdecisive_reasons={}",
        diagnostics.records.len(),
        diagnostics.global_capacity_exhausted,
        diagnostics.omitted_evidence_total,
        diagnostics.diagnostics_partial,
        diagnostics.omitted_evidence_total_saturated,
        reasons,
    )
}

#[cfg(test)]
pub(crate) mod tests {
    fn test_send_args() -> SendWireArgs {
        SendWireArgs {
            id: None,
            sender_id: None,
            sender_name: "ada".into(),
            sender_host: None,
            to: WireAddress::AgentName("bob".into()),
            kind: MessageKind::Mail,
            urgency: MessageUrgency::Normal,
            durable: true,
            subject: "hello".into(),
            body: "test".into(),
            reply_to: None,
            thread_id: None,
            deadline_at: None,
            at: now_at(),
        }
    }

    #[test]
    fn connected_send_without_a_response_reports_the_transmitted_id_and_does_not_open_the_store() {
        use std::io::{BufRead, BufReader};
        use std::os::unix::net::UnixListener;

        let rig = HookRig::new(Some("ada"));
        let listener = UnixListener::bind(rig.dir.path().join("none.sock")).unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(stream).read_line(&mut line).unwrap();
            let request: LocalRequest = serde_json::from_str(&line).unwrap();
            let LocalRequest::Send { args } = request else {
                panic!("expected send")
            };
            args.id.expect("client generated message id")
        });
        let env = StateEnv {
            explicit_db: Some(&rig.db),
            default_store: &rig.db,
            cfg: &rig.cfg,
            cwd: rig.dir.path(),
        };
        let err = execute_send(&env, test_send_args()).unwrap_err();
        let sent_id = server.join().unwrap();
        assert!(err.contains(sent_id.as_str()), "{err}");
        assert!(err.contains("may already be delivered"), "{err}");
        assert!(!rig.db.exists());
        assert!(!rig.db.with_extension("fls").exists());
    }

    #[test]
    fn connected_send_timeout_preserves_caller_id_without_opening_store() {
        use std::io::{BufRead, BufReader};
        use std::os::unix::net::UnixListener;
        use std::sync::mpsc;

        let rig = HookRig::new(Some("ada"));
        let listener = UnixListener::bind(rig.dir.path().join("none.sock")).unwrap();
        let (release_tx, release_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let LocalRequest::Send { args } = serde_json::from_str(&line).unwrap() else {
                panic!("expected send");
            };
            let id = args.id.unwrap();
            release_rx.recv().unwrap();
            id
        });
        let env = StateEnv {
            explicit_db: Some(&rig.db),
            default_store: &rig.db,
            cfg: &rig.cfg,
            cwd: rig.dir.path(),
        };
        let mut args = test_send_args();
        let id = parse_message_id("01900000-0000-7000-8000-000000000001").unwrap();
        args.id = Some(id.clone());
        let error = execute_send(&env, args).unwrap_err();
        release_tx.send(()).unwrap();
        assert_eq!(server.join().unwrap(), id);
        assert!(error.contains(id.as_str()), "{error}");
        assert!(error.contains("may already be delivered"), "{error}");
        assert!(!rig.db.exists());
        assert!(!rig.db.with_extension("fls").exists());
    }

    #[test]
    fn connected_send_with_unexpected_response_is_ambiguous_without_opening_store() {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;

        let rig = HookRig::new(Some("ada"));
        let listener = UnixListener::bind(rig.dir.path().join("none.sock")).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            let LocalRequest::Send { args } = serde_json::from_str(&line).unwrap() else {
                panic!("expected send");
            };
            stream
                .write_all(b"{\"status\":\"route_registered\"}\n")
                .unwrap();
            args.id.unwrap()
        });
        let env = StateEnv {
            explicit_db: Some(&rig.db),
            default_store: &rig.db,
            cfg: &rig.cfg,
            cwd: rig.dir.path(),
        };
        let error = execute_send(&env, test_send_args()).unwrap_err();
        let id = server.join().unwrap();
        assert!(error.contains(id.as_str()), "{error}");
        assert!(error.contains("may already be delivered"), "{error}");
        assert!(error.contains("unexpected daemon response"), "{error}");
        assert!(!rig.db.exists());
        assert!(!rig.db.with_extension("fls").exists());
    }

    #[test]
    fn missing_daemon_uses_direct_store_with_caller_generated_id() {
        let rig = HookRig::new(Some("ada"));
        let env = StateEnv {
            explicit_db: Some(&rig.db),
            default_store: &rig.db,
            cfg: &rig.cfg,
            cwd: rig.dir.path(),
        };
        let (sent, _) = execute_send(&env, test_send_args()).unwrap();
        assert!(sent.id.as_str().starts_with("urn:uuid:"));
        assert_eq!(
            env.open_for_scope(Some(&StateScope::Workspace))
                .unwrap()
                .get_wire_message(&sent.id)
                .unwrap()
                .unwrap()
                .id,
            sent.id
        );
    }

    #[test]
    fn wire_and_agent_msg_send_do_not_expose_retry_id() {
        #[derive(Parser)]
        struct WireTestCli {
            #[command(subcommand)]
            cmd: WireCmd,
        }
        let retry_id = "01900000-0000-7000-8000-000000000001";
        let wire = WireTestCli::try_parse_from([
            "fl-wire", "send", "bob", "hello", "body", "--id", retry_id,
        ]);
        assert_eq!(
            wire.err().unwrap().kind(),
            clap::error::ErrorKind::UnknownArgument
        );
        let agent = AgentMsgCli::try_parse_from([
            "agent-msg",
            "send",
            "bob",
            "hello",
            "body",
            "--id",
            retry_id,
        ]);
        assert_eq!(
            agent.err().unwrap().kind(),
            clap::error::ErrorKind::UnknownArgument
        );
        let wire =
            WireTestCli::try_parse_from(["fl-wire", "send", "bob", "hello", "body"]).unwrap();
        assert!(matches!(wire.cmd, WireCmd::Send { .. }));
        let agent =
            AgentMsgCli::try_parse_from(["agent-msg", "send", "bob", "hello", "body"]).unwrap();
        assert!(matches!(agent.cmd, AgentMsgCmd::Send { .. }));
    }
    #[test]
    fn a_rejected_daemon_registration_keeps_the_muse_roster_entry() {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;
        let rig = HookRig::new(Some("ada"));
        let path = rig.home().join(".config/muse/active-sessions.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"ada":{"session_id":"keep"}}"#).unwrap();
        let listener = UnixListener::bind(rig.dir.path().join("none.sock")).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            stream
                .write_all(b"{\"status\":\"error\",\"message\":\"registration refused\"}\n")
                .unwrap();
        });
        let env = StateEnv {
            explicit_db: Some(&rig.db),
            default_store: &rig.db,
            cfg: &rig.cfg,
            cwd: rig.dir.path(),
        };
        let result = record_caller_route_with_provenance(
            &env,
            "ada",
            VendorRoute::Codex {
                thread_id: "new".into(),
                cwd: None,
            },
            true,
        );
        server.join().unwrap();
        assert!(result.is_err());
        let roster: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(roster["ada"]["session_id"], "keep");
    }
    #[test]
    fn registering_a_non_muse_route_prunes_only_that_personas_muse_roster_entry() {
        let rig = HookRig::new(Some("ada"));
        let path = rig.home().join(".config/muse/active-sessions.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"ada":{"session_id":"old"},"bob":{"session_id":"keep"}}"#,
        )
        .unwrap();
        let env = StateEnv {
            explicit_db: Some(&rig.db),
            default_store: &rig.db,
            cfg: &rig.cfg,
            cwd: rig.dir.path(),
        };
        record_caller_route_with_provenance(
            &env,
            "ada",
            VendorRoute::Codex {
                thread_id: "new".into(),
                cwd: None,
            },
            true,
        )
        .unwrap();
        let roster: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert!(roster.get("ada").is_none());
        assert_eq!(roster["bob"]["session_id"], "keep");
        assert!(
            RouteTable::load(Some(&rig.routes_path()))
                .get("ada")
                .unwrap()
                .explicit
        );
    }

    #[test]
    fn unregister_removes_the_route_and_muse_roster_entry() {
        let rig = HookRig::new(Some("ada"));
        let env = StateEnv {
            explicit_db: Some(&rig.db),
            default_store: &rig.db,
            cfg: &rig.cfg,
            cwd: rig.dir.path(),
        };
        let store = env.open_for_scope(Some(&StateScope::Workspace)).unwrap();
        store
            .register_agent(RegisterAgentArgs {
                display_name: "Ada".into(),
                email: None,
                project: "ferroloop".into(),
                role: AgentRole::Assistant,
                extra_areas: vec![],
                create_area: true,
                replace: false,
                at: now_at(),
                by: "ada".into(),
            })
            .unwrap();
        drop(store);
        let path = rig.home().join(".config/muse/active-sessions.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"ada":{"session_id":"old"},"bob":{"session_id":"keep"}}"#,
        )
        .unwrap();
        rig.seed("ada", grok_route("old"));
        let cli = AgentMsgCli::parse_from(["agent-msg", "unregister", "Ada"]);
        assert_eq!(run_agent_msg(&env, cli).unwrap(), 0);
        assert!(
            RouteTable::load(Some(&rig.routes_path()))
                .get("ada")
                .is_none()
        );
        let roster: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert!(roster.get("ada").is_none());
        assert_eq!(roster["bob"]["session_id"], "keep");
    }

    #[test]
    fn agent_retire_evicts_the_retired_personas_route() {
        let rig = HookRig::new(Some("ada"));
        let env = StateEnv {
            explicit_db: Some(&rig.db),
            default_store: &rig.db,
            cfg: &rig.cfg,
            cwd: rig.dir.path(),
        };
        let store = env.open_for_scope(Some(&StateScope::Workspace)).unwrap();
        store
            .register_agent(RegisterAgentArgs {
                display_name: "Ada".into(),
                email: None,
                project: "ferroloop".into(),
                role: AgentRole::Assistant,
                extra_areas: vec![],
                create_area: true,
                replace: false,
                at: now_at(),
                by: "ada".into(),
            })
            .unwrap();
        rig.seed("ada", grok_route("old"));
        crate::cmd::agent::run(
            &store,
            crate::cmd::agent::AgentCmd::Retire {
                agent: "Ada".into(),
                by: "owner".into(),
                json: false,
            },
        )
        .unwrap();
        assert!(
            RouteTable::load(Some(&rig.routes_path()))
                .get("ada")
                .is_none()
        );
        assert!(
            fl_wire::identity::load_retired_personas(None).contains("ada"),
            "fl agent retire must sync retired_personas in decommission sidecar"
        );

        crate::cmd::agent::run(
            &store,
            crate::cmd::agent::AgentCmd::Register {
                name: "Ada".into(),
                project: "ferroloop".into(),
                role: "assistant".into(),
                create_area: false,
                extra_areas: vec![],
                email: None,
                replace: true,
                by: "ada".into(),
                json: false,
            },
        )
        .unwrap();
        assert!(
            !fl_wire::identity::load_retired_personas(None).contains("ada"),
            "fl agent register --replace must clear retired_personas in decommission sidecar"
        );
    }
    #[test]
    fn retention_cutoff_saturates_instead_of_wrapping() {
        assert_eq!(super::retention_cutoff_ms(1_000, u64::MAX), 0);
        // 213503982335 days * 86_400_000 wraps u64 to a tiny value.
        assert_eq!(
            super::retention_cutoff_ms(5_000_000_000, 213_503_982_335),
            0
        );
        assert_eq!(
            super::retention_cutoff_ms(200_000_000, 1),
            200_000_000 - 86_400_000
        );
    }

    use super::*;
    use crate::config::{Config, WireConfig};
    use fl_core::agent::{AgentRole, RegisterAgentArgs};

    static HOOK_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Serialises the tests that read or write process-global environment variables.
    pub(crate) fn hook_env_lock() -> std::sync::MutexGuard<'static, ()> {
        HOOK_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    const HOOK_ENV_VARS: [&str; 16] = [
        "HOME",
        "XDG_RUNTIME_DIR",
        "AGENT_PERSONA",
        "AGENT_NAME",
        "FL_WIRE_ROUTES",
        "FL_WIRE_SOCKET",
        "FL_WIRE_DISABLE_HOST_DISCOVERY",
        "CLAUDE_CODE_MESSAGING_SOCKET",
        "CLAUDE_SESSION_ID",
        "ANTIGRAVITY_CONVERSATION_ID",
        "CODEX_THREAD_ID",
        "MUSE_SESSION_ID",
        "GROK_SESSION_ID",
        "GROK_HOOK_EVENT",
        "TMUX_PANE",
        "FL_STORE_BACKEND",
    ];

    /// Sandbox for one hook run: a temp HOME, route table and store, and a cleared vendor
    /// environment. Holds the env lock and restores the old environment on drop.
    struct HookRig {
        _lock: std::sync::MutexGuard<'static, ()>,
        saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
        dir: tempfile::TempDir,
        db: PathBuf,
        cfg: Config,
    }

    impl HookRig {
        fn new(agent_persona: Option<&str>) -> Self {
            let lock = hook_env_lock();
            let dir = tempfile::tempdir().unwrap();
            let saved = HOOK_ENV_VARS
                .iter()
                .map(|k| (*k, std::env::var_os(k)))
                .collect();
            // SAFETY: the env lock serialises every test that touches these variables.
            unsafe {
                // HOME is read by unrelated tests that do not take the lock: it is switched
                // straight to the new value and never unset, even for an instant.
                for k in HOOK_ENV_VARS {
                    if k != "HOME" {
                        std::env::remove_var(k);
                    }
                }
                std::env::set_var("HOME", dir.path().join("home"));
                std::env::set_var("FL_WIRE_ROUTES", dir.path().join("routes.json"));
                std::env::set_var("FL_WIRE_SOCKET", dir.path().join("none.sock"));
                std::env::set_var("FL_WIRE_DISABLE_HOST_DISCOVERY", "1");
                if let Some(p) = agent_persona {
                    std::env::set_var("AGENT_PERSONA", p);
                }
            }
            std::fs::create_dir_all(dir.path().join("home")).unwrap();
            let db = dir.path().join("fl.redb");
            Self {
                _lock: lock,
                saved,
                dir,
                db,
                cfg: Config::default(),
            }
        }

        fn home(&self) -> PathBuf {
            self.dir.path().join("home")
        }

        fn routes_path(&self) -> PathBuf {
            self.dir.path().join("routes.json")
        }

        fn seed(&self, persona: &str, route: VendorRoute) {
            let path = self.routes_path();
            let mut table = RouteTable::load(Some(&path));
            table.upsert(persona, route, now_at());
            table.save(Some(&path)).unwrap();
        }

        fn route_of(&self, persona: &str) -> Option<VendorRoute> {
            RouteTable::load(Some(&self.routes_path()))
                .get(persona)
                .map(|e| e.route.clone())
        }

        fn run(&self, vendor: &str, payload: Value) -> Value {
            let env = StateEnv {
                explicit_db: Some(&self.db),
                default_store: &self.db,
                cfg: &self.cfg,
                cwd: self.dir.path(),
            };
            vendor_hook_response(&env, Some(vendor), &payload.to_string()).unwrap()
        }

        /// A Grok `summary.json` for `sid` under the rig's HOME.
        fn grok_summary(&self, sid: &str, extra: Value) {
            let dir = self.home().join(".grok/sessions/proj").join(sid);
            std::fs::create_dir_all(&dir).unwrap();
            let mut val = json!({
                "info": {"id": sid, "cwd": "/tmp"},
                "session_summary": "Jarvis registers as lead",
            });
            if let (Some(base), Some(add)) = (val.as_object_mut(), extra.as_object()) {
                base.extend(add.clone());
            }
            std::fs::write(dir.join("summary.json"), val.to_string()).unwrap();
        }

        /// Make this process look like the live Grok session `sid` to the process-tree walk.
        fn grok_active(&self, sid: &str) {
            std::fs::create_dir_all(self.home().join(".grok")).unwrap();
            std::fs::write(
                self.home().join(".grok/active_sessions.json"),
                json!([{"session_id": sid, "pid": std::process::id(), "cwd": "/tmp"}]).to_string(),
            )
            .unwrap();
        }
    }

    impl Drop for HookRig {
        fn drop(&mut self) {
            // SAFETY: still holding the env lock.
            unsafe {
                for (k, v) in &self.saved {
                    match v {
                        Some(v) => std::env::set_var(k, v),
                        None => std::env::remove_var(k),
                    }
                }
            }
        }
    }

    #[test]
    fn hook_rig_never_leaves_home_unset() {
        if std::env::var_os("HOME").is_none() {
            return;
        }
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let seen_unset = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let poller = {
            let (stop, seen_unset) = (stop.clone(), seen_unset.clone());
            std::thread::spawn(move || {
                use std::sync::atomic::Ordering::Relaxed;
                while !stop.load(Relaxed) {
                    if std::env::var_os("HOME").is_none() {
                        seen_unset.store(true, Relaxed);
                    }
                }
            })
        };
        for _ in 0..300 {
            drop(HookRig::new(Some("jarvis")));
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        poller.join().unwrap();
        assert!(
            !seen_unset.load(std::sync::atomic::Ordering::Relaxed),
            "HOME was observed unset while a HookRig was created or dropped"
        );
    }

    fn agy_route(conversation_id: &str) -> VendorRoute {
        VendorRoute::Agy {
            conversation_id: conversation_id.into(),
            ls_pid: None,
            ls_address: Some("127.0.0.1:1".into()),
            csrf_token: None,
            agentapi_exe: None,
        }
    }

    fn grok_route(session_id: &str) -> VendorRoute {
        VendorRoute::Grok {
            session_id: session_id.into(),
            cwd: Some(PathBuf::from("/old")),
            tmux_target: None,
            tmux_pane: None,
        }
    }

    fn grok_hook_payload(sid: &str) -> Value {
        json!({"hookEventName": "SessionStart", "session_id": sid, "cwd": "/tmp"})
    }

    #[test]
    fn headless_grok_hook_leaves_the_route_table_alone() {
        // Even with a named persona, a `grok -p` one-shot registers nothing.
        let rig = HookRig::new(Some("jarvis"));
        rig.seed("jarvis", agy_route("conv-1"));
        rig.grok_summary("sid-h", json!({"session_kind": "headless"}));
        let out = rig.run("grok", grok_hook_payload("sid-h"));
        assert_eq!(out, json!({}));
        assert_eq!(rig.route_of("jarvis"), Some(agy_route("conv-1")));
    }

    #[test]
    fn headless_check_follows_the_transcript_directory() {
        let rig = HookRig::new(Some("jarvis"));
        rig.seed("jarvis", agy_route("conv-1"));
        rig.grok_summary("sid-t", json!({"session_kind": "headless"}));
        let transcript = rig
            .home()
            .join(".grok/sessions/proj/sid-t/chat_history.jsonl");
        let mut payload = grok_hook_payload("some-other-id");
        payload["transcript_path"] = json!(transcript);
        rig.run("grok", payload);
        assert_eq!(rig.route_of("jarvis"), Some(agy_route("conv-1")));
    }

    #[test]
    fn inferred_persona_does_not_replace_a_route_of_another_vendor() {
        // No AGENT_PERSONA and no summary yet (the headless case at `session_start`): the
        // persona comes from the process-tree walk into the parent's Grok session.
        let rig = HookRig::new(None);
        rig.seed("jarvis", agy_route("conv-1"));
        rig.grok_summary("sid-p", json!({}));
        rig.grok_active("sid-p");
        let out = rig.run("grok", grok_hook_payload("sid-child"));
        assert_eq!(out, json!({}));
        assert_eq!(rig.route_of("jarvis"), Some(agy_route("conv-1")));
    }

    #[test]
    fn inherited_session_id_persona_does_not_replace_another_vendor_route() {
        let rig = HookRig::new(None);
        rig.seed("jarvis", agy_route("conv-1"));
        // SAFETY: the rig holds the env lock; the rig restores the variable on drop.
        unsafe { std::env::set_var("ANTIGRAVITY_CONVERSATION_ID", "conv-1") };
        rig.run("grok", grok_hook_payload("sid-child"));
        // The env capture may refresh the Agy route's fields; it stays an Agy route.
        assert!(matches!(
            rig.route_of("jarvis"),
            Some(VendorRoute::Agy { conversation_id, .. }) if conversation_id == "conv-1"
        ));
    }

    #[test]
    fn muse_hook_registers_no_guessed_tmux_target() {
        let rig = HookRig::new(Some("kacey"));
        rig.run(
            "muse",
            json!({"hook_event_name": "SessionStart", "session_id": "muse-sid", "cwd": "/tmp"}),
        );
        match rig.route_of("kacey") {
            Some(VendorRoute::Muse {
                session_id,
                tmux_target,
                ..
            }) => {
                assert_eq!(session_id, "muse-sid");
                assert_eq!(tmux_target, None, "the hook must not guess a tmux target");
            }
            other => panic!("expected a Muse route, got {other:?}"),
        }
    }

    #[test]
    fn explicit_persona_may_switch_vendor() {
        let rig = HookRig::new(Some("jarvis"));
        rig.seed("jarvis", agy_route("conv-1"));
        rig.run("grok", grok_hook_payload("sid-new"));
        assert!(matches!(
            rig.route_of("jarvis"),
            Some(VendorRoute::Grok { session_id, .. }) if session_id == "sid-new"
        ));
    }

    #[test]
    fn transcript_persona_may_switch_vendor() {
        let rig = HookRig::new(None);
        rig.seed("jarvis", agy_route("conv-1"));
        let transcript = rig.dir.path().join("t.jsonl");
        std::fs::write(
            &transcript,
            "{\"type\":\"user\",\"content\":\"agent-msg register Jarvis\"}\n",
        )
        .unwrap();
        let mut payload = grok_hook_payload("sid-new");
        payload["transcript_path"] = json!(transcript);
        rig.run("grok", payload);
        assert!(matches!(
            rig.route_of("jarvis"),
            Some(VendorRoute::Grok { session_id, .. }) if session_id == "sid-new"
        ));
    }

    #[test]
    fn codex_hook_prefers_the_transcript_persona_over_the_daemon_environment() {
        let rig = HookRig::new(Some("fiona"));
        let transcript = rig.dir.path().join("rollout.jsonl");
        std::fs::write(
            &transcript,
            "{\"type\":\"user\",\"content\":\"agent-msg register Freya\"}\n",
        )
        .unwrap();
        rig.run(
            "codex",
            json!({
                "hook_event_name": "Stop",
                "session_id": "thread-freya",
                "cwd": "/tmp/freya",
                "transcript_path": transcript,
            }),
        );
        assert!(
            matches!(
                rig.route_of("freya"),
                Some(VendorRoute::Codex { thread_id, .. }) if thread_id == "thread-freya"
            ),
            "{:?}",
            rig.route_of("freya")
        );
        assert!(rig.route_of("fiona").is_none());
    }

    #[test]
    fn codex_hook_without_transcript_never_falls_back_to_daemon_agent_persona_env() {
        let rig = HookRig::new(Some("fiona"));
        let out = rig.run(
            "codex",
            json!({
                "hook_event_name": "SessionStart",
                "session_id": "thread-unclaimed",
                "cwd": "/tmp/ferris",
            }),
        );
        assert_eq!(out, json!({}));
        assert!(
            rig.route_of("fiona").is_none(),
            "codex hook must not register fiona from shared daemon AGENT_PERSONA"
        );

        // Once a Codex route is registered for the thread_id, a hook without transcript resolves it.
        rig.seed(
            "ferris",
            VendorRoute::Codex {
                thread_id: "thread-ferris".into(),
                cwd: None,
            },
        );
        rig.run(
            "codex",
            json!({
                "hook_event_name": "Stop",
                "session_id": "thread-ferris",
                "cwd": "/tmp/ferris",
            }),
        );
        assert!(rig.route_of("fiona").is_none());
        assert!(matches!(
            rig.route_of("ferris"),
            Some(VendorRoute::Codex { thread_id, cwd })
                if thread_id == "thread-ferris" && cwd == Some(PathBuf::from("/tmp/ferris"))
        ));
    }

    #[test]
    fn codex_transcript_claims_a_thread_held_by_an_explicit_alias() {
        let rig = HookRig::new(Some("fiona"));
        let path = rig.routes_path();
        let mut table = RouteTable::load(Some(&path));
        table.upsert_explicit(
            "fiona",
            VendorRoute::Codex {
                thread_id: "thread-freya".into(),
                cwd: None,
            },
            now_at(),
        );
        table.save(Some(&path)).unwrap();
        let transcript = rig.dir.path().join(".codex-rollout.jsonl");
        std::fs::write(
            &transcript,
            "{\"type\":\"user\",\"content\":\"agent-msg register Freya\"}\n",
        )
        .unwrap();
        rig.run(
            "codex",
            json!({
                "hook_event_name": "Stop",
                "session_id": "thread-freya",
                "transcript_path": transcript,
            }),
        );
        assert!(
            rig.route_of("fiona").is_none(),
            "{:?}",
            rig.route_of("fiona")
        );
        assert!(matches!(
            rig.route_of("freya"),
            Some(VendorRoute::Codex { thread_id, .. }) if thread_id == "thread-freya"
        ));
        assert!(RouteTable::load(Some(&path)).get("freya").unwrap().explicit);
    }

    #[test]
    fn inferred_persona_still_refreshes_a_route_of_the_same_vendor() {
        let rig = HookRig::new(None);
        rig.seed("jarvis", grok_route("sid-old"));
        rig.grok_summary("sid-p", json!({}));
        rig.grok_active("sid-p");
        rig.run("grok", grok_hook_payload("sid-new"));
        assert!(matches!(
            rig.route_of("jarvis"),
            Some(VendorRoute::Grok { session_id, .. }) if session_id == "sid-new"
        ));
    }

    #[test]
    fn inferred_persona_with_no_route_yet_registers() {
        let rig = HookRig::new(None);
        rig.grok_summary("sid-p", json!({}));
        rig.grok_active("sid-p");
        rig.run("grok", grok_hook_payload("sid-new"));
        assert!(matches!(
            rig.route_of("jarvis"),
            Some(VendorRoute::Grok { session_id, .. }) if session_id == "sid-new"
        ));
    }

    #[test]
    fn dead_route_is_replaced_by_an_inferred_persona() {
        let claude = |pid: u32| VendorRoute::Claude {
            pid,
            session_id: "s".into(),
            socket_path: PathBuf::from("/tmp/x.sock"),
            key_path: None,
        };
        let new = grok_route("sid-new");
        let src = PersonaSource::Inferred;
        // Dead Claude pid: replaced.
        let dead = |_: u32| false;
        assert_eq!(
            route_replacement_verdict("jarvis", Some(&claude(4242)), &new, src, &dead),
            Ok(())
        );
        // Live pid (this test process): blocked, with the diagnostic.
        let me = std::process::id();
        let err =
            route_replacement_verdict("jarvis", Some(&claude(me)), &new, src, &process_exists)
                .unwrap_err();
        assert_eq!(
            err,
            "fl wire: not replacing jarvis's Claude route from an inferred Grok session; \
             set AGENT_PERSONA=jarvis or run `agent-msg register` to switch"
        );
        // Agy: a dead ls_pid is replaced, a live one is not.
        let agy_with = |ls_pid| VendorRoute::Agy {
            conversation_id: "c".into(),
            ls_pid,
            ls_address: None,
            csrf_token: None,
            agentapi_exe: None,
        };
        assert!(
            route_replacement_verdict("j", Some(&agy_with(Some(4242))), &new, src, &dead).is_ok()
        );
        assert!(
            route_replacement_verdict("j", Some(&agy_with(Some(me))), &new, src, &process_exists)
                .is_err()
        );
        // No liveness signal: never dead, so blocked even when no pid exists.
        assert!(route_replacement_verdict("j", Some(&agy_with(None)), &new, src, &dead).is_err());
        let codex = VendorRoute::Codex {
            thread_id: "t".into(),
            cwd: None,
        };
        assert!(route_replacement_verdict("j", Some(&codex), &new, src, &dead).is_err());
        assert!(!route_is_dead(&codex, &dead));
        // A trusted source and the same vendor never produce a diagnostic.
        assert!(
            route_replacement_verdict("j", Some(&claude(me)), &new, PersonaSource::Env, &|_| true)
                .is_ok()
        );
        assert!(
            route_replacement_verdict("j", Some(&grok_route("a")), &new, src, &|_| true).is_ok()
        );
    }

    #[test]
    fn hook_replaces_a_dead_claude_route_from_an_inferred_grok_persona() {
        let rig = HookRig::new(None);
        rig.seed(
            "jarvis",
            VendorRoute::Claude {
                // Far above any pid_max, so /proc/<pid> never exists.
                pid: u32::MAX - 1,
                session_id: "s".into(),
                socket_path: PathBuf::from("/tmp/x.sock"),
                key_path: None,
            },
        );
        rig.grok_summary("sid-p", json!({}));
        rig.grok_active("sid-p");
        rig.run("grok", grok_hook_payload("sid-new"));
        assert!(matches!(
            rig.route_of("jarvis"),
            Some(VendorRoute::Grok { session_id, .. }) if session_id == "sid-new"
        ));
    }

    #[test]
    fn hook_keeps_a_live_claude_route_from_an_inferred_grok_persona() {
        let rig = HookRig::new(None);
        let live = VendorRoute::Claude {
            pid: std::process::id(),
            session_id: "s".into(),
            socket_path: PathBuf::from("/tmp/x.sock"),
            key_path: None,
        };
        rig.seed("jarvis", live.clone());
        rig.grok_summary("sid-p", json!({}));
        rig.grok_active("sid-p");
        let out = rig.run("grok", grok_hook_payload("sid-new"));
        assert_eq!(out, json!({}));
        assert_eq!(rig.route_of("jarvis"), Some(live));
    }

    #[test]
    fn route_replacement_decision_table() {
        let agy = agy_route("c");
        let grok = grok_route("s");
        let codex = VendorRoute::Codex {
            thread_id: "t".into(),
            cwd: None,
        };
        let muse = VendorRoute::Muse {
            session_id: "m".into(),
            cwd: None,
            tmux_target: None,
            tmux_pane: None,
        };
        for source in [
            PersonaSource::Explicit,
            PersonaSource::Env,
            PersonaSource::Transcript,
        ] {
            for new in [&grok, &codex, &muse] {
                assert!(route_replacement_allowed(Some(&agy), new, source));
            }
        }
        for new in [&grok, &codex, &muse] {
            assert!(!route_replacement_allowed(
                Some(&agy),
                new,
                PersonaSource::Inferred
            ));
        }
        assert!(route_replacement_allowed(
            Some(&grok),
            &grok_route("other"),
            PersonaSource::Inferred
        ));
        assert!(route_replacement_allowed(
            None,
            &grok,
            PersonaSource::Inferred
        ));
    }

    #[test]
    fn test_wire_cli_send_read_ack_reply_and_agent_msg_compat() {
        let rig = HookRig::new(None);
        let dir = &rig.dir;
        let db = dir.path().join("fl.redb");
        let sock = dir.path().join("wire.sock");
        let cfg = Config {
            wire: Some(WireConfig {
                socket: Some(sock),
                ..Default::default()
            }),
            ..Default::default()
        };
        let env = StateEnv {
            explicit_db: Some(&db),
            default_store: &db,
            cfg: &cfg,
            cwd: dir.path(),
        };

        // Pre-populate two agents directly in the store so we don't capture live env routes
        {
            let store = fl_store::open_redb(&db).unwrap();
            store
                .register_agent(RegisterAgentArgs {
                    display_name: "Kacey".into(),
                    email: None,
                    project: "ferroloop".into(),
                    role: AgentRole::Lead,
                    extra_areas: vec![],
                    create_area: true,
                    replace: false,
                    at: now_at(),
                    by: "kacey".into(),
                })
                .unwrap();
            store
                .register_agent(RegisterAgentArgs {
                    display_name: "Ferris".into(),
                    email: None,
                    project: "ferroloop".into(),
                    role: AgentRole::Assistant,
                    extra_areas: vec![],
                    create_area: false,
                    replace: false,
                    at: now_at(),
                    by: "ferris".into(),
                })
                .unwrap();
        }

        // Collision via AgentMsgCmd::Register exits 2
        let reg_dup = AgentMsgCli::parse_from([
            "agent-msg",
            "register",
            "Kacey",
            "--project",
            "ferroloop",
            "--role",
            "lead",
        ]);
        assert_eq!(run_agent_msg(&env, reg_dup).unwrap_err().0, 2);

        // Verify parse_message_id with and without prefix
        let raw_uuid = "01900000-0000-7000-8000-000000000001";
        let id1 = parse_message_id(raw_uuid).unwrap();
        let id2 = parse_message_id(&format!("urn:uuid:{raw_uuid}")).unwrap();
        assert_eq!(id1, id2);

        // Verify store send_wire, read, reply, ack
        let msg_id = {
            let store = env.open_for_scope(Some(&StateScope::Workspace)).unwrap();
            let (sent_msg, receipts) = store
                .send_wire(SendWireArgs {
                    id: None,
                    sender_id: None,
                    sender_name: "ferris".into(),
                    sender_host: None,
                    to: WireAddress::AgentName("kacey".into()),
                    kind: MessageKind::Mail,
                    urgency: MessageUrgency::Normal,
                    durable: true,
                    subject: "Ping".into(),
                    body: "Test message".into(),
                    reply_to: None,
                    thread_id: None,
                    deadline_at: None,
                    at: now_at(),
                })
                .unwrap();
            assert_eq!(receipts.len(), 1);
            sent_msg.id.to_string()
        };

        // Read inbox for kacey
        let items = execute_read(
            &env,
            WireFilter {
                recipient: Some("kacey".into()),
                unread_only: true,
                thread_id: None,
                correlation_id: None,
                limit: None,
            },
            false,
        )
        .unwrap();
        assert_eq!(items.len(), 1);
        let json_val = inbox_item_to_json(&items[0]);
        assert_eq!(json_val["subject"], "Ping");

        // Ack original message via execute_ack
        let parent_id = parse_message_id(&msg_id).unwrap();
        let acked = execute_ack(&env, vec![parent_id], "kacey".into()).unwrap();
        assert_eq!(acked.len(), 1);

        // Verify unread for kacey is now empty
        let unread_kacey = execute_read(
            &env,
            WireFilter {
                recipient: Some("kacey".into()),
                unread_only: true,
                thread_id: None,
                correlation_id: None,
                limit: None,
            },
            false,
        )
        .unwrap();
        assert!(unread_kacey.is_empty());
    }
}
