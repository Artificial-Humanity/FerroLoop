//! Host reboot behaviour: restored panes of retired or unknown personas get no route and are
//! reported; each active agent with a live route is told once per boot. Every test uses its own
//! store and home with a fake pane source; no real tmux, route table or resurrect directory is
//! read.

use std::path::PathBuf;
use std::time::Duration;

use fl_core::At;
use fl_core::agent::{AgentRole, RegisterAgentArgs};
use fl_core::ferrowire::WireFilter;
use fl_core::store::{AgentRegistry, MessageStore, StateStore};
use fl_exec::stamp;
use fl_wire::identity::{RouteTable, VendorRoute};
use tempfile::TempDir;

use crate::boot_reconcile::{
    EPOCH_SLUG, Host, NOTICE_BODY, NOTICE_SUBJECT, NOTICE_WINDOW, Pane, PaneSource, parse_panes,
    reconcile_pass, runs_vendor_cli, vendor_binaries,
};
use crate::{HubServerConfig, HubState};

#[derive(Clone)]
struct FakePanes(Option<Vec<Pane>>);

impl PaneSource for FakePanes {
    fn panes(&self) -> Option<Vec<Pane>> {
        self.0.clone()
    }
}

fn pane(session: &str, pane_id: &str, command: &str) -> Pane {
    Pane {
        session: session.into(),
        pane_id: pane_id.into(),
        command: command.into(),
        start_command: format!("{command} --resume x"),
    }
}

fn no_panes() -> FakePanes {
    FakePanes(Some(Vec::new()))
}

fn host(boot_id: &str) -> Host {
    Host {
        boot_id: Some(boot_id.into()),
        uptime: Some(Duration::from_secs(90)),
    }
}

struct Fixture {
    tmp: TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self {
            tmp: TempDir::new().unwrap(),
        }
    }

    fn home(&self) -> PathBuf {
        self.tmp.path().join("home")
    }

    /// A hub state on this fixture's store and home. Opening it again is a hub restart.
    fn state(&self) -> HubState {
        std::fs::create_dir_all(self.home()).unwrap();
        HubState::new(HubServerConfig {
            store_path: self.tmp.path().join("store.redb"),
            repo_root: self.tmp.path().to_path_buf(),
            home_override: Some(self.home()),
            ..HubServerConfig::default()
        })
    }

    fn agent(&self, state: &HubState, name: &str, retire: bool) {
        let store = state.open_store().unwrap();
        let rec = store
            .register_agent(RegisterAgentArgs {
                display_name: name.into(),
                email: None,
                project: "ferroloop".into(),
                role: AgentRole::Assistant,
                extra_areas: vec![],
                create_area: true,
                replace: true,
                at: stamp::now(),
                by: "owner".into(),
            })
            .unwrap();
        if retire {
            store.retire_agent(&rec.id, stamp::now(), "owner").unwrap();
        }
    }

    fn routes_path(&self) -> PathBuf {
        self.home().join(".cache/agent-bus/routes.json")
    }

    /// A webhook route for `persona`; live while its socket file exists.
    fn route(&self, persona: &str, live: bool) -> PathBuf {
        let socket = self.home().join(format!("{persona}.sock"));
        if live {
            std::fs::write(&socket, "").unwrap();
        }
        let mut table = RouteTable::load(Some(&self.routes_path()));
        table.upsert(
            persona,
            VendorRoute::Webhook {
                socket_path: socket.clone(),
            },
            At::from_unix_millis(1),
        );
        table.save(Some(&self.routes_path())).unwrap();
        socket
    }

    fn has_route(&self, persona: &str) -> bool {
        RouteTable::load(Some(&self.routes_path()))
            .get(persona)
            .is_some()
    }
}

fn inbox(state: &HubState, who: &str) -> Vec<(String, String, String)> {
    let store = state.open_store().unwrap();
    store
        .list_wire_inbox(&WireFilter {
            recipient: Some(who.into()),
            ..WireFilter::default()
        })
        .unwrap()
        .into_iter()
        .map(|i| (i.message.sender_name, i.message.subject, i.message.body))
        .collect()
}

fn drain(rx: &mut tokio::sync::broadcast::Receiver<crate::HubWsFrame>) -> Vec<crate::HubWsFrame> {
    let mut out = Vec::new();
    while let Ok(frame) = rx.try_recv() {
        out.push(frame);
    }
    out
}

#[test]
fn a_retired_or_unknown_pane_gets_no_route_and_is_reported_once() {
    let fx = Fixture::new();
    let state = fx.state();
    fx.agent(&state, "Freya", false);
    fx.agent(&state, "Fiona", true);
    fx.route("fiona", true);
    fx.route("freya", true);
    let mut rx = state.subscribe();
    let panes = FakePanes(Some(vec![
        pane("Freya", "%1", "codex"),
        pane("Fiona", "%6", "codex"),
        pane("Stranger", "%7", "claude"),
        pane("0", "%0", "zsh"),
    ]));

    let report = reconcile_pass(&state, &host("boot-a"), &panes).unwrap();

    let reasons: Vec<_> = report
        .orphans
        .iter()
        .map(|o| (o.session.as_str(), o.reason.as_str()))
        .collect();
    assert_eq!(reasons, [("Fiona", "retired"), ("Stranger", "unknown")]);
    assert!(
        !fx.has_route("fiona"),
        "the retired persona's route is dropped"
    );
    assert!(fx.has_route("freya"), "the active persona keeps its route");
    let events: Vec<_> = drain(&mut rx)
        .into_iter()
        .filter(|f| f.event == "session.restore_orphan")
        .collect();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].channel, "sessions");
    assert_eq!(events[0].data["session"], "Fiona");
    assert_eq!(events[0].data["pane_id"], "%6");
    assert_eq!(events[0].data["reason"], "retired");

    // The same pane is not reported again, by the next pass or by a restarted hub.
    assert!(
        reconcile_pass(&state, &host("boot-a"), &panes)
            .unwrap()
            .orphans
            .is_empty()
    );
    assert!(
        reconcile_pass(&fx.state(), &host("boot-a"), &panes)
            .unwrap()
            .orphans
            .is_empty()
    );
    assert!(
        drain(&mut rx)
            .iter()
            .all(|f| f.event != "session.restore_orphan")
    );
}

#[test]
fn a_retired_persona_is_never_sent_the_notice_and_its_pane_is_not_killed() {
    let fx = Fixture::new();
    let state = fx.state();
    fx.agent(&state, "Fiona", true);
    fx.route("fiona", true);
    let panes = FakePanes(Some(vec![pane("Fiona", "%6", "codex")]));
    // A retired persona with a live route and no pane is not told either.
    fx.agent(&state, "Gone", true);
    fx.route("gone", true);
    reconcile_pass(&state, &host("boot-a"), &panes).unwrap();
    assert!(inbox(&state, "fiona").is_empty());
    assert!(inbox(&state, "gone").is_empty());
}

#[test]
fn an_active_agent_with_a_live_route_is_told_once_per_boot() {
    let fx = Fixture::new();
    let state = fx.state();
    fx.agent(&state, "Freya", false);
    fx.route("freya", true);

    let first = reconcile_pass(&state, &host("boot-a"), &no_panes()).unwrap();
    assert_eq!(first.notified, ["freya"]);
    let sent = inbox(&state, "freya");
    assert_eq!(
        sent,
        [(
            "system".to_string(),
            NOTICE_SUBJECT.to_string(),
            NOTICE_BODY.to_string()
        )]
    );

    // Same boot: another pass, and a hub restart, send nothing.
    assert!(
        reconcile_pass(&state, &host("boot-a"), &no_panes())
            .unwrap()
            .notified
            .is_empty()
    );
    let restarted = fx.state();
    assert!(
        reconcile_pass(&restarted, &host("boot-a"), &no_panes())
            .unwrap()
            .notified
            .is_empty()
    );
    assert_eq!(inbox(&restarted, "freya").len(), 1);

    // A new boot sends again, once.
    let again = reconcile_pass(&restarted, &host("boot-b"), &no_panes()).unwrap();
    assert_eq!(again.notified, ["freya"]);
    assert!(
        reconcile_pass(&restarted, &host("boot-b"), &no_panes())
            .unwrap()
            .notified
            .is_empty()
    );
    assert_eq!(inbox(&restarted, "freya").len(), 2);
}

#[test]
fn an_agent_whose_route_is_not_live_yet_is_told_when_it_comes_up() {
    let fx = Fixture::new();
    let state = fx.state();
    fx.agent(&state, "Freya", false);
    fx.agent(&state, "Ferris", false);
    fx.route("ferris", true);
    fx.route("freya", false);

    let first = reconcile_pass(&state, &host("boot-a"), &no_panes()).unwrap();
    assert_eq!(first.notified, ["ferris"]);
    assert_eq!(first.pending, ["freya"]);
    assert!(inbox(&state, "freya").is_empty());

    fx.route("freya", true);
    let second = reconcile_pass(&state, &host("boot-a"), &no_panes()).unwrap();
    assert_eq!(second.notified, ["freya"]);
    assert!(second.pending.is_empty());
    assert_eq!(inbox(&state, "ferris").len(), 1, "ferris is not told twice");
}

#[test]
fn an_agent_with_no_route_at_all_is_not_told() {
    let fx = Fixture::new();
    let state = fx.state();
    fx.agent(&state, "Freya", false);
    let report = reconcile_pass(&state, &host("boot-a"), &no_panes()).unwrap();
    assert!(report.notified.is_empty());
    assert_eq!(report.pending, ["freya"]);
}

#[test]
fn no_notice_goes_out_when_the_boot_is_unknown_or_old() {
    let fx = Fixture::new();
    let state = fx.state();
    fx.agent(&state, "Freya", false);
    fx.route("freya", true);
    let old = Host {
        boot_id: Some("boot-a".into()),
        uptime: Some(NOTICE_WINDOW + Duration::from_secs(1)),
    };
    let unknown = Host {
        boot_id: None,
        uptime: Some(Duration::from_secs(5)),
    };
    let no_uptime = Host {
        boot_id: Some("boot-a".into()),
        uptime: None,
    };
    for h in [old, unknown, no_uptime] {
        let report = reconcile_pass(&state, &h, &no_panes()).unwrap();
        assert!(
            report.notified.is_empty() && report.pending.is_empty(),
            "{h:?}"
        );
    }
    assert!(inbox(&state, "freya").is_empty());
}

#[test]
fn a_pane_listing_that_tmux_would_not_give_changes_nothing() {
    let fx = Fixture::new();
    let state = fx.state();
    fx.agent(&state, "Fiona", true);
    fx.route("fiona", true);
    let report = reconcile_pass(&state, &host("boot-a"), &FakePanes(None)).unwrap();
    assert!(report.orphans.is_empty());
    assert!(fx.has_route("fiona"), "an unknown pane list drops no route");
}

#[test]
fn the_boot_record_is_a_runtime_state_entry() {
    let fx = Fixture::new();
    let state = fx.state();
    fx.agent(&state, "Freya", false);
    fx.route("freya", true);
    reconcile_pass(&state, &host("boot-a"), &no_panes()).unwrap();
    let store = state.open_store().unwrap();
    let entry = store
        .get_state_by_key(
            &fl_core::state::StateScope::Workspace,
            fl_core::state::StateConcern::Runtime,
            EPOCH_SLUG,
        )
        .unwrap()
        .expect("record");
    let fl_core::state::StatePayload::Runtime(rt) = entry.payload else {
        panic!("not a runtime entry");
    };
    assert_eq!(rt.value["boot_id"], "boot-a");
    assert_eq!(rt.value["notified"], serde_json::json!(["freya"]));
}

#[test]
fn tmux_pane_lines_parse_and_only_vendor_cli_panes_count() {
    let panes = parse_panes(
        "Freya\t%1\tcodex\tcodex resume 01a --yolo\n0\t%0\tzsh\t\n\t%9\tx\ty\nFeira\t%2\tnode\t/usr/bin/env X=1 claude --resume a\n",
    );
    assert_eq!(panes.len(), 3, "a line with no session name is skipped");
    assert_eq!(panes[0].start_command, "codex resume 01a --yolo");
    let binaries = vendor_binaries();
    let runs: Vec<_> = panes
        .iter()
        .map(|p| runs_vendor_cli(p, &binaries))
        .collect();
    assert_eq!(runs, [true, false, true]);
}

#[tokio::test]
async fn the_start_up_passes_are_bounded_and_stop_when_nothing_is_owed() {
    use crate::boot_reconcile::run_with;
    use std::sync::Arc;
    let fx = Fixture::new();
    let state = fx.state();
    fx.agent(&state, "Freya", false);
    fx.route("freya", false);
    let panes: Arc<dyn PaneSource + Send + Sync> = Arc::new(no_panes());
    let tick = Duration::from_millis(1);

    // A route that never comes up is waited on for the pass limit only.
    let passes = run_with(state.clone(), host("boot-a"), panes.clone(), tick, 3).await;
    assert_eq!(passes, 3);
    assert!(inbox(&state, "freya").is_empty());

    // Once it is up, one pass tells the agent and the loop ends.
    fx.route("freya", true);
    let passes = run_with(state.clone(), host("boot-a"), panes, tick, 3).await;
    assert_eq!(passes, 1);
    assert_eq!(inbox(&state, "freya").len(), 1);
}
