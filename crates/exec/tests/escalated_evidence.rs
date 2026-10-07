//! Evidence names an escalated record where it now lives (routing spec
//! §2.5 "Evidence", §3.5): a local finding about a local record keeps its
//! stored reference when the record is escalated, and a reproduction run
//! through the routing tracker afterwards is tagged with the record's issue,
//! never the old local IRI.

use fl_core::mem_issues::{ISSUES, MemIssues};
use fl_core::model::{CommandSpec, GateKind, PopulationDelivery, Selector};
use fl_core::store::{Catalog, Ledger, Roles, Tracker};
use fl_core::{Finding, Kind, MemStore, RecordId, RoutingMap, TieredTracker};
use fl_exec::finding::attach_reproduction;
use std::process::Command;

fn repo() -> tempfile::TempDir {
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
    std::fs::create_dir_all(d.path().join("src")).unwrap();
    std::fs::write(d.path().join("src/a.rs"), "fn a() {}").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-qm", "first"]);
    d
}

#[test]
fn a_reproduction_after_its_records_escalation_tags_its_run_with_the_issue() {
    let d = repo();
    let local = MemStore::default();
    let issues = MemIssues::default();
    let p = local.add_project(&d.path().display().to_string()).unwrap();
    local.set_routes(&p, &RoutingMap::starting()).unwrap();
    let router = TieredTracker {
        catalog: &local,
        local: &local,
        routes: &local,
        github: &issues,
        escalations: &local,
    };
    let r = router
        .add_record_with_area(&p, "fix the parser", Some("code"))
        .unwrap();
    let fid = router
        .add_finding(Finding::raise(p.clone(), r.clone(), "rev", "it breaks"))
        .unwrap();
    assert!(
        !fid.iri().as_str().starts_with(ISSUES),
        "the finding is local"
    );

    let at = router.prepare_escalation(r.iri(), Kind::Record).unwrap();
    let issue = RecordId(
        router
            .escalate(&at, "alice", "a person decides", 1)
            .unwrap(),
    );
    assert!(
        issue.iri().as_str().starts_with(ISSUES),
        "the record is on GitHub"
    );

    let head = fl_exec::Git::head(d.path()).unwrap();
    let kind = GateKind::Command(CommandSpec {
        program: "false".into(),
        args: vec![],
        delivery: PopulationDelivery::Args,
        timeout_secs: 30,
        pass_codes: vec![0],
    });
    let sel = Selector::Glob {
        pattern: "src/**/*.rs".into(),
    };
    let gate = local
        .add_gate(&p, "fails", kind, sel, 1, &head, "o")
        .unwrap();
    let roles = Roles {
        catalog: &local,
        tracker: &router,
        ledger: &local,
    };
    attach_reproduction(roles, &fid, &gate).unwrap();
    let runs = local.gate_runs(&gate).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(
        runs[0].record.as_ref(),
        Some(&issue),
        "the run names the record where it lives now"
    );
    // The finding stays local, and its stored reference is not rewritten
    // (routing spec §3.5): it resolves through the tombstone.
    let stored = local.get_finding(&fid).unwrap().unwrap();
    assert_eq!(stored.record, r);
    assert_eq!(stored.reproduction, Some(gate));
}
