//! Hosted agent session lifecycle: spin-up, two-phase decommission, and restart of sessions that
//! FerroDeck and `fl agent` manage (Issue #82 / #69).
//!
//! The functions are generic over the agent registry and state store, so the hub's HTTP handlers
//! (`crates/hub/src/api.rs`) and `fl agent spin-up|decommission` share one implementation. Hub-only
//! concerns (the in-memory operator prompt queue) reach this module through caller closures or the
//! returned `handshake_dispatched` flag.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use fl_core::agent::{AgentRecord, AgentRole, AgentStatus, RegisterAgentArgs};
use fl_core::state::{
    MergeMode, RuntimeEntry, SensitivityPatch, StateConcern, StatePayload, StateScope,
    UpsertStateArgs, validate_slug,
};
use fl_core::store::{AgentRegistry, StateStore, StoreError};
use fl_exec::stamp;
use fl_wire::dispatcher::dispatch_operator_prompt_to_route;
use fl_wire::identity::{RouteTable, VendorRoute, default_routes_path, legacy_routes_path};
use serde::{Deserialize, Serialize};

use crate::api::{
    canonical_reforge_prompt, reforge_role_label, route_matches_session_id,
    synthesize_decommission_prompt,
};

/// Roles `spin-up` accepts (the FerroDeck role list).
pub const SPIN_UP_ROLES: [&str; 4] = ["lead", "assistant", "ui-developer", "lab-manager"];
/// Vendors `spin-up` accepts; the first segment of a hosted `session_id`.
pub const SPIN_UP_VENDORS: [&str; 5] = ["claude", "antigravity", "codex", "grok", "muse"];
/// Node a session lands on when the request names none.
pub const DEFAULT_NODE_ID: &str = "ai-lab-0";

/// `session_id` prefix of every hosted-session runtime entry.
const HOSTED_SLUG_PREFIX: &str = "hosted-session-";
/// Longest persona that still fits `hosted-session-<vendor>-<persona>` in the 64-char state slug.
const MAX_PERSONA_LEN: usize = 36;
const MAX_FIELD_LEN: usize = 128;

pub const STATUS_RUNNING: &str = "running";
pub const STATUS_DECOMMISSIONING: &str = "decommissioning";
pub const STATUS_DECOMMISSIONED: &str = "decommissioned";

/// Failure of a lifecycle operation, with the HTTP status the hub reports for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleError {
    /// The request is malformed (400).
    Invalid(String),
    /// No such session or agent (404).
    NotFound(String),
    /// The caller's authority does not cover the request (403).
    Forbidden(String),
    /// The request conflicts with registry state (409).
    Conflict(String),
    /// The store failed (500).
    Store(String),
}

impl LifecycleError {
    pub fn status(&self) -> u16 {
        match self {
            Self::Invalid(_) => 400,
            Self::NotFound(_) => 404,
            Self::Forbidden(_) => 403,
            Self::Conflict(_) => 409,
            Self::Store(_) => 500,
        }
    }
}

impl std::fmt::Display for LifecycleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(m)
            | Self::NotFound(m)
            | Self::Forbidden(m)
            | Self::Conflict(m)
            | Self::Store(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for LifecycleError {}

impl From<StoreError> for LifecycleError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::LeadTaken { .. } | StoreError::AgentCollision { .. } => {
                Self::Conflict(e.to_string())
            }
            other => Self::Store(other.to_string()),
        }
    }
}

/// Where lifecycle side effects go. `spawn_runtime` gates every real `tmux` call.
#[derive(Debug, Clone, Copy)]
pub struct LifecycleEnv<'a> {
    /// Replaces `$HOME` when it locates the wire route table (tests, hub `home_override`).
    pub home_override: Option<&'a Path>,
    pub spawn_runtime: bool,
}

impl<'a> LifecycleEnv<'a> {
    /// Real `tmux` calls are off under `cfg(test)` and when `FL_TEST_SPINUP_NO_SPAWN=1`.
    pub fn new(home_override: Option<&'a Path>) -> Self {
        let disabled =
            cfg!(test) || std::env::var("FL_TEST_SPINUP_NO_SPAWN").is_ok_and(|v| v.trim() == "1");
        Self {
            home_override,
            spawn_runtime: !disabled,
        }
    }
}

/// `POST /api/v1/sessions/spin-up` body; the field names are the FerroDeck contract.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct SpinUpRequest {
    #[serde(default)]
    pub persona: String,
    #[serde(default)]
    pub project: String,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub vendor: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub node_id: Option<String>,
    #[serde(default)]
    pub hosted: Option<bool>,
    #[serde(default)]
    pub cwd: Option<String>,
}

/// `SpinUpRequest` after validation and defaulting.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ValidSpinUp {
    persona: String,
    project: String,
    role: String,
    vendor: String,
    model: Option<String>,
    node_id: String,
    hosted: bool,
    cwd: Option<String>,
}

fn trimmed_opt(raw: &Option<String>) -> Option<String> {
    raw.as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

fn invalid_field(name: &str, why: &str) -> LifecycleError {
    LifecycleError::Invalid(format!("{name} {why}"))
}

/// Single-line free text no longer than `MAX_FIELD_LEN`.
fn check_text(name: &str, value: &str) -> Result<(), LifecycleError> {
    if value.chars().count() > MAX_FIELD_LEN || value.chars().any(char::is_control) {
        return Err(invalid_field(
            name,
            "must be single-line text of at most 128 characters",
        ));
    }
    Ok(())
}

impl SpinUpRequest {
    fn validate(&self) -> Result<ValidSpinUp, LifecycleError> {
        let persona = self.persona.trim().to_string();
        let slug_ok = persona
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if persona.is_empty()
            || persona.len() > MAX_PERSONA_LEN
            || !persona.as_bytes()[0].is_ascii_lowercase()
            || !slug_ok
        {
            return Err(invalid_field(
                "persona",
                "must be a lowercase slug (a-z, 0-9, '-'; starts with a letter; at most 36 characters)",
            ));
        }
        if fl_wire::dispatcher::is_owner_sender(&persona) {
            return Err(invalid_field(
                "persona",
                "must not be the owner's name (`owner`, `lmcfarlin`)",
            ));
        }
        let project = self.project.trim().to_string();
        if project.is_empty() {
            return Err(invalid_field("project", "must not be empty"));
        }
        validate_slug(&project)
            .map_err(|_| invalid_field("project", "must be a valid area slug"))?;
        let role = self.role.trim().to_ascii_lowercase();
        if !SPIN_UP_ROLES.contains(&role.as_str()) {
            return Err(LifecycleError::Invalid(format!(
                "role must be one of: {}",
                SPIN_UP_ROLES.join(", ")
            )));
        }
        let vendor = self.vendor.trim().to_ascii_lowercase();
        if !SPIN_UP_VENDORS.contains(&vendor.as_str()) {
            return Err(LifecycleError::Invalid(format!(
                "vendor must be one of: {}",
                SPIN_UP_VENDORS.join(", ")
            )));
        }
        let model = trimmed_opt(&self.model);
        if let Some(model) = &model {
            check_text("model", model)?;
        }
        let node_id = trimmed_opt(&self.node_id).unwrap_or_else(|| DEFAULT_NODE_ID.to_string());
        if !node_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
            || node_id.len() > 64
        {
            return Err(invalid_field(
                "node_id",
                "must be at most 64 characters of letters, digits, '-', '_' or '.'",
            ));
        }
        let cwd = trimmed_opt(&self.cwd);
        if let Some(cwd) = &cwd {
            check_text("cwd", cwd)?;
        }
        Ok(ValidSpinUp {
            persona,
            project,
            role,
            vendor,
            model,
            node_id,
            hosted: self.hosted.unwrap_or(true),
            cwd,
        })
    }
}

/// `POST /api/v1/sessions/spin-up` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpinUpResponse {
    pub session_id: String,
    pub persona: String,
    pub project: String,
    pub role: String,
    pub vendor: String,
    pub model: Option<String>,
    pub node_id: String,
    pub hosted: bool,
    pub status: String,
    pub tmux_target: Option<String>,
    pub hydration_dispatched: bool,
    pub prompt: Option<String>,
}

/// Durable record of a hosted session, one `hosted-session-*` workspace runtime entry each.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostedSessionRecord {
    pub session_id: String,
    pub persona: String,
    pub project: String,
    pub role: String,
    pub vendor: String,
    pub model: Option<String>,
    pub node_id: String,
    pub hosted: bool,
    /// `running`, `decommissioning`, or `decommissioned`.
    pub status: String,
    pub restart_policy: String,
    pub restart_count: u32,
    pub tmux_target: Option<String>,
    pub cwd: Option<String>,
    pub hydration_dispatched: bool,
    /// Whether this lifecycle created the tmux session; only then does teardown kill it.
    #[serde(default)]
    pub runtime_spawned: bool,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub decommission_prepared_at_ms: Option<u64>,
}

/// Slug of the runtime entry for `session_id` (`:` is not a slug character).
pub fn hosted_session_slug(session_id: &str) -> String {
    format!("{HOSTED_SLUG_PREFIX}{}", session_id.replace(':', "-"))
}

pub fn persist_hosted_session<S: StateStore>(
    store: &S,
    record: &HostedSessionRecord,
    by: &str,
) -> Result<(), LifecycleError> {
    let value = serde_json::to_value(record).map_err(|e| LifecycleError::Store(e.to_string()))?;
    store.upsert_state(UpsertStateArgs {
        scope: StateScope::Workspace,
        slug: hosted_session_slug(&record.session_id),
        sensitivity: SensitivityPatch::Inherit,
        payload: StatePayload::Runtime(RuntimeEntry {
            value,
            merge_mode: MergeMode::LastWriter,
        }),
        at: stamp::now(),
        by: by.to_string(),
    })?;
    Ok(())
}

/// Every hosted-session record, sorted by `session_id`. Entries that do not decode are skipped.
pub fn list_hosted_sessions<S: StateStore>(
    store: &S,
) -> Result<Vec<HostedSessionRecord>, LifecycleError> {
    let entries = store.list_state(Some(&StateScope::Workspace), Some(StateConcern::Runtime))?;
    let mut records: Vec<HostedSessionRecord> = entries
        .into_iter()
        .filter(|e| e.slug.starts_with(HOSTED_SLUG_PREFIX))
        .filter_map(|e| match e.payload {
            StatePayload::Runtime(rt) => serde_json::from_value(rt.value).ok(),
            _ => None,
        })
        .collect();
    records.sort_by(|a, b| a.session_id.cmp(&b.session_id));
    Ok(records)
}

/// Vendor and persona of a `vendor:persona` session id, when the vendor is a known one.
fn split_vendor_id(id: &str) -> Option<(String, &str)> {
    let (vendor, persona) = id.trim().split_once(':')?;
    let vendor = vendor.to_ascii_lowercase();
    SPIN_UP_VENDORS
        .contains(&vendor.as_str())
        .then_some((vendor, persona))
}

/// Whether `key` names `record`'s persona. A `vendor:persona` key only matches that vendor's
/// record, so `codex:ada` never reaches `claude:ada`; any other key is a bare persona.
fn persona_key_matches(record: &HostedSessionRecord, key: &str) -> bool {
    match split_vendor_id(key) {
        Some((vendor, persona)) => {
            record.vendor == vendor && record.persona.eq_ignore_ascii_case(persona)
        }
        None => record.persona.eq_ignore_ascii_case(key.trim()),
    }
}

/// The record whose `session_id` or `persona` is one of `keys`. An exact `session_id` match wins;
/// the persona fallback applies only when no record matches exactly.
pub fn find_hosted_session<S: StateStore>(
    store: &S,
    keys: &[&str],
) -> Result<Option<HostedSessionRecord>, LifecycleError> {
    let records = list_hosted_sessions(store)?;
    let exact = records
        .iter()
        .find(|r| keys.iter().any(|k| r.session_id == k.trim()));
    let by_persona = || {
        records
            .iter()
            .find(|r| keys.iter().any(|k| persona_key_matches(r, k)))
    };
    Ok(exact.or_else(by_persona).cloned())
}

/// Record `model` on the hosted-session record that `keys` name (same lookup as
/// [`find_hosted_session`]), so the record and a later restart agree with an operator's model
/// switch. Returns the updated record, or `None` when no record matches.
pub fn set_hosted_session_model<S: StateStore>(
    store: &S,
    keys: &[&str],
    model: &str,
    by: &str,
) -> Result<Option<HostedSessionRecord>, LifecycleError> {
    let Some(mut record) = find_hosted_session(store, keys)? else {
        return Ok(None);
    };
    record.model = Some(model.to_string());
    record.updated_at_ms = now_ms();
    persist_hosted_session(store, &record, by)?;
    Ok(Some(record))
}

fn now_ms() -> u64 {
    stamp::now().unix_millis()
}

fn capitalize(persona: &str) -> String {
    let mut chars = persona.chars();
    match chars.next() {
        Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

/// `AgentRole` for a spin-up role string; `ui-developer` is a custom role.
fn agent_role_from_label(label: &str) -> AgentRole {
    match label.trim().to_ascii_lowercase().as_str() {
        "lead" => AgentRole::Lead,
        "assistant" => AgentRole::Assistant,
        "lab-manager" | "lab_manager" => AgentRole::LabManager,
        other => match other.strip_prefix("custom:").unwrap_or(other) {
            slug if validate_slug(slug).is_ok() => AgentRole::Custom(slug.to_string()),
            _ => AgentRole::Assistant,
        },
    }
}

/// Inverse of [`agent_role_from_label`], in the spin-up spelling.
fn role_label_of(role: &AgentRole) -> String {
    match role {
        AgentRole::LabManager => "lab-manager".to_string(),
        other => reforge_role_label(other),
    }
}

fn prompt_for(display: &str, project: &str, role: &str) -> String {
    canonical_reforge_prompt(
        display,
        project,
        &reforge_role_label(&agent_role_from_label(role)),
    )
}

/// Register or revive `persona` as an Active agent, creating its coverage area.
fn register_spun_up_agent<S: AgentRegistry>(
    store: &S,
    v: &ValidSpinUp,
    by: &str,
) -> Result<AgentRecord, LifecycleError> {
    let role = agent_role_from_label(&v.role);
    let project_key = validate_slug(&v.project)
        .map_err(|_| invalid_field("project", "must be a valid area slug"))?;
    if role == AgentRole::Lead
        && let Some(lead) = store
            .list_agents(Some(&project_key), false)?
            .into_iter()
            .find(|a| a.role == AgentRole::Lead && a.name_key != v.persona)
    {
        // `replace` would silently retire the sitting Lead; make the caller decide.
        return Err(LifecycleError::Conflict(format!(
            "project `{project_key}` already has an active Lead: {}",
            lead.display_name
        )));
    }
    // An existing agent keeps its email and extra areas. Only a retired agent, or an active one
    // with the same project and role (a repeat spin-up), may be registered over: otherwise a
    // spin-up under a resident's persona would re-home or de-role that resident.
    let (email, extra_areas, replace) = match store.agent_by_name(&v.persona)? {
        None => (None, Vec::new(), false),
        Some(agent) => {
            if agent.status == AgentStatus::Active
                && (agent.project != project_key || agent.role != role)
            {
                return Err(LifecycleError::Conflict(format!(
                    "agent `{}` is already active in project `{}` as {}",
                    v.persona,
                    agent.project,
                    role_label_of(&agent.role)
                )));
            }
            (Some(agent.email), agent.extra_areas, true)
        }
    };
    Ok(store.register_agent(RegisterAgentArgs {
        display_name: capitalize(&v.persona),
        email,
        project: project_key,
        role,
        extra_areas,
        create_area: true,
        replace,
        at: stamp::now(),
        by: by.to_string(),
    })?)
}

fn tmux_ok(args: &[&str]) -> bool {
    Command::new("tmux")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Best-effort detached `tmux` session. `false` when tmux is absent or the session exists.
fn spawn_tmux_runtime(session: &str, window: &str, cwd: Option<&str>) -> bool {
    let mut args = vec!["new-session", "-d", "-s", session, "-n", window];
    if let Some(dir) = cwd.filter(|d| Path::new(d).is_dir()) {
        args.extend(["-c", dir]);
    }
    tmux_ok(&args)
}

/// Kill a `tmux` session this lifecycle created; `true` when it is gone afterwards.
fn kill_tmux_runtime(session: &str) -> bool {
    let target = format!("={session}");
    tmux_ok(&["kill-session", "-t", &target]) || !tmux_ok(&["has-session", "-t", &target])
}

/// The live route for `persona`, else any route that matches `fallback_session_id`.
pub(crate) fn find_route(
    home_override: Option<&Path>,
    persona: Option<&str>,
    fallback_session_id: &str,
) -> Option<VendorRoute> {
    let route_override =
        home_override.map(|h| h.join(".cache").join("agent-bus").join("routes.json"));
    let mut routes = RouteTable::load(route_override.as_deref());
    routes.discover_routes(home_override, stamp::now());
    let lookup_key = persona.unwrap_or(fallback_session_id);
    if let Some(entry) = routes.get(lookup_key) {
        return Some(entry.route.clone());
    }
    routes
        .entries
        .values()
        .find(|e| route_matches_session_id(&e.route, fallback_session_id))
        .map(|e| e.route.clone())
}

/// Best-effort delivery of `prompt` to a live route for the session; `true` when delivered.
fn deliver_prompt(
    env: &LifecycleEnv<'_>,
    persona: &str,
    session_id: &str,
    by: &str,
    prompt: &str,
) -> bool {
    find_route(env.home_override, Some(persona), session_id)
        .is_some_and(|route| dispatch_operator_prompt_to_route(&route, by, prompt).is_ok())
}

/// Route-table files a teardown prunes: the override, or the default and legacy paths.
fn route_table_paths(home_override: Option<&Path>) -> Vec<PathBuf> {
    match home_override {
        Some(home) => vec![home.join(".cache").join("agent-bus").join("routes.json")],
        None => std::iter::once(default_routes_path())
            .chain(legacy_routes_path())
            .collect(),
    }
}

/// Drop every route keyed by one of `personas` or matching one of `session_ids`; returns the count.
fn remove_routes(home_override: Option<&Path>, personas: &[&str], session_ids: &[&str]) -> usize {
    let mut removed = 0;
    for path in route_table_paths(home_override) {
        if !path.is_file() {
            continue;
        }
        let mut table = RouteTable::load(Some(&path));
        let before = table.entries.len();
        table.entries.retain(|key, entry| {
            let by_key = personas.iter().any(|p| key.eq_ignore_ascii_case(p.trim()));
            let by_id = session_ids
                .iter()
                .any(|id| route_matches_session_id(&entry.route, id));
            !(by_key || by_id)
        });
        let dropped = before - table.entries.len();
        if dropped > 0 && table.save(Some(&path)).is_ok() {
            removed += dropped;
        }
    }
    removed
}

/// Spin up a session: register the agent, persist its record, open its tmux runtime, hydrate it.
///
/// The record is written before tmux starts, so a runtime this call opens always has a record
/// that teardown can find; the record is then updated with what actually started.
///
/// `caller` gates hosting an unmanaged agent: an Active agent with no LIVE hosted record for this
/// session (none, or a decommissioned one left from an earlier teardown) becomes hosted by the
/// spin-up, after which a peer's teardown would be allowed, so only the owner may do it. A peer
/// may still spin up a new or retired persona, or re-spin a session whose hosted record is live.
pub fn spin_up<S: AgentRegistry + StateStore>(
    store: &S,
    env: &LifecycleEnv<'_>,
    req: &SpinUpRequest,
    by: &str,
    caller: Caller,
) -> Result<SpinUpResponse, LifecycleError> {
    let v = req.validate()?;
    let session_id = format!("{}:{}", v.vendor, v.persona);
    // The unfiltered record carries `created_at_ms`, `restart_count` and `runtime_spawned` over;
    // the gate below looks only at whether it is LIVE.
    let previous =
        find_hosted_session(store, &[&session_id])?.filter(|r| r.session_id == session_id);
    // Order: this gate runs first, before the Lead and project/role Conflict checks in
    // `register_spun_up_agent`, because a refusal here reveals nothing about the agent's
    // project or role. It sits before every side effect.
    require_owner_for_unmanaged(
        store,
        caller,
        &v.persona,
        previous.as_ref(),
        OwnerGate::SpinUp,
    )?;
    register_spun_up_agent(store, &v, by)?;

    let display = capitalize(&v.persona);
    let tmux_target = format!("{display}:{}", v.vendor);
    let prompt = prompt_for(&display, &v.project, &v.role);

    let now = now_ms();
    let mut record = HostedSessionRecord {
        session_id: session_id.clone(),
        persona: v.persona.clone(),
        project: v.project.clone(),
        role: v.role.clone(),
        vendor: v.vendor.clone(),
        model: v.model.clone(),
        node_id: v.node_id.clone(),
        hosted: v.hosted,
        status: STATUS_RUNNING.to_string(),
        restart_policy: "on-failure".to_string(),
        restart_count: previous.as_ref().map_or(0, |r| r.restart_count),
        tmux_target: Some(tmux_target.clone()),
        cwd: v.cwd.clone(),
        hydration_dispatched: false,
        runtime_spawned: previous.as_ref().is_some_and(|r| r.runtime_spawned),
        created_at_ms: previous.as_ref().map_or(now, |r| r.created_at_ms),
        updated_at_ms: now,
        decommission_prepared_at_ms: None,
    };
    persist_hosted_session(store, &record, by)?;

    let spawned =
        v.hosted && env.spawn_runtime && spawn_tmux_runtime(&display, &v.vendor, v.cwd.as_deref());
    let hydration_dispatched = deliver_prompt(env, &v.persona, &session_id, by, &prompt);
    record.runtime_spawned |= spawned;
    record.hydration_dispatched = hydration_dispatched;
    record.updated_at_ms = now_ms();
    if let Err(e) = persist_hosted_session(store, &record, by) {
        if spawned {
            // The record cannot learn about this runtime, so teardown could never stop it.
            kill_tmux_runtime(&display);
        }
        return Err(e);
    }

    Ok(SpinUpResponse {
        session_id,
        persona: v.persona,
        project: v.project,
        role: v.role,
        vendor: v.vendor,
        model: v.model,
        node_id: v.node_id,
        hosted: v.hosted,
        status: STATUS_RUNNING.to_string(),
        tmux_target: Some(tmux_target),
        hydration_dispatched,
        prompt: Some(prompt),
    })
}

/// Decommission phase named by the request body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DecommissionMode {
    Prepare,
    Teardown,
}

impl DecommissionMode {
    pub fn parse(raw: &str) -> Result<Self, LifecycleError> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "prepare" => Ok(Self::Prepare),
            "teardown" => Ok(Self::Teardown),
            _ => Err(LifecycleError::Invalid(
                "mode must be \"prepare\" or \"teardown\"".to_string(),
            )),
        }
    }
}

/// `POST /api/v1/sessions/:id/decommission` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecommissionResponse {
    pub session_id: String,
    pub persona: Option<String>,
    pub mode: DecommissionMode,
    pub status: String,
    pub handshake_dispatched: bool,
    pub unregistered: bool,
    pub runtime_stopped: bool,
    pub prompt: Option<String>,
}

/// A session a decommission acts on: its hosted record when one exists, else the caller's guess.
#[derive(Debug, Clone)]
pub struct SessionTarget {
    pub raw_id: String,
    pub session_id: String,
    pub persona: String,
    /// The caller's canonical id for the session, which can differ from `raw_id` and
    /// `session_id`; queues and routes may be keyed by it.
    pub discovered_id: String,
    pub hosted: Option<HostedSessionRecord>,
}

impl SessionTarget {
    /// `discovered` is the caller's `(canonical id, persona)` for `raw_id`, when it has one.
    pub fn resolve<S: StateStore>(
        store: &S,
        raw_id: &str,
        discovered: Option<(String, Option<String>)>,
    ) -> Result<Self, LifecycleError> {
        let raw = raw_id.trim();
        let (disc_id, disc_persona) = discovered.unwrap_or_else(|| (raw.to_string(), None));
        // `vendor:persona` ids name their persona even when no record exists.
        let derived = raw.rsplit(':').next().unwrap_or(raw).to_ascii_lowercase();
        let persona_guess = disc_persona
            .map(|p| p.rsplit(':').next().unwrap_or(&p).to_ascii_lowercase())
            .unwrap_or(derived);
        // A `vendor:persona` id is exact: it never falls back to another vendor's record of the
        // same persona, so the bare persona is not offered as a lookup key for it.
        let mut keys = vec![raw, disc_id.as_str()];
        if split_vendor_id(raw).is_none() {
            keys.push(persona_guess.as_str());
        }
        let hosted = find_hosted_session(store, &keys)?;
        Ok(match hosted {
            Some(rec) => Self {
                raw_id: raw.to_string(),
                session_id: rec.session_id.clone(),
                persona: rec.persona.clone(),
                discovered_id: disc_id,
                hosted: Some(rec),
            },
            None => {
                // Reject a guess no agent can have, before anything looks it up or acts on it.
                if validate_slug(&persona_guess).is_err() {
                    return Err(LifecycleError::Invalid(format!(
                        "session id `{raw}` does not name a valid persona"
                    )));
                }
                Self {
                    raw_id: raw.to_string(),
                    session_id: disc_id.clone(),
                    persona: persona_guess,
                    discovered_id: disc_id,
                    hosted: None,
                }
            }
        })
    }

    /// Distinct ids that can key this session in a queue or route table.
    fn keys(&self) -> Vec<&str> {
        let mut keys: Vec<&str> = Vec::with_capacity(4);
        for key in [
            self.session_id.as_str(),
            self.raw_id.as_str(),
            self.discovered_id.as_str(),
            self.persona.as_str(),
        ] {
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
        keys
    }
}

/// Vendor named by the leading segment of a session id (`claude:ada`, `muse-<uuid>`), if known.
fn vendor_of_session_id(session_id: &str) -> Option<&'static str> {
    let head = session_id.split([':', '-']).next().unwrap_or_default();
    SPIN_UP_VENDORS.iter().copied().find(|v| *v == head)
}

/// Project and role for a session: its hosted record, else its agent registration.
fn project_and_role<S: AgentRegistry>(
    store: &S,
    target: &SessionTarget,
) -> Result<Option<(String, String)>, LifecycleError> {
    if let Some(rec) = &target.hosted {
        return Ok(Some((rec.project.clone(), rec.role.clone())));
    }
    Ok(store
        .agent_by_name(&target.persona)?
        .map(|a| (a.project, role_label_of(&a.role))))
}

/// The hosted record for `target`, adopting an unmanaged session when none exists.
fn record_for_target<S: AgentRegistry>(
    store: &S,
    target: &SessionTarget,
) -> Result<HostedSessionRecord, LifecycleError> {
    if let Some(rec) = &target.hosted {
        return Ok(rec.clone());
    }
    let (project, role) = project_and_role(store, target)?
        .unwrap_or_else(|| ("unknown".to_string(), "assistant".to_string()));
    let now = now_ms();
    Ok(HostedSessionRecord {
        session_id: target.session_id.clone(),
        persona: target.persona.clone(),
        project,
        role,
        vendor: vendor_of_session_id(&target.session_id)
            .unwrap_or("unknown")
            .to_string(),
        model: None,
        node_id: DEFAULT_NODE_ID.to_string(),
        hosted: false,
        status: STATUS_RUNNING.to_string(),
        // A session this lifecycle never started is not restarted by it.
        restart_policy: "never".to_string(),
        restart_count: 0,
        tmux_target: None,
        cwd: None,
        hydration_dispatched: false,
        runtime_spawned: false,
        created_at_ms: now,
        updated_at_ms: now,
        decommission_prepared_at_ms: None,
    })
}

/// Phase one: ask the agent to save its work and hand off. Nothing is stopped or unregistered.
///
/// `handshake_dispatched` is `false` when no live route took the prompt; the caller may queue it.
/// `caller` gates adopting a session that has no hosted record (owner only).
pub fn prepare_decommission<S: AgentRegistry + StateStore>(
    store: &S,
    env: &LifecycleEnv<'_>,
    target: &SessionTarget,
    by: &str,
    caller: Caller,
) -> Result<DecommissionResponse, LifecycleError> {
    // Prepare adopts an unmanaged session into a hosted record, which teardown then accepts from
    // any caller, so the adoption gate applies here too, before any side effect. Order: this
    // gate runs before the decommissioned 409 below. A Peer is refused with 403 when the agent
    // is Active and unmanaged (no record, or only a stale decommissioned one), which reveals
    // nothing; the 409 is reached only by the Owner, or by a Peer when no Active agent exists.
    require_owner_for_unmanaged(
        store,
        caller,
        &target.persona,
        target.hosted.as_ref(),
        OwnerGate::Decommission,
    )?;
    if target.hosted.as_ref().is_some_and(is_decommissioned) {
        return Err(decommissioned_conflict(&target.session_id));
    }
    let Some((project, role)) = project_and_role(store, target)? else {
        return Err(LifecycleError::NotFound(format!(
            "no hosted session or agent record for `{}`",
            target.raw_id
        )));
    };
    let display = capitalize(&target.persona);
    let prompt = synthesize_decommission_prompt(
        &display,
        &project,
        &reforge_role_label(&agent_role_from_label(&role)),
    );
    let handshake_dispatched =
        deliver_prompt(env, &target.persona, &target.session_id, by, &prompt);

    let now = now_ms();
    let mut record = record_for_target(store, target)?;
    record.status = STATUS_DECOMMISSIONING.to_string();
    record.decommission_prepared_at_ms = Some(now);
    record.updated_at_ms = now;
    persist_hosted_session(store, &record, by)?;

    Ok(DecommissionResponse {
        session_id: record.session_id,
        persona: Some(target.persona.clone()),
        mode: DecommissionMode::Prepare,
        status: STATUS_DECOMMISSIONING.to_string(),
        handshake_dispatched,
        unregistered: false,
        runtime_stopped: false,
        prompt: Some(prompt),
    })
}

/// Who asks for a spin-up or teardown. A session with no live hosted record is adopted by a
/// teardown, which retires whatever agent its id names, and an Active agent with no live hosted
/// record is adopted by a spin-up, so only the owner may adopt one
/// (see `require_owner_for_unmanaged`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Caller {
    Owner,
    Peer,
}

fn is_decommissioned(record: &HostedSessionRecord) -> bool {
    record.status == STATUS_DECOMMISSIONED
}

/// A hosted record that still manages its session. A decommissioned record is history: it is kept
/// after teardown, and the persona may since have been registered again outside the lifecycle.
fn is_live_hosted(record: &HostedSessionRecord) -> bool {
    !is_decommissioned(record)
}

/// The action a Peer asks for, which decides what a missing or stale hosted record means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OwnerGate {
    /// Hosting a persona: needs the Owner only when it adopts an Active agent.
    SpinUp,
    /// Prepare or teardown: needs the Owner for any session with no record, and for an Active
    /// agent behind a decommissioned one.
    Decommission,
}

/// The one decision on whether a non-Owner caller needs Owner authority for a target. The Owner
/// always passes. For anyone else a LIVE hosted record always passes. Without one (none, or only
/// a decommissioned record), the action adopts whatever the persona now names: a spin-up is
/// refused when an Active agent exists, a prepare or teardown when no record exists or an Active
/// agent exists. A new or Retired persona still passes a spin-up, and a repeated teardown of a
/// decommissioned record whose agent is Retired or absent stays harmless and allowed.
///
/// Every gate calls this before its first side effect.
fn require_owner_for_unmanaged<S: AgentRegistry>(
    store: &S,
    caller: Caller,
    persona: &str,
    hosted: Option<&HostedSessionRecord>,
    gate: OwnerGate,
) -> Result<(), LifecycleError> {
    if caller == Caller::Owner || hosted.is_some_and(is_live_hosted) {
        return Ok(());
    }
    let active = store
        .agent_by_name(persona)?
        .is_some_and(|a| a.status == AgentStatus::Active);
    match (gate, hosted) {
        (OwnerGate::SpinUp, _) if active => Err(LifecycleError::Forbidden(format!(
            "only Owner authority may spin up `{persona}`, an active agent with no live hosted \
             session record"
        ))),
        (OwnerGate::Decommission, None) => Err(LifecycleError::Forbidden(format!(
            "only Owner authority may decommission `{persona}`, which has no hosted session record"
        ))),
        (OwnerGate::Decommission, Some(_)) if active => Err(LifecycleError::Forbidden(format!(
            "only Owner authority may decommission `{persona}`, an active agent whose hosted \
             session record is decommissioned"
        ))),
        _ => Ok(()),
    }
}

fn decommissioned_conflict(session_id: &str) -> LifecycleError {
    LifecycleError::Conflict(format!(
        "hosted session `{session_id}` is decommissioned; spin it up again"
    ))
}

/// `tmux` session name of a `Session:window` target.
fn tmux_session_name(tmux_target: &str) -> &str {
    tmux_target.split(':').next().unwrap_or(tmux_target)
}

/// Phase two: drop queued prompts and routes, stop the runtime this lifecycle started, retire
/// the agent, and mark the record `decommissioned`. Safe to repeat.
///
/// `caller` gates adopting a session that has no hosted record (owner only). `clear_queue` is
/// called with each id the session may be queued under. Everything that can fail on a lookup
/// runs before the first side effect.
pub fn teardown_session<S: AgentRegistry + StateStore>(
    store: &S,
    env: &LifecycleEnv<'_>,
    target: &SessionTarget,
    by: &str,
    caller: Caller,
    mut clear_queue: impl FnMut(&str),
) -> Result<DecommissionResponse, LifecycleError> {
    require_owner_for_unmanaged(
        store,
        caller,
        &target.persona,
        target.hosted.as_ref(),
        OwnerGate::Decommission,
    )?;
    let mut record = record_for_target(store, target)?;
    let agent = store.agent_by_name(&target.persona)?;
    // The tmux session name is the capitalised persona, so `claude:ada` and `codex:ada` share
    // session `Ada`. Killing it would end the other, still-live session.
    let sharing: Vec<HostedSessionRecord> = match record.tmux_target.as_deref() {
        Some(tmux_target) => list_hosted_sessions(store)?
            .into_iter()
            .filter(|r| {
                r.session_id != record.session_id
                    && !is_decommissioned(r)
                    && r.tmux_target.as_deref().map(tmux_session_name)
                        == Some(tmux_session_name(tmux_target))
            })
            .collect(),
        None => Vec::new(),
    };
    let shared_runtime = record.runtime_spawned && !sharing.is_empty();
    if shared_runtime {
        // The runtime stays up for the other sessions, so ownership of it moves to them: the
        // last one torn down then stops it.
        for mut other in sharing {
            other.runtime_spawned = true;
            other.updated_at_ms = now_ms();
            persist_hosted_session(store, &other, by)?;
        }
        record.runtime_spawned = false;
    }

    for key in target.keys() {
        clear_queue(key);
    }
    remove_routes(
        env.home_override,
        &[target.persona.as_str()],
        &[target.session_id.as_str(), target.raw_id.as_str()],
    );

    let runtime_stopped = match (&record.tmux_target, record.runtime_spawned) {
        (Some(tmux_target), true) if env.spawn_runtime => {
            kill_tmux_runtime(tmux_session_name(tmux_target))
        }
        // Still running for the sessions that share it: report that truthfully.
        _ if shared_runtime => false,
        // Spawning is off, so no runtime was started in this run to stop.
        _ => true,
    };

    if let Some(agent) = agent
        && agent.status == AgentStatus::Active
    {
        store.retire_agent(&agent.id, stamp::now(), by)?;
    }

    record.status = STATUS_DECOMMISSIONED.to_string();
    record.updated_at_ms = now_ms();
    persist_hosted_session(store, &record, by)?;

    Ok(DecommissionResponse {
        session_id: record.session_id,
        persona: Some(target.persona.clone()),
        mode: DecommissionMode::Teardown,
        status: STATUS_DECOMMISSIONED.to_string(),
        handshake_dispatched: false,
        unregistered: true,
        runtime_stopped,
        prompt: None,
    })
}

/// `POST /api/v1/hosted-sessions/:id/restart` response: the updated record plus the reforge prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestartResponse {
    #[serde(flatten)]
    pub record: HostedSessionRecord,
    pub prompt: String,
}

/// Count a restart: reopen a missing tmux session, redeliver the reforge prompt, mark running.
/// A `decommissioned` session is refused (409); spin it up again instead.
pub fn restart_hosted_session<S: StateStore>(
    store: &S,
    env: &LifecycleEnv<'_>,
    id: &str,
    by: &str,
) -> Result<RestartResponse, LifecycleError> {
    let mut record = find_hosted_session(store, &[id])?
        .ok_or_else(|| LifecycleError::NotFound(format!("no hosted session `{id}`")))?;
    if is_decommissioned(&record) {
        // Its agent is retired; restarting would mark it running with no agent behind it.
        return Err(decommissioned_conflict(&record.session_id));
    }
    let display = capitalize(&record.persona);
    if record.hosted
        && env.spawn_runtime
        && spawn_tmux_runtime(&display, &record.vendor, record.cwd.as_deref())
    {
        record.runtime_spawned = true;
    }
    let prompt = prompt_for(&display, &record.project, &record.role);
    record.hydration_dispatched =
        deliver_prompt(env, &record.persona, &record.session_id, by, &prompt);
    record.restart_count += 1;
    record.status = STATUS_RUNNING.to_string();
    record.decommission_prepared_at_ms = None;
    record.updated_at_ms = now_ms();
    persist_hosted_session(store, &record, by)?;
    Ok(RestartResponse { record, prompt })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::MemStore;
    use fl_core::at::At;
    use tempfile::TempDir;

    fn req(persona: &str, role: &str, vendor: &str) -> SpinUpRequest {
        SpinUpRequest {
            persona: persona.into(),
            project: "FerroLoop".into(),
            role: role.into(),
            vendor: vendor.into(),
            ..SpinUpRequest::default()
        }
    }

    fn env(home: &TempDir) -> LifecycleEnv<'_> {
        LifecycleEnv {
            home_override: Some(home.path()),
            spawn_runtime: false,
        }
    }

    fn webhook_route_file(home: &Path, persona: &str) -> PathBuf {
        let mut routes = RouteTable::default();
        routes.upsert(
            persona,
            VendorRoute::Webhook {
                socket_path: home.join("none.sock"),
            },
            At::from_unix_millis(1),
        );
        let path = home.join(".cache").join("agent-bus").join("routes.json");
        routes.save(Some(&path)).unwrap();
        path
    }

    #[test]
    fn validation_rejects_each_bad_field() {
        let bad_persona = |p: &str| req(p, "assistant", "claude").validate().unwrap_err();
        for p in [
            "",
            "  ",
            "Felicity",
            "fel icity",
            "fel_icity",
            "-fel",
            "1fel",
            "fel:x",
        ] {
            assert!(
                matches!(bad_persona(p), LifecycleError::Invalid(m) if m.starts_with("persona"))
            );
        }
        assert!(bad_persona(&"a".repeat(MAX_PERSONA_LEN + 1)).status() == 400);

        let mut r = req("felicity", "assistant", "claude");
        r.project = "  ".into();
        assert!(
            matches!(r.validate().unwrap_err(), LifecycleError::Invalid(m) if m.starts_with("project"))
        );
        r.project = "no spaces".into();
        assert_eq!(r.validate().unwrap_err().status(), 400);

        for role in ["", "owner", "lab_manager", "custom:x"] {
            let e = req("felicity", role, "claude").validate().unwrap_err();
            assert!(
                matches!(e, LifecycleError::Invalid(m) if m.starts_with("role")),
                "{role}"
            );
        }
        for vendor in ["", "gemini", "Claude2"] {
            let e = req("felicity", "lead", vendor).validate().unwrap_err();
            assert!(
                matches!(e, LifecycleError::Invalid(m) if m.starts_with("vendor")),
                "{vendor}"
            );
        }
        let mut r = req("felicity", "lead", "claude");
        r.node_id = Some("bad node".into());
        assert_eq!(r.validate().unwrap_err().status(), 400);
        r.node_id = None;
        r.model = Some("a\nb".into());
        assert_eq!(r.validate().unwrap_err().status(), 400);
    }

    #[test]
    fn validation_defaults_and_accepts_every_listed_role_and_vendor() {
        for role in SPIN_UP_ROLES {
            for vendor in SPIN_UP_VENDORS {
                req("felicity", role, vendor).validate().unwrap();
            }
        }
        let v = req(" felicity ", "UI-Developer", "Claude")
            .validate()
            .unwrap();
        assert_eq!(
            (v.persona.as_str(), v.role.as_str(), v.vendor.as_str()),
            ("felicity", "ui-developer", "claude")
        );
        assert_eq!(v.node_id, DEFAULT_NODE_ID);
        assert!(v.hosted);
        assert_eq!(v.model, None);
        assert_eq!(v.cwd, None);
    }

    #[test]
    fn role_labels_round_trip_through_agent_roles() {
        for label in SPIN_UP_ROLES {
            assert_eq!(role_label_of(&agent_role_from_label(label)), label);
        }
        assert_eq!(
            agent_role_from_label("ui-developer"),
            AgentRole::Custom("ui-developer".into())
        );
        assert_eq!(agent_role_from_label("lab-manager"), AgentRole::LabManager);
    }

    #[test]
    fn hosted_session_slug_is_a_valid_state_slug_for_every_vendor() {
        for vendor in SPIN_UP_VENDORS {
            let id = format!("{vendor}:{}", "a".repeat(MAX_PERSONA_LEN));
            let slug = hosted_session_slug(&id);
            assert_eq!(validate_slug(&slug).unwrap(), slug);
        }
        assert_eq!(
            hosted_session_slug("claude:ada"),
            "hosted-session-claude-ada"
        );
    }

    #[test]
    fn hosted_record_round_trips_through_the_store_and_lists_sorted() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let env = env(&home);
        for (persona, vendor) in [("zed", "codex"), ("ada", "muse"), ("bob", "claude")] {
            spin_up(
                &store,
                &env,
                &req(persona, "assistant", vendor),
                "owner",
                Caller::Owner,
            )
            .unwrap();
        }
        let listed = list_hosted_sessions(&store).unwrap();
        let ids: Vec<&str> = listed.iter().map(|r| r.session_id.as_str()).collect();
        assert_eq!(ids, ["claude:bob", "codex:zed", "muse:ada"]);

        let rec = &listed[0];
        assert_eq!(rec.status, "running");
        assert_eq!(rec.restart_policy, "on-failure");
        assert_eq!(rec.restart_count, 0);
        assert_eq!(rec.tmux_target.as_deref(), Some("Bob:claude"));
        assert!(rec.hosted && !rec.runtime_spawned && !rec.hydration_dispatched);
        assert_eq!(rec.decommission_prepared_at_ms, None);

        let json = serde_json::to_value(rec).unwrap();
        assert_eq!(
            serde_json::from_value::<HostedSessionRecord>(json).unwrap(),
            *rec
        );
        assert_eq!(
            find_hosted_session(&store, &["bob"])
                .unwrap()
                .unwrap()
                .session_id,
            "claude:bob"
        );
        assert!(find_hosted_session(&store, &["nobody"]).unwrap().is_none());
    }

    #[test]
    fn spin_up_registers_agent_and_area_and_is_idempotent() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let env = env(&home);
        let out = spin_up(
            &store,
            &env,
            &req("felicity", "ui-developer", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        assert_eq!(out.session_id, "claude:felicity");
        assert_eq!(out.tmux_target.as_deref(), Some("Felicity:claude"));
        assert_eq!(out.status, "running");
        let prompt = out.prompt.as_deref().unwrap();
        assert!(prompt.starts_with(
            "Take the name of Felicity and take up residence in FerroLoop/ as the ui-developer."
        ));

        let agent = store.agent_by_name("felicity").unwrap().unwrap();
        assert_eq!(agent.status, AgentStatus::Active);
        assert_eq!(agent.role, AgentRole::Custom("ui-developer".into()));
        assert!(store.get_area("ferroloop").unwrap().is_some());

        let again = spin_up(
            &store,
            &env,
            &req("felicity", "ui-developer", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        assert_eq!(again.session_id, out.session_id);
        assert_eq!(list_hosted_sessions(&store).unwrap().len(), 1);
    }

    #[test]
    fn spin_up_refuses_to_displace_the_sitting_lead() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let env = env(&home);
        spin_up(
            &store,
            &env,
            &req("ferris", "lead", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        let err = spin_up(
            &store,
            &env,
            &req("felix", "lead", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap_err();
        assert_eq!(err.status(), 409);
        assert!(store.agent_by_name("felix").unwrap().is_none());
        // The sitting Lead may be spun up again.
        spin_up(
            &store,
            &env,
            &req("ferris", "lead", "codex"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
    }

    #[test]
    fn decommission_prompt_names_each_required_step() {
        let p = synthesize_decommission_prompt("Felicity", "FerroLoop", "assistant");
        for needle in [
            "Felicity",
            "Notes/FerroLoop/STATE.md",
            "fl state handoff",
            "fl state checkpoint",
            "worktrees",
            "fl wire send",
            "agent-msg send",
            "fl wire read --ack",
            "agent-msg read --ack",
            "Lead",
        ] {
            assert!(p.contains(needle), "missing {needle}: {p}");
        }
        assert!(synthesize_decommission_prompt("Ferris", "FerroLoop", "lead").contains("jarvis"));
        assert!(!p.contains("jarvis"));
    }

    #[test]
    fn prepare_marks_decommissioning_and_keeps_the_agent_and_routes() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let env = env(&home);
        spin_up(
            &store,
            &env,
            &req("felicity", "assistant", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        let routes = webhook_route_file(home.path(), "felicity");

        let target = SessionTarget::resolve(&store, "claude:felicity", None).unwrap();
        let out = prepare_decommission(&store, &env, &target, "owner", Caller::Owner).unwrap();
        assert_eq!(out.mode, DecommissionMode::Prepare);
        assert_eq!(out.status, "decommissioning");
        assert!(!out.unregistered && !out.runtime_stopped);
        assert!(out.prompt.unwrap().contains("decommission"));
        // The webhook socket does not exist, so nothing took the prompt.
        assert!(!out.handshake_dispatched);

        let rec = find_hosted_session(&store, &["claude:felicity"])
            .unwrap()
            .unwrap();
        assert_eq!(rec.status, "decommissioning");
        assert!(rec.decommission_prepared_at_ms.is_some());
        assert_eq!(
            store.agent_by_name("felicity").unwrap().unwrap().status,
            AgentStatus::Active
        );
        assert!(RouteTable::load(Some(&routes)).get("felicity").is_some());
    }

    #[test]
    fn prepare_of_an_unknown_session_is_not_found() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let target = SessionTarget::resolve(&store, "ghost", None).unwrap();
        let err =
            prepare_decommission(&store, &env(&home), &target, "owner", Caller::Owner).unwrap_err();
        assert_eq!(err.status(), 404);
    }

    #[test]
    fn teardown_retires_agent_removes_routes_clears_queues_and_is_repeatable() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let env = env(&home);
        spin_up(
            &store,
            &env,
            &req("felicity", "assistant", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        webhook_route_file(home.path(), "felicity");

        let target = SessionTarget::resolve(&store, "claude:felicity", None).unwrap();
        let mut cleared = Vec::new();
        let out = teardown_session(&store, &env, &target, "owner", Caller::Owner, |k| {
            cleared.push(k.to_string())
        })
        .unwrap();
        assert_eq!(out.status, "decommissioned");
        assert!(out.unregistered && out.runtime_stopped && !out.handshake_dispatched);
        assert_eq!(out.prompt, None);
        assert!(cleared.contains(&"claude:felicity".to_string()));
        assert!(cleared.contains(&"felicity".to_string()));

        let routes = home.path().join(".cache/agent-bus/routes.json");
        assert!(RouteTable::load(Some(&routes)).get("felicity").is_none());
        assert_eq!(
            store.agent_by_name("felicity").unwrap().unwrap().status,
            AgentStatus::Retired
        );
        let rec = find_hosted_session(&store, &["felicity"]).unwrap().unwrap();
        assert_eq!(rec.status, "decommissioned");

        teardown_session(&store, &env, &target, "owner", Caller::Owner, |_| {}).unwrap();
    }

    #[test]
    fn teardown_adopts_an_unmanaged_session() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let target = SessionTarget::resolve(
            &store,
            "claude-abc",
            Some(("claude-abc".into(), Some("Ghost".into()))),
        )
        .unwrap();
        assert_eq!(target.persona, "ghost");
        let out =
            teardown_session(&store, &env(&home), &target, "owner", Caller::Owner, |_| {}).unwrap();
        assert_eq!(out.session_id, "claude-abc");
        let rec = find_hosted_session(&store, &["claude-abc"])
            .unwrap()
            .unwrap();
        assert_eq!(
            (rec.vendor.as_str(), rec.status.as_str(), rec.hosted),
            ("claude", "decommissioned", false)
        );
        assert_eq!(rec.restart_policy, "never");
    }

    #[test]
    fn restart_counts_and_revives_a_prepared_record() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let env = env(&home);
        spin_up(
            &store,
            &env,
            &req("felicity", "assistant", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        let target = SessionTarget::resolve(&store, "claude:felicity", None).unwrap();
        prepare_decommission(&store, &env, &target, "owner", Caller::Owner).unwrap();

        let out = restart_hosted_session(&store, &env, "claude:felicity", "owner").unwrap();
        assert_eq!(out.record.restart_count, 1);
        assert_eq!(out.record.status, "running");
        assert_eq!(out.record.decommission_prepared_at_ms, None);
        assert!(out.prompt.contains("take up residence in FerroLoop/"));
        let out = restart_hosted_session(&store, &env, "felicity", "owner").unwrap();
        assert_eq!(out.record.restart_count, 2);
        assert_eq!(
            restart_hosted_session(&store, &env, "nope", "owner")
                .unwrap_err()
                .status(),
            404
        );
    }

    fn decommissioned_felicity(store: &MemStore, env: &LifecycleEnv<'_>) -> SessionTarget {
        spin_up(
            store,
            env,
            &req("felicity", "assistant", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        let target = SessionTarget::resolve(store, "claude:felicity", None).unwrap();
        teardown_session(store, env, &target, "owner", Caller::Owner, |_| {}).unwrap();
        SessionTarget::resolve(store, "claude:felicity", None).unwrap()
    }

    #[test]
    fn restart_and_prepare_of_a_decommissioned_session_conflict_and_change_nothing() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let env = env(&home);
        let target = decommissioned_felicity(&store, &env);
        let before = find_hosted_session(&store, &["claude:felicity"]).unwrap();

        let err = restart_hosted_session(&store, &env, "claude:felicity", "owner").unwrap_err();
        assert!(matches!(err, LifecycleError::Conflict(_)), "{err}");
        assert_eq!(err.status(), 409);
        let err = prepare_decommission(&store, &env, &target, "owner", Caller::Owner).unwrap_err();
        assert_eq!(err.status(), 409);

        assert_eq!(
            find_hosted_session(&store, &["claude:felicity"]).unwrap(),
            before
        );
        assert_eq!(before.unwrap().status, "decommissioned");
        assert_eq!(
            store.agent_by_name("felicity").unwrap().unwrap().status,
            AgentStatus::Retired
        );
        // Teardown stays repeatable, and spin-up revives the session and its agent.
        teardown_session(&store, &env, &target, "owner", Caller::Owner, |_| {}).unwrap();
        spin_up(
            &store,
            &env,
            &req("felicity", "assistant", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        assert_eq!(
            restart_hosted_session(&store, &env, "claude:felicity", "owner")
                .unwrap()
                .record
                .status,
            "running"
        );
    }

    fn register(store: &MemStore, persona: &str, project: &str, role: AgentRole) {
        store
            .register_agent(RegisterAgentArgs {
                display_name: capitalize(persona),
                email: Some(format!("{persona}@example.org")),
                project: project.into(),
                role,
                extra_areas: vec!["notes".into()],
                create_area: true,
                replace: false,
                at: stamp::now(),
                by: "owner".into(),
            })
            .unwrap();
    }

    #[test]
    fn idempotent_respin_keeps_the_agents_email_and_extra_areas() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let env = env(&home);
        register(&store, "felicity", "ferroloop", AgentRole::Assistant);
        spin_up(
            &store,
            &env,
            &req("felicity", "assistant", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        let agent = store.agent_by_name("felicity").unwrap().unwrap();
        assert_eq!(agent.email, "felicity@example.org");
        assert_eq!(agent.extra_areas, ["notes"]);
        assert_eq!(agent.status, AgentStatus::Active);

        // A retired agent comes back in its new project, still with its email and extra areas.
        store
            .retire_agent(&agent.id, stamp::now(), "owner")
            .unwrap();
        let mut other = req("felicity", "ui-developer", "claude");
        other.project = "elsewhere".into();
        spin_up(&store, &env, &other, "owner", Caller::Owner).unwrap();
        let agent = store.agent_by_name("felicity").unwrap().unwrap();
        assert_eq!(
            (agent.status, agent.project.as_str(), agent.email.as_str()),
            (AgentStatus::Active, "elsewhere", "felicity@example.org")
        );
        assert_eq!(agent.extra_areas, ["notes"]);
    }

    #[test]
    fn spin_up_of_an_active_agent_in_another_project_or_role_conflicts() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let env = env(&home);
        register(&store, "jarvis", "ferroloop", AgentRole::LabManager);
        let before = store.agent_by_name("jarvis").unwrap().unwrap();

        let mut other_project = req("jarvis", "lab-manager", "claude");
        other_project.project = "elsewhere".into();
        for r in [
            req("jarvis", "assistant", "claude"),
            req("jarvis", "ui-developer", "codex"),
            other_project,
        ] {
            // The owner reaches the conflict check; a peer is refused earlier (Forbidden), which
            // is covered by the peer_spin_up_* tests.
            let err = spin_up(&store, &env, &r, "owner", Caller::Owner).unwrap_err();
            assert!(matches!(err, LifecycleError::Conflict(_)), "{err}");
        }
        assert_eq!(store.agent_by_name("jarvis").unwrap().unwrap(), before);
        assert!(list_hosted_sessions(&store).unwrap().is_empty());
        assert!(store.get_area("elsewhere").unwrap().is_none());
    }

    fn assert_unmanaged_agent_untouched<S: AgentRegistry + StateStore>(
        store: &S,
        before: &AgentRecord,
    ) {
        assert_eq!(&store.agent_by_name("jarvis").unwrap().unwrap(), before);
        assert!(list_hosted_sessions(store).unwrap().is_empty());
    }

    #[test]
    fn peer_spin_up_of_an_unmanaged_active_agent_is_forbidden_and_changes_nothing() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let env = env(&home);
        // A live webhook route for jarvis: any prompt delivery would connect to this socket.
        webhook_route_file(home.path(), "jarvis");
        let listener =
            std::os::unix::net::UnixListener::bind(home.path().join("none.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        register(&store, "jarvis", "ferroloop", AgentRole::LabManager);
        let before = store.agent_by_name("jarvis").unwrap().unwrap();

        // Same project and role: without the gate this is the idempotent re-spin that hosts it.
        let mut same = req("jarvis", "lab-manager", "claude");
        same.project = "ferroloop".into();
        for r in [same, req("jarvis", "assistant", "codex")] {
            let err = spin_up(&store, &env, &r, "peer", Caller::Peer).unwrap_err();
            assert!(matches!(err, LifecycleError::Forbidden(_)), "{err}");
            assert_eq!(err.status(), 403);
        }
        assert_unmanaged_agent_untouched(&store, &before);
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "no prompt may be delivered"
        );

        // The owner may host it, and keeps its email and extra areas.
        let mut same = req("jarvis", "lab-manager", "claude");
        same.project = "ferroloop".into();
        let out = spin_up(&store, &env, &same, "owner", Caller::Owner).unwrap();
        assert_eq!(out.session_id, "claude:jarvis");
        assert!(
            listener.accept().is_ok(),
            "the owner's spin-up delivers the prompt"
        );
        let after = store.agent_by_name("jarvis").unwrap().unwrap();
        assert_eq!(after.status, AgentStatus::Active);
        assert_eq!(after.email, before.email);
        assert_eq!(after.extra_areas, before.extra_areas);
        assert_eq!(list_hosted_sessions(&store).unwrap().len(), 1);
    }

    #[test]
    fn peer_spin_up_still_works_for_new_retired_and_already_hosted_personas() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let env = env(&home);

        // A persona with no agent record.
        spin_up(
            &store,
            &env,
            &req("ada", "assistant", "claude"),
            "peer",
            Caller::Peer,
        )
        .unwrap();
        // A re-spin of a session that already has a hosted record.
        spin_up(
            &store,
            &env,
            &req("ada", "assistant", "claude"),
            "peer",
            Caller::Peer,
        )
        .unwrap();
        assert_eq!(list_hosted_sessions(&store).unwrap().len(), 1);

        // A retired agent, whether or not it was ever hosted.
        register(&store, "bob", "ferroloop", AgentRole::Assistant);
        let bob = store.agent_by_name("bob").unwrap().unwrap();
        store.retire_agent(&bob.id, stamp::now(), "owner").unwrap();
        spin_up(
            &store,
            &env,
            &req("bob", "assistant", "claude"),
            "peer",
            Caller::Peer,
        )
        .unwrap();
        assert_eq!(
            store.agent_by_name("bob").unwrap().unwrap().status,
            AgentStatus::Active
        );
        assert_eq!(list_hosted_sessions(&store).unwrap().len(), 2);
    }

    #[test]
    fn respin_preserves_created_at_restart_count_and_runtime_ownership() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let env = env(&home);
        spin_up(
            &store,
            &env,
            &req("ada", "assistant", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        let mut rec = find_hosted_session(&store, &["claude:ada"])
            .unwrap()
            .unwrap();
        rec.restart_count = 3;
        rec.runtime_spawned = true;
        rec.created_at_ms = 1;
        persist_hosted_session(&store, &rec, "owner").unwrap();

        spin_up(
            &store,
            &env,
            &req("ada", "assistant", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        let again = find_hosted_session(&store, &["claude:ada"])
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                again.restart_count,
                again.created_at_ms,
                again.runtime_spawned
            ),
            (3, 1, true)
        );
        assert!(again.updated_at_ms > 1);
    }

    #[test]
    fn peer_teardown_of_an_unmanaged_session_is_forbidden_and_changes_nothing() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let env = env(&home);
        register(&store, "jarvis", "ferroloop", AgentRole::LabManager);
        let routes = webhook_route_file(home.path(), "jarvis");
        let target = SessionTarget::resolve(&store, "claude:jarvis", None).unwrap();
        assert!(target.hosted.is_none());

        let mut cleared = Vec::new();
        let err = teardown_session(&store, &env, &target, "peer", Caller::Peer, |k| {
            cleared.push(k.to_string())
        })
        .unwrap_err();
        assert!(matches!(err, LifecycleError::Forbidden(_)), "{err}");
        assert_eq!(err.status(), 403);
        assert!(cleared.is_empty());
        assert!(RouteTable::load(Some(&routes)).get("jarvis").is_some());
        assert_eq!(
            store.agent_by_name("jarvis").unwrap().unwrap().status,
            AgentStatus::Active
        );
        assert!(list_hosted_sessions(&store).unwrap().is_empty());

        // The owner may adopt it.
        teardown_session(&store, &env, &target, "owner", Caller::Owner, |_| {}).unwrap();
        assert_eq!(
            store.agent_by_name("jarvis").unwrap().unwrap().status,
            AgentStatus::Retired
        );
        assert_eq!(list_hosted_sessions(&store).unwrap().len(), 1);
    }

    #[test]
    fn peer_teardown_of_a_hosted_session_still_works() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let env = env(&home);
        spin_up(
            &store,
            &env,
            &req("ada", "assistant", "claude"),
            "peer",
            Caller::Peer,
        )
        .unwrap();
        let target = SessionTarget::resolve(&store, "claude:ada", None).unwrap();
        let out = teardown_session(&store, &env, &target, "peer", Caller::Peer, |_| {}).unwrap();
        assert_eq!(out.status, "decommissioned");
        assert_eq!(
            store.agent_by_name("ada").unwrap().unwrap().status,
            AgentStatus::Retired
        );
    }

    #[test]
    fn peer_prepare_of_an_unmanaged_session_is_forbidden_and_adopts_nothing() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let env = env(&home);
        register(&store, "jarvis", "ferroloop", AgentRole::LabManager);
        let routes = webhook_route_file(home.path(), "jarvis");
        // A live listener on the route's socket would receive any prompt delivered.
        let listener =
            std::os::unix::net::UnixListener::bind(home.path().join("none.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        let target = SessionTarget::resolve(&store, "claude:jarvis", None).unwrap();
        assert!(target.hosted.is_none());

        let err = prepare_decommission(&store, &env, &target, "peer", Caller::Peer).unwrap_err();
        assert!(matches!(err, LifecycleError::Forbidden(_)), "{err}");
        assert_eq!(err.status(), 403);
        assert!(list_hosted_sessions(&store).unwrap().is_empty());
        assert!(listener.accept().is_err(), "no prompt may be delivered");
        assert!(RouteTable::load(Some(&routes)).get("jarvis").is_some());
        assert_eq!(
            store.agent_by_name("jarvis").unwrap().unwrap().status,
            AgentStatus::Active
        );
        // The peer cannot reach teardown through an adopted record either.
        let target = SessionTarget::resolve(&store, "claude:jarvis", None).unwrap();
        let err =
            teardown_session(&store, &env, &target, "peer", Caller::Peer, |_| {}).unwrap_err();
        assert_eq!(err.status(), 403);

        // The owner may still adopt it.
        let out = prepare_decommission(&store, &env, &target, "owner", Caller::Owner).unwrap();
        assert_eq!(out.status, "decommissioning");
        assert_eq!(list_hosted_sessions(&store).unwrap().len(), 1);
    }

    /// `claude:ada` hosted and torn down (record decommissioned, agent Retired), then registered
    /// Active again outside the lifecycle, as a resident does after a restart.
    fn stale_ada(store: &MemStore, env: &LifecycleEnv<'_>) {
        spin_up(
            store,
            env,
            &req("ada", "assistant", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        let target = SessionTarget::resolve(store, "claude:ada", None).unwrap();
        teardown_session(store, env, &target, "owner", Caller::Owner, |_| {}).unwrap();
        assert_eq!(
            store.agent_by_name("ada").unwrap().unwrap().status,
            AgentStatus::Retired
        );
        store
            .register_agent(RegisterAgentArgs {
                display_name: "Ada".into(),
                email: Some("ada@example.org".into()),
                project: "FerroLoop".into(),
                role: AgentRole::Assistant,
                extra_areas: Vec::new(),
                create_area: true,
                replace: true,
                at: stamp::now(),
                by: "ada".into(),
            })
            .unwrap();
        assert_eq!(
            store.agent_by_name("ada").unwrap().unwrap().status,
            AgentStatus::Active
        );
    }

    #[test]
    fn peer_cannot_use_a_stale_decommissioned_record_to_reach_a_reregistered_agent() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let env = env(&home);
        stale_ada(&store, &env);
        let agent_before = store.agent_by_name("ada").unwrap().unwrap();
        let record_before = find_hosted_session(&store, &["claude:ada"])
            .unwrap()
            .unwrap();
        assert_eq!(record_before.status, "decommissioned");
        let target = SessionTarget::resolve(&store, "claude:ada", None).unwrap();
        assert!(target.hosted.is_some());

        let err = spin_up(
            &store,
            &env,
            &req("ada", "assistant", "claude"),
            "peer",
            Caller::Peer,
        )
        .unwrap_err();
        assert!(matches!(err, LifecycleError::Forbidden(_)), "{err}");
        assert_eq!(err.status(), 403);
        let err = prepare_decommission(&store, &env, &target, "peer", Caller::Peer).unwrap_err();
        assert!(matches!(err, LifecycleError::Forbidden(_)), "{err}");
        assert_eq!(err.status(), 403);
        let mut cleared = Vec::new();
        let err = teardown_session(&store, &env, &target, "peer", Caller::Peer, |k| {
            cleared.push(k.to_string())
        })
        .unwrap_err();
        assert!(matches!(err, LifecycleError::Forbidden(_)), "{err}");
        assert_eq!(err.status(), 403);
        assert!(cleared.is_empty());

        // Nothing changed: the record, the agent, and the record count.
        assert_eq!(
            find_hosted_session(&store, &["claude:ada"])
                .unwrap()
                .unwrap(),
            record_before
        );
        assert_eq!(store.agent_by_name("ada").unwrap().unwrap(), agent_before);
        assert_eq!(agent_before.status, AgentStatus::Active);
        assert_eq!(list_hosted_sessions(&store).unwrap().len(), 1);

        // The Owner still revives it, keeping the record's history.
        let out = spin_up(
            &store,
            &env,
            &req("ada", "assistant", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        assert_eq!(out.status, "running");
        let revived = find_hosted_session(&store, &["claude:ada"])
            .unwrap()
            .unwrap();
        assert_eq!(revived.status, "running");
        assert_eq!(revived.created_at_ms, record_before.created_at_ms);
    }

    #[test]
    fn peer_spin_up_and_repeat_teardown_still_work_on_a_decommissioned_record_with_a_retired_agent()
    {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let env = env(&home);
        let target = decommissioned_felicity(&store, &env);
        let before = find_hosted_session(&store, &["claude:felicity"])
            .unwrap()
            .unwrap();

        // Teardown stays repeatable and harmless for a Peer.
        let out = teardown_session(&store, &env, &target, "peer", Caller::Peer, |_| {}).unwrap();
        assert_eq!(out.status, "decommissioned");
        // Prepare keeps its 409 for a Peer when no Active agent exists.
        let err = prepare_decommission(&store, &env, &target, "peer", Caller::Peer).unwrap_err();
        assert_eq!(err.status(), 409);

        // A Peer spin-up of the persona whose agent is Retired revives it and keeps the history.
        let out = spin_up(
            &store,
            &env,
            &req("felicity", "assistant", "claude"),
            "peer",
            Caller::Peer,
        )
        .unwrap();
        assert_eq!(out.status, "running");
        let after = find_hosted_session(&store, &["claude:felicity"])
            .unwrap()
            .unwrap();
        assert_eq!(after.status, "running");
        assert_eq!(after.created_at_ms, before.created_at_ms);
        assert_eq!(
            store.agent_by_name("felicity").unwrap().unwrap().status,
            AgentStatus::Active
        );
    }

    #[test]
    fn peer_prepare_of_a_hosted_session_still_works() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let env = env(&home);
        spin_up(
            &store,
            &env,
            &req("ada", "assistant", "claude"),
            "peer",
            Caller::Peer,
        )
        .unwrap();
        let target = SessionTarget::resolve(&store, "claude:ada", None).unwrap();
        let out = prepare_decommission(&store, &env, &target, "peer", Caller::Peer).unwrap();
        assert_eq!(out.status, "decommissioning");
        assert!(out.prompt.is_some());
    }

    #[test]
    fn teardown_clears_queues_under_the_discovered_id_of_a_hosted_session() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let env = env(&home);
        spin_up(
            &store,
            &env,
            &req("ada", "assistant", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        let target = SessionTarget::resolve(
            &store,
            "claude:ada",
            Some(("claude-xyz".into(), Some("Ada".into()))),
        )
        .unwrap();
        assert!(target.hosted.is_some());
        assert_eq!(target.discovered_id, "claude-xyz");
        let mut cleared = Vec::new();
        teardown_session(&store, &env, &target, "owner", Caller::Owner, |k| {
            cleared.push(k.to_string())
        })
        .unwrap();
        for key in ["claude:ada", "claude-xyz", "ada"] {
            assert!(cleared.contains(&key.to_string()), "{key}: {cleared:?}");
        }
        let mut distinct = cleared.clone();
        distinct.sort();
        distinct.dedup();
        assert_eq!(distinct.len(), cleared.len(), "keys repeat: {cleared:?}");
    }

    #[test]
    fn target_keys_are_distinct_even_when_equal_ids_are_not_adjacent() {
        let target = SessionTarget {
            raw_id: "ada".into(),
            session_id: "claude:ada".into(),
            persona: "ada".into(),
            discovered_id: "claude:ada".into(),
            hosted: None,
        };
        assert_eq!(target.keys(), vec!["claude:ada", "ada"]);
    }

    #[test]
    fn a_malformed_session_id_is_rejected_before_any_side_effect() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let routes = webhook_route_file(home.path(), "ada");
        for raw in ["claude:Foo Bar", "claude:", "a/b", "1abc"] {
            let err = SessionTarget::resolve(&store, raw, None).unwrap_err();
            assert!(matches!(err, LifecycleError::Invalid(_)), "{raw}: {err}");
            assert_eq!(err.status(), 400);
        }
        let err = SessionTarget::resolve(
            &store,
            "claude-abc",
            Some(("claude-abc".into(), Some("Bad Name".into()))),
        )
        .unwrap_err();
        assert_eq!(err.status(), 400);
        assert!(RouteTable::load(Some(&routes)).get("ada").is_some());
        assert!(list_hosted_sessions(&store).unwrap().is_empty());
    }

    #[test]
    fn teardown_lookup_failure_happens_before_side_effects() {
        // A target built by hand with an unusable persona still fails before queues or routes change.
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let env = env(&home);
        let target = SessionTarget {
            raw_id: "claude:Bad Name".into(),
            session_id: "claude:Bad Name".into(),
            persona: "bad name".into(),
            discovered_id: "claude:Bad Name".into(),
            hosted: None,
        };
        let mut cleared = Vec::new();
        let err = teardown_session(&store, &env, &target, "owner", Caller::Owner, |k| {
            cleared.push(k.to_string())
        })
        .unwrap_err();
        assert_eq!(err.status(), 500);
        assert!(cleared.is_empty());
        assert!(list_hosted_sessions(&store).unwrap().is_empty());
    }

    #[test]
    fn a_vendor_qualified_id_never_matches_another_vendors_record() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let env = env(&home);
        spin_up(
            &store,
            &env,
            &req("ada", "assistant", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();

        let id_of = |raw: &str| {
            SessionTarget::resolve(&store, raw, None)
                .unwrap()
                .hosted
                .map(|r| r.session_id)
        };
        assert_eq!(id_of("claude:ada").as_deref(), Some("claude:ada"));
        assert_eq!(id_of("ada").as_deref(), Some("claude:ada"));
        assert_eq!(id_of("codex:ada"), None);
        assert!(
            find_hosted_session(&store, &["codex:ada"])
                .unwrap()
                .is_none()
        );
        assert_eq!(
            restart_hosted_session(&store, &env, "codex:ada", "owner")
                .unwrap_err()
                .status(),
            404
        );
        // A discovered session of a vendor with a record still maps onto it by persona.
        let found = SessionTarget::resolve(
            &store,
            "claude-abc",
            Some(("claude-abc".into(), Some("Ada".into()))),
        )
        .unwrap();
        assert_eq!(found.session_id, "claude:ada");

        // With both vendors present each id resolves to its own record, and the exact id wins
        // over a persona key listed before it.
        spin_up(
            &store,
            &env,
            &req("ada", "assistant", "codex"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        assert_eq!(id_of("codex:ada").as_deref(), Some("codex:ada"));
        assert_eq!(id_of("claude:ada").as_deref(), Some("claude:ada"));
        assert_eq!(
            find_hosted_session(&store, &["ada", "codex:ada"])
                .unwrap()
                .unwrap()
                .session_id,
            "codex:ada"
        );
    }

    #[test]
    fn teardown_keeps_a_tmux_session_that_another_hosted_session_shares() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let env = env(&home);
        spin_up(
            &store,
            &env,
            &req("ada", "assistant", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        spin_up(
            &store,
            &env,
            &req("ada", "assistant", "codex"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        let set_spawned = |id: &str, spawned: bool| {
            let mut rec = find_hosted_session(&store, &[id]).unwrap().unwrap();
            rec.runtime_spawned = spawned;
            persist_hosted_session(&store, &rec, "owner").unwrap();
        };
        let spawned = |id: &str| {
            find_hosted_session(&store, &[id])
                .unwrap()
                .unwrap()
                .runtime_spawned
        };
        // `claude:ada` opened the shared tmux session `Ada`.
        set_spawned("claude:ada", true);

        let target = SessionTarget::resolve(&store, "claude:ada", None).unwrap();
        let out = teardown_session(&store, &env, &target, "owner", Caller::Owner, |_| {}).unwrap();
        assert_eq!(out.status, "decommissioned");
        assert!(!out.runtime_stopped, "the shared session is still running");
        // Ownership moved to the surviving session, which stops it in its turn.
        assert!(!spawned("claude:ada") && spawned("codex:ada"));
        assert_eq!(
            find_hosted_session(&store, &["codex:ada"])
                .unwrap()
                .unwrap()
                .status,
            "running"
        );

        let target = SessionTarget::resolve(&store, "codex:ada", None).unwrap();
        let out = teardown_session(&store, &env, &target, "owner", Caller::Owner, |_| {}).unwrap();
        assert!(out.runtime_stopped);
        assert!(spawned("codex:ada"));
    }

    #[test]
    fn teardown_stops_the_tmux_session_when_the_other_sharer_is_decommissioned() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let env = env(&home);
        spin_up(
            &store,
            &env,
            &req("ada", "assistant", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        spin_up(
            &store,
            &env,
            &req("ada", "assistant", "codex"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        let other = SessionTarget::resolve(&store, "codex:ada", None).unwrap();
        teardown_session(&store, &env, &other, "owner", Caller::Owner, |_| {}).unwrap();
        let mut rec = find_hosted_session(&store, &["claude:ada"])
            .unwrap()
            .unwrap();
        rec.runtime_spawned = true;
        persist_hosted_session(&store, &rec, "owner").unwrap();

        let target = SessionTarget::resolve(&store, "claude:ada", None).unwrap();
        let out = teardown_session(&store, &env, &target, "owner", Caller::Owner, |_| {}).unwrap();
        // Spawning is off in tests, so the stop is reported without a real `tmux` call.
        assert!(out.runtime_stopped);
        assert!(
            find_hosted_session(&store, &["claude:ada"])
                .unwrap()
                .unwrap()
                .runtime_spawned
        );
    }

    #[test]
    fn mode_parsing_accepts_only_prepare_and_teardown() {
        assert_eq!(
            DecommissionMode::parse(" Prepare ").unwrap(),
            DecommissionMode::Prepare
        );
        assert_eq!(
            DecommissionMode::parse("teardown").unwrap(),
            DecommissionMode::Teardown
        );
        for bad in ["", "clear", "stop"] {
            assert_eq!(DecommissionMode::parse(bad).unwrap_err().status(), 400);
        }
    }
}
