//! The contract every store must meet, one suite per role (spec §6.1).
//!
//! ⚠ A store gets no suite of its own and no privileged path. `MemStore`,
//! `RedbStore` and, later, a GitHub store all run these same functions. A
//! case that only one store can pass is a defect in that store, not a case to
//! move out of here — with one exception: a case that pins a property only a
//! LOCAL store can have (handles numbered per kind from one) lives in
//! `local_handles`, not in the shared suites, because a GitHub tracker's
//! handles are issue numbers shared by records and findings (spec §2.1).
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

/// Run every case in `cases` against a fresh store from `make`.
///
/// ⚠ `expected` is the number of cases the suite is DECLARED to have, kept
/// beside each list by hand. A floor of "at least one case" would fail only
/// if every case were deleted; an exact count fails as soon as one is. If
/// you add or remove a case on purpose, update the count beside that list
/// in the same change.
fn run_suite<S, G>(suite: &str, expected: usize, cases: &[fn(&S)], make: impl Fn() -> (S, G)) {
    assert_eq!(
        cases.len(),
        expected,
        "the {suite} suite lists {} cases but declares {expected}. A case was added or \
         removed: if that was deliberate, update the count beside the list; if not, \
         restore the case",
        cases.len()
    );
    for case in cases {
        let (s, _guard) = make();
        case(&s);
    }
}

/// The roles a case uses, each bound to the store that backs it (spec
/// §1.3 of the GitHub tracker design). A local store binds itself to every
/// role; a split binding — a GitHub tracker over a local catalog and ledger
/// — binds each to its own. `handles` answers for every kind, routed to
/// whichever store holds that kind.
pub struct Bound<'a> {
    pub catalog: &'a dyn Catalog,
    pub tracker: &'a dyn Tracker,
    pub ledger: &'a dyn Ledger,
    pub handles: &'a dyn Handles,
}

/// Something that can hand out a [`Bound`] for one case. Owns its stores
/// and any guard (a temp directory, a fake server) for the case's lifetime.
pub trait Fixture {
    fn bound(&self) -> Bound<'_>;
}

/// One store backing every role, plus a guard to keep alive.
pub struct Single<S, G>(pub S, pub G);

impl<S: Catalog + Tracker + Ledger + Handles, G> Fixture for Single<S, G> {
    fn bound(&self) -> Bound<'_> {
        Bound {
            catalog: &self.0,
            tracker: &self.0,
            ledger: &self.0,
            handles: &self.0,
        }
    }
}

/// [`run_suite`] for cases written against a [`Bound`].
fn run_bound<F: Fixture>(
    suite: &str,
    expected: usize,
    cases: &[fn(&Bound<'_>)],
    make: impl Fn() -> F,
) {
    assert_eq!(
        cases.len(),
        expected,
        "the {suite} suite lists {} cases but declares {expected}. A case was added or \
         removed: if that was deliberate, update the count beside the list; if not, \
         restore the case",
        cases.len()
    );
    for case in cases {
        let fixture = make();
        case(&fixture.bound());
    }
}

/// How many cases [`catalog`] runs. Update deliberately — see [`run_suite`].
const CATALOG_CASES: usize = 3;
/// How many cases [`tracker`] runs. Update deliberately — see [`run_suite`].
const TRACKER_CASES: usize = 12;
/// How many cases [`ledger`] runs. Update deliberately — see [`run_suite`].
const LEDGER_CASES: usize = 1;
/// How many cases [`all_roles`] runs. Update deliberately — see [`run_suite`].
const ALL_ROLES_CASES: usize = 7;
/// How many cases [`local_handles`] runs. Update deliberately — see [`run_suite`].
const LOCAL_HANDLES_CASES: usize = 1;

pub fn catalog<S: Catalog, G>(make: impl Fn() -> (S, G)) {
    let cases: &[fn(&S)] = &[
        a_project_round_trips::<S>,
        affirming_a_gate_moves_only_its_stamp::<S>,
        list_transitions_returns_every_transition_of_one_project_and_no_others::<S>,
    ];
    run_suite("catalog", CATALOG_CASES, cases, make);
}

pub fn tracker<F: Fixture>(make: impl Fn() -> F) {
    let cases: &[fn(&Bound<'_>)] = &[
        list_records_returns_only_the_named_projects_records,
        a_record_state_change_is_visible_on_the_next_read,
        a_finding_round_trips_and_gets_a_real_id,
        withdrawals_are_counted_against_whoever_raised_the_finding,
        findings_are_listed_per_project,
        an_alias_reaches_the_item_it_names,
        an_alias_already_in_use_is_refused_and_names_it,
        an_alias_on_a_finding_reaches_it,
        add_alias_resolves_through_an_existing_alias_to_the_true_primary,
        set_record_state_and_update_finding_through_an_alias_touch_the_primary_once,
        a_finding_raised_against_a_record_alias_stores_the_primary,
        update_finding_keeps_the_stored_aliases_whatever_the_caller_holds,
    ];
    run_bound("tracker", TRACKER_CASES, cases, make);
}

pub fn ledger<S: Catalog + Ledger, G>(make: impl Fn() -> (S, G)) {
    let cases: &[fn(&S)] = &[the_logs_are_append_only_and_read_back_in_order::<S>];
    run_suite("ledger", LEDGER_CASES, cases, make);
}

/// Cases over every role at once, including ownership and handles.
pub fn all_roles<F: Fixture>(make: impl Fn() -> F) {
    let cases: &[fn(&Bound<'_>)] = &[
        an_id_this_store_never_held_is_not_owned_rather_than_absent,
        a_list_over_a_project_this_store_never_held_is_refused_not_empty,
        a_finding_on_a_record_this_store_never_held_is_refused,
        an_id_of_another_kind_is_owned_but_not_found,
        an_id_of_another_kind_where_a_project_or_record_is_needed_is_refused_as_the_wrong_kind,
        an_id_has_no_handle_under_any_kind_but_its_own,
        no_operation_gives_up_an_owned_id,
    ];
    run_bound("all-roles", ALL_ROLES_CASES, cases, make);
}

/// A property of the LOCAL stores, not of the `Handles` contract: handles
/// are numbered per kind from one. A GitHub tracker's handles are issue
/// numbers shared by records and findings (spec §2.1), so this case is not
/// part of the shared contract (spec §8.1 item 3).
pub fn local_handles<S: Catalog + Tracker + Ledger + Handles, G>(make: impl Fn() -> (S, G)) {
    let cases: &[fn(&S)] = &[handles_are_per_kind_and_start_at_one::<S>];
    run_suite("local-handles", LOCAL_HANDLES_CASES, cases, make);
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

fn list_records_returns_only_the_named_projects_records(roles: &Bound<'_>) {
    let p1 = roles.catalog.add_project("/tmp/p").unwrap();
    let p2 = roles.catalog.add_project("/tmp/q").unwrap();
    let r1 = roles.tracker.add_record(&p1, "fix the thing").unwrap();
    let _r2 = roles.tracker.add_record(&p2, "unrelated").unwrap();
    let recs = roles.tracker.list_records(&p1).unwrap();
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].id, r1);
    assert_eq!(recs[0].title, "fix the thing");
}

fn a_record_state_change_is_visible_on_the_next_read(roles: &Bound<'_>) {
    let p = roles.catalog.add_project("/tmp/p").unwrap();
    let r = roles.tracker.add_record(&p, "fix the thing").unwrap();
    roles.tracker.set_record_state(&r, State::Doing).unwrap();
    assert_eq!(
        roles.tracker.get_record(&r).unwrap().unwrap().state,
        State::Doing
    );
}

fn a_finding_round_trips_and_gets_a_real_id(roles: &Bound<'_>) {
    let p = roles.catalog.add_project("/p").unwrap();
    let r = roles.tracker.add_record(&p, "t").unwrap();
    let id = roles
        .tracker
        .add_finding(Finding::raise(p, r, "reviewer", "wrong on empty"))
        .unwrap();
    assert_ne!(
        id,
        FindingId(seq_iri(0)),
        "the store must replace the placeholder id"
    );
    let back = roles.tracker.get_finding(&id).unwrap().unwrap();
    assert_eq!(back.id, id);
    assert_eq!(back.state, FindingState::Raised);
}

fn withdrawals_are_counted_against_whoever_raised_the_finding(roles: &Bound<'_>) {
    let p = roles.catalog.add_project("/p").unwrap();
    let r = roles.tracker.add_record(&p, "t").unwrap();

    for claim in ["a", "b"] {
        let id = roles
            .tracker
            .add_finding(Finding::raise(p.clone(), r.clone(), "hasty", claim))
            .unwrap();
        let mut f = roles.tracker.get_finding(&id).unwrap().unwrap();
        f.withdraw("not concrete").unwrap();
        roles.tracker.update_finding(&f).unwrap();
    }
    let id = roles
        .tracker
        .add_finding(Finding::raise(p.clone(), r.clone(), "careful", "c"))
        .unwrap();
    let mut f = roles.tracker.get_finding(&id).unwrap().unwrap();
    let gate = roles
        .catalog
        .add_gate(&p, "g", sample_kind(), sample_selector(), 1, "abc", "owner")
        .unwrap();
    f.attach_reproduction(gate).unwrap();
    roles.tracker.update_finding(&f).unwrap();

    assert_eq!(roles.tracker.withdrawals_by("hasty").unwrap(), 2);
    assert_eq!(roles.tracker.withdrawals_by("careful").unwrap(), 0);
    assert_eq!(roles.tracker.withdrawals_by("nobody").unwrap(), 0);
}

fn findings_are_listed_per_project(roles: &Bound<'_>) {
    let a = roles.catalog.add_project("/a").unwrap();
    let b = roles.catalog.add_project("/b").unwrap();
    let ra = roles.tracker.add_record(&a, "t").unwrap();
    let rb = roles.tracker.add_record(&b, "t").unwrap();
    roles
        .tracker
        .add_finding(Finding::raise(a.clone(), ra, "r", "one"))
        .unwrap();
    roles
        .tracker
        .add_finding(Finding::raise(b, rb, "r", "two"))
        .unwrap();
    assert_eq!(roles.tracker.list_findings(&a).unwrap().len(), 1);
}

pub fn an_alias_reaches_the_item_it_names(roles: &Bound<'_>) {
    let p = roles.catalog.add_project("/p").unwrap();
    let r = roles.tracker.add_record(&p, "t").unwrap();
    let old = Iri::parse("https://github.com/o/r/issues/41").unwrap();
    roles.tracker.add_alias(r.iri(), old.clone()).unwrap();
    let via_alias = roles
        .tracker
        .get_record(&RecordId(old.clone()))
        .unwrap()
        .unwrap();
    assert_eq!(via_alias.id, r);
    assert!(via_alias.also_known_as.contains(&old));
}

pub fn an_alias_already_in_use_is_refused_and_names_it(roles: &Bound<'_>) {
    let p = roles.catalog.add_project("/p").unwrap();
    let a = roles.tracker.add_record(&p, "a").unwrap();
    let b = roles.tracker.add_record(&p, "b").unwrap();
    let old = Iri::parse("https://github.com/o/r/issues/41").unwrap();
    roles.tracker.add_alias(a.iri(), old.clone()).unwrap();
    let err = roles.tracker.add_alias(b.iri(), old.clone()).unwrap_err();
    assert!(
        matches!(err, StoreError::AlreadyExists(ref i) if *i == old),
        "{err:?}"
    );
    // An existing primary id cannot become an alias either.
    let err = roles.tracker.add_alias(b.iri(), a.0.clone()).unwrap_err();
    assert!(matches!(err, StoreError::AlreadyExists(_)), "{err:?}");
}

/// Aliases are not record-only: a `Finding` can carry one too (spec §2.5
/// makes no distinction between the two aliasable kinds).
pub fn an_alias_on_a_finding_reaches_it(roles: &Bound<'_>) {
    let p = roles.catalog.add_project("/p").unwrap();
    let r = roles.tracker.add_record(&p, "t").unwrap();
    let f = roles
        .tracker
        .add_finding(Finding::raise(p, r, "reviewer", "claim"))
        .unwrap();
    let old = Iri::parse("https://github.com/o/r/issues/7").unwrap();
    roles.tracker.add_alias(f.iri(), old.clone()).unwrap();
    let via_alias = roles
        .tracker
        .get_finding(&FindingId(old.clone()))
        .unwrap()
        .unwrap();
    assert_eq!(via_alias.id, f);
    assert!(via_alias.also_known_as.contains(&old));
}

/// `add_alias`'s `primary` argument may itself already be an alias. This
/// pins the one-hop resolution: the new alias reaches the item through
/// `first`, but `ALIASES` never chains — the item ends up with BOTH names in
/// its own `also_known_as`, addressed by the one true primary.
pub fn add_alias_resolves_through_an_existing_alias_to_the_true_primary(roles: &Bound<'_>) {
    let p = roles.catalog.add_project("/p").unwrap();
    let r = roles.tracker.add_record(&p, "t").unwrap();
    let first = Iri::parse("https://github.com/o/r/issues/8").unwrap();
    let second = Iri::parse("https://github.com/o/r/issues/9").unwrap();
    roles.tracker.add_alias(r.iri(), first.clone()).unwrap();
    roles.tracker.add_alias(&first, second.clone()).unwrap();

    let via_second = roles
        .tracker
        .get_record(&RecordId(second.clone()))
        .unwrap()
        .unwrap();
    assert_eq!(
        via_second.id, r,
        "second hop still reaches the true primary"
    );
    assert!(via_second.also_known_as.contains(&first));
    assert!(via_second.also_known_as.contains(&second));
}

/// Fix round 1, item 1: `set_record_state` and `update_finding` both take an
/// id/struct the caller may have addressed by alias. Either must land on —
/// and stay keyed by — the primary: no phantom second row, no double count.
pub fn set_record_state_and_update_finding_through_an_alias_touch_the_primary_once(
    roles: &Bound<'_>,
) {
    let p = roles.catalog.add_project("/p").unwrap();

    let r = roles.tracker.add_record(&p, "t").unwrap();
    let r_alias = Iri::parse("https://github.com/o/r/issues/10").unwrap();
    roles.tracker.add_alias(r.iri(), r_alias.clone()).unwrap();
    roles
        .tracker
        .set_record_state(&RecordId(r_alias), State::Doing)
        .unwrap();
    assert_eq!(
        roles.tracker.get_record(&r).unwrap().unwrap().state,
        State::Doing
    );
    assert_eq!(
        roles.tracker.list_records(&p).unwrap().len(),
        1,
        "no phantom row under the alias"
    );

    let fid = roles
        .tracker
        .add_finding(Finding::raise(p.clone(), r.clone(), "reviewer", "claim"))
        .unwrap();
    let f_alias = Iri::parse("https://github.com/o/r/issues/11").unwrap();
    roles.tracker.add_alias(fid.iri(), f_alias.clone()).unwrap();
    let mut f = roles
        .tracker
        .get_finding(&FindingId(f_alias.clone()))
        .unwrap()
        .unwrap();
    assert_eq!(
        f.id, fid,
        "get_finding via an alias already answers with the primary id"
    );
    // A caller that still addresses the update by the alias it looked the
    // finding up with, rather than the primary `get_finding` returned.
    f.id = FindingId(f_alias);
    f.withdraw("closing").unwrap();
    roles.tracker.update_finding(&f).unwrap();

    let back = roles.tracker.get_finding(&fid).unwrap().unwrap();
    assert_eq!(back.id, fid, "the stored item's id is always the primary");
    assert_eq!(back.state, FindingState::Withdrawn);
    assert_eq!(
        roles.tracker.list_findings(&p).unwrap().len(),
        1,
        "no phantom row under the alias"
    );
    assert_eq!(
        roles.tracker.withdrawals_by("reviewer").unwrap(),
        1,
        "counted once, not once per row"
    );
}

/// Fix round 1, item 6: `add_finding` must store the referenced record's
/// PRIMARY id, never whatever alias the caller happened to raise against
/// (e.g. the CLI stores exactly what the user typed) — otherwise two
/// findings against "the same" record could disagree on which IRI names it.
pub fn a_finding_raised_against_a_record_alias_stores_the_primary(roles: &Bound<'_>) {
    let p = roles.catalog.add_project("/p").unwrap();
    let r = roles.tracker.add_record(&p, "t").unwrap();
    let alias = Iri::parse("https://github.com/o/r/issues/12").unwrap();
    roles.tracker.add_alias(r.iri(), alias.clone()).unwrap();

    let fid = roles
        .tracker
        .add_finding(Finding::raise(p, RecordId(alias), "reviewer", "claim"))
        .unwrap();
    let f = roles.tracker.get_finding(&fid).unwrap().unwrap();
    assert_eq!(f.record, r, "the finding stores the record's primary id");
}

/// Final review, item 12: `update_finding` keeps the STORED
/// `also_known_as` and ignores the caller's. A caller holding a copy read
/// before an `add_alias` must not erase that alias from the item while the
/// alias index still resolves it; a caller that edits the list must not add
/// a name the index cannot resolve.
pub fn update_finding_keeps_the_stored_aliases_whatever_the_caller_holds(roles: &Bound<'_>) {
    let p = roles.catalog.add_project("/p").unwrap();
    let r = roles.tracker.add_record(&p, "t").unwrap();
    let fid = roles
        .tracker
        .add_finding(Finding::raise(p, r, "reviewer", "claim"))
        .unwrap();

    // Read BEFORE the alias exists: this copy's `also_known_as` is empty.
    let mut stale = roles.tracker.get_finding(&fid).unwrap().unwrap();
    let alias = Iri::parse("https://github.com/o/r/issues/13").unwrap();
    roles.tracker.add_alias(fid.iri(), alias.clone()).unwrap();

    stale.withdraw("closing").unwrap();
    let invented = Iri::parse("https://github.com/o/r/issues/14").unwrap();
    stale.also_known_as.push(invented.clone());
    roles.tracker.update_finding(&stale).unwrap();

    let back = roles.tracker.get_finding(&fid).unwrap().unwrap();
    assert_eq!(
        back.state,
        FindingState::Withdrawn,
        "the caller's other fields are written"
    );
    assert_eq!(
        back.also_known_as,
        vec![alias.clone()],
        "the stored aliases are kept and the caller's list is ignored"
    );
    assert_eq!(
        roles
            .tracker
            .get_finding(&FindingId(alias))
            .unwrap()
            .unwrap()
            .id,
        fid,
        "the alias still resolves"
    );
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
fn an_id_this_store_never_held_is_not_owned_rather_than_absent(roles: &Bound<'_>) {
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
            ("get_project", roles.catalog.get_project(&p).map(drop)),
            ("get_gate", roles.catalog.get_gate(&g).map(drop)),
            ("get_record", roles.tracker.get_record(&r).map(drop)),
            ("get_finding", roles.tracker.get_finding(&f).map(drop)),
            (
                "get_transition",
                roles.catalog.get_transition(&p, "launch").map(drop),
            ),
            ("gate_runs", roles.ledger.gate_runs(&g).map(drop)),
            ("update_gate", roles.catalog.update_gate(&gate_def)),
            ("update_finding", roles.tracker.update_finding(&finding)),
            (
                "set_record_state",
                roles.tracker.set_record_state(&r, State::Doing),
            ),
            (
                "add_gate",
                roles
                    .catalog
                    .add_gate(&p, "g", sample_kind(), sample_selector(), 1, "abc", "o")
                    .map(drop),
            ),
            ("add_record", roles.tracker.add_record(&p, "t").map(drop)),
            (
                "add_finding",
                roles.tracker.add_finding(finding.clone()).map(drop),
            ),
            (
                "add_transition",
                roles.catalog.add_transition(Transition {
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

fn a_list_over_a_project_this_store_never_held_is_refused_not_empty(roles: &Bound<'_>) {
    let id = stranger();
    let p = ProjectId(id.clone());
    assert_all_not_owned(
        &id,
        vec![
            ("list_gates", roles.catalog.list_gates(&p).map(drop)),
            (
                "list_transitions",
                roles.catalog.list_transitions(&p).map(drop),
            ),
            ("list_records", roles.tracker.list_records(&p).map(drop)),
            ("list_findings", roles.tracker.list_findings(&p).map(drop)),
            ("attempts", roles.ledger.attempts(&p).map(drop)),
        ],
    );
}

// The project is held, so only the record check can refuse this. A store
// that checked the project alone would store a finding about a record it
// never held.
fn a_finding_on_a_record_this_store_never_held_is_refused(roles: &Bound<'_>) {
    let p = roles.catalog.add_project("/p").unwrap();
    let id = stranger();
    let finding = Finding::raise(p.clone(), RecordId(id.clone()), "a", "c");
    assert_all_not_owned(
        &id,
        vec![("add_finding", roles.tracker.add_finding(finding).map(drop))],
    );
    assert!(
        roles.tracker.list_findings(&p).unwrap().is_empty(),
        "nothing may be stored"
    );
}

fn an_id_of_another_kind_is_owned_but_not_found(roles: &Bound<'_>) {
    let p = roles.catalog.add_project("/p").unwrap();
    let as_gate = GateId(p.0.clone());
    assert_eq!(roles.catalog.get_gate(&as_gate).unwrap(), None);
}

/// Asserts that every result is `WrongKind` for `id`, naming `expected` and
/// `found`. Each result is labelled with the method that produced it, so a
/// failure says which method answered for an item of the wrong kind.
fn assert_all_wrong_kind(
    id: &Iri,
    expected: Kind,
    found: Kind,
    results: Vec<(&str, Result<(), StoreError>)>,
) {
    assert!(
        !results.is_empty(),
        "no methods were asked, so nothing was checked"
    );
    for (method, result) in results {
        match result {
            Err(StoreError::WrongKind {
                id: ref got,
                expected: e,
                found: f,
            }) if got == id && e == expected && f == found => {}
            other => panic!(
                "{method} answered {other:?} for a {} passed as a {}; it must be WrongKind",
                found.as_wire(),
                expected.as_wire()
            ),
        }
    }
}

// Final review, item 1: an id this store DOES hold, but as another kind,
// passed where a project (or `add_finding`'s record) is needed. An empty
// list would claim the store looked at a project and found nothing in it;
// `NotOwned` would claim the store never held the id. Both are false, so
// every project-taking method must answer `WrongKind`, and act on nothing.
fn an_id_of_another_kind_where_a_project_or_record_is_needed_is_refused_as_the_wrong_kind(
    roles: &Bound<'_>,
) {
    let p = roles.catalog.add_project("/p").unwrap();
    let g = roles
        .catalog
        .add_gate(&p, "g", sample_kind(), sample_selector(), 1, "abc", "o")
        .unwrap();
    let r = roles.tracker.add_record(&p, "t").unwrap();
    let as_project = ProjectId(g.0.clone());
    assert_all_wrong_kind(
        g.iri(),
        Kind::Project,
        Kind::Gate,
        vec![
            (
                "list_gates",
                roles.catalog.list_gates(&as_project).map(drop),
            ),
            (
                "list_transitions",
                roles.catalog.list_transitions(&as_project).map(drop),
            ),
            (
                "list_records",
                roles.tracker.list_records(&as_project).map(drop),
            ),
            (
                "list_findings",
                roles.tracker.list_findings(&as_project).map(drop),
            ),
            ("attempts", roles.ledger.attempts(&as_project).map(drop)),
            (
                "get_transition",
                roles
                    .catalog
                    .get_transition(&as_project, "launch")
                    .map(drop),
            ),
            (
                "add_gate",
                roles
                    .catalog
                    .add_gate(
                        &as_project,
                        "g",
                        sample_kind(),
                        sample_selector(),
                        1,
                        "abc",
                        "o",
                    )
                    .map(drop),
            ),
            (
                "add_transition",
                roles.catalog.add_transition(Transition {
                    project: as_project.clone(),
                    name: "launch".into(),
                    from: State::Review,
                    to: State::Done,
                    regret: Regret::High,
                    gates: vec![],
                }),
            ),
            (
                "add_record",
                roles.tracker.add_record(&as_project, "t").map(drop),
            ),
            (
                "add_finding (project)",
                roles
                    .tracker
                    .add_finding(Finding::raise(as_project.clone(), r.clone(), "a", "c"))
                    .map(drop),
            ),
        ],
    );
    // The project is right, so only the record's kind can refuse this.
    assert_all_wrong_kind(
        g.iri(),
        Kind::Record,
        Kind::Gate,
        vec![(
            "add_finding (record)",
            roles
                .tracker
                .add_finding(Finding::raise(p.clone(), RecordId(g.0.clone()), "a", "c"))
                .map(drop),
        )],
    );
    // Nothing was minted by any refused `add_*`: each kind's next handle is
    // still free.
    assert_eq!(roles.handles.resolve_handle(Kind::Gate, 2).unwrap(), None);
    assert_eq!(roles.handles.resolve_handle(Kind::Record, 2).unwrap(), None);
    assert_eq!(
        roles.handles.resolve_handle(Kind::Finding, 1).unwrap(),
        None
    );
    assert!(roles.catalog.list_transitions(&p).unwrap().is_empty());
}

fn handles_are_per_kind_and_start_at_one<S: Catalog + Tracker + Ledger + Handles>(s: &S) {
    let p = s.add_project("/p").unwrap();
    let g = s
        .add_gate(&p, "g", sample_kind(), sample_selector(), 1, "abc", "o")
        .unwrap();
    let r = s.add_record(&p, "t").unwrap();
    let f = s
        .add_finding(Finding::raise(p.clone(), r.clone(), "a", "c"))
        .unwrap();
    assert_eq!(s.handle_of(Kind::Finding, f.iri()).unwrap(), Some(1));
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

// Final review, item 11: `refs::show` prints a handle only when
// `handle_of(kind, id)` answers `Some`, so an id held under another kind must
// answer `None` — never the handle it has under its OWN kind. A store that
// ignored `kind` would answer `Some` in every row that must be `None`.
fn an_id_has_no_handle_under_any_kind_but_its_own(roles: &Bound<'_>) {
    let p = roles.catalog.add_project("/p").unwrap();
    let g = roles
        .catalog
        .add_gate(&p, "g", sample_kind(), sample_selector(), 1, "abc", "o")
        .unwrap();
    let r = roles.tracker.add_record(&p, "t").unwrap();
    let f = roles
        .tracker
        .add_finding(Finding::raise(p.clone(), r.clone(), "a", "c"))
        .unwrap();
    let held = [
        (Kind::Project, p.0),
        (Kind::Gate, g.0),
        (Kind::Record, r.0),
        (Kind::Finding, f.0),
    ];
    for own in Kind::ALL {
        assert!(
            held.iter().any(|(k, _)| k == own),
            "no id of kind {} was minted, so its row was never checked",
            own.as_wire()
        );
    }
    for (own, id) in &held {
        let h = roles
            .handles
            .handle_of(*own, id)
            .unwrap()
            .unwrap_or_else(|| panic!("{id} has no handle as its own kind"));
        assert_eq!(
            roles.handles.resolve_handle(*own, h).unwrap().as_ref(),
            Some(id),
            "{id}'s handle resolves back to it"
        );
        for other in Kind::ALL.iter().filter(|k| *k != own) {
            assert_eq!(
                roles.handles.handle_of(*other, id).unwrap(),
                None,
                "a {} id has no {} handle",
                own.as_wire(),
                other.as_wire()
            );
        }
    }
}

/// Spec §2.6 / the deletion ruling: ownership is membership, and nothing
/// removes an entry. Every id minted along the way must still be owned at
/// the end.
fn no_operation_gives_up_an_owned_id(roles: &Bound<'_>) {
    let p = roles.catalog.add_project("/p").unwrap();
    let g = roles
        .catalog
        .add_gate(&p, "g", sample_kind(), sample_selector(), 1, "abc", "o")
        .unwrap();
    let r = roles.tracker.add_record(&p, "t").unwrap();
    let f = roles
        .tracker
        .add_finding(Finding::raise(p.clone(), r.clone(), "a", "c"))
        .unwrap();
    let mut def = roles.catalog.get_gate(&g).unwrap().unwrap();
    def.authored_at_commit = "def".into();
    roles.catalog.update_gate(&def).unwrap();
    roles.tracker.set_record_state(&r, State::Doing).unwrap();
    let mut fin = roles.tracker.get_finding(&f).unwrap().unwrap();
    fin.withdraw("x").unwrap();
    roles.tracker.update_finding(&fin).unwrap();
    roles
        .ledger
        .append_gate_run(sample_run(g.clone(), "abc", 1))
        .unwrap();
    assert!(roles.catalog.get_project(&p).unwrap().is_some());
    assert!(roles.catalog.get_gate(&g).unwrap().is_some());
    assert!(roles.tracker.get_record(&r).unwrap().is_some());
    assert!(roles.tracker.get_finding(&f).unwrap().is_some());
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
