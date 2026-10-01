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

use crate::at::At;
use crate::decision::{Decision, LeftLocal, Outcome, TransitionOutcome};
use crate::fault::LedgerFault;
use crate::finding::{Finding, FindingState};
use crate::ids::{FindingId, GateId, Kind, ProjectId, RecordId, seq_iri};
use crate::iri::Iri;
use crate::log::{Attempt, AttemptStatus, GateRun, PathsTouched};
use crate::model::{
    CommandSpec, GateDef, GateKind, PopulationDelivery, Regret, Selector, State, Transition,
};
use crate::split::{Batch, CachedSegment, LedgerCache, Outbox, RemoteLedger, SplitLedger};
use crate::store::{Catalog, Handles, Ledger, StoreError, Tracker};
use crate::verdict::Verdict;
use std::cell::RefCell;
use std::collections::BTreeSet;

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

/// Hands one case its `Bound`. A callback rather than a return value, so a
/// fixture can build adapters that live only for the call — a split
/// binding's `CatalogChecked` borrows two stores the fixture owns.
pub trait Fixture {
    fn with(&self, f: &mut dyn FnMut(&Bound<'_>));
}

/// One store backing every role, plus a guard to keep alive.
pub struct Single<S, G>(pub S, pub G);

impl<S: Catalog + Tracker + Ledger + Handles, G> Fixture for Single<S, G> {
    fn with(&self, f: &mut dyn FnMut(&Bound<'_>)) {
        f(&Bound {
            catalog: &self.0,
            tracker: &self.0,
            ledger: &self.0,
            handles: &self.0,
        });
    }
}

/// A split ledger to test, with the means to drive its GitHub side.
pub trait SplitFixture {
    fn with_split(&self, f: &mut dyn FnMut(&Bound<'_>, &dyn RemoteControl));
}

/// `SplitLedger` over a local store `S` and a [`MemRemote`] for `R_1`,
/// whose ledger was switched on (a cut-over before every sample entry). A
/// [`Fixture`] too, so the shared ledger suite runs over it. Plan B's
/// fixture swaps `MemRemote` for `GithubLedger` over the fake GitHub.
pub struct SplitOver<S, G>(pub S, pub G);

impl<S: Catalog + Tracker + Ledger + Handles + Outbox, G> SplitFixture for SplitOver<S, G> {
    fn with_split(&self, f: &mut dyn FnMut(&Bound<'_>, &dyn RemoteControl)) {
        self.0
            .set_cutover("R_1", &entry_iri(0))
            .expect("a fresh store records a cut-over");
        let remote = MemRemote::new("R_1");
        let split = SplitLedger {
            local: &self.0,
            github: &remote,
        };
        f(
            &Bound {
                catalog: &self.0,
                tracker: &self.0,
                ledger: &split,
                handles: &self.0,
            },
            &remote,
        );
    }
}

impl<S: Catalog + Tracker + Ledger + Handles + Outbox, G> Fixture for SplitOver<S, G> {
    fn with(&self, f: &mut dyn FnMut(&Bound<'_>)) {
        self.with_split(&mut |b, _| f(b));
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
        fixture.with(&mut |b| case(b));
    }
}

/// How many cases [`catalog`] runs. Update deliberately — see [`run_suite`].
const CATALOG_CASES: usize = 3;
/// How many cases [`tracker`] runs. Update deliberately — see [`run_suite`].
const TRACKER_CASES: usize = 12;
/// How many cases [`ledger`] runs. Update deliberately — see [`run_suite`].
const LEDGER_CASES: usize = 4;
/// How many cases [`split_ledger`] runs. Update deliberately — see [`run_suite`].
const SPLIT_LEDGER_CASES: usize = 10;
/// How many cases [`all_roles`] runs. Update deliberately — see [`run_suite`].
const ALL_ROLES_CASES: usize = 7;
/// How many cases [`local_handles`] runs. Update deliberately — see [`run_suite`].
const LOCAL_HANDLES_CASES: usize = 1;
/// How many cases [`ledger_cache`] runs. Update deliberately — see [`run_suite`].
const LEDGER_CACHE_CASES: usize = 3;

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

/// The contract every `Ledger` meets: the local stores, and a split ledger
/// over them. Bound-based, so a split binding runs it too.
pub fn ledger<F: Fixture>(make: impl Fn() -> F) {
    let cases: &[fn(&Bound<'_>)] = &[
        the_logs_are_append_only_and_read_back_in_order,
        attempts_are_read_back_per_project,
        a_flush_with_nothing_waiting_still_answers,
        a_run_tied_to_a_record_reads_back_once_after_its_flush,
    ];
    run_bound("ledger", LEDGER_CASES, cases, make);
}

/// What only a split ledger has (GitHub ledger spec §8.2, §8.3): runs tied
/// to a record, the flush, merge by id, the local store succeeding while
/// GitHub fails, a lost answer retried without a duplicate, another
/// repository's entry skipped without blocking, and `NotOwned` from the
/// local catalog.
pub fn split_ledger<F: SplitFixture>(make: impl Fn() -> F) {
    let cases: &[fn(&Bound<'_>, &dyn RemoteControl)] = &[
        runs_tied_to_a_record_reach_github_at_the_flush_and_not_before,
        a_run_tied_to_no_record_is_never_published,
        a_second_flush_publishes_nothing_twice_and_reads_see_each_run_once,
        while_github_is_down_appends_succeed_and_flushes_and_reads_are_refused_until_it_returns,
        another_machines_run_is_read_back_beside_this_ones,
        a_gate_the_local_catalog_never_held_is_not_owned_whatever_github_holds,
        a_commit_whose_answer_was_lost_is_not_duplicated_by_the_next_flush,
        a_pending_entry_of_another_repository_does_not_block_the_decision,
        a_published_entry_is_never_offered_to_github_again,
        a_skipped_entry_is_reported_by_one_flush_only,
    ];
    assert_eq!(
        cases.len(),
        SPLIT_LEDGER_CASES,
        "the split-ledger suite lists {} cases but declares {SPLIT_LEDGER_CASES}. A case was \
         added or removed: if that was deliberate, update the count beside the list; if not, \
         restore the case",
        cases.len()
    );
    for case in cases {
        let fixture = make();
        fixture.with_split(&mut |b, c| case(b, c));
    }
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

/// What a local store remembers of a GitHub ledger, per repository `node_id`
/// (GitHub ledger spec §3.2 step 6, §3.3, §3.5 checks 2–4).
pub fn ledger_cache<S: LedgerCache, G>(make: impl Fn() -> (S, G)) {
    let cases: &[fn(&S)] = &[
        the_last_head_is_kept_per_repository_and_replaced::<S>,
        a_cached_file_reads_back_by_path_and_by_its_own_directory_only::<S>,
        a_segment_at_the_same_path_in_another_repository_is_never_returned::<S>,
    ];
    run_suite("ledger-cache", LEDGER_CACHE_CASES, cases, make);
}

fn the_last_head_is_kept_per_repository_and_replaced<S: LedgerCache>(s: &S) {
    assert_eq!(s.last_head("R_1").unwrap(), None);
    s.set_last_head("R_1", "c1").unwrap();
    s.set_last_head("R_1", "c2").unwrap();
    assert_eq!(s.last_head("R_1").unwrap().as_deref(), Some("c2"));
    assert_eq!(s.last_head("R_2").unwrap(), None);
}

fn a_cached_file_reads_back_by_path_and_by_its_own_directory_only<S: LedgerCache>(s: &S) {
    let seg = |oid: &str, closed: bool| CachedSegment {
        oid: oid.into(),
        text: format!("{oid}\n"),
        closed,
    };
    assert_eq!(s.cached("R_1", "runs/aa/1.jsonl").unwrap(), None);
    assert!(s.cached_under("R_1", "runs/aa").unwrap().is_empty());
    s.cache("R_1", "runs/aa/1.jsonl", &seg("o1", true)).unwrap();
    s.cache("R_1", "runs/aa/2.jsonl", &seg("o2", false))
        .unwrap();
    s.cache("R_1", "runs/aab/1.jsonl", &seg("o3", false))
        .unwrap();
    s.cache("R_2", "runs/aa/1.jsonl", &seg("o4", false))
        .unwrap();
    s.cache("R_1", "runs/aa/2.jsonl", &seg("o5", true)).unwrap();
    assert_eq!(
        s.cached("R_1", "runs/aa/1.jsonl").unwrap(),
        Some(seg("o1", true))
    );
    assert_eq!(
        s.cached("R_1", "runs/aa/2.jsonl").unwrap(),
        Some(seg("o5", true)),
        "a second write replaces the first"
    );
    let under: Vec<(String, String)> = s
        .cached_under("R_1", "runs/aa")
        .unwrap()
        .into_iter()
        .map(|(p, c)| (p, c.oid))
        .collect();
    assert_eq!(
        under,
        vec![
            ("runs/aa/1.jsonl".to_string(), "o1".to_string()),
            ("runs/aa/2.jsonl".to_string(), "o5".to_string()),
        ],
        "not `runs/aab`, and not another repository's"
    );
}

/// ⚠ The case above's `runs/aab` row (meant to probe the DIRECTORY guard)
/// happens to sort, as a `(repo, path)` key, between `R_1`'s matching rows
/// and `R_2`'s — so a store that walks a range and stops at the first path
/// mismatch never even reaches `R_2`'s row there, whether or not it checks
/// the repository. This case uses the SAME path in both repositories, with
/// no other row sorting between them, so nothing but the repository check
/// itself can keep them apart.
fn a_segment_at_the_same_path_in_another_repository_is_never_returned<S: LedgerCache>(s: &S) {
    let seg = |oid: &str| CachedSegment {
        oid: oid.into(),
        text: format!("{oid}\n"),
        closed: false,
    };
    s.cache("R_1", "runs/aa/1.jsonl", &seg("o1")).unwrap();
    s.cache("R_2", "runs/aa/1.jsonl", &seg("o2")).unwrap();

    assert_eq!(s.cached("R_1", "runs/aa/1.jsonl").unwrap(), Some(seg("o1")));
    assert_eq!(s.cached("R_2", "runs/aa/1.jsonl").unwrap(), Some(seg("o2")));

    let under_r1: Vec<(String, String)> = s
        .cached_under("R_1", "runs/aa")
        .unwrap()
        .into_iter()
        .map(|(p, c)| (p, c.oid))
        .collect();
    assert_eq!(
        under_r1,
        vec![("runs/aa/1.jsonl".to_string(), "o1".to_string())],
        "R_2's segment at the identical path must not appear in R_1's read"
    );
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

fn the_logs_are_append_only_and_read_back_in_order(roles: &Bound<'_>) {
    let p = roles.catalog.add_project("/tmp/p").unwrap();
    let g = roles
        .catalog
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
    roles
        .ledger
        .append_gate_run(sample_run(g.clone(), "abc", 3))
        .unwrap();
    roles
        .ledger
        .append_gate_run(sample_run(g.clone(), "def", 5))
        .unwrap();
    let runs = roles.ledger.gate_runs(&g).unwrap();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].commit, "abc");
    assert_eq!(runs[1].population, 5);
}

fn attempts_are_read_back_per_project(roles: &Bound<'_>) {
    let p1 = roles.catalog.add_project("/p1").unwrap();
    let p2 = roles.catalog.add_project("/p2").unwrap();
    let r1 = roles.tracker.add_record(&p1, "a").unwrap();
    let r2 = roles.tracker.add_record(&p2, "b").unwrap();
    for a in [
        sample_attempt(1, &p1, &r1),
        sample_attempt(2, &p2, &r2),
        sample_attempt(3, &p1, &r1),
    ] {
        roles.ledger.append_attempt(a).unwrap();
    }
    let ids = |p: &ProjectId| -> Vec<Option<Iri>> {
        roles
            .ledger
            .attempts(p)
            .unwrap()
            .into_iter()
            .map(|a| a.id)
            .collect()
    };
    assert_eq!(ids(&p1), vec![Some(entry_iri(1)), Some(entry_iri(3))]);
    assert_eq!(ids(&p2), vec![Some(entry_iri(2))]);
}

fn a_flush_with_nothing_waiting_still_answers(roles: &Bound<'_>) {
    let p = roles.catalog.add_project("/p").unwrap();
    let r = roles.tracker.add_record(&p, "t").unwrap();
    roles
        .ledger
        .flush(sample_decision(1, &r, vec![]))
        .expect("a flush with nothing waiting is not an error");
}

/// A project with one gate and one record.
fn record_world(roles: &Bound<'_>) -> (ProjectId, GateId, RecordId) {
    let p = roles.catalog.add_project("/p").unwrap();
    let g = roles
        .catalog
        .add_gate(&p, "g", sample_kind(), sample_selector(), 1, "abc", "o")
        .unwrap();
    let r = roles.tracker.add_record(&p, "t").unwrap();
    (p, g, r)
}

fn run_ids(runs: Vec<GateRun>) -> Vec<Option<Iri>> {
    runs.into_iter().map(|r| r.id).collect()
}

fn a_run_tied_to_a_record_reads_back_once_after_its_flush(roles: &Bound<'_>) {
    let (_p, g, r) = record_world(roles);
    let run = sample_record_run(1, &g, Some(&r));
    let id = run.id.clone().unwrap();
    roles.ledger.append_gate_run(run).unwrap();
    roles
        .ledger
        .flush(sample_decision(1, &r, vec![id.clone()]))
        .unwrap();
    assert_eq!(run_ids(roles.ledger.gate_runs(&g).unwrap()), vec![Some(id)]);
}

fn runs_tied_to_a_record_reach_github_at_the_flush_and_not_before(
    roles: &Bound<'_>,
    ctl: &dyn RemoteControl,
) {
    let (_p, g, r) = record_world(roles);
    let run = sample_record_run(1, &g, Some(&r));
    roles.ledger.append_gate_run(run.clone()).unwrap();
    assert!(
        ctl.remote().gate_runs(&g).unwrap().is_empty(),
        "an append publishes nothing"
    );
    roles
        .ledger
        .flush(sample_decision(1, &r, vec![run.id.clone().unwrap()]))
        .unwrap();
    assert_eq!(run_ids(ctl.remote().gate_runs(&g).unwrap()), vec![run.id]);
}

fn a_run_tied_to_no_record_is_never_published(roles: &Bound<'_>, ctl: &dyn RemoteControl) {
    let (_p, g, r) = record_world(roles);
    let untied = sample_record_run(1, &g, None);
    let tied = sample_record_run(2, &g, Some(&r));
    roles.ledger.append_gate_run(untied).unwrap();
    roles.ledger.append_gate_run(tied.clone()).unwrap();
    roles
        .ledger
        .flush(sample_decision(1, &r, vec![tied.id.clone().unwrap()]))
        .unwrap();
    assert_eq!(run_ids(ctl.remote().gate_runs(&g).unwrap()), vec![tied.id]);
}

fn a_second_flush_publishes_nothing_twice_and_reads_see_each_run_once(
    roles: &Bound<'_>,
    ctl: &dyn RemoteControl,
) {
    let (_p, g, r) = record_world(roles);
    let run = sample_record_run(1, &g, Some(&r));
    let id = run.id.clone().unwrap();
    roles.ledger.append_gate_run(run).unwrap();
    roles
        .ledger
        .flush(sample_decision(1, &r, vec![id.clone()]))
        .unwrap();
    // The second decision rests on a run that is already published.
    roles
        .ledger
        .flush(sample_decision(2, &r, vec![id.clone()]))
        .unwrap();
    assert_eq!(
        run_ids(ctl.remote().gate_runs(&g).unwrap()),
        vec![Some(id.clone())]
    );
    assert_eq!(run_ids(roles.ledger.gate_runs(&g).unwrap()), vec![Some(id)]);
}

// ⚠ Spec §2.2 and decision 8: the local store keeps every entry while
// GitHub is down; the decision is refused; reads are errors, not the local
// half; and the next flush publishes what was left behind.
fn while_github_is_down_appends_succeed_and_flushes_and_reads_are_refused_until_it_returns(
    roles: &Bound<'_>,
    ctl: &dyn RemoteControl,
) {
    let (p, g, r) = record_world(roles);
    let run = sample_record_run(1, &g, Some(&r));
    let attempt = sample_attempt(2, &p, &r);
    ctl.set_down(true);
    roles.ledger.append_gate_run(run.clone()).unwrap();
    roles.ledger.append_attempt(attempt.clone()).unwrap();
    assert!(
        roles
            .ledger
            .flush(sample_decision(1, &r, vec![attempt.id.clone().unwrap()]))
            .is_err(),
        "a flush GitHub cannot take refuses the decision"
    );
    assert!(
        roles.ledger.gate_runs(&g).is_err(),
        "unreachable is not empty"
    );
    ctl.set_down(false);
    assert!(ctl.remote().gate_runs(&g).unwrap().is_empty());
    roles
        .ledger
        .flush(sample_decision(2, &r, vec![run.id.clone().unwrap()]))
        .unwrap();
    assert_eq!(run_ids(ctl.remote().gate_runs(&g).unwrap()), vec![run.id]);
    assert_eq!(
        ctl.remote()
            .attempts(&p)
            .unwrap()
            .into_iter()
            .map(|a| a.id)
            .collect::<Vec<_>>(),
        vec![attempt.id],
        "the attempt left behind goes out with the next flush"
    );
}

fn another_machines_run_is_read_back_beside_this_ones(roles: &Bound<'_>, ctl: &dyn RemoteControl) {
    let (_p, g, r) = record_world(roles);
    let mine = sample_record_run(1, &g, Some(&r));
    roles.ledger.append_gate_run(mine.clone()).unwrap();
    let theirs = sample_record_run(2, &g, Some(&r));
    ctl.remote()
        .publish(&Batch {
            decision: sample_decision(9, &r, vec![theirs.id.clone().unwrap()]),
            runs: vec![theirs.clone()],
            attempts: vec![],
        })
        .unwrap();
    assert_eq!(
        run_ids(roles.ledger.gate_runs(&g).unwrap()),
        vec![mine.id, theirs.id]
    );
}

fn a_gate_the_local_catalog_never_held_is_not_owned_whatever_github_holds(
    roles: &Bound<'_>,
    _ctl: &dyn RemoteControl,
) {
    let err = roles.ledger.gate_runs(&GateId(stranger())).unwrap_err();
    assert!(matches!(err, StoreError::NotOwned { .. }), "{err:?}");
}

// ⚠ Spec §3.2 step 5 and §8.3: a commit that landed before its answer was
// lost reads, to fl, as a failure; the retry must not add a second copy.
fn a_commit_whose_answer_was_lost_is_not_duplicated_by_the_next_flush(
    roles: &Bound<'_>,
    ctl: &dyn RemoteControl,
) {
    let (_p, g, r) = record_world(roles);
    let run = sample_record_run(1, &g, Some(&r));
    let id = run.id.clone().unwrap();
    roles.ledger.append_gate_run(run).unwrap();
    ctl.lose_next_answer();
    assert!(
        roles
            .ledger
            .flush(sample_decision(1, &r, vec![id.clone()]))
            .is_err(),
        "an answer that never came is not a landed commit, as far as fl can tell"
    );
    let flushed = roles
        .ledger
        .flush(sample_decision(2, &r, vec![id.clone()]))
        .unwrap();
    assert!(
        flushed.commit.is_some(),
        "the new decision is new: a commit"
    );
    assert_eq!(
        run_ids(ctl.remote().gate_runs(&g).unwrap()),
        vec![Some(id.clone())],
        "the retry added no second copy"
    );
    assert_eq!(run_ids(roles.ledger.gate_runs(&g).unwrap()), vec![Some(id)]);
}

// ⚠ Spec §2.1: skipped and reported, never an error that blocks.
fn a_pending_entry_of_another_repository_does_not_block_the_decision(
    roles: &Bound<'_>,
    ctl: &dyn RemoteControl,
) {
    let (_p, g, r) = record_world(roles);
    let theirs = ctl.foreign_record();
    let mine = sample_record_run(1, &g, Some(&r));
    let other = sample_record_run(2, &g, Some(&theirs));
    roles.ledger.append_gate_run(mine.clone()).unwrap();
    roles.ledger.append_gate_run(other.clone()).unwrap();
    let flushed = roles
        .ledger
        .flush(sample_decision(1, &r, vec![mine.id.clone().unwrap()]))
        .unwrap();
    assert_eq!(
        flushed.left_local,
        vec![LeftLocal::OtherRepository {
            entry: other.id.clone().unwrap(),
            record: theirs,
        }]
    );
    assert_eq!(run_ids(ctl.remote().gate_runs(&g).unwrap()), vec![mine.id]);
}

// ⚠ Spec §2.1, §3.2 step 6: a published entry is marked, so no later flush
// offers it again. Seen in the batches, because GitHub's de-duplication
// would hide a missing mark from every read.
fn a_published_entry_is_never_offered_to_github_again(roles: &Bound<'_>, ctl: &dyn RemoteControl) {
    let (_p, g, r) = record_world(roles);
    let first = sample_record_run(1, &g, Some(&r));
    roles.ledger.append_gate_run(first.clone()).unwrap();
    roles
        .ledger
        .flush(sample_decision(1, &r, vec![first.id.clone().unwrap()]))
        .unwrap();
    let second = sample_record_run(2, &g, Some(&r));
    roles.ledger.append_gate_run(second.clone()).unwrap();
    roles
        .ledger
        .flush(sample_decision(2, &r, vec![second.id.clone().unwrap()]))
        .unwrap();
    let batches = ctl.batches();
    assert_eq!(batches.len(), 2, "one batch per flush");
    assert_eq!(run_ids(batches[0].runs.clone()), vec![first.id]);
    assert_eq!(
        run_ids(batches[1].runs.clone()),
        vec![second.id],
        "the first run was published and is not offered again"
    );
}

// ⚠ Spec §2.1: an entry another repository owns is reported by the flush
// that skips it, and by no later one. A foreign ATTEMPT is skipped the same
// way as a foreign run — covering both guards the attempt branch's own
// `set_aside` push, which a run-only case cannot catch.
fn a_skipped_entry_is_reported_by_one_flush_only(roles: &Bound<'_>, ctl: &dyn RemoteControl) {
    let (p, g, r) = record_world(roles);
    let theirs = ctl.foreign_record();
    let other = sample_record_run(1, &g, Some(&theirs));
    let elsewhere = sample_attempt(2, &p, &theirs);
    roles.ledger.append_gate_run(other).unwrap();
    roles.ledger.append_attempt(elsewhere).unwrap();
    let first = roles.ledger.flush(sample_decision(1, &r, vec![])).unwrap();
    assert_eq!(first.left_local.len(), 2, "{first:?}");
    let second = roles.ledger.flush(sample_decision(2, &r, vec![])).unwrap();
    assert!(second.left_local.is_empty(), "{second:?}");
    let batches = ctl.batches();
    assert_eq!(batches.len(), 2, "one batch per flush");
    assert!(
        batches
            .iter()
            .all(|b| b.runs.is_empty() && b.attempts.is_empty()),
        "never published"
    );
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

/// A run as recorded before entry ids (spec §1.3): no `id`, no `at`.
fn sample_run(gate: GateId, commit: &str, population: u64) -> GateRun {
    GateRun {
        id: None,
        at: None,
        gate,
        record: None,
        commit: commit.into(),
        verdict: Verdict::from_predicate(true, population),
        population,
        output_excerpt: Some(String::new()),
        duration_ms: 1,
        cost_usd_micros: 0,
    }
}

/// An entry id no store mints: the `9` variant nibble keeps it apart from
/// [`seq_iri`]'s ids. Ordered by `n`, as UUIDv7 ids are ordered by time.
pub fn entry_iri(n: u64) -> Iri {
    Iri::parse(&format!("urn:uuid:00000000-0000-7000-9000-{n:012x}"))
        .expect("a formatted urn:uuid is a valid IRI")
}

/// A run with an id, stamped `n` milliseconds after the epoch.
pub fn sample_record_run(n: u64, gate: &GateId, record: Option<&RecordId>) -> GateRun {
    GateRun {
        id: Some(entry_iri(n)),
        at: Some(At::from_unix_millis(n)),
        gate: gate.clone(),
        record: record.cloned(),
        commit: "abc".into(),
        verdict: Verdict::from_predicate(true, 1),
        population: 1,
        output_excerpt: Some(format!("run {n}")),
        duration_ms: 1,
        cost_usd_micros: 0,
    }
}

/// An attempt with an id, stamped `n` milliseconds after the epoch.
pub fn sample_attempt(n: u64, project: &ProjectId, record: &RecordId) -> Attempt {
    Attempt {
        id: Some(entry_iri(n)),
        at: Some(At::from_unix_millis(n)),
        project: project.clone(),
        record: record.clone(),
        adapter: "claude".into(),
        status: AttemptStatus::Completed,
        duration_ms: 1,
        tokens_in: 0,
        tokens_out: 0,
        cost_usd_micros: 0,
        paths_touched: PathsTouched::Listed(vec!["src/a.rs".into()]),
        output_excerpt: Some(format!("attempt {n}")),
    }
}

/// A `check` decision about `record`, resting on `rests_on`.
pub fn sample_decision(n: u64, record: &RecordId, rests_on: Vec<Iri>) -> Decision {
    Decision {
        id: entry_iri(1_000_000 + n),
        at: At::from_unix_millis(1_000_000 + n),
        record: record.clone(),
        finding: None,
        outcome: Outcome::Check {
            transition: TransitionOutcome {
                transition: "launch".into(),
                passed: true,
            },
        },
        rests_on,
    }
}

/// Drives a split ledger's GitHub side. `MemRemote` implements it here;
/// plan B's fixture implements it over `GithubLedger` and the fake GitHub.
pub trait RemoteControl {
    /// While down, every remote read and write fails as unreachable.
    /// Ownership is a local check (spec §2.1) and keeps answering.
    fn set_down(&self, down: bool);
    /// The next publish lands, and then its answer is lost — a timeout
    /// after the commit (spec §3.2 step 5, §8.3).
    fn lose_next_answer(&self);
    /// A record another repository owns.
    fn foreign_record(&self) -> RecordId;
    /// The remote side itself, read without the local store.
    fn remote(&self) -> &dyn RemoteLedger;
    /// Every batch the remote side was handed to publish, in order — what
    /// each flush offered, which GitHub's own de-duplication would hide
    /// from every read.
    fn batches(&self) -> Vec<Batch>;
}

/// The one record [`MemRemote`] does not own.
fn foreign() -> RecordId {
    RecordId(
        Iri::parse("urn:uuid:00000000-0000-7000-f000-000000000001")
            .expect("a formatted urn:uuid is a valid IRI"),
    )
}

/// An in-memory GitHub side for [`crate::split::SplitLedger`] (GitHub
/// ledger spec §8.2). Ownership is a pure, local answer, as the spec
/// requires: it owns every record except [`RemoteControl::foreign_record`],
/// and never reads a tracker. `publish` adds only ids it does not hold, and
/// makes no commit when nothing is left.
pub struct MemRemote {
    node_id: String,
    inner: RefCell<RemoteInner>,
}

#[derive(Default)]
struct RemoteInner {
    down: bool,
    fail_publish: bool,
    damaged: bool,
    rate_limited: bool,
    lose_next_answer: bool,
    foreign: BTreeSet<RecordId>,
    runs: Vec<GateRun>,
    attempts: Vec<Attempt>,
    decisions: Vec<Decision>,
    commits: u64,
    batches: Vec<Batch>,
}

impl MemRemote {
    pub fn new(node_id: &str) -> Self {
        let inner = RemoteInner {
            foreign: BTreeSet::from([foreign()]),
            ..RemoteInner::default()
        };
        Self {
            node_id: node_id.to_string(),
            inner: RefCell::new(inner),
        }
    }

    /// Every `publish` fails and nothing lands, while everything else
    /// answers.
    pub fn fail_publish(&self, on: bool) {
        self.inner.borrow_mut().fail_publish = on;
    }

    /// Every read answers that the ledger was altered — a damaged ledger,
    /// not an unreachable one — while publishing still works.
    pub fn damage(&self, on: bool) {
        self.inner.borrow_mut().damaged = on;
    }

    /// Every read answers that GitHub's rate limit is spent.
    pub fn rate_limit_reads(&self, on: bool) {
        self.inner.borrow_mut().rate_limited = on;
    }

    /// A read refused for a spent rate limit or a damaged ledger.
    fn refuse_read(&self) -> Result<(), StoreError> {
        if self.inner.borrow().rate_limited {
            return Err(StoreError::RateLimited {
                reset: "1700000000 (unix seconds)".into(),
            });
        }
        if self.inner.borrow().damaged {
            return Err(LedgerFault::Altered {
                repo: self.node_id.clone(),
                file: "runs/0/1.jsonl".into(),
                what: "was changed by the test".into(),
                commit: "commit-0".into(),
            }
            .into());
        }
        Ok(())
    }

    /// A run as GitHub holds it — another machine's, or an altered copy —
    /// added without de-duplication.
    pub fn insert_run(&self, run: GateRun) {
        self.inner.borrow_mut().runs.push(run);
    }

    /// An attempt as GitHub holds it, added without de-duplication.
    pub fn insert_attempt(&self, attempt: Attempt) {
        self.inner.borrow_mut().attempts.push(attempt);
    }

    pub fn decisions(&self) -> Vec<Decision> {
        self.inner.borrow().decisions.clone()
    }

    fn unreachable(&self, cause: &str) -> StoreError {
        StoreError::Unreachable {
            store: format!("github ledger {}", self.node_id),
            cause: cause.to_string(),
        }
    }

    fn refuse_if_down(&self) -> Result<(), StoreError> {
        if self.inner.borrow().down {
            return Err(self.unreachable("taken down by the test"));
        }
        Ok(())
    }
}

impl RemoteLedger for MemRemote {
    fn repo_node_id(&self) -> &str {
        &self.node_id
    }

    fn owns_record(&self, record: &RecordId) -> Result<bool, StoreError> {
        Ok(!self.inner.borrow().foreign.contains(record))
    }

    fn publish(&self, batch: &Batch) -> Result<Option<String>, StoreError> {
        self.refuse_if_down()?;
        let mut s = self.inner.borrow_mut();
        s.batches.push(batch.clone());
        if s.fail_publish {
            return Err(self.unreachable("the commit did not land"));
        }
        let mut added = 0;
        for run in &batch.runs {
            if !s.runs.iter().any(|held| held.id == run.id) {
                s.runs.push(run.clone());
                added += 1;
            }
        }
        for attempt in &batch.attempts {
            if !s.attempts.iter().any(|held| held.id == attempt.id) {
                s.attempts.push(attempt.clone());
                added += 1;
            }
        }
        if !s.decisions.iter().any(|d| d.id == batch.decision.id) {
            s.decisions.push(batch.decision.clone());
            added += 1;
        }
        let commit = if added == 0 {
            None
        } else {
            s.commits += 1;
            Some(format!("commit-{}", s.commits))
        };
        if std::mem::take(&mut s.lose_next_answer) {
            return Err(self.unreachable("the commit landed, but its answer was lost"));
        }
        Ok(commit)
    }

    fn gate_runs(&self, gate: &GateId) -> Result<Vec<GateRun>, StoreError> {
        self.refuse_if_down()?;
        self.refuse_read()?;
        Ok(self
            .inner
            .borrow()
            .runs
            .iter()
            .filter(|r| r.gate == *gate)
            .cloned()
            .collect())
    }

    fn attempts(&self, project: &ProjectId) -> Result<Vec<Attempt>, StoreError> {
        self.refuse_if_down()?;
        self.refuse_read()?;
        Ok(self
            .inner
            .borrow()
            .attempts
            .iter()
            .filter(|a| a.project == *project)
            .cloned()
            .collect())
    }
}

impl RemoteControl for MemRemote {
    fn set_down(&self, down: bool) {
        self.inner.borrow_mut().down = down;
    }
    fn lose_next_answer(&self) {
        self.inner.borrow_mut().lose_next_answer = true;
    }
    fn foreign_record(&self) -> RecordId {
        foreign()
    }
    fn remote(&self) -> &dyn RemoteLedger {
        self
    }
    fn batches(&self) -> Vec<Batch> {
        self.inner.borrow().batches.clone()
    }
}
