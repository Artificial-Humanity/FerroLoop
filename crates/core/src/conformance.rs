//! The contract every store must meet, one suite per role (spec §6.1).
//!
//! ⚠ A store gets no suite of its own and no privileged path. `MemStore`,
//! `RedbStore` and, later, a GitHub store all run these same functions. A
//! case that only one store can pass is a defect in that store, not a case to
//! move out of here.
//!
//! `make` returns the store plus a guard to keep alive (a temp directory for
//! a file store, `()` for memory).

use crate::finding::{Finding, FindingState};
use crate::log::GateRun;
use crate::model::{
    CommandSpec, GateKind, PopulationDelivery, Regret, Selector, State, Transition,
};
use crate::store::{Catalog, Ledger, Tracker};
use crate::verdict::Verdict;

pub fn catalog<S: Catalog, G>(make: impl Fn() -> (S, G)) {
    let cases: &[fn(&S)] = &[
        a_project_round_trips::<S>,
        affirming_a_gate_moves_only_its_stamp::<S>,
        list_transitions_returns_every_transition_of_one_project_and_no_others::<S>,
    ];
    for case in cases {
        let (s, _guard) = make();
        case(&s);
    }
}

pub fn tracker<S: Catalog + Tracker, G>(make: impl Fn() -> (S, G)) {
    let cases: &[fn(&S)] = &[
        list_records_returns_only_the_named_projects_records::<S>,
        a_record_state_change_is_visible_on_the_next_read::<S>,
        a_finding_round_trips_and_gets_a_real_id::<S>,
        withdrawals_are_counted_against_whoever_raised_the_finding::<S>,
        findings_are_listed_per_project::<S>,
    ];
    for case in cases {
        let (s, _guard) = make();
        case(&s);
    }
}

pub fn ledger<S: Catalog + Ledger, G>(make: impl Fn() -> (S, G)) {
    let cases: &[fn(&S)] = &[the_logs_are_append_only_and_read_back_in_order::<S>];
    for case in cases {
        let (s, _guard) = make();
        case(&s);
    }
}

/// Cases that need one store backing all three roles. Empty until Task 4.
pub fn all_roles<S: Catalog + Tracker + Ledger, G>(make: impl Fn() -> (S, G)) {
    let cases: &[fn(&S)] = &[];
    for case in cases {
        let (s, _guard) = make();
        case(&s);
    }
}

fn a_project_round_trips<S: Catalog>(s: &S) {
    let id = s.add_project("/tmp/p").unwrap();
    assert_eq!(s.get_project(&id).unwrap().unwrap().root, "/tmp/p");
    assert_eq!(s.list_projects().unwrap().len(), 1);
}

fn affirming_a_gate_moves_only_its_stamp<S: Catalog>(s: &S) {
    let p = s.add_project("/tmp/p").unwrap();
    let g = s
        .add_gate(
            &p,
            "fmt",
            sample_kind(),
            sample_selector(),
            1,
            "abc",
            "owner",
        )
        .unwrap();
    let mut def = s.get_gate(&g).unwrap().unwrap();
    def.authored_at_commit = "def".into();
    s.update_gate(&def).unwrap();
    let back = s.get_gate(&g).unwrap().unwrap();
    assert_eq!(back.authored_at_commit, "def");
    assert_eq!(back.name, "fmt");
}

// `record move` asks which transitions cover a (from, to) pair, so this
// scan decides whether a move is gated at all. A scan that leaked another
// project's transitions would gate a move against the wrong repository's
// tree; one that dropped its own would let a gated move through ungated.
fn list_transitions_returns_every_transition_of_one_project_and_no_others<S: Catalog>(s: &S) {
    let p1 = s.add_project("/p1").unwrap();
    let p2 = s.add_project("/p2").unwrap();
    let p3 = s.add_project("/p3").unwrap();

    for (project, name) in [(p1, "launch"), (p1, "ship"), (p2, "launch")] {
        s.add_transition(Transition {
            project,
            name: name.into(),
            from: State::Review,
            to: State::Done,
            regret: Regret::High,
            gates: vec![],
        })
        .unwrap();
    }

    let mut names: Vec<String> = s
        .list_transitions(&p1)
        .unwrap()
        .into_iter()
        .map(|t| t.name)
        .collect();
    names.sort();
    assert_eq!(names, vec!["launch".to_string(), "ship".to_string()]);
    // Project 2 has a transition of the SAME name, so a scan that keyed
    // on the name alone would return it here.
    assert!(
        s.list_transitions(&p1)
            .unwrap()
            .iter()
            .all(|t| t.project == p1)
    );
    assert_eq!(s.list_transitions(&p2).unwrap().len(), 1);
    assert_eq!(s.list_transitions(&p3).unwrap().len(), 0);
}

fn list_records_returns_only_the_named_projects_records<S: Catalog + Tracker>(s: &S) {
    let p1 = s.add_project("/tmp/p").unwrap();
    let p2 = s.add_project("/tmp/q").unwrap();
    let r1 = s.add_record(&p1, "fix the thing").unwrap();
    let _r2 = s.add_record(&p2, "unrelated").unwrap();
    let recs = s.list_records(&p1).unwrap();
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].id, r1);
    assert_eq!(recs[0].title, "fix the thing");
}

fn a_record_state_change_is_visible_on_the_next_read<S: Catalog + Tracker>(s: &S) {
    let p = s.add_project("/tmp/p").unwrap();
    let r = s.add_record(&p, "fix the thing").unwrap();
    s.set_record_state(&r, State::Doing).unwrap();
    assert_eq!(s.get_record(&r).unwrap().unwrap().state, State::Doing);
}

fn a_finding_round_trips_and_gets_a_real_id<S: Catalog + Tracker>(s: &S) {
    let p = s.add_project("/p").unwrap();
    let r = s.add_record(&p, "t").unwrap();
    let id = s
        .add_finding(Finding::raise(p, r, "reviewer", "wrong on empty"))
        .unwrap();
    assert_ne!(id.get(), 0, "the store must replace the placeholder id");
    let back = s.get_finding(&id).unwrap().unwrap();
    assert_eq!(back.id, id);
    assert_eq!(back.state, FindingState::Raised);
}

fn withdrawals_are_counted_against_whoever_raised_the_finding<S: Catalog + Tracker>(s: &S) {
    let p = s.add_project("/p").unwrap();
    let r = s.add_record(&p, "t").unwrap();

    for claim in ["a", "b"] {
        let id = s.add_finding(Finding::raise(p, r, "hasty", claim)).unwrap();
        let mut f = s.get_finding(&id).unwrap().unwrap();
        f.withdraw("not concrete").unwrap();
        s.update_finding(&f).unwrap();
    }
    let id = s.add_finding(Finding::raise(p, r, "careful", "c")).unwrap();
    let mut f = s.get_finding(&id).unwrap().unwrap();
    let gate = s
        .add_gate(&p, "g", sample_kind(), sample_selector(), 1, "abc", "owner")
        .unwrap();
    f.attach_reproduction(gate).unwrap();
    s.update_finding(&f).unwrap();

    assert_eq!(s.withdrawals_by("hasty").unwrap(), 2);
    assert_eq!(s.withdrawals_by("careful").unwrap(), 0);
    assert_eq!(s.withdrawals_by("nobody").unwrap(), 0);
}

fn findings_are_listed_per_project<S: Catalog + Tracker>(s: &S) {
    let a = s.add_project("/a").unwrap();
    let b = s.add_project("/b").unwrap();
    let ra = s.add_record(&a, "t").unwrap();
    let rb = s.add_record(&b, "t").unwrap();
    s.add_finding(Finding::raise(a, ra, "r", "one")).unwrap();
    s.add_finding(Finding::raise(b, rb, "r", "two")).unwrap();
    assert_eq!(s.list_findings(&a).unwrap().len(), 1);
}

fn the_logs_are_append_only_and_read_back_in_order<S: Catalog + Ledger>(s: &S) {
    let p = s.add_project("/tmp/p").unwrap();
    let g = s
        .add_gate(
            &p,
            "fmt",
            sample_kind(),
            sample_selector(),
            1,
            "abc",
            "owner",
        )
        .unwrap();
    s.append_gate_run(sample_run(g, "abc", 3)).unwrap();
    s.append_gate_run(sample_run(g, "def", 5)).unwrap();
    let runs = s.gate_runs(&g).unwrap();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].commit, "abc");
    assert_eq!(runs[1].population, 5);
}

fn sample_kind() -> GateKind {
    GateKind::Command(CommandSpec {
        program: "true".into(),
        args: vec![],
        delivery: PopulationDelivery::Args,
        timeout_secs: 5,
        pass_codes: vec![0],
    })
}

fn sample_selector() -> Selector {
    Selector::Glob {
        pattern: "**/*.rs".into(),
    }
}

fn sample_run(gate: crate::ids::GateId, commit: &str, population: u64) -> GateRun {
    GateRun {
        gate,
        record: None,
        commit: commit.into(),
        verdict: Verdict::from_predicate(true, population),
        population,
        output_excerpt: String::new(),
        duration_ms: 1,
        cost_usd_micros: 0,
    }
}
