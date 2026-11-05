//! Host reboot behaviour of the hub: reconcile the tmux panes a boot restore started with the
//! agent registry, and tell each active agent once per boot that the host restarted.
//!
//! One [`reconcile_pass`] does, in this order:
//!
//! 1. Scan the host and save the route table, so an active agent's route is registered from its
//!    own session identity (transcript first, see `fl_wire::identity::resolve_persona`).
//! 2. For every restored pane that runs a vendor CLI, take the persona from the pane's tmux
//!    session name (never from an inherited `AGENT_PERSONA`), or, in the shared Muse session,
//!    from the pane's window name. A persona that is not an active
//!    agent (retired, or not in the registry) gets no route: its route is dropped from the route
//!    table, and the pane is reported once as a `session.restore_orphan` event. The pane is not
//!    killed.
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
use std::path::Path;
use std::time::{Duration, Instant};

use fl_core::agent::AgentStatus;
use fl_core::ferrowire::{MessageKind, MessageUrgency, SendWireArgs, WireAddress};
use fl_core::state::{
    MergeMode, RuntimeEntry, SensitivityPatch, StateConcern, StatePayload, StateScope,
    UpsertStateArgs,
};
use fl_core::store::{AgentRegistry, MessageStore, StateStore};
use fl_core::{VendorRegistry, VendorTransport};
use fl_exec::stamp;
use fl_wire::identity::{RouteEntry, RouteTable, normalize_persona};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::HubState;
use crate::lifecycle::{
    HostedSessionRecord, MUSE_SESSION, STATUS_DECOMMISSIONING, list_hosted_sessions, remove_routes,
    tmux_stdout, tmux_stdout_of, undeliverable,
};

/// Runtime state entry (workspace scope) that holds the boot the hub last reconciled.
pub(crate) const EPOCH_SLUG: &str = "host-boot-epoch";
/// The subject of the notice.
pub(crate) const NOTICE_SUBJECT: &str = "Host restarted";
/// The body of the notice.
pub(crate) const NOTICE_BODY: &str = "The host restarted. Resume your task from your last handoff (`fl state handoff get ...`) and reply to confirm.";
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
}

pub(crate) fn vendor_binaries() -> Vec<String> {
    VendorRegistry::global()
        .list()
        .into_iter()
        .filter_map(|v| match v.transport {
            VendorTransport::SubscriptionCli { default_binary } => Path::new(&default_binary)
                .file_name()
                .map(|n| n.to_string_lossy().to_ascii_lowercase()),
            VendorTransport::HttpApi { .. } => None,
        })
        .collect()
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

fn send_notice<S: MessageStore + AgentRegistry>(store: &S, persona: &str) -> Result<(), String> {
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
            body: NOTICE_BODY.into(),
            reply_to: None,
            thread_id: None,
            deadline_at: None,
            at: stamp::now(),
        })
        .map(|_| ())
        .map_err(|e| e.to_string())
}

impl Memory {
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
            if let Some(persona) = &persona {
                remove_routes(home, &[persona.as_str()], &[]);
            }
            let reason = if persona.as_deref().is_some_and(retired) {
                "retired"
            } else {
                "unknown"
            };
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
            let sent = if memory.take_send_failure() {
                Err("injected send failure".to_string())
            } else {
                let store = state.open_store().map_err(|e| e.to_string())?;
                send_notice(&*store, &name)
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
    /// The real host and its tmux, at the production pace.
    pub(crate) fn host() -> Self {
        Self {
            host: None,
            panes: std::sync::Arc::new(TmuxPanes::default()),
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
