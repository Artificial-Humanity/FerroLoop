//! Host reboot behaviour of the hub: reconcile the tmux panes a boot restore started with the
//! agent registry, and tell each active agent once per boot that the host restarted.
//!
//! One [`reconcile_pass`] does, in this order:
//!
//! 1. Scan the host and save the route table, so an active agent's route is registered from its
//!    own session identity (transcript first, see `fl_wire::identity::resolve_persona`).
//! 2. For every restored pane that runs a vendor CLI, take the persona from the pane's tmux
//!    session name (never from an inherited `AGENT_PERSONA`). A persona that is not an active
//!    agent (retired, or not in the registry) gets no route: its route is dropped from the route
//!    table, and the pane is reported once as a `session.restore_orphan` event. The pane is not
//!    killed.
//! 3. Send each active agent whose route is live one `system` message, "Host restarted", once per
//!    boot. The boot is the host's `boot_id`; the agents already told are kept in a runtime state
//!    entry ([`EPOCH_SLUG`]), so a hub restart in the same boot does not send again and a new
//!    boot does. An agent without a live route is retried by the next pass; [`run`] makes at
//!    most [`MAX_PASSES`] passes. A notice goes out only while the host is younger than
//!    [`NOTICE_WINDOW`], so a hub that starts long after a boot, or the first start after this
//!    rule shipped, tells nobody.

use std::path::Path;
use std::time::Duration;

use fl_core::agent::AgentStatus;
use fl_core::ferrowire::{MessageKind, MessageUrgency, SendWireArgs, WireAddress};
use fl_core::state::{
    MergeMode, RuntimeEntry, SensitivityPatch, StateConcern, StatePayload, StateScope,
    UpsertStateArgs,
};
use fl_core::store::{AgentRegistry, MessageStore, StateStore};
use fl_core::{VendorRegistry, VendorTransport};
use fl_exec::stamp;
use fl_wire::identity::normalize_persona;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::HubState;
use crate::lifecycle::{
    HostedSessionRecord, RouteQuery, STATUS_DECOMMISSIONING, find_route, list_hosted_sessions,
    remove_routes, tmux_stdout, undeliverable,
};

/// Runtime state entry (workspace scope) that holds the boot the hub last reconciled.
pub(crate) const EPOCH_SLUG: &str = "host-boot-epoch";
/// The subject of the notice.
pub(crate) const NOTICE_SUBJECT: &str = "Host restarted";
/// The body of the notice.
pub(crate) const NOTICE_BODY: &str = "The host restarted. Resume your task from your last handoff (`fl state handoff get ...`) and reply to confirm.";
/// The sender of the notice, as for the hub's other system messages.
const NOTICE_SENDER: &str = "system";
/// A notice goes out only while the host has been up no longer than this.
pub(crate) const NOTICE_WINDOW: Duration = Duration::from_secs(60 * 60);
/// Time between the passes of [`run`].
pub(crate) const RETRY_INTERVAL: Duration = Duration::from_secs(15);
/// Most passes [`run`] makes, so a route that never comes up is not waited on for ever.
pub(crate) const MAX_PASSES: u32 = 20;

/// A tmux pane the host runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Pane {
    pub(crate) session: String,
    pub(crate) pane_id: String,
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

/// The host's tmux (`FL_WIRE_TMUX_BIN`, else `tmux`).
pub(crate) struct TmuxPanes;

impl PaneSource for TmuxPanes {
    fn panes(&self) -> Option<Vec<Pane>> {
        let out = tmux_stdout(&[
            "list-panes",
            "-a",
            "-F",
            "#{session_name}\t#{pane_id}\t#{pane_current_command}\t#{pane_start_command}",
        ])?;
        Some(parse_panes(&out))
    }
}

pub(crate) fn parse_panes(text: &str) -> Vec<Pane> {
    text.lines()
        .filter_map(|line| {
            let mut f = line.splitn(4, '\t');
            let session = f.next().filter(|s| !s.is_empty())?;
            Some(Pane {
                session: session.to_string(),
                pane_id: f.next().unwrap_or_default().to_string(),
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
    /// The persona the session name stands for, when it is a valid one.
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
    /// Active personas still owed the notice (no live route yet).
    pub(crate) pending: Vec<String>,
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

/// One reconciliation pass (see the module doc). An `Err` is a store failure; the caller logs it.
pub(crate) fn reconcile_pass(
    state: &HubState,
    host: &Host,
    panes: &dyn PaneSource,
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

    // A new boot starts a new record: nobody has been told yet, nothing reported yet.
    let mut record = match (&epoch.take(), &host.boot_id) {
        (Some(saved), Some(id)) if saved.boot_id == *id => saved.clone(),
        (_, Some(id)) => EpochRecord {
            boot_id: id.clone(),
            started_at_ms: stamp::now().unix_millis(),
            ..EpochRecord::default()
        },
        (saved, None) => saved.clone().unwrap_or_default(),
    };
    let before = record.clone();
    let mut report = Report::default();

    // 1. Register the routes the restored sessions can prove from their own identity.
    crate::discovery::scan_and_save_route_table(home);
    crate::discovery::invalidate_route_discovery_cache();

    // 2. Restored panes whose persona is not an active agent get no route.
    if let Some(panes) = panes.panes() {
        let binaries = vendor_binaries();
        for pane in panes.iter().filter(|p| runs_vendor_cli(p, &binaries)) {
            let persona = normalize_persona(&pane.session);
            if persona.as_deref().is_some_and(&active) {
                continue;
            }
            if let Some(persona) = &persona {
                remove_routes(home, &[persona.as_str()], &[]);
            }
            let reason = if persona.as_deref().is_some_and(&retired) {
                "retired"
            } else {
                "unknown"
            };
            let orphan = OrphanPane {
                session: pane.session.clone(),
                pane_id: pane.pane_id.clone(),
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

    // 3. One notice per active agent with a live route, once per boot.
    let notice_open = host.boot_id.is_some() && host.uptime.is_some_and(|u| u <= NOTICE_WINDOW);
    if notice_open {
        let skip = decommissioning(&hosted);
        for agent in agents.iter().filter(|a| a.status == AgentStatus::Active) {
            let name = agent.name_key.to_ascii_lowercase();
            if record.notified.contains(&name) || skip.contains(&name) {
                continue;
            }
            let live = find_route(home, RouteQuery::persona(&name, &name))
                .is_some_and(|route| undeliverable(&route).is_none());
            if !live {
                report.pending.push(name);
                continue;
            }
            let sent = {
                let store = state.open_store().map_err(|e| e.to_string())?;
                send_notice(&*store, &name)
            };
            match sent {
                Ok(()) => {
                    record.notified.push(name.clone());
                    // Saved at once: a hub that dies after this send must not send it again.
                    persist(state, &record)?;
                    report.notified.push(name);
                }
                Err(e) => {
                    eprintln!("hub: the host-restart notice to `{name}` was not sent: {e}");
                    report.pending.push(name);
                }
            }
        }
    }

    if record != before {
        persist(state, &record)?;
    }
    Ok(report)
}

fn persist(state: &HubState, record: &EpochRecord) -> Result<(), String> {
    let store = state.open_store().map_err(|e| e.to_string())?;
    save_epoch(&*store, record)
}

/// The hub's start-up reconciliation: a pass now, and a bounded number of further passes while
/// an active agent's route is not live yet. Never fails the hub; a failed pass is logged.
pub(crate) async fn run(state: HubState) {
    run_with(
        state,
        Host::current(),
        std::sync::Arc::new(TmuxPanes),
        RETRY_INTERVAL,
        MAX_PASSES,
    )
    .await;
}

/// [`run`] with its inputs given; returns the number of passes made.
pub(crate) async fn run_with(
    state: HubState,
    host: Host,
    panes: std::sync::Arc<dyn PaneSource + Send + Sync>,
    interval: Duration,
    max_passes: u32,
) -> u32 {
    let mut made = 0;
    while made < max_passes {
        made += 1;
        let (st, h, p) = (state.clone(), host.clone(), panes.clone());
        match tokio::task::spawn_blocking(move || reconcile_pass(&st, &h, p.as_ref())).await {
            Ok(Ok(report)) if report.pending.is_empty() => return made,
            Ok(Ok(_)) => {}
            Ok(Err(e)) => eprintln!("hub: boot reconciliation pass {made} failed: {e}"),
            Err(e) => eprintln!("hub: boot reconciliation pass {made} stopped: {e}"),
        }
        if made < max_passes {
            tokio::time::sleep(interval).await;
        }
    }
    made
}
