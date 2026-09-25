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
use crate::ids::{FindingId, GateId, Kind, ProjectId, RecordId, seq_iri};
use crate::iri::Iri;
use crate::log::GateRun;
use crate::model::{
    CommandSpec, GateDef, GateKind, PopulationDelivery, Regret, Selector, State, Transition,
};
use crate::store::{Catalog, Handles, Ledger, StoreError, Tracker};
use crate::verdict::Verdict;

pub fn catalog<S: Catalog, G>(make: impl Fn() -> (S, G)) {
    let cases: &[fn(&S)] = &[
        a_project_round_trips::<S>,
        affirming_a_gate_moves_only_its_stamp::<S>,
        list_transitions_returns_every_transition_of_one_project_and_no_others::<S>,
    ];
    assert!(
        !cases.is_empty(),
        "the catalog suite has no cases, so it would pass over nothing"
    );
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
        an_alias_reaches_the_item_it_names::<S>,
        an_alias_already_in_use_is_refused_and_names_it::<S>,
    ];
    assert!(
        !cases.is_empty(),
        "the tracker suite has no cases, so it would pass over nothing"
    );
    for case in cases {
        let (s, _guard) = make();
        case(&s);
    }
}

pub fn ledger<S: Catalog + Ledger, G>(make: impl Fn() -> (S, G)) {
    let cases: &[fn(&S)] = &[the_logs_are_append_only_and_read_back_in_order::<S>];
    assert!(
        !cases.is_empty(),
        "the ledger suite has no cases, so it would pass over nothing"
    );
    for case in cases {
        let (s, _guard) = make();
        case(&s);
    }
}

/// Cases that need one store backing all three roles, and its handles.
pub fn all_roles<S: Catalog + Tracker + Ledger + Handles, G>(make: impl Fn() -> (S, G)) {
    let cases: &[fn(&S)] = &[
        an_id_this_store_never_held_is_not_owned_rather_than_absent::<S>,
        a_list_over_a_project_this_store_never_held_is_refused_not_empty::<S>,
        a_finding_on_a_record_this_store_never_held_is_refused::<S>,
        an_id_of_another_kind_is_owned_but_not_found::<S>,
        handles_are_per_kind_and_start_at_one::<S>,
        no_operation_gives_up_an_owned_id::<S>,
    ];
    assert!(
        !cases.is_empty(),
        "the all-roles suite has no cases, so it would pass over nothing"
    );
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

    for (project, name) in [(&p1, "launch"), (&p1, "ship"), (&p2, "launch")] {
        s.add_transition(Transition {
            project: project.clone(),
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
    assert_ne!(
        id,
        FindingId(seq_iri(0)),
        "the store must replace the placeholder id"
    );
    let back = s.get_finding(&id).unwrap().unwrap();
    assert_eq!(back.id, id);
    assert_eq!(back.state, FindingState::Raised);
}

fn withdrawals_are_counted_against_whoever_raised_the_finding<S: Catalog + Tracker>(s: &S) {
    let p = s.add_project("/p").unwrap();
    let r = s.add_record(&p, "t").unwrap();

    for claim in ["a", "b"] {
        let id = s
            .add_finding(Finding::raise(p.clone(), r.clone(), "hasty", claim))
            .unwrap();
        let mut f = s.get_finding(&id).unwrap().unwrap();
        f.withdraw("not concrete").unwrap();
        s.update_finding(&f).unwrap();
    }
    let id = s
        .add_finding(Finding::raise(p.clone(), r.clone(), "careful", "c"))
        .unwrap();
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
    s.add_finding(Finding::raise(a.clone(), ra, "r", "one"))
        .unwrap();
    s.add_finding(Finding::raise(b, rb, "r", "two")).unwrap();
    assert_eq!(s.list_findings(&a).unwrap().len(), 1);
}

pub fn an_alias_reaches_the_item_it_names<S: Catalog + Tracker>(s: &S) {
    let p = s.add_project("/p").unwrap();
    let r = s.add_record(&p, "t").unwrap();
    let old = Iri::parse("https://github.com/o/r/issues/41").unwrap();
    s.add_alias(r.iri(), old.clone()).unwrap();
    let via_alias = s.get_record(&RecordId(old.clone())).unwrap().unwrap();
    assert_eq!(via_alias.id, r);
    assert!(via_alias.also_known_as.contains(&old));
}

pub fn an_alias_already_in_use_is_refused_and_names_it<S: Catalog + Tracker>(s: &S) {
    let p = s.add_project("/p").unwrap();
    let a = s.add_record(&p, "a").unwrap();
    let b = s.add_record(&p, "b").unwrap();
    let old = Iri::parse("https://github.com/o/r/issues/41").unwrap();
    s.add_alias(a.iri(), old.clone()).unwrap();
    let err = s.add_alias(b.iri(), old.clone()).unwrap_err();
    assert!(
        matches!(err, StoreError::AlreadyExists(ref i) if *i == old),
        "{err:?}"
    );
    // An existing primary id cannot become an alias either.
    let err = s.add_alias(b.iri(), a.0.clone()).unwrap_err();
    assert!(matches!(err, StoreError::AlreadyExists(_)), "{err:?}");
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
    s.append_gate_run(sample_run(g.clone(), "abc", 3)).unwrap();
    s.append_gate_run(sample_run(g.clone(), "def", 5)).unwrap();
    let runs = s.gate_runs(&g).unwrap();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].commit, "abc");
    assert_eq!(runs[1].population, 5);
}

/// Asserts that every result is `NotOwned` and names `id`. Each result is
/// labelled with the method that produced it, so a failure says which
/// method answered for an id its store never held.
fn assert_all_not_owned(id: &Iri, results: Vec<(&str, Result<(), StoreError>)>) {
    assert!(
        !results.is_empty(),
        "no methods were asked, so nothing was checked"
    );
    for (method, result) in results {
        match result {
            Err(e @ StoreError::NotOwned { .. }) => {
                assert!(e.to_string().contains(id.as_str()), "{method}: {e}");
            }
            other => panic!("{method} answered {other:?} for an id its store never held"),
        }
    }
}

// ⚠ Every method that takes an id must refuse a stranger with `NotOwned`,
// never answer `None`, and never act on it. One case per shape, so a store
// that skips the check in any single method fails here by name.
fn an_id_this_store_never_held_is_not_owned_rather_than_absent<S: Catalog + Tracker + Ledger>(
    s: &S,
) {
    let id = stranger();
    let (p, g, r, f) = (
        ProjectId(id.clone()),
        GateId(id.clone()),
        RecordId(id.clone()),
        FindingId(id.clone()),
    );
    let gate_def = GateDef {
        id: g.clone(),
        project: p.clone(),
        name: "g".into(),
        kind: sample_kind(),
        selector: sample_selector(),
        min_population: 1,
        authored_at_commit: "abc".into(),
        authored_by: "o".into(),
        last_pass_commit: None,
    };
    let mut finding = Finding::raise(p.clone(), r.clone(), "a", "c");
    finding.id = f.clone();
    assert_all_not_owned(
        &id,
        vec![
            ("get_project", s.get_project(&p).map(drop)),
            ("get_gate", s.get_gate(&g).map(drop)),
            ("get_record", s.get_record(&r).map(drop)),
            ("get_finding", s.get_finding(&f).map(drop)),
            ("get_transition", s.get_transition(&p, "launch").map(drop)),
            ("gate_runs", s.gate_runs(&g).map(drop)),
            ("update_gate", s.update_gate(&gate_def)),
            ("update_finding", s.update_finding(&finding)),
            ("set_record_state", s.set_record_state(&r, State::Doing)),
            (
                "add_gate",
                s.add_gate(&p, "g", sample_kind(), sample_selector(), 1, "abc", "o")
                    .map(drop),
            ),
            ("add_record", s.add_record(&p, "t").map(drop)),
            ("add_finding", s.add_finding(finding.clone()).map(drop)),
            (
                "add_transition",
                s.add_transition(Transition {
                    project: p.clone(),
                    name: "launch".into(),
                    from: State::Review,
                    to: State::Done,
                    regret: Regret::High,
                    gates: vec![],
                }),
            ),
        ],
    );
}

fn a_list_over_a_project_this_store_never_held_is_refused_not_empty<
    S: Catalog + Tracker + Ledger,
>(
    s: &S,
) {
    let id = stranger();
    let p = ProjectId(id.clone());
    assert_all_not_owned(
        &id,
        vec![
            ("list_gates", s.list_gates(&p).map(drop)),
            ("list_transitions", s.list_transitions(&p).map(drop)),
            ("list_records", s.list_records(&p).map(drop)),
            ("list_findings", s.list_findings(&p).map(drop)),
            ("attempts", s.attempts(&p).map(drop)),
        ],
    );
}

// The project is held, so only the record check can refuse this. A store
// that checked the project alone would store a finding about a record it
// never held.
fn a_finding_on_a_record_this_store_never_held_is_refused<S: Catalog + Tracker + Ledger>(s: &S) {
    let p = s.add_project("/p").unwrap();
    let id = stranger();
    let finding = Finding::raise(p.clone(), RecordId(id.clone()), "a", "c");
    assert_all_not_owned(&id, vec![("add_finding", s.add_finding(finding).map(drop))]);
    assert!(
        s.list_findings(&p).unwrap().is_empty(),
        "nothing may be stored"
    );
}

fn an_id_of_another_kind_is_owned_but_not_found<S: Catalog + Tracker + Ledger>(s: &S) {
    let p = s.add_project("/p").unwrap();
    let as_gate = GateId(p.0.clone());
    assert_eq!(s.get_gate(&as_gate).unwrap(), None);
}

fn handles_are_per_kind_and_start_at_one<S: Catalog + Tracker + Ledger + Handles>(s: &S) {
    let p = s.add_project("/p").unwrap();
    let g = s
        .add_gate(&p, "g", sample_kind(), sample_selector(), 1, "abc", "o")
        .unwrap();
    let r = s.add_record(&p, "t").unwrap();
    assert_eq!(s.handle_of(Kind::Project, p.iri()).unwrap(), Some(1));
    assert_eq!(s.handle_of(Kind::Gate, g.iri()).unwrap(), Some(1));
    assert_eq!(s.handle_of(Kind::Record, r.iri()).unwrap(), Some(1));
    assert_eq!(
        s.resolve_handle(Kind::Gate, 1).unwrap().as_ref(),
        Some(g.iri())
    );
    assert_eq!(s.resolve_handle(Kind::Gate, 2).unwrap(), None);
    assert_eq!(s.resolve_handle(Kind::Finding, 0).unwrap(), None);
}

/// Spec §2.6 / the deletion ruling: ownership is membership, and nothing
/// removes an entry. Every id minted along the way must still be owned at
/// the end.
fn no_operation_gives_up_an_owned_id<S: Catalog + Tracker + Ledger>(s: &S) {
    let p = s.add_project("/p").unwrap();
    let g = s
        .add_gate(&p, "g", sample_kind(), sample_selector(), 1, "abc", "o")
        .unwrap();
    let r = s.add_record(&p, "t").unwrap();
    let f = s
        .add_finding(Finding::raise(p.clone(), r.clone(), "a", "c"))
        .unwrap();
    let mut def = s.get_gate(&g).unwrap().unwrap();
    def.authored_at_commit = "def".into();
    s.update_gate(&def).unwrap();
    s.set_record_state(&r, State::Doing).unwrap();
    let mut fin = s.get_finding(&f).unwrap().unwrap();
    fin.withdraw("x").unwrap();
    s.update_finding(&fin).unwrap();
    s.append_gate_run(sample_run(g.clone(), "abc", 1)).unwrap();
    assert!(s.get_project(&p).unwrap().is_some());
    assert!(s.get_gate(&g).unwrap().is_some());
    assert!(s.get_record(&r).unwrap().is_some());
    assert!(s.get_finding(&f).unwrap().is_some());
}

/// A well-formed id that no store in these tests ever mints.
fn stranger() -> Iri {
    Iri::parse("urn:uuid:0190a1b2-c3d4-7e5f-8a6b-7c8d9e0f1a2b").unwrap()
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

fn sample_run(gate: GateId, commit: &str, population: u64) -> GateRun {
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
