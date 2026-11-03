//! `GET /api/v1/mcp/catalog` and `POST /api/v1/mcp/sync` (Record #81, Gate G5): the hub's view of
//! the committed MCP catalog (`.fl/mcp.toml`, docs/mcp.md) over the `fl-mcp` library. Both answer
//! the Owner only: the answer names servers and paths, so a Peer gets a 403 and an
//! unauthenticated caller a 401 (before this module is reached).
//!
//! **Known repos.** The hub knows one repo: its `repo_root`. `HubServerConfig` carries no
//! `[[project]]` list, so the configured projects' roots are not listed here. A known repo's
//! project directory is found as `fl mcp` finds it: the nearest directory, from the canonical
//! root up, that holds `.fl/mcp.toml`, else the nearest that holds `.git`. A repo with neither,
//! or with no catalog, is `missing`.
//!
//! **`GET /api/v1/mcp/catalog`** answers `{ "repos": [ row, ... ] }`, one row per known repo:
//!
//! - `repo` (the root's directory name), `root`, `catalog_path`, and `status`:
//!   `synced` (`fl mcp check` exits 0), `drift` (a sync would change a vendor file), `missing`
//!   (no catalog) or `error` (the catalog is malformed, a vendor file is refused, git would
//!   commit a vendor file, ...). The FerroDeck type knows only the first three; for `error` the
//!   row carries an `error` text, and `refusals` (one line per refused entry) when entries were
//!   refused.
//! - Additive fields: `enabled_servers` (server names after this machine's switches, or `null`
//!   when the plan failed before the switches were read) and `vendors` (per vendor `synced`,
//!   `drift` or `refused`, or `null` when no plan was made).
//! - `deduplicated_daemons`, `active_vendor_sessions` and `spawned_mcp_processes` are always
//!   `null`: nothing in the hub measures them. They are not invented and not `0`.
//!
//! Only server names and statuses leave the hub: never an `env` or header value, a command or
//! its arguments, or a secret's variable name. A refusal or an error text comes from the
//! library, which never quotes a catalog or vendor-file value.
//!
//! **`POST /api/v1/mcp/sync`** takes `{}` or an empty body (every known repo) or
//! `{ "repo": "<name or absolute path>" }`. A name is a known repo's `repo`; a path must be a
//! known root after canonicalisation. Anything else is a 404 (a name that two roots share is a
//! 400): the hub never syncs a path it was not configured with. Any other key is a 400.
//!
//! The answer is `{ "ok": bool, "repos": [ { "repo", "status_before", "status_after",
//! "written": [ path relative to the project dir ], "error"? } ] }`. **Partial failure:** a
//! request the hub can read answers 200 whatever the repos did; `ok` is `false` when any repo
//! has an `error`, and that repo's `error` says why (`written` is then empty: a sync writes all of a
//! repo's files or none). The 400, 403 and 404 answers are
//! `{ "error" }`. A repo with no catalog is a no-op with a `note`, not an error.
//!
//! **What a sync writes.** The vendor files `fl mcp sync` writes under the project directory,
//! each a plain file beside a temporary one, and nothing else under the project; plus the
//! library's ownership records and lock under fl's state directory
//! (`$XDG_STATE_HOME/fl/mcp`, default `~/.local/state/fl/mcp`), the same records `fl mcp` keeps,
//! so the hub and the CLI agree on which entries are fl's. Every vendor target is a path
//! relative to the project (the five built-ins' and a registered vendor's, which the library
//! refuses unless it is inside the project); a plan with any other target is refused before
//! anything is written ([`project_relative`]). The hub never reads or writes a vendor's user-level
//! file (`~/.claude.json`, `~/.codex/config.toml`, ...), and unlike the CLI it prints no Codex
//! trust warning.
//!
//! **Same gates as the CLI.** The plan uses the library's `plan`/`apply`; a vendor file that git
//! tracks or would not ignore is refused, as `fl mcp sync` refuses it. The machine's `[[mcp]]`
//! switches come from the resolver the CLI sets with [`crate::HubState::set_mcp_switches`]; with
//! none set, the team defaults apply.
//!
//! **Concurrency.** One mutex serialises every catalog read and sync in this hub; the library's
//! file lock serialises against `fl mcp sync` processes. Handlers are blocking functions: the
//! server runs each request in `spawn_blocking`, so no filesystem or git work runs on the async
//! runtime.

use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use fl_exec::git::Git;
use fl_mcp::catalog::Catalog;
use fl_mcp::sync::{self, Action, Check, Plan, Switches};
use fl_mcp::vendor;
use serde_json::{Value, json};

use crate::auth::HubAuthority;
use crate::{HttpRequest, HttpResponse, HubState};

const CATALOG: &str = "/api/v1/mcp/catalog";
const SYNC: &str = "/api/v1/mcp/sync";

/// The machine's switches for a project directory (docs/mcp.md, "Each machine's switches").
type Resolver = Arc<dyn Fn(&Path) -> Result<Switches, String> + Send + Sync>;

/// What the hub keeps for its MCP endpoints.
#[derive(Default)]
pub(crate) struct McpHub {
    /// Held across every catalog read and every sync.
    gate: Mutex<()>,
    settings: Mutex<Settings>,
}

#[derive(Default)]
struct Settings {
    /// Where the ownership records live; `None`: fl's state directory.
    records: Option<PathBuf>,
    switches: Option<Resolver>,
}

impl fmt::Debug for McpHub {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpHub").finish_non_exhaustive()
    }
}

impl McpHub {
    pub(crate) fn set_switches(&self, resolver: Resolver) {
        self.settings().switches = Some(resolver);
    }

    #[cfg(test)]
    pub(crate) fn set_records_dir(&self, dir: PathBuf) {
        self.settings().records = Some(dir);
    }

    fn settings(&self) -> std::sync::MutexGuard<'_, Settings> {
        self.settings.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn gate(&self) -> std::sync::MutexGuard<'_, ()> {
        self.gate.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The ownership-record directory, as `fl mcp` resolves it (`<state base>/fl/mcp`).
    fn records_dir(&self) -> Result<PathBuf, String> {
        if let Some(dir) = self.settings().records.clone() {
            return Ok(dir);
        }
        let absolute = |k: &str| {
            std::env::var_os(k)
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
        };
        let base = absolute("XDG_STATE_HOME")
            .or_else(|| absolute("HOME").map(|h| h.join(".local/state")))
            .ok_or("neither an absolute $XDG_STATE_HOME nor $HOME is set, so there is nowhere to keep fl's ownership records")?;
        Ok(base.join("fl").join("mcp"))
    }

    fn switches(&self, project: &Path) -> Result<Switches, String> {
        let resolver = self.settings().switches.clone();
        resolver.map_or_else(|| Ok(Switches::default()), |r| r(project))
    }
}

/// The repos the hub knows: its `repo_root`. See the module note.
fn known_roots(state: &HubState) -> Vec<PathBuf> {
    vec![state.config.repo_root.clone()]
}

/// The name a repo goes by: its (canonical) root's directory name.
fn repo_name(root: &Path) -> String {
    let canonical = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    canonical
        .file_name()
        .map_or_else(|| "repo".into(), |n| n.to_string_lossy().into_owned())
}

/// `fl mcp`'s project rule, from a repo root: the nearest directory, from the canonical root up,
/// that holds `.fl/mcp.toml`, else the nearest that holds `.git`. `None` when the root is absent
/// or there is neither.
fn project_dir(root: &Path) -> Option<PathBuf> {
    let root = fs::canonicalize(root).ok()?;
    let found = (root.ancestors().find(|d| Catalog::path(d).is_file()))
        .or_else(|| root.ancestors().find(|d| d.join(".git").exists()));
    found.map(Path::to_path_buf)
}

/// `path` as a path inside `project`: `Some(relative)` only when it is `project` joined with
/// plain components (no `..`, no root, no prefix). The hub writes nothing else.
fn project_relative(project: &Path, path: &Path) -> Option<PathBuf> {
    let rel = path.strip_prefix(project).ok()?;
    let plain = rel.components().all(|c| matches!(c, Component::Normal(_)));
    (plain && rel.components().next().is_some()).then(|| rel.to_path_buf())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Synced,
    Drift,
    Missing,
    Error,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Synced => "synced",
            Status::Drift => "drift",
            Status::Missing => "missing",
            Status::Error => "error",
        }
    }
}

/// What looking at one repo found.
struct Probe {
    status: Status,
    catalog_path: PathBuf,
    /// The project directory, when there is a catalog to plan from.
    project: Option<PathBuf>,
    plan: Option<Plan>,
    enabled: Option<Vec<String>>,
    error: Option<String>,
    refusals: Vec<String>,
}

impl Probe {
    fn failed(catalog_path: PathBuf, project: Option<PathBuf>, error: String) -> Probe {
        Probe {
            status: Status::Error,
            catalog_path,
            project,
            plan: None,
            enabled: None,
            error: Some(error),
            refusals: Vec::new(),
        }
    }
}

/// Plans one repo exactly as `fl mcp check` does (no write, no registry, no network).
fn probe(hub: &McpHub, root: &Path) -> Probe {
    let Some(project) = project_dir(root) else {
        return Probe {
            status: Status::Missing,
            catalog_path: Catalog::path(root),
            project: None,
            plan: None,
            enabled: None,
            error: None,
            refusals: Vec::new(),
        };
    };
    let catalog_path = Catalog::path(&project);
    let catalog = match Catalog::load(&project) {
        Ok(Some(catalog)) => catalog,
        Ok(None) => {
            return Probe {
                status: Status::Missing,
                catalog_path,
                project: Some(project),
                plan: None,
                enabled: None,
                error: None,
                refusals: Vec::new(),
            };
        }
        Err(e) => return Probe::failed(catalog_path, Some(project), e.to_string()),
    };
    let fail = |error: String| Probe::failed(catalog_path.clone(), Some(project.clone()), error);
    let switches = match hub.switches(&project) {
        Ok(s) => s,
        Err(e) => return fail(e),
    };
    let records = match hub.records_dir() {
        Ok(r) => r,
        Err(e) => return fail(e),
    };
    let plan = match sync::plan(&project, &catalog, &switches, &records, &[]) {
        Ok(plan) => plan,
        Err(e) => return fail(e.to_string()),
    };
    let enabled: Vec<String> = (catalog.servers.iter())
        .filter(|(name, server)| {
            let on = server.enabled || switches.enable.contains(name);
            on && !switches.disable.contains(name)
        })
        .map(|(name, _)| name.clone())
        .collect();
    let mut probe = Probe {
        status: Status::Synced,
        catalog_path,
        project: Some(project.clone()),
        plan: None,
        enabled: Some(enabled),
        error: None,
        refusals: Vec::new(),
    };
    if let Err(e) = confine(&project, &plan).and_then(|()| git_gate(&project, &plan)) {
        probe.status = Status::Error;
        probe.error = Some(e);
        probe.plan = Some(plan);
        return probe;
    }
    match plan.check() {
        Check::Matches => {}
        Check::Changes => probe.status = Status::Drift,
        Check::Refused => {
            probe.status = Status::Error;
            probe.refusals = plan.refusals().iter().map(|r| r.to_string()).collect();
            probe.error = Some(format!(
                "{} entries were refused; nothing would be written",
                probe.refusals.len()
            ));
        }
    }
    probe.plan = Some(plan);
    probe
}

/// Every vendor target is a plain path inside the project; anything else is refused.
fn confine(project: &Path, plan: &Plan) -> Result<(), String> {
    for target in plan.targets() {
        if project_relative(project, &target.path).is_none() {
            return Err(format!(
                "the {} target is not a path inside the project, and the hub writes only inside it",
                target.vendor.as_str()
            ));
        }
    }
    Ok(())
}

/// docs/mcp.md ("The `.gitignore` lines"): a vendor file fl writes must be ignored and not
/// tracked. Asked of git, which fails rather than answer `no`.
fn git_gate(project: &Path, plan: &Plan) -> Result<(), String> {
    let mut problems = Vec::new();
    let writes = |t: &sync::Target| {
        (t.entries.iter()).any(|e| !matches!(e.action, Action::Untouched | Action::Forget))
    };
    for target in plan.targets().iter().filter(|t| writes(t)) {
        let rel = vendor::vendor(target.vendor).target();
        let tracked = Git::is_tracked(project, rel).map_err(|e| format!("{e}"))?;
        if tracked {
            problems.push(format!("{rel} is tracked by git"));
        } else if !Git::is_ignored(project, rel).map_err(|e| format!("{e}"))? {
            problems.push(format!("{rel} is not ignored by git"));
        }
    }
    if problems.is_empty() {
        return Ok(());
    }
    Err(format!(
        "fl writes these files from the catalog, so git must ignore them ({}): add them to \
         .gitignore; nothing was written",
        problems.join("; ")
    ))
}

/// Per vendor: `refused`, `drift` or `synced`.
fn vendors(plan: &Plan) -> Value {
    let mut map = serde_json::Map::new();
    for target in plan.targets() {
        let refused = (target.entries.iter()).any(|e| matches!(e.action, Action::Refused(_)));
        let state = if refused {
            "refused"
        } else if target.changes() {
            "drift"
        } else {
            "synced"
        };
        map.insert(target.vendor.as_str().to_string(), json!(state));
    }
    Value::Object(map)
}

fn row(root: &Path, probe: &Probe) -> Value {
    let mut row = json!({
        "repo": repo_name(root),
        "root": fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf()).display().to_string(),
        "catalog_path": probe.catalog_path.display().to_string(),
        "status": probe.status.as_str(),
        "deduplicated_daemons": Value::Null,
        "active_vendor_sessions": Value::Null,
        "spawned_mcp_processes": Value::Null,
        "enabled_servers": probe.enabled,
        "vendors": probe.plan.as_ref().map(vendors),
    });
    if let Some(error) = &probe.error {
        row["error"] = json!(error);
    }
    if !probe.refusals.is_empty() {
        row["refusals"] = json!(probe.refusals);
    }
    row
}

fn error(status: u16, message: impl Into<String>) -> HttpResponse {
    HttpResponse::json(status, json!({ "error": message.into() }))
}

/// `GET /api/v1/mcp/catalog`.
fn handle_get(state: &HubState) -> HttpResponse {
    let hub = &state.mcp;
    let _held = hub.gate();
    let repos: Vec<Value> = known_roots(state)
        .iter()
        .map(|root| row(root, &probe(hub, root)))
        .collect();
    HttpResponse::json(200, json!({ "repos": repos }))
}

/// The known roots a request's `repo` selects.
fn select(roots: Vec<PathBuf>, body: &Value) -> Result<Vec<PathBuf>, HttpResponse> {
    let object = body.as_object().expect("json_object_body gave an object");
    if let Some(key) = object.keys().find(|k| *k != "repo") {
        return Err(error(
            400,
            format!("unknown field `{key}`; the body takes only `repo`"),
        ));
    }
    let given = match object.get("repo") {
        None | Some(Value::Null) => return Ok(roots),
        Some(Value::String(s)) if !s.trim().is_empty() => s.trim(),
        Some(_) => return Err(error(400, "`repo` must be a non-empty string")),
    };
    let unknown = || {
        error(
            404,
            "no such repo; GET /api/v1/mcp/catalog lists the repos the hub knows",
        )
    };
    if given.contains('/') || given.starts_with('.') {
        if !Path::new(given).is_absolute() {
            return Err(error(400, "a `repo` path must be absolute"));
        }
        let wanted = fs::canonicalize(given).map_err(|_| unknown())?;
        let hit = roots
            .into_iter()
            .find(|r| fs::canonicalize(r).is_ok_and(|c| c == wanted));
        return hit.map(|r| vec![r]).ok_or_else(unknown);
    }
    let mut named: Vec<PathBuf> = roots
        .into_iter()
        .filter(|r| repo_name(r) == given)
        .collect();
    match named.len() {
        0 => Err(unknown()),
        1 => Ok(vec![named.remove(0)]),
        _ => Err(error(
            400,
            "more than one known repo has that name; name it by its absolute path",
        )),
    }
}

/// Syncs one repo; the hub's gate is held.
fn sync_one(hub: &McpHub, root: &Path) -> Value {
    let before = probe(hub, root);
    let mut result = json!({
        "repo": repo_name(root),
        "status_before": before.status.as_str(),
        "status_after": before.status.as_str(),
        "written": [],
    });
    match before.status {
        Status::Missing => {
            result["note"] = json!("no MCP catalog (.fl/mcp.toml); nothing to sync");
            return result;
        }
        Status::Error => {
            result["error"] = json!(before.error.clone().unwrap_or_default());
            if !before.refusals.is_empty() {
                result["refusals"] = json!(before.refusals);
            }
            return result;
        }
        Status::Synced | Status::Drift => {}
    }
    let (Some(project), Some(plan)) = (&before.project, &before.plan) else {
        return result;
    };
    let written: Vec<String> = plan
        .targets()
        .iter()
        .filter(|t| t.changes())
        .filter_map(|t| project_relative(project, &t.path))
        .map(|rel| rel.display().to_string())
        .collect();
    let applied = sync::apply(plan);
    let after = probe(hub, root);
    result["status_after"] = json!(after.status.as_str());
    match applied {
        Ok(()) => result["written"] = json!(written),
        Err(e) => result["error"] = json!(e.to_string()),
    }
    result
}

/// `POST /api/v1/mcp/sync`.
fn handle_post(state: &HubState, req: &HttpRequest) -> HttpResponse {
    let body = match crate::api::json_object_body(req) {
        Ok(body) => body,
        Err(resp) => return resp,
    };
    let roots = match select(known_roots(state), &body) {
        Ok(roots) => roots,
        Err(resp) => return resp,
    };
    let hub = &state.mcp;
    let _held = hub.gate();
    let repos: Vec<Value> = roots.iter().map(|root| sync_one(hub, root)).collect();
    let ok = repos.iter().all(|r| r.get("error").is_none());
    HttpResponse::json(200, json!({ "ok": ok, "repos": repos }))
}

/// Routes the two MCP endpoints; `None` for any other path.
pub(crate) fn route(
    state: &HubState,
    authority: &HubAuthority,
    req: &HttpRequest,
) -> Option<HttpResponse> {
    let path = req.path.trim_end_matches('/');
    let method = match path {
        CATALOG => "GET",
        SYNC => "POST",
        _ => return None,
    };
    if !authority.is_owner() {
        return Some(error(403, "owner authority required"));
    }
    if req.method != method {
        return Some(
            error(405, format!("{path} takes only {method}")).with_header("Allow", method),
        );
    }
    Some(match path {
        CATALOG => handle_get(state),
        _ => handle_post(state, req),
    })
}

#[cfg(test)]
#[path = "mcp_api_tests.rs"]
mod tests;
