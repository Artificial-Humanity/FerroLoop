//! `POST /api/v1/agents/:name/recast`: the Owner moves an existing agent to another project or
//! role.
//!
//! Recast changes only the registry record. It does not touch tmux sessions, routes,
//! `PERSONA.md` or wire state. The rules:
//!
//! - Only Owner authority may recast. Any other caller gets 403.
//! - The agent must exist (404) and be `active` (409 "reactivate it first"). The store's
//!   `register_agent` always writes `active`, so recasting a retired agent would reactivate it.
//! - `display_name`, `email` and `id` stay the same. `extra_areas`, when omitted, stay the same.
//! - A missing area is a 409 with `missing_areas` unless `create_area` is `true`.
//! - To make the agent the Lead of a project that has another active Lead, the body must set
//!   `replace_lead: true` (else 409 with `current_lead`). That Lead is then retired, and the
//!   response names it. When both conflicts apply, one 409 carries both `current_lead` and
//!   `missing_areas`, and nothing is written.
//! - A 200 carries `agent`, `retires_lead` (the retired Lead's display name or `null`),
//!   `creates_areas` (the areas this call created) and, when a Lead was retired, the full
//!   `retired_lead` record. The `agents:recast` event carries the same fields. A recast that
//!   does not make the agent a Lead never reports a retired Lead.
//! - Recasting a project's only Lead away leaves the project with no Lead. PR policy then falls
//!   back to the LabManager tier.
//! - Recast does not enforce LabManager uniqueness. The store has no such rule, and PR policy
//!   uses the first active `lab_manager`.
//!
//! The checks and the write use one store guard. The guard holds the hub's `store_lock`, so no
//! other hub request can write between the checks and the write. A separate process (the `fl`
//! CLI) can still write to a FerroStore file in that gap. For that case the response reports the
//! Lead that the write actually retired, read back after the write, not the one the checks saw.
//! The read-back runs only for a Lead recast. It checks the Lead that sat in the project before
//! the write (by id). Only when no Lead sat there does it fall back to the store's mark for this
//! call's `at` and `by`.

use fl_core::agent::{AgentRecord, AgentRole, AgentStatus, RegisterAgentArgs};
use fl_core::at::At;
use fl_core::ids::AgentId;
use fl_core::state::validate_slug;
use fl_core::store::{AgentRegistry, StoreError};
use fl_exec::stamp;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::api::{HttpRequest, HttpResponse, percent_decode};
use crate::auth::HubAuthority;
use crate::{HubState, HubStoreGuard};

fn error_json(status: u16, message: &str) -> HttpResponse {
    HttpResponse::json(status, json!({ "error": message }))
}

/// The recast body. Unknown fields are refused, so a field this endpoint does not support
/// (for example `dry_run`) is a 400, never a silent real write.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecastBody {
    project: String,
    role: AgentRole,
    #[serde(default)]
    extra_areas: Option<Vec<String>>,
    #[serde(default)]
    create_area: bool,
    #[serde(default)]
    replace_lead: bool,
}

fn placement(agent: &AgentRecord) -> Value {
    json!({
        "project": agent.project,
        "role": agent.role,
        "extra_areas": agent.extra_areas,
    })
}

fn store_error(err: &StoreError) -> HttpResponse {
    match err {
        StoreError::NoSuchArea(area) => error_json(404, &format!("no such area: {area}")),
        StoreError::LeadTaken { .. } | StoreError::AgentCollision { .. } => {
            error_json(409, &err.to_string())
        }
        StoreError::InvalidState(msg) => error_json(400, msg),
        other => error_json(500, &other.to_string()),
    }
}

/// `POST /api/v1/agents/:name/recast`. See the module documentation for the rules.
pub(crate) fn handle_post_agent_recast(
    state: &HubState,
    authority: &HubAuthority,
    req: &HttpRequest,
    raw_name: &str,
) -> HttpResponse {
    if !authority.is_owner() {
        return error_json(403, "only Owner authority may recast agents");
    }
    let Ok(name_key) = validate_slug(&percent_decode(raw_name)) else {
        return error_json(400, "invalid agent name");
    };
    if req.body.iter().all(u8::is_ascii_whitespace) {
        return error_json(400, "body must be a JSON object");
    }
    let body: RecastBody = match serde_json::from_slice(&req.body) {
        Ok(body) => body,
        Err(e) => return error_json(400, &format!("invalid recast body: {e}")),
    };
    let Ok(project) = validate_slug(body.project.trim()) else {
        return error_json(400, &format!("invalid project `{}`", body.project));
    };
    let extra_areas = match body.extra_areas.as_ref() {
        None => None,
        Some(areas) => {
            let mut out = Vec::with_capacity(areas.len());
            for area in areas {
                match validate_slug(area.trim()) {
                    Ok(key) => out.push(key),
                    Err(_) => return error_json(400, &format!("invalid extra area `{area}`")),
                }
            }
            out.sort();
            out.dedup();
            Some(out)
        }
    };

    let Ok(store) = state.open_store() else {
        return error_json(500, "failed to open store: store unavailable");
    };

    let existing = match store.agent_by_name(&name_key) {
        Ok(Some(agent)) => agent,
        Ok(None) => return error_json(404, &format!("no agent named `{name_key}`")),
        Err(e) => return store_error(&e),
    };
    if existing.status != AgentStatus::Active {
        return HttpResponse::json(
            409,
            json!({
                "error": format!(
                    "agent `{}` is {}; reactivate it first",
                    existing.display_name,
                    existing.status.as_wire()
                ),
                "status": existing.status,
            }),
        );
    }
    let extra_areas = extra_areas.unwrap_or_else(|| existing.extra_areas.clone());

    // Areas: check before any write.
    let mut missing = Vec::new();
    for area in std::iter::once(&project).chain(extra_areas.iter()) {
        match store.get_area(area) {
            Ok(Some(_)) => {}
            Ok(None) => {
                if !missing.contains(area) {
                    missing.push(area.clone());
                }
            }
            Err(e) => return store_error(&e),
        }
    }
    let areas_unconfirmed = !missing.is_empty() && !body.create_area;

    // Lead: `register_agent` with `replace: true` retires the sitting Lead without asking, so
    // the caller must confirm with `replace_lead`.
    // The active Lead that sits in the target project now (the agent itself excluded). A Lead
    // recast needs it for the conflict check and, after the write, to name the retired Lead.
    let mut lead_before = None;
    if body.role == AgentRole::Lead {
        let agents = match store.list_agents(None, false) {
            Ok(agents) => agents,
            Err(e) => return store_error(&e),
        };
        lead_before = agents.into_iter().find(|a| {
            a.status == AgentStatus::Active
                && a.project == project
                && a.role == AgentRole::Lead
                && a.name_key != existing.name_key
        });
    }
    let sitting_lead = lead_before.as_ref().filter(|_| !body.replace_lead);

    // One 409 lists every unconfirmed conflict, so a single confirm resolves them all. Nothing
    // is written.
    if areas_unconfirmed || sitting_lead.is_some() {
        let mut messages = Vec::new();
        let mut payload = serde_json::Map::new();
        if let Some(lead) = sitting_lead {
            messages.push(format!(
                "project `{project}` already has an active Lead: {} ({}); \
                 pass replace_lead=true to confirm",
                lead.display_name,
                lead.id.as_str()
            ));
            payload.insert(
                "current_lead".into(),
                json!({ "display_name": lead.display_name, "id": lead.id }),
            );
        }
        if areas_unconfirmed {
            messages.push(format!(
                "no such area: {}; pass create_area=true to confirm",
                missing.join(", ")
            ));
            payload.insert("missing_areas".into(), json!(missing));
        }
        payload.insert("error".into(), json!(messages.join("; ")));
        return HttpResponse::json(409, Value::Object(payload));
    }

    let at = stamp::now();
    let by = authority.actor_name().to_string();
    let updated = match store.register_agent(RegisterAgentArgs {
        display_name: existing.display_name.clone(),
        email: Some(existing.email.clone()),
        project: project.clone(),
        role: body.role.clone(),
        extra_areas,
        create_area: body.create_area,
        replace: true,
        at: at.clone(),
        by: by.clone(),
    }) {
        Ok(record) => record,
        Err(e) => return store_error(&e),
    };

    let retired_lead = match retired_lead_after(
        &store,
        &body.role,
        lead_before.as_ref().map(|lead| &lead.id),
        &updated,
        &at,
        &by,
    ) {
        Ok(lead) => lead,
        Err(e) => return store_error(&e),
    };
    drop(store);

    let mut event = json!({
        "agent": updated,
        "from": placement(&existing),
        "to": placement(&updated),
    });
    // `retires_lead` and `creates_areas` are the names the UI reads; `retired_lead` is the full
    // record. `missing` is non-empty here only when `create_area` was `true`.
    let retires_lead = retired_lead.as_ref().map(|lead| lead.display_name.clone());
    let creates_areas = missing;
    let mut response = json!({
        "agent": updated,
        "retires_lead": retires_lead,
        "creates_areas": creates_areas,
    });
    event["retires_lead"] = json!(retires_lead);
    event["creates_areas"] = json!(creates_areas);
    if let Some(lead) = retired_lead {
        event["retired_lead"] = json!(lead);
        response["retired_lead"] = json!(lead);
    }
    state.broadcast("agents", "recast", event);
    HttpResponse::json(200, response)
}

/// The Lead that a recast write retired, read back from the store.
///
/// Only a Lead recast can retire a Lead, so any other role returns `None` at once. This stops one
/// call from reporting a retirement that another call made in the same millisecond. When a Lead
/// sat in the project before the write, that Lead is re-read by id and reported only if it is now
/// retired. When none sat there, a Lead that is retired now with this call's `at` and `by` is the
/// result of a write by another process between the checks and the write.
fn retired_lead_after(
    store: &HubStoreGuard<'_>,
    new_role: &AgentRole,
    lead_before: Option<&AgentId>,
    updated: &AgentRecord,
    at: &At,
    by: &str,
) -> Result<Option<AgentRecord>, StoreError> {
    if *new_role != AgentRole::Lead {
        return Ok(None);
    }
    if let Some(id) = lead_before {
        return Ok(store
            .get_agent(id)?
            .filter(|lead| lead.status == AgentStatus::Retired));
    }
    Ok(store.list_agents(None, true)?.into_iter().find(|a| {
        a.status == AgentStatus::Retired
            && a.role == AgentRole::Lead
            && a.project == updated.project
            && a.name_key != updated.name_key
            && a.updated_at == *at
            && a.by == by
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::handle_http_request;
    use crate::{HubServerConfig, HubWsFrame};
    use fl_core::agent::AreaRecord;
    use tempfile::TempDir;
    use tokio::sync::broadcast::Receiver;

    fn test_state(tmp: &TempDir) -> HubState {
        HubState::new(HubServerConfig {
            listen: "127.0.0.1:0".into(),
            store_path: tmp.path().join("store.redb"),
            repo_root: tmp.path().to_path_buf(),
            ui_dir: None,
            owner_login: "lmcfarlin".into(),
            reviewer_apps: fl_core::pr_policy::ReviewerApps::default(),
            owner_token: Some("owner-tok-secret".into()),
            peer_token: Some("peer-tok-secret".into()),
            trust_tailscale_headers: false,
            enable_terminal: false,
            session_ttl_secs: 3600,
            home_override: Some(tmp.path().to_path_buf()),
        })
    }

    fn register(state: &HubState, name: &str, project: &str, role: AgentRole) -> AgentRecord {
        state
            .open_store()
            .unwrap()
            .register_agent(RegisterAgentArgs {
                display_name: name.into(),
                email: Some(format!("{}@example.org", name.to_ascii_lowercase())),
                project: project.into(),
                role,
                extra_areas: vec!["notes".into()],
                create_area: true,
                replace: false,
                at: stamp::now(),
                by: "owner".into(),
            })
            .unwrap()
    }

    fn agent(state: &HubState, name: &str) -> Option<AgentRecord> {
        state.open_store().unwrap().agent_by_name(name).unwrap()
    }

    fn area(state: &HubState, key: &str) -> Option<AreaRecord> {
        state.open_store().unwrap().get_area(key).unwrap()
    }

    fn recast(state: &HubState, token: &str, name: &str, body: &Value) -> (u16, Value) {
        let req = HttpRequest::new("POST", format!("/api/v1/agents/{name}/recast"))
            .with_header("Authorization", format!("Bearer {token}"))
            .with_json_body(body);
        let resp = handle_http_request(state, &req);
        (resp.status, resp.json_value().unwrap_or(Value::Null))
    }

    fn owner_recast(state: &HubState, name: &str, body: &Value) -> (u16, Value) {
        recast(state, "owner-tok-secret", name, body)
    }

    fn recast_frames(frames: &mut Receiver<HubWsFrame>) -> Vec<HubWsFrame> {
        let mut out = Vec::new();
        while let Ok(frame) = frames.try_recv() {
            if frame.channel == "agents" && frame.event == "recast" {
                out.push(frame);
            }
        }
        out
    }

    #[test]
    fn owner_recasts_an_assistant_to_lead_of_a_project_with_no_lead() {
        let tmp = TempDir::new().unwrap();
        let state = test_state(&tmp);
        let before = register(&state, "Ada", "ferroloop", AgentRole::Assistant);
        register(&state, "Nova", "sonora", AgentRole::Assistant);
        let mut frames = state.subscribe();

        let (status, body) = owner_recast(
            &state,
            "ada",
            &json!({ "project": "sonora", "role": "lead" }),
        );
        assert_eq!(status, 200, "{body}");
        assert!(body.get("retired_lead").is_none());
        assert_eq!(body["agent"]["project"], "sonora");
        assert_eq!(body["agent"]["role"], "lead");

        let after = agent(&state, "ada").unwrap();
        assert_eq!(after.id, before.id);
        assert_eq!(after.display_name, before.display_name);
        assert_eq!(after.email, before.email);
        assert_eq!(after.status, AgentStatus::Active);
        assert_eq!(after.project, "sonora");
        assert_eq!(after.role, AgentRole::Lead);
        // `extra_areas` omitted: kept.
        assert_eq!(after.extra_areas, before.extra_areas);
        assert_eq!(after.by, "lmcfarlin");

        let sent = recast_frames(&mut frames);
        assert_eq!(sent.len(), 1);
        let data = &sent[0].data;
        assert_eq!(data["agent"]["name_key"], "ada");
        assert_eq!(
            data["from"],
            json!({ "project": "ferroloop", "role": "assistant", "extra_areas": ["notes"] })
        );
        assert_eq!(
            data["to"],
            json!({ "project": "sonora", "role": "lead", "extra_areas": ["notes"] })
        );
        assert!(data.get("retired_lead").is_none());
        assert_eq!(data["retires_lead"], Value::Null);
        assert_eq!(data["creates_areas"], json!([]));
        assert_eq!(body["retires_lead"], Value::Null);
        assert_eq!(body["creates_areas"], json!([]));
    }

    #[test]
    fn a_non_owner_gets_403_and_nothing_changes() {
        let tmp = TempDir::new().unwrap();
        let state = test_state(&tmp);
        let before = register(&state, "Ada", "ferroloop", AgentRole::Assistant);
        let mut frames = state.subscribe();

        let (status, body) = recast(
            &state,
            "peer-tok-secret",
            "ada",
            &json!({ "project": "ferroloop", "role": "lead" }),
        );
        assert_eq!(status, 403);
        assert_eq!(body["error"], "only Owner authority may recast agents");
        assert_eq!(agent(&state, "ada").unwrap(), before);
        assert!(recast_frames(&mut frames).is_empty());
    }

    #[test]
    fn an_unknown_agent_is_404_and_no_agent_is_created() {
        let tmp = TempDir::new().unwrap();
        let state = test_state(&tmp);
        register(&state, "Ada", "ferroloop", AgentRole::Assistant);

        let (status, body) = owner_recast(
            &state,
            "ghost",
            &json!({ "project": "ferroloop", "role": "assistant", "create_area": true }),
        );
        assert_eq!(status, 404, "{body}");
        assert!(body["error"].as_str().unwrap().contains("ghost"));
        assert!(agent(&state, "ghost").is_none());
        let all = state.open_store().unwrap().list_agents(None, true).unwrap();
        assert_eq!(all.len(), 1);
    }

    #[test]
    fn a_missing_area_is_409_without_create_area_and_created_with_it() {
        let tmp = TempDir::new().unwrap();
        let state = test_state(&tmp);
        let before = register(&state, "Ada", "ferroloop", AgentRole::Assistant);

        let (status, body) = owner_recast(
            &state,
            "ada",
            &json!({ "project": "prosodia", "role": "assistant", "extra_areas": ["lexicon"] }),
        );
        assert_eq!(status, 409, "{body}");
        assert_eq!(body["missing_areas"], json!(["prosodia", "lexicon"]));
        let error = body["error"].as_str().unwrap();
        assert!(
            error.contains("prosodia") && error.contains("lexicon"),
            "{error}"
        );
        assert!(
            error.contains("pass create_area=true to confirm"),
            "{error}"
        );
        assert!(body.get("current_lead").is_none());
        assert!(area(&state, "prosodia").is_none());
        assert!(area(&state, "lexicon").is_none());
        assert_eq!(agent(&state, "ada").unwrap(), before);

        let (status, body) = owner_recast(
            &state,
            "ada",
            &json!({
                "project": "prosodia", "role": "assistant",
                "extra_areas": ["lexicon"], "create_area": true,
            }),
        );
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["creates_areas"], json!(["prosodia", "lexicon"]));
        assert_eq!(body["retires_lead"], Value::Null);
        assert!(area(&state, "prosodia").is_some());
        assert!(area(&state, "lexicon").is_some());
        let after = agent(&state, "ada").unwrap();
        assert_eq!(after.project, "prosodia");
        assert_eq!(after.extra_areas, vec!["lexicon".to_string()]);
        assert_eq!(after.id, before.id);
    }

    #[test]
    fn a_sitting_lead_needs_replace_lead_and_is_then_retired_and_reported() {
        let tmp = TempDir::new().unwrap();
        let state = test_state(&tmp);
        let ferris = register(&state, "Ferris", "ferroloop", AgentRole::Lead);
        let felix = register(&state, "Felix", "ferroloop", AgentRole::Assistant);
        let mut frames = state.subscribe();

        let (status, body) = owner_recast(
            &state,
            "felix",
            &json!({ "project": "ferroloop", "role": "lead" }),
        );
        assert_eq!(status, 409, "{body}");
        let error = body["error"].as_str().unwrap();
        assert!(error.contains("Ferris"), "{error}");
        assert!(error.contains(ferris.id.as_str()), "{error}");
        assert!(
            error.contains("pass replace_lead=true to confirm"),
            "{error}"
        );
        assert!(body.get("missing_areas").is_none());
        assert_eq!(body["current_lead"]["display_name"], "Ferris");
        assert_eq!(body["current_lead"]["id"], json!(ferris.id));
        assert_eq!(agent(&state, "ferris").unwrap(), ferris);
        assert_eq!(agent(&state, "felix").unwrap(), felix);
        assert!(recast_frames(&mut frames).is_empty());

        let (status, body) = owner_recast(
            &state,
            "felix",
            &json!({ "project": "ferroloop", "role": "lead", "replace_lead": true }),
        );
        assert_eq!(status, 200, "{body}");
        let old = agent(&state, "ferris").unwrap();
        assert_eq!(old.status, AgentStatus::Retired);
        assert_eq!(body["retired_lead"]["name_key"], "ferris");
        assert_eq!(body["retired_lead"]["status"], "retired");
        assert_eq!(body["retires_lead"], "Ferris");
        assert_eq!(body["creates_areas"], json!([]));
        let new = agent(&state, "felix").unwrap();
        assert_eq!(new.role, AgentRole::Lead);
        assert_eq!(new.id, felix.id);

        let sent = recast_frames(&mut frames);
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].data["retired_lead"]["name_key"], "ferris");
        assert_eq!(sent[0].data["retires_lead"], "Ferris");
        assert_eq!(sent[0].data["creates_areas"], json!([]));
        assert_eq!(sent[0].data["from"]["role"], "assistant");
        assert_eq!(sent[0].data["to"]["role"], "lead");
    }

    #[test]
    fn a_non_lead_recast_never_reports_a_retirement_from_the_same_millisecond() {
        let tmp = TempDir::new().unwrap();
        let state = test_state(&tmp);
        let ferris = register(&state, "Ferris", "ferroloop", AgentRole::Lead);
        let ada = register(&state, "Ada", "ferroloop", AgentRole::Assistant);
        // Another call retires the Lead with the same `at` and `by` as this call would use.
        let at = stamp::now();
        let store = state.open_store().unwrap();
        store.retire_agent(&ferris.id, at.clone(), "owner").unwrap();

        for role in [AgentRole::Assistant, AgentRole::LabManager] {
            assert_eq!(
                retired_lead_after(&store, &role, None, &ada, &at, "owner").unwrap(),
                None,
                "{role:?}"
            );
            assert_eq!(
                retired_lead_after(&store, &role, Some(&ferris.id), &ada, &at, "owner").unwrap(),
                None,
                "{role:?}"
            );
        }
        drop(store);

        let (status, body) = owner_recast(
            &state,
            "ada",
            &json!({ "project": "ferroloop", "role": "lab_manager" }),
        );
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["retires_lead"], Value::Null);
        assert!(body.get("retired_lead").is_none());
    }

    #[test]
    fn a_lead_recast_reports_exactly_the_lead_that_sat_there() {
        let tmp = TempDir::new().unwrap();
        let state = test_state(&tmp);
        let ferris = register(&state, "Ferris", "ferroloop", AgentRole::Lead);
        let other = register(&state, "Olga", "other", AgentRole::Lead);
        let felix = register(&state, "Felix", "ferroloop", AgentRole::Assistant);
        let at = stamp::now();
        let store = state.open_store().unwrap();
        // A Lead of another project, retired in the same millisecond by another call.
        store.retire_agent(&other.id, at.clone(), "owner").unwrap();
        // The sitting Lead is still active: nothing to report.
        assert_eq!(
            retired_lead_after(
                &store,
                &AgentRole::Lead,
                Some(&ferris.id),
                &felix,
                &at,
                "owner"
            )
            .unwrap(),
            None
        );
        drop(store);

        let (status, body) = owner_recast(
            &state,
            "felix",
            &json!({ "project": "ferroloop", "role": "lead", "replace_lead": true }),
        );
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["retired_lead"]["id"], json!(ferris.id));
        assert_eq!(body["retires_lead"], "Ferris");
    }

    #[test]
    fn a_lead_recast_with_no_lead_before_reports_a_cross_process_retirement() {
        let tmp = TempDir::new().unwrap();
        let state = test_state(&tmp);
        let ferris = register(&state, "Ferris", "ferroloop", AgentRole::Lead);
        let felix = register(&state, "Felix", "ferroloop", AgentRole::Assistant);
        let at = stamp::now();
        let store = state.open_store().unwrap();
        let retired = store.retire_agent(&ferris.id, at.clone(), "owner").unwrap();
        assert_eq!(
            retired_lead_after(&store, &AgentRole::Lead, None, &felix, &at, "owner").unwrap(),
            Some(retired)
        );
    }

    #[test]
    fn a_lead_conflict_and_missing_areas_are_one_409_and_one_confirm_resolves_both() {
        let tmp = TempDir::new().unwrap();
        let state = test_state(&tmp);
        let ferris = register(&state, "Ferris", "ferroloop", AgentRole::Lead);
        let ada = register(&state, "Ada", "sonora", AgentRole::Assistant);
        let felix = register(&state, "Felix", "ferroloop", AgentRole::Assistant);
        let mut frames = state.subscribe();

        // Both conflicts: Ferris leads ferroloop, and `lexicon` does not exist.
        let both = json!({
            "project": "ferroloop", "role": "lead", "extra_areas": ["lexicon"],
            "replace_lead": false, "create_area": false,
        });
        let (status, body) = owner_recast(&state, "felix", &both);
        assert_eq!(status, 409, "{body}");
        assert_eq!(body["current_lead"]["display_name"], "Ferris");
        assert_eq!(body["current_lead"]["id"], json!(ferris.id));
        assert_eq!(body["missing_areas"], json!(["lexicon"]));
        let error = body["error"].as_str().unwrap();
        assert!(error.contains("Ferris"), "{error}");
        assert!(error.contains("lexicon"), "{error}");
        assert!(error.contains("replace_lead=true"), "{error}");
        assert!(error.contains("create_area=true"), "{error}");
        assert_eq!(agent(&state, "ferris").unwrap(), ferris);
        assert_eq!(agent(&state, "felix").unwrap(), felix);
        assert_eq!(agent(&state, "ada").unwrap(), ada);
        assert!(area(&state, "lexicon").is_none());
        assert!(recast_frames(&mut frames).is_empty());

        let (status, body) = owner_recast(
            &state,
            "felix",
            &json!({
                "project": "ferroloop", "role": "lead", "extra_areas": ["lexicon"],
                "replace_lead": true, "create_area": true,
            }),
        );
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["retires_lead"], "Ferris");
        assert_eq!(body["creates_areas"], json!(["lexicon"]));
        assert_eq!(body["retired_lead"]["name_key"], "ferris");
        assert!(area(&state, "lexicon").is_some());
        assert_eq!(
            agent(&state, "ferris").unwrap().status,
            AgentStatus::Retired
        );
        let sent = recast_frames(&mut frames);
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].data["retires_lead"], "Ferris");
        assert_eq!(sent[0].data["creates_areas"], json!(["lexicon"]));
    }

    #[test]
    fn a_retired_agent_is_409_and_stays_retired() {
        let tmp = TempDir::new().unwrap();
        let state = test_state(&tmp);
        let ada = register(&state, "Ada", "ferroloop", AgentRole::Assistant);
        let retired = state
            .open_store()
            .unwrap()
            .retire_agent(&ada.id, stamp::now(), "owner")
            .unwrap();

        let (status, body) = owner_recast(
            &state,
            "ada",
            &json!({ "project": "ferroloop", "role": "assistant" }),
        );
        assert_eq!(status, 409, "{body}");
        assert!(
            body["error"]
                .as_str()
                .unwrap()
                .contains("is retired; reactivate it first")
        );
        assert_eq!(agent(&state, "ada").unwrap(), retired);
    }

    #[test]
    fn a_bad_body_is_400_and_an_unsupported_field_is_refused() {
        let tmp = TempDir::new().unwrap();
        let state = test_state(&tmp);
        let before = register(&state, "Ada", "ferroloop", AgentRole::Assistant);

        for body in [
            json!({ "project": "ferroloop", "role": "leed" }),
            json!({ "role": "lead" }),
            json!({ "project": "ferroloop", "role": "lead", "dry_run": true }),
            json!({ "project": "Not A Slug", "role": "lead" }),
        ] {
            let (status, resp) = owner_recast(&state, "ada", &body);
            assert_eq!(status, 400, "{body} -> {resp}");
        }
        assert_eq!(agent(&state, "ada").unwrap(), before);
    }
}
