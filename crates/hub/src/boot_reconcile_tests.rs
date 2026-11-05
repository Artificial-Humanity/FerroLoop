//! Host reboot behaviour: restored panes of retired or unknown personas get no route and are
//! reported; each expected active agent with a live route is told once per boot. Every test uses
//! its own store and home with a fake pane source (or a fake tmux binary it passes in); no real
//! tmux, route table or resurrect directory is read, and no environment variable is set.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use fl_core::At;
use fl_core::agent::{AgentRole, RegisterAgentArgs};
use fl_core::ferrowire::WireFilter;
use fl_core::store::{AgentRegistry, MessageStore, StateStore};
use fl_exec::stamp;
use fl_wire::identity::{RouteTable, VendorRoute};
use tempfile::TempDir;

use crate::boot_reconcile::{
    EPOCH_SLUG, Host, Memory, NOTICE_BODY, NOTICE_SUBJECT, NOTICE_WINDOW, PANE_FORMAT, Pane,
    PaneSource, Plan, TmuxPanes, parse_panes, reconcile_pass, reconcile_pass_with, run_with,
    runs_vendor_cli, vendor_binaries,
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
        window: command.into(),
        command: command.into(),
        start_command: format!("{command} --resume x"),
    }
}

/// A pane of the shared Muse session: the window is named by the persona.
fn muse_pane(window: &str, pane_id: &str) -> Pane {
    Pane {
        window: window.into(),
        ..pane("muse", pane_id, "muse")
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

    /// A webhook route for `persona`, found just now; deliverable while its socket file exists.
    fn route(&self, persona: &str, live: bool) -> PathBuf {
        let socket = self.home().join(format!("{persona}.sock"));
        if live {
            std::fs::write(&socket, "").unwrap();
        }
        self.put_route(
            persona,
            VendorRoute::Webhook {
                socket_path: socket.clone(),
            },
            stamp::now(),
        );
        socket
    }

    /// A Codex route saved at `at`. Nothing checks a Codex route, so it is "live" to any check
    /// that only looks at the route.
    fn codex_route(&self, persona: &str, at: At) {
        self.put_route(
            persona,
            VendorRoute::Codex {
                thread_id: format!("thread-{persona}"),
                cwd: None,
            },
            at,
        );
    }

    fn put_route(&self, persona: &str, route: VendorRoute, at: At) {
        let mut table = RouteTable::load(Some(&self.routes_path()));
        table.upsert(persona, route, at);
        table.save(Some(&self.routes_path())).unwrap();
    }

    fn epoch(&self, state: &HubState) -> serde_json::Value {
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
        rt.value
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

fn old() -> At {
    At::from_unix_millis(1)
}

fn hours(h: u64) -> Duration {
    Duration::from_secs(h * 3600)
}

fn host_up(boot_id: &str, uptime: Duration) -> Host {
    Host {
        boot_id: Some(boot_id.into()),
        uptime: Some(uptime),
    }
}

#[test]
fn a_retired_persona_is_never_sent_the_notice() {
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
fn a_retired_persona_that_is_registered_active_again_is_not_an_orphan_and_is_told() {
    let fx = Fixture::new();
    let state = fx.state();
    fx.agent(&state, "Fiona", true);
    fx.agent(&state, "Fiona", false);
    fx.route("fiona", true);
    let panes = FakePanes(Some(vec![pane("Fiona", "%6", "codex")]));

    let report = reconcile_pass(&state, &host("boot-a"), &panes).unwrap();

    assert!(report.orphans.is_empty(), "{:?}", report.orphans);
    assert!(fx.has_route("fiona"), "an active persona keeps its route");
    assert_eq!(report.notified, ["fiona"]);
    assert_eq!(inbox(&state, "fiona").len(), 1);
}

#[test]
fn a_route_keyed_to_another_persona_is_not_a_route_for_this_one() {
    let fx = Fixture::new();
    let state = fx.state();
    fx.agent(&state, "Freya", false);
    fx.agent(&state, "Ferris", false);
    // Only Ferris has a route; the pane of Freya is not enough on its own either.
    fx.route("ferris", true);

    let report = reconcile_pass(&state, &host("boot-a"), &no_panes()).unwrap();

    assert_eq!(report.notified, ["ferris"]);
    assert_eq!(report.pending, ["freya"]);
    assert!(
        inbox(&state, "freya").is_empty(),
        "freya is not told on ferris's route"
    );
    assert_eq!(inbox(&state, "ferris").len(), 1);

    let panes = FakePanes(Some(vec![pane("Freya", "%1", "codex")]));
    let again = reconcile_pass(&state, &host("boot-a"), &panes).unwrap();
    assert!(
        again.notified.is_empty(),
        "a pane without a route of its own sends nothing"
    );
    assert_eq!(again.pending, ["freya"]);
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

// F1: a route saved before the reboot proves nothing.

#[test]
fn a_route_saved_before_the_boot_and_no_pane_is_not_live() {
    let fx = Fixture::new();
    let state = fx.state();
    fx.agent(&state, "Freya", false);
    fx.codex_route("freya", old());

    let report = reconcile_pass(&state, &host("boot-a"), &no_panes()).unwrap();

    assert!(report.notified.is_empty());
    assert_eq!(report.pending, ["freya"]);
    assert!(inbox(&state, "freya").is_empty());
}

#[test]
fn a_route_found_after_the_boot_is_live() {
    let fx = Fixture::new();
    let state = fx.state();
    fx.agent(&state, "Freya", false);
    fx.codex_route("freya", stamp::now());
    let report = reconcile_pass(&state, &host("boot-a"), &no_panes()).unwrap();
    assert_eq!(report.notified, ["freya"]);

    // The boundary: a route stamped before the boot is not, one stamped after it is.
    let fx = Fixture::new();
    let state = fx.state();
    fx.agent(&state, "Freya", false);
    let up = Duration::from_secs(600);
    let before_boot = At::from_unix_millis(stamp::now().unix_millis() - 3_600_000);
    fx.codex_route("freya", before_boot);
    let report = reconcile_pass(&state, &host_up("boot-a", up), &no_panes()).unwrap();
    assert!(
        report.notified.is_empty(),
        "stamped an hour ago, booted 10 min ago"
    );
    let after_boot = At::from_unix_millis(stamp::now().unix_millis() - 300_000);
    fx.codex_route("freya", after_boot);
    let report = reconcile_pass(&state, &host_up("boot-a", up), &no_panes()).unwrap();
    assert_eq!(
        report.notified,
        ["freya"],
        "stamped 5 min ago, booted 10 min ago"
    );
}

#[test]
fn a_restored_pane_of_the_persona_makes_an_old_route_live() {
    let fx = Fixture::new();
    let state = fx.state();
    fx.agent(&state, "Freya", false);
    fx.codex_route("freya", old());
    // A pane of another persona, and a pane that is not a vendor CLI, count for nothing.
    let others = FakePanes(Some(vec![
        pane("Ferris", "%2", "codex"),
        pane("Freya", "%3", "zsh"),
    ]));
    let report = reconcile_pass(&state, &host("boot-a"), &others).unwrap();
    assert!(report.notified.is_empty(), "{report:?}");

    let panes = FakePanes(Some(vec![pane("Freya", "%1", "codex")]));
    let report = reconcile_pass(&state, &host("boot-a"), &panes).unwrap();
    assert_eq!(report.notified, ["freya"]);
}

#[test]
fn a_claude_route_with_a_reused_pid_is_not_live_unless_it_is_new() {
    let fx = Fixture::new();
    let state = fx.state();
    fx.agent(&state, "Freya", false);
    // This test process's pid exists, as a pid reused after a reboot does.
    let claude = VendorRoute::Claude {
        pid: std::process::id(),
        session_id: "s".into(),
        socket_path: fx.home().join("c.sock"),
        key_path: None,
    };
    fx.put_route("freya", claude.clone(), old());
    let report = reconcile_pass(&state, &host("boot-a"), &no_panes()).unwrap();
    assert!(report.notified.is_empty());
    fx.put_route("freya", claude, stamp::now());
    let report = reconcile_pass(&state, &host("boot-a"), &no_panes()).unwrap();
    assert_eq!(report.notified, ["freya"]);
}

// F2: at most once.

#[test]
fn a_notice_whose_name_cannot_be_saved_is_not_sent_and_the_others_are() {
    let fx = Fixture::new();
    let state = fx.state();
    fx.agent(&state, "Freya", false);
    fx.agent(&state, "Ferris", false);
    fx.route("freya", true);
    fx.route("ferris", true);
    let mut memory = Memory {
        fail_save_notified: Some("freya".into()),
        ..Memory::default()
    };
    let panes = no_panes();

    for pass in 0..3 {
        let report = reconcile_pass_with(&state, &host("boot-a"), &panes, &mut memory).unwrap();
        assert_eq!(report.pending, ["freya"], "pass {pass}");
    }
    assert!(inbox(&state, "freya").is_empty(), "never sent unsaved");
    assert_eq!(
        inbox(&state, "ferris").len(),
        1,
        "the other agent was processed, once, across the passes"
    );

    memory.fail_save_notified = None;
    let report = reconcile_pass_with(&state, &host("boot-a"), &panes, &mut memory).unwrap();
    assert_eq!(report.notified, ["freya"]);
    reconcile_pass_with(&state, &host("boot-a"), &panes, &mut memory).unwrap();
    assert_eq!(inbox(&state, "freya").len(), 1);
}

#[test]
fn a_notice_that_was_sent_is_not_sent_again_by_this_process_even_if_the_record_is_lost() {
    let fx = Fixture::new();
    let state = fx.state();
    fx.agent(&state, "Freya", false);
    fx.route("freya", true);
    let mut memory = Memory::default();
    let first = reconcile_pass_with(&state, &host("boot-a"), &no_panes(), &mut memory).unwrap();
    assert_eq!(first.notified, ["freya"]);

    // The store no longer has the record (here: another store), the memory still has the name.
    let other = Fixture::new();
    let lost = other.state();
    other.agent(&lost, "Freya", false);
    other.route("freya", true);
    let again = reconcile_pass_with(&lost, &host("boot-a"), &no_panes(), &mut memory).unwrap();
    assert!(again.notified.is_empty());
    assert!(inbox(&lost, "freya").is_empty());
}

#[test]
fn a_notice_that_fails_to_send_is_rolled_back_and_retried() {
    let fx = Fixture::new();
    let state = fx.state();
    fx.agent(&state, "Freya", false);
    fx.route("freya", true);
    let mut memory = Memory {
        fail_sends: 1,
        ..Memory::default()
    };

    let first = reconcile_pass_with(&state, &host("boot-a"), &no_panes(), &mut memory).unwrap();
    assert!(first.notified.is_empty());
    assert_eq!(first.pending, ["freya"]);
    assert_eq!(
        fx.epoch(&state)["notified"],
        serde_json::json!([]),
        "the rollback is saved"
    );
    assert!(inbox(&state, "freya").is_empty());

    let second = reconcile_pass_with(&state, &host("boot-a"), &no_panes(), &mut memory).unwrap();
    assert_eq!(second.notified, ["freya"]);
    assert_eq!(fx.epoch(&state)["notified"], serde_json::json!(["freya"]));
    assert_eq!(inbox(&state, "freya").len(), 1);
}

// F3: who is expected.

#[test]
fn with_no_record_the_uptime_decides_and_the_window_boundary_is_exact() {
    for (uptime, told) in [
        (NOTICE_WINDOW, true),
        (NOTICE_WINDOW - Duration::from_secs(1), true),
        (NOTICE_WINDOW + Duration::from_secs(1), false),
    ] {
        let fx = Fixture::new();
        let state = fx.state();
        fx.agent(&state, "Freya", false);
        fx.route("freya", true);
        let report = reconcile_pass(&state, &host_up("boot-a", uptime), &no_panes()).unwrap();
        assert_eq!(!report.notified.is_empty(), told, "{uptime:?}");
        assert!(report.pending.is_empty());
        assert_eq!(
            inbox(&state, "freya").len(),
            usize::from(told),
            "{uptime:?}"
        );
        assert_eq!(
            fx.epoch(&state)["expected"],
            serde_json::json!(if told { vec!["freya"] } else { vec![] })
        );
    }
}

#[test]
fn an_unknown_boot_or_uptime_with_no_record_tells_nobody() {
    let fx = Fixture::new();
    let state = fx.state();
    fx.agent(&state, "Freya", false);
    fx.route("freya", true);
    let unknown = Host {
        boot_id: None,
        uptime: Some(Duration::from_secs(5)),
    };
    let no_uptime = Host {
        boot_id: Some("boot-a".into()),
        uptime: None,
    };
    for h in [unknown, no_uptime] {
        let report = reconcile_pass(&state, &h, &no_panes()).unwrap();
        assert!(
            report.notified.is_empty() && report.pending.is_empty(),
            "{h:?}"
        );
    }
    assert!(inbox(&state, "freya").is_empty());
}

#[test]
fn another_boot_id_proves_a_reboot_whatever_the_uptime() {
    let fx = Fixture::new();
    let state = fx.state();
    fx.agent(&state, "Freya", false);
    fx.route("freya", true);
    reconcile_pass(
        &state,
        &host_up("boot-a", Duration::from_secs(90)),
        &no_panes(),
    )
    .unwrap();
    assert_eq!(inbox(&state, "freya").len(), 1);

    // The hub starts ten hours after the next boot: still told.
    let late = host_up("boot-b", hours(10));
    let report = reconcile_pass(&state, &late, &no_panes()).unwrap();
    assert_eq!(report.notified, ["freya"]);
    assert_eq!(inbox(&state, "freya").len(), 2);
    assert_eq!(fx.epoch(&state)["boot_id"], "boot-b");
}

#[test]
fn a_hub_restart_in_the_same_boot_tells_only_the_agents_expected_when_the_record_was_made() {
    let fx = Fixture::new();
    let state = fx.state();
    fx.agent(&state, "Freya", false);
    fx.agent(&state, "Ferris", false);
    fx.route("freya", true);
    fx.route("ferris", false);
    let first = reconcile_pass(&state, &host("boot-a"), &no_panes()).unwrap();
    assert_eq!(first.notified, ["freya"]);
    assert_eq!(first.pending, ["ferris"]);

    // An agent registered after the record was made, with a live route, is never told.
    fx.agent(&state, "Fenn", false);
    fx.route("fenn", true);
    // Ferris's route comes up after a hub restart long into the boot: told, as it is expected.
    fx.route("ferris", true);
    let restarted = fx.state();
    let late = host_up("boot-a", hours(30));
    let report = reconcile_pass(&restarted, &late, &no_panes()).unwrap();
    assert_eq!(report.notified, ["ferris"]);
    assert!(report.pending.is_empty(), "fenn is not owed anything");
    assert!(inbox(&restarted, "fenn").is_empty());
    assert_eq!(
        fx.epoch(&restarted)["expected"],
        serde_json::json!(["ferris", "freya"])
    );
}

#[test]
fn a_hub_deploy_in_the_first_hour_tells_nobody_new() {
    let fx = Fixture::new();
    let state = fx.state();
    fx.agent(&state, "Freya", false);
    fx.route("freya", true);
    reconcile_pass(&state, &host("boot-a"), &no_panes()).unwrap();
    fx.agent(&state, "Newcomer", false);
    fx.route("newcomer", true);

    let report = reconcile_pass(&fx.state(), &host("boot-a"), &no_panes()).unwrap();

    assert!(report.notified.is_empty() && report.pending.is_empty());
    assert!(inbox(&state, "newcomer").is_empty());
    assert_eq!(inbox(&state, "freya").len(), 1);
}

#[test]
fn an_agent_that_was_retired_after_the_record_was_made_is_not_told() {
    let fx = Fixture::new();
    let state = fx.state();
    fx.agent(&state, "Freya", false);
    fx.route("freya", false);
    let first = reconcile_pass(&state, &host("boot-a"), &no_panes()).unwrap();
    assert_eq!(first.pending, ["freya"]);
    fx.agent(&state, "Freya", true);
    fx.route("freya", true);
    let second = reconcile_pass(&state, &host("boot-a"), &no_panes()).unwrap();
    assert!(second.notified.is_empty() && second.pending.is_empty());
}

#[test]
fn a_pane_listing_that_tmux_would_not_give_changes_nothing() {
    let fx = Fixture::new();
    let state = fx.state();
    fx.agent(&state, "Fiona", true);
    fx.route("fiona", true);
    let report = reconcile_pass(&state, &host("boot-a"), &FakePanes(None)).unwrap();
    assert!(report.orphans.is_empty());
    assert!(!report.tmux_answered);
    assert!(fx.has_route("fiona"), "an unknown pane list drops no route");
}

#[test]
fn the_boot_record_is_a_runtime_state_entry() {
    let fx = Fixture::new();
    let state = fx.state();
    fx.agent(&state, "Freya", false);
    fx.route("freya", true);
    reconcile_pass(&state, &host("boot-a"), &no_panes()).unwrap();
    let value = fx.epoch(&state);
    assert_eq!(value["boot_id"], "boot-a");
    assert_eq!(value["expected"], serde_json::json!(["freya"]));
    assert_eq!(value["notified"], serde_json::json!(["freya"]));
}

// F5: the shared Muse session.

#[test]
fn a_muse_window_of_an_active_agent_is_not_an_orphan() {
    let fx = Fixture::new();
    let state = fx.state();
    fx.agent(&state, "Ada", false);
    fx.route("ada", true);
    let panes = FakePanes(Some(vec![muse_pane("ada", "%4")]));

    let report = reconcile_pass(&state, &host("boot-a"), &panes).unwrap();

    assert!(report.orphans.is_empty(), "{:?}", report.orphans);
    assert!(fx.has_route("ada"));
    assert_eq!(report.notified, ["ada"]);
}

#[test]
fn a_muse_window_makes_an_old_route_live_for_its_persona_only() {
    let fx = Fixture::new();
    let state = fx.state();
    fx.agent(&state, "Ada", false);
    fx.codex_route("ada", old());
    let report = reconcile_pass(
        &state,
        &host("boot-a"),
        &FakePanes(Some(vec![muse_pane("bob", "%4")])),
    )
    .unwrap();
    assert!(report.notified.is_empty());
    let report = reconcile_pass(
        &state,
        &host("boot-a"),
        &FakePanes(Some(vec![muse_pane("ada", "%5")])),
    )
    .unwrap();
    assert_eq!(report.notified, ["ada"]);
}

#[test]
fn a_muse_window_of_a_retired_or_unknown_persona_is_an_orphan_of_that_persona() {
    let fx = Fixture::new();
    let state = fx.state();
    fx.agent(&state, "Ada", false);
    fx.agent(&state, "Fiona", true);
    fx.route("ada", true);
    fx.route("fiona", true);
    let panes = FakePanes(Some(vec![
        muse_pane("ada", "%4"),
        muse_pane("fiona", "%5"),
        muse_pane("stranger", "%6"),
    ]));

    let report = reconcile_pass(&state, &host("boot-a"), &panes).unwrap();

    let found: Vec<_> = report
        .orphans
        .iter()
        .map(|o| (o.persona.as_deref(), o.reason.as_str(), o.session.as_str()))
        .collect();
    assert_eq!(
        found,
        [
            (Some("fiona"), "retired", "muse"),
            (Some("stranger"), "unknown", "muse")
        ]
    );
    assert!(!fx.has_route("fiona"));
    assert!(fx.has_route("ada"), "the active window keeps its route");
}

#[test]
fn a_muse_pane_with_no_window_name_is_skipped_and_never_stands_for_a_persona_muse() {
    let fx = Fixture::new();
    let state = fx.state();
    fx.route("muse", true);
    let panes = FakePanes(Some(vec![muse_pane("", "%4")]));

    let report = reconcile_pass(&state, &host("boot-a"), &panes).unwrap();

    assert!(report.orphans.is_empty(), "{:?}", report.orphans);
    assert!(fx.has_route("muse"), "no route is removed for `muse`");
}

#[test]
fn tmux_pane_lines_parse_and_only_vendor_cli_panes_count() {
    let panes = parse_panes(
        "Freya\t%1\tmain\tcodex\tcodex resume 01a --yolo\n0\t%0\t0\tzsh\t\n\t%9\tw\tx\ty\nFeira\t%2\tmain\tnode\t/usr/bin/env X=1 claude --resume a\nmuse\t%3\tada\tmuse\tmuse --resume\twith\ttab\n",
    );
    assert_eq!(panes.len(), 4, "a line with no session name is skipped");
    assert_eq!(panes[0].window, "main");
    assert_eq!(panes[0].start_command, "codex resume 01a --yolo");
    assert_eq!(
        (panes[3].session.as_str(), panes[3].window.as_str()),
        ("muse", "ada")
    );
    assert_eq!(panes[3].start_command, "muse --resume\twith\ttab");
    let binaries = vendor_binaries();
    let runs: Vec<_> = panes
        .iter()
        .map(|p| runs_vendor_cli(p, &binaries))
        .collect();
    assert_eq!(runs, [true, false, true, true]);
}

// F10: nothing is killed, and the hub wiring.

/// A fake tmux that logs its arguments and answers `list-panes` with `panes`.
fn fake_tmux(dir: &Path, panes: &str) -> (PathBuf, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let log = dir.join("tmux.log");
    let canned = dir.join("panes.txt");
    std::fs::write(&canned, panes).unwrap();
    let bin = dir.join("fake-tmux");
    std::fs::write(
        &bin,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\ncase \"$1\" in list-panes) cat '{}';; esac\n",
            log.display(),
            canned.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    (bin, log)
}

#[test]
fn a_restored_pane_is_only_listed_never_killed() {
    let fx = Fixture::new();
    let state = fx.state();
    fx.agent(&state, "Fiona", true);
    fx.route("fiona", true);
    let (bin, log) = fake_tmux(
        fx.tmp.path(),
        "Fiona\t%6\tmain\tcodex\tcodex resume x\nmuse\t%7\tstranger\tmuse\tmuse\n",
    );
    let tmux = TmuxPanes { bin: Some(bin) };
    // A fresh script can be busy for an instant when another test forks.
    let mut tries = 0;
    while tmux.panes().is_none() && tries < 40 {
        tries += 1;
        std::thread::sleep(Duration::from_millis(25));
    }
    std::fs::remove_file(&log).ok();

    let report = reconcile_pass(&state, &host("boot-a"), &tmux).unwrap();

    assert_eq!(report.orphans.len(), 2, "{:?}", report.orphans);
    assert!(report.tmux_answered);
    let calls = std::fs::read_to_string(&log).unwrap();
    assert!(!calls.is_empty(), "tmux was asked");
    for call in calls.lines() {
        assert!(
            call.starts_with("list-panes -a -F ") && call.contains(PANE_FORMAT),
            "{call}"
        );
    }
    assert!(!calls.contains("kill"), "{calls}");
}

fn server_config(fx: &Fixture) -> HubServerConfig {
    std::fs::create_dir_all(fx.home()).unwrap();
    HubServerConfig {
        listen: "127.0.0.1:0".into(),
        store_path: fx.tmp.path().join("store.redb"),
        repo_root: fx.tmp.path().to_path_buf(),
        home_override: Some(fx.home()),
        ..HubServerConfig::default()
    }
}

struct CountingPanes(Arc<AtomicU32>);

impl PaneSource for CountingPanes {
    fn panes(&self) -> Option<Vec<Pane>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Some(Vec::new())
    }
}

fn counting_plan(asked: &Arc<AtomicU32>) -> Plan {
    Plan {
        host: Some(host("boot-a")),
        panes: Arc::new(CountingPanes(asked.clone())),
        interval: Duration::from_millis(5),
        max_passes: 2,
        min_window: Duration::ZERO,
    }
}

#[tokio::test]
async fn serve_runs_the_reconciliation_only_when_it_is_enabled() {
    use crate::HubServer;
    let fx = Fixture::new();

    // The daemon's switch turns it on; a plain server does not have it.
    assert!(
        HubServer::bind(server_config(&fx))
            .await
            .unwrap()
            .with_boot_reconcile()
            .boot_reconcile_enabled()
    );
    assert!(
        !HubServer::bind(server_config(&fx))
            .await
            .unwrap()
            .boot_reconcile_enabled()
    );

    // Off: serving asks tmux nothing.
    let asked = Arc::new(AtomicU32::new(0));
    let off = HubServer::bind(server_config(&fx)).await.unwrap();
    let stop = off.shutdown_handle();
    let task = tokio::spawn(off.serve());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(asked.load(Ordering::SeqCst), 0);
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();

    // On: serving makes the pass (with the fake host and tmux handed in).
    let on = HubServer::bind(server_config(&fx))
        .await
        .unwrap()
        .with_boot_reconcile_plan(counting_plan(&asked));
    let stop = on.shutdown_handle();
    let task = tokio::spawn(on.serve());
    for _ in 0..200 {
        if asked.load(Ordering::SeqCst) > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(asked.load(Ordering::SeqCst) > 0, "the pass did not run");
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
}

// F6: the start-up passes.

/// Panes that change as the passes go: `script[n]` for the n-th call, the last one after that.
struct ScriptedPanes {
    script: Vec<Option<Vec<Pane>>>,
    calls: AtomicU32,
}

impl PaneSource for ScriptedPanes {
    fn panes(&self) -> Option<Vec<Pane>> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst) as usize;
        self.script[n.min(self.script.len() - 1)].clone()
    }
}

#[tokio::test]
async fn the_start_up_passes_are_bounded_and_stop_when_nothing_is_owed() {
    let fx = Fixture::new();
    let state = fx.state();
    fx.agent(&state, "Freya", false);
    fx.route("freya", false);
    let panes: Arc<dyn PaneSource + Send + Sync> = Arc::new(no_panes());
    let tick = Duration::from_millis(1);

    // A route that never comes up is waited on for the pass limit only.
    let passes = run_with(
        state.clone(),
        host("boot-a"),
        panes.clone(),
        tick,
        3,
        Duration::ZERO,
    )
    .await;
    assert_eq!(passes, 3);
    assert!(inbox(&state, "freya").is_empty());

    // Once it is up, one pass tells the agent and the loop ends.
    fx.route("freya", true);
    let passes = run_with(
        state.clone(),
        host("boot-a"),
        panes,
        tick,
        3,
        Duration::ZERO,
    )
    .await;
    assert_eq!(passes, 1);
    assert_eq!(inbox(&state, "freya").len(), 1);
}

#[tokio::test]
async fn the_passes_go_on_until_tmux_has_answered() {
    let fx = Fixture::new();
    let state = fx.state();
    let tick = Duration::from_millis(1);
    // Nothing is pending, but tmux never answers: all the passes are made.
    let never: Arc<dyn PaneSource + Send + Sync> = Arc::new(FakePanes(None));
    let passes = run_with(
        state.clone(),
        host("boot-a"),
        never,
        tick,
        4,
        Duration::ZERO,
    )
    .await;
    assert_eq!(passes, 4);

    // It answers on the third call: the loop ends there.
    let late: Arc<dyn PaneSource + Send + Sync> = Arc::new(ScriptedPanes {
        script: vec![None, None, Some(Vec::new())],
        calls: AtomicU32::new(0),
    });
    let passes = run_with(state, host("boot-a"), late, tick, 10, Duration::ZERO).await;
    assert_eq!(passes, 3);
}

#[tokio::test]
async fn the_passes_go_on_for_the_minimum_window_even_when_nothing_is_pending() {
    let fx = Fixture::new();
    let state = fx.state();
    let panes: Arc<dyn PaneSource + Send + Sync> = Arc::new(no_panes());
    let started = std::time::Instant::now();
    let passes = run_with(
        state,
        host("boot-a"),
        panes,
        Duration::from_millis(10),
        500,
        Duration::from_millis(120),
    )
    .await;
    assert!(started.elapsed() >= Duration::from_millis(120));
    assert!((2..500).contains(&passes), "{passes}");
}

#[tokio::test]
async fn a_pane_restored_late_is_still_found_and_reported_once() {
    let fx = Fixture::new();
    let state = fx.state();
    fx.agent(&state, "Fiona", true);
    fx.route("fiona", true);
    let mut rx = state.subscribe();
    let panes: Arc<dyn PaneSource + Send + Sync> = Arc::new(ScriptedPanes {
        script: vec![Some(Vec::new()), Some(vec![pane("Fiona", "%6", "codex")])],
        calls: AtomicU32::new(0),
    });

    let passes = run_with(
        state.clone(),
        host("boot-a"),
        panes,
        Duration::from_millis(5),
        8,
        Duration::from_millis(80),
    )
    .await;

    assert!(passes >= 3, "{passes}");
    assert!(!fx.has_route("fiona"), "the late pane's route is dropped");
    let events: Vec<_> = drain(&mut rx)
        .into_iter()
        .filter(|f| f.event == "session.restore_orphan")
        .collect();
    assert_eq!(events.len(), 1, "reported once across the passes");
}
