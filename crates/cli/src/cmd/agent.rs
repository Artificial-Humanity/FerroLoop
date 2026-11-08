use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use clap::Subcommand;
use fl_core::Iri;
use fl_core::agent::{
    AgentRecord, AgentRole, AgentStatus, AreaRecord, RegisterAgentArgs, reports_to,
};
use fl_core::at::At;
use fl_core::ids::{AgentId, Kind, RecordId};
use fl_core::state::{StateConcern, StatePayload, StateScope, validate_slug};
use fl_core::store::{AgentRegistry, Catalog, Handles, StateStore, StoreError};
use fl_core::telemetry::{
    AttemptHeartbeat, DEFAULT_STALE_AFTER_MS, LivenessVerdict, evaluate_liveness,
    liveness_agent_slug, liveness_record_slug,
};
use fl_hub::lifecycle::{
    self, DecommissionMode, LifecycleEnv, LifecycleError, RemodelMode, RemodelRequest,
    SessionTarget, SpinUpRequest,
};
use serde::Serialize;

use crate::refs::Ref;

/// Current UTC wall-clock timestamp via `fl_exec::stamp::now()`.
pub fn now_at() -> At {
    fl_exec::stamp::now()
}

#[derive(Subcommand)]
pub enum AgentCmd {
    /// Manage workspace coverage areas and projects (`areas` table).
    Area {
        #[command(subcommand)]
        cmd: AreaCmd,
    },
    /// Register or resume an agent identity in the workspace registry.
    Register {
        /// Display name (`Ferris`, `JARVIS`, `Kayla`); lowercased form is `name_key`.
        name: String,
        /// Primary project / area slug.
        #[arg(long)]
        project: String,
        /// Role: a default role (`lead`, `assistant`, `lab_manager`, `ui-developer`,
        /// `web-developer`, `mobile-developer`, `system-developer`, `data-engineer`, `tester`,
        /// `strategist`, `journalist`) or `custom:<slug>`.
        #[arg(long)]
        role: String,
        /// Create `--project` and any `--extra-area` in `areas` if not yet present.
        #[arg(long)]
        create_area: bool,
        /// Additional coverage area slugs.
        #[arg(long = "extra-area")]
        extra_areas: Vec<String>,
        /// Optional explicit lowercase email (defaults to `<name_key>@artificialhumanity.io`).
        #[arg(long)]
        email: Option<String>,
        /// Replace an existing agent session or retire a previous project Lead.
        #[arg(long)]
        replace: bool,
        /// Author recording this registration.
        #[arg(long)]
        by: String,
        #[arg(long)]
        json: bool,
    },
    /// Mark a registered agent as `retired`.
    Retire {
        /// Agent IRI (`urn:uuid:...`), numeric handle (`1`, `2`, …), or `name_key`.
        agent: String,
        /// Author recording this retirement.
        #[arg(long)]
        by: String,
        #[arg(long)]
        json: bool,
    },
    /// Spin up a hosted agent session: register the agent, open its hosted runtime, and hydrate it.
    SpinUp {
        /// Persona slug (`felicity`): lowercase letters, digits and `-`.
        persona: String,
        /// Project / area the agent works in.
        #[arg(long)]
        project: String,
        /// Role: `lead`, `assistant`, `ui-developer`, `web-developer`, `mobile-developer`,
        /// `system-developer`, `data-engineer`, `tester`, `strategist`, `journalist`, or
        /// `lab-manager`.
        #[arg(long)]
        role: String,
        /// Vendor runtime (`claude`, `antigravity`, `codex`, `grok`, or `muse`).
        #[arg(long)]
        vendor: String,
        /// Model on the vendor runtime (default: the vendor's own default).
        #[arg(long)]
        model: Option<String>,
        /// Vendor-native thinking level (`--effort` for Claude, reasoning effort for Codex, the
        /// model id suffix for Antigravity). Other vendors reject it. Antigravity takes only
        /// `low`, `medium` or `high` and needs `--model`.
        #[arg(long = "thinking-level", visible_alias = "effort")]
        thinking_level: Option<String>,
        /// Node the session runs on (default `ai-lab-0`).
        #[arg(long = "node-id")]
        node_id: Option<String>,
        /// Open a hosted runtime (`fl-host`) for the session (`--hosted false` records it without one).
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        hosted: bool,
        /// Working directory for the hosted session.
        #[arg(long)]
        cwd: Option<String>,
        /// Author recording this spin-up.
        #[arg(long)]
        by: String,
        #[arg(long)]
        json: bool,
    },
    /// Decommission a session in two phases: `prepare` asks it to hand off, `teardown` removes it.
    Decommission {
        /// Session id (`claude:felicity`) or persona.
        session: String,
        /// `prepare` (handoff prompt, status `decommissioning`) or `teardown` (retire and clean up).
        #[arg(long)]
        mode: String,
        /// Author recording this decommission.
        #[arg(long)]
        by: String,
        #[arg(long)]
        json: bool,
    },
    /// Move a session to another vendor, keeping its persona, project, role and worktree:
    /// `prepare` asks it to hand off, `execute` (the default) tears it down and spins it up again.
    Remodel {
        /// Session id (`claude:felicity`) or persona.
        session: String,
        /// New vendor runtime (`claude`, `antigravity`, `codex`, `grok`, or `muse`).
        #[arg(long)]
        vendor: String,
        /// Model on the new vendor (default: the vendor's own default, never the old model).
        #[arg(long)]
        model: Option<String>,
        /// Vendor-native thinking level on the new vendor (see `spin-up --thinking-level`).
        #[arg(long = "thinking-level", visible_alias = "effort")]
        thinking_level: Option<String>,
        /// `prepare` (handoff prompt, status `decommissioning`) or `execute`.
        #[arg(long)]
        mode: Option<String>,
        /// Working directory for the new runtime (default: the session's own).
        #[arg(long)]
        cwd: Option<String>,
        /// Node the new session runs on (default: the session's own).
        #[arg(long = "node", visible_alias = "node-id")]
        node_id: Option<String>,
        /// Open a hosted runtime (`fl-host`) for the new session (default: as the session was).
        #[arg(long, action = clap::ArgAction::Set)]
        hosted: Option<bool>,
        /// Author recording this remodel.
        #[arg(long, default_value = "owner")]
        by: String,
        #[arg(long)]
        json: bool,
    },
    /// List active (or all) registered agents and optionally render a Markdown roster (`--render <path>`).
    Roster {
        /// Filter to agents covering this project/area slug.
        #[arg(long)]
        project: Option<String>,
        /// Include retired agents in the output.
        #[arg(long)]
        include_retired: bool,
        /// Atomically write the two-table Markdown roster to `<path>`.
        #[arg(long)]
        render: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Report wrapper-observed liveness (`live`, `unverifiable`, or `exited`) for an agent or record.
    Liveness {
        /// Optional agent persona (`ferris`) or record handle/IRI (`1`, `urn:uuid:...`).
        target: Option<String>,
        /// Maximum heartbeat age in milliseconds before a `live` status becomes `unverifiable (stale_status)`.
        #[arg(long, default_value_t = DEFAULT_STALE_AFTER_MS)]
        stale_after_ms: u64,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub enum AreaCmd {
    /// Create or update a coverage area / project row.
    Set {
        /// Display name (`FerroLoop`, `AI-Lab-AMD`, `Lab-Manager`); lowercased to `area_key`.
        name: String,
        /// Optional workspace-relative directory path (e.g. `FerroLoop/`).
        #[arg(long)]
        path: Option<String>,
        /// One-line description of the area's purpose (<= 240 chars).
        #[arg(long)]
        purpose: String,
        /// Author recording this update.
        #[arg(long)]
        by: String,
        #[arg(long)]
        json: bool,
    },
    /// List all registered coverage areas.
    List {
        #[arg(long)]
        json: bool,
    },
}

/// Write `content` atomically to `path` via a sibling tempfile + rename,
/// refusing symlinked targets or symlinked ancestor directories (spec §5.1, §5.2).
pub fn write_atomic_non_symlink(path: &Path, content: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    for ancestor in parent.ancestors() {
        if ancestor.as_os_str().is_empty() {
            break;
        }
        if let Ok(meta) = std::fs::symlink_metadata(ancestor)
            && meta.file_type().is_symlink()
        {
            return Err(format!(
                "refusing to write under symlinked directory `{}`",
                ancestor.display()
            ));
        }
    }
    if let Ok(meta) = std::fs::symlink_metadata(path)
        && meta.file_type().is_symlink()
    {
        return Err(format!(
            "refusing to overwrite symlinked file `{}`",
            path.display()
        ));
    }
    std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    for ancestor in parent.ancestors() {
        if ancestor.as_os_str().is_empty() {
            break;
        }
        if let Ok(meta) = std::fs::symlink_metadata(ancestor)
            && meta.file_type().is_symlink()
        {
            return Err(format!(
                "refusing to write under symlinked directory `{}`",
                ancestor.display()
            ));
        }
    }
    let mut tmp = tempfile::NamedTempFile::new_in(parent)
        .map_err(|e| format!("failed to create tempfile in `{}`: {e}", parent.display()))?;
    tmp.write_all(content)
        .map_err(|e| format!("failed to write tempfile for `{}`: {e}", path.display()))?;
    tmp.flush()
        .map_err(|e| format!("failed to flush tempfile for `{}`: {e}", path.display()))?;
    tmp.persist(path)
        .map_err(|e| format!("failed to persist `{}`: {}", path.display(), e.error))?;
    Ok(())
}

/// Resolve `<name-or-id>` to an `AgentRecord` (spec §5.1):
/// 1. `urn:uuid:...` -> `get_agent`
/// 2. Bare digits (`1`, `2`, …) -> `resolve_handle(Kind::Agent, n)` -> `get_agent`
/// 3. Slug (`[a-z]...`) -> `agent_by_name`
pub fn resolve_agent<S: AgentRegistry + Handles>(
    store: &S,
    input: &str,
) -> Result<AgentRecord, String> {
    if let Ok(r) = input.parse::<Ref>()
        && !matches!(r, Ref::Slug(_))
    {
        let iri = match r {
            Ref::Iri(iri) => iri,
            Ref::Handle(h) => store
                .resolve_handle(Kind::Agent, h)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("no agent with handle {h} in this store"))?,
            Ref::Issue(_) | Ref::Slug(_) => {
                return Err(format!(
                    "`{input}` is an issue reference, not an agent handle, IRI, or name"
                ));
            }
        };
        return store
            .get_agent(&AgentId(iri))
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no agent matching `{input}`"));
    }
    store
        .agent_by_name(input)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no agent matching `{input}`"))
}

/// Format the two-table Markdown roster (`Projects and Coverage Areas` + `Active Agents`)
/// for `fl agent roster --render <path>` (spec §5.1).
pub fn render_registry_markdown(areas: &[AreaRecord], agents: &[AgentRecord]) -> String {
    let mut out = String::from(
        "# Agent and Project Registry\n\n\
         ## Projects and Coverage Areas\n\n\
         | Key | Name | Path | Purpose |\n\
         | --- | --- | --- | --- |\n",
    );
    let mut sorted_areas = areas.to_vec();
    sorted_areas.sort_by(|a, b| a.area_key.cmp(&b.area_key));
    for a in &sorted_areas {
        let path_cell = a
            .path
            .as_deref()
            .map(|p| format!("`{p}`"))
            .unwrap_or_else(|| "—".to_string());
        out.push_str(&format!(
            "| `{}` | {} | {} | {} |\n",
            a.area_key, a.name, path_cell, a.purpose
        ));
    }

    out.push_str(
        "\n## Active Agents\n\n\
         | Persona | ID | Project | Extra Areas | Role | Reports To | Email |\n\
         | --- | --- | --- | --- | --- | --- | --- |\n",
    );
    let mut active: Vec<&AgentRecord> = agents
        .iter()
        .filter(|a| a.status == AgentStatus::Active)
        .collect();
    active.sort_by(|a, b| a.name_key.cmp(&b.name_key));
    for ag in active {
        let extra = if ag.extra_areas.is_empty() {
            "—".to_string()
        } else {
            ag.extra_areas
                .iter()
                .map(|x| format!("`{x}`"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mgr = reports_to(ag, agents);
        out.push_str(&format!(
            "| **{}** (`{}`) | `{}` | `{}` | {} | `{}` | {} | `{}` |\n",
            ag.display_name,
            ag.name_key,
            ag.id,
            ag.project,
            extra,
            ag.role.as_wire(),
            mgr,
            ag.email,
        ));
    }
    out
}

#[derive(Debug, Clone, Serialize)]
struct LivenessReportItem {
    target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    attempt_id: Option<Iri>,
    #[serde(skip_serializing_if = "Option::is_none")]
    record: Option<RecordId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    adapter: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent: Option<String>,
    #[serde(flatten)]
    verdict: LivenessVerdict,
}

fn format_verdict_label(verdict: &LivenessVerdict) -> String {
    match verdict {
        LivenessVerdict::Live { .. } => "live".to_string(),
        LivenessVerdict::Unverifiable { reason, .. } => {
            format!("unverifiable ({})", reason.as_wire())
        }
        LivenessVerdict::Exited { outcome, .. } => {
            format!("exited ({})", outcome.as_wire())
        }
    }
}

fn format_liveness_line(
    target: &str,
    hb: Option<&AttemptHeartbeat>,
    adapter: Option<&str>,
    verdict: &LivenessVerdict,
) -> String {
    let v = format_verdict_label(verdict);
    match (hb, verdict) {
        (Some(hb), _) => format!(
            "{target}\t{v}\tadapter={}\tduration={}ms\ttokens={}/{}\tcost={} micro-USD",
            hb.adapter, hb.duration_ms, hb.tokens_in, hb.tokens_out, hb.cost_usd_micros
        ),
        (None, LivenessVerdict::Live { duration_ms, .. }) => format!(
            "{target}\t{v}\tadapter={}\tduration={duration_ms}ms\ttokens=0/0\tcost=0 micro-USD",
            adapter.unwrap_or("-")
        ),
        (None, _) => format!("{target}\t{v}"),
    }
}

/// Evaluate live `fl-host` session or active `RouteTable` liveness for `target` when no
/// wrapper `AttemptHeartbeat` exists in the store.
pub fn hosted_session_liveness<F>(
    target: &str,
    sessions: &[fl_host_proto::SessionSummary],
    routes: Option<&fl_wire::identity::RouteTable>,
    now: &At,
    local_host: &str,
    pid_alive: F,
) -> Option<(String, String, LivenessVerdict)>
where
    F: Fn(u32) -> bool,
{
    let target_trimmed = target.trim();
    for summary in sessions {
        let fl_host_proto::ChildState::Running(rec) = &summary.state else {
            continue;
        };
        if !pid_alive(rec.pid) {
            continue;
        }
        let sid = summary.stream.session.as_str();
        let matched = match sid.split_once('.') {
            Some((s, w)) if s.eq_ignore_ascii_case("muse") => (w
                .eq_ignore_ascii_case(target_trimmed)
                || sid.eq_ignore_ascii_case(target_trimmed)
                || format!("{s}:{w}").eq_ignore_ascii_case(target_trimmed))
            .then(|| (w.trim().to_ascii_lowercase(), "muse".to_string())),
            Some((s, w)) => (s.eq_ignore_ascii_case(target_trimmed)
                || sid.eq_ignore_ascii_case(target_trimmed)
                || format!("{s}:{w}").eq_ignore_ascii_case(target_trimmed))
            .then(|| (s.trim().to_ascii_lowercase(), w.trim().to_ascii_lowercase())),
            None => sid
                .eq_ignore_ascii_case(target_trimmed)
                .then(|| (sid.trim().to_ascii_lowercase(), "unknown".to_string())),
        };
        if let Some((persona, adapter)) = matched {
            let duration_ms = now.unix_millis().saturating_sub(rec.started_at_ms);
            return Some((
                adapter,
                persona,
                LivenessVerdict::Live {
                    pid: rec.pid,
                    host: local_host.to_string(),
                    heartbeat_at: now.clone(),
                    duration_ms,
                    tokens_in: 0,
                    tokens_out: 0,
                    cost_usd_micros: 0,
                },
            ));
        }
    }

    if let Some(table) = routes
        && let Some(entry) = table.get(target)
    {
        let matched = match &entry.route {
            fl_wire::identity::VendorRoute::Claude { pid, .. } if *pid > 0 && pid_alive(*pid) => {
                Some(("claude".to_string(), *pid))
            }
            fl_wire::identity::VendorRoute::Agy {
                ls_pid: Some(pid), ..
            } if *pid > 0 && pid_alive(*pid) => Some(("antigravity".to_string(), *pid)),
            _ => None,
        };
        if let Some((adapter, pid)) = matched {
            let duration_ms = now
                .unix_millis()
                .saturating_sub(entry.updated_at.unix_millis());
            return Some((
                adapter,
                entry.persona.clone(),
                LivenessVerdict::Live {
                    pid,
                    host: local_host.to_string(),
                    heartbeat_at: now.clone(),
                    duration_ms,
                    tokens_in: 0,
                    tokens_out: 0,
                    cost_usd_micros: 0,
                },
            ));
        }
    }
    None
}

fn discover_live_host_sessions_for_liveness() -> Vec<fl_host_proto::SessionSummary> {
    if std::env::var("FL_WIRE_DISABLE_HOST_DISCOVERY").as_deref() == Ok("1")
        && std::env::var_os("FL_HOST_SOCK").is_none()
    {
        return Vec::new();
    }
    #[cfg(test)]
    if std::env::var_os("FL_HOST_SOCK").is_none() {
        return Vec::new();
    }
    let Ok(endpoint) = fl_host_proto::HostEndpoint::discover() else {
        return Vec::new();
    };
    let Ok(mut client) = fl_host_proto::BlockingHostClient::connect(&endpoint) else {
        return Vec::new();
    };
    client
        .list_sessions()
        .map(crate::cmd::host::dedup_session_summaries)
        .unwrap_or_default()
}

fn load_routes_for_liveness() -> Option<fl_wire::identity::RouteTable> {
    if let Some(explicit) = std::env::var_os("FL_WIRE_ROUTES").map(PathBuf::from) {
        return Some(fl_wire::identity::RouteTable::load(Some(&explicit)));
    }
    if std::env::var("FL_WIRE_DISABLE_HOST_DISCOVERY").as_deref() == Ok("1") {
        return None;
    }
    #[cfg(test)]
    {
        None
    }
    #[cfg(not(test))]
    {
        let path = fl_wire::identity::default_routes_path();
        path.is_file()
            .then(|| fl_wire::identity::RouteTable::load(Some(&path)))
    }
}

pub fn pid_is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(unix)]
    {
        if Path::new("/proc").is_dir() {
            let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
                return false;
            };
            if let Some(idx) = stat.rfind(") ") {
                let state = stat[idx + 2..].chars().next();
                return state.is_some_and(|c| c != 'Z');
            }
            return true;
        }
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

/// Exit code 2 for a registry conflict (as `register` reports one), 1 for everything else.
fn lifecycle_failure(e: LifecycleError) -> (i32, String) {
    let code = if matches!(e, LifecycleError::Conflict(_)) {
        2
    } else {
        1
    };
    (code, e.to_string())
}

/// The role and extra areas a `register --replace` should store.
///
/// A replace keeps what the existing record already holds, and never raises anyone:
/// - an existing `LabManager` asked to re-register as `Lead` stays `LabManager` (a reforge's
///   `register --role lead --replace` must not drop the lab manager's authority);
/// - when no extra areas were given (`extras_given == false`), the existing record's are kept.
///
/// With no existing record, or without `replace`, the request is used as given. Only an
/// `Active` existing record counts: a retired record holds no role, so it is treated like no
/// record (`unregister X` then `register X --role lead --replace` comes back as `lead`). The name
/// is never consulted: only a role the store already granted is preserved.
///
/// The existing record is read in a separate read transaction from the `register_agent` write,
/// so a role changed in between can be written back. It can only restore a role that name held a
/// moment earlier, never raise anyone.
pub fn preserve_on_replace(
    existing: Option<&AgentRecord>,
    replace: bool,
    requested_role: AgentRole,
    requested_extras: Vec<String>,
    extras_given: bool,
) -> (AgentRole, Vec<String>) {
    let Some(existing) = existing.filter(|e| replace && e.status == AgentStatus::Active) else {
        return (requested_role, requested_extras);
    };
    let role = if existing.role == AgentRole::LabManager && requested_role == AgentRole::Lead {
        AgentRole::LabManager
    } else {
        requested_role
    };
    let extras = if extras_given {
        requested_extras
    } else {
        existing.extra_areas.clone()
    };
    (role, extras)
}

pub fn run<S: AgentRegistry + Handles + StateStore + Catalog>(
    store: &S,
    cmd: AgentCmd,
) -> Result<(), (i32, String)> {
    match cmd {
        AgentCmd::Area { cmd } => match cmd {
            AreaCmd::Set {
                name,
                path,
                purpose,
                by,
                json,
            } => {
                let rec = AreaRecord::new(&name, path, &purpose, now_at(), &by)
                    .map_err(|e| (1, e.to_string()))?;
                let saved = store.upsert_area(rec).map_err(|e| (1, e.to_string()))?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&saved).unwrap());
                } else {
                    println!(
                        "{}\t{}\t{}\t{}",
                        saved.area_key,
                        saved.name,
                        saved.path.as_deref().unwrap_or("-"),
                        saved.purpose
                    );
                }
                Ok(())
            }
            AreaCmd::List { json } => {
                let areas = store.list_areas().map_err(|e| (1, e.to_string()))?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&areas).unwrap());
                } else {
                    for a in areas {
                        println!(
                            "{}\t{}\t{}\t{}",
                            a.area_key,
                            a.name,
                            a.path.as_deref().unwrap_or("-"),
                            a.purpose
                        );
                    }
                }
                Ok(())
            }
        },
        AgentCmd::Register {
            name,
            project,
            role,
            create_area,
            extra_areas,
            email,
            replace,
            by,
            json,
        } => {
            let parsed_role = AgentRole::from_wire(&role).map_err(|e| (1, e.to_string()))?;
            let args = RegisterAgentArgs {
                display_name: name,
                email,
                project,
                role: parsed_role,
                extra_areas,
                create_area,
                replace,
                at: now_at(),
                by,
            };
            match store.register_agent(args) {
                Ok(rec) => {
                    crate::cmd::wire::sync_store_retired_personas(store);
                    let all_active = store.list_agents(None, false).unwrap_or_default();
                    let mgr = reports_to(&rec, &all_active);
                    if json {
                        let mut val = serde_json::to_value(&rec).unwrap();
                        if let Some(obj) = val.as_object_mut() {
                            obj.insert("reports_to".into(), serde_json::Value::String(mgr));
                            obj.insert(
                                "co_author_trailer".into(),
                                serde_json::Value::String(rec.co_author_trailer()),
                            );
                        }
                        println!("{}", serde_json::to_string_pretty(&val).unwrap());
                    } else {
                        let h = store
                            .handle_of(Kind::Agent, rec.id.iri())
                            .ok()
                            .flatten()
                            .unwrap_or(0);
                        println!(
                            "{h}\t{}\t{}\t{}\t{}\treports_to={mgr}\t{}",
                            rec.id,
                            rec.display_name,
                            rec.project,
                            rec.role.as_wire(),
                            rec.co_author_trailer()
                        );
                    }
                    Ok(())
                }
                Err(e @ (StoreError::AgentCollision { .. } | StoreError::LeadTaken { .. })) => {
                    Err((2, e.to_string()))
                }
                Err(other) => Err((1, other.to_string())),
            }
        }
        AgentCmd::Retire { agent, by, json } => {
            let target = resolve_agent(store, &agent).map_err(|e| (1, e))?;
            let retired = store
                .retire_agent(&target.id, now_at(), &by)
                .map_err(|e| (1, e.to_string()))?;
            crate::cmd::wire::sync_store_retired_personas(store);
            crate::cmd::wire::evict_persona_route(&retired.name_key);
            if json {
                println!("{}", serde_json::to_string_pretty(&retired).unwrap());
            } else {
                println!("{}\t{}\tretired", retired.id, retired.display_name);
            }
            Ok(())
        }
        AgentCmd::SpinUp {
            persona,
            project,
            role,
            vendor,
            model,
            thinking_level,
            node_id,
            hosted,
            cwd,
            by,
            json,
        } => {
            let req = SpinUpRequest {
                persona,
                project,
                role,
                vendor,
                model,
                thinking_level,
                node_id,
                hosted: Some(hosted),
                cwd,
                context_window: None,
            }
            .with_default_cwd(
                store,
                &fl_core::resolve_workspace_root(&std::env::current_dir().unwrap_or_default()),
            );
            let out = lifecycle::spin_up(
                store,
                &LifecycleEnv::new(None),
                &req,
                &by,
                lifecycle::Caller::Owner,
            )
            .map_err(lifecycle_failure)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&out).unwrap());
            } else {
                println!(
                    "{}\t{}\t{}\thydration_dispatched={}",
                    out.session_id,
                    out.status,
                    out.tmux_target.as_deref().unwrap_or("-"),
                    out.hydration_dispatched
                );
            }
            Ok(())
        }
        AgentCmd::Decommission {
            session,
            mode,
            by,
            json,
        } => {
            let mode = DecommissionMode::parse(&mode).map_err(lifecycle_failure)?;
            let env = LifecycleEnv::new(None);
            let target =
                SessionTarget::resolve(store, &session, None).map_err(lifecycle_failure)?;
            // `fl` holds no hub prompt queue: an undelivered prepare prompt is printed instead.
            let out = match mode {
                DecommissionMode::Prepare => lifecycle::prepare_decommission(
                    store,
                    &env,
                    &target,
                    &by,
                    lifecycle::Caller::Owner,
                ),
                DecommissionMode::Teardown => lifecycle::teardown_session(
                    store,
                    &env,
                    &target,
                    &by,
                    lifecycle::Caller::Owner,
                    |_| {},
                ),
            }
            .map_err(lifecycle_failure)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&out).unwrap());
            } else {
                println!(
                    "{}\t{}\thandshake_dispatched={}\tunregistered={}\truntime_stopped={}",
                    out.session_id,
                    out.status,
                    out.handshake_dispatched,
                    out.unregistered,
                    out.runtime_stopped
                );
                if let Some(restore) = &out.restore_source {
                    println!("restore_source={}", serde_json::to_string(restore).unwrap());
                }
                if let (false, Some(prompt)) = (out.handshake_dispatched, &out.prompt) {
                    println!(
                        "no live route took the handshake; send this prompt to the session:\n{prompt}"
                    );
                }
            }
            Ok(())
        }
        AgentCmd::Remodel {
            session,
            vendor,
            model,
            thinking_level,
            mode,
            cwd,
            node_id,
            hosted,
            by,
            json,
        } => {
            let req = RemodelRequest {
                vendor,
                model,
                thinking_level,
                mode,
                cwd,
                node_id,
                hosted,
            };
            let target =
                SessionTarget::resolve(store, &session, None).map_err(lifecycle_failure)?;
            // `fl` holds no hub prompt queue: an undelivered prepare prompt is printed instead.
            let out = lifecycle::remodel_session(
                store,
                &LifecycleEnv::new(None),
                &target,
                &req,
                &by,
                lifecycle::Caller::Owner,
                |_| {},
            )
            .map_err(lifecycle_failure)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&out).unwrap());
            } else if out.mode == RemodelMode::Prepare {
                println!(
                    "{}\t{}\thandshake_dispatched={}",
                    out.session_id, out.status, out.handshake_dispatched
                );
                if let (false, Some(prompt)) = (out.handshake_dispatched, &out.prompt) {
                    println!(
                        "no live route took the handshake; send this prompt to the session:\n{prompt}"
                    );
                }
            } else {
                println!(
                    "{}\t{}\t{}\thydration_dispatched={}\tremodeled_from={}",
                    out.session_id,
                    out.status,
                    out.tmux_target.as_deref().unwrap_or("-"),
                    out.hydration_dispatched,
                    out.previous_session_id
                );
                for warning in &out.warnings {
                    eprintln!("warning: {warning}");
                }
            }
            Ok(())
        }
        AgentCmd::Roster {
            project,
            include_retired,
            render,
            json,
        } => {
            if let Some(ref p) = project {
                let _ = validate_slug(p).map_err(|e| (1, e.to_string()))?;
            }
            let agents = store
                .list_agents(project.as_deref(), include_retired)
                .map_err(|e| (1, e.to_string()))?;
            let all_agents = store
                .list_agents(None, true)
                .map_err(|e| (1, e.to_string()))?;
            if let Some(ref out_path) = render {
                let areas = store.list_areas().map_err(|e| (1, e.to_string()))?;
                let md = render_registry_markdown(&areas, &all_agents);
                write_atomic_non_symlink(out_path, md.as_bytes()).map_err(|e| (1, e))?;
            }
            if json {
                let enriched: Vec<serde_json::Value> = agents
                    .iter()
                    .map(|a| {
                        let mut v = serde_json::to_value(a).unwrap();
                        if let Some(obj) = v.as_object_mut() {
                            obj.insert(
                                "reports_to".into(),
                                serde_json::Value::String(reports_to(a, &all_agents)),
                            );
                        }
                        v
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&enriched).unwrap());
            } else {
                for a in &agents {
                    let mgr = reports_to(a, &all_agents);
                    println!(
                        "{}\t{}\t{}\t{}\t{}\treports_to={mgr}\t{}",
                        a.name_key,
                        a.display_name,
                        a.project,
                        a.role.as_wire(),
                        a.status.as_wire(),
                        a.email
                    );
                }
            }
            Ok(())
        }
        AgentCmd::Liveness {
            target,
            stale_after_ms,
            json,
        } => {
            let entries = store
                .list_state(Some(&StateScope::Workspace), Some(StateConcern::Runtime))
                .map_err(|e| (1, e.to_string()))?;
            let mut heartbeats: Vec<(String, AttemptHeartbeat)> = Vec::new();
            for entry in entries {
                if !entry.slug.starts_with("liveness-") {
                    continue;
                }
                if let StatePayload::Runtime(rt) = entry.payload
                    && let Ok(hb) = serde_json::from_value::<AttemptHeartbeat>(rt.value)
                {
                    heartbeats.push((entry.slug, hb));
                }
            }
            let now = now_at();
            let host = fl_exec::adapters::harness::detect_host();

            if let Some(t) = target {
                let resolved_record_iri = if let Ok(h) = t.parse::<u64>() {
                    store.resolve_handle(Kind::Record, h).ok().flatten()
                } else {
                    Iri::parse(&t).ok()
                };
                let target_rec_slug = resolved_record_iri
                    .as_ref()
                    .map(|iri| liveness_record_slug(&RecordId(iri.clone())));
                let target_ag_slug = liveness_agent_slug(&t);

                let matched_hb = heartbeats
                    .into_iter()
                    .filter(|(slug, hb)| {
                        slug == &t
                            || Some(slug.as_str()) == target_rec_slug.as_deref()
                            || slug == &target_ag_slug
                            || hb
                                .agent
                                .as_deref()
                                .is_some_and(|a| a.eq_ignore_ascii_case(&t))
                            || hb.record.iri().as_str() == t
                            || resolved_record_iri
                                .as_ref()
                                .is_some_and(|iri| hb.record.iri() == iri)
                    })
                    .max_by(|(_, a), (_, b)| {
                        (a.heartbeat_at.as_str(), a.attempt_id.as_str())
                            .cmp(&(b.heartbeat_at.as_str(), b.attempt_id.as_str()))
                    })
                    .map(|(_, hb)| hb);

                let (adapter, agent, verdict) = if let Some(ref hb) = matched_hb {
                    let v = evaluate_liveness(Some(hb), &now, stale_after_ms, &host, pid_is_alive);
                    (Some(hb.adapter.clone()), hb.agent.clone(), v)
                } else {
                    let host_sessions = discover_live_host_sessions_for_liveness();
                    let routes = load_routes_for_liveness();
                    if let Some((adapter, agent, v)) = hosted_session_liveness(
                        &t,
                        &host_sessions,
                        routes.as_ref(),
                        &now,
                        &host,
                        pid_is_alive,
                    ) {
                        (Some(adapter), Some(agent), v)
                    } else {
                        let v = evaluate_liveness(None, &now, stale_after_ms, &host, pid_is_alive);
                        (None, None, v)
                    }
                };
                if json {
                    let item = LivenessReportItem {
                        target: t,
                        attempt_id: matched_hb.as_ref().map(|h| h.attempt_id.clone()),
                        record: matched_hb.as_ref().map(|h| h.record.clone()),
                        adapter,
                        agent,
                        verdict,
                    };
                    println!("{}", serde_json::to_string_pretty(&item).unwrap());
                } else {
                    println!(
                        "{}",
                        format_liveness_line(&t, matched_hb.as_ref(), adapter.as_deref(), &verdict,)
                    );
                }
                Ok(())
            } else {
                let mut dedup: BTreeMap<String, (String, AttemptHeartbeat)> = BTreeMap::new();
                for (slug, hb) in heartbeats {
                    let key = hb.attempt_id.as_str().to_string();
                    match dedup.get(&key) {
                        Some((_, prev))
                            if prev.heartbeat_at.as_str() >= hb.heartbeat_at.as_str() => {}
                        _ => {
                            dedup.insert(key, (slug, hb));
                        }
                    }
                }
                let mut items = Vec::new();
                let mut seen_agents = std::collections::BTreeSet::new();
                for (_, (slug, hb)) in dedup {
                    let verdict =
                        evaluate_liveness(Some(&hb), &now, stale_after_ms, &host, pid_is_alive);
                    let label = hb.agent.clone().unwrap_or(slug);
                    seen_agents.insert(label.to_ascii_lowercase());
                    if json {
                        items.push(LivenessReportItem {
                            target: label,
                            attempt_id: Some(hb.attempt_id),
                            record: Some(hb.record),
                            adapter: Some(hb.adapter),
                            agent: hb.agent,
                            verdict,
                        });
                    } else {
                        println!(
                            "{}",
                            format_liveness_line(
                                &label,
                                Some(&hb),
                                Some(hb.adapter.as_str()),
                                &verdict,
                            )
                        );
                    }
                }
                let host_sessions = discover_live_host_sessions_for_liveness();
                for summary in &host_sessions {
                    if !matches!(summary.state, fl_host_proto::ChildState::Running(_)) {
                        continue;
                    }
                    let sess_name = summary.stream.session.as_str();
                    if let Some((adapter, agent, verdict)) = hosted_session_liveness(
                        sess_name,
                        &host_sessions,
                        None,
                        &now,
                        &host,
                        pid_is_alive,
                    ) {
                        if !seen_agents.insert(agent.to_ascii_lowercase()) {
                            continue;
                        }
                        if json {
                            items.push(LivenessReportItem {
                                target: agent.clone(),
                                attempt_id: None,
                                record: None,
                                adapter: Some(adapter),
                                agent: Some(agent),
                                verdict,
                            });
                        } else {
                            println!(
                                "{}",
                                format_liveness_line(
                                    &agent,
                                    None,
                                    Some(adapter.as_str()),
                                    &verdict
                                )
                            );
                        }
                    }
                }
                if json {
                    println!("{}", serde_json::to_string_pretty(&items).unwrap());
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::MemStore;

    #[derive(clap::Parser)]
    struct AgentTestCli {
        #[command(subcommand)]
        cmd: AgentCmd,
    }

    #[test]
    fn spin_up_and_remodel_parse_thinking_level_and_its_effort_alias() {
        use clap::Parser;
        let parse = |args: &[&str]| {
            let mut full = vec!["fl"];
            full.extend_from_slice(args);
            AgentTestCli::try_parse_from(full).unwrap().cmd
        };
        for flag in ["--thinking-level", "--effort"] {
            let AgentCmd::SpinUp { thinking_level, .. } = parse(&[
                "spin-up",
                "ada",
                "--project",
                "p",
                "--role",
                "assistant",
                "--vendor",
                "claude",
                "--by",
                "x",
                "--json",
                flag,
                "high",
            ]) else {
                panic!("not spin-up");
            };
            assert_eq!(thinking_level.as_deref(), Some("high"), "{flag}");

            let AgentCmd::Remodel { thinking_level, .. } = parse(&[
                "remodel", "ada", "--vendor", "codex", "--by", "x", "--json", flag, "xhigh",
            ]) else {
                panic!("not remodel");
            };
            assert_eq!(thinking_level.as_deref(), Some("xhigh"), "{flag}");
        }
        let AgentCmd::SpinUp { thinking_level, .. } = parse(&[
            "spin-up",
            "ada",
            "--project",
            "p",
            "--role",
            "assistant",
            "--vendor",
            "claude",
            "--by",
            "x",
            "--json",
        ]) else {
            panic!("not spin-up");
        };
        assert!(thinking_level.is_none());
    }

    #[test]
    fn test_write_atomic_non_symlink_refuses_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("registry.md");
        let content = b"# Roster";
        assert!(write_atomic_non_symlink(&target, content).is_ok());
        assert_eq!(std::fs::read(&target).unwrap(), content);

        #[cfg(unix)]
        {
            let link = dir.path().join("link.md");
            std::os::unix::fs::symlink(&target, &link).unwrap();
            let err = write_atomic_non_symlink(&link, b"# Overwrite");
            assert!(err.is_err());
            assert!(
                err.unwrap_err()
                    .contains("refusing to overwrite symlinked file")
            );

            let sub = dir.path().join("real_dir");
            std::fs::create_dir_all(&sub).unwrap();
            let sym_dir = dir.path().join("sym_dir");
            std::os::unix::fs::symlink(&sub, &sym_dir).unwrap();
            let under_sym = sym_dir.join("test.md");
            let err = write_atomic_non_symlink(&under_sym, b"# Bad");
            assert!(err.is_err());
            assert!(
                err.unwrap_err()
                    .contains("refusing to write under symlinked directory")
            );
        }
    }

    fn record(role: AgentRole, extras: &[&str]) -> AgentRecord {
        AgentRecord {
            id: AgentId(fl_core::ids::seq_iri(1)),
            name_key: "synthetic".into(),
            display_name: "Synthetic".into(),
            email: "synthetic@example.test".into(),
            project: "lab".into(),
            role,
            extra_areas: extras.iter().map(|s| (*s).to_string()).collect(),
            status: AgentStatus::Active,
            updated_at: At::from_unix_millis(1),
            by: "synthetic".into(),
        }
    }

    #[test]
    fn replace_keeps_a_lab_manager_and_its_extra_areas_but_never_raises_anyone() {
        let lm = record(AgentRole::LabManager, &["agents", "notes"]);
        // `register --role lead --replace` over a lab manager keeps the role and, with no
        // `--extra-areas`, the areas.
        assert_eq!(
            preserve_on_replace(Some(&lm), true, AgentRole::Lead, vec![], false),
            (
                AgentRole::LabManager,
                vec!["agents".to_string(), "notes".to_string()]
            )
        );
        // Explicit extra areas win.
        assert_eq!(
            preserve_on_replace(Some(&lm), true, AgentRole::Lead, vec!["x".into()], true),
            (AgentRole::LabManager, vec!["x".to_string()])
        );
        // Only a Lead request is held at LabManager; other roles are taken as asked.
        assert_eq!(
            preserve_on_replace(Some(&lm), true, AgentRole::Assistant, vec![], true).0,
            AgentRole::Assistant
        );
        // No elevation: a lead, assistant or no record never becomes LabManager, whatever the name.
        for existing in [
            Some(record(AgentRole::Lead, &[])),
            Some(record(AgentRole::Assistant, &[])),
            None,
        ] {
            assert_eq!(
                preserve_on_replace(existing.as_ref(), true, AgentRole::Lead, vec![], false).0,
                AgentRole::Lead
            );
        }
        // A retired record holds no role: a Lead request comes back as Lead with the requested
        // extras.
        let retired = AgentRecord {
            status: AgentStatus::Retired,
            ..record(AgentRole::LabManager, &["agents", "notes"])
        };
        assert_eq!(
            preserve_on_replace(
                Some(&retired),
                true,
                AgentRole::Lead,
                vec!["x".into()],
                true
            ),
            (AgentRole::Lead, vec!["x".to_string()])
        );
        assert_eq!(
            preserve_on_replace(Some(&retired), true, AgentRole::Lead, vec![], false),
            (AgentRole::Lead, vec![])
        );
        // Without --replace nothing is preserved.
        assert_eq!(
            preserve_on_replace(Some(&lm), false, AgentRole::Lead, vec![], false),
            (AgentRole::Lead, vec![])
        );
    }

    #[test]
    fn test_render_registry_markdown() {
        let store = MemStore::default();
        let area = store
            .upsert_area(
                AreaRecord::new(
                    "FerroLoop",
                    Some("FerroLoop/".into()),
                    "agent orchestration",
                    now_at(),
                    "ferris",
                )
                .unwrap(),
            )
            .unwrap();
        let ag1 = store
            .register_agent(RegisterAgentArgs {
                display_name: "Ferris".into(),
                email: None,
                project: "ferroloop".into(),
                role: AgentRole::Lead,
                extra_areas: vec![],
                create_area: false,
                replace: false,
                at: now_at(),
                by: "ferris".into(),
            })
            .unwrap();
        let ag2 = store
            .register_agent(RegisterAgentArgs {
                display_name: "Alex".into(),
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

        let md = render_registry_markdown(&[area], &[ag1, ag2]);
        assert!(md.contains("## Projects and Coverage Areas"));
        assert!(md.contains("`ferroloop`"));
        assert!(md.contains("## Active Agents"));
        assert!(md.contains("**Ferris** (`ferris`)"));
        assert!(md.contains("**Alex** (`alex`)"));
        assert!(md.contains("Ferris"));
    }

    #[test]
    fn test_agent_run_exit_codes() {
        let _lock = crate::cmd::wire::tests::hook_env_lock();
        let store = MemStore::default();
        let res = run(
            &store,
            AgentCmd::Register {
                name: "Ferris".into(),
                project: "ferroloop".into(),
                role: "lead".into(),
                create_area: true,
                extra_areas: vec![],
                email: None,
                replace: false,
                by: "ferris".into(),
                json: false,
            },
        );
        assert!(res.is_ok());

        let dup = run(
            &store,
            AgentCmd::Register {
                name: "Ferris".into(),
                project: "ferroloop".into(),
                role: "lead".into(),
                create_area: false,
                extra_areas: vec![],
                email: None,
                replace: false,
                by: "ferris".into(),
                json: false,
            },
        );
        assert!(matches!(dup, Err((2, _))));

        let second_lead = run(
            &store,
            AgentCmd::Register {
                name: "Jarvis".into(),
                project: "ferroloop".into(),
                role: "lead".into(),
                create_area: false,
                extra_areas: vec![],
                email: None,
                replace: false,
                by: "jarvis".into(),
                json: false,
            },
        );
        assert!(matches!(second_lead, Err((2, _))));
    }

    #[test]
    fn hosted_session_liveness_resolves_running_fl_host_session_and_active_route() {
        use fl_host_proto::{
            ChildRecord, ChildState, HostInstance, InputState, SessionEpoch, SessionId,
            SessionMode, SessionSummary, SpawnKind, StreamId,
        };
        use fl_wire::identity::{RouteTable, VendorRoute};

        let sid = SessionId::new("Feira.antigravity").unwrap();
        let sessions = vec![SessionSummary {
            stream: StreamId {
                host: HostInstance::new([1u8; 16]),
                session: sid.clone(),
                epoch: SessionEpoch(2),
            },
            mode: SessionMode::Pty,
            state: ChildState::Running(ChildRecord {
                pid: 2270049,
                session: sid,
                session_epoch: SessionEpoch(2),
                kind: SpawnKind::Pty,
                input: InputState::Writable,
                started_at_ms: 1_000,
                exit: None,
                consumer_stalled: false,
            }),
            acked: 0,
            latest: 0,
            input: InputState::Writable,
        }];
        let now = At::from_unix_millis(6_000);
        let (adapter, agent, verdict) =
            hosted_session_liveness("feira", &sessions, None, &now, "ai-lab-0", |pid| {
                pid == 2270049
            })
            .expect("live fl-host session must report live");
        assert_eq!(adapter, "antigravity");
        assert_eq!(agent, "feira");
        assert_eq!(
            verdict,
            LivenessVerdict::Live {
                pid: 2270049,
                host: "ai-lab-0".into(),
                heartbeat_at: now.clone(),
                duration_ms: 5_000,
                tokens_in: 0,
                tokens_out: 0,
                cost_usd_micros: 0,
            }
        );
        let line = format_liveness_line("feira", None, Some(&adapter), &verdict);
        assert!(line.starts_with("feira\tlive\tadapter=antigravity\tduration=5000ms"));

        // RouteTable fallback when fl-host session list is empty.
        let mut routes = RouteTable::default();
        routes.upsert_explicit(
            "felix",
            VendorRoute::Agy {
                conversation_id: "conv-felix".into(),
                ls_pid: Some(2667475),
                ls_address: None,
                csrf_token: None,
                agentapi_exe: None,
            },
            At::from_unix_millis(2_000),
        );
        let (adapter2, agent2, verdict2) =
            hosted_session_liveness("felix", &[], Some(&routes), &now, "ai-lab-0", |pid| {
                pid == 2667475
            })
            .expect("live route pid must report live");
        assert_eq!(adapter2, "antigravity");
        assert_eq!(agent2, "felix");
        assert!(matches!(
            verdict2,
            LivenessVerdict::Live {
                pid: 2667475,
                duration_ms: 4_000,
                ..
            }
        ));

        // An Agy route with `ls_pid: None` must return `None` even when `ls_address` is listening.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        routes.upsert_explicit(
            "pidless",
            VendorRoute::Agy {
                conversation_id: "conv-pidless".into(),
                ls_pid: None,
                ls_address: Some(format!("127.0.0.1:{port}")),
                csrf_token: Some("csrf-tok".into()),
                agentapi_exe: None,
            },
            At::from_unix_millis(2_000),
        );
        assert!(
            hosted_session_liveness("pidless", &[], Some(&routes), &now, "ai-lab-0", |_| true)
                .is_none(),
            "Agy route without live ls_pid must not report Live (pid 0)"
        );
    }
}
