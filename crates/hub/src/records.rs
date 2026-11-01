//! Project records and the Kanban board (Wave B, Record 1): list a project's records with their
//! findings and cost, create a record, and move one between columns.
//!
//! A `project` is a slug naming `<home>/.local/share/fl/<slug>.redb`. When that file exists the
//! request reads and writes it; otherwise it falls back to the hub's own store. The slug is
//! validated before any path is built, and a symlinked store file is refused, so a request can
//! never reach a file outside that directory.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io;
use std::ops::Deref;
use std::path::{Path, PathBuf};

use fl_core::escalation::Escalations;
use fl_core::finding::FindingState;
use fl_core::ids::{Kind, ProjectId, RecordId};
use fl_core::iri::Iri;
use fl_core::model::{Record, State};
use fl_core::store::{Catalog, Handles, Ledger, Roles, StoreError, Tracker};
use fl_exec::population::ExecError;
use fl_exec::record::{MoveOutcome, move_record};
use fl_store::AnyStore;
use fl_store::manifest::{ImportCheckError, ensure_import_current};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::api::{HttpRequest, HttpResponse, hub_home_dir, percent_decode};
use crate::auth::HubAuthority;
use crate::{HubState, HubStoreGuard, ProjectOpenError, ProjectStoreGuard};

/// The project a request names when it names none.
const DEFAULT_PROJECT: &str = "ferroloop";
/// `fl.redb` is the hub's own store, never a project store.
const RESERVED_PROJECT: &str = "fl";
const MAX_PROJECT_LEN: usize = 64;
/// Most directory entries the project scan reads.
const MAX_PROJECT_SCAN: usize = 200;
const MAX_TITLE_CHARS: usize = 500;
const MAX_RECORD_ID_LEN: usize = 256;

/// `<home>/.local/share/fl`, where per-project stores live.
fn projects_dir(home: &Path) -> PathBuf {
    home.join(".local/share/fl")
}

fn error_json(status: u16, message: &str) -> HttpResponse {
    HttpResponse::json(status, json!({ "error": message }))
}

fn bad_request(message: &str) -> HttpResponse {
    error_json(400, message)
}

/// The slug a request names, lowercased: `[a-z0-9_-]`, 1 to 64 characters, empty meaning the
/// default. Checked before the slug goes anywhere near a path.
pub(crate) fn normalize_project(raw: Option<&str>) -> Result<String, &'static str> {
    let trimmed = raw.map(str::trim).unwrap_or("");
    if trimmed.is_empty() {
        return Ok(DEFAULT_PROJECT.to_string());
    }
    if trimmed.len() > MAX_PROJECT_LEN {
        return Err("`project` is too long (64 characters at most)");
    }
    let slug = trimmed.to_ascii_lowercase();
    let chars_ok = slug
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
    if !chars_ok {
        return Err("`project` may hold only letters, digits, `-` and `_`");
    }
    if slug == RESERVED_PROJECT {
        return Err("`project` names the hub's own store, which is not a project");
    }
    Ok(slug)
}

/// The file the engine `env` (the `FL_STORE_BACKEND` value) selects keeps the store named
/// `logical` in: `logical` itself for redb, its sibling `.fls` for FerroStore. The hub names a
/// project store by its logical `<slug>.redb` path and passes that to `fl_store::open_store`;
/// only a check of the file itself needs this.
fn physical_store_file_with(logical: &Path, env: Option<&str>) -> PathBuf {
    match fl_store::choose_backend(logical, env) {
        Ok(kind) => fl_store::physical_path(logical, kind),
        Err(_) => logical.to_path_buf(),
    }
}

/// The extension a per-project store file has under the engine `env` selects.
fn store_extension_with(env: Option<&str>) -> String {
    physical_store_file_with(Path::new("x.redb"), env)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("redb")
        .to_string()
}

/// The engine selection a request reads the project files by: the hub's own selection when it has
/// one (`store_backend`, set only by tests), else `FL_STORE_BACKEND`. It is the same selection the
/// `open_*` functions open the stores with.
fn engine_env(state: &HubState) -> Option<String> {
    match state.store_backend {
        Some(fl_store::BackendKind::Redb) => Some("redb".to_string()),
        Some(fl_store::BackendKind::Ferro) => Some("ferro".to_string()),
        None => std::env::var(fl_store::BACKEND_ENV).ok(),
    }
}

/// The logical path of the per-project store (`<slug>.redb`) when its physical file exists as a
/// plain file. A symlink or anything else that is not a regular file is refused, so the slug
/// cannot be used to reach another path.
///
/// Under the FerroStore engine a project that still has only its `<slug>.redb` (not yet
/// migrated) is an error, not `None`: `None` sends the request to the hub store, which would
/// take a write meant for the project. The hub never migrates a project store: the first `fl`
/// command that opens it, or `fl store migrate`, does.
fn project_redb(
    state: &HubState,
    home: &Path,
    slug: &str,
) -> Result<Option<PathBuf>, HttpResponse> {
    project_redb_with(home, slug, engine_env(state).as_deref())
}

/// [`project_redb`] with the value of `FL_STORE_BACKEND` passed in.
fn project_redb_with(
    home: &Path,
    slug: &str,
    env: Option<&str>,
) -> Result<Option<PathBuf>, HttpResponse> {
    let path = projects_dir(home).join(format!("{slug}.redb"));
    let physical = physical_store_file_with(&path, env);
    match fs::symlink_metadata(&physical) {
        Ok(meta) if meta.file_type().is_symlink() => Err(bad_request(
            "the project store is a symlink, which is refused",
        )),
        Ok(meta) if meta.is_file() => Ok(Some(path)),
        Ok(_) => Err(bad_request("the project store is not a regular file")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let unmigrated =
                physical != path && fs::symlink_metadata(&path).is_ok_and(|m| m.is_file());
            if unmigrated {
                Err(error_json(
                    409,
                    "the project's store is a redb file that has not been migrated to the ferro \
                     engine; migrate it or set the engine to redb",
                ))
            } else {
                Ok(None)
            }
        }
        Err(_) => Err(store_unavailable()),
    }
}

fn store_unavailable() -> HttpResponse {
    error_json(503, "project store unavailable")
}

/// The store a request works on: a project's own file, held on that file's lock alone, or the
/// hub store, held on the hub-wide `store_lock`.
enum OpenedStore<'a> {
    Project(ProjectStoreGuard),
    Hub(HubStoreGuard<'a>),
}

impl Deref for OpenedStore<'_> {
    type Target = AnyStore;

    fn deref(&self) -> &AnyStore {
        match self {
            OpenedStore::Project(s) => s,
            OpenedStore::Hub(s) => s,
        }
    }
}

fn project_busy() -> HttpResponse {
    error_json(503, "project busy")
}

/// Open the store `slug` names: its own `.redb` when present, else the hub's store. Every
/// failure is an answer, never a panic.
///
/// A project's own file is serialised per path, not under the hub-wide `store_lock`, and the
/// wait is bounded (about 2 s): a request that cannot get the file in time, because another
/// request or a CLI process holds it, is a 503 `project busy`. A move runs gate commands (can
/// take minutes) while it holds its project's file, so it blocks that project only.
///
/// The hub-store fallback (`config.store_path`, used when no `<slug>.redb` exists) is the
/// dev/test case. It keeps the old behaviour: it takes `store_lock`, so a move there holds the
/// hub-wide lock for the whole gate run.
fn open_project_store<'a>(
    state: &'a HubState,
    slug: &str,
) -> Result<OpenedStore<'a>, HttpResponse> {
    let own = match hub_home_dir(state) {
        Some(home) => project_redb(state, &home, slug)?,
        None => None,
    };
    match own {
        Some(path) => match state.open_project_store_at(&path) {
            Ok(store) => Ok(OpenedStore::Project(store)),
            Err(ProjectOpenError::Busy) => Err(project_busy()),
            Err(ProjectOpenError::Store(_)) => Err(store_unavailable()),
        },
        None => state
            .open_store()
            .map(OpenedStore::Hub)
            .map_err(|_| store_unavailable()),
    }
}

/// Every project this hub can show: each project store file of the selected engine
/// (`<slug>.fls` under FerroStore, `<slug>.redb` under redb) beside the hub store (not the hub
/// store itself, not a symlink, only well-formed lowercase slugs), plus the default. Reads at
/// most 200 entries.
///
/// Under FerroStore a project that still has only its `<slug>.redb` is not listed (its records
/// answer `409`) until a `fl` command, or `fl store migrate`, opens it and makes the `.fls`.
fn available_projects(state: &HubState, home: Option<&Path>) -> Vec<String> {
    available_projects_with(home, engine_env(state).as_deref())
}

/// [`available_projects`] with the value of `FL_STORE_BACKEND` passed in.
fn available_projects_with(home: Option<&Path>, env: Option<&str>) -> Vec<String> {
    let mut found = BTreeSet::new();
    found.insert(DEFAULT_PROJECT.to_string());
    let Some(entries) = home.and_then(|h| fs::read_dir(projects_dir(h)).ok()) else {
        return found.into_iter().collect();
    };
    let suffix = format!(".{}", store_extension_with(env));
    for entry in entries.take(MAX_PROJECT_SCAN).flatten() {
        // `DirEntry::file_type` does not follow symlinks.
        if !entry.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        let name = entry.file_name();
        let Some(stem) = name.to_str().and_then(|n| n.strip_suffix(suffix.as_str())) else {
            continue;
        };
        if !stem.is_empty() && normalize_project(Some(stem)).as_deref() == Ok(stem) {
            found.insert(stem.to_string());
        }
    }
    found.into_iter().collect()
}

fn store_error_response(err: &StoreError) -> HttpResponse {
    match err {
        StoreError::NoSuchRecord(_)
        | StoreError::NotOwned { .. }
        | StoreError::WrongKind { .. } => error_json(404, "record not found"),
        StoreError::Escalating { .. } | StoreError::Escalated { .. } => {
            error_json(409, &err.to_string())
        }
        _ => error_json(500, "store operation failed"),
    }
}

/// Number of findings about `record` that are open and in total, counted for the record's id and
/// its aliases alike (the escalation code reads them the same way). Open means not `Fixed` and
/// not `Withdrawn`.
fn finding_counts(per_record: &HashMap<String, (u64, u64)>, record: &Record) -> (u64, u64) {
    std::iter::once(record.id.iri())
        .chain(record.also_known_as.iter())
        .filter_map(|iri| per_record.get(iri.as_str()))
        .fold((0, 0), |(open, total), (o, t)| (open + o, total + t))
}

/// `(open, total)` findings of a project, keyed by the id of the record each is about.
fn findings_by_record(
    store: &dyn Tracker,
    project: &ProjectId,
) -> Result<HashMap<String, (u64, u64)>, StoreError> {
    let mut per_record: HashMap<String, (u64, u64)> = HashMap::new();
    for f in store.list_findings(project)? {
        let slot = per_record.entry(f.record.as_str().to_string()).or_default();
        slot.1 += 1;
        if !matches!(f.state, FindingState::Fixed | FindingState::Withdrawn) {
            slot.0 += 1;
        }
    }
    Ok(per_record)
}

fn record_json(
    store: &dyn Handles,
    slug: &str,
    record: &Record,
    findings: (u64, u64),
) -> Result<Value, StoreError> {
    let handle = store.handle_of(Kind::Record, record.id.iri())?;
    Ok(json!({
        "id": record.id.as_str(),
        "handle": handle,
        "display_id": handle.map_or_else(|| record.id.as_str().to_string(), |h| h.to_string()),
        "project": slug,
        "title": record.title,
        "state": record.state.as_wire(),
        "area": record.area,
        // The hub has no per-record tier yet, so every record reads as local.
        "tier": "local",
        "findings_open": findings.0,
        "findings_total": findings.1,
    }))
}

/// Records with a handle first, by handle; the rest after, by id.
fn sort_records(records: &mut [Value]) {
    records.sort_by(|a, b| {
        let key = |v: &Value| {
            (
                v["handle"].as_u64().is_none(),
                v["handle"].as_u64().unwrap_or(0),
                v["id"].as_str().unwrap_or("").to_string(),
            )
        };
        key(a).cmp(&key(b))
    });
}

fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

/// The first project's records (sorted) and the board summary. A store with no project yields
/// nothing and a zeroed summary.
fn board<S: Catalog + Tracker + Ledger + Handles>(
    store: &S,
    slug: &str,
) -> Result<(Vec<Value>, Value), StoreError> {
    let Some(project) = store.list_projects()?.into_iter().next() else {
        return Ok((Vec::new(), summary_json(&[], 0, 0, 0)));
    };
    let records = store.list_records(&project.id)?;
    let per_record = findings_by_record(store, &project.id)?;
    let attempts = store.attempts(&project.id)?;
    let cost_micros: u64 = attempts.iter().map(|a| a.cost_usd_micros).sum();

    let mut open_total = 0;
    let mut states = Vec::with_capacity(records.len());
    let mut out = Vec::with_capacity(records.len());
    for record in &records {
        let counts = finding_counts(&per_record, record);
        open_total += counts.0;
        states.push(record.state);
        out.push(record_json(store, slug, record, counts)?);
    }
    sort_records(&mut out);
    Ok((
        out,
        summary_json(&states, open_total, attempts.len() as u64, cost_micros),
    ))
}

fn summary_json(states: &[State], open_findings: u64, attempts: u64, cost_micros: u64) -> Value {
    let count = |s: State| states.iter().filter(|&&x| x == s).count();
    let total = states.len();
    let done = count(State::Done);
    // Review, needs_human and any other state the board has no column for.
    let other = total - count(State::Todo) - count(State::Doing) - done;
    let pct = if total == 0 {
        0.0
    } else {
        round1(done as f64 / total as f64 * 100.0)
    };
    json!({
        "todo_count": count(State::Todo),
        "doing_count": count(State::Doing),
        "done_count": done,
        "other_count": other,
        "total_count": total,
        "completion_pct": pct,
        "open_findings_count": open_findings,
        "attempts_count": attempts,
        "total_cost_usd": cost_micros as f64 / 1e6,
    })
}

/// `GET /api/v1/records?project=<slug>`: the project's records and board summary. Any
/// authenticated caller.
pub(crate) fn handle_get_records(state: &HubState, req: &HttpRequest) -> HttpResponse {
    let slug = match normalize_project(req.query.get("project").map(String::as_str)) {
        Ok(slug) => slug,
        Err(e) => return bad_request(e),
    };
    let result = {
        let store = match open_project_store(state, &slug) {
            Ok(store) => store,
            Err(resp) => return resp,
        };
        board(&*store, &slug)
    };
    match result {
        Ok((records, summary)) => HttpResponse::json(
            200,
            json!({
                "project": slug,
                "available_projects": available_projects(state, hub_home_dir(state).as_deref()),
                "records": records,
                "summary": summary,
            }),
        ),
        Err(e) => store_error_response(&e),
    }
}

/// A column a record can be created in or moved to.
fn board_state(raw: &str) -> Result<State, &'static str> {
    match State::from_wire(raw.trim().to_ascii_lowercase().as_str()) {
        Some(s @ (State::Todo | State::Doing | State::Done)) => Ok(s),
        _ => Err("state must be one of todo, doing, done"),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateBody {
    project: Option<String>,
    title: String,
    area: Option<String>,
    state: Option<String>,
}

fn owner_only(authority: &HubAuthority, what: &str) -> Option<HttpResponse> {
    (!authority.is_owner()).then(|| error_json(403, &format!("only Owner authority may {what}")))
}

/// `POST /api/v1/records` (Owner only): create a record, in `todo` unless a state is given. A
/// store with no project gets one rooted at `/workspace/<slug>`.
pub(crate) fn handle_post_record(
    state: &HubState,
    authority: &HubAuthority,
    req: &HttpRequest,
) -> HttpResponse {
    if let Some(resp) = owner_only(authority, "create records") {
        return resp;
    }
    let body: CreateBody = match req.json() {
        Ok(body) => body,
        Err(e) => return bad_request(&e),
    };
    let slug = match normalize_project(body.project.as_deref()) {
        Ok(slug) => slug,
        Err(e) => return bad_request(e),
    };
    let title = body.title.trim();
    if title.is_empty() || title.chars().count() > MAX_TITLE_CHARS {
        return bad_request("`title` must be 1 to 500 characters");
    }
    if title.chars().any(char::is_control) {
        return bad_request("`title` must not contain control characters");
    }
    let target = match body.state.as_deref() {
        None => State::Todo,
        Some(raw) => match board_state(raw) {
            Ok(s) => s,
            Err(e) => return bad_request(e),
        },
    };
    // ⚠ A record with an area raises the store's format (a one-way upgrade for every older fl)
    // and the CLI makes it only after its routing checks (`ensure_routing_current`,
    // `place_record`). The hub does neither, so it does not make one yet.
    if body.area.as_deref().is_some_and(|a| !a.trim().is_empty()) {
        return error_json(422, "area is not supported by the hub yet");
    }

    let created = {
        let store = match open_project_store(state, &slug) {
            Ok(store) => store,
            Err(resp) => return resp,
        };
        create_record(&*store, &slug, title, target)
    };
    match created {
        Ok(record) => {
            state.broadcast("records", "record.created", record.clone());
            HttpResponse::json(201, json!({ "record": record }))
        }
        Err(e) => store_error_response(&e),
    }
}

fn create_record<S: Catalog + Tracker + Handles>(
    store: &S,
    slug: &str,
    title: &str,
    target: State,
) -> Result<Value, StoreError> {
    let project = match store.list_projects()?.into_iter().next() {
        Some(p) => p.id,
        None => store.add_project(&format!("/workspace/{slug}"))?,
    };
    let id = store.add_record(&project, title)?;
    if target != State::Todo {
        store.set_record_state(&id, target)?;
    }
    let record = store
        .get_record(&id)?
        .ok_or_else(|| StoreError::NoSuchRecord(id.clone()))?;
    record_json(store, slug, &record, (0, 0))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MoveBody {
    to: String,
    project: Option<String>,
}

/// A record id from a URL segment, decoded and checked: bounded, and free of path separators,
/// `..`, NUL and other control characters.
fn decode_record_id(raw: &str) -> Result<String, HttpResponse> {
    let id = percent_decode(raw);
    let bad = id.is_empty()
        || id.len() > MAX_RECORD_ID_LEN
        || id.contains(['/', '\\'])
        || id.contains("..")
        || id.chars().any(char::is_control);
    if bad {
        return Err(bad_request("invalid record id"));
    }
    Ok(id)
}

/// The record `id` names in `store`: a bare number is a handle, anything else an IRI.
fn find_record(store: &(impl Tracker + Handles), id: &str) -> Result<Record, HttpResponse> {
    let record_id = if id.bytes().all(|b| b.is_ascii_digit()) {
        let handle: u64 = id
            .parse()
            .map_err(|_| bad_request("record handle is too large"))?;
        match store.resolve_handle(Kind::Record, handle) {
            Ok(Some(iri)) => RecordId(iri),
            Ok(None) => return Err(error_json(404, "record not found")),
            Err(e) => return Err(store_error_response(&e)),
        }
    } else {
        RecordId(Iri::parse(id).map_err(|_| bad_request("invalid record id"))?)
    };
    match store.get_record(&record_id) {
        Ok(Some(record)) => Ok(record),
        Ok(None) => Err(error_json(404, "record not found")),
        // A tombstoned record reads as `Escalated`: the move is refused, not a fault.
        Err(e @ (StoreError::Escalating { .. } | StoreError::Escalated { .. })) => {
            Err(refused(&e.to_string()))
        }
        Err(e) => Err(store_error_response(&e)),
    }
}

/// An error from the move itself, as a response. A refused publish says nothing changed.
fn exec_error_response(err: &ExecError) -> HttpResponse {
    let status = match err {
        ExecError::Unpublished(_) => 503,
        ExecError::PublishRefused(_) | ExecError::Escalation(_) | ExecError::StaleMove(_) => 409,
        ExecError::BadSelector(_) => 422,
        _ => 500,
    };
    HttpResponse::json(status, json!({ "ok": false, "error": err.to_string() }))
}

fn refused(message: &str) -> HttpResponse {
    HttpResponse::json(409, json!({ "ok": false, "error": message }))
}

/// The CLI's marked-record guard (routing spec §3.3 step 1; `refuse_marked`): a record marked
/// escalating, or already escalated, is not moved, and no gate runs for it.
fn refuse_marked(store: &AnyStore, record: &Record) -> Result<(), HttpResponse> {
    let iri = record.id.iri();
    let fault = match (store.mark_of(iri), store.tombstone_of(iri)) {
        (Err(e), _) | (_, Err(e)) => return Err(store_error_response(&e)),
        (Ok(Some(_)), _) => {
            let shown = match store.handle_of(Kind::Record, iri) {
                Ok(Some(h)) => h.to_string(),
                _ => iri.as_str().to_string(),
            };
            StoreError::Escalating {
                id: iri.clone(),
                to_finish: format!("fl record escalate {shown}"),
            }
        }
        (_, Ok(Some(t))) => StoreError::Escalated {
            from: iri.clone(),
            to: t.to,
        },
        (Ok(None), Ok(None)) => return Ok(()),
    };
    Err(refused(&fault.to_string()))
}

/// The CLI's import check for a gated move (spec §4.5): a store that imported the project's
/// manifest runs its gates only while the working tree still holds that manifest.
fn refuse_stale_import(store: &AnyStore, record: &Record) -> Result<(), HttpResponse> {
    match ensure_import_current(store, &record.project) {
        Ok(()) => Ok(()),
        Err(ImportCheckError::Store(e)) => Err(store_error_response(&e)),
        Err(e) => Err(refused(&e.to_string())),
    }
}

/// The record as the store holds it now, with its findings counted.
fn current_record_json<S: Tracker + Handles>(
    store: &S,
    slug: &str,
    id: &RecordId,
) -> Result<Value, StoreError> {
    let record = store
        .get_record(id)?
        .ok_or_else(|| StoreError::NoSuchRecord(id.clone()))?;
    let counts = finding_counts(&findings_by_record(store, &record.project)?, &record);
    record_json(store, slug, &record, counts)
}

/// `POST /api/v1/records/:id/move` (Owner only): run the transitions that cover the move, then
/// change the state. `:id` is a handle or an IRI; the project comes from the body or the query.
///
/// The guards of `fl record move` that do not need the CLI's config run first, in its order: a
/// marked or escalated record is refused before any gate runs, and so is a gated move on a
/// stale manifest import. A record the project's board does not list is not found.
///
/// ⚠ Not done: the CLI's GitHub-ledger pre-flush (`preflight::check`) and its comment after the
/// move. Whether a project is bound to a GitHub ledger is read from the CLI's config file,
/// which the hub does not read, so the hub's move writes the local ledger only.
///
/// ⚠ A transition's gates run commands: the program each gate declares, with the project root
/// as working directory, and `git` there. They run while this request holds the project's
/// store file (see [`open_project_store`]) and, for the hub-store fallback, the hub-wide lock.
pub(crate) fn handle_post_record_move(
    state: &HubState,
    authority: &HubAuthority,
    req: &HttpRequest,
    raw_id: &str,
) -> HttpResponse {
    if let Some(resp) = owner_only(authority, "move records") {
        return resp;
    }
    let id = match decode_record_id(raw_id) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let body: MoveBody = match req.json() {
        Ok(body) => body,
        Err(e) => return bad_request(&e),
    };
    let target = match board_state(&body.to) {
        Ok(s) => s,
        Err(e) => return bad_request(e),
    };
    let project = body
        .project
        .as_deref()
        .or_else(|| req.query.get("project").map(String::as_str));
    let slug = match normalize_project(project) {
        Ok(slug) => slug,
        Err(e) => return bad_request(e),
    };

    let moved = {
        let store = match open_project_store(state, &slug) {
            Ok(store) => store,
            Err(resp) => return resp,
        };
        let record = match find_record(&*store, &id) {
            Ok(record) => record,
            Err(resp) => return resp,
        };
        // The board lists the store's first project; `find_record` finds a record of any.
        match store.list_projects() {
            Ok(projects) if projects.first().is_some_and(|p| p.id == record.project) => {}
            Ok(_) => return error_json(404, "record not found"),
            Err(e) => return store_error_response(&e),
        }
        if let Err(resp) = refuse_marked(&store, &record) {
            return resp;
        }
        let gated = match store.list_transitions(&record.project) {
            Ok(ts) => ts.iter().any(|t| t.from == record.state && t.to == target),
            Err(e) => return store_error_response(&e),
        };
        if gated && let Err(resp) = refuse_stale_import(&store, &record) {
            return resp;
        }
        let outcome = match move_record(Roles::single(&*store), &record, target) {
            Err(e) => return exec_error_response(&e),
            Ok(report) => match report.outcome {
                MoveOutcome::Refused { code } => {
                    return HttpResponse::json(
                        409,
                        json!({ "ok": false, "outcome": "refused", "exit_code": code }),
                    );
                }
                MoveOutcome::Moved => "moved",
                MoveOutcome::Ungated => "ungated",
            },
        };
        current_record_json(&*store, &slug, &record.id).map(|json| (outcome, json))
    };
    match moved {
        Ok((outcome, record)) => {
            state.broadcast(
                "records",
                "record.moved",
                json!({ "record": record, "outcome": outcome }),
            );
            HttpResponse::json(
                200,
                json!({ "ok": true, "outcome": outcome, "record": record }),
            )
        }
        Err(e) => store_error_response(&e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::handle_http_request;
    use crate::{HubServerConfig, HubWsFrame};
    use fl_core::finding::Finding;
    use fl_core::log::{Attempt, AttemptStatus, PathsTouched};
    use fl_core::model::{CommandSpec, GateKind, PopulationDelivery, Regret, Selector, Transition};
    use fl_store::AnyStore;
    use std::process::Command;
    use std::time::{Duration, Instant};
    use tempfile::TempDir;

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

    fn owner(method: &str, target: &str) -> HttpRequest {
        HttpRequest::new(method, target).with_header("Authorization", "Bearer owner-tok-secret")
    }

    fn peer(method: &str, target: &str) -> HttpRequest {
        HttpRequest::new(method, target).with_header("Authorization", "Bearer peer-tok-secret")
    }

    fn fl_dir(tmp: &TempDir) -> PathBuf {
        let dir = projects_dir(tmp.path());
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn call(state: &HubState, req: &HttpRequest) -> (u16, Value) {
        let resp = handle_http_request(state, req);
        (resp.status, resp.json_value().unwrap_or(Value::Null))
    }

    fn titles(body: &Value) -> Vec<String> {
        body["records"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["title"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn a_project_store_file_follows_the_selected_engine() {
        let logical = Path::new("/h/.local/share/fl/foo.redb");
        let ferro = Path::new("/h/.local/share/fl/foo.fls");
        // No engine named: the default, FerroStore.
        assert_eq!(physical_store_file_with(logical, None), ferro);
        assert_eq!(physical_store_file_with(logical, Some("redb")), logical);
        assert_eq!(physical_store_file_with(logical, Some("ferro")), ferro);
        assert_eq!(store_extension_with(None), "fls");
        assert_eq!(store_extension_with(Some("redb")), "redb");
        assert_eq!(store_extension_with(Some("ferro")), "fls");
        // A bad engine name falls back to the logical path; the open reports the error.
        assert_eq!(physical_store_file_with(logical, Some("sqlite")), logical);
    }

    #[test]
    fn project_redb_follows_the_engine_and_refuses_an_unmigrated_project() {
        let tmp = TempDir::new().unwrap();
        let dir = fl_dir(&tmp);
        let home = tmp.path();
        // Only `foo.fls`: ferro finds it (by its logical path), redb does not.
        fs::write(dir.join("foo.fls"), b"x").unwrap();
        assert_eq!(
            project_redb_with(home, "foo", Some("ferro")).unwrap(),
            Some(dir.join("foo.redb"))
        );
        assert_eq!(
            project_redb_with(home, "foo", None).unwrap(),
            Some(dir.join("foo.redb")),
            "ferro is the default"
        );
        assert_eq!(project_redb_with(home, "foo", Some("redb")).unwrap(), None);

        // Only `bar.redb`: redb finds it; ferro refuses rather than fall back to the hub store.
        fs::write(dir.join("bar.redb"), b"x").unwrap();
        assert_eq!(
            project_redb_with(home, "bar", Some("redb")).unwrap(),
            Some(dir.join("bar.redb"))
        );
        let err = project_redb_with(home, "bar", Some("ferro")).unwrap_err();
        assert_eq!(err.status, 409);
        assert!(String::from_utf8_lossy(&err.body).contains("not been migrated"));
        // The same under the default engine.
        assert_eq!(
            project_redb_with(home, "bar", None).unwrap_err().status,
            409
        );
        // Neither file: ferro still falls back (no project by that name).
        assert_eq!(project_redb_with(home, "baz", Some("ferro")).unwrap(), None);
    }

    #[test]
    fn a_symlinked_ferro_project_store_is_refused() {
        let tmp = TempDir::new().unwrap();
        let dir = fl_dir(&tmp);
        let target = tmp.path().join("target.fls");
        fs::write(&target, b"x").unwrap();
        std::os::unix::fs::symlink(&target, dir.join("link.fls")).unwrap();
        let err = project_redb_with(tmp.path(), "link", Some("ferro")).unwrap_err();
        assert_eq!(err.status, 400);
        assert_eq!(
            project_redb_with(tmp.path(), "link", None)
                .unwrap_err()
                .status,
            400,
            "ferro is the default"
        );
        // Under redb the `.fls` link is not a store at all.
        assert_eq!(
            project_redb_with(tmp.path(), "link", Some("redb")).unwrap(),
            None
        );
    }

    #[test]
    fn available_projects_follows_the_engine() {
        let tmp = TempDir::new().unwrap();
        let dir = fl_dir(&tmp);
        fs::write(dir.join("foo.fls"), b"x").unwrap();
        fs::write(dir.join("old.redb"), b"x").unwrap();
        fs::write(dir.join("fl.fls"), b"x").unwrap();
        let home = Some(tmp.path());
        assert_eq!(
            available_projects_with(home, Some("ferro")),
            vec!["ferroloop", "foo"]
        );
        assert_eq!(
            available_projects_with(home, Some("redb")),
            vec!["ferroloop", "old"]
        );
        // No engine named: the default, FerroStore.
        assert_eq!(
            available_projects_with(home, None),
            vec!["ferroloop", "foo"]
        );
    }

    #[test]
    fn available_projects_ignores_the_migration_lock_file() {
        let tmp = TempDir::new().unwrap();
        let dir = fl_dir(&tmp);
        fs::write(dir.join("foo.fls"), b"x").unwrap();
        // Left beside a store by an automatic migration; also one with no store.
        fs::write(dir.join("foo.fls.migrate.lock"), b"").unwrap();
        fs::write(dir.join("bar.fls.migrate.lock"), b"").unwrap();
        fs::write(dir.join("baz.redb.migrate.lock"), b"").unwrap();
        for env in [Some("ferro"), None] {
            let found = available_projects_with(Some(tmp.path()), env);
            assert!(
                !found
                    .iter()
                    .any(|n| n.contains("lock") || n == "bar" || n == "baz"),
                "{env:?}: {found:?}"
            );
        }
        assert_eq!(
            available_projects_with(Some(tmp.path()), Some("ferro")),
            vec!["ferroloop", "foo"]
        );
    }

    #[test]
    fn the_code_vector_sidecar_is_not_a_project_store() {
        let tmp = TempDir::new().unwrap();
        let dir = fl_dir(&tmp);
        fs::write(dir.join("foo.fls"), b"x").unwrap();
        // `<store>.code.fls` is derived data beside a store, never a project.
        fs::write(dir.join("foo.code.fls"), b"x").unwrap();
        fs::write(dir.join("bar.code.fls"), b"x").unwrap();
        fs::write(dir.join("fl.code.fls"), b"x").unwrap();
        for env in [Some("ferro"), None] {
            let found = available_projects_with(Some(tmp.path()), env);
            assert!(
                !found.iter().any(|n| n.contains("code") || n == "bar"),
                "{env:?}: {found:?}"
            );
        }
        assert_eq!(
            available_projects_with(Some(tmp.path()), Some("ferro")),
            vec!["ferroloop", "foo"]
        );
        // `foo.code` is not a project name, so a request cannot reach it either
        // (every request slug passes `normalize_project` first).
        assert!(normalize_project(Some("foo.code")).is_err());
    }

    #[test]
    fn available_projects_reads_a_bounded_number_of_entries_under_ferro() {
        let tmp = TempDir::new().unwrap();
        let dir = fl_dir(&tmp);
        for i in 0..(MAX_PROJECT_SCAN + 50) {
            fs::write(dir.join(format!("p{i:04}.fls")), b"x").unwrap();
        }
        let found = available_projects_with(Some(tmp.path()), Some("ferro"));
        assert!(found.len() > 1, "{}", found.len());
        assert!(found.len() <= MAX_PROJECT_SCAN + 1, "{}", found.len());
        assert!(found.contains(&"ferroloop".to_string()));
    }

    /// A store at `path` holding one project and one record per `(title, state)`.
    fn seed(path: &Path, records: &[(&str, State)]) -> (AnyStore, ProjectId, Vec<RecordId>) {
        seed_on(fl_store::open_store(path).unwrap(), records)
    }

    /// [`seed`] on a store already opened (so a test can pick the engine).
    fn seed_on(store: AnyStore, records: &[(&str, State)]) -> (AnyStore, ProjectId, Vec<RecordId>) {
        let project = store.add_project("/workspace/seed").unwrap();
        let ids = records
            .iter()
            .map(|(title, state)| {
                let id = store.add_record(&project, title).unwrap();
                store.set_record_state(&id, *state).unwrap();
                id
            })
            .collect();
        (store, project, ids)
    }

    /// Open the store at `path` on the named engine, whatever the environment says.
    fn open_on(kind: fl_store::BackendKind, path: &Path) -> AnyStore {
        fl_store::open_store_with(
            path,
            &fl_store::StoreOptions {
                backend: Some(kind),
                ..fl_store::StoreOptions::default()
            },
        )
        .unwrap()
    }

    fn seed_hub(state: &HubState, records: &[(&str, State)]) -> Vec<RecordId> {
        let store = state.open_store().unwrap();
        let project = store.add_project("/workspace/seed").unwrap();
        records
            .iter()
            .map(|(title, st)| {
                let id = store.add_record(&project, title).unwrap();
                store.set_record_state(&id, *st).unwrap();
                id
            })
            .collect()
    }

    fn drain(rx: &mut tokio::sync::broadcast::Receiver<HubWsFrame>) -> Vec<HubWsFrame> {
        let mut out = Vec::new();
        while let Ok(frame) = rx.try_recv() {
            out.push(frame);
        }
        out
    }

    // --- slug rules -------------------------------------------------------------------------

    #[test]
    fn project_slugs_are_normalised_or_rejected() {
        assert_eq!(normalize_project(None).unwrap(), "ferroloop");
        assert_eq!(normalize_project(Some("")).unwrap(), "ferroloop");
        assert_eq!(normalize_project(Some("   ")).unwrap(), "ferroloop");
        assert_eq!(normalize_project(Some("Foo_Bar-9")).unwrap(), "foo_bar-9");
        assert!(normalize_project(Some(&"a".repeat(64))).is_ok());
        for bad in [
            "../x",
            "a/b",
            "a\\b",
            "..",
            ".",
            "a\0b",
            "a.b",
            "a b",
            "caf\u{e9}",
            "fl",
            "FL",
            &"a".repeat(65),
        ] {
            assert!(
                normalize_project(Some(bad)).is_err(),
                "{bad:?} must be refused"
            );
        }
    }

    #[test]
    fn a_traversing_project_is_a_400_and_nothing_outside_the_data_dir_is_read_or_made() {
        let tmp = TempDir::new().unwrap();
        let state = test_state(&tmp);
        fl_dir(&tmp);
        // A real store one level above the data dir, which `../evil` would reach.
        let evil = tmp.path().join(".local/share/evil.redb");
        {
            let (_s, _p, _) = seed(&evil, &[("secret", State::Todo)]);
        }
        // The default engine keeps the store at the sibling `.fls`.
        let evil_file = fl_store::physical_path(&evil, fl_store::DEFAULT_BACKEND);
        let before = fs::read(&evil_file).unwrap();
        for bad in [
            "../evil",
            "..%2Fevil",
            "a%2Fb",
            "a%5Cb",
            "..",
            "%00",
            "x%00y",
        ] {
            let (status, body) = call(
                &state,
                &owner("GET", &format!("/api/v1/records?project={bad}")),
            );
            assert_eq!(status, 400, "GET project={bad}: {body}");
            assert!(body["records"].is_null(), "{bad}: {body}");
            let create = owner("POST", "/api/v1/records")
                .with_json_body(&json!({ "project": percent_decode(bad), "title": "t" }));
            assert_eq!(call(&state, &create).0, 400, "POST project={bad}");
            let mv = owner("POST", &format!("/api/v1/records/1/move?project={bad}"))
                .with_json_body(&json!({ "to": "done" }));
            assert_eq!(call(&state, &mv).0, 400, "move project={bad}");
        }
        assert_eq!(
            fs::read(&evil_file).unwrap(),
            before,
            "the outside store is untouched"
        );
        assert!(
            !fl_store::store_exists(&state.config.store_path),
            "no store was opened for a bad slug"
        );
    }

    // --- store resolution -------------------------------------------------------------------

    #[test]
    fn a_project_store_on_disk_is_preferred_over_the_hub_store() {
        let tmp = TempDir::new().unwrap();
        let state = test_state(&tmp);
        seed_hub(&state, &[("hub-record", State::Todo)]);
        {
            seed(
                &fl_dir(&tmp).join("foo.redb"),
                &[("foo-record", State::Doing)],
            );
        }

        let (status, body) = call(&state, &owner("GET", "/api/v1/records?project=Foo"));
        assert_eq!(status, 200);
        assert_eq!(body["project"], "foo");
        assert_eq!(titles(&body), vec!["foo-record"]);
        assert_eq!(body["records"][0]["project"], "foo");

        // No project named, no `ferroloop.redb`: the hub store answers.
        let (_, default) = call(&state, &owner("GET", "/api/v1/records"));
        assert_eq!(default["project"], "ferroloop");
        assert_eq!(titles(&default), vec!["hub-record"]);
        // A project with no file of its own falls back to the hub store too.
        let (_, other) = call(&state, &owner("GET", "/api/v1/records?project=nofile"));
        assert_eq!(titles(&other), vec!["hub-record"]);
    }

    #[test]
    fn the_default_project_reads_ferroloop_redb_when_it_exists() {
        let tmp = TempDir::new().unwrap();
        let state = test_state(&tmp);
        seed_hub(&state, &[("hub-record", State::Todo)]);
        {
            seed(
                &fl_dir(&tmp).join("ferroloop.redb"),
                &[("own-record", State::Todo)],
            );
        }
        let (_, body) = call(&state, &owner("GET", "/api/v1/records"));
        assert_eq!(titles(&body), vec!["own-record"]);
    }

    #[test]
    fn available_projects_lists_stores_but_not_the_hub_store_and_always_the_default() {
        let tmp = TempDir::new().unwrap();
        let dir = fl_dir(&tmp);
        let mut state = test_state(&tmp);
        state.store_backend = Some(fl_store::BackendKind::Ferro);
        assert_eq!(
            available_projects(&state, Some(tmp.path())),
            vec!["ferroloop"]
        );
        assert_eq!(available_projects(&state, None), vec!["ferroloop"]);
        for name in [
            "fl.fls",
            "zeta.fls",
            "alpha.fls",
            "Upper.fls",
            "bad name.fls",
            "notes.txt",
            ".fls",
        ] {
            fs::write(dir.join(name), b"x").unwrap();
        }
        fs::create_dir(dir.join("adir.fls")).unwrap();
        fs::write(tmp.path().join("target.fls"), b"x").unwrap();
        std::os::unix::fs::symlink(tmp.path().join("target.fls"), dir.join("linked.fls")).unwrap();
        assert_eq!(
            available_projects(&state, Some(tmp.path())),
            vec!["alpha", "ferroloop", "zeta"]
        );
        // `ferroloop.fls` present does not list it twice.
        fs::write(dir.join("ferroloop.fls"), b"x").unwrap();
        assert_eq!(
            available_projects(&state, Some(tmp.path())),
            vec!["alpha", "ferroloop", "zeta"]
        );
    }

    #[test]
    fn available_projects_reads_a_bounded_number_of_entries() {
        let tmp = TempDir::new().unwrap();
        let dir = fl_dir(&tmp);
        for i in 0..(MAX_PROJECT_SCAN + 50) {
            fs::write(dir.join(format!("p{i:04}.redb")), b"x").unwrap();
        }
        let found = available_projects_with(Some(tmp.path()), Some("redb"));
        assert!(found.len() > 1, "{}", found.len());
        assert!(found.len() <= MAX_PROJECT_SCAN + 1, "{}", found.len());
        assert!(found.contains(&"ferroloop".to_string()));
    }

    #[test]
    fn a_symlinked_project_store_is_refused_and_its_target_never_read() {
        let tmp = TempDir::new().unwrap();
        let state = test_state(&tmp);
        let target = tmp.path().join("elsewhere.redb");
        {
            seed(&target, &[("outside", State::Todo)]);
        }
        // The default engine keeps the store at the sibling `.fls`; a link to it is refused.
        let target = fl_store::physical_path(&target, fl_store::DEFAULT_BACKEND);
        let before = fs::read(&target).unwrap();
        std::os::unix::fs::symlink(&target, fl_dir(&tmp).join("link.fls")).unwrap();

        let (status, body) = call(&state, &owner("GET", "/api/v1/records?project=link"));
        assert_eq!(status, 400, "{body}");
        let create = owner("POST", "/api/v1/records")
            .with_json_body(&json!({ "project": "link", "title": "t" }));
        assert_eq!(call(&state, &create).0, 400);
        assert_eq!(fs::read(&target).unwrap(), before);
    }

    #[test]
    fn a_project_store_that_cannot_be_opened_is_a_503_not_a_panic() {
        let tmp = TempDir::new().unwrap();
        let state = test_state(&tmp);
        // Not a store file: opening fails, whatever holds the file.
        fs::write(fl_dir(&tmp).join("junk.fls"), b"this is not a database").unwrap();
        let (status, body) = call(&state, &owner("GET", "/api/v1/records?project=junk"));
        assert_eq!(status, 503, "{body}");
        assert!(body["error"].is_string());
        assert_eq!(store_unavailable().status, 503);
    }

    // --- GET --------------------------------------------------------------------------------

    #[test]
    fn an_empty_store_lists_no_records_and_a_zeroed_summary() {
        let tmp = TempDir::new().unwrap();
        let state = test_state(&tmp);
        let (status, body) = call(&state, &owner("GET", "/api/v1/records"));
        assert_eq!(status, 200);
        assert_eq!(body["records"], json!([]));
        assert_eq!(body["available_projects"], json!(["ferroloop"]));
        assert_eq!(
            body["summary"],
            json!({
                "todo_count": 0, "doing_count": 0, "done_count": 0, "other_count": 0,
                "total_count": 0,
                "completion_pct": 0.0, "open_findings_count": 0, "attempts_count": 0,
                "total_cost_usd": 0.0
            })
        );
    }

    #[test]
    fn a_seeded_store_lists_records_by_handle_with_counts_findings_and_cost() {
        let tmp = TempDir::new().unwrap();
        let state = test_state(&tmp);
        let ids = seed_hub(
            &state,
            &[
                ("first", State::Done),
                ("second", State::Todo),
                ("third", State::Doing),
            ],
        );
        {
            let store = state.open_store().unwrap();
            let project = store.list_projects().unwrap().remove(0).id;
            // Two findings on `first` (one withdrawn), one open on `third`.
            let a = store
                .add_finding(Finding::raise(project.clone(), ids[0].clone(), "t", "a"))
                .unwrap();
            store
                .add_finding(Finding::raise(project.clone(), ids[0].clone(), "t", "b"))
                .unwrap();
            let mut fa = store.get_finding(&a).unwrap().unwrap();
            fa.withdraw("no").unwrap();
            store.update_finding(&fa).unwrap();
            store
                .add_finding(Finding::raise(project.clone(), ids[2].clone(), "t", "c"))
                .unwrap();
            for micros in [1_500_000, 250_000] {
                store
                    .append_attempt(Attempt {
                        id: None,
                        at: None,
                        project: project.clone(),
                        record: ids[0].clone(),
                        adapter: "claude".into(),
                        status: AttemptStatus::Completed,
                        duration_ms: 1,
                        tokens_in: 0,
                        tokens_out: 0,
                        cost_usd_micros: micros,
                        paths_touched: PathsTouched::Listed(vec![]),
                        output_excerpt: None,
                    })
                    .unwrap();
            }
        }

        let (status, body) = call(&state, &owner("GET", "/api/v1/records"));
        assert_eq!(status, 200);
        assert_eq!(titles(&body), vec!["first", "second", "third"]);
        let rs = body["records"].as_array().unwrap();
        assert_eq!(rs[0]["handle"], 1);
        assert_eq!(rs[0]["display_id"], "1");
        assert_eq!(rs[0]["id"], ids[0].as_str());
        assert_eq!(rs[0]["state"], "done");
        assert_eq!(rs[0]["tier"], "local");
        assert_eq!(rs[0]["area"], Value::Null);
        assert_eq!(
            (
                rs[0]["findings_open"].clone(),
                rs[0]["findings_total"].clone()
            ),
            (json!(1), json!(2))
        );
        assert_eq!(
            (
                rs[1]["findings_open"].clone(),
                rs[1]["findings_total"].clone()
            ),
            (json!(0), json!(0))
        );
        assert_eq!(
            (
                rs[2]["findings_open"].clone(),
                rs[2]["findings_total"].clone()
            ),
            (json!(1), json!(1))
        );
        assert_eq!(
            body["summary"],
            json!({
                "todo_count": 1, "doing_count": 1, "done_count": 1, "other_count": 0,
                "total_count": 3,
                "completion_pct": 33.3, "open_findings_count": 2, "attempts_count": 2,
                "total_cost_usd": 1.75
            })
        );
    }

    #[test]
    fn records_sort_by_handle_with_unnumbered_ones_last_then_by_id() {
        let rec = |id: &str, handle: Value| json!({ "id": id, "handle": handle });
        let mut records = vec![
            rec("b", Value::Null),
            rec("x", json!(10)),
            rec("a", Value::Null),
            rec("y", json!(2)),
            rec("z", json!(9)),
        ];
        sort_records(&mut records);
        let ids: Vec<&str> = records.iter().map(|r| r["id"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["y", "z", "x", "a", "b"]);
    }

    #[test]
    fn a_peer_may_read_records_but_an_unauthenticated_caller_may_not() {
        let tmp = TempDir::new().unwrap();
        let state = test_state(&tmp);
        assert_eq!(call(&state, &peer("GET", "/api/v1/records")).0, 200);
        assert_eq!(
            call(&state, &HttpRequest::new("GET", "/api/v1/records")).0,
            401
        );
    }

    // --- POST create ------------------------------------------------------------------------

    #[test]
    fn a_peer_cannot_create_or_move_and_no_store_is_touched() {
        let tmp = TempDir::new().unwrap();
        let state = test_state(&tmp);
        let create = peer("POST", "/api/v1/records").with_json_body(&json!({ "title": "t" }));
        assert_eq!(call(&state, &create).0, 403);
        let create_foo = peer("POST", "/api/v1/records")
            .with_json_body(&json!({ "project": "foo", "title": "t" }));
        assert_eq!(call(&state, &create_foo).0, 403);
        let mv = peer("POST", "/api/v1/records/1/move").with_json_body(&json!({ "to": "done" }));
        assert_eq!(call(&state, &mv).0, 403);
        assert!(
            !fl_store::store_exists(&state.config.store_path),
            "the hub store was never opened"
        );
        assert!(
            !projects_dir(tmp.path()).exists(),
            "nothing was made under the data dir"
        );

        // And a record that exists stays where it is after a refused peer move.
        let ids = seed_hub(&state, &[("keep", State::Todo)]);
        assert_eq!(call(&state, &mv).0, 403);
        let store = state.open_store().unwrap();
        assert_eq!(
            store.get_record(&ids[0]).unwrap().unwrap().state,
            State::Todo
        );
    }

    #[test]
    fn creating_a_record_defaults_to_todo_creates_the_project_and_broadcasts() {
        let tmp = TempDir::new().unwrap();
        let state = test_state(&tmp);
        let mut rx = state.subscribe();

        let req =
            owner("POST", "/api/v1/records").with_json_body(&json!({ "title": "  Ship it  " }));
        let (status, body) = call(&state, &req);
        assert_eq!(status, 201, "{body}");
        let rec = &body["record"];
        assert_eq!(rec["title"], "Ship it");
        assert_eq!(rec["state"], "todo");
        assert_eq!(rec["handle"], 1);
        assert_eq!(rec["project"], "ferroloop");
        assert_eq!(rec["findings_total"], 0);

        // The project was made on the fresh store, and the record is listed.
        {
            let store = state.open_store().unwrap();
            let projects = store.list_projects().unwrap();
            assert_eq!(projects.len(), 1);
            assert_eq!(projects[0].root, "/workspace/ferroloop");
        }
        let (_, listed) = call(&state, &owner("GET", "/api/v1/records"));
        assert_eq!(titles(&listed), vec!["Ship it"]);

        let frames = drain(&mut rx);
        assert_eq!(frames.len(), 1, "{frames:?}");
        assert_eq!(frames[0].channel, "records");
        assert_eq!(frames[0].event, "record.created");
        assert_eq!(frames[0].data, *rec);

        // A second record reuses the project.
        let again = owner("POST", "/api/v1/records").with_json_body(&json!({ "title": "two" }));
        assert_eq!(call(&state, &again).0, 201);
        assert_eq!(
            state.open_store().unwrap().list_projects().unwrap().len(),
            1
        );
    }

    #[test]
    fn creating_a_record_in_doing_in_a_project_store_leaves_the_store_format_alone() {
        creating_a_record_in_doing_leaves_the_format_alone(fl_store::BackendKind::Ferro);
    }

    /// The rollback engine keeps the precondition the format check needs: a new redb store is
    /// older than the routing format, so a raise would show.
    #[test]
    fn creating_a_record_in_doing_in_a_redb_project_store_leaves_the_store_format_alone() {
        creating_a_record_in_doing_leaves_the_format_alone(fl_store::BackendKind::Redb);
    }

    fn creating_a_record_in_doing_leaves_the_format_alone(kind: fl_store::BackendKind) {
        let tmp = TempDir::new().unwrap();
        let mut state = test_state(&tmp);
        state.store_backend = Some(kind);
        let path = fl_dir(&tmp).join("foo.redb");
        let format_before = {
            let (store, _, _) = seed_on(open_on(kind, &path), &[("existing", State::Todo)]);
            store.format_version().unwrap()
        };
        if kind == fl_store::BackendKind::Redb {
            assert!(format_before < Some(fl_store::FORMAT_WITH_ROUTING));
        }
        let req = owner("POST", "/api/v1/records")
            .with_json_body(&json!({ "project": "FOO", "title": "board", "state": "doing" }));
        let (status, body) = call(&state, &req);
        assert_eq!(status, 201, "{body}");
        assert_eq!(body["record"]["state"], "doing");
        assert_eq!(body["record"]["area"], Value::Null);
        assert_eq!(body["record"]["project"], "foo");
        assert_eq!(body["record"]["handle"], 2);
        let (_, listed) = call(&state, &owner("GET", "/api/v1/records?project=foo"));
        assert_eq!(titles(&listed), vec!["existing", "board"]);
        assert!(
            !state.config.store_path.exists(),
            "the hub store was not used"
        );
        // An empty area is no area.
        let none = owner("POST", "/api/v1/records")
            .with_json_body(&json!({ "project": "foo", "title": "x", "area": "  " }));
        assert_eq!(call(&state, &none).1["record"]["area"], Value::Null);
        // ⚠ Creating a record is not a format upgrade: an older fl still reads the store.
        let store = open_on(kind, &path);
        assert_eq!(store.format_version().unwrap(), format_before);
    }

    #[test]
    fn a_create_with_an_area_is_a_422_and_writes_nothing() {
        a_create_with_an_area_writes_nothing(fl_store::BackendKind::Ferro);
    }

    #[test]
    fn a_create_with_an_area_in_a_redb_project_store_is_a_422_and_writes_nothing() {
        a_create_with_an_area_writes_nothing(fl_store::BackendKind::Redb);
    }

    fn a_create_with_an_area_writes_nothing(kind: fl_store::BackendKind) {
        let tmp = TempDir::new().unwrap();
        let mut state = test_state(&tmp);
        state.store_backend = Some(kind);
        let path = fl_dir(&tmp).join("foo.redb");
        let format_before = {
            let (store, _, _) = seed_on(open_on(kind, &path), &[("existing", State::Todo)]);
            store.format_version().unwrap()
        };
        if kind == fl_store::BackendKind::Redb {
            assert!(format_before < Some(fl_store::FORMAT_WITH_ROUTING));
        }
        for area in ["frontend", " frontend ", "Bad Area", "a\u{0}b"] {
            let req = owner("POST", "/api/v1/records").with_json_body(
                &json!({ "project": "foo", "title": "board", "area": area, "state": "doing" }),
            );
            let (status, body) = call(&state, &req);
            assert_eq!(status, 422, "{area:?}: {body}");
            assert_eq!(
                body,
                json!({ "error": "area is not supported by the hub yet" })
            );
        }
        let (_, listed) = call(&state, &owner("GET", "/api/v1/records?project=foo"));
        assert_eq!(titles(&listed), vec!["existing"]);
        let store = open_on(kind, &path);
        assert_eq!(store.format_version().unwrap(), format_before);
    }

    #[test]
    fn a_record_stored_with_an_area_still_reads_back_with_it() {
        let tmp = TempDir::new().unwrap();
        let state = test_state(&tmp);
        {
            let (store, project, _) = seed(&fl_dir(&tmp).join("foo.redb"), &[]);
            store
                .add_record_with_area(&project, "routed", Some("frontend"))
                .unwrap();
        }
        let (_, listed) = call(&state, &owner("GET", "/api/v1/records?project=foo"));
        assert_eq!(listed["records"][0]["area"], "frontend");
    }

    #[test]
    fn create_validates_title_state_area_and_fields() {
        let tmp = TempDir::new().unwrap();
        let state = test_state(&tmp);
        let long = "x".repeat(501);
        let ok_long = "x".repeat(500);
        let bad_bodies = [
            json!({ "title": "" }),
            json!({ "title": "   " }),
            json!({ "title": long }),
            json!({ "title": "a\u{7}b" }),
            json!({ "title": "a\nb" }),
            json!({ "title": "t", "state": "review" }),
            json!({ "title": "t", "state": "bogus" }),
            json!({ "title": "t", "area": 5 }),
            json!({ "title": "t", "extra": 1 }),
            json!({ "title": 5 }),
            json!({ "title": null }),
            json!({}),
            json!(["title"]),
        ];
        for body in bad_bodies {
            let (status, resp) = call(
                &state,
                &owner("POST", "/api/v1/records").with_json_body(&body),
            );
            assert_eq!(status, 400, "{body}: {resp}");
        }
        let malformed = owner("POST", "/api/v1/records").with_body(b"{not json".to_vec());
        assert_eq!(call(&state, &malformed).0, 400);
        assert!(
            !fl_store::store_exists(&state.config.store_path),
            "a refused body opens no store"
        );
        let ok = owner("POST", "/api/v1/records").with_json_body(&json!({ "title": ok_long }));
        assert_eq!(call(&state, &ok).0, 201);
    }

    // --- POST move --------------------------------------------------------------------------

    fn git_repo() -> TempDir {
        let d = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .args(args)
                    .current_dir(d.path())
                    .status()
                    .unwrap()
                    .success()
            );
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        fs::write(d.path().join("a.rs"), "fn a() {}").unwrap();
        git(&["add", "a.rs"]);
        git(&["commit", "-qm", "first"]);
        d
    }

    /// A record whose `todo` to `done` move is covered by one gate that runs `program`.
    fn gated_record(state: &HubState, root: &Path, program: &str) -> RecordId {
        let store = state.open_store().unwrap();
        let head = fl_exec::git::Git::head(root).unwrap();
        let project = store.add_project(&root.display().to_string()).unwrap();
        let gate = store
            .add_gate(
                &project,
                "check",
                GateKind::Command(CommandSpec {
                    program: program.into(),
                    args: vec![],
                    delivery: PopulationDelivery::Args,
                    timeout_secs: 10,
                    pass_codes: vec![0],
                }),
                Selector::Glob {
                    pattern: "*.rs".into(),
                },
                1,
                &head,
                "tester",
            )
            .unwrap();
        store
            .add_transition(Transition {
                project: project.clone(),
                name: "launch".into(),
                from: State::Todo,
                to: State::Done,
                regret: Regret::Low,
                gates: vec![gate],
            })
            .unwrap();
        store.add_record(&project, "gated").unwrap()
    }

    fn state_of(state: &HubState, id: &RecordId) -> State {
        state
            .open_store()
            .unwrap()
            .get_record(id)
            .unwrap()
            .unwrap()
            .state
    }

    #[test]
    fn moving_by_handle_and_by_iri_changes_state_and_broadcasts() {
        let tmp = TempDir::new().unwrap();
        let state = test_state(&tmp);
        let ids = seed_hub(&state, &[("one", State::Todo), ("two", State::Todo)]);
        let mut rx = state.subscribe();

        let (status, body) = call(
            &state,
            &owner("POST", "/api/v1/records/1/move").with_json_body(&json!({ "to": "doing" })),
        );
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["ok"], true);
        assert_eq!(body["outcome"], "ungated");
        assert_eq!(body["record"]["state"], "doing");
        assert_eq!(body["record"]["handle"], 1);
        assert_eq!(state_of(&state, &ids[0]), State::Doing);

        let frames = drain(&mut rx);
        assert_eq!(frames.len(), 1, "{frames:?}");
        assert_eq!(
            (frames[0].channel.as_str(), frames[0].event.as_str()),
            ("records", "record.moved")
        );
        assert_eq!(frames[0].data["outcome"], "ungated");
        assert_eq!(frames[0].data["record"], body["record"]);

        // By IRI (percent-encoded colons), with the project in the query.
        let target = format!(
            "/api/v1/records/{}/move?project=ferroloop",
            ids[1].as_str().replace(':', "%3A")
        );
        let (status, body) = call(
            &state,
            &owner("POST", &target).with_json_body(&json!({ "to": "done" })),
        );
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["record"]["state"], "done");
        assert_eq!(state_of(&state, &ids[1]), State::Done);
        // A body project beats the query, and the one in the body is validated.
        let both = owner("POST", "/api/v1/records/2/move?project=nofile")
            .with_json_body(&json!({ "to": "todo", "project": "../x" }));
        assert_eq!(call(&state, &both).0, 400);
    }

    #[test]
    fn moving_through_a_passing_gate_is_moved() {
        let tmp = TempDir::new().unwrap();
        let state = test_state(&tmp);
        let repo = git_repo();
        let id = gated_record(&state, repo.path(), "true");
        let (status, body) = call(
            &state,
            &owner("POST", "/api/v1/records/1/move").with_json_body(&json!({ "to": "done" })),
        );
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["outcome"], "moved");
        assert_eq!(state_of(&state, &id), State::Done);
    }

    #[test]
    fn a_refused_move_is_a_409_with_the_exit_code_and_the_record_stays() {
        let tmp = TempDir::new().unwrap();
        let state = test_state(&tmp);
        let repo = git_repo();
        let id = gated_record(&state, repo.path(), "false");
        let mut rx = state.subscribe();
        let (status, body) = call(
            &state,
            &owner("POST", "/api/v1/records/1/move").with_json_body(&json!({ "to": "done" })),
        );
        assert_eq!(status, 409, "{body}");
        assert_eq!(
            body,
            json!({ "ok": false, "outcome": "refused", "exit_code": 1 })
        );
        assert_eq!(state_of(&state, &id), State::Todo);
        assert!(
            drain(&mut rx).is_empty(),
            "a refused move broadcasts nothing"
        );
    }

    #[test]
    fn move_errors_map_to_statuses() {
        assert_eq!(
            exec_error_response(&ExecError::Unpublished("x".into())).status,
            503
        );
        assert_eq!(
            exec_error_response(&ExecError::PublishRefused("x".into())).status,
            409
        );
        assert_eq!(
            exec_error_response(&ExecError::BadSelector("x".into())).status,
            422
        );
        // A record moved by someone else mid-gate is a conflict, not a server fault.
        let stale = exec_error_response(&ExecError::StaleMove("record moved".into()));
        assert_eq!(stale.status, 409);
        let body = stale.json_value().unwrap();
        assert_eq!(body["ok"], false);
        assert_eq!(body["error"], "record moved");
        for e in [
            ExecError::Store("x".into()),
            ExecError::Git("x".into()),
            ExecError::Timeout("x".into(), 1),
        ] {
            let resp = exec_error_response(&e);
            assert_eq!(resp.status, 500);
            assert_eq!(resp.json_value().unwrap()["ok"], false);
        }
    }

    #[test]
    fn a_gate_that_cannot_run_is_an_error_response_not_a_panic() {
        let tmp = TempDir::new().unwrap();
        let state = test_state(&tmp);
        // The project root is not a git repository, so the gate cannot even read HEAD.
        let not_git = tempfile::tempdir().unwrap();
        {
            let store = state.open_store().unwrap();
            let project = store
                .add_project(&not_git.path().display().to_string())
                .unwrap();
            let gate = store
                .add_gate(
                    &project,
                    "g",
                    GateKind::Command(CommandSpec {
                        program: "true".into(),
                        args: vec![],
                        delivery: PopulationDelivery::Args,
                        timeout_secs: 10,
                        pass_codes: vec![0],
                    }),
                    Selector::Glob {
                        pattern: "*".into(),
                    },
                    1,
                    "deadbeef",
                    "tester",
                )
                .unwrap();
            store
                .add_transition(Transition {
                    project: project.clone(),
                    name: "launch".into(),
                    from: State::Todo,
                    to: State::Done,
                    regret: Regret::Low,
                    gates: vec![gate],
                })
                .unwrap();
            store.add_record(&project, "r").unwrap();
        }
        let (status, body) = call(
            &state,
            &owner("POST", "/api/v1/records/1/move").with_json_body(&json!({ "to": "done" })),
        );
        assert_eq!(status, 500, "{body}");
        assert_eq!(body["ok"], false);
    }

    #[test]
    fn a_move_that_names_nothing_real_is_refused() {
        let tmp = TempDir::new().unwrap();
        let state = test_state(&tmp);
        seed_hub(&state, &[("one", State::Todo)]);
        let mv = |id: &str, body: Value| {
            call(
                &state,
                &owner("POST", &format!("/api/v1/records/{id}/move")).with_json_body(&body),
            )
        };
        // Unknown handle, unknown IRI, an IRI of the wrong kind of item.
        assert_eq!(mv("999", json!({ "to": "done" })).0, 404);
        assert_eq!(
            mv(
                "urn:uuid:00000000-0000-7000-8000-000000000999",
                json!({ "to": "done" })
            )
            .0,
            404
        );
        let project_iri = {
            let store = state.open_store().unwrap();
            store
                .list_projects()
                .unwrap()
                .remove(0)
                .id
                .as_str()
                .to_string()
        };
        assert_eq!(
            mv(&project_iri.replace(':', "%3A"), json!({ "to": "done" })).0,
            404
        );
        // Bad state, missing state, extra field, malformed id.
        assert_eq!(mv("1", json!({ "to": "review" })).0, 400);
        assert_eq!(mv("1", json!({ "to": "bogus" })).0, 400);
        assert_eq!(mv("1", json!({})).0, 400);
        assert_eq!(mv("1", json!({ "to": "done", "extra": true })).0, 400);
        assert_eq!(mv("not-an-iri", json!({ "to": "done" })).0, 400);
        assert_eq!(
            mv("99999999999999999999999", json!({ "to": "done" })).0,
            400
        );
        for bad in [
            "..",
            "a%2Fb",
            "a%5Cb",
            "%00",
            "..%2F..%2Fetc",
            &"1".repeat(300),
        ] {
            let (status, body) = mv(bad, json!({ "to": "done" }));
            assert_eq!(status, 400, "{bad}: {body}");
        }
    }

    #[test]
    fn record_ids_in_urls_are_decoded_and_bounded() {
        assert_eq!(decode_record_id("7").unwrap(), "7");
        assert_eq!(
            decode_record_id("urn%3Auuid%3Aabc").unwrap(),
            "urn:uuid:abc"
        );
        for bad in [
            "",
            "..",
            "a%2Fb",
            "a%5Cb",
            "a%00b",
            "a%0Ab",
            &"x".repeat(257),
        ] {
            assert!(decode_record_id(bad).is_err(), "{bad:?}");
        }
    }
    // --- the CLI's guards around a move -----------------------------------------------------

    /// A gate program that leaves a file behind when it runs, and the file's path.
    fn sentinel_gate(dir: &Path) -> (PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let sentinel = dir.join("gate-ran");
        let script = dir.join("gate.sh");
        fs::write(
            &script,
            format!("#!/bin/sh\ntouch '{}'\nexit 0\n", sentinel.display()),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        (script, sentinel)
    }

    fn mark(state: &HubState, id: &RecordId) {
        state
            .open_store()
            .unwrap()
            .mark(
                id.iri(),
                &fl_core::escalation::Mark {
                    by: "tester".into(),
                    reason: "test".into(),
                    at_ms: 1,
                },
            )
            .unwrap();
    }

    #[test]
    fn a_marked_record_is_a_409_before_any_gate_runs_and_keeps_its_state() {
        let tmp = TempDir::new().unwrap();
        let state = test_state(&tmp);
        let repo = git_repo();
        let scripts = tempfile::tempdir().unwrap();
        let (script, sentinel) = sentinel_gate(scripts.path());
        let id = gated_record(&state, repo.path(), &script.display().to_string());
        mark(&state, &id);
        let mut rx = state.subscribe();

        let (status, body) = call(
            &state,
            &owner("POST", "/api/v1/records/1/move").with_json_body(&json!({ "to": "done" })),
        );
        assert_eq!(status, 409, "{body}");
        assert_eq!(body["ok"], false);
        let error = body["error"].as_str().unwrap();
        assert!(error.contains("marked escalating"), "{error}");
        assert!(error.contains("fl record escalate 1"), "{error}");
        assert!(!sentinel.exists(), "a gate ran for a marked record");
        assert_eq!(state_of(&state, &id), State::Todo);
        assert!(
            drain(&mut rx).is_empty(),
            "a refused move broadcasts nothing"
        );
        {
            let store = state.open_store().unwrap();
            let project = store.list_projects().unwrap().remove(0).id;
            assert!(
                store.attempts(&project).unwrap().is_empty(),
                "no evidence was written"
            );
        }

        // Unmarked, the same move runs its gate and lands.
        state.open_store().unwrap().unmark(id.iri()).unwrap();
        let (status, body) = call(
            &state,
            &owner("POST", "/api/v1/records/1/move").with_json_body(&json!({ "to": "done" })),
        );
        assert_eq!(status, 200, "{body}");
        assert!(sentinel.exists());
    }

    #[test]
    fn an_escalated_record_is_a_409_before_any_gate_runs() {
        let tmp = TempDir::new().unwrap();
        let state = test_state(&tmp);
        let repo = git_repo();
        let scripts = tempfile::tempdir().unwrap();
        let (script, sentinel) = sentinel_gate(scripts.path());
        let id = gated_record(&state, repo.path(), &script.display().to_string());
        mark(&state, &id);
        {
            let store = state.open_store().unwrap();
            let to = Iri::parse("urn:uuid:00000000-0000-7000-8000-0000000000aa").unwrap();
            store.tombstone(id.iri(), &to).unwrap();
        }
        let (status, body) = call(
            &state,
            &owner(
                "POST",
                &format!("/api/v1/records/{}/move", id.as_str().replace(':', "%3A")),
            )
            .with_json_body(&json!({ "to": "done" })),
        );
        assert_eq!(status, 409, "{body}");
        assert_eq!(body["ok"], false);
        assert!(
            body["error"].as_str().unwrap().contains("was escalated"),
            "{body}"
        );
        assert!(!sentinel.exists(), "a gate ran for an escalated record");
    }

    #[test]
    fn a_store_that_refuses_the_state_change_as_escalating_is_a_409_not_a_500() {
        let tmp = TempDir::new().unwrap();
        let state = test_state(&tmp);
        let ids = seed_hub(&state, &[("one", State::Todo)]);
        mark(&state, &ids[0]);
        let store = state.open_store().unwrap();
        let record = store.get_record(&ids[0]).unwrap().unwrap();
        // The race the early guard cannot see: the mark lands after it, before the write.
        let err = match move_record(Roles::single(&*store), &record, State::Done) {
            Err(e) => e,
            Ok(_) => panic!("a marked record moved"),
        };
        assert!(matches!(err, ExecError::Escalation(_)), "{err:?}");
        let resp = exec_error_response(&err);
        assert_eq!(resp.status, 409);
        let body = resp.json_value().unwrap();
        assert_eq!(body["ok"], false);
        assert!(body["error"].as_str().unwrap().contains("escalating"));
    }

    /// A project whose manifest this store imported, with one passing gate on `todo` to `done`:
    /// the path of the working tree, the project store, and the record's handle.
    fn imported_project(tmp: &TempDir) -> (TempDir, PathBuf, fl_store::manifest::Manifest) {
        let repo = git_repo();
        let root = repo.path().display().to_string();
        let head = fl_exec::git::Git::head(repo.path()).unwrap();
        let author = fl_store::open_store(&tmp.path().join("author.redb")).unwrap();
        let project = author.add_project(&root).unwrap();
        let gate = author
            .add_gate(
                &project,
                "check",
                GateKind::Command(CommandSpec {
                    program: "true".into(),
                    args: vec![],
                    delivery: PopulationDelivery::Args,
                    timeout_secs: 10,
                    pass_codes: vec![0],
                }),
                Selector::Glob {
                    pattern: "*.rs".into(),
                },
                1,
                &head,
                "tester",
            )
            .unwrap();
        author
            .add_transition(Transition {
                project: project.clone(),
                name: "launch".into(),
                from: State::Todo,
                to: State::Done,
                regret: Regret::Low,
                gates: vec![gate],
            })
            .unwrap();
        let manifest = author.export_manifest(&project, "c1", 1, None).unwrap();
        fs::create_dir_all(repo.path().join(".fl")).unwrap();
        fs::write(
            repo.path().join(fl_store::manifest::MANIFEST_PATH),
            manifest.to_json(),
        )
        .unwrap();
        let path = fl_dir(tmp).join("foo.redb");
        let store = fl_store::open_store(&path).unwrap();
        store.import_manifest(&manifest, &root).unwrap();
        store.add_record(&project, "imported").unwrap();
        let newer = author.export_manifest(&project, "c2", 2, None).unwrap();
        (repo, path, newer)
    }

    #[test]
    fn a_gated_move_on_a_stale_manifest_import_is_a_409_and_runs_no_gate() {
        let tmp = TempDir::new().unwrap();
        let state = test_state(&tmp);
        let (repo, path, newer) = imported_project(&tmp);
        let mv = || {
            call(
                &state,
                &owner("POST", "/api/v1/records/1/move?project=foo")
                    .with_json_body(&json!({ "to": "done" })),
            )
        };
        // The working tree's manifest is the imported one: the move runs.
        let (status, body) = mv();
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["outcome"], "moved");

        // Put the record back, then change the manifest under the store.
        {
            let store = fl_store::open_store(&path).unwrap();
            let record = store
                .list_records(&store.list_projects().unwrap()[0].id)
                .unwrap();
            store.set_record_state(&record[0].id, State::Todo).unwrap();
        }
        fs::write(
            repo.path().join(fl_store::manifest::MANIFEST_PATH),
            newer.to_json(),
        )
        .unwrap();
        let (status, body) = mv();
        assert_eq!(status, 409, "{body}");
        assert_eq!(body["ok"], false);
        assert!(
            body["error"]
                .as_str()
                .unwrap()
                .contains("changed since this store imported it"),
            "{body}"
        );
        {
            let store = fl_store::open_store(&path).unwrap();
            let project = store.list_projects().unwrap().remove(0).id;
            assert_eq!(store.list_records(&project).unwrap()[0].state, State::Todo);
        }

        // An ungated move on the same stale import is not held up by it.
        let ungated = call(
            &state,
            &owner("POST", "/api/v1/records/1/move?project=foo")
                .with_json_body(&json!({ "to": "doing" })),
        );
        assert_eq!(ungated.0, 200, "{}", ungated.1);
    }

    #[test]
    fn a_record_of_another_project_in_the_store_is_not_found_to_move() {
        let tmp = TempDir::new().unwrap();
        let state = test_state(&tmp);
        let ids = seed_hub(&state, &[("listed", State::Todo)]);
        let other = {
            let store = state.open_store().unwrap();
            let other = store.add_project("/workspace/other").unwrap();
            store.add_record(&other, "unlisted").unwrap()
        };
        let (_, listed) = call(&state, &owner("GET", "/api/v1/records"));
        assert_eq!(titles(&listed), vec!["listed"]);

        let target = format!(
            "/api/v1/records/{}/move",
            other.as_str().replace(':', "%3A")
        );
        let (status, body) = call(
            &state,
            &owner("POST", &target).with_json_body(&json!({ "to": "done" })),
        );
        assert_eq!(status, 404, "{body}");
        assert_eq!(state_of(&state, &other), State::Todo);
        let (status, _) = call(
            &state,
            &owner("POST", "/api/v1/records/1/move").with_json_body(&json!({ "to": "done" })),
        );
        assert_eq!(status, 200);
        assert_eq!(state_of(&state, &ids[0]), State::Done);
    }

    #[test]
    fn the_summary_counts_states_without_a_column_as_other() {
        let tmp = TempDir::new().unwrap();
        let state = test_state(&tmp);
        seed_hub(
            &state,
            &[
                ("a", State::Todo),
                ("b", State::Doing),
                ("c", State::Done),
                ("d", State::Review),
                ("e", State::NeedsHuman),
            ],
        );
        let (_, body) = call(&state, &owner("GET", "/api/v1/records"));
        let s = &body["summary"];
        assert_eq!(s["other_count"], 2);
        assert_eq!(s["total_count"], 5);
        let sum: u64 = ["todo_count", "doing_count", "done_count", "other_count"]
            .iter()
            .map(|k| s[k].as_u64().unwrap())
            .sum();
        assert_eq!(sum, 5);
    }

    // --- bounded waits and per-project locks ------------------------------------------------

    /// Generous: the budget is about 2 s, and the old behaviour was minutes.
    const BOUND: Duration = Duration::from_secs(6);

    #[test]
    fn a_redb_project_held_by_another_process_is_a_503_busy_within_a_bounded_time() {
        let tmp = TempDir::new().unwrap();
        let mut state = test_state(&tmp);
        // The rollback engine, named explicitly: redb takes one process per file.
        state.store_backend = Some(fl_store::BackendKind::Redb);
        let path = fl_dir(&tmp).join("foo.redb");
        let holder = fl_store::open_redb(&path).unwrap();
        // `holder` is open on the file, as a CLI process would be.
        let started = Instant::now();
        let busy = state.open_project_store_at(&path);
        assert!(started.elapsed() < BOUND, "took {:?}", started.elapsed());
        assert!(matches!(busy, Err(ProjectOpenError::Busy)));
        drop(holder);
        assert!(state.open_project_store_at(&path).is_ok());
    }

    #[test]
    fn a_ferro_project_held_by_another_handle_is_not_busy() {
        let tmp = TempDir::new().unwrap();
        let state = test_state(&tmp);
        let path = fl_dir(&tmp).join("foo.redb");
        let (_holder, _, _) = seed(&path, &[("x", State::Todo)]);
        // FerroStore is multi-process safe: a second handle on the file opens at once.
        let (status, body) = call(&state, &peer("GET", "/api/v1/records?project=foo"));
        assert_eq!(status, 200, "{body}");
        assert_eq!(titles(&body), vec!["x"]);
    }

    #[test]
    fn a_project_whose_lock_is_held_is_a_503_busy_within_a_bounded_time() {
        let tmp = TempDir::new().unwrap();
        let state = test_state(&tmp);
        let path = fl_dir(&tmp).join("foo.redb");
        {
            seed(&path, &[("x", State::Todo)]);
        }
        let held = state.open_project_store_at(&path).unwrap();
        let started = Instant::now();
        let (status, body) = call(&state, &owner("GET", "/api/v1/records?project=foo"));
        assert!(started.elapsed() < BOUND, "took {:?}", started.elapsed());
        assert_eq!(status, 503, "{body}");
        assert_eq!(body, json!({ "error": "project busy" }));
        drop(held);
        assert_eq!(
            call(&state, &owner("GET", "/api/v1/records?project=foo")).0,
            200
        );
    }

    #[test]
    fn a_busy_project_does_not_block_another_project_or_the_hub_store() {
        let tmp = TempDir::new().unwrap();
        let state = test_state(&tmp);
        let foo = fl_dir(&tmp).join("foo.redb");
        {
            seed(&foo, &[("foo-record", State::Todo)]);
            seed(
                &fl_dir(&tmp).join("bar.redb"),
                &[("bar-record", State::Todo)],
            );
        }
        seed_hub(&state, &[("hub-record", State::Todo)]);
        // `foo` is held, as a long move on it would hold it.
        let held = state.open_project_store_at(&foo).unwrap();
        std::thread::scope(|scope| {
            for (query, want) in [
                ("/api/v1/records?project=bar", "bar-record"),
                ("/api/v1/records", "hub-record"),
            ] {
                let state = &state;
                let worker = scope.spawn(move || {
                    let started = Instant::now();
                    let (status, body) = call(state, &owner("GET", query));
                    (started.elapsed(), status, titles(&body), want)
                });
                let (took, status, got, want) = worker.join().unwrap();
                assert_eq!(status, 200, "{query}");
                assert_eq!(got, vec![want], "{query}");
                assert!(
                    took < Duration::from_secs(1),
                    "{query} waited {took:?} behind a held project"
                );
            }
        });
        drop(held);
    }

    #[test]
    fn requests_queued_behind_a_held_project_all_get_their_turn() {
        // Four waiters share one `notify_one` per release: each must pass the baton on, none
        // may sleep through the wake-up and time out while the file is free.
        let tmp = TempDir::new().unwrap();
        let state = test_state(&tmp);
        let path = fl_dir(&tmp).join("queue.redb");
        seed(&path, &[("only", State::Todo)]);
        let held = state.open_project_store_at(&path).unwrap();
        let got = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..4)
                .map(|_| {
                    scope.spawn(|| {
                        let store = state.open_project_store_at(&path).map_err(|_| "busy")?;
                        std::thread::sleep(Duration::from_millis(40));
                        let n = store.list_projects().map(|p| p.len()).map_err(|_| "err")?;
                        Ok::<usize, &str>(n)
                    })
                })
                .collect();
            std::thread::sleep(Duration::from_millis(150));
            drop(held);
            workers
                .into_iter()
                .map(|w| w.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert_eq!(got, vec![Ok(1); 4], "{got:?}");
    }

    #[test]
    fn concurrent_creates_on_one_project_neither_lose_nor_duplicate_records() {
        let tmp = TempDir::new().unwrap();
        let state = test_state(&tmp);
        seed(&fl_dir(&tmp).join("busy.redb"), &[]);
        const WRITERS: usize = 8;
        let statuses: Vec<(u16, Value)> = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..WRITERS)
                .map(|i| {
                    let state = &state;
                    scope.spawn(move || {
                        let req = owner("POST", "/api/v1/records").with_json_body(
                            &json!({ "project": "busy", "title": format!("r{i}") }),
                        );
                        call(state, &req)
                    })
                })
                .collect();
            workers.into_iter().map(|w| w.join().unwrap()).collect()
        });
        let created: Vec<&Value> = statuses
            .iter()
            .filter(|(status, _)| *status == 201)
            .map(|(_, body)| &body["record"])
            .collect();
        for (status, body) in &statuses {
            assert!(matches!(*status, 201 | 503), "{status} {body}");
        }
        assert!(!created.is_empty());
        let mut handles: Vec<u64> = created
            .iter()
            .map(|r| r["handle"].as_u64().unwrap())
            .collect();
        handles.sort_unstable();
        handles.dedup();
        assert_eq!(handles.len(), created.len(), "handles must be unique");
        let (_, listed) = call(&state, &owner("GET", "/api/v1/records?project=busy"));
        assert_eq!(
            listed["records"].as_array().unwrap().len(),
            created.len(),
            "every 201 is listed, and nothing else is"
        );
        assert_eq!(listed["summary"]["total_count"], created.len());
    }

    #[test]
    fn a_move_to_the_state_a_record_is_already_in_is_harmless() {
        let tmp = TempDir::new().unwrap();
        let state = test_state(&tmp);
        seed(&fl_dir(&tmp).join("same.redb"), &[("a", State::Doing)]);
        let (status, body) = call(
            &state,
            &owner("POST", "/api/v1/records/1/move?project=same")
                .with_json_body(&json!({ "to": "doing" })),
        );
        assert!(matches!(status, 200 | 409 | 422), "{status} {body}");
        let (_, listed) = call(&state, &owner("GET", "/api/v1/records?project=same"));
        assert_eq!(listed["records"][0]["state"], "doing");
    }

    #[test]
    fn project_slug_tricks_in_query_and_body_never_reach_another_path() {
        let tmp = TempDir::new().unwrap();
        let state = test_state(&tmp);
        // A store outside `.local/share/fl` that a traversal would reach.
        let outside = tmp.path().join("outside.redb");
        seed(&outside, &[("secret", State::Todo)]);
        for target in [
            "/api/v1/records?project=..%2F..%2F..%2Foutside",
            "/api/v1/records?project=%2e%2e",
            "/api/v1/records?project=a%00b",
            "/api/v1/records?project=%2Ftmp%2Foutside",
        ] {
            let (status, body) = call(&state, &owner("GET", target));
            assert_eq!(status, 400, "{target}: {body}");
        }
        let req = owner("POST", "/api/v1/records")
            .with_json_body(&json!({ "project": "../../outside", "title": "x" }));
        assert_eq!(call(&state, &req).0, 400);
        let req = owner("POST", "/api/v1/records/1/move")
            .with_json_body(&json!({ "project": "..\\outside", "to": "done" }));
        assert_eq!(call(&state, &req).0, 400);
        // The outside store is untouched.
        let store = fl_store::open_store(&outside).unwrap();
        let project = store.list_projects().unwrap().remove(0);
        assert_eq!(store.list_records(&project.id).unwrap().len(), 1);
    }
}
