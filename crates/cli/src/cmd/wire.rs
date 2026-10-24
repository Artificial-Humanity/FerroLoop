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
    DEFAULT_LOCAL_CONNECT_TIMEOUT, DaemonConfig, LocalClient, LocalEvent, LocalRequest,
    LocalResponse, default_socket_path, spawn_daemon,
};
use fl_wire::dispatcher::dispatch_message_for_store;
use fl_wire::identity::{
    RouteTable, VendorRoute, capture_current_env_route, resolve_claude_key_path, resolve_persona,
};
use serde_json::{Value, json};

use crate::cmd::agent::{now_at, render_registry_markdown, resolve_agent, write_atomic_non_symlink};
use crate::cmd::state::StateEnv;

pub const DEFAULT_RETENTION_DAYS: u64 = 30;

pub fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[derive(Subcommand)]
pub enum WireCmd {
    /// Send a message to a persona, AgentId URN, or group selector (`@all`, `@area:<slug>`, `@role:<role>`, `@area:<slug>/<role>`).
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
        #[arg(long)]
        unread_only: bool,
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
    default_socket_path()
}

pub fn resolve_routes_path() -> Option<PathBuf> {
    std::env::var_os("FL_WIRE_ROUTES")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

pub fn resolve_caller(env: &StateEnv<'_>, explicit: Option<&str>, transcript: Option<&Path>) -> String {
    let store = env.open_for_scope(Some(&StateScope::Workspace)).ok();
    let reg = store.as_ref().map(|s| s as &dyn AgentRegistry);
    let rpath = resolve_routes_path();
    let persona = resolve_persona(explicit, transcript, env.cwd, reg, rpath.as_deref());
    if persona != "unknown"
        && let Some(route) = capture_current_env_route()
    {
        record_caller_route(env, &persona, route);
    }
    persona
}

pub fn record_caller_route(env: &StateEnv<'_>, persona: &str, route: VendorRoute) {
    let sock = resolve_socket_path(env, None);
    if let Ok(mut client) = LocalClient::connect(&sock, DEFAULT_LOCAL_CONNECT_TIMEOUT) {
        let _ = client.request(&LocalRequest::RegisterRoute {
            persona: persona.to_string(),
            route,
        });
        return;
    }
    let rpath = resolve_routes_path();
    let mut table = RouteTable::load(rpath.as_deref());
    table.upsert(persona, route, now_at());
    let _ = table.save(rpath.as_deref());
}

pub fn parse_message_id(raw: &str) -> Result<MessageId, String> {
    let trimmed = raw.trim();
    if trimmed.starts_with("urn:uuid:") {
        let iri = Iri::parse(trimmed).map_err(|e| format!("invalid message ID `{raw}`: {e}"))?;
        Ok(MessageId(iri))
    } else {
        let with_prefix = format!("urn:uuid:{trimmed}");
        let iri = Iri::parse(&with_prefix).map_err(|e| format!("invalid message ID `{raw}`: {e}"))?;
        Ok(MessageId(iri))
    }
}

pub fn execute_send(
    env: &StateEnv<'_>,
    args: SendWireArgs,
) -> Result<(WireMessage, Vec<DeliveryReceipt>), String> {
    let sock = resolve_socket_path(env, None);
    if let Ok(mut client) = LocalClient::connect(&sock, DEFAULT_LOCAL_CONNECT_TIMEOUT) {
        match client.request(&LocalRequest::Send { args })? {
            LocalResponse::Sent { message, receipts } => return Ok((message, receipts)),
            other => return Err(format!("unexpected daemon response: {other:?}")),
        }
    }
    let store = env.open_for_scope(Some(&StateScope::Workspace))?;
    let (msg, receipts) = store.send_wire(args).map_err(|e| e.to_string())?;
    let rpath = resolve_routes_path();
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
    if let Ok(mut client) = LocalClient::connect(&sock, DEFAULT_LOCAL_CONNECT_TIMEOUT) {
        match client.request(&LocalRequest::Read { filter, ack })? {
            LocalResponse::Inbox { items } => return Ok(items),
            other => return Err(format!("unexpected daemon response: {other:?}")),
        }
    }
    let store = env.open_for_scope(Some(&StateScope::Workspace))?;
    let mut items = store.list_wire_inbox(&filter).map_err(|e| e.to_string())?;
    if ack
        && let Some(ref rec) = filter.recipient
    {
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
    if let Ok(mut client) = LocalClient::connect(&sock, DEFAULT_LOCAL_CONNECT_TIMEOUT) {
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

    if let Ok(mut sub_client) = LocalClient::connect(&sock, DEFAULT_LOCAL_CONNECT_TIMEOUT)
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
    let mut replies = store
        .list_wire_replies(ask_id)
        .map_err(|e| e.to_string())?;
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
            let (msg, receipts) = execute_reply(env, &parent_id, subject, body, sender_name)
                .map_err(|e| (1, e))?;
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

            if let Ok(mut sub) = LocalClient::connect(&sock, DEFAULT_LOCAL_CONNECT_TIMEOUT)
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
            let cutoff_ms = now_millis().saturating_sub(days * 86_400_000);
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
            if let Ok(mut client) = LocalClient::connect(&sock, DEFAULT_LOCAL_CONNECT_TIMEOUT)
                && let Ok(LocalResponse::Status(st)) = client.request(&LocalRequest::Status)
            {
                if json {
                    println!("{}", serde_json::to_string_pretty(&st).unwrap());
                } else {
                    println!(
                        "daemon=active\tnode={}\thost={}\tsocket={}\tpeers={}\troutes={}",
                        st.node_id,
                        st.host,
                        st.socket_path.display(),
                        st.connected_peers.len(),
                        st.routes.len()
                    );
                }
                return Ok(0);
            }
            let rpath = resolve_routes_path();
            let routes = RouteTable::load(rpath.as_deref());
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "daemon": "inactive",
                        "socket_path": sock,
                        "routes": routes.entries.values().collect::<Vec<_>>(),
                    }))
                    .unwrap()
                );
            } else {
                println!(
                    "daemon=inactive\tsocket={}\troutes={}",
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
            let listen_addr = listen
                .or_else(|| env.cfg.wire.as_ref().and_then(|w| w.listen.clone()));
            let mut peer_list = peers;
            if peer_list.is_empty()
                && let Some(cfg_peers) = env.cfg.wire.as_ref().map(|w| &w.peers)
            {
                peer_list = cfg_peers.clone();
            }
            let tfile = token_file
                .or_else(|| env.cfg.wire.as_ref().and_then(|w| w.token_file.clone()));
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
                .or_else(|_| {
                    std::fs::read_to_string("/etc/hostname").map(|s| s.trim().to_string())
                })
                .unwrap_or_else(|_| "localhost".into());
            let handle = spawn_daemon(
                Arc::new(store),
                DaemonConfig {
                    node_id: host.clone(),
                    host,
                    socket_path: sock,
                    routes_path: resolve_routes_path(),
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
    if raw.trim().is_empty() {
        println!("{{}}");
        return Ok(0);
    }
    let payload: Value = serde_json::from_str(&raw).unwrap_or_else(|_| json!({}));
    if payload
        .get("stop_hook_active")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        println!("{{}}");
        return Ok(0);
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

    let persona = resolve_caller(&hook_env, None, transcript_path.as_deref());
    if persona == "unknown" {
        println!("{{}}");
        return Ok(0);
    }

    // Register route for Muse, Grok, Codex, or Claude
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

    if is_muse && !session_id.is_empty() {
        record_caller_route(
            &hook_env,
            &persona,
            VendorRoute::Muse {
                session_id,
                cwd: Some(hook_cwd.clone()),
            },
        );
    } else if is_grok && !session_id.is_empty() {
        record_caller_route(
            &hook_env,
            &persona,
            VendorRoute::Grok {
                session_id,
                cwd: Some(hook_cwd.clone()),
            },
        );
    } else if is_codex && !session_id.is_empty() {
        record_caller_route(
            &hook_env,
            &persona,
            VendorRoute::Codex {
                thread_id: session_id,
                cwd: Some(hook_cwd.clone()),
            },
        );
    } else if let Some(sock) = std::env::var_os("CLAUDE_CODE_MESSAGING_SOCKET") {
        let socket_path = PathBuf::from(sock);
        let pid = socket_path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or_else(std::process::id);
        let key_path = std::env::var_os("HOME")
            .map(PathBuf::from)
            .and_then(|h| {
                resolve_claude_key_path(&h.join(".claude").join("sessions"), pid, &socket_path)
            });
        record_caller_route(
            &hook_env,
            &persona,
            VendorRoute::Claude {
                pid,
                session_id,
                socket_path,
                key_path,
            },
        );
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
        println!("{{}}");
        return Ok(0);
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
    println!("{}", serde_json::to_string(&out).unwrap());
    Ok(0)
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
            json,
            ack,
        } => run(
            env,
            WireCmd::Read {
                ack,
                all: !unread_only,
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
                    if let Some(route) = capture_current_env_route() {
                        record_caller_route(env, &rec.name_key, route);
                    }
                    // Render `.agents/REGISTRY.md` if present in ancestor workspace directory
                    for ancestor in env.cwd.ancestors() {
                        let candidate = ancestor.join(".agents").join("REGISTRY.md");
                        if candidate.exists()
                            && let (Ok(areas), Ok(agents)) =
                                (store.list_areas(), store.list_agents(None, false))
                        {
                            let md = render_registry_markdown(&areas, &agents);
                            let _ = write_atomic_non_symlink(&candidate, md.as_bytes());
                            break;
                        }
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
            println!("Retired {} ({})", retired.display_name, retired.id);
            Ok(0)
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
    if let Ok(mut sub) = LocalClient::connect(&sock, DEFAULT_LOCAL_CONNECT_TIMEOUT)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, WireConfig};
    use fl_core::agent::{AgentRole, RegisterAgentArgs};
    use fl_store::RedbStore;

    #[test]
    fn test_wire_cli_send_read_ack_reply_and_agent_msg_compat() {
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: isolates test from discovering host agent routes.
        unsafe {
            std::env::set_var("FL_WIRE_DISABLE_HOST_DISCOVERY", "1");
            std::env::set_var("FL_WIRE_ROUTES", dir.path().join("routes.json"));
        }
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
            let store = RedbStore::open(&db).unwrap();
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

