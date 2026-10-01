//! Drives fl-exec's decision sites through a real [`SplitLedger`], not the
//! `Journal` test double every unit test in `record.rs`/`finding.rs` uses.
//!
//! ⚠⚠ Whole-branch review, finding 1: `Journal::flush` accepts any
//! `rests_on` it is handed — it never checks that a cited run is tied to a
//! record this repository owns, or that it is actually pending publication
//! (`SplitLedger::flush` does both; GitHub ledger spec §2.1, §2.2). No
//! committed test drove fl-exec's decision sites (`attach_reproduction`,
//! `verify_finding`, `move_record`) through a real `SplitLedger`, so a
//! defect in how those call sites tag their gate runs — for instance,
//! forgetting to tie a neighbour's re-run to the finding's record — could
//! pass the whole suite. This test exercises the real thing: `reproduce` →
//! assign → `verify` (with a neighbour gate) → an ungated `move`, over
//! `SplitLedger { local: &MemStore, github: &MemRemote }`.
//!
//! Mutation check performed by hand (see the commit message): changing
//! `verify_finding`'s neighbour re-run from `Some(&f.record)` to `None`
//! turns this test red — `SplitLedger::flush` refuses the verify because
//! the decision would rest on a run that is neither being published nor
//! already published. Restored before committing.

use fl_core::conformance::{MemRemote, entry_iri};
use fl_core::model::{CommandSpec, GateKind, PopulationDelivery, Selector, State};
use fl_core::split::{Outbox, RemoteLedger, SplitLedger};
use fl_core::store::{Catalog, Ledger, Roles, Tracker};
use fl_core::{Finding, GateId, MemStore, ProjectId};
use fl_exec::evaluate::run_single_gate;
use fl_exec::finding::{attach_reproduction, verify_finding};
use fl_exec::record::{MoveOutcome, move_record};
use std::path::Path;
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

fn gate(s: &MemStore, p: &ProjectId, root: &Path, name: &str, program: &str) -> GateId {
    let head = fl_exec::Git::head(root).unwrap();
    s.add_gate(
        p,
        name,
        GateKind::Command(CommandSpec {
            program: program.into(),
            args: vec![],
            delivery: PopulationDelivery::Args,
            timeout_secs: 10,
            pass_codes: vec![0],
        }),
        Selector::Glob {
            pattern: "src/**/*.rs".into(),
        },
        1,
        &head,
        "tester",
    )
    .unwrap()
}

#[test]
fn reproduce_assign_verify_and_an_ungated_move_publish_through_a_real_split_ledger() {
    let d = repo();
    let s = MemStore::default();
    s.set_cutover("R_1", &entry_iri(0)).unwrap();
    let remote = MemRemote::new("R_1");
    let ledger = SplitLedger {
        local: &s,
        github: &remote,
    };

    let p = s.add_project(&d.path().display().to_string()).unwrap();
    let r = s.add_record(&p, "t").unwrap();
    let neighbour = gate(&s, &p, d.path(), "neighbour", "true");
    let reproduction = gate(&s, &p, d.path(), "reproduction", "false");

    // The neighbour's baseline pass: untagged (no record), so it earns a
    // `last_pass_commit` but must never become a candidate for publishing.
    run_single_gate(&s, &ledger, &p, &neighbour, None).unwrap();
    assert!(
        s.get_gate(&neighbour)
            .unwrap()
            .unwrap()
            .last_pass_commit
            .is_some(),
        "the baseline pass must qualify the neighbour for verify's regression check"
    );
    let baseline_id = s.gate_runs(&neighbour).unwrap()[0].id.clone().unwrap();

    let f = s
        .add_finding(Finding::raise(p.clone(), r.clone(), "reviewer", "claim"))
        .unwrap();

    let roles = || Roles {
        catalog: &s,
        tracker: &s,
        ledger: &ledger,
    };

    // reproduce
    let (_, reproduce_flushed) = attach_reproduction(roles(), &f, &reproduction).unwrap();
    assert!(reproduce_flushed.commit.is_some());

    // assign
    let mut fin = s.get_finding(&f).unwrap().unwrap();
    fin.assign("fixer").unwrap();
    s.update_finding(&fin).unwrap();

    // The "repair": the reproduction now passes; the neighbour is untouched,
    // so verify runs it again (tied to the record this time) and it stays
    // green — no regression.
    let mut rep_def = s.get_gate(&reproduction).unwrap().unwrap();
    rep_def.kind = GateKind::Command(CommandSpec {
        program: "true".into(),
        args: vec![],
        delivery: PopulationDelivery::Args,
        timeout_secs: 10,
        pass_codes: vec![0],
    });
    s.update_gate(&rep_def).unwrap();

    // verify, with the neighbour gate exercised
    let report = verify_finding(roles(), &f).unwrap();
    assert!(report.closed, "no regression: the verify must close it");
    assert_eq!(report.neighbours.len(), 1);
    assert!(report.flushed.commit.is_some());

    // an ungated move: nothing covers (Todo, Doing) for this project
    let record = s.get_record(&r).unwrap().unwrap();
    let move_report = move_record(roles(), &record, State::Doing).unwrap();
    assert!(matches!(move_report.outcome, MoveOutcome::Ungated));
    assert!(move_report.flushed.commit.is_some());

    assert_eq!(
        remote.decisions().len(),
        3,
        "reproduce, verify and the ungated move each published one decision"
    );

    // The neighbour's untagged baseline run stayed local...
    assert!(!s.is_published("R_1", &baseline_id).unwrap());
    assert!(
        remote
            .gate_runs(&neighbour)
            .unwrap()
            .iter()
            .all(|run| run.id.as_ref() != Some(&baseline_id)),
        "the untied baseline run must never reach GitHub"
    );
    // ...while the runs tagged with the record went out: `reproduce`'s run
    // and verify's own re-run of the reproduction gate, plus the neighbour's
    // tied re-run from verify.
    assert_eq!(remote.gate_runs(&reproduction).unwrap().len(), 2);
    let published_neighbour_runs = remote.gate_runs(&neighbour).unwrap();
    assert_eq!(
        published_neighbour_runs.len(),
        1,
        "only the tied re-run from verify, not the untagged baseline"
    );
    assert_eq!(published_neighbour_runs[0].record, Some(r));
}
