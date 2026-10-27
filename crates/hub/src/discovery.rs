//! Host session & transcript discovery, git worktree & diff inspection, and Cargo
//! workspace architecture conformance scanning (spec §1.3, §3.2, §3.3).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use fl_core::arch::{
    ArchConformanceReport, ArchitectureModel, PackageDep, SourceReference,
    check_architecture_conformance,
};
use fl_core::store::{AgentRegistry, Catalog, Ledger};
use fl_exec::stamp;
use fl_store::RedbStore;
use fl_wire::identity::{RouteTable, VendorRoute};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use walkdir::WalkDir;

/// Summary of an active or historical agent session on the host or in the store (spec §3.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredSessionSummary {
    pub id: String,
    pub source: String,
    pub adapter: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persona: Option<String>,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_path: Option<String>,
}

/// Normalized tool invocation inside a [`TranscriptTurn`] (spec §3.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptToolCall {
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
}

/// Normalized conversation turn parsed from an Antigravity, Claude, Codex, Muse, Grok,
/// or store `Attempt` transcript (spec §3.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptTurn {
    pub index: usize,
    pub role: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<TranscriptToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
}

/// Summary of a Git worktree discovered via `git worktree list --porcelain` (spec §3.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeSummary {
    pub id: String,
    pub path: String,
    pub head: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub dirty_files: Vec<String>,
    pub is_clean: bool,
}

/// File status line inside a worktree (`git status --porcelain`, spec §3.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeFileStatus {
    pub status: String,
    pub path: String,
}

/// Detailed status and `git diff HEAD` output for a specific worktree (spec §3.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeDiff {
    pub id: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub head: String,
    pub files: Vec<WorktreeFileStatus>,
    pub diff: String,
}

/// Extracted Cargo package dependencies, source references, and repository files for
/// [`check_architecture_conformance`] (spec §1.3).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceArchScan {
    pub package_deps: Vec<PackageDep>,
    pub source_refs: Vec<SourceReference>,
    pub repo_files: Vec<String>,
}

/// Discover active sessions from `RouteTable` and recorded `Attempt` runs in `store` (spec §3.2).
pub fn discover_sessions(store: &RedbStore) -> Vec<DiscoveredSessionSummary> {
    discover_sessions_with_home(store, None)
}

/// Discover active sessions with an optional `HOME` directory override for hermetic tests.
pub fn discover_sessions_with_home(
    store: &RedbStore,
    home_override: Option<&Path>,
) -> Vec<DiscoveredSessionSummary> {
    let mut out = Vec::new();
    let mut seen_ids = BTreeSet::new();

    let route_override =
        home_override.map(|h| h.join(".cache").join("agent-bus").join("routes.json"));
    let mut routes = RouteTable::load(route_override.as_deref());
    routes.discover_routes(home_override, stamp::now());

    let home = home_override
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from));

    let registered_names: BTreeSet<String> = store
        .list_agents(None, true)
        .unwrap_or_default()
        .into_iter()
        .map(|a| a.name_key.to_ascii_lowercase())
        .collect();

    let mut ordered_routes: Vec<_> = routes.entries.iter().collect();
    ordered_routes.sort_by_key(|(persona, _)| {
        let is_registered = registered_names.contains(&persona.to_ascii_lowercase());
        (!is_registered, *persona)
    });

    for (persona, entry) in ordered_routes {
        let (adapter, raw_id, cwd, transcript_path) = match &entry.route {
            VendorRoute::Claude {
                pid, session_id, ..
            } => {
                let id = if session_id.trim().is_empty() {
                    format!("claude-{persona}-{pid}")
                } else {
                    session_id.clone()
                };
                let tpath = home
                    .as_deref()
                    .and_then(|h| find_transcript_file_in_home(h, &id));
                ("claude", id, None, tpath)
            }
            VendorRoute::Agy {
                conversation_id, ..
            } => {
                let tpath = home.as_deref().and_then(|h| {
                    let p = h
                        .join(".gemini")
                        .join("antigravity-cli")
                        .join("brain")
                        .join(conversation_id)
                        .join(".system_generated")
                        .join("logs")
                        .join("transcript.jsonl");
                    p.is_file().then(|| p.display().to_string())
                });
                ("antigravity", conversation_id.clone(), None, tpath)
            }
            VendorRoute::Codex { thread_id, cwd } => {
                let tpath = home
                    .as_deref()
                    .and_then(|h| find_transcript_file_in_home(h, thread_id));
                (
                    "codex",
                    thread_id.clone(),
                    cwd.as_ref().map(|p| p.display().to_string()),
                    tpath,
                )
            }
            VendorRoute::Muse { session_id, cwd } => {
                let tpath = home
                    .as_deref()
                    .and_then(|h| find_transcript_file_in_home(h, session_id));
                (
                    "muse",
                    session_id.clone(),
                    cwd.as_ref().map(|p| p.display().to_string()),
                    tpath,
                )
            }
            VendorRoute::Grok { session_id, cwd } => {
                let tpath = home
                    .as_deref()
                    .and_then(|h| find_transcript_file_in_home(h, session_id));
                (
                    "grok",
                    session_id.clone(),
                    cwd.as_ref().map(|p| p.display().to_string()),
                    tpath,
                )
            }
            VendorRoute::Webhook { socket_path } => (
                "webhook",
                format!("webhook-{persona}"),
                Some(socket_path.display().to_string()),
                None,
            ),
        };

        if seen_ids.insert(raw_id.clone()) {
            out.push(DiscoveredSessionSummary {
                id: raw_id,
                source: "route".to_string(),
                adapter: adapter.to_string(),
                persona: Some(persona.clone()),
                status: "active".to_string(),
                updated_at: Some(entry.updated_at.to_string()),
                cwd,
                record_id: None,
                transcript_path,
            });
        }
    }

    if let Ok(projects) = store.list_projects() {
        for project in projects {
            if let Ok(attempts) = store.attempts(&project.id) {
                for (idx, attempt) in attempts.into_iter().enumerate() {
                    let id = attempt
                        .id
                        .as_ref()
                        .map(|i| i.to_string())
                        .unwrap_or_else(|| format!("attempt-{}-{idx}", project.id));
                    if seen_ids.insert(id.clone()) {
                        out.push(DiscoveredSessionSummary {
                            id,
                            source: "attempt".to_string(),
                            adapter: attempt.adapter,
                            persona: None,
                            status: attempt.status.as_wire().to_string(),
                            updated_at: attempt.at.map(|a| a.to_string()),
                            cwd: Some(project.root.clone()),
                            record_id: Some(attempt.record.to_string()),
                            transcript_path: None,
                        });
                    }
                }
            }
        }
    }

    out
}

fn find_transcript_file_in_home(home: &Path, session_id: &str) -> Option<String> {
    let agy = home
        .join(".gemini")
        .join("antigravity-cli")
        .join("brain")
        .join(session_id)
        .join(".system_generated")
        .join("logs")
        .join("transcript.jsonl");
    if agy.is_file() {
        return Some(agy.display().to_string());
    }

    for sub in [
        home.join(".claude"),
        home.join(".codex"),
        home.join(".local").join("share").join("muse"),
        home.join(".grok"),
    ] {
        if !sub.is_dir() {
            continue;
        }
        for entry in WalkDir::new(&sub).max_depth(8).into_iter().flatten() {
            let p = entry.path();
            if !p.is_file() {
                continue;
            }
            let Some(ext) = p.extension().and_then(|s| s.to_str()) else {
                continue;
            };
            if ext != "jsonl" {
                continue;
            }
            if let Some(stem) = p.file_stem().and_then(|s| s.to_str())
                && stem == session_id
            {
                return Some(p.display().to_string());
            }
            if p.file_name().and_then(|s| s.to_str()) == Some("session.jsonl")
                && p.parent()
                    .and_then(|parent| parent.file_name())
                    .and_then(|s| s.to_str())
                    == Some(session_id)
            {
                return Some(p.display().to_string());
            }
        }
    }
    None
}

/// Read and normalize a session transcript by `session_id` (or persona name / attempt IRI) (spec §3.2).
pub fn read_session_transcript(session_id: &str, store: &RedbStore) -> Vec<TranscriptTurn> {
    read_session_transcript_with_home(session_id, store, None)
}

/// Read and normalize a session transcript with an optional `HOME` directory override.
pub fn read_session_transcript_with_home(
    session_id: &str,
    store: &RedbStore,
    home_override: Option<&Path>,
) -> Vec<TranscriptTurn> {
    let trimmed = session_id.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    // 1. Check discovered sessions for a matching id or persona with a transcript_path.
    let sessions = discover_sessions_with_home(store, home_override);
    for s in &sessions {
        if (s.id == trimmed || s.persona.as_deref() == Some(trimmed))
            && let Some(ref tpath) = s.transcript_path
            && let Ok(raw) = fs::read_to_string(tpath)
        {
            let turns = parse_jsonl_transcript(&raw);
            if !turns.is_empty() {
                return turns;
            }
        }
    }

    // 2. Search under HOME directly by session_id.
    let home = home_override
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from));
    if let Some(ref h) = home
        && let Some(tpath) = find_transcript_file_in_home(h, trimmed)
        && let Ok(raw) = fs::read_to_string(tpath)
    {
        let turns = parse_jsonl_transcript(&raw);
        if !turns.is_empty() {
            return turns;
        }
    }

    // 3. Fall back to recorded `Attempt` output in `store`.
    if let Ok(projects) = store.list_projects() {
        for project in projects {
            if let Ok(attempts) = store.attempts(&project.id) {
                for (idx, attempt) in attempts.into_iter().enumerate() {
                    let id = attempt
                        .id
                        .as_ref()
                        .map(|i| i.to_string())
                        .unwrap_or_else(|| format!("attempt-{}-{idx}", project.id));
                    if id == trimmed || attempt.record.as_str() == trimmed {
                        let content = attempt.output_excerpt.unwrap_or_default();
                        return vec![TranscriptTurn {
                            index: 0,
                            role: "assistant".to_string(),
                            content,
                            thinking: None,
                            tool_calls: Vec::new(),
                            created_at: attempt.at.map(|a| a.to_string()),
                        }];
                    }
                }
            }
        }
    }

    Vec::new()
}

/// Normalize JSONL lines from Antigravity (`transcript.jsonl`), Claude Code, Codex, Muse, or Grok
/// into a unified `Vec<TranscriptTurn>` sequence (spec §3.2).
pub fn parse_jsonl_transcript(text: &str) -> Vec<TranscriptTurn> {
    let mut turns = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(val) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        let Some(obj) = val.as_object() else {
            continue;
        };

        let created_at = obj
            .get("created_at")
            .or_else(|| obj.get("timestamp"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .or_else(|| {
                obj.get("recorded_at")
                    .and_then(Value::as_i64)
                    .and_then(format_unix_micros_iso8601)
            });

        if let Some(muse_turn) = parse_muse_event_turn(&val, created_at.clone(), turns.len()) {
            turns.push(muse_turn);
            continue;
        }

        // Determine role from Antigravity (`type` / `source`), Claude (`type` / `message.role`), or generic (`role`).
        let role = if let Some(r) = obj.get("role").and_then(Value::as_str) {
            normalize_role(r)
        } else if let Some(r) = val.pointer("/message/role").and_then(Value::as_str) {
            normalize_role(r)
        } else if let Some(t) = obj.get("type").and_then(Value::as_str) {
            match t {
                "USER_INPUT" | "user" | "human" => "user".to_string(),
                "PLANNER_RESPONSE" | "assistant" | "model" | "result" => "assistant".to_string(),
                "GENERIC" | "TOOL_RESULT" | "tool_result" | "tool" => "tool".to_string(),
                "SYSTEM" | "SYSTEM_MESSAGE" | "system" => "system".to_string(),
                _ => {
                    if let Some(src) = obj.get("source").and_then(Value::as_str) {
                        normalize_role(src)
                    } else {
                        continue;
                    }
                }
            }
        } else if let Some(src) = obj.get("source").and_then(Value::as_str) {
            normalize_role(src)
        } else {
            continue;
        };

        let mut content_parts = Vec::new();
        let mut thinking_parts = Vec::new();
        let mut tool_calls = Vec::new();

        if let Some(c) = obj.get("content") {
            extract_content_and_tools(c, &mut content_parts, &mut thinking_parts, &mut tool_calls);
        } else if let Some(msg_c) = val.pointer("/message/content") {
            extract_content_and_tools(
                msg_c,
                &mut content_parts,
                &mut thinking_parts,
                &mut tool_calls,
            );
        } else if let Some(res) = obj
            .get("result")
            .or_else(|| obj.get("text"))
            .and_then(Value::as_str)
            && !res.trim().is_empty()
        {
            content_parts.push(res.to_string());
        }

        if let Some(th) = obj
            .get("thinking")
            .or_else(|| obj.get("reasoning"))
            .and_then(Value::as_str)
            && !th.trim().is_empty()
        {
            thinking_parts.push(th.to_string());
        }

        if let Some(arr) = obj.get("tool_calls").and_then(Value::as_array) {
            for tc in arr {
                if let Some(parsed) = parse_tool_call_item(tc) {
                    tool_calls.push(parsed);
                }
            }
        }

        let content = content_parts.join("\n");
        let (mut role, content) = normalize_system_turn(&role, &content);
        let thinking = if thinking_parts.is_empty() {
            None
        } else {
            Some(thinking_parts.join("\n"))
        };

        if role == "assistant"
            && content.trim_start().starts_with("Created At:")
            && thinking.is_none()
            && tool_calls.is_empty()
        {
            role = "tool".to_string();
        }

        if content.is_empty() && thinking.is_none() && tool_calls.is_empty() {
            continue;
        }

        let index = turns.len();
        turns.push(TranscriptTurn {
            index,
            role,
            content,
            thinking,
            tool_calls,
            created_at,
        });
    }
    turns
}

/// Return `true` when the latest turn in `turns` indicates the agent session is actively working
/// (e.g. a user/wire/tool/system turn is awaiting an assistant reply, or the latest assistant turn
/// issued tool calls that have not yet produced a final text-only assistant turn).
pub fn is_transcript_busy(turns: &[TranscriptTurn]) -> bool {
    let Some(last) = turns.last() else {
        return false;
    };
    !(last.role == "assistant"
        && last.tool_calls.is_empty()
        && (!last.content.trim().is_empty() || last.thinking.is_none()))
}

fn normalize_role(raw: &str) -> String {
    match raw.trim().to_ascii_lowercase().as_str() {
        "user" | "user_explicit" | "human" => "user".to_string(),
        "assistant" | "model" | "planner_response" => "assistant".to_string(),
        "system" | "system_message" => "system".to_string(),
        other => other.to_string(),
    }
}

fn normalize_system_turn(role: &str, content: &str) -> (String, String) {
    if role != "system" && !content.contains("<SYSTEM_MESSAGE>") {
        return (role.to_string(), content.to_string());
    }

    let mut inner = content.trim();
    if content.contains("<SYSTEM_MESSAGE>") {
        let before_close = match content.rfind("</SYSTEM_MESSAGE>") {
            Some(close_idx) => &content[..close_idx],
            None => content,
        };
        if let Some(rest) = before_close.trim_start().strip_prefix("<SYSTEM_MESSAGE>") {
            inner = rest.trim();
        } else if let Some(idx) = before_close.find("\n<SYSTEM_MESSAGE>") {
            inner = before_close[idx + "\n<SYSTEM_MESSAGE>".len()..].trim();
        } else if let Some(idx) = before_close.find("<SYSTEM_MESSAGE>") {
            inner = before_close[idx + "<SYSTEM_MESSAGE>".len()..].trim();
        }
    }

    if inner.starts_with("[Message]") {
        let first_line_end = inner.find('\n').unwrap_or(inner.len());
        if let Some(pos) = inner[..first_line_end].find(" content=") {
            inner = inner[pos + " content=".len()..].trim();
        }
    }

    if inner.starts_with("[Operator Prompt via FerroDeck") {
        let cleaned = match inner.find('\n') {
            Some(nl) => inner[nl + 1..].trim(),
            None => inner,
        };
        return ("user".to_string(), cleaned.to_string());
    }

    if inner.starts_with("[Incoming NATS Message") {
        let mut sender = String::new();
        let mut subject = String::new();
        let mut body_lines = Vec::new();
        let mut in_headers = true;
        for (i, line) in inner.lines().enumerate() {
            if i == 0 && line.starts_with("[Incoming NATS Message") {
                continue;
            }
            if in_headers {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    in_headers = false;
                    continue;
                }
                if let Some((k, v)) = trimmed.split_once(':') {
                    let key_lower = k.trim().to_ascii_lowercase();
                    if matches!(
                        key_lower.as_str(),
                        "from" | "subject" | "timestamp" | "message id"
                    ) {
                        if key_lower == "from" {
                            sender = v.trim().to_string();
                        } else if key_lower == "subject" {
                            subject = v.trim().to_string();
                        }
                        continue;
                    }
                }
                in_headers = false;
            }
            body_lines.push(line);
        }
        let mut cleaned = body_lines.join("\n").trim().to_string();
        if cleaned.is_empty() && !subject.is_empty() {
            cleaned = subject;
        }
        if sender.eq_ignore_ascii_case("owner") || sender.eq_ignore_ascii_case("lmcfarlin") {
            return ("user".to_string(), cleaned);
        }
        if !sender.is_empty() {
            return (format!("wire:{}", sender.to_ascii_lowercase()), cleaned);
        }
        return ("system".to_string(), cleaned);
    }

    if inner.starts_with("[fl-wire ") {
        let first_line_end = inner.find('\n').unwrap_or(inner.len());
        let first_line = &inner[..first_line_end];
        let mut sender = String::new();
        if let Some(from_idx) = first_line.find("] from ") {
            let after_from = &first_line[from_idx + "] from ".len()..];
            let end_idx = after_from
                .find(" (")
                .or_else(|| after_from.find(':'))
                .unwrap_or(after_from.len());
            sender = after_from[..end_idx].trim().to_string();
        }
        let body = if let Some(blank_idx) = inner.find("\n\n") {
            inner[blank_idx + 2..].trim().to_string()
        } else if first_line_end < inner.len() {
            inner[first_line_end + 1..].trim().to_string()
        } else {
            first_line
                .split_once("): ")
                .map(|(_, subj)| subj.trim().to_string())
                .unwrap_or_else(|| inner.to_string())
        };
        if sender.eq_ignore_ascii_case("owner") || sender.eq_ignore_ascii_case("lmcfarlin") {
            return ("user".to_string(), body);
        }
        if !sender.is_empty() {
            return (format!("wire:{}", sender.to_ascii_lowercase()), body);
        }
        return ("system".to_string(), body);
    }

    if content.contains("<SYSTEM_MESSAGE>")
        && !inner.starts_with("[Incoming NATS Message")
        && !inner.starts_with("[fl-wire ")
        && !inner.starts_with("[Subagent ")
        && !inner.starts_with("[Task ")
    {
        return ("user".to_string(), inner.to_string());
    }

    ("system".to_string(), inner.to_string())
}

fn extract_content_and_tools(
    val: &Value,
    content_parts: &mut Vec<String>,
    thinking_parts: &mut Vec<String>,
    tool_calls: &mut Vec<TranscriptToolCall>,
) {
    match val {
        Value::String(s) => {
            if !s.is_empty() {
                content_parts.push(s.clone());
            }
        }
        Value::Array(items) => {
            for item in items {
                match item {
                    Value::String(s) if !s.is_empty() => content_parts.push(s.clone()),
                    Value::Object(map) => {
                        let item_type = map.get("type").and_then(Value::as_str).unwrap_or("");
                        match item_type {
                            "text" => {
                                if let Some(t) = map.get("text").and_then(Value::as_str)
                                    && !t.is_empty()
                                {
                                    content_parts.push(t.to_string());
                                }
                            }
                            "thinking" => {
                                if let Some(t) = map
                                    .get("thinking")
                                    .or_else(|| map.get("text"))
                                    .and_then(Value::as_str)
                                    && !t.is_empty()
                                {
                                    thinking_parts.push(t.to_string());
                                }
                            }
                            "tool_use" | "tool_call" => {
                                if let Some(tc) = parse_tool_call_item(item) {
                                    tool_calls.push(tc);
                                }
                            }
                            _ => {
                                if let Some(t) = map
                                    .get("text")
                                    .or_else(|| map.get("content"))
                                    .and_then(Value::as_str)
                                    && !t.is_empty()
                                {
                                    content_parts.push(t.to_string());
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

fn parse_tool_call_item(v: &Value) -> Option<TranscriptToolCall> {
    let obj = v.as_object()?;
    let name = obj
        .get("name")
        .or_else(|| obj.get("tool_name"))
        .or_else(|| v.pointer("/function/name"))
        .and_then(Value::as_str)?
        .to_string();
    let raw_args = obj
        .get("arguments")
        .or_else(|| obj.get("input"))
        .or_else(|| obj.get("args"))
        .or_else(|| v.pointer("/function/arguments"))
        .cloned()
        .unwrap_or(Value::Null);
    let arguments = match raw_args {
        Value::String(ref s) => serde_json::from_str::<Value>(s).unwrap_or(raw_args),
        other => other,
    };
    Some(TranscriptToolCall { name, arguments })
}

fn format_unix_micros_iso8601(micros: i64) -> Option<String> {
    if micros <= 0 {
        return None;
    }
    let millis = u64::try_from(micros / 1_000).ok()?;
    Some(fl_core::At::from_unix_millis(millis).to_string())
}

fn parse_muse_event_turn(
    val: &Value,
    created_at: Option<String>,
    index: usize,
) -> Option<TranscriptTurn> {
    let pt = val.get("payload_type").and_then(Value::as_str)?;
    match pt {
        "runtime.user_intent.accepted" => {
            let msgs = val.pointer("/payload/model_messages")?.as_array()?;
            let mut parts = Vec::new();
            for m in msgs {
                if let Some(c) = m.get("content") {
                    let mut th = Vec::new();
                    let mut tc = Vec::new();
                    extract_content_and_tools(c, &mut parts, &mut th, &mut tc);
                }
            }
            let raw = parts.join("\n");
            if raw.trim().is_empty() {
                return None;
            }
            let (role, content) = normalize_system_turn("system", &raw);
            let final_role = if role == "system" {
                "user".to_string()
            } else {
                role
            };
            Some(TranscriptTurn {
                index,
                role: final_role,
                content,
                thinking: None,
                tool_calls: Vec::new(),
                created_at,
            })
        }
        "peer_message.admission.requested" => {
            let body = val
                .pointer("/payload/body")
                .and_then(Value::as_str)?
                .trim();
            if body.is_empty() {
                return None;
            }
            let (role, content) = normalize_system_turn("system", body);
            let final_role = if role == "system" {
                "user".to_string()
            } else {
                role
            };
            Some(TranscriptTurn {
                index,
                role: final_role,
                content,
                thinking: None,
                tool_calls: Vec::new(),
                created_at,
            })
        }
        "runtime.session" => {
            if val.pointer("/payload/kind").and_then(Value::as_str) != Some("run") {
                return None;
            }
            let ev = val.pointer("/payload/event")?;
            let ev_kind = ev.get("kind").and_then(Value::as_str)?;
            match ev_kind {
                "assistant_message_committed" => {
                    let text = ev.get("text").and_then(Value::as_str)?.trim();
                    if text.is_empty()
                        || text == "[Previous turn ended without an assistant reply.]"
                    {
                        return None;
                    }
                    Some(TranscriptTurn {
                        index,
                        role: "assistant".to_string(),
                        content: text.to_string(),
                        thinking: None,
                        tool_calls: Vec::new(),
                        created_at,
                    })
                }
                "reasoning_summary_committed" => {
                    let text = ev.get("text").and_then(Value::as_str)?.trim();
                    if text.is_empty() {
                        return None;
                    }
                    Some(TranscriptTurn {
                        index,
                        role: "assistant".to_string(),
                        content: String::new(),
                        thinking: Some(text.to_string()),
                        tool_calls: Vec::new(),
                        created_at,
                    })
                }
                "assistant_tool_calls_committed" => {
                    let arr = ev.get("tool_calls").and_then(Value::as_array)?;
                    let tool_calls: Vec<TranscriptToolCall> =
                        arr.iter().filter_map(parse_tool_call_item).collect();
                    if tool_calls.is_empty() {
                        return None;
                    }
                    Some(TranscriptTurn {
                        index,
                        role: "assistant".to_string(),
                        content: String::new(),
                        thinking: None,
                        tool_calls,
                        created_at,
                    })
                }
                "tool_result_batch_committed" => {
                    let arr = ev.get("results").and_then(Value::as_array)?;
                    let mut texts = Vec::new();
                    for item in arr {
                        if let Some(t) = item.get("text").and_then(Value::as_str)
                            && !t.trim().is_empty()
                        {
                            texts.push(t.to_string());
                        }
                    }
                    if texts.is_empty() {
                        return None;
                    }
                    Some(TranscriptTurn {
                        index,
                        role: "tool".to_string(),
                        content: texts.join("\n"),
                        thinking: None,
                        tool_calls: Vec::new(),
                        created_at,
                    })
                }
                _ => None,
            }
        }
        _ => None,
    }
}

fn slugify_worktree_path(path: &Path) -> String {
    let raw = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("worktree");
    let mut slug = String::new();
    for ch in raw.chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() || lower == '-' || lower == '_' {
            slug.push(lower);
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        "worktree".to_string()
    } else {
        slug
    }
}

/// List Git worktrees for `repo_root` via `git worktree list --porcelain` and `git status --porcelain` (spec §3.3).
pub fn list_git_worktrees(repo_root: &Path) -> Result<Vec<WorktreeSummary>, String> {
    let out = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(repo_root)
        .output()
        .map_err(|e| format!("failed to run git worktree list: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git worktree list failed in {}: {}",
            repo_root.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }

    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut raw_entries: Vec<(PathBuf, String, Option<String>)> = Vec::new();
    let mut cur_path: Option<PathBuf> = None;
    let mut cur_head = String::new();
    let mut cur_branch: Option<String> = None;

    for line in stdout.lines().chain(std::iter::once("")) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if let Some(p) = cur_path.take() {
                raw_entries.push((p, std::mem::take(&mut cur_head), cur_branch.take()));
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("worktree ") {
            if let Some(p) = cur_path.take() {
                raw_entries.push((p, std::mem::take(&mut cur_head), cur_branch.take()));
            }
            cur_path = Some(PathBuf::from(rest.trim()));
        } else if let Some(rest) = trimmed.strip_prefix("HEAD ") {
            cur_head = rest.trim().to_string();
        } else if let Some(rest) = trimmed.strip_prefix("branch ") {
            let b = rest.trim();
            let short = b.strip_prefix("refs/heads/").unwrap_or(b);
            cur_branch = Some(short.to_string());
        }
    }

    let mut used_ids = BTreeSet::new();
    let mut summaries = Vec::with_capacity(raw_entries.len());

    for (wt_path, head, branch) in raw_entries {
        let base_id = slugify_worktree_path(&wt_path);
        let mut id = base_id.clone();
        let mut n = 2usize;
        while !used_ids.insert(id.clone()) {
            id = format!("{base_id}-{n}");
            n += 1;
        }

        let files = read_worktree_status_files(&wt_path);
        let dirty_files: Vec<String> = files.into_iter().map(|f| f.path).collect();
        let is_clean = dirty_files.is_empty();

        summaries.push(WorktreeSummary {
            id,
            path: wt_path.display().to_string(),
            head,
            branch,
            dirty_files,
            is_clean,
        });
    }

    Ok(summaries)
}

fn read_worktree_status_files(wt_path: &Path) -> Vec<WorktreeFileStatus> {
    if !wt_path.is_dir() {
        return Vec::new();
    }
    let Ok(out) = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(wt_path)
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut files = Vec::new();
    for line in stdout.lines() {
        if line.len() < 4 {
            continue;
        }
        let status = line[..2].trim().to_string();
        let path = line[3..].trim().to_string();
        if !path.is_empty() {
            files.push(WorktreeFileStatus {
                status: if status.is_empty() {
                    "M".to_string()
                } else {
                    status
                },
                path,
            });
        }
    }
    files
}

/// Inspect file status (`git status --porcelain`) and unified diff (`git diff HEAD`) for a single worktree (spec §3.3).
pub fn read_worktree_diff(
    repo_root: &Path,
    worktree_id: &str,
) -> Result<Option<WorktreeDiff>, String> {
    let worktrees = list_git_worktrees(repo_root)?;
    let target = worktree_id.trim();
    let Some(wt) = worktrees.into_iter().find(|w| {
        w.id == target || Path::new(&w.path).file_name().and_then(|s| s.to_str()) == Some(target)
    }) else {
        return Ok(None);
    };

    let wt_path = PathBuf::from(&wt.path);
    let files = read_worktree_status_files(&wt_path);
    let diff = if wt_path.is_dir() {
        let out = Command::new("git")
            .args(["diff", "HEAD"])
            .current_dir(&wt_path)
            .output()
            .map_err(|e| format!("failed to run git diff HEAD: {e}"))?;
        String::from_utf8_lossy(&out.stdout).to_string()
    } else {
        String::new()
    };

    Ok(Some(WorktreeDiff {
        id: wt.id,
        path: wt.path,
        branch: wt.branch,
        head: wt.head,
        files,
        diff,
    }))
}

fn should_skip_dir(name: &str) -> bool {
    matches!(
        name,
        ".git" | "target" | "node_modules" | "dist" | ".fl" | ".superpowers" | ".cargo"
    )
}

/// Scan `repo_root` for Cargo package dependencies, Rust source crate references, and
/// repository files for architecture conformance checking (spec §1.3).
pub fn scan_workspace_for_arch(repo_root: &Path) -> Result<WorkspaceArchScan, String> {
    let mut manifest_paths = BTreeSet::new();
    let root_cargo = repo_root.join("Cargo.toml");
    if root_cargo.is_file() {
        manifest_paths.insert(root_cargo);
    }

    for entry in WalkDir::new(repo_root)
        .into_iter()
        .filter_entry(|e| {
            e.depth() == 0
                || !e.file_type().is_dir()
                || e.file_name()
                    .to_str()
                    .is_none_or(|name| !should_skip_dir(name))
        })
        .flatten()
    {
        let path = entry.path();
        if path.is_file() && path.file_name().and_then(|s| s.to_str()) == Some("Cargo.toml") {
            manifest_paths.insert(path.to_path_buf());
        }
    }

    let mut package_deps = Vec::new();
    let mut workspace_packages = BTreeMap::new();

    for manifest in &manifest_paths {
        let Ok(raw) = fs::read_to_string(manifest) else {
            continue;
        };
        let Ok(parsed) = toml::from_str::<toml::Value>(&raw) else {
            continue;
        };
        let Some(pkg_name) = parsed
            .get("package")
            .and_then(|p| p.get("name"))
            .and_then(toml::Value::as_str)
        else {
            continue;
        };
        let rel_manifest = manifest
            .strip_prefix(repo_root)
            .unwrap_or(manifest)
            .to_string_lossy()
            .replace('\\', "/");

        let ident = pkg_name.replace('-', "_");
        workspace_packages.insert(ident, pkg_name.to_string());

        let mut recorded_any = false;
        for section in ["dependencies", "build-dependencies"] {
            if let Some(deps_table) = parsed.get(section).and_then(toml::Value::as_table) {
                for (dep_key, dep_val) in deps_table {
                    let target_pkg = dep_val
                        .as_table()
                        .and_then(|t| t.get("package"))
                        .and_then(toml::Value::as_str)
                        .unwrap_or(dep_key.as_str());
                    package_deps.push(PackageDep {
                        from_package: pkg_name.to_string(),
                        to_package: target_pkg.to_string(),
                        manifest_path: rel_manifest.clone(),
                    });
                    recorded_any = true;
                }
            }
        }
        if !recorded_any {
            package_deps.push(PackageDep {
                from_package: pkg_name.to_string(),
                to_package: pkg_name.to_string(),
                manifest_path: rel_manifest,
            });
        }
    }

    let mut source_refs = Vec::new();
    let mut repo_files = Vec::new();
    let has_crates_or_ui = repo_root.join("crates").is_dir() || repo_root.join("ui").is_dir();

    for entry in WalkDir::new(repo_root)
        .into_iter()
        .filter_entry(|e| {
            e.depth() == 0
                || !e.file_type().is_dir()
                || e.file_name()
                    .to_str()
                    .is_none_or(|name| !should_skip_dir(name))
        })
        .flatten()
    {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let rel = path
            .strip_prefix(repo_root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        if rel.is_empty() || rel == "Cargo.toml" || rel == "Cargo.lock" || rel.starts_with('.') {
            continue;
        }

        if !has_crates_or_ui || rel.starts_with("crates/") || rel.starts_with("ui/") {
            repo_files.push(rel.clone());
        }

        if path.extension().and_then(|s| s.to_str()) == Some("rs")
            && let Ok(src_text) = fs::read_to_string(path)
        {
            for target_pkg in extract_rust_crate_imports(&src_text, &workspace_packages) {
                source_refs.push(SourceReference {
                    from_path: rel.clone(),
                    target_component_or_package: target_pkg,
                });
            }
        }
    }

    repo_files.sort();
    Ok(WorkspaceArchScan {
        package_deps,
        source_refs,
        repo_files,
    })
}

fn extract_rust_crate_imports(
    src_text: &str,
    workspace_packages: &BTreeMap<String, String>,
) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for line in src_text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("//") {
            continue;
        }
        if trimmed.starts_with("#[cfg(test)]") {
            break;
        }
        let rest = trimmed
            .strip_prefix("pub(crate) use ")
            .or_else(|| trimmed.strip_prefix("pub use "))
            .or_else(|| trimmed.strip_prefix("use "))
            .or_else(|| trimmed.strip_prefix("extern crate "));
        let Some(after_use) = rest else {
            continue;
        };
        let root_ident: String = after_use
            .trim_start_matches("::")
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if let Some(pkg) = workspace_packages.get(&root_ident) {
            found.insert(pkg.clone());
        }
    }
    found
}

/// Scan `repo_root` and evaluate [`check_architecture_conformance`] against `model` (spec §1.3).
pub fn scan_cargo_architecture_conformance(
    repo_root: &Path,
    model: &ArchitectureModel,
) -> Result<ArchConformanceReport, String> {
    let scan = scan_workspace_for_arch(repo_root)?;
    check_architecture_conformance(
        model,
        &scan.package_deps,
        &scan.source_refs,
        &scan.repo_files,
    )
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn parse_jsonl_transcript_normalizes_antigravity_claude_and_codex_turns() {
        let jsonl = r#"
{"step_index":0,"type":"USER_INPUT","source":"USER_EXPLICIT","content":"Check hub status","created_at":"2026-10-27T01:00:00Z"}
{"step_index":1,"type":"PLANNER_RESPONSE","source":"MODEL","content":"Running status check.","thinking":"Let me inspect the hub.","tool_calls":[{"name":"run_command","arguments":{"cmd":"fl hub status"}}],"created_at":"2026-10-27T01:00:05Z"}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"Checking files"},{"type":"text","text":"All checks passed."},{"type":"tool_use","name":"read_file","input":{"path":"Cargo.toml"}}]},"timestamp":"2026-10-27T01:00:10Z"}
"#;
        let turns = parse_jsonl_transcript(jsonl);
        assert_eq!(turns.len(), 3);

        assert_eq!(turns[0].index, 0);
        assert_eq!(turns[0].role, "user");
        assert_eq!(turns[0].content, "Check hub status");

        assert_eq!(turns[1].index, 1);
        assert_eq!(turns[1].role, "assistant");
        assert_eq!(turns[1].content, "Running status check.");
        assert_eq!(
            turns[1].thinking.as_deref(),
            Some("Let me inspect the hub.")
        );
        assert_eq!(turns[1].tool_calls.len(), 1);
        assert_eq!(turns[1].tool_calls[0].name, "run_command");

        assert_eq!(turns[2].index, 2);
        assert_eq!(turns[2].role, "assistant");
        assert_eq!(turns[2].content, "All checks passed.");
        assert_eq!(turns[2].thinking.as_deref(), Some("Checking files"));
        assert_eq!(turns[2].tool_calls.len(), 1);
        assert_eq!(turns[2].tool_calls[0].name, "read_file");
    }

    #[test]
    fn parse_jsonl_transcript_normalizes_antigravity_system_message_prompts_and_wire() {
        let jsonl = [
            serde_json::json!({
                "step_index": 0,
                "source": "SYSTEM",
                "type": "SYSTEM_MESSAGE",
                "content": "The following is a <SYSTEM_MESSAGE> not actually sent by the user. It is provided by the system as important information to pay attention to.\n\n<SYSTEM_MESSAGE>\n[Message] timestamp=2026-10-27T15:35:06Z sender=system priority=MESSAGE_PRIORITY_HIGH content=[Operator Prompt via FerroDeck — from owner]\n\nPlease inspect the iPad layout.\n</SYSTEM_MESSAGE>",
                "created_at": "2026-10-27T15:35:06Z"
            })
            .to_string(),
            serde_json::json!({
                "step_index": 1,
                "source": "SYSTEM",
                "type": "SYSTEM_MESSAGE",
                "content": "The following is a <SYSTEM_MESSAGE> not actually sent by the user. It is provided by the system as important information to pay attention to.\n\n<SYSTEM_MESSAGE>\n[Message] timestamp=2026-10-27T15:35:10Z sender=system priority=MESSAGE_PRIORITY_HIGH content=Direct prompt without prefix\n</SYSTEM_MESSAGE>",
                "created_at": "2026-10-27T15:35:10Z"
            })
            .to_string(),
            serde_json::json!({
                "step_index": 2,
                "source": "SYSTEM",
                "type": "SYSTEM_MESSAGE",
                "content": "The following is a <SYSTEM_MESSAGE> not actually sent by the user. It is provided by the system as important information to pay attention to.\n\n<SYSTEM_MESSAGE>\n[Message] timestamp=2026-10-27T15:35:20Z sender=system priority=MESSAGE_PRIORITY_HIGH content=[Incoming NATS Message — pushed automatically]\nFrom: jarvis\nSubject: PR #16 review\nTimestamp: 2026-10-27T15:35:20Z\nMessage ID: msg-1\n\nApproved and ready to merge.\n</SYSTEM_MESSAGE>",
                "created_at": "2026-10-27T15:35:20Z"
            })
            .to_string(),
            serde_json::json!({
                "step_index": 3,
                "source": "SYSTEM",
                "type": "SYSTEM_MESSAGE",
                "content": "<SYSTEM_MESSAGE>\n[Message] timestamp=2026-10-27T15:35:30Z sender=system priority=MESSAGE_PRIORITY_HIGH content=[fl-wire mail/normal] from jarvis (id: urn:uuid:11111111-1111-1111-1111-111111111111, thread: urn:uuid:11111111-1111-1111-1111-111111111111): Status check\n\nAll systems nominal.\n</SYSTEM_MESSAGE>",
                "created_at": "2026-10-27T15:35:30Z"
            })
            .to_string(),
        ]
        .join("\n");

        let turns = parse_jsonl_transcript(&jsonl);
        assert_eq!(turns.len(), 4);

        assert_eq!(turns[0].role, "user");
        assert_eq!(turns[0].content, "Please inspect the iPad layout.");
        assert!(!turns[0].content.contains("<SYSTEM_MESSAGE>"));

        assert_eq!(turns[1].role, "user");
        assert_eq!(turns[1].content, "Direct prompt without prefix");
        assert!(!turns[1].content.contains("<SYSTEM_MESSAGE>"));

        assert_eq!(turns[2].role, "wire:jarvis");
        assert_eq!(turns[2].content, "Approved and ready to merge.");
        assert!(!turns[2].content.contains("<SYSTEM_MESSAGE>"));

        assert_eq!(turns[3].role, "wire:jarvis");
        assert_eq!(turns[3].content, "All systems nominal.");
        assert!(!turns[3].content.contains("<SYSTEM_MESSAGE>"));
    }

    #[test]
    fn parse_jsonl_transcript_normalizes_antigravity_generic_tool_output_as_tool_role() {
        let jsonl = [
            serde_json::json!({
                "step_index": 0,
                "source": "MODEL",
                "type": "GENERIC",
                "content": "Created At: 2026-10-27T15:38:05Z\nCompleted At: 2026-10-27T15:38:06Z\nExit Code: 0\n\ntest result: ok.",
                "created_at": "2026-10-27T15:38:06Z"
            })
            .to_string(),
            serde_json::json!({
                "step_index": 1,
                "source": "MODEL",
                "content": "Created At: 2026-10-27T15:38:07Z\nCompleted At: 2026-10-27T15:38:08Z\nShowing lines 1-10",
                "created_at": "2026-10-27T15:38:08Z"
            })
            .to_string(),
        ]
        .join("\n");

        let turns = parse_jsonl_transcript(&jsonl);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].role, "tool");
        assert!(turns[0].content.contains("test result: ok."));
        assert_eq!(turns[1].role, "tool");
    }

    #[test]
    fn git_worktree_inspector_lists_worktree_and_diff() {
        let tmp = TempDir::new().unwrap();
        let run = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .args(args)
                    .current_dir(tmp.path())
                    .status()
                    .unwrap()
                    .success()
            );
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "t"]);
        fs::write(tmp.path().join("README.md"), "hello\n").unwrap();
        run(&["add", "README.md"]);
        run(&["commit", "-qm", "init"]);

        // Modify README.md so worktree is dirty and has a diff
        fs::write(tmp.path().join("README.md"), "hello world\n").unwrap();

        let wts = list_git_worktrees(tmp.path()).unwrap();
        assert_eq!(wts.len(), 1);
        assert!(!wts[0].is_clean);
        assert_eq!(wts[0].dirty_files, vec!["README.md".to_string()]);

        let diff = read_worktree_diff(tmp.path(), &wts[0].id).unwrap().unwrap();
        assert_eq!(diff.files.len(), 1);
        assert_eq!(diff.files[0].path, "README.md");
        assert!(diff.diff.contains("+hello world"));
    }

    #[test]
    fn is_transcript_busy_detects_active_vs_idle_session_states() {
        assert!(!is_transcript_busy(&[]));

        let user_turn = TranscriptTurn {
            index: 0,
            role: "user".into(),
            content: "Please run tests".into(),
            thinking: None,
            tool_calls: Vec::new(),
            created_at: None,
        };
        assert!(is_transcript_busy(std::slice::from_ref(&user_turn)));

        let assistant_with_tools = TranscriptTurn {
            index: 1,
            role: "assistant".into(),
            content: "Running cargo test".into(),
            thinking: None,
            tool_calls: vec![TranscriptToolCall {
                name: "run_command".into(),
                arguments: serde_json::json!({"cmd": "cargo test"}),
            }],
            created_at: None,
        };
        assert!(is_transcript_busy(&[
            user_turn.clone(),
            assistant_with_tools.clone()
        ]));

        let tool_turn = TranscriptTurn {
            index: 2,
            role: "tool".into(),
            content: "test result: ok.".into(),
            thinking: None,
            tool_calls: Vec::new(),
            created_at: None,
        };
        assert!(is_transcript_busy(&[
            user_turn.clone(),
            assistant_with_tools.clone(),
            tool_turn.clone()
        ]));

        let final_assistant = TranscriptTurn {
            index: 3,
            role: "assistant".into(),
            content: "All tests passed.".into(),
            thinking: None,
            tool_calls: Vec::new(),
            created_at: None,
        };
        let thinking_only_assistant = TranscriptTurn {
            index: 3,
            role: "assistant".into(),
            content: String::new(),
            thinking: Some("Reasoning in progress...".into()),
            tool_calls: Vec::new(),
            created_at: None,
        };
        assert!(is_transcript_busy(&[
            user_turn.clone(),
            thinking_only_assistant
        ]));
        assert!(!is_transcript_busy(&[
            user_turn,
            assistant_with_tools,
            tool_turn,
            final_assistant
        ]));
    }

    #[test]
    fn find_transcript_file_and_parse_muse_session_jsonl() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let session_id = "01a17d5b-482d-7ec0-a974-eaa56ef667a7";

        // Create a non-transcript `.json` runtime file that shares the session stem
        let runtime_dir = home.join(".local/share/muse/runtime/muse/sessions");
        fs::create_dir_all(&runtime_dir).unwrap();
        fs::write(
            runtime_dir.join(format!("{session_id}.json")),
            r#"{"status":"running"}"#,
        )
        .unwrap();

        // Create the actual Muse `session.jsonl` inside the session directory
        let session_dir = home
            .join(".local/share/muse/sessions/2026/10/27")
            .join(session_id);
        fs::create_dir_all(&session_dir).unwrap();
        let jsonl_path = session_dir.join("session.jsonl");
        let jsonl = [
            serde_json::json!({
                "schema_version": 1,
                "recorded_at": 1_793_104_670_899_152_i64,
                "payload_type": "peer_message.admission.requested",
                "payload": {
                    "body": "Hello felicia"
                }
            })
            .to_string(),
            serde_json::json!({
                "schema_version": 1,
                "recorded_at": 1_793_104_845_773_692_i64,
                "payload_type": "runtime.session",
                "payload": {
                    "kind": "run",
                    "event": {
                        "kind": "assistant_tool_calls_committed",
                        "tool_calls": [
                            {
                                "name": "list_peer_sessions",
                                "args": "{\"scope\":\"workspace\"}"
                            }
                        ]
                    }
                }
            })
            .to_string(),
            serde_json::json!({
                "schema_version": 1,
                "recorded_at": 1_793_104_889_531_568_i64,
                "payload_type": "runtime.session",
                "payload": {
                    "kind": "run",
                    "event": {
                        "kind": "assistant_message_committed",
                        "text": "Peer session greeted Felicia; I replied with a hello back."
                    }
                }
            })
            .to_string(),
        ]
        .join("\n");
        fs::write(&jsonl_path, &jsonl).unwrap();

        let found = find_transcript_file_in_home(home, session_id).expect("must find session.jsonl");
        assert_eq!(found, jsonl_path.display().to_string());

        let turns = parse_jsonl_transcript(&jsonl);
        assert_eq!(turns.len(), 3);
        assert_eq!(turns[0].role, "user");
        assert_eq!(turns[0].content, "Hello felicia");
        assert!(turns[0].created_at.is_some());
        assert_eq!(turns[1].role, "assistant");
        assert_eq!(turns[1].tool_calls.len(), 1);
        assert_eq!(turns[1].tool_calls[0].name, "list_peer_sessions");
        assert_eq!(
            turns[1].tool_calls[0].arguments,
            serde_json::json!({"scope": "workspace"})
        );
        assert_eq!(turns[2].role, "assistant");
        assert_eq!(
            turns[2].content,
            "Peer session greeted Felicia; I replied with a hello back."
        );
        assert!(!is_transcript_busy(&turns));
    }
}
