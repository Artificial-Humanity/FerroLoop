//! Host reboot behaviour of the hub: reconcile the tmux panes a boot restore started with the
//! agent registry, and tell each active agent once per boot that the host restarted.
//!
//! One [`reconcile_pass`] does, in this order:
//!
//! 1. Scan the host and save the route table, so an active agent's route is registered from its
//!    own session identity (transcript first, see `fl_wire::identity::resolve_persona`).
//! 2. For every restored pane that runs a vendor CLI, take the persona from the pane's tmux
//!    session name (never from an inherited `AGENT_PERSONA`), or, in the shared Muse session,
//!    from the pane's window name. A persona that is not an active agent (retired, or not in the
//!    registry) is reported once as a `session.restore_orphan` event; the pane is not killed.
//!    Routes are removed for a RETIRED persona only: the route keyed by its name, and any route
//!    (whatever persona it is keyed to) that names the retired pane (`tmux_pane`) or its window
//!    (`tmux_target`). Only Grok and Muse routes carry a pane or target; a Codex or Claude route
//!    cannot be matched to a pane. The hub's registry is not the agent bus's, so a live agent
//!    that is unknown to the hub keeps its routes and is only reported.
//! 3. Send each expected agent whose route is live one `system` message, "Host restarted", once
//!    per boot (see below for who is expected and what live means).
//!
//! The boot is the host's `boot_id`; the boot record ([`EPOCH_SLUG`], a runtime state entry)
//! holds the `expected` agents (the active agents when the record was made) and the `notified`
//! ones, so a hub restart in the same boot does not send again and a new boot does.
//!
//! - A saved record with another `boot_id` proves a reboot: a new record opens, whatever the
//!   uptime, and its `expected` agents are the active agents at that moment.
//! - With no record at all the hub cannot tell a reboot from a first start, so the uptime decides:
//!   a host up no longer than [`NOTICE_WINDOW`] counts as just rebooted (`expected` = the active
//!   agents now), an older host expects nobody.
//! - A record of the same `boot_id` keeps its `expected` list: an agent registered after the
//!   record was made is never told.
//!
//! A route is live for the notice only if the pre-delivery check passes ([`undeliverable`]) and it
//! was (re)discovered after the host booted (`updated_at` at or after boot time), or a restored
//! vendor-CLI pane of the persona exists. A route saved before the reboot proves nothing. An
//! agent that is not live yet is retried by the next pass.
//!
//! The notice is at most once: the name is saved before the send and rolled back if the send
//! fails; a name this process sent is also kept in memory across passes.
//!
//! [`run`] keeps making passes until no agent is pending, tmux has answered at least once and
//! [`MIN_WINDOW`] has passed, but never more than [`MAX_PASSES`]. The orphan check runs on every
//! pass, so a pane restored late is still found; each orphan is reported once.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, UNIX_EPOCH};

use fl_core::agent::AgentStatus;
use fl_core::ferrowire::{MessageKind, MessageUrgency, SendWireArgs, WireAddress, WireFilter};
use fl_core::state::{
    MergeMode, RuntimeEntry, SensitivityPatch, StateConcern, StatePayload, StateScope,
    UpsertStateArgs,
};
use fl_core::store::{AgentRegistry, MessageStore, StateStore};
use fl_core::{VendorRegistry, VendorTransport};
use fl_exec::stamp;
use fl_wire::identity::{RouteEntry, RouteTable, VendorRoute, normalize_persona};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::HubState;
use crate::lifecycle::{
    HostedSessionRecord, HydrationReceipt, MUSE_SESSION, STATUS_DECOMMISSIONED,
    STATUS_DECOMMISSIONING, STATUS_RUNNING, hydration_ack_slug, is_valid_hydration_generation,
    list_hosted_sessions, remove_routes, route_table_paths, tmux_stdout, tmux_stdout_of,
    undeliverable,
};

/// Runtime state entry (workspace scope) that holds the boot the hub last reconciled.
pub(crate) const EPOCH_SLUG: &str = "host-boot-epoch";
/// The subject of the notice.
pub(crate) const NOTICE_SUBJECT: &str = "Host restarted";
/// The body of the notice when no specific modified instruction files can be enumerated.
pub(crate) const NOTICE_BODY: &str = "The host restarted. Re-read any instruction files (`AGENTS.md`, `WORKFLOW.md`, `PERSONA.md`, or your role guidance file) modified since your hydration, then resume your task from your last handoff (`fl state handoff get ...`) and reply to confirm.";
/// The sender of the notice, as for the hub's other system messages.
const NOTICE_SENDER: &str = "system";
/// With no boot record, the host counts as just rebooted while it has been up no longer than this.
pub(crate) const NOTICE_WINDOW: Duration = Duration::from_secs(60 * 60);
/// Time between the passes of [`run`].
pub(crate) const RETRY_INTERVAL: Duration = Duration::from_secs(15);
/// Most passes [`run`] makes, so a route that never comes up is not waited on for ever.
pub(crate) const MAX_PASSES: u32 = 20;
/// [`run`] does not stop before this much time has passed, even when nothing is pending, so a
/// pane that tmux restores late is still found.
pub(crate) const MIN_WINDOW: Duration = Duration::from_secs(60);

/// A tmux pane the host runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Pane {
    pub(crate) session: String,
    pub(crate) pane_id: String,
    /// `window_name`.
    pub(crate) window: String,
    /// `pane_current_command`.
    pub(crate) command: String,
    /// `pane_start_command`.
    pub(crate) start_command: String,
}

/// Where the panes come from: the host's tmux, or a fake in tests.
pub(crate) trait PaneSource {
    /// Every pane, or `None` when tmux did not answer (no server, or it is not trustworthy).
    fn panes(&self) -> Option<Vec<Pane>>;
}

/// The host's tmux: the binary `bin`, else `FL_WIRE_TMUX_BIN`, else `tmux`. Only lists panes.
#[derive(Default)]
pub(crate) struct TmuxPanes {
    pub(crate) bin: Option<std::path::PathBuf>,
}

/// The `list-panes` format [`parse_panes`] reads.
pub(crate) const PANE_FORMAT: &str =
    "#{session_name}\t#{pane_id}\t#{window_name}\t#{pane_current_command}\t#{pane_start_command}";

impl PaneSource for TmuxPanes {
    fn panes(&self) -> Option<Vec<Pane>> {
        let args = ["list-panes", "-a", "-F", PANE_FORMAT];
        let out = match &self.bin {
            Some(bin) => tmux_stdout_of(bin, &args)?,
            None => tmux_stdout(&args)?,
        };
        Some(parse_panes(&out))
    }
}

/// The host's `fl-host` session supervisor, with fallback to `TmuxPanes` when `FL_WIRE_TMUX_BIN`
/// is set and `fl-host` is unreachable.
#[derive(Default)]
pub(crate) struct HostPanes;

impl PaneSource for HostPanes {
    fn panes(&self) -> Option<Vec<Pane>> {
        let endpoint = fl_host_proto::HostEndpoint::discover().ok();
        let client_res = endpoint
            .as_ref()
            .and_then(|ep| fl_host_proto::BlockingHostClient::connect(ep).ok());
        if let Some(mut client) = client_res
            && let Ok(sessions) = client.list_sessions()
        {
            let mut out = Vec::new();
            for s in sessions {
                if !matches!(s.state, fl_host_proto::ChildState::Running(_)) {
                    continue;
                }
                let unit = fl_wire::vendor_adapter::host_session_id_to_unit(&s.stream.session);
                let (session, window) = match unit.split_once(':') {
                    Some((sess, win)) => (sess.to_string(), win.to_string()),
                    None => (unit, String::new()),
                };
                let bin = if session.eq_ignore_ascii_case(MUSE_SESSION) {
                    "muse".to_string()
                } else {
                    fl_wire::vendor_adapter::lookup_vendor_cli_adapter(&window)
                        .map(|a| a.default_binary().to_string())
                        .unwrap_or_else(|| window.clone())
                };
                out.push(Pane {
                    session,
                    pane_id: s.stream.session.as_str().to_string(),
                    window,
                    command: bin.clone(),
                    start_command: bin,
                });
            }
            return Some(out);
        }
        if std::env::var_os("FL_WIRE_TMUX_BIN").is_some() {
            return TmuxPanes::default().panes();
        }
        None
    }
}

pub(crate) fn parse_panes(text: &str) -> Vec<Pane> {
    text.lines()
        .filter_map(|line| {
            let mut f = line.splitn(5, '\t');
            let session = f.next().filter(|s| !s.is_empty())?;
            Some(Pane {
                session: session.to_string(),
                pane_id: f.next().unwrap_or_default().to_string(),
                window: f.next().unwrap_or_default().to_string(),
                command: f.next().unwrap_or_default().to_string(),
                start_command: f.next().unwrap_or_default().to_string(),
            })
        })
        .collect()
}

/// What the hub knows about this boot of the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Host {
    /// `/proc/sys/kernel/random/boot_id`: one value per boot. `None` when unreadable.
    pub(crate) boot_id: Option<String>,
    /// Seconds since boot (`/proc/uptime`).
    pub(crate) uptime: Option<Duration>,
}

impl Host {
    pub(crate) fn current() -> Self {
        let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let uptime = std::fs::read_to_string("/proc/uptime")
            .ok()
            .and_then(|s| s.split_whitespace().next()?.parse::<f64>().ok())
            .filter(|s| s.is_finite() && *s >= 0.0)
            .map(Duration::from_secs_f64);
        Self { boot_id, uptime }
    }
}

/// A restored pane whose persona is not an active agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct OrphanPane {
    pub(crate) session: String,
    pub(crate) pane_id: String,
    /// The tmux window of the pane (the persona's window in the shared Muse session).
    #[serde(default)]
    pub(crate) window: String,
    /// The persona the pane stands for, when it is a valid one.
    pub(crate) persona: Option<String>,
    /// `retired` or `unknown`.
    pub(crate) reason: String,
    pub(crate) command: String,
}

/// The persisted state of one boot ([`EPOCH_SLUG`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct EpochRecord {
    pub(crate) boot_id: String,
    pub(crate) started_at_ms: u64,
    /// The active personas owed the notice in this boot, fixed when the record was made.
    #[serde(default)]
    pub(crate) expected: Vec<String>,
    /// Personas already sent the notice in this boot.
    #[serde(default)]
    pub(crate) notified: Vec<String>,
    /// Orphan panes already reported in this boot.
    #[serde(default)]
    pub(crate) orphans: Vec<OrphanPane>,
}

/// What one pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Report {
    /// Orphan panes first reported by this pass.
    pub(crate) orphans: Vec<OrphanPane>,
    /// Personas sent the notice by this pass.
    pub(crate) notified: Vec<String>,
    /// Expected personas still owed the notice (no live route yet).
    pub(crate) pending: Vec<String>,
    /// Whether tmux answered the pane listing in this pass.
    pub(crate) tmux_answered: bool,
}

/// What a run of passes remembers between them, so a failed store write never resends.
#[derive(Debug, Default)]
pub(crate) struct Memory {
    pub(crate) boot_id: Option<String>,
    /// Personas this process sent the notice to in the boot `boot_id`.
    pub(crate) notified: BTreeSet<String>,
    /// Test hook: every save of a record that lists this persona as notified fails.
    #[cfg(test)]
    pub(crate) fail_save_notified: Option<String>,
    /// Test hook: this many sends fail.
    #[cfg(test)]
    pub(crate) fail_sends: u32,
    /// Test hook: this many store opens for a send fail.
    #[cfg(test)]
    pub(crate) fail_send_opens: u32,
}

pub(crate) fn vendor_binaries() -> Vec<String> {
    let mut bins: BTreeSet<String> = VendorRegistry::global()
        .list()
        .into_iter()
        .filter_map(|v| match v.transport {
            VendorTransport::SubscriptionCli { default_binary } => Path::new(&default_binary)
                .file_name()
                .map(|n| n.to_string_lossy().to_ascii_lowercase()),
            VendorTransport::HttpApi { .. } => None,
        })
        .collect();
    for b in fl_wire::vendor_adapter::vendor_cli_binaries() {
        bins.insert(b);
    }
    bins.into_iter().collect()
}

/// Whether a pane runs a vendor CLI: one of the first words of its current or start command is a
/// vendor's binary (`codex resume ...`, `env X=1 claude ...`, `/usr/bin/grok`).
pub(crate) fn runs_vendor_cli(pane: &Pane, binaries: &[String]) -> bool {
    [&pane.command, &pane.start_command]
        .into_iter()
        .flat_map(|c| c.split_whitespace().take(3))
        .filter_map(|w| Path::new(w).file_name())
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .any(|n| binaries.contains(&n))
}

fn epoch_state<S: StateStore>(store: &S) -> Option<EpochRecord> {
    let entry = store
        .get_state_by_key(&StateScope::Workspace, StateConcern::Runtime, EPOCH_SLUG)
        .ok()??;
    match entry.payload {
        StatePayload::Runtime(rt) => serde_json::from_value(rt.value).ok(),
        _ => None,
    }
}

fn save_epoch<S: StateStore>(store: &S, record: &EpochRecord) -> Result<(), String> {
    store
        .upsert_state(UpsertStateArgs {
            scope: StateScope::Workspace,
            slug: EPOCH_SLUG.to_string(),
            sensitivity: SensitivityPatch::Inherit,
            payload: StatePayload::Runtime(RuntimeEntry {
                value: serde_json::to_value(record).map_err(|e| e.to_string())?,
                merge_mode: MergeMode::LastWriter,
            }),
            at: stamp::now(),
            by: "hub".to_string(),
        })
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// The personas that must not be told while their session is being decommissioned.
fn decommissioning(records: &[HostedSessionRecord]) -> Vec<String> {
    records
        .iter()
        .filter(|r| r.status.eq_ignore_ascii_case(STATUS_DECOMMISSIONING))
        .map(|r| r.persona.to_ascii_lowercase())
        .collect()
}

fn safe_role_stem(role_label: Option<&str>) -> Option<String> {
    let raw = role_label?.trim();
    if raw.is_empty() {
        return None;
    }
    let lowered = raw.to_ascii_lowercase();
    let stem = lowered.strip_prefix("custom:").unwrap_or(&lowered);
    if stem.is_empty()
        || stem == "."
        || stem.contains("..")
        || stem.contains('/')
        || stem.contains('\\')
        || stem.contains(':')
        || stem.chars().any(char::is_control)
    {
        return None;
    }
    let mut components = Path::new(stem).components();
    match (components.next(), components.next()) {
        (Some(std::path::Component::Normal(_)), None) => Some(stem.to_string()),
        _ => None,
    }
}

fn regular_file_canonical_and_mtime_ms(path: &Path) -> Option<(PathBuf, u64)> {
    let canonical = path.canonicalize().ok()?;
    let meta = std::fs::metadata(&canonical).ok()?;
    if !meta.is_file() {
        return None;
    }
    let modified = meta.modified().ok()?;
    let dur = modified.duration_since(UNIX_EPOCH).ok()?;
    Some((canonical, millis(dur)))
}

const MODIFIED_NOTICE_PREFIX: &str = "The host restarted. Re-read instruction files modified since your hydration before continuing: ";
const MODIFIED_NOTICE_SUFFIX: &str = ". Then resume your task from your last handoff (`fl state handoff get ...`) and reply to confirm.";

fn format_modified_instruction_notice(modified: &[String]) -> String {
    if modified.is_empty() {
        NOTICE_BODY.to_string()
    } else {
        format!(
            "{MODIFIED_NOTICE_PREFIX}{}{MODIFIED_NOTICE_SUFFIX}",
            modified.join(", ")
        )
    }
}

fn extract_modified_instruction_files(body: &str) -> Vec<String> {
    let Some(rest) = body.strip_prefix(MODIFIED_NOTICE_PREFIX) else {
        return Vec::new();
    };
    let Some(list_part) = rest.strip_suffix(MODIFIED_NOTICE_SUFFIX) else {
        return Vec::new();
    };
    list_part
        .split(", ")
        .map(str::trim)
        .filter(|s| s.starts_with('`') && s.ends_with('`') && s.len() > 2)
        .map(str::to_string)
        .collect()
}

fn merge_modified_instruction_files(existing: &[Vec<String>], candidate: &[String]) -> Vec<String> {
    const CANONICAL_PREFIX: [&str; 3] = ["`AGENTS.md`", "`WORKFLOW.md`", "`PERSONA.md`"];
    let mut union: Vec<String> = Vec::new();
    for list in existing.iter().map(Vec::as_slice).chain([candidate]) {
        for item in list {
            if !union.contains(item) {
                union.push(item.clone());
            }
        }
    }
    let mut ordered: Vec<String> = Vec::with_capacity(union.len());
    for fixed in CANONICAL_PREFIX {
        if union.iter().any(|s| s == fixed) {
            ordered.push(fixed.to_string());
        }
    }
    for item in union {
        if !ordered.contains(&item) {
            ordered.push(item);
        }
    }
    ordered
}

/// Format the `"Host restarted"` instruction-refresh notice body, listing any candidate
/// instruction files (`AGENTS.md`, `WORKFLOW.md`, `PERSONA.md`, `docs/roles/<role>.md`,
/// `.agents/roles/<role>.md`, `../.agents/roles/<role>.md`) whose `mtime_ms > baseline_ms`, or
/// returning [`NOTICE_BODY`] when none changed or `cwd` / `baseline_ms` is unknown.
pub(crate) fn format_host_restarted_notice(
    cwd: Option<&Path>,
    role_label: Option<&str>,
    baseline_ms: Option<u64>,
) -> String {
    let (Some(cwd), Some(baseline_ms)) = (cwd, baseline_ms) else {
        return NOTICE_BODY.to_string();
    };
    let mut candidates = vec![
        "AGENTS.md".to_string(),
        "WORKFLOW.md".to_string(),
        "PERSONA.md".to_string(),
    ];
    if let Some(role) = safe_role_stem(role_label) {
        candidates.push(format!("docs/roles/{role}.md"));
        candidates.push(format!(".agents/roles/{role}.md"));
        candidates.push(format!("../.agents/roles/{role}.md"));
    }
    let mut seen_canonical = BTreeSet::new();
    let mut modified = Vec::new();
    for rel in candidates {
        let candidate = cwd.join(&rel);
        let Some((canonical, mtime_ms)) = regular_file_canonical_and_mtime_ms(&candidate) else {
            continue;
        };
        if !seen_canonical.insert(canonical) {
            continue;
        }
        if mtime_ms > baseline_ms {
            modified.push(format!("`{rel}`"));
        }
    }
    format_modified_instruction_notice(&modified)
}

/// Resolve the hydration baseline timestamp (`hydration-ack-<gen>.accepted_at_ms`, falling back to
/// `record.created_at_ms` when positive) for comparing instruction file modification times.
pub(crate) fn hydration_baseline_ms<S: StateStore>(
    store: &S,
    record: &HostedSessionRecord,
) -> Option<u64> {
    if let Some(generation) = record.hydration_generation.as_deref()
        && is_valid_hydration_generation(generation)
        && let Ok(Some(entry)) = store.get_state_by_key(
            &StateScope::Workspace,
            StateConcern::Runtime,
            &hydration_ack_slug(generation),
        )
        && let StatePayload::Runtime(rt) = entry.payload
        && let Ok(receipt) = serde_json::from_value::<HydrationReceipt>(rt.value)
        && receipt.persona.eq_ignore_ascii_case(&record.persona)
        && (receipt.session_id.is_empty()
            || receipt.session_id.eq_ignore_ascii_case(&record.session_id))
        && receipt.accepted_at_ms > 0
    {
        return Some(receipt.accepted_at_ms);
    }
    (record.created_at_ms > 0).then_some(record.created_at_ms)
}

fn notice_body_for_persona<S: StateStore>(
    store: &S,
    hosted: &[HostedSessionRecord],
    persona: &str,
) -> String {
    let matching = hosted
        .iter()
        .filter(|r| {
            r.persona.eq_ignore_ascii_case(persona)
                && !r.status.eq_ignore_ascii_case(STATUS_DECOMMISSIONING)
                && !r.status.eq_ignore_ascii_case(STATUS_DECOMMISSIONED)
        })
        .max_by_key(|r| {
            (
                r.status.eq_ignore_ascii_case(STATUS_RUNNING),
                r.updated_at_ms,
            )
        });
    let cwd = matching.and_then(|r| r.cwd.as_deref()).map(Path::new);
    let role_label = matching
        .map(|r| r.role.as_str())
        .filter(|s| !s.trim().is_empty());
    let baseline_ms = matching.and_then(|r| hydration_baseline_ms(store, r));
    format_host_restarted_notice(cwd, role_label, baseline_ms)
}

fn send_notice_with_body<S: MessageStore + AgentRegistry>(
    store: &S,
    persona: &str,
    body: &str,
) -> Result<(), String> {
    store
        .send_wire(SendWireArgs {
            id: None,
            sender_id: None,
            sender_name: NOTICE_SENDER.into(),
            sender_host: None,
            to: WireAddress::AgentName(persona.to_string()),
            kind: MessageKind::Mail,
            urgency: MessageUrgency::Normal,
            durable: true,
            subject: NOTICE_SUBJECT.into(),
            body: body.to_string(),
            reply_to: None,
            thread_id: None,
            deadline_at: None,
            at: stamp::now(),
        })
        .map(|_| ())
        .map_err(|e| e.to_string())
}

fn record_epoch_notified_best_effort<S: StateStore>(store: &S, host: &Host, persona: &str) {
    if let Some(boot_id) = host.boot_id.as_deref()
        && let Some(mut epoch) = epoch_state(store)
        && epoch.boot_id == boot_id
        && !epoch
            .notified
            .iter()
            .any(|n| n.eq_ignore_ascii_case(persona))
    {
        epoch.notified.push(persona.to_string());
        if let Err(e) = save_epoch(store, &epoch) {
            eprintln!(
                "hub: failed to record service-resume notice for `{persona}` in boot epoch: {e}"
            );
        }
    }
}

/// Enqueue a durable `"Host restarted"` system mail for a service-level `--resume` respawn in
/// `store`, deduplicating against any unread `"Host restarted"` mail already in `persona`'s inbox
/// and marking `EpochRecord.notified` for `host.boot_id` when present so a subsequent
/// [`reconcile_pass_with`] in the same boot does not send a duplicate.
pub(crate) fn send_service_resume_notice_if_unnotified_in_store<
    S: MessageStore + AgentRegistry + StateStore,
>(
    store: &S,
    host: &Host,
    persona: &str,
    cwd: Option<&Path>,
    role_label: Option<&str>,
    baseline_ms: Option<u64>,
) -> Result<bool, String> {
    let Some(persona) = normalize_persona(persona) else {
        return Err(format!("invalid persona `{persona}`"));
    };

    let agents = store.list_agents(None, true).map_err(|e| e.to_string())?;
    let is_active = agents
        .iter()
        .any(|a| a.name_key.eq_ignore_ascii_case(&persona) && a.status == AgentStatus::Active);
    let is_retired = !is_active
        && agents
            .iter()
            .any(|a| a.name_key.eq_ignore_ascii_case(&persona) && a.status == AgentStatus::Retired);
    if is_retired {
        return Ok(false);
    }

    let hosted = list_hosted_sessions(store).unwrap_or_default();
    let persona_hosted: Vec<&HostedSessionRecord> = hosted
        .iter()
        .filter(|r| r.persona.eq_ignore_ascii_case(&persona))
        .collect();
    if persona_hosted
        .iter()
        .any(|r| r.status.eq_ignore_ascii_case(STATUS_DECOMMISSIONING))
        || (!persona_hosted.is_empty()
            && persona_hosted
                .iter()
                .all(|r| r.status.eq_ignore_ascii_case(STATUS_DECOMMISSIONED)))
    {
        return Ok(false);
    }

    let candidate_body = format_host_restarted_notice(cwd, role_label, baseline_ms);
    let unread = store
        .list_wire_inbox(&WireFilter {
            recipient: Some(persona.clone()),
            unread_only: true,
            ..WireFilter::default()
        })
        .map_err(|e| e.to_string())?;
    let unread_notices: Vec<_> = unread
        .into_iter()
        .filter(|item| {
            item.message.sender_name == NOTICE_SENDER && item.message.subject == NOTICE_SUBJECT
        })
        .collect();
    if !unread_notices.is_empty() {
        let existing_file_lists: Vec<Vec<String>> = unread_notices
            .iter()
            .map(|item| extract_modified_instruction_files(&item.message.body))
            .collect();
        let latest_files = existing_file_lists.last().cloned().unwrap_or_default();
        let candidate_files = extract_modified_instruction_files(&candidate_body);
        let merged_files = merge_modified_instruction_files(&existing_file_lists, &candidate_files);

        if merged_files.is_empty() || merged_files == latest_files {
            record_epoch_notified_best_effort(store, host, &persona);
            return Ok(false);
        }

        let merged_body = format_modified_instruction_notice(&merged_files);
        let now_at = stamp::now();
        for item in &unread_notices {
            store
                .update_wire_receipt(
                    &item.message.id,
                    &persona,
                    fl_core::ferrowire::DeliveryStatus::Acked,
                    false,
                    now_at.clone(),
                )
                .map_err(|e| e.to_string())?;
        }
        send_notice_with_body(store, &persona, &merged_body)?;
        record_epoch_notified_best_effort(store, host, &persona);
        return Ok(true);
    }

    send_notice_with_body(store, &persona, &candidate_body)?;
    record_epoch_notified_best_effort(store, host, &persona);
    Ok(true)
}

/// Convenience wrapper around [`send_service_resume_notice_if_unnotified_in_store`] using
/// [`Host::current`].
pub(crate) fn send_service_resume_notice_in_store<S: MessageStore + AgentRegistry + StateStore>(
    store: &S,
    persona: &str,
    cwd: Option<&Path>,
    role_label: Option<&str>,
    baseline_ms: Option<u64>,
) -> Result<bool, String> {
    send_service_resume_notice_if_unnotified_in_store(
        store,
        &Host::current(),
        persona,
        cwd,
        role_label,
        baseline_ms,
    )
}

/// Enqueue a durable `"Host restarted"` system mail for a service-level `--resume` respawn via
/// `state`, deduplicating against unread inbox notices and updating `EpochRecord.notified` for
/// `host.boot_id` when present.
#[allow(dead_code)]
pub(crate) fn send_service_resume_notice_if_unnotified(
    state: &HubState,
    host: &Host,
    persona: &str,
    cwd: Option<&Path>,
    role_label: Option<&str>,
    baseline_ms: Option<u64>,
) -> Result<bool, String> {
    let store = state.open_store().map_err(|e| e.to_string())?;
    send_service_resume_notice_if_unnotified_in_store(
        &*store,
        host,
        persona,
        cwd,
        role_label,
        baseline_ms,
    )
}

/// Convenience wrapper around [`send_service_resume_notice_if_unnotified`] using [`Host::current`].
#[allow(dead_code)]
pub(crate) fn send_service_resume_notice(
    state: &HubState,
    persona: &str,
    cwd: Option<&Path>,
    role_label: Option<&str>,
    baseline_ms: Option<u64>,
) -> Result<bool, String> {
    send_service_resume_notice_if_unnotified(
        state,
        &Host::current(),
        persona,
        cwd,
        role_label,
        baseline_ms,
    )
}

impl Memory {
    /// Open the store for one send; the error is the send's error.
    fn open_for_send<'s>(
        &mut self,
        state: &'s HubState,
    ) -> Result<crate::HubStoreGuard<'s>, String> {
        #[cfg(test)]
        if self.fail_send_opens > 0 {
            self.fail_send_opens -= 1;
            return Err("injected store failure".to_string());
        }
        state.open_store().map_err(|e| e.to_string())
    }

    /// Test hook: whether this send is to fail.
    fn take_send_failure(&mut self) -> bool {
        #[cfg(test)]
        if self.fail_sends > 0 {
            self.fail_sends -= 1;
            return true;
        }
        false
    }
}

/// Whose pane this is.
enum PaneOwner {
    /// The pane cannot be tied to one persona (a pane of the shared Muse session with no window
    /// name): it is neither reconciled nor reported.
    Unknown,
    /// The persona the pane stands for; `None` when its name is not a valid persona.
    Persona(Option<String>),
}

/// The persona of a pane: its tmux session name, except in the shared Muse session, where every
/// agent has a window named by its persona and the session name stands for nobody.
fn pane_owner(pane: &Pane) -> PaneOwner {
    if pane.session.eq_ignore_ascii_case(MUSE_SESSION) {
        return match pane.window.trim() {
            "" => PaneOwner::Unknown,
            window => PaneOwner::Persona(normalize_persona(window)),
        };
    }
    PaneOwner::Persona(normalize_persona(&pane.session))
}

/// Whether the saved route entry of an agent proves a session that is up after this boot: the
/// pre-delivery check passes, and the route was (re)discovered after the host booted or a
/// restored vendor-CLI pane of the persona exists.
fn route_is_live(entry: &RouteEntry, boot_ms: Option<u64>, pane_present: bool) -> bool {
    if undeliverable(&entry.route).is_some() {
        return false;
    }
    pane_present || boot_ms.is_some_and(|boot| entry.updated_at.unix_millis() >= boot)
}

/// Whether `route` names the tmux pane `pane_id`, or the window `target` (`session:window`) it
/// lives in. Only the Grok and Muse routes carry a tmux pane or target; a Codex or Claude route
/// names a conversation or a process and cannot be tied to a pane this way.
fn route_names_pane(route: &VendorRoute, pane_id: &str, target: &str) -> bool {
    match route {
        VendorRoute::Grok {
            tmux_target,
            tmux_pane,
            ..
        }
        | VendorRoute::Muse {
            tmux_target,
            tmux_pane,
            ..
        }
        | VendorRoute::HostCli {
            tmux_target,
            tmux_pane,
            ..
        } => {
            (!pane_id.is_empty() && tmux_pane.as_deref() == Some(pane_id))
                || tmux_target.as_deref() == Some(target)
        }
        _ => false,
    }
}

/// Drop every route that names the restored pane `pane` or its window, whatever persona the route
/// is keyed to: a retired pane's registration can sit under another persona's key. Returns the
/// count. Holds the hub's route-file lock like [`remove_routes`].
fn remove_routes_of_pane(home: Option<&Path>, pane: &Pane) -> usize {
    let target = format!("{}:{}", pane.session, pane.window);
    crate::discovery::with_route_file_lock(|| {
        let mut removed = 0;
        for path in route_table_paths(home) {
            if !path.is_file() {
                continue;
            }
            let mut table = RouteTable::load(Some(&path));
            let before = table.entries.len();
            table
                .entries
                .retain(|_, e| !route_names_pane(&e.route, &pane.pane_id, &target));
            let dropped = before - table.entries.len();
            if dropped > 0 && table.save(Some(&path)).is_ok() {
                removed += dropped;
            }
        }
        removed
    })
}

/// The expected personas of a new record: the active agents now.
fn active_names(agents: &[fl_core::agent::AgentRecord]) -> Vec<String> {
    agents
        .iter()
        .filter(|a| a.status == AgentStatus::Active)
        .map(|a| a.name_key.to_ascii_lowercase())
        .collect()
}

/// One reconciliation pass with nothing remembered from an earlier one (see the module doc).
#[cfg(test)]
pub(crate) fn reconcile_pass(
    state: &HubState,
    host: &Host,
    panes: &dyn PaneSource,
) -> Result<Report, String> {
    reconcile_pass_with(state, host, panes, &mut Memory::default())
}

/// One reconciliation pass (see the module doc). An `Err` is a failure to open the store; the
/// caller logs it. A failed save of the record is logged and never aborts the pass.
pub(crate) fn reconcile_pass_with(
    state: &HubState,
    host: &Host,
    panes: &dyn PaneSource,
    memory: &mut Memory,
) -> Result<Report, String> {
    let home = state.config.home_override.as_deref();
    let (agents, hosted, mut epoch) = {
        let store = state.open_store().map_err(|e| e.to_string())?;
        crate::lifecycle::refresh_decommission_tombstones(&*store, home);
        let agents = store.list_agents(None, true).map_err(|e| e.to_string())?;
        let hosted = list_hosted_sessions(&*store).unwrap_or_default();
        (agents, hosted, epoch_state(&*store))
    };
    let active = |name: &str| {
        agents
            .iter()
            .any(|a| a.name_key.eq_ignore_ascii_case(name) && a.status == AgentStatus::Active)
    };
    let retired = |name: &str| {
        agents
            .iter()
            .any(|a| a.name_key.eq_ignore_ascii_case(name) && a.status == AgentStatus::Retired)
    };

    let saved_record = epoch.clone();
    // The boot record. Another boot_id than the saved one proves a reboot, whatever the uptime.
    // With no record the uptime is all there is to go on.
    let mut record = match (&epoch.take(), &host.boot_id) {
        (Some(saved), Some(id)) if saved.boot_id == *id => saved.clone(),
        (saved, Some(id)) => EpochRecord {
            boot_id: id.clone(),
            started_at_ms: stamp::now().unix_millis(),
            expected: if saved.is_some() || host.uptime.is_some_and(|u| u <= NOTICE_WINDOW) {
                active_names(&agents)
            } else {
                Vec::new()
            },
            ..EpochRecord::default()
        },
        (saved, None) => saved.clone().unwrap_or_default(),
    };
    if memory.boot_id != host.boot_id {
        memory.boot_id.clone_from(&host.boot_id);
        memory.notified.clear();
    }
    let mut report = Report::default();

    // 1. Register the routes the restored sessions can prove from their own identity.
    crate::discovery::scan_and_save_route_table(home);
    crate::discovery::invalidate_route_discovery_cache();

    // 2. Restored panes whose persona is not an active agent get no route.
    let listing = panes.panes();
    report.tmux_answered = listing.is_some();
    let mut pane_personas: Vec<String> = Vec::new();
    if let Some(panes) = listing {
        let binaries = vendor_binaries();
        for pane in panes.iter().filter(|p| runs_vendor_cli(p, &binaries)) {
            let PaneOwner::Persona(persona) = pane_owner(pane) else {
                continue;
            };
            if persona.as_deref().is_some_and(active) {
                pane_personas.extend(persona);
                continue;
            }
            // Only a RETIRED persona's routes are removed. The hub's agent registry is not the
            // agent bus's: a live agent that was never registered with the hub (a lead, the
            // workspace manager) looks unknown here, and its routes stay. An unknown persona's
            // pane is reported and nothing more.
            // TODO(#336 tombstones): a later route scan can find a retired persona's route again
            // from the running transcript; once the tombstones are on main, drop the route through
            // them so the scan cannot bring it back.
            let is_retired = persona.as_deref().is_some_and(retired);
            if is_retired && let Some(persona) = &persona {
                remove_routes(home, &[persona.as_str()], &[]);
                remove_routes_of_pane(home, pane);
            }
            let reason = if is_retired { "retired" } else { "unknown" };
            let orphan = OrphanPane {
                session: pane.session.clone(),
                pane_id: pane.pane_id.clone(),
                window: pane.window.clone(),
                persona,
                reason: reason.to_string(),
                command: pane.command.clone(),
            };
            if record.orphans.contains(&orphan) {
                continue;
            }
            eprintln!(
                "hub: restored tmux pane {} of session `{}` runs `{}` for a {} persona; it has no route and was left running",
                orphan.pane_id, orphan.session, orphan.command, orphan.reason
            );
            state.broadcast(
                "sessions",
                "session.restore_orphan",
                json!({
                    "boot_id": record.boot_id,
                    "session": orphan.session,
                    "window": orphan.window,
                    "pane_id": orphan.pane_id,
                    "persona": orphan.persona,
                    "reason": orphan.reason,
                    "command": orphan.command,
                }),
            );
            record.orphans.push(orphan.clone());
            report.orphans.push(orphan);
        }
    }

    // 3. One notice per expected, active agent with a live route, at most once per boot.
    if host.boot_id.is_some() {
        let skip = decommissioning(&hosted);
        let boot_ms = host
            .uptime
            .map(|u| stamp::now().unix_millis().saturating_sub(millis(u)));
        let routes = RouteTable::load(crate::discovery::route_table_path(home).as_deref());
        for name in record.expected.clone() {
            if record.notified.contains(&name)
                || memory.notified.contains(&name)
                || skip.contains(&name)
                || !active(&name)
            {
                continue;
            }
            let live = routes
                .get(&name)
                .is_some_and(|e| route_is_live(e, boot_ms, pane_personas.contains(&name)));
            if !live {
                report.pending.push(name);
                continue;
            }
            // Saved before the send: a crash or a failed save after it must not send again. A
            // name that cannot be saved is not sent; the next pass tries again.
            record.notified.push(name.clone());
            if let Err(e) = persist(state, &record, memory) {
                eprintln!("hub: the host-restart notice to `{name}` was held back: {e}");
                record.notified.retain(|n| *n != name);
                report.pending.push(name);
                continue;
            }
            // A store that cannot be opened is a failed send of this name only: the name is
            // rolled back below and the other agents are still processed.
            let sent = if memory.take_send_failure() {
                Err("injected send failure".to_string())
            } else {
                memory.open_for_send(state).and_then(|store| {
                    let body = notice_body_for_persona(&*store, &hosted, &name);
                    send_notice_with_body(&*store, &name, &body)
                })
            };
            match sent {
                Ok(()) => {
                    memory.notified.insert(name.clone());
                    report.notified.push(name);
                }
                Err(e) => {
                    eprintln!("hub: the host-restart notice to `{name}` was not sent: {e}");
                    record.notified.retain(|n| *n != name);
                    if let Err(e) = persist(state, &record, memory) {
                        eprintln!("hub: the notice to `{name}` stays recorded as sent: {e}");
                    }
                    report.pending.push(name);
                }
            }
        }
    }

    if host.boot_id.is_some()
        && saved_record.as_ref() != Some(&record)
        && let Err(e) = persist(state, &record, memory)
    {
        eprintln!("hub: the boot record was not saved: {e}");
    }
    let _ = crate::lifecycle::reconcile_and_respawn_hosted_sessions_in_hub(state);
    Ok(report)
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

fn persist(state: &HubState, record: &EpochRecord, memory: &Memory) -> Result<(), String> {
    #[cfg(test)]
    if memory
        .fail_save_notified
        .as_ref()
        .is_some_and(|n| record.notified.contains(n))
    {
        return Err("injected save failure".into());
    }
    let _ = memory;
    let store = state.open_store().map_err(|e| e.to_string())?;
    save_epoch(&*store, record)
}

/// What [`run`] works on: the host, the tmux to ask and the pacing.
pub(crate) struct Plan {
    /// `None`: the host as it is when the run starts ([`Host::current`]).
    pub(crate) host: Option<Host>,
    pub(crate) panes: std::sync::Arc<dyn PaneSource + Send + Sync>,
    pub(crate) interval: Duration,
    pub(crate) max_passes: u32,
    pub(crate) min_window: Duration,
}

impl Plan {
    /// The real host and its session supervisor, at the production pace.
    pub(crate) fn host() -> Self {
        Self {
            host: None,
            panes: std::sync::Arc::new(HostPanes),
            interval: RETRY_INTERVAL,
            max_passes: MAX_PASSES,
            min_window: MIN_WINDOW,
        }
    }
}

/// The hub's start-up reconciliation: a pass now, and further passes (see the module doc) while
/// an expected agent's route is not live, tmux has not answered, or [`MIN_WINDOW`] has not
/// passed. Never fails the hub; a failed pass is logged.
pub(crate) async fn run(state: HubState, plan: Plan) -> u32 {
    run_with(
        state,
        plan.host.unwrap_or_else(Host::current),
        plan.panes,
        plan.interval,
        plan.max_passes,
        plan.min_window,
    )
    .await
}

/// [`run`] with its inputs given; returns the number of passes made.
pub(crate) async fn run_with(
    state: HubState,
    host: Host,
    panes: std::sync::Arc<dyn PaneSource + Send + Sync>,
    interval: Duration,
    max_passes: u32,
    min_window: Duration,
) -> u32 {
    let started = Instant::now();
    let memory = std::sync::Arc::new(std::sync::Mutex::new(Memory::default()));
    let mut tmux_answered = false;
    let mut made = 0;
    while made < max_passes {
        made += 1;
        let (st, h, p, m) = (state.clone(), host.clone(), panes.clone(), memory.clone());
        let pass = tokio::task::spawn_blocking(move || {
            let mut memory = m.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            reconcile_pass_with(&st, &h, p.as_ref(), &mut memory)
        })
        .await;
        match pass {
            Ok(Ok(report)) => {
                tmux_answered |= report.tmux_answered;
                if report.pending.is_empty() && tmux_answered && started.elapsed() >= min_window {
                    return made;
                }
            }
            Ok(Err(e)) => eprintln!("hub: boot reconciliation pass {made} failed: {e}"),
            Err(e) => eprintln!("hub: boot reconciliation pass {made} stopped: {e}"),
        }
        if made < max_passes {
            tokio::time::sleep(interval).await;
        }
    }
    made
}
