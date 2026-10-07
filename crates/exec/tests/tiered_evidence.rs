//! A finding's evidence names its record across tiers (routing spec §2.5):
//! a GitHub-tier finding about a local record is reproduced through the
//! routing tracker, and the run it records is tied to the local record.

use fl_core::mem_issues::{ISSUES, MemIssues};
use fl_core::model::{CommandSpec, GateKind, PopulationDelivery, Selector};
use fl_core::store::{Catalog, Ledger, Roles, Tracker};
use fl_core::{Finding, MemStore, RoutingMap, TieredTracker};
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
fn a_github_finding_about_a_local_record_tags_its_evidence_with_the_local_iri() {
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
    };
    let r = router
        .add_record_with_area(&p, "fix the parser", Some("code"))
        .unwrap();
    let mut f = Finding::raise(p.clone(), r.clone(), "rev", "it breaks");
    f.area = Some("design".into());
    let fid = router.add_finding(f).unwrap();
    assert!(
        fid.iri().as_str().starts_with(ISSUES),
        "the finding is on GitHub"
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
        Some(&r),
        "the run names the record where it lives"
    );
    assert_eq!(
        issues.get_finding(&fid).unwrap().unwrap().reproduction,
        Some(gate)
    );
}
