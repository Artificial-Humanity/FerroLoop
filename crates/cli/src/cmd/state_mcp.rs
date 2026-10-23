//! Newline-delimited JSON-RPC 2.0 MCP server over `stdin`/`stdout` (spec §5.3).
//!
//! Opens and drops `RedbStore` inside each individual `tools/call` invocation so the
//! exclusive `redb` file lock is never held while waiting on `stdin`.

use std::io::{BufRead, Write};

use fl_core::agent::{AgentRole, RegisterAgentArgs, reports_to};
use fl_core::state::{
    HandoffEntry, MemoryBasis, MemoryCategory, MemoryEntry, MemoryLifecycle, MergeMode,
    RuntimeEntry, SensitivityPatch, StateConcern, StateEntry, StatePayload, StateScope,
    UpsertStateArgs,
};
use fl_core::store::{AgentRegistry, StateStore};
use serde_json::{Value, json};

use crate::cmd::agent::now_at;
use crate::cmd::state::{
    DEFAULT_RENDER_MAX_BYTES, StateEnv, parse_concern, parse_memory_basis, parse_memory_category,
    parse_merge_mode, render_state_markdown, resolve_state_in_store,
};

fn parse_memory_lifecycle(s: &str) -> Result<MemoryLifecycle, String> {
    MemoryLifecycle::from_wire(s).ok_or_else(|| {
        format!(
            "unknown memory lifecycle `{s}`; expected one of: {}",
            MemoryLifecycle::wire_values()
        )
    })
}

pub fn run(env: &StateEnv<'_>) -> Result<(), String> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = stdin.lock();
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader.read_line(&mut line).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                let err_resp = json!({
                    "jsonrpc": "2.0",
                    "id": Value::Null,
                    "error": {
                        "code": -32700,
                        "message": format!("parse error: {e}"),
                    }
                });
                write_line(&stdout, &err_resp)?;
                continue;
            }
        };

        let id = req.get("id").cloned();
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");
        let params = req.get("params").cloned().unwrap_or_else(|| json!({}));

        // Notifications have no `id` and expect no response.
        let Some(req_id) = id else {
            continue;
        };

        let response = match method {
            "initialize" => json!({
                "jsonrpc": "2.0",
                "id": req_id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {
                        "tools": {}
                    },
                    "serverInfo": {
                        "name": "fl-state-mcp",
                        "version": env!("CARGO_PKG_VERSION"),
                    }
                }
            }),
            "ping" => json!({
                "jsonrpc": "2.0",
                "id": req_id,
                "result": {}
            }),
            "tools/list" => json!({
                "jsonrpc": "2.0",
                "id": req_id,
                "result": {
                    "tools": tool_definitions(),
                }
            }),
            "tools/call" => {
                let tool_name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let args = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                match dispatch_tool(env, tool_name, &args) {
                    Ok(payload) => json!({
                        "jsonrpc": "2.0",
                        "id": req_id,
                        "result": {
                            "content": [{
                                "type": "text",
                                "text": payload,
                            }],
                            "isError": false,
                        }
                    }),
                    Err(err_msg) => json!({
                        "jsonrpc": "2.0",
                        "id": req_id,
                        "result": {
                            "content": [{
                                "type": "text",
                                "text": err_msg,
                            }],
                            "isError": true,
                        }
                    }),
                }
            }
            other => json!({
                "jsonrpc": "2.0",
                "id": req_id,
                "error": {
                    "code": -32601,
                    "message": format!("method not found: {other}"),
                }
            }),
        };

        write_line(&stdout, &response)?;
    }
    Ok(())
}

fn write_line(stdout: &std::io::Stdout, val: &Value) -> Result<(), String> {
    let mut out = stdout.lock();
    let s = serde_json::to_string(val).map_err(|e| e.to_string())?;
    out.write_all(s.as_bytes()).map_err(|e| e.to_string())?;
    out.write_all(b"\n").map_err(|e| e.to_string())?;
    out.flush().map_err(|e| e.to_string())
}

fn parse_sensitivity(args: &Value) -> SensitivityPatch {
    if args
        .get("sensitive")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        SensitivityPatch::SetSensitive
    } else if args
        .get("clear_sensitive")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        SensitivityPatch::ClearSensitive
    } else {
        SensitivityPatch::Inherit
    }
}

fn req_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing required string argument `{key}`"))
}

fn dispatch_tool(env: &StateEnv<'_>, name: &str, args: &Value) -> Result<String, String> {
    match name {
        "agent_roster" => {
            let project = args.get("project").and_then(Value::as_str);
            let include_retired = args
                .get("include_retired")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let store = env.open_for_scope(Some(&StateScope::Workspace))?;
            let agents = store
                .list_agents(project, include_retired)
                .map_err(|e| e.to_string())?;
            let all_agents = store.list_agents(None, true).map_err(|e| e.to_string())?;
            let enriched: Vec<Value> = agents
                .iter()
                .map(|a| {
                    let mut v = serde_json::to_value(a).unwrap();
                    if let Some(obj) = v.as_object_mut() {
                        obj.insert(
                            "reports_to".into(),
                            Value::String(reports_to(a, &all_agents)),
                        );
                    }
                    v
                })
                .collect();
            Ok(serde_json::to_string_pretty(&enriched).unwrap())
        }
        "agent_register" => {
            let display_name = req_str(args, "name")?.to_string();
            let project = req_str(args, "project")?.to_string();
            let role_raw = req_str(args, "role")?;
            let role = AgentRole::from_wire(role_raw).map_err(|e| e.to_string())?;
            let by = req_str(args, "by")?.to_string();
            let email = args
                .get("email")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            let create_area = args
                .get("create_area")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let replace = args
                .get("replace")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let extra_areas: Vec<String> = args
                .get("extra_areas")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(Value::as_str)
                        .map(ToOwned::to_owned)
                        .collect()
                })
                .unwrap_or_default();

            let store = env.open_for_scope(Some(&StateScope::Workspace))?;
            let rec = store
                .register_agent(RegisterAgentArgs {
                    display_name,
                    email,
                    project,
                    role,
                    extra_areas,
                    create_area,
                    replace,
                    at: now_at(),
                    by,
                })
                .map_err(|e| e.to_string())?;
            let all_active = store.list_agents(None, false).unwrap_or_default();
            let mgr = reports_to(&rec, &all_active);
            let mut val = serde_json::to_value(&rec).unwrap();
            if let Some(obj) = val.as_object_mut() {
                obj.insert("reports_to".into(), Value::String(mgr));
                obj.insert(
                    "co_author_trailer".into(),
                    Value::String(rec.co_author_trailer()),
                );
            }
            Ok(serde_json::to_string_pretty(&val).unwrap())
        }
        "state_render" => {
            let max_bytes = args
                .get("max_bytes")
                .and_then(Value::as_u64)
                .map(|n| n as usize)
                .unwrap_or(DEFAULT_RENDER_MAX_BYTES);
            let include_sensitive = args
                .get("include_sensitive")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let mut parsed_scopes = Vec::new();
            if let Some(arr) = args.get("scopes").and_then(Value::as_array) {
                for v in arr {
                    if let Some(s) = v.as_str() {
                        parsed_scopes.push(StateScope::from_wire(s).map_err(|e| e.to_string())?);
                    }
                }
            } else if let Some(s) = args.get("scope").and_then(Value::as_str) {
                parsed_scopes.push(StateScope::from_wire(s).map_err(|e| e.to_string())?);
            }
            if parsed_scopes.is_empty() {
                parsed_scopes = env.default_render_scopes();
            }

            let (areas, agents) = {
                let ws_store = env.open_for_scope(Some(&StateScope::Workspace))?;
                let areas = ws_store.list_areas().map_err(|e| e.to_string())?;
                let agents = ws_store
                    .list_agents(None, false)
                    .map_err(|e| e.to_string())?;
                (areas, agents)
            };
            let mut entries = Vec::new();
            for sc in &parsed_scopes {
                let st = env.open_for_scope(Some(sc))?;
                entries.extend(st.list_state(Some(sc), None).map_err(|e| e.to_string())?);
            }
            Ok(render_state_markdown(
                &areas,
                &agents,
                &entries,
                max_bytes,
                include_sensitive,
            ))
        }
        "state_get" => {
            let id_or_key = req_str(args, "id_or_key")?;
            let parsed_scope = args
                .get("scope")
                .and_then(Value::as_str)
                .map(StateScope::from_wire)
                .transpose()
                .map_err(|e| e.to_string())?;
            let parsed_concern: Option<StateConcern> = args
                .get("concern")
                .and_then(Value::as_str)
                .map(parse_concern)
                .transpose()?;
            let history = args
                .get("history")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let store = env.open_for_id_or_key(id_or_key, parsed_scope.as_ref())?;
            let entry =
                resolve_state_in_store(&store, id_or_key, parsed_scope.as_ref(), parsed_concern)?;
            if history {
                let revs = store
                    .state_revisions(&entry.id)
                    .map_err(|e| e.to_string())?;
                Ok(serde_json::to_string_pretty(&json!({
                    "entry": entry,
                    "revisions": revs,
                }))
                .unwrap())
            } else {
                Ok(serde_json::to_string_pretty(&entry).unwrap())
            }
        }
        "state_list" => {
            let parsed_scope = args
                .get("scope")
                .and_then(Value::as_str)
                .map(StateScope::from_wire)
                .transpose()
                .map_err(|e| e.to_string())?;
            let parsed_concern: Option<StateConcern> = args
                .get("concern")
                .and_then(Value::as_str)
                .map(parse_concern)
                .transpose()?;
            let parsed_cat: Option<MemoryCategory> = args
                .get("category")
                .and_then(Value::as_str)
                .map(parse_memory_category)
                .transpose()?;
            let include_retired = args
                .get("include_retired")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let store = env.open_for_scope(parsed_scope.as_ref())?;
            let entries: Vec<StateEntry> = store
                .list_state(parsed_scope.as_ref(), parsed_concern)
                .map_err(|e| e.to_string())?
                .into_iter()
                .filter(|e| match &e.payload {
                    StatePayload::Memory(m) => {
                        (include_retired || m.lifecycle == MemoryLifecycle::Active)
                            && parsed_cat.is_none_or(|c| m.category == c)
                    }
                    _ => parsed_cat.is_none(),
                })
                .collect();
            Ok(serde_json::to_string_pretty(&entries).unwrap())
        }
        "state_set_memory" => {
            let scope =
                StateScope::from_wire(req_str(args, "scope")?).map_err(|e| e.to_string())?;
            let key = req_str(args, "key")?.to_string();
            let by = req_str(args, "by")?.to_string();
            let lifecycle: MemoryLifecycle = match args.get("lifecycle").and_then(Value::as_str) {
                Some(s) => parse_memory_lifecycle(s)?,
                None => MemoryLifecycle::Active,
            };
            let store = env.open_for_scope(Some(&scope))?;
            if matches!(
                lifecycle,
                MemoryLifecycle::Retired | MemoryLifecycle::Superseded
            ) && args.get("summary").is_none()
            {
                let sup_id = match args.get("superseded_by").and_then(Value::as_str) {
                    Some(raw_sup) => {
                        let sup_entry = resolve_state_in_store(
                            &store,
                            raw_sup,
                            Some(&scope),
                            Some(StateConcern::Memory),
                        )?;
                        Some(sup_entry.id)
                    }
                    None => None,
                };
                let updated = store
                    .retire_memory(&scope, &key, lifecycle, sup_id, now_at(), &by)
                    .map_err(|e| e.to_string())?;
                return Ok(serde_json::to_string_pretty(&updated).unwrap());
            }

            let category: MemoryCategory = parse_memory_category(req_str(args, "category")?)?;
            let basis: MemoryBasis = parse_memory_basis(req_str(args, "basis")?)?;
            let summary = req_str(args, "summary")?.to_string();
            let body = req_str(args, "body")?.to_string();
            let pinned = args.get("pinned").and_then(Value::as_bool).unwrap_or(false);
            let superseded_by = match args.get("superseded_by").and_then(Value::as_str) {
                Some(raw_sup) => {
                    let sup_entry = resolve_state_in_store(
                        &store,
                        raw_sup,
                        Some(&scope),
                        Some(StateConcern::Memory),
                    )?;
                    Some(sup_entry.id)
                }
                None => None,
            };
            let entry = store
                .upsert_state(UpsertStateArgs {
                    scope,
                    slug: key,
                    sensitivity: parse_sensitivity(args),
                    payload: StatePayload::Memory(MemoryEntry {
                        category,
                        basis,
                        lifecycle,
                        superseded_by,
                        pinned,
                        summary,
                        body,
                    }),
                    at: now_at(),
                    by,
                })
                .map_err(|e| e.to_string())?;
            Ok(serde_json::to_string_pretty(&entry).unwrap())
        }
        "state_set_handoff" => {
            let scope =
                StateScope::from_wire(req_str(args, "scope")?).map_err(|e| e.to_string())?;
            let key = args
                .get("key")
                .and_then(Value::as_str)
                .unwrap_or("current")
                .to_string();
            let summary = req_str(args, "summary")?.to_string();
            let by = req_str(args, "by")?.to_string();
            let str_vec = |field: &str| -> Vec<String> {
                args.get(field)
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(Value::as_str)
                            .map(ToOwned::to_owned)
                            .collect()
                    })
                    .unwrap_or_default()
            };
            let shipped = str_vec("shipped");
            let in_flight = str_vec("in_flight");
            let blockers = str_vec("blockers");
            let next_steps = str_vec("next_steps");
            let notes = args
                .get("notes")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            let store = env.open_for_scope(Some(&scope))?;
            let entry = store
                .upsert_state(UpsertStateArgs {
                    scope,
                    slug: key,
                    sensitivity: parse_sensitivity(args),
                    payload: StatePayload::Handoff(HandoffEntry {
                        summary,
                        shipped,
                        in_flight,
                        blockers,
                        next_steps,
                        notes,
                    }),
                    at: now_at(),
                    by,
                })
                .map_err(|e| e.to_string())?;
            Ok(serde_json::to_string_pretty(&entry).unwrap())
        }
        "state_set_runtime" => {
            let scope =
                StateScope::from_wire(req_str(args, "scope")?).map_err(|e| e.to_string())?;
            let key = req_str(args, "key")?.to_string();
            let by = req_str(args, "by")?.to_string();
            let value = args
                .get("value")
                .cloned()
                .ok_or_else(|| "missing required argument `value`".to_string())?;
            let merge_mode: MergeMode = match args.get("merge_mode").and_then(Value::as_str) {
                Some(m) => parse_merge_mode(m)?,
                None => MergeMode::Conflict,
            };
            let store = env.open_for_scope(Some(&scope))?;
            let entry = store
                .upsert_state(UpsertStateArgs {
                    scope,
                    slug: key,
                    sensitivity: parse_sensitivity(args),
                    payload: StatePayload::Runtime(RuntimeEntry { value, merge_mode }),
                    at: now_at(),
                    by,
                })
                .map_err(|e| e.to_string())?;
            Ok(serde_json::to_string_pretty(&entry).unwrap())
        }
        other => Err(format!("unknown tool `{other}`")),
    }
}

fn tool_definitions() -> Value {
    json!([
        {
            "name": "agent_roster",
            "description": "List registered agents (and derived reports_to) from the workspace state store.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": { "type": "string" },
                    "include_retired": { "type": "boolean" }
                }
            }
        },
        {
            "name": "agent_register",
            "description": "Register or resume an agent identity in the workspace state store.",
            "inputSchema": {
                "type": "object",
                "required": ["name", "project", "role", "by"],
                "properties": {
                    "name": { "type": "string" },
                    "project": { "type": "string" },
                    "role": { "type": "string" },
                    "email": { "type": "string" },
                    "extra_areas": { "type": "array", "items": { "type": "string" } },
                    "create_area": { "type": "boolean" },
                    "replace": { "type": "boolean" },
                    "by": { "type": "string" }
                }
            }
        },
        {
            "name": "state_render",
            "description": "Render the deterministic read-only startup Markdown summary.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "scopes": { "type": "array", "items": { "type": "string" } },
                    "max_bytes": { "type": "integer" },
                    "include_sensitive": { "type": "boolean" }
                }
            }
        },
        {
            "name": "state_get",
            "description": "Fetch a single state entry (and optional revision history) by IRI, handle, or slug.",
            "inputSchema": {
                "type": "object",
                "required": ["id_or_key"],
                "properties": {
                    "id_or_key": { "type": "string" },
                    "scope": { "type": "string" },
                    "concern": { "type": "string" },
                    "history": { "type": "boolean" }
                }
            }
        },
        {
            "name": "state_list",
            "description": "List state entries filtered by scope, concern, or memory category.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "scope": { "type": "string" },
                    "concern": { "type": "string" },
                    "category": { "type": "string" },
                    "include_retired": { "type": "boolean" }
                }
            }
        },
        {
            "name": "state_set_memory",
            "description": "Create, update, retire, or supersede a durable Memory entry.",
            "inputSchema": {
                "type": "object",
                "required": ["scope", "key", "by"],
                "properties": {
                    "scope": { "type": "string" },
                    "key": { "type": "string" },
                    "category": { "type": "string" },
                    "basis": { "type": "string" },
                    "lifecycle": { "type": "string" },
                    "superseded_by": { "type": "string" },
                    "pinned": { "type": "boolean" },
                    "summary": { "type": "string" },
                    "body": { "type": "string" },
                    "sensitive": { "type": "boolean" },
                    "clear_sensitive": { "type": "boolean" },
                    "by": { "type": "string" }
                }
            }
        },
        {
            "name": "state_set_handoff",
            "description": "Create or update a Handoff status snapshot.",
            "inputSchema": {
                "type": "object",
                "required": ["scope", "summary", "by"],
                "properties": {
                    "scope": { "type": "string" },
                    "key": { "type": "string" },
                    "summary": { "type": "string" },
                    "shipped": { "type": "array", "items": { "type": "string" } },
                    "in_flight": { "type": "array", "items": { "type": "string" } },
                    "blockers": { "type": "array", "items": { "type": "string" } },
                    "next_steps": { "type": "array", "items": { "type": "string" } },
                    "notes": { "type": "string" },
                    "sensitive": { "type": "boolean" },
                    "clear_sensitive": { "type": "boolean" },
                    "by": { "type": "string" }
                }
            }
        },
        {
            "name": "state_set_runtime",
            "description": "Create or update a structured JSON Runtime entry.",
            "inputSchema": {
                "type": "object",
                "required": ["scope", "key", "value", "by"],
                "properties": {
                    "scope": { "type": "string" },
                    "key": { "type": "string" },
                    "value": {},
                    "merge_mode": { "type": "string" },
                    "sensitive": { "type": "boolean" },
                    "clear_sensitive": { "type": "boolean" },
                    "by": { "type": "string" }
                }
            }
        }
    ])
}
