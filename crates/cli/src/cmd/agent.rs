use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};

use clap::Subcommand;
use fl_core::Iri;
use fl_core::agent::{
    AgentRecord, AgentRole, AgentStatus, AreaRecord, RegisterAgentArgs, reports_to,
};
use fl_core::at::At;
use fl_core::ids::{AgentId, Kind, RecordId};
use fl_core::state::validate_slug;
use fl_core::store::{AgentRegistry, Catalog, Handles, StateStore, StoreError};
use fl_core::telemetry::{
    AttemptHeartbeat, DEFAULT_STALE_AFTER_MS, LivenessVerdict, evaluate_liveness,
    liveness_agent_slug, liveness_record_slug,
};
use fl_hub::lifecycle::{
    self, DecommissionMode, LifecycleEnv, LifecycleError, RemodelMode, RemodelRequest,
    SessionTarget, SpinUpRequest,
};
use fl_watch::{HostedBlock, WatchHostedSession};
use serde::Serialize;

use crate::refs::Ref;

/// Current UTC wall-clock timestamp via `fl_exec::stamp::now()`.
pub fn now_at() -> At {
    fl_exec::stamp::now()
}

/// Compose and install the concrete [`fl_core::VendorHostAdapterRegistry`] from `fl-vendor-adapters`.
pub fn composed_vendor_host_adapters() -> &'static fl_core::VendorHostAdapterRegistry {
    fl_hub::lifecycle::set_global_vendor_version_adapters(
        fl_vendor_adapters::default_vendor_version_registry(),
    );
    fl_hub::lifecycle::set_global_vendor_host_adapters(
        fl_vendor_adapters::default_vendor_host_adapter_registry(),
    )
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
        /// Permission posture (`ask`, `accept-edits`, `auto`, `plan`, or `bypass`; default `auto`).
        #[arg(long, default_value_t = fl_core::permissions::Posture::Auto)]
        posture: fl_core::permissions::Posture,
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
    /// Restart a hosted session using its saved settings.
    Restart {
        /// Persona or exact hosted session id (`codex:ada`).
        session: String,
        /// Author recording this manual restart.
        #[arg(long, default_value = "owner")]
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
        /// Permission posture (`ask`, `accept-edits`, `auto`, `plan`, or `bypass`; default `auto`).
        #[arg(long, default_value_t = fl_core::permissions::Posture::Auto)]
        posture: fl_core::permissions::Posture,
        /// `prepare` (handoff prompt, status `decommissioning`) or `execute`.
        #[arg(long)]
        mode: Option<String>,
        /// Prepare or execute even if the destination daemon has no usable sign-in.
        #[arg(long)]
        allow_blocked_sign_in: bool,
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
        /// Optional agent persona, hosted session ID, or record handle/IRI.
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
    hosted_block: HostedLivenessBlock,
    #[serde(flatten)]
    verdict: LivenessVerdict,
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
struct HostedLivenessBlock {
    #[serde(skip_serializing_if = "Option::is_none")]
    block: Option<HostedBlock>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop_reason: Option<&'static str>,
}

impl HostedLivenessBlock {
    fn from_session(seat: &WatchHostedSession) -> Self {
        Self {
            block: seat.block,
            stop_reason: seat
                .is_stopped_while_blocked()
                .then_some(lifecycle::STOP_REASON_STARTUP_BLOCKED),
        }
    }

    fn append_to(self, mut line: String) -> String {
        if let Some(block) = self.block {
            line.push_str("\tblock=");
            line.push_str(block.reason.as_str());
        }
        if let Some(reason) = self.stop_reason {
            line.push_str("\tstop_reason=");
            line.push_str(reason);
        }
        line
    }
}

fn blocked_seat_for<'a>(
    seats: &[&'a WatchHostedSession],
    target: &str,
    vendor: Option<&str>,
) -> Option<&'a WatchHostedSession> {
    let target = target.trim();
    seats
        .iter()
        .copied()
        .filter(|seat| {
            seat.session_id.eq_ignore_ascii_case(target)
                || (seat.persona.eq_ignore_ascii_case(target)
                    && vendor.is_none_or(|vendor| seat.vendor.eq_ignore_ascii_case(vendor)))
        })
        .max_by_key(|seat| (seat.is_running(), seat.updated_at_ms))
}

fn take_running_block<'a>(
    seats: &[&'a WatchHostedSession],
    persona: &str,
    vendor: &str,
    reported_blocks: &mut BTreeSet<&'a str>,
) -> HostedLivenessBlock {
    blocked_seat_for(seats, persona, Some(vendor))
        .filter(|seat| seat.is_running() && reported_blocks.insert(seat.session_id.as_str()))
        .map(HostedLivenessBlock::from_session)
        .unwrap_or_default()
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
/// wrapper `AttemptHeartbeat` exists in the store. Canonical stored session IDs
/// may only resolve to their own live Host seat.
pub fn hosted_session_liveness<F>(
    target: &str,
    sessions: &[fl_host_proto::SessionSummary],
    hosted_seats: &[WatchHostedSession],
    routes: Option<&fl_wire::identity::RouteTable>,
    now: &At,
    local_host: &str,
    pid_alive: F,
) -> Option<(String, String, LivenessVerdict)>
where
    F: Fn(u32) -> bool,
{
    let target = target.trim();
    let colon_parts = target.split_once(':');
    let canonical_only = colon_parts.is_some()
        && hosted_seats
            .iter()
            .any(|seat| seat.session_id.eq_ignore_ascii_case(target));
    let mut selected = None;
    for summary in sessions {
        let fl_host_proto::ChildState::Running(rec) = &summary.state else {
            continue;
        };
        let sid = summary.stream.session.as_str();
        let host_parts = sid.split_once('.');
        let (persona, adapter) = match host_parts {
            Some((prefix, persona)) if prefix.eq_ignore_ascii_case("muse") => (persona, "muse"),
            Some((persona, adapter)) => (persona, adapter),
            None => (sid, "unknown"),
        };
        let canonical = host_parts.is_some()
            && colon_parts.is_some_and(|(vendor, name)| {
                vendor.eq_ignore_ascii_case(adapter) && name.eq_ignore_ascii_case(persona)
            });
        let alias = !canonical_only
            && selected.is_none()
            && (sid.eq_ignore_ascii_case(target)
                || host_parts.is_some_and(|(prefix, suffix)| {
                    persona.eq_ignore_ascii_case(target)
                        || colon_parts.is_some_and(|(first, last)| {
                            prefix.eq_ignore_ascii_case(first) && suffix.eq_ignore_ascii_case(last)
                        })
                }));
        if (canonical || alias) && pid_alive(rec.pid) {
            selected = Some((rec, persona, adapter));
            if canonical || colon_parts.is_none() {
                break;
            }
        }
    }
    if let Some((rec, persona, adapter)) = selected {
        let duration_ms = now.unix_millis().saturating_sub(rec.started_at_ms);
        return Some((
            adapter.trim().to_ascii_lowercase(),
            persona.trim().to_ascii_lowercase(),
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
    if canonical_only {
        return None;
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

fn restart_exit_status(runtime_live: bool) -> Result<(), (i32, String)> {
    if runtime_live {
        Ok(())
    } else {
        Err((1, "restart resulted in no live runtime; inspect the saved session and vendor prerequisites".into()))
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

pub fn run<S: AgentRegistry + Handles + StateStore + Catalog + fl_core::MessageStore>(
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
                let mut rec = AreaRecord::new(&name, path, &purpose, now_at(), &by)
                    .map_err(|e| (1, e.to_string()))?;
                let st = |e: fl_core::store::StoreError| (1, e.to_string());
                if let Some(before) = store.get_area(&rec.area_key).map_err(st)?
                    && (before.parent.is_some()
                        || store
                            .list_areas()
                            .map_err(st)?
                            .iter()
                            .any(|area| area.parent.as_deref() == Some(rec.area_key.as_str())))
                {
                    if rec.path.is_some() && rec.path != before.path {
                        return Err((
                            1,
                            format!("cannot repoint linked project `{}`", rec.area_key),
                        ));
                    }
                    rec.name = before.name;
                    rec.path = before.path;
                    rec.parent = before.parent;
                }
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
            posture,
            node_id,
            hosted,
            cwd,
            by,
            json,
        } => {
            let ws_root =
                fl_core::resolve_workspace_root(&std::env::current_dir().unwrap_or_default());
            let req = SpinUpRequest {
                persona,
                project,
                role,
                vendor,
                model,
                thinking_level,
                posture,
                node_id,
                hosted: Some(hosted),
                cwd,
                context_window: None,
            };
            let _ = composed_vendor_host_adapters();
            let mut env = LifecycleEnv::new(None);
            env.workspace_root = Some(&ws_root);
            env.log_hydration_warnings = false;
            let out = lifecycle::spin_up(store, &env, &req, &by, lifecycle::Caller::Owner)
                .map_err(lifecycle_failure)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&out).unwrap());
            } else {
                println!(
                    "{}\t{}\t{}\thydration_dispatched={}\thydration_status={}",
                    out.session_id,
                    out.status,
                    out.tmux_target.as_deref().unwrap_or("-"),
                    out.hydration_dispatched,
                    out.hydration_status
                );
                for warning in &out.warnings {
                    eprintln!("warning: {warning}");
                }
            }
            Ok(())
        }
        AgentCmd::Restart { session, by, json } => {
            let _ = composed_vendor_host_adapters();
            let ws_root =
                fl_core::resolve_workspace_root(&std::env::current_dir().unwrap_or_default());
            let mut env = LifecycleEnv::new(None);
            env.workspace_root = Some(&ws_root);
            env.log_hydration_warnings = false;
            let out = lifecycle::restart_agent_session(
                store,
                &env,
                &session,
                &by,
                lifecycle::Caller::Owner,
            )
            .map_err(lifecycle_failure)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&out).unwrap());
            } else {
                println!(
                    "{}\t{}\trestart_count={}\truntime_spawned={}\truntime_live={}\thydration_dispatched={}\thydration_status={}",
                    out.record.session_id,
                    out.record.status,
                    out.record.restart_count,
                    out.record.runtime_spawned,
                    out.runtime_live,
                    out.record.hydration_dispatched,
                    out.record.hydration_status
                );
                for warning in &out.warnings {
                    eprintln!("warning: {warning}");
                }
            }
            restart_exit_status(out.runtime_live)
        }
        AgentCmd::Decommission {
            session,
            mode,
            by,
            json,
        } => {
            let mode = DecommissionMode::parse(&mode).map_err(lifecycle_failure)?;
            let _ = composed_vendor_host_adapters();
            let ws_root =
                fl_core::resolve_workspace_root(&std::env::current_dir().unwrap_or_default());
            let mut env = LifecycleEnv::new(None);
            env.workspace_root = Some(&ws_root);
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
            posture,
            mode,
            allow_blocked_sign_in,
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
                posture,
                mode,
                allow_blocked_sign_in,
                cwd,
                node_id,
                hosted,
            };
            let target =
                SessionTarget::resolve(store, &session, None).map_err(lifecycle_failure)?;
            // `fl` holds no hub prompt queue: an undelivered prepare prompt is printed instead.
            let _ = composed_vendor_host_adapters();
            let ws_root =
                fl_core::resolve_workspace_root(&std::env::current_dir().unwrap_or_default());
            let mut env = LifecycleEnv::new(None);
            env.workspace_root = Some(&ws_root);
            env.log_hydration_warnings = false;
            let out = lifecycle::remodel_session(
                store,
                &env,
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
                    "{}\t{}\t{}\thydration_dispatched={}\tremodeled_from={}\thydration_status={}",
                    out.session_id,
                    out.status,
                    out.tmux_target.as_deref().unwrap_or("-"),
                    out.hydration_dispatched,
                    out.previous_session_id,
                    out.hydration_status
                );
            }
            if !json {
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
            let runtime = fl_watch::read_runtime(store).map_err(|e| (1, e.to_string()))?;
            let heartbeats = runtime.heartbeats;
            let blocked_seats: Vec<_> = runtime
                .hosted_sessions
                .iter()
                .filter(|seat| {
                    (seat.is_running() && seat.block.is_some()) || seat.is_stopped_while_blocked()
                })
                .collect();
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
                    .filter(|entry| {
                        let slug = &entry.slug;
                        let hb = &entry.heartbeat;
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
                    .max_by(|a, b| {
                        let a = &a.heartbeat;
                        let b = &b.heartbeat;
                        (a.heartbeat_at.as_str(), a.attempt_id.as_str())
                            .cmp(&(b.heartbeat_at.as_str(), b.attempt_id.as_str()))
                    })
                    .map(|entry| entry.heartbeat);

                let (adapter, agent, verdict) = if let Some(ref hb) = matched_hb {
                    let v = evaluate_liveness(Some(hb), &now, stale_after_ms, &host, pid_is_alive);
                    (Some(hb.adapter.clone()), hb.agent.clone(), v)
                } else {
                    let host_sessions = discover_live_host_sessions_for_liveness();
                    let routes = load_routes_for_liveness();
                    if let Some((adapter, agent, v)) = hosted_session_liveness(
                        &t,
                        &host_sessions,
                        &runtime.hosted_sessions,
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
                let seat = blocked_seat_for(&blocked_seats, &t, adapter.as_deref()).or_else(|| {
                    if matched_hb.is_some() {
                        return None;
                    }
                    blocked_seat_for(&blocked_seats, agent.as_deref()?, adapter.as_deref())
                });
                let hosted_block = seat
                    .map(HostedLivenessBlock::from_session)
                    .unwrap_or_default();
                let adapter = adapter.or_else(|| seat.map(|seat| seat.vendor.clone()));
                let agent = agent.or_else(|| seat.map(|seat| seat.persona.clone()));
                if json {
                    let item = LivenessReportItem {
                        target: t,
                        attempt_id: matched_hb.as_ref().map(|h| h.attempt_id.clone()),
                        record: matched_hb.as_ref().map(|h| h.record.clone()),
                        adapter,
                        agent,
                        hosted_block,
                        verdict,
                    };
                    println!("{}", serde_json::to_string_pretty(&item).unwrap());
                } else {
                    let line =
                        format_liveness_line(&t, matched_hb.as_ref(), adapter.as_deref(), &verdict);
                    println!("{}", hosted_block.append_to(line));
                }
                Ok(())
            } else {
                let mut dedup: BTreeMap<String, (String, AttemptHeartbeat)> = BTreeMap::new();
                for entry in heartbeats {
                    let slug = entry.slug;
                    let hb = entry.heartbeat;
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
                let mut seen_agents = BTreeSet::new();
                let mut reported_blocks = BTreeSet::new();
                for (_, (slug, hb)) in dedup {
                    let verdict =
                        evaluate_liveness(Some(&hb), &now, stale_after_ms, &host, pid_is_alive);
                    let label = hb.agent.clone().unwrap_or(slug);
                    seen_agents.insert(label.to_ascii_lowercase());
                    let hosted_block = take_running_block(
                        &blocked_seats,
                        &label,
                        &hb.adapter,
                        &mut reported_blocks,
                    );
                    if json {
                        items.push(LivenessReportItem {
                            target: label,
                            attempt_id: Some(hb.attempt_id),
                            record: Some(hb.record),
                            adapter: Some(hb.adapter),
                            agent: hb.agent,
                            hosted_block,
                            verdict,
                        });
                    } else {
                        let line =
                            format_liveness_line(&label, Some(&hb), Some(&hb.adapter), &verdict);
                        println!("{}", hosted_block.append_to(line));
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
                        &[],
                        None,
                        &now,
                        &host,
                        pid_is_alive,
                    ) {
                        if !seen_agents.insert(agent.to_ascii_lowercase()) {
                            continue;
                        }
                        let hosted_block = take_running_block(
                            &blocked_seats,
                            &agent,
                            &adapter,
                            &mut reported_blocks,
                        );
                        if json {
                            items.push(LivenessReportItem {
                                target: agent.clone(),
                                attempt_id: None,
                                record: None,
                                adapter: Some(adapter),
                                agent: Some(agent),
                                hosted_block,
                                verdict,
                            });
                        } else {
                            let line = format_liveness_line(&agent, None, Some(&adapter), &verdict);
                            println!("{}", hosted_block.append_to(line));
                        }
                    }
                }
                for seat in &blocked_seats {
                    if reported_blocks.contains(seat.session_id.as_str()) {
                        continue;
                    }
                    let verdict =
                        evaluate_liveness(None, &now, stale_after_ms, &host, pid_is_alive);
                    let hosted_block = HostedLivenessBlock::from_session(seat);
                    if json {
                        items.push(LivenessReportItem {
                            target: seat.session_id.clone(),
                            attempt_id: None,
                            record: None,
                            adapter: Some(seat.vendor.clone()),
                            agent: Some(seat.persona.clone()),
                            hosted_block,
                            verdict,
                        });
                    } else {
                        let line = format_liveness_line(
                            &seat.session_id,
                            None,
                            Some(&seat.vendor),
                            &verdict,
                        );
                        println!("{}", hosted_block.append_to(line));
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
    fn remodel_blocked_sign_in_override_defaults_to_false() {
        use clap::Parser;

        for explicit in [false, true] {
            let mut args = vec!["fl", "remodel", "ada", "--vendor", "codex"];
            if explicit {
                args.push("--allow-blocked-sign-in");
            }
            let AgentCmd::Remodel {
                allow_blocked_sign_in,
                ..
            } = AgentTestCli::try_parse_from(args).unwrap().cmd
            else {
                panic!("not remodel");
            };
            assert_eq!(allow_blocked_sign_in, explicit);
        }
    }

    #[test]
    fn restart_exit_status_requires_current_live_runtime() {
        assert!(restart_exit_status(true).is_ok());
        let (code, detail) = restart_exit_status(false).unwrap_err();
        assert_ne!(code, 0);
        assert!(detail.contains("no live runtime"));
    }

    #[test]
    fn restart_parser_uses_saved_settings_without_override_flags() {
        use clap::Parser;
        let AgentCmd::Restart { session, by, json } =
            AgentTestCli::try_parse_from(["fl", "restart", "ada"])
                .unwrap()
                .cmd
        else {
            panic!("restart command expected")
        };
        assert_eq!(session, "ada");
        assert_eq!(by, "owner");
        assert!(!json);
        assert!(matches!(
            AgentTestCli::try_parse_from(["fl", "restart", "codex:ada", "--by", "fiona", "--json"]).unwrap().cmd,
            AgentCmd::Restart { by, json: true, .. } if by == "fiona"
        ));
        for flag in [
            "--model",
            "--thinking-level",
            "--effort",
            "--role",
            "--project",
            "--vendor",
            "--posture",
            "--cwd",
            "--node-id",
            "--hosted",
            "--context-window",
        ] {
            assert!(
                AgentTestCli::try_parse_from(["fl", "restart", "ada", flag, "override"]).is_err(),
                "{flag}"
            );
        }
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
    fn liveness_list_claims_the_matching_running_block_for_only_one_row() {
        let block = HostedBlock {
            reason: fl_watch::HostedBlockReason::VendorSignIn,
            observed_at_ms: 100,
        };
        let codex = WatchHostedSession {
            session_id: "codex:ada".into(),
            persona: "ada".into(),
            vendor: "codex".into(),
            status: "running".into(),
            block: Some(block),
            ..WatchHostedSession::default()
        };
        let muse = WatchHostedSession {
            session_id: "muse:ada".into(),
            vendor: "muse".into(),
            ..codex.clone()
        };
        let claude = WatchHostedSession {
            session_id: "claude:ada".into(),
            persona: "ada".into(),
            vendor: "claude".into(),
            status: "stopped".into(),
            stop_reason: Some(lifecycle::STOP_REASON_STARTUP_BLOCKED.into()),
            ..WatchHostedSession::default()
        };
        let seats = [&codex, &muse, &claude];
        let mut reported_blocks = BTreeSet::new();
        assert_eq!(
            take_running_block(&seats, "Ada", "CODEX", &mut reported_blocks).block,
            Some(block)
        );
        assert_eq!(
            take_running_block(&seats, "ada", "codex", &mut reported_blocks).block,
            None
        );
        assert_eq!(
            take_running_block(&seats, "ada", "claude", &mut reported_blocks).stop_reason,
            None
        );
        assert_eq!(reported_blocks, BTreeSet::from(["codex:ada"]));
    }

    #[test]
    fn hosted_session_liveness_prefers_canonical_id_over_reversed_alias() {
        use fl_host_proto::{
            ChildRecord, ChildState, HostInstance, InputState, SessionEpoch, SessionId,
            SessionMode, SessionSummary, SpawnKind, StreamId,
        };

        let running = |name, pid| {
            let session = SessionId::new(name).unwrap();
            SessionSummary {
                stream: StreamId {
                    host: HostInstance::new([1u8; 16]),
                    session: session.clone(),
                    epoch: SessionEpoch(2),
                },
                mode: SessionMode::Pty,
                state: ChildState::Running(ChildRecord {
                    pid,
                    session,
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
            }
        };
        let legacy = running("Codex.claude", 22);
        let canonical = running("Claude.codex", 33);
        let now = At::from_unix_millis(6_000);
        for seats in [
            [legacy.clone(), canonical.clone()],
            [canonical, legacy.clone()],
        ] {
            let (adapter, persona, verdict) =
                hosted_session_liveness("codex:claude", &seats, &[], None, &now, "host", |_| true)
                    .expect("canonical Host session is live");
            assert_eq!((adapter.as_str(), persona.as_str()), ("codex", "claude"));
            assert!(matches!(verdict, LivenessVerdict::Live { pid: 33, .. }));
        }

        let legacy_only = [legacy];
        let stopped = WatchHostedSession {
            session_id: "codex:claude".into(),
            persona: "claude".into(),
            vendor: "codex".into(),
            status: "stopped".into(),
            stop_reason: Some(lifecycle::STOP_REASON_STARTUP_BLOCKED.into()),
            ..WatchHostedSession::default()
        };
        assert!(
            hosted_session_liveness(
                "codex:claude",
                &legacy_only,
                &[stopped],
                None,
                &now,
                "host",
                |_| true,
            )
            .is_none()
        );
        let (adapter, persona, verdict) = hosted_session_liveness(
            "codex:claude",
            &legacy_only,
            &[],
            None,
            &now,
            "host",
            |_| true,
        )
        .expect("legacy alias remains available without a canonical seat");
        assert_eq!((adapter.as_str(), persona.as_str()), ("claude", "codex"));
        assert!(matches!(verdict, LivenessVerdict::Live { pid: 22, .. }));
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
            hosted_session_liveness("feira", &sessions, &[], None, &now, "ai-lab-0", |pid| {
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

        for target in [
            "Feira.antigravity",
            "Feira:antigravity",
            "Antigravity:Feira",
        ] {
            let resolved =
                hosted_session_liveness(target, &sessions, &[], None, &now, "ai-lab-0", |pid| {
                    pid == 2270049
                });
            assert_eq!(
                resolved,
                Some((adapter.clone(), agent.clone(), verdict.clone())),
                "hosted session target {target} must identify the same live session"
            );
        }

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
            hosted_session_liveness("felix", &[], &[], Some(&routes), &now, "ai-lab-0", |pid| {
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
            hosted_session_liveness("pidless", &[], &[], Some(&routes), &now, "ai-lab-0", |_| {
                true
            })
            .is_none(),
            "Agy route without live ls_pid must not report Live (pid 0)"
        );
    }

    #[test]
    fn composed_vendor_host_adapters_contains_codex_and_claude() {
        let reg = composed_vendor_host_adapters();
        assert_eq!(
            reg.vendor_ids(),
            vec!["claude".to_string(), "codex".to_string()]
        );
        let codex = reg.lookup("codex").expect("codex host adapter composed");
        assert_eq!(codex.vendor_id(), "codex");
        assert_eq!(codex.default_binary(), "codex");
        let claude = reg.lookup("claude").expect("claude host adapter composed");
        assert_eq!(claude.vendor_id(), "claude");
        assert_eq!(claude.default_binary(), "claude");
    }

    #[cfg(unix)]
    #[test]
    fn runtime_version_probes_use_composed_startup_update_controls() {
        use std::os::unix::fs::PermissionsExt as _;

        let _ = composed_vendor_host_adapters();
        let root = tempfile::tempdir().unwrap();
        for (binary, check) in [
            (
                "muse",
                r#"test "$MUSE_NO_AUTO_UPDATE" = "1" && test "$1" = "--version""#,
            ),
            (
                "codex",
                r#"test "$1" = "-c" && test "$2" = "check_for_update_on_startup=false" && test "$3" = "--version""#,
            ),
        ] {
            let path = root.path().join(binary);
            std::fs::write(&path, format!("#!/bin/sh\n{check} && printf '1.2.3\\n'\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            let version = (0..200)
                .find_map(|_| {
                    match fl_hub::runtime_matrix::run_version(
                        &path,
                        std::time::Duration::from_secs(5),
                    ) {
                        Err(e) if e.contains("Text file busy") => {
                            std::thread::sleep(std::time::Duration::from_millis(10));
                            None
                        }
                        result => Some(result),
                    }
                })
                .expect("spawn fixture")
                .expect("fixture verified the update control");
            assert_eq!(version, "1.2.3");
        }
    }

    #[test]
    fn spin_up_and_teardown_with_composed_claude_host_adapter_binds_and_archives_session_uuid() {
        if std::env::var_os("FL_TEST_COMPOSED_ADAPTER_CHILD").is_none() {
            let endpoint_root = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .env_clear().env("PATH", "/usr/bin:/bin")
                .env("HOME", endpoint_root.path())
                .env("FL_HOST_SOCK", endpoint_root.path().join("host.sock"))
                .env("TMUX", format!("{},1,0", endpoint_root.path().join("tmux.sock").display()))
                .env("FL_TEST_COMPOSED_ADAPTER_CHILD", "1")
                .args(["--exact", "cmd::agent::tests::spin_up_and_teardown_with_composed_claude_host_adapter_binds_and_archives_session_uuid", "--nocapture"])
                .output().unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
            return;
        }
        #[derive(Debug, Default)]
        struct RecordingRuntime {
            live: std::cell::RefCell<std::collections::BTreeSet<String>>,
            launches: std::cell::RefCell<Vec<fl_hub::lifecycle::LaunchSpec>>,
        }
        impl fl_hub::lifecycle::SessionRuntime for RecordingRuntime {
            fn spawn(
                &self,
                session: &str,
                _window: &str,
                _cwd: Option<&str>,
                launch: Option<&fl_hub::lifecycle::LaunchSpec>,
                _hosted_id: &str,
            ) -> bool {
                self.live.borrow_mut().insert(session.to_string());
                if let Some(spec) = launch {
                    self.launches.borrow_mut().push(spec.clone());
                }
                true
            }
            fn kill(&self, unit: &str, _hosted_id: &str) -> bool {
                self.live.borrow_mut().remove(unit);
                true
            }
            fn exists(&self, unit: &str) -> bool {
                self.live.borrow().contains(unit)
            }
            fn owns(&self, unit: &str, _hosted_id: &str) -> bool {
                self.live.borrow().contains(unit)
            }
        }

        let _ = composed_vendor_host_adapters();
        let store = MemStore::default();
        let home = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let proj_dir = ws.path().join("Kaggle-Gemma4");
        std::fs::create_dir_all(&proj_dir).unwrap();

        let runtime = RecordingRuntime::default();
        let mut env = LifecycleEnv::new(Some(home.path()));
        env.spawn_runtime = true;
        env.runtime = &runtime;
        env.workspace_root = Some(ws.path());

        let req = SpinUpRequest {
            persona: "kayla".into(),
            project: "kaggle-gemma4".into(),
            role: "lead".into(),
            vendor: "claude".into(),
            ..SpinUpRequest::default()
        };
        let out = lifecycle::spin_up(&store, &env, &req, "owner", lifecycle::Caller::Owner)
            .expect("claude spin-up succeeds");
        assert!(out.runtime_spawned);

        let rec = lifecycle::find_hosted_session(&store, &["claude:kayla"])
            .unwrap()
            .expect("hosted record persisted");
        assert_eq!(
            rec.cwd.as_deref(),
            Some(proj_dir.to_string_lossy().as_ref())
        );
        let sid = rec
            .vendor_session_id
            .clone()
            .expect("vendor_session_id persisted on HostedSessionRecord");

        let launches = runtime.launches.borrow();
        assert_eq!(launches.len(), 1);
        let argv = &launches[0].argv;
        assert!(
            argv.len() >= 8,
            "expected claude launch argv with session-id and name, got {argv:?}"
        );
        assert_eq!(
            &argv[1..8],
            &[
                "--remote-control".to_string(),
                "--permission-mode".to_string(),
                "auto".to_string(),
                "--session-id".to_string(),
                sid.clone(),
                "--name".to_string(),
                "Kayla".to_string(),
            ]
        );

        // Create a transcript file for this exact session UUID and verify teardown archives it.
        let transcript_dir = home
            .path()
            .join(".claude/projects/-workspace-kaggle-gemma4");
        std::fs::create_dir_all(&transcript_dir).unwrap();
        let transcript_path = transcript_dir.join(format!("{sid}.jsonl"));
        std::fs::write(&transcript_path, "{\"type\":\"user\"}\n").unwrap();

        let target = SessionTarget::resolve(&store, "claude:kayla", None).unwrap();
        let torn = lifecycle::teardown_session(
            &store,
            &env,
            &target,
            "owner",
            lifecycle::Caller::Owner,
            |_| {},
        )
        .expect("teardown succeeds");
        assert_eq!(torn.status, lifecycle::STATUS_DECOMMISSIONED);

        let after_rec = lifecycle::find_hosted_session(&store, &["claude:kayla"])
            .unwrap()
            .unwrap();
        assert!(after_rec.tombstone_ids.contains(&sid));
        let archived_path = home
            .path()
            .join(".claude/archived/-workspace-kaggle-gemma4")
            .join(format!("{sid}.jsonl"));
        assert!(
            archived_path.is_file(),
            "ClaudeHostAdapter::archive must move transcript to archived path"
        );
        assert!(!transcript_path.exists());
    }

    #[test]
    fn cli_spin_up_and_remodel_parse_posture_and_reject_unsupported_vendor_posture() {
        use clap::Parser;
        use fl_core::permissions::Posture;

        #[derive(Parser)]
        struct TestCli {
            #[command(subcommand)]
            cmd: AgentCmd,
        }

        // Default posture is `auto` when `--posture` is omitted.
        let parsed = TestCli::try_parse_from([
            "agent",
            "spin-up",
            "ada",
            "--project",
            "FerroLoop",
            "--role",
            "assistant",
            "--vendor",
            "claude",
            "--by",
            "owner",
        ])
        .unwrap();
        match parsed.cmd {
            AgentCmd::SpinUp { posture, .. } => assert_eq!(posture, Posture::Auto),
            _ => panic!("expected SpinUp"),
        }

        // Explicit `--posture` values parse for both `spin-up` and `remodel`.
        for (raw, want) in [
            ("ask", Posture::Ask),
            ("accept-edits", Posture::AcceptEdits),
            ("auto", Posture::Auto),
            ("plan", Posture::Plan),
            ("bypass", Posture::Bypass),
        ] {
            let spin_cli = TestCli::try_parse_from([
                "agent",
                "spin-up",
                "ada",
                "--project",
                "FerroLoop",
                "--role",
                "assistant",
                "--vendor",
                "claude",
                "--posture",
                raw,
                "--by",
                "owner",
            ])
            .unwrap();
            match spin_cli.cmd {
                AgentCmd::SpinUp { posture, .. } => assert_eq!(posture, want),
                _ => panic!("expected SpinUp"),
            }

            let remodel_cli = TestCli::try_parse_from([
                "agent",
                "remodel",
                "claude:ada",
                "--vendor",
                "codex",
                "--posture",
                raw,
            ])
            .unwrap();
            match remodel_cli.cmd {
                AgentCmd::Remodel { posture, .. } => assert_eq!(posture, want),
                _ => panic!("expected Remodel"),
            }
        }

        // Unknown `--posture` value is rejected by clap.
        assert!(
            TestCli::try_parse_from([
                "agent",
                "spin-up",
                "ada",
                "--project",
                "FerroLoop",
                "--role",
                "assistant",
                "--vendor",
                "claude",
                "--posture",
                "invalid-posture",
                "--by",
                "owner",
            ])
            .is_err()
        );

        // Unsupported `(vendor, posture)` on `agent::run` returns exit code 1 with zero side effects.
        let store = MemStore::default();
        let err = run(
            &store,
            AgentCmd::SpinUp {
                persona: "ada".into(),
                project: "FerroLoop".into(),
                role: "assistant".into(),
                vendor: "codex".into(),
                model: None,
                thinking_level: None,
                posture: Posture::Ask,
                node_id: None,
                hosted: false,
                cwd: None,
                by: "owner".into(),
                json: true,
            },
        )
        .unwrap_err();
        assert_eq!(err.0, 1);
        assert!(err.1.contains("posture"), "unexpected error: {}", err.1);
        assert!(store.agent_by_name("ada").unwrap().is_none());
    }
}
