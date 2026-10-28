//! Newline-delimited JSON-RPC 2.0 MCP server over `stdin`/`stdout` (spec §5.3).
//!
//! Opens and drops `RedbStore` inside each individual `tools/call` invocation so the
//! exclusive `redb` file lock is never held while waiting on `stdin`.

use std::io::{BufRead, Write};

use fl_core::agent::{AgentRole, RegisterAgentArgs, reports_to};
use fl_core::at::At;
use fl_core::ferrostate::create_state_checkpoint;
use fl_core::ferrowire::{MessageKind, MessageUrgency, SendWireArgs, WireAddress, WireFilter};
use fl_core::ids::{Kind, ProjectId, WorkflowRunId};
use fl_core::state::{
    HandoffEntry, MemoryBasis, MemoryCategory, MemoryEntry, MemoryLifecycle, MergeMode,
    RuntimeEntry, SensitivityPatch, StateConcern, StateEntry, StatePayload, StateScope,
    UpsertStateArgs,
};
use fl_core::store::{AgentRegistry, Catalog, Handles, StateStore, WorkflowStore};
use fl_core::workflow::{
    StepAttemptReport, StepOutcome, WorkflowError, WorkflowRun, advance_workflow,
};
use fl_exec::workflow::{
    list_workflow_facts, list_workflow_handoffs, record_step_handoff, write_workflow_fact,
};
use fl_store::RedbStore;
use serde_json::{Value, json};
use std::time::Duration;

use crate::cmd::agent::now_at;
use crate::cmd::state::{
    DEFAULT_RENDER_MAX_BYTES, StateEnv, collect_search_hits, effective_search_limit, parse_concern,
    parse_memory_basis, parse_memory_category, parse_merge_mode, render_state_markdown,
    resolve_state_in_store, search_result_json,
};
use crate::cmd::wire::{
    execute_ack, execute_read, execute_reply, execute_send, inbox_item_to_json, now_millis,
    parse_message_id, resolve_caller, wait_for_reply,
};
use crate::refs::{self, Ref};

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

fn opt_str(args: &Value, key: &str) -> Result<Option<String>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(format!("argument `{key}` must be a string")),
    }
}

fn parse_search_limit(args: &Value) -> Result<usize, String> {
    match args.get("limit") {
        None | Some(Value::Null) => Ok(effective_search_limit(None)),
        Some(v) => {
            if let Some(u) = v.as_u64() {
                let n = usize::try_from(u)
                    .map_err(|_| "argument `limit` must be a non-negative integer".to_string())?;
                Ok(effective_search_limit(Some(n)))
            } else if let Some(f) = v.as_f64() {
                if f.is_finite() && f >= 0.0 && f.fract() == 0.0 && f <= usize::MAX as f64 {
                    Ok(effective_search_limit(Some(f as usize)))
                } else {
                    Err("argument `limit` must be a non-negative integer".to_string())
                }
            } else {
                Err("argument `limit` must be a number".to_string())
            }
        }
    }
}

/// Parsed `fl_memory_search` arguments: `(query, scope, concern, limit)`.
fn parse_search_args(
    args: &Value,
) -> Result<(String, Option<StateScope>, Option<StateConcern>, usize), String> {
    let query = req_str(args, "query")?.to_string();
    let scope = match opt_str(args, "scope")? {
        Some(s) => Some(StateScope::from_wire(&s).map_err(|e| e.to_string())?),
        None => None,
    };
    let concern = match opt_str(args, "concern")? {
        Some(s) => Some(parse_concern(&s)?),
        None => None,
    };
    let limit = parse_search_limit(args)?;
    Ok((query, scope, concern, limit))
}

/// Parsed `fl_state_checkpoint` arguments, with `scope` left unresolved
/// (`None` means the caller's default checkpoint scope) and `by`
/// defaulting to `"fl"`.
struct CheckpointArgs {
    scope: Option<StateScope>,
    slug: Option<String>,
    summary: Option<String>,
    by: String,
}

fn parse_checkpoint_args(args: &Value) -> Result<CheckpointArgs, String> {
    if !args.is_object() {
        return Err("arguments must be a JSON object".to_string());
    }
    let scope = match opt_str(args, "scope")? {
        Some(s) => Some(StateScope::from_wire(&s).map_err(|e| e.to_string())?),
        None => None,
    };
    let slug = opt_str(args, "slug")?;
    if let Some(raw) = slug.as_deref() {
        fl_core::state::validate_slug(raw).map_err(|e| e.to_string())?;
    }
    let summary = opt_str(args, "summary")?;
    let by = match opt_str(args, "by")? {
        Some(s) if !s.trim().is_empty() => s,
        _ => "fl".to_string(),
    };
    Ok(CheckpointArgs {
        scope,
        slug,
        summary,
        by,
    })
}

fn memory_search_json(
    entries: &[StateEntry],
    query: &str,
    scope: Option<&StateScope>,
    concern: Option<StateConcern>,
    limit: usize,
) -> String {
    let (hits, total_hits) = collect_search_hits(entries, query, scope, concern, limit);
    serde_json::to_string_pretty(&search_result_json(query, &hits, total_hits)).unwrap()
}

fn open_workflow_store(
    env: &StateEnv<'_>,
    project_filter: Option<&str>,
    run_id_hint: Option<&str>,
) -> Result<RedbStore, String> {
    if let Some(db) = env.explicit_db {
        if let Some(dir) = db.parent()
            && !dir.as_os_str().is_empty()
        {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        return RedbStore::open(db).map_err(|e| format!("{}: {e}", db.display()));
    }

    let primary_path = if let Some(proj_slug) = project_filter
        && let Some(entry) = env.cfg.find_project_entry(proj_slug)
    {
        entry.store.clone()
    } else if let Ok(Some(entry)) = crate::config::bound_entry(&env.cfg.projects, env.cwd) {
        entry.store
    } else {
        env.cfg.workspace_state_store(env.default_store)
    };

    if let Some(dir) = primary_path.parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let primary =
        RedbStore::open(&primary_path).map_err(|e| format!("{}: {e}", primary_path.display()))?;

    if let Some(raw_run) = run_id_hint
        && let Ok(Ref::Iri(iri)) = raw_run.parse::<Ref>()
    {
        if primary.owns(&iri).map_err(|e| e.to_string())? {
            return Ok(primary);
        }
        let ws_path = env.cfg.workspace_state_store(env.default_store);
        if ws_path != primary_path && ws_path.exists() {
            let other = RedbStore::open(&ws_path).map_err(|e| e.to_string())?;
            if other.owns(&iri).map_err(|e| e.to_string())? {
                return Ok(other);
            }
        }
        for entry in &env.cfg.projects {
            if entry.store == primary_path || !entry.store.exists() {
                continue;
            }
            let other = RedbStore::open(&entry.store).map_err(|e| e.to_string())?;
            if other.owns(&iri).map_err(|e| e.to_string())? {
                return Ok(other);
            }
        }
    }

    Ok(primary)
}

fn resolve_workflow_run_in_store(store: &RedbStore, raw: &str) -> Result<WorkflowRun, String> {
    let r = raw.parse::<Ref>()?;
    let iri =
        refs::resolve(store, store.label(), Kind::WorkflowRun, &r).map_err(|e| e.to_string())?;
    store
        .get_workflow_run(&WorkflowRunId(iri))
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no workflow run matching `{raw}` in {}", store.label()))
}

fn resolve_optional_project_filter(
    store: &RedbStore,
    raw_project: Option<&str>,
) -> Result<Option<ProjectId>, String> {
    let Some(raw) = raw_project.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    if let Ok(r) = raw.parse::<Ref>() {
        let iri =
            refs::resolve(store, store.label(), Kind::Project, &r).map_err(|e| e.to_string())?;
        return Ok(Some(ProjectId(iri)));
    }
    let projects = store.list_projects().map_err(|e| e.to_string())?;
    for p in projects {
        let base_match = std::path::Path::new(&p.root)
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|b| b.eq_ignore_ascii_case(raw));
        if base_match || p.root == raw {
            return Ok(Some(p.id));
        }
    }
    Ok(None)
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
        "fl_state_checkpoint" => {
            let parsed = parse_checkpoint_args(args)?;
            let scope = parsed
                .scope
                .unwrap_or_else(|| env.default_checkpoint_scope());
            let store = env.open_for_scope(Some(&scope))?;
            let report = create_state_checkpoint(
                &store,
                env.cwd,
                scope,
                parsed.slug.as_deref(),
                parsed.summary.as_deref(),
                &parsed.by,
                now_at(),
            )?;
            Ok(serde_json::to_string_pretty(&report).unwrap())
        }
        "fl_memory_search" => {
            let (query, parsed_scope, parsed_concern, limit) = parse_search_args(args)?;
            let store = env.open_for_scope(parsed_scope.as_ref())?;
            let entries: Vec<StateEntry> = store
                .list_state(parsed_scope.as_ref(), parsed_concern)
                .map_err(|e| e.to_string())?;
            Ok(memory_search_json(
                &entries,
                &query,
                parsed_scope.as_ref(),
                parsed_concern,
                limit,
            ))
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
        "send_message" => {
            let to_raw = args
                .get("to")
                .or_else(|| args.get("recipient"))
                .and_then(Value::as_str)
                .ok_or_else(|| "missing required string argument `to`".to_string())?;
            let to = WireAddress::parse(to_raw).map_err(|e| e.to_string())?;
            let subject = req_str(args, "subject")?.to_string();
            let body = req_str(args, "body")?.to_string();
            let kind = match args.get("kind").and_then(Value::as_str) {
                Some(k) => MessageKind::from_wire(k).ok_or_else(|| {
                    format!(
                        "unknown message kind `{k}`; expected one of: {}",
                        MessageKind::wire_values()
                    )
                })?,
                None => MessageKind::Mail,
            };
            let urgent = args.get("urgent").and_then(Value::as_bool).unwrap_or(false);
            let durable = args
                .get("durable")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let thread_id = args
                .get("thread_id")
                .and_then(Value::as_str)
                .map(parse_message_id)
                .transpose()?;
            let from_agent = args.get("from_agent").and_then(Value::as_str);
            let sender_name = resolve_caller(env, from_agent, None);
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
                    to,
                    kind,
                    urgency,
                    durable,
                    subject,
                    body,
                    reply_to: None,
                    thread_id,
                    deadline_at: None,
                    at: now_at(),
                },
            )?;
            Ok(serde_json::to_string_pretty(&json!({
                "status": "sent",
                "message": msg,
                "receipts": receipts,
            }))
            .unwrap())
        }
        "broadcast_message" => {
            let subject = req_str(args, "subject")?.to_string();
            let body = req_str(args, "body")?.to_string();
            let urgent = args.get("urgent").and_then(Value::as_bool).unwrap_or(false);
            let durable = args
                .get("durable")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let from_agent = args.get("from_agent").and_then(Value::as_str);
            let sender_name = resolve_caller(env, from_agent, None);
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
            )?;
            Ok(serde_json::to_string_pretty(&json!({
                "status": "broadcast",
                "message": msg,
                "receipts": receipts,
            }))
            .unwrap())
        }
        "ask_message" => {
            let to = WireAddress::parse(req_str(args, "to")?).map_err(|e| e.to_string())?;
            let subject = req_str(args, "subject")?.to_string();
            let body = req_str(args, "body")?.to_string();
            let timeout_secs = args.get("timeout_secs").and_then(Value::as_u64);
            let from_agent = args.get("from_agent").and_then(Value::as_str);
            let sender_name = resolve_caller(env, from_agent, None);
            let now_ms = now_millis();
            let now = At::from_unix_millis(now_ms);
            let deadline_at = timeout_secs.map(|s| At::from_unix_millis(now_ms + s * 1000));
            let (ask_msg, receipts) = execute_send(
                env,
                SendWireArgs {
                    id: None,
                    sender_id: None,
                    sender_name: sender_name.clone(),
                    sender_host: None,
                    to,
                    kind: MessageKind::Ask,
                    urgency: MessageUrgency::Normal,
                    durable: true,
                    subject,
                    body,
                    reply_to: None,
                    thread_id: None,
                    deadline_at,
                    at: now,
                },
            )?;
            if let Some(secs) = timeout_secs {
                let reply =
                    wait_for_reply(env, &ask_msg.id, &sender_name, Duration::from_secs(secs))?;
                Ok(serde_json::to_string_pretty(&json!({
                    "ask": ask_msg,
                    "receipts": receipts,
                    "reply": reply,
                }))
                .unwrap())
            } else {
                Ok(serde_json::to_string_pretty(&json!({
                    "ask": ask_msg,
                    "receipts": receipts,
                }))
                .unwrap())
            }
        }
        "reply_message" => {
            let parent_id = parse_message_id(req_str(args, "message_id")?)?;
            let body = req_str(args, "body")?.to_string();
            let subject = args
                .get("subject")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            let from_agent = args.get("from_agent").and_then(Value::as_str);
            let sender_name = resolve_caller(env, from_agent, None);
            let (msg, receipts) = execute_reply(env, &parent_id, subject, body, sender_name)?;
            Ok(serde_json::to_string_pretty(&json!({
                "status": "replied",
                "message": msg,
                "receipts": receipts,
            }))
            .unwrap())
        }
        "get_messages" => {
            let unread_only = args
                .get("unread_only")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let ack = args.get("ack").and_then(Value::as_bool).unwrap_or(false);
            let agent = args.get("agent").and_then(Value::as_str);
            let recipient = resolve_caller(env, agent, None);
            let thread_id = args
                .get("thread_id")
                .and_then(Value::as_str)
                .map(parse_message_id)
                .transpose()?;
            let items = execute_read(
                env,
                WireFilter {
                    recipient: Some(recipient),
                    unread_only,
                    thread_id,
                    correlation_id: None,
                    limit: None,
                },
                ack,
            )?;
            let out: Vec<Value> = items.iter().map(inbox_item_to_json).collect();
            Ok(serde_json::to_string_pretty(&out).unwrap())
        }
        "ack_message" => {
            let mid = parse_message_id(req_str(args, "message_id")?)?;
            let agent = args.get("agent").and_then(Value::as_str);
            let recipient = resolve_caller(env, agent, None);
            let receipts = execute_ack(env, vec![mid], recipient)?;
            Ok(serde_json::to_string_pretty(&json!({
                "status": "acknowledged",
                "receipts": receipts,
            }))
            .unwrap())
        }
        "workflow_list" => {
            let project_raw = args.get("project").and_then(Value::as_str);
            let store = open_workflow_store(env, project_raw, None)?;
            let proj_filter = resolve_optional_project_filter(&store, project_raw)?;
            let defs = store
                .list_workflow_defs(proj_filter.as_ref())
                .map_err(|e| e.to_string())?;
            let runs = store
                .list_workflow_runs(proj_filter.as_ref(), None)
                .map_err(|e| e.to_string())?;
            let workflows_json: Vec<Value> = defs
                .iter()
                .map(|d| {
                    let mut v = serde_json::to_value(d).unwrap();
                    if let Ok(Some(h)) = store.handle_of(Kind::Workflow, d.id.iri())
                        && let Some(obj) = v.as_object_mut()
                    {
                        obj.insert("handle".into(), Value::String(h.to_string()));
                    }
                    v
                })
                .collect();
            let runs_json: Vec<Value> = runs
                .iter()
                .map(|r| {
                    let mut v = serde_json::to_value(r).unwrap();
                    if let Ok(Some(h)) = store.handle_of(Kind::WorkflowRun, r.id.iri())
                        && let Some(obj) = v.as_object_mut()
                    {
                        obj.insert("handle".into(), Value::String(h.to_string()));
                    }
                    v
                })
                .collect();
            Ok(serde_json::to_string_pretty(&json!({
                "workflows": workflows_json,
                "runs": runs_json,
            }))
            .unwrap())
        }
        "workflow_status" => {
            let raw_run = req_str(args, "run_id")?;
            let store = open_workflow_store(env, None, Some(raw_run))?;
            let run = resolve_workflow_run_in_store(&store, raw_run)?;
            let def = store
                .get_workflow_def(&run.workflow_id)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("workflow `{}` not found", run.workflow_id))?;
            let handoffs = list_workflow_handoffs(&store, &run.id).map_err(|e| e.to_string())?;
            let facts = list_workflow_facts(&store, &run.id).map_err(|e| e.to_string())?;
            Ok(serde_json::to_string_pretty(&json!({
                "run": run,
                "workflow": def,
                "handoffs": handoffs,
                "facts": facts,
            }))
            .unwrap())
        }
        "workflow_handoff" => {
            let raw_run = req_str(args, "run_id")?;
            let step_id = req_str(args, "step_id")?;
            let summary = req_str(args, "summary")?.to_string();
            let by = req_str(args, "by")?.to_string();
            let store = open_workflow_store(env, None, Some(raw_run))?;
            let run = resolve_workflow_run_in_store(&store, raw_run)?;
            let def = store
                .get_workflow_def(&run.workflow_id)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("workflow `{}` not found", run.workflow_id))?;
            if !def.steps.iter().any(|s| s.step_id == step_id) {
                return Err(WorkflowError::UnknownStep(step_id.to_string()).to_string());
            }

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

            let now = now_at();
            let mut written_facts = Vec::new();
            if let Some(facts_val) = args.get("facts")
                && !facts_val.is_null()
            {
                let Some(facts_obj) = facts_val.as_object() else {
                    return Err(
                        "`facts` must be a JSON object mapping declared keys to values".into(),
                    );
                };
                for key in facts_obj.keys() {
                    if !def.declared_keys.contains_key(key) {
                        return Err(WorkflowError::UndeclaredWorkflowKey {
                            workflow: def.slug.clone(),
                            key: key.clone(),
                        }
                        .to_string());
                    }
                }
                for (key, val) in facts_obj {
                    let fact_entry =
                        write_workflow_fact(&store, &def, &run, key, val.clone(), now.clone(), &by)
                            .map_err(|e| e.to_string())?;
                    written_facts.push(fact_entry);
                }
            }

            let visit_count = run
                .steps
                .get(step_id)
                .map(|s| s.visit_count.max(1))
                .unwrap_or(1);
            let handoff_entry = record_step_handoff(
                &store,
                &run.id,
                step_id,
                visit_count,
                HandoffEntry {
                    summary,
                    shipped,
                    in_flight,
                    blockers,
                    next_steps,
                    notes,
                },
                now,
                &by,
            )
            .map_err(|e| e.to_string())?;

            Ok(serde_json::to_string_pretty(&json!({
                "handoff": handoff_entry,
                "facts": written_facts,
            }))
            .unwrap())
        }
        "workflow_step" => {
            let raw_run = req_str(args, "run_id")?;
            let step_id = req_str(args, "step_id")?.to_string();
            let outcome_raw = req_str(args, "outcome")?;
            let outcome = StepOutcome::from_wire(outcome_raw).ok_or_else(|| {
                format!(
                    "unknown step outcome `{outcome_raw}`; expected one of: {}",
                    StepOutcome::wire_values()
                )
            })?;
            let by = req_str(args, "by")?.to_string();
            let summary = args
                .get("summary")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            let reason = args
                .get("reason")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            let tokens_used = args.get("tokens_used").and_then(Value::as_u64).unwrap_or(0);
            let cost_usd_micros = args
                .get("cost_usd_micros")
                .and_then(Value::as_u64)
                .unwrap_or(0);

            let store = open_workflow_store(env, None, Some(raw_run))?;
            let mut run = resolve_workflow_run_in_store(&store, raw_run)?;
            let def = store
                .get_workflow_def(&run.workflow_id)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("workflow `{}` not found", run.workflow_id))?;

            let now = now_at();
            let visit_count = run
                .steps
                .get(&step_id)
                .map(|s| s.visit_count.max(1))
                .unwrap_or(1);

            if summary.is_some() || reason.is_some() {
                let handoff_summary = summary
                    .clone()
                    .or_else(|| reason.clone())
                    .unwrap_or_else(|| format!("Step `{step_id}` {}", outcome.as_wire()));
                let blockers = match (&reason, outcome) {
                    (Some(r), StepOutcome::Fail | StepOutcome::NeedsHuman) => vec![r.clone()],
                    _ => Vec::new(),
                };
                record_step_handoff(
                    &store,
                    &run.id,
                    &step_id,
                    visit_count,
                    HandoffEntry {
                        summary: handoff_summary,
                        shipped: Vec::new(),
                        in_flight: Vec::new(),
                        blockers,
                        next_steps: Vec::new(),
                        notes: None,
                    },
                    now.clone(),
                    &by,
                )
                .map_err(|e| e.to_string())?;
            }

            advance_workflow(
                &def,
                &mut run,
                StepAttemptReport {
                    step_id,
                    outcome,
                    attempt_iri: None,
                    tokens_used,
                    cost_usd_micros,
                    reason: reason.or(summary),
                },
                now,
            )
            .map_err(|e| e.to_string())?;
            run.by = by;

            let updated = store.update_workflow_run(run).map_err(|e| e.to_string())?;
            Ok(serde_json::to_string_pretty(&json!({
                "run": updated,
            }))
            .unwrap())
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
            "name": "fl_state_checkpoint",
            "description": "Record an automatic git session checkpoint (branch, HEAD, dirty files) as a Handoff state entry. Returns {entry, branch, head_sha, dirty_files}.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "scope": { "type": "string" },
                    "slug": { "type": "string" },
                    "summary": { "type": "string" },
                    "by": { "type": "string" }
                }
            }
        },
        {
            "name": "fl_memory_search",
            "description": "Search state entries with BM25 ranking. Returns {query, hits, total_hits} where total_hits is the full ranked count before limit truncation.",
            "inputSchema": {
                "type": "object",
                "required": ["query"],
                "properties": {
                    "query": { "type": "string" },
                    "scope": { "type": "string" },
                    "concern": { "type": "string" },
                    "limit": { "type": "number" }
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
        },
        {
            "name": "send_message",
            "description": "Send a FerroWire message to an agent persona, AgentId URN, or group selector (@all, @area:<slug>, @role:<role>, @area:<slug>/<role>).",
            "inputSchema": {
                "type": "object",
                "required": ["to", "subject", "body"],
                "properties": {
                    "to": { "type": "string" },
                    "recipient": { "type": "string" },
                    "subject": { "type": "string" },
                    "body": { "type": "string" },
                    "kind": { "type": "string" },
                    "urgent": { "type": "boolean" },
                    "durable": { "type": "boolean" },
                    "thread_id": { "type": "string" },
                    "from_agent": { "type": "string" }
                }
            }
        },
        {
            "name": "broadcast_message",
            "description": "Broadcast a FerroWire message to all active agents (@all), excluding the sender.",
            "inputSchema": {
                "type": "object",
                "required": ["subject", "body"],
                "properties": {
                    "subject": { "type": "string" },
                    "body": { "type": "string" },
                    "urgent": { "type": "boolean" },
                    "durable": { "type": "boolean" },
                    "from_agent": { "type": "string" }
                }
            }
        },
        {
            "name": "ask_message",
            "description": "Send a durable Ask message and optionally wait for a correlated Reply.",
            "inputSchema": {
                "type": "object",
                "required": ["to", "subject", "body"],
                "properties": {
                    "to": { "type": "string" },
                    "subject": { "type": "string" },
                    "body": { "type": "string" },
                    "timeout_secs": { "type": "integer" },
                    "from_agent": { "type": "string" }
                }
            }
        },
        {
            "name": "reply_message",
            "description": "Reply to a FerroWire message by MessageId, inheriting its thread and correlation ID.",
            "inputSchema": {
                "type": "object",
                "required": ["message_id", "body"],
                "properties": {
                    "message_id": { "type": "string" },
                    "body": { "type": "string" },
                    "subject": { "type": "string" },
                    "from_agent": { "type": "string" }
                }
            }
        },
        {
            "name": "get_messages",
            "description": "Read FerroWire inbox messages for the caller or specified agent.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "unread_only": { "type": "boolean", "default": true },
                    "ack": { "type": "boolean", "default": false },
                    "agent": { "type": "string" },
                    "thread_id": { "type": "string" }
                }
            }
        },
        {
            "name": "ack_message",
            "description": "Acknowledge a FerroWire message by MessageId.",
            "inputSchema": {
                "type": "object",
                "required": ["message_id"],
                "properties": {
                    "message_id": { "type": "string" },
                    "agent": { "type": "string" }
                }
            }
        },
        {
            "name": "workflow_list",
            "description": "List workflow definitions and workflow runs.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": { "type": "string" }
                }
            }
        },
        {
            "name": "workflow_status",
            "description": "Return a WorkflowRun's full JSON state plus curated HandoffEntry and RuntimeEntry records.",
            "inputSchema": {
                "type": "object",
                "required": ["run_id"],
                "properties": {
                    "run_id": { "type": "string" }
                }
            }
        },
        {
            "name": "workflow_handoff",
            "description": "Record a curated HandoffEntry and optional declared workflow runtime facts for a step.",
            "inputSchema": {
                "type": "object",
                "required": ["run_id", "step_id", "summary", "by"],
                "properties": {
                    "run_id": { "type": "string" },
                    "step_id": { "type": "string" },
                    "summary": { "type": "string" },
                    "shipped": { "type": "array", "items": { "type": "string" } },
                    "in_flight": { "type": "array", "items": { "type": "string" } },
                    "blockers": { "type": "array", "items": { "type": "string" } },
                    "next_steps": { "type": "array", "items": { "type": "string" } },
                    "notes": { "type": "string" },
                    "facts": { "type": "object" },
                    "by": { "type": "string" }
                }
            }
        },
        {
            "name": "workflow_step",
            "description": "Record a step completion outcome and advance a ready step in a WorkflowRun.",
            "inputSchema": {
                "type": "object",
                "required": ["run_id", "step_id", "outcome", "by"],
                "properties": {
                    "run_id": { "type": "string" },
                    "step_id": { "type": "string" },
                    "outcome": { "type": "string" },
                    "summary": { "type": "string" },
                    "reason": { "type": "string" },
                    "tokens_used": { "type": "integer" },
                    "cost_usd_micros": { "type": "integer" },
                    "by": { "type": "string" }
                }
            }
        }
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::MemStore;

    fn seeded_entries() -> Vec<StateEntry> {
        let store = MemStore::default();
        let weak = store
            .upsert_state(UpsertStateArgs {
                scope: StateScope::Workspace,
                slug: "weak-note".into(),
                sensitivity: SensitivityPatch::Inherit,
                payload: StatePayload::Memory(MemoryEntry {
                    category: MemoryCategory::Note,
                    basis: MemoryBasis::Inference,
                    lifecycle: MemoryLifecycle::Active,
                    superseded_by: None,
                    pinned: false,
                    summary: "ranking mention".into(),
                    body: "ranking appears once here".into(),
                }),
                at: now_at(),
                by: "ferris".into(),
            })
            .unwrap();
        let strong = store
            .upsert_state(UpsertStateArgs {
                scope: StateScope::Workspace,
                slug: "strong-note".into(),
                sensitivity: SensitivityPatch::Inherit,
                payload: StatePayload::Memory(MemoryEntry {
                    category: MemoryCategory::Note,
                    basis: MemoryBasis::Inference,
                    lifecycle: MemoryLifecycle::Active,
                    superseded_by: None,
                    pinned: false,
                    summary: "search design".into(),
                    body: "bm25 ranking search ranking bm25 ranking".into(),
                }),
                at: now_at(),
                by: "ferris".into(),
            })
            .unwrap();
        vec![weak, strong]
    }

    #[test]
    fn fl_memory_search_returns_ranked_json_shape() {
        let entries = seeded_entries();
        let args = json!({ "query": "bm25 ranking", "limit": 1 });
        let (query, scope, concern, limit) = parse_search_args(&args).unwrap();
        let text = memory_search_json(&entries, &query, scope.as_ref(), concern, limit);
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["query"], "bm25 ranking");
        assert_eq!(value["total_hits"], 2);
        assert_eq!(value["hits"].as_array().unwrap().len(), 1);
        assert_eq!(value["hits"][0]["entry"]["slug"], "strong-note");
        assert!(value["hits"][0]["score_millis"].as_u64().unwrap() > 0);

        // Tool is advertised.
        let defs = tool_definitions();
        assert!(
            defs.as_array()
                .unwrap()
                .iter()
                .any(|d| d["name"] == "fl_memory_search"),
            "fl_memory_search missing from tool_definitions"
        );
    }

    #[test]
    fn fl_memory_search_rejects_invalid_types_and_values() {
        assert!(parse_search_args(&json!({})).is_err(), "missing query");
        assert!(
            parse_search_args(&json!({ "query": "x", "scope": 3 })).is_err(),
            "non-string scope"
        );
        assert!(
            parse_search_args(&json!({ "query": "x", "scope": "bogus:1" })).is_err(),
            "invalid scope value"
        );
        assert!(
            parse_search_args(&json!({ "query": "x", "concern": true })).is_err(),
            "non-string concern"
        );
        assert!(
            parse_search_args(&json!({ "query": "x", "concern": "bogus" })).is_err(),
            "invalid concern value"
        );
        assert!(
            parse_search_args(&json!({ "query": "x", "limit": "5" })).is_err(),
            "string limit"
        );
        assert!(
            parse_search_args(&json!({ "query": "x", "limit": -1 })).is_err(),
            "negative limit"
        );
        assert!(
            parse_search_args(&json!({ "query": "x", "limit": 1.5 })).is_err(),
            "fractional limit"
        );
        let (_, _, _, default_limit) = parse_search_args(&json!({ "query": "x" })).unwrap();
        assert_eq!(default_limit, 10);
        let (_, _, _, capped) = parse_search_args(&json!({ "query": "x", "limit": 500 })).unwrap();
        assert_eq!(capped, 100);
        let (_, _, _, zero) = parse_search_args(&json!({ "query": "x", "limit": 0 })).unwrap();
        assert_eq!(zero, 0);
    }

    #[test]
    fn fl_state_checkpoint_args_default_and_reject_bad_types() {
        let parsed = parse_checkpoint_args(&json!({})).unwrap();
        assert!(parsed.scope.is_none());
        assert!(parsed.slug.is_none());
        assert!(parsed.summary.is_none());
        assert_eq!(parsed.by, "fl");

        let parsed = parse_checkpoint_args(
            &json!({ "scope": "workspace", "slug": "snap-1", "summary": "hi", "by": "tester" }),
        )
        .unwrap();
        assert_eq!(parsed.scope, Some(StateScope::Workspace));
        assert_eq!(parsed.slug.as_deref(), Some("snap-1"));
        assert_eq!(parsed.summary.as_deref(), Some("hi"));
        assert_eq!(parsed.by, "tester");

        for bad in [
            json!({ "scope": 3 }),
            json!({ "scope": "bogus:1" }),
            json!({ "slug": 4 }),
            json!({ "slug": "1bad slug!" }),
            json!({ "summary": true }),
            json!({ "by": 5 }),
            json!(["nope"]),
        ] {
            assert!(parse_checkpoint_args(&bad).is_err(), "{bad}");
        }

        assert!(
            tool_definitions()
                .as_array()
                .unwrap()
                .iter()
                .any(|d| d["name"] == "fl_state_checkpoint"),
            "fl_state_checkpoint missing from tool_definitions"
        );
    }
}
