//! `GET /api/v1/projects`, `POST /api/v1/projects`, `PATCH /api/v1/projects/:slug` and
//! `DELETE /api/v1/projects/:slug`.
//!
//! One shape describes a project everywhere (`ProjectSummary` in [`crate::project_list`], plus
//! the artifacts-directory fields the Settings screen reads): the list, a creation and an
//! update all return it. Creating and updating are Owner only: they write the Owner's config
//! and may run `git init` in a directory. The flows are the ones `fl project` runs
//! ([`crate::project_init`], [`crate::project_list`]), in the Hub's home: the config is
//! `<home>/.config/fl/config.toml`, a project's store is `<home>/.local/share/fl/<slug>.fls`,
//! and the registry is the Hub's own store.

use std::path::PathBuf;

use fl_core::store::Catalog;
use serde_json::{Value, json};

use crate::api::{HttpRequest, HttpResponse, hub_home_dir, percent_decode};
use crate::auth::HubAuthority;
use crate::project_delete::{DeleteContext, DeleteError, DeleteRequest, delete_project};
use crate::project_init::{
    Credential, InitError, InitPaths, InitRequest, ProjectKind, VirtualRequest, canonical_root,
    create_virtual, init_project, user_config_path,
};
use crate::project_list::{ProjectSummary, UpdateRequest, list_projects, update_project};
use crate::{HubState, ProjectOpenError, artifact_dirs, records};

fn error_response(status: u16, message: &str) -> HttpResponse {
    HttpResponse::json(status, json!({ "error": message }))
}

fn init_error_response(e: &InitError) -> HttpResponse {
    let status = match e {
        InitError::Invalid(_) => 400,
        InitError::Conflict(_) => 409,
        InitError::NotFound(_) => 404,
        InitError::Failed(_) => 500,
    };
    error_response(status, &e.to_string())
}

/// The places the Hub keeps things, or the response that says it has none.
fn hub_paths(state: &HubState) -> Result<InitPaths, HttpResponse> {
    let Some(home) = hub_home_dir(state) else {
        return Err(error_response(500, "the Hub has no home directory"));
    };
    let Some(config_path) = user_config_path(state.config.home_override.as_deref()) else {
        return Err(error_response(500, "the Hub has no config directory"));
    };
    Ok(InitPaths {
        config_path,
        store_dir: records::projects_dir(&home),
        workspace_store: std::path::absolute(&state.config.store_path)
            .unwrap_or_else(|_| state.config.store_path.clone()),
    })
}

fn open_workspace<'a>(
    state: &'a HubState,
) -> impl FnOnce() -> Result<crate::HubStoreGuard<'a>, InitError> {
    move || {
        state
            .open_store()
            .map_err(|e| InitError::Failed(format!("workspace store: {e}")))
    }
}

fn open_project(
    state: &HubState,
    store: &std::path::Path,
) -> Result<crate::ProjectStoreGuard, InitError> {
    state.open_project_store_at(store).map_err(|e| match e {
        ProjectOpenError::Busy => InitError::Failed("the project's store is busy; retry".into()),
        ProjectOpenError::Store(e) => InitError::Failed(format!("project store: {e}")),
    })
}

/// A summary as the API shows it: the summary's own fields, `id` (the slug) and the three
/// artifacts-directory fields the Settings screen reads. A virtual group has no artifacts
/// directory, so those are `null`.
fn summary_json(
    state: &HubState,
    settings: &crate::settings::Settings,
    p: &ProjectSummary,
) -> Value {
    let mut out = serde_json::to_value(p).unwrap_or(Value::Null);
    let legacy = match p.kind {
        ProjectKind::Project => artifact_dirs::project_view(state, settings, &p.slug),
        ProjectKind::Virtual => json!({
            "id": p.slug,
            "artifacts_dir": null,
            "default_artifacts_dir": null,
            "effective_artifacts_dir": null,
        }),
    };
    if let (Some(out), Some(legacy)) = (out.as_object_mut(), legacy.as_object()) {
        out.extend(legacy.clone());
    }
    out
}

fn list_with_held(
    paths: &InitPaths,
    hints: &[crate::project_list::StoreHint],
    ws: &fl_store::AnyStore,
) -> Result<Vec<ProjectSummary>, InitError> {
    let held = ws
        .list_projects()
        .map_err(|e| InitError::Failed(format!("workspace store: {e}")))?;
    list_projects(&paths.config_path, hints, &held, ws)
}

fn summary_of(state: &HubState, paths: &InitPaths, slug: &str) -> Result<Value, HttpResponse> {
    let hints = records::project_store_hints(state, hub_home_dir(state).as_deref());
    let ws = state
        .open_store()
        .map_err(|_| error_response(503, "workspace store unavailable"))?;
    let listed = list_with_held(paths, &hints, &ws).map_err(|e| init_error_response(&e))?;
    let settings = artifact_dirs::current_settings(state);
    listed
        .iter()
        .find(|p| p.slug == slug)
        .map(|p| summary_json(state, &settings, p))
        .ok_or_else(|| error_response(500, "the project vanished while it was reported"))
}

/// Refuses a root in a system directory, a secret's directory or the home directory itself, as
/// the artifacts directories do. Before anything is made.
fn check_root(state: &HubState, path: &std::path::Path) -> Result<(), HttpResponse> {
    match canonical_root(path) {
        Ok(root) => match artifact_dirs::sensitive_reason(state, &root) {
            Some(reason) => Err(error_response(400, &reason)),
            None => Ok(()),
        },
        Err(e) => Err(init_error_response(&e)),
    }
}

/// `GET /api/v1/projects`: `{"projects": [ProjectSummary, ...]}`, by slug.
pub(crate) fn handle_get_projects(state: &HubState) -> HttpResponse {
    let paths = match hub_paths(state) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let hints = records::project_store_hints(state, hub_home_dir(state).as_deref());
    let ws = match state.open_store() {
        Ok(ws) => ws,
        Err(_) => return error_response(503, "workspace store unavailable"),
    };
    let listed = match list_with_held(&paths, &hints, &ws) {
        Ok(l) => l,
        Err(e) => return init_error_response(&e),
    };
    let settings = artifact_dirs::current_settings(state);
    let projects: Vec<Value> = listed
        .iter()
        .map(|p| summary_json(state, &settings, p))
        .collect();
    HttpResponse::json(200, json!({ "projects": projects }))
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateProjectBody {
    name: Option<String>,
    slug: Option<String>,
    kind: Option<String>,
    path: Option<String>,
    purpose: Option<String>,
    git_init: Option<bool>,
    github: Option<String>,
    credential: Option<String>,
    /// The earlier spelling of `kind: "virtual"`.
    #[serde(rename = "virtual")]
    virtual_group: Option<bool>,
}

/// A string field where an empty one means absent.
fn present(s: Option<String>) -> Option<String> {
    s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// `POST /api/v1/projects`. Owner only. Creates a directory-backed project (`kind` "project",
/// the default: `path` required, `git_init` default false) or a virtual group (`kind`
/// "virtual", or `virtual: true`: `path`, `git_init`, `github` and `credential` are ignored).
/// `201` with the summary for a new one, `200` when it was already there.
pub(crate) fn handle_post_projects(
    state: &HubState,
    authority: &HubAuthority,
    req: &HttpRequest,
) -> HttpResponse {
    if !authority.is_owner() {
        return error_response(403, "only Owner authority may create projects");
    }
    let body: CreateProjectBody = match req.json() {
        Ok(body) => body,
        Err(e) => return error_response(400, &e),
    };
    let kind = match body.kind.as_deref().map(str::parse::<ProjectKind>) {
        Some(Ok(k)) => Some(k),
        Some(Err(e)) => return error_response(400, &e),
        None => None,
    };
    let kind = match (kind, body.virtual_group) {
        (Some(ProjectKind::Project), Some(true)) => {
            return error_response(400, "`kind` is `project` and `virtual` is true: pick one");
        }
        (Some(k), _) => k,
        (None, Some(true)) => ProjectKind::Virtual,
        (None, _) => ProjectKind::Project,
    };
    let paths = match hub_paths(state) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let name = present(body.name);
    let slug = present(body.slug);
    let purpose = present(body.purpose);
    let by = authority.actor_name().to_string();

    let outcome = match kind {
        ProjectKind::Virtual => {
            let Some(display_name) = name.clone().or_else(|| slug.clone()) else {
                return error_response(400, "a virtual group needs a `name`");
            };
            let request = VirtualRequest {
                slug,
                display_name,
                purpose,
                by,
            };
            create_virtual(&request, &paths, open_workspace(state))
        }
        ProjectKind::Project => {
            let credential = match body.credential.as_deref().map(str::parse::<Credential>) {
                Some(Ok(c)) => Some(c),
                Some(Err(e)) => return error_response(400, &e),
                None => None,
            };
            let Some(path) = body.path.map(PathBuf::from) else {
                return error_response(400, "`path` is required for a project");
            };
            if let Err(resp) = check_root(state, &path) {
                return resp;
            }
            let request = InitRequest {
                path,
                slug,
                display_name: name,
                github: present(body.github),
                credential,
                purpose,
                git_init: body.git_init.unwrap_or(false),
                by,
            };
            init_project(&request, &paths, open_workspace(state), |r| {
                open_project(state, &r.store)
            })
        }
    };
    let outcome = match outcome {
        Ok(o) => o,
        Err(e) => return init_error_response(&e),
    };
    match summary_of(state, &paths, &outcome.slug) {
        Ok(summary) => {
            state.broadcast("projects", "project.created", summary.clone());
            HttpResponse::json(if outcome.registered { 201 } else { 200 }, summary)
        }
        Err(resp) => resp,
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateProjectBody {
    name: Option<String>,
    purpose: Option<String>,
    path: Option<String>,
    github: Option<String>,
    credential: Option<String>,
}

/// `PATCH /api/v1/projects/:slug`. Owner only. Updates `name` (the display name; the slug is
/// fixed), `purpose`, `path` (a directory-backed project only), and the tracker (`github` sets
/// or replaces it, `""` clears it; `credential` applies to it). Returns the summary.
pub(crate) fn handle_patch_project(
    state: &HubState,
    authority: &HubAuthority,
    raw_slug: &str,
    req: &HttpRequest,
) -> HttpResponse {
    if !authority.is_owner() {
        return error_response(403, "only Owner authority may change projects");
    }
    let decoded = percent_decode(raw_slug);
    if decoded.trim().is_empty() {
        return error_response(400, "project slug is empty");
    }
    let slug = match records::normalize_project(Some(&decoded)) {
        Ok(s) => s,
        Err(e) => return error_response(400, e),
    };
    let body: UpdateProjectBody = match req.json() {
        Ok(body) => body,
        Err(e) => return error_response(400, &e),
    };
    let credential = match body.credential.as_deref().map(str::parse::<Credential>) {
        Some(Ok(c)) => Some(c),
        Some(Err(e)) => return error_response(400, &e),
        None => None,
    };
    let paths = match hub_paths(state) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let path = body.path.map(PathBuf::from);
    if let Some(path) = &path
        && let Err(resp) = check_root(state, path)
    {
        return resp;
    }
    let request = UpdateRequest {
        name: body.name,
        purpose: body.purpose,
        path,
        github: body.github,
        credential,
        by: authority.actor_name().to_string(),
    };
    let hints = records::project_store_hints(state, hub_home_dir(state).as_deref());
    let updated = update_project(
        &slug,
        &request,
        &paths,
        &hints,
        open_workspace(state),
        |store| open_project(state, store),
    );
    if let Err(e) = updated {
        return init_error_response(&e);
    }
    match summary_of(state, &paths, &slug) {
        Ok(summary) => {
            state.broadcast("projects", "project.updated", summary.clone());
            HttpResponse::json(200, summary)
        }
        Err(resp) => resp,
    }
}

/// `DELETE /api/v1/projects/:slug`. Owner only. The body (empty means all defaults) is
/// `{"reassign_to"?, "decommission_agents"?, "delete_store"?, "delete_dir"?}`; see
/// [`crate::project_delete`] for the order and the safety rules. `409` with the active agents
/// when some are assigned and the request says nothing about them; `404` for an unknown project.
pub(crate) fn handle_delete_project(
    state: &HubState,
    authority: &HubAuthority,
    raw_slug: &str,
    req: &HttpRequest,
) -> HttpResponse {
    if !authority.is_owner() {
        return error_response(403, "only Owner authority may delete projects");
    }
    let decoded = percent_decode(raw_slug);
    if decoded.trim().is_empty() {
        return error_response(400, "project slug is empty");
    }
    let slug = match records::normalize_project(Some(&decoded)) {
        Ok(s) => s,
        Err(e) => return error_response(400, e),
    };
    let body: DeleteRequest = match req.json() {
        Ok(body) => body,
        Err(e) => return error_response(400, &e),
    };
    let paths = match hub_paths(state) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let home = hub_home_dir(state);
    let hints = records::project_store_hints(state, home.as_deref());
    let workspace_root = fl_core::resolve_workspace_root(&state.config.repo_root);
    let env = crate::api::lifecycle_env(state);
    let ctx = DeleteContext {
        paths: &paths,
        stores: &hints,
        home: home.as_deref(),
        workspace_root: &workspace_root,
        lifecycle: &env,
    };
    let result = delete_project(
        &slug,
        &body,
        authority.actor_name(),
        &ctx,
        open_workspace(state),
        |key| {
            state.clear_session_prompt_queue(key);
            state.clear_session_model_override(key);
        },
    );
    match result {
        Ok(outcome) => {
            let data = serde_json::to_value(&outcome).unwrap_or(Value::Null);
            state.broadcast("projects", "project.deleted", data.clone());
            HttpResponse::json(200, data)
        }
        Err(DeleteError::ActiveAgents { agents }) => {
            HttpResponse::json(409, DeleteError::active_agents_body(&agents))
        }
        Err(DeleteError::Init(e)) => init_error_response(&e),
    }
}
