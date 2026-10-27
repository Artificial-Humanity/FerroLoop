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
use crate::escalation::{EscalationFault, Escalations, Mark, Tombstone, escalate_command};
use crate::fault::LedgerFault;
use crate::finding::{Finding, FindingState};
use crate::ids::{
    FindingId, GateId, Kind, ProjectId, RecordId, WorkflowId, WorkflowRunId, seq_iri,
};
use crate::iri::Iri;
use crate::log::{Attempt, AttemptStatus, GateRun, PathsTouched};
use crate::model::{
    CommandSpec, GateDef, GateKind, PopulationDelivery, Regret, Selector, State, Transition,
};
use crate::split::{Batch, CachedSegment, LedgerCache, Outbox, RemoteLedger, SplitLedger};
use crate::store::{
    AgentRegistry, Catalog, Handles, Ledger, MessageStore, StateStore, StoreError, Tracker,
    WorkflowStore,
};
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
    pub state: &'a dyn StateStore,
    pub agents: &'a dyn AgentRegistry,
    pub wire: &'a dyn MessageStore,
    pub workflows: &'a dyn WorkflowStore,
}

/// Hands one case its `Bound`. A callback rather than a return value, so a
/// fixture can build adapters that live only for the call — a split
/// binding's `CatalogChecked` borrows two stores the fixture owns.
pub trait Fixture {
    fn with(&self, f: &mut dyn FnMut(&Bound<'_>));
}

/// One store backing every role, plus a guard to keep alive.
pub struct Single<S, G>(pub S, pub G);

impl<
    S: Catalog
        + Tracker
        + Ledger
        + Handles
        + StateStore
        + AgentRegistry
        + MessageStore
        + WorkflowStore,
    G,
> Fixture for Single<S, G>
{
    fn with(&self, f: &mut dyn FnMut(&Bound<'_>)) {
        f(&Bound {
            catalog: &self.0,
            tracker: &self.0,
            ledger: &self.0,
            handles: &self.0,
            state: &self.0,
            agents: &self.0,
            wire: &self.0,
            workflows: &self.0,
        });
    }
}

/// A split ledger to test, with the means to drive its GitHub side.
pub trait SplitFixture {
    fn with_split(&self, f: &mut dyn FnMut(&Bound<'_>, &dyn RemoteControl));
}

/// `SplitLedger` over a local store `S` and a [`MemRemote`] for `R_1`,
/// whose ledger was switched on (a cut-over before every sample entry). A
/// [`Fixture`] too, so the shared ledger suite runs over it. `fl_github`'s
/// fixture swaps `MemRemote` for `GithubLedger` over the fake GitHub.
pub struct SplitOver<S, G>(pub S, pub G);

impl<
    S: Catalog
        + Tracker
        + Ledger
        + Handles
        + Outbox
        + StateStore
        + AgentRegistry
        + MessageStore
        + WorkflowStore,
    G,
> SplitFixture for SplitOver<S, G>
{
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
                state: &self.0,
                agents: &self.0,
                wire: &self.0,
                workflows: &self.0,
            },
            &remote,
        );
    }
}

impl<
    S: Catalog
        + Tracker
        + Ledger
        + Handles
        + Outbox
        + StateStore
        + AgentRegistry
        + MessageStore
        + WorkflowStore,
    G,
> Fixture for SplitOver<S, G>
{
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
const TRACKER_CASES: usize = 14;
/// How many cases [`ledger`] runs. Update deliberately — see [`run_suite`].
const LEDGER_CASES: usize = 4;
/// How many cases [`split_ledger`] runs. Update deliberately — see [`run_suite`].
const SPLIT_LEDGER_CASES: usize = 10;
/// How many cases [`all_roles`] runs. Update deliberately — see [`run_suite`].
const ALL_ROLES_CASES: usize = 7;
/// How many cases [`local_handles`] runs. Update deliberately — see [`run_suite`].
const LOCAL_HANDLES_CASES: usize = 1;
/// How many cases [`ledger_cache`] runs. Update deliberately — see [`run_suite`].
const LEDGER_CACHE_CASES: usize = 6;
/// How many cases [`escalations`] runs. Update deliberately — see [`run_suite`].
const ESCALATION_CASES: usize = 8;

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
        an_area_given_at_creation_reads_back,
        an_update_keeps_what_a_finding_was_raised_with,
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
pub fn local_handles<S: Catalog + Tracker + Ledger + Handles + StateStore + AgentRegistry, G>(
    make: impl Fn() -> (S, G),
) {
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
        cached_under_scans_from_the_directory_not_from_the_start_of_the_repository::<S>,
        remember_commits_the_head_and_every_segment_together::<S>,
        a_cached_file_keeps_bytes_that_are_not_utf8_exactly::<S>,
    ];
    run_suite("ledger-cache", LEDGER_CACHE_CASES, cases, make);
}

/// The roles an escalation case uses (routing spec §3.3, §3.6): a local
/// store's catalog and tracker, and its marks and tombstones — one store.
pub struct EscalationBound<'a> {
    pub catalog: &'a dyn Catalog,
    pub tracker: &'a dyn Tracker,
    pub escalations: &'a dyn Escalations,
}

/// Hands one escalation case its [`EscalationBound`], as [`Fixture`] does.
pub trait EscalationFixture {
    fn with_escalations(&self, f: &mut dyn FnMut(&EscalationBound<'_>));
}

impl<S: Catalog + Tracker + Escalations, G> EscalationFixture for Single<S, G> {
    fn with_escalations(&self, f: &mut dyn FnMut(&EscalationBound<'_>)) {
        f(&EscalationBound {
            catalog: &self.0,
            tracker: &self.0,
            escalations: &self.0,
        });
    }
}

/// What every local store does with an escalation's mark and tombstone
/// (routing spec §3.3, §3.6): a marked item reads as itself and refuses
/// every write, naming the command that finishes it; a tombstoned item
/// reads as `Escalated` from every name it has, refuses every write, and
/// is left out of every list.
pub fn escalations<F: EscalationFixture>(make: impl Fn() -> F) {
    let cases: &[fn(&EscalationBound<'_>)] = &[
        a_mark_reads_back_and_a_second_mark_is_refused,
        unmarking_clears_the_mark_and_a_second_unmark_is_refused,
        a_tombstone_is_made_from_the_mark_and_replaces_it,
        a_tombstoned_item_reads_as_escalated_and_refuses_every_write,
        a_marked_item_refuses_every_write_and_a_finding_about_it_is_allowed,
        a_tombstoned_item_is_left_out_of_every_list,
        an_id_this_store_never_held_has_no_mark_and_no_tombstone,
        an_alias_reaches_the_primary_in_every_escalation_method,
    ];
    assert_eq!(
        cases.len(),
        ESCALATION_CASES,
        "the escalation suite lists {} cases but declares {ESCALATION_CASES}. A case was \
         added or removed: if that was deliberate, update the count beside the list; if not, \
         restore the case",
        cases.len()
    );
    for case in cases {
        let fixture = make();
        fixture.with_escalations(&mut |b| case(b));
    }
}

fn the_last_head_is_kept_per_repository_and_replaced<S: LedgerCache>(s: &S) {
    assert_eq!(s.last_head("R_1").unwrap(), None);
    s.remember("R_1", "c1", &[]).unwrap();
    s.remember("R_1", "c2", &[]).unwrap();
    assert_eq!(s.last_head("R_1").unwrap().as_deref(), Some("c2"));
    assert_eq!(s.last_head("R_2").unwrap(), None);
}

/// Caches `segment` at `path` in `repo`, as a read that validated it at a
/// head `h` does: through `remember`, the only write.
fn put<S: LedgerCache>(s: &S, repo: &str, path: &str, segment: CachedSegment) {
    s.remember(repo, "h", &[(path.to_string(), segment)])
        .unwrap();
}

fn a_cached_file_reads_back_by_path_and_by_its_own_directory_only<S: LedgerCache>(s: &S) {
    let seg = |oid: &str, closed: bool| CachedSegment {
        oid: oid.into(),
        bytes: format!("{oid}\n").into_bytes(),
        closed,
    };
    assert_eq!(s.cached("R_1", "runs/aa/1.jsonl").unwrap(), None);
    assert!(s.cached_under("R_1", "runs/aa").unwrap().is_empty());
    put(s, "R_1", "runs/aa/1.jsonl", seg("o1", true));
    put(s, "R_1", "runs/aa/2.jsonl", seg("o2", false));
    put(s, "R_1", "runs/aab/1.jsonl", seg("o3", false));
    put(s, "R_2", "runs/aa/1.jsonl", seg("o4", false));
    put(s, "R_1", "runs/aa/2.jsonl", seg("o5", true));
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
        bytes: format!("{oid}\n").into_bytes(),
        closed: false,
    };
    put(s, "R_1", "runs/aa/1.jsonl", seg("o1"));
    put(s, "R_2", "runs/aa/1.jsonl", seg("o2"));

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

/// ⚠ Pins `cached_under`'s scan START, not just its stopping condition.
/// `R_1`'s `format` row sorts, as a `(repo, path)` key, before `runs/aa/` —
/// and `R_1`'s `runs/a9/1.jsonl` sorts between `format` and `runs/aa/` too,
/// since `'9'` is less than `'a'`. A scan that starts too early (the whole
/// table, or the repository's own first row) hits one of these before ever
/// reaching `runs/aa`, and an implementation that stops at the first
/// mismatch breaks right there — silently returning an empty or short list,
/// never an error.
fn cached_under_scans_from_the_directory_not_from_the_start_of_the_repository<S: LedgerCache>(
    s: &S,
) {
    let seg = |oid: &str| CachedSegment {
        oid: oid.into(),
        bytes: format!("{oid}\n").into_bytes(),
        closed: false,
    };
    put(s, "R_1", "format", seg("meta"));
    put(s, "R_1", "runs/a9/1.jsonl", seg("a9"));
    put(s, "R_1", "runs/aa/1.jsonl", seg("r1a"));
    put(s, "R_2", "runs/aa/1.jsonl", seg("r2a"));
    put(s, "R_2", "runs/aa/2.jsonl", seg("r2b"));

    let r1: Vec<(String, String)> = s
        .cached_under("R_1", "runs/aa")
        .unwrap()
        .into_iter()
        .map(|(p, c)| (p, c.oid))
        .collect();
    assert_eq!(
        r1,
        vec![("runs/aa/1.jsonl".to_string(), "r1a".to_string())],
        "neither `format` nor `runs/a9` belong under `runs/aa`"
    );

    let r2: Vec<(String, String)> = s
        .cached_under("R_2", "runs/aa")
        .unwrap()
        .into_iter()
        .map(|(p, c)| (p, c.oid))
        .collect();
    assert_eq!(
        r2,
        vec![
            ("runs/aa/1.jsonl".to_string(), "r2a".to_string()),
            ("runs/aa/2.jsonl".to_string(), "r2b".to_string()),
        ],
        "in path order"
    );
}

/// `remember` is the write a read commits with: the head and every segment
/// it validated land together, and a later call only touches what it names
/// — it adds and replaces, it does not wipe what an earlier call cached.
fn remember_commits_the_head_and_every_segment_together<S: LedgerCache>(s: &S) {
    let seg = |oid: &str, closed: bool| CachedSegment {
        oid: oid.into(),
        bytes: format!("{oid}\n").into_bytes(),
        closed,
    };
    assert_eq!(s.last_head("R_1").unwrap(), None);
    s.remember(
        "R_1",
        "c1",
        &[
            ("format".to_string(), seg("f1", true)),
            ("runs/aa/1.jsonl".to_string(), seg("r1", false)),
        ],
    )
    .unwrap();
    assert_eq!(s.last_head("R_1").unwrap().as_deref(), Some("c1"));
    assert_eq!(s.cached("R_1", "format").unwrap(), Some(seg("f1", true)));
    assert_eq!(
        s.cached("R_1", "runs/aa/1.jsonl").unwrap(),
        Some(seg("r1", false))
    );

    // A head-only `remember` (no directory read, as `check_format` makes)
    // still advances the head, and leaves earlier segments alone.
    s.remember("R_1", "c2", &[]).unwrap();
    assert_eq!(s.last_head("R_1").unwrap().as_deref(), Some("c2"));
    assert_eq!(
        s.cached("R_1", "runs/aa/1.jsonl").unwrap(),
        Some(seg("r1", false)),
        "a head-only remember must not touch a segment it does not name"
    );

    // A segment an earlier call cached, that this call does not mention,
    // survives this call: `remember` adds and replaces, it never wipes.
    s.remember(
        "R_1",
        "c3",
        &[("runs/aa/2.jsonl".to_string(), seg("r2", true))],
    )
    .unwrap();
    assert_eq!(s.last_head("R_1").unwrap().as_deref(), Some("c3"));
    assert_eq!(
        s.cached("R_1", "runs/aa/1.jsonl").unwrap(),
        Some(seg("r1", false)),
        "a segment this call did not name is untouched"
    );
    assert_eq!(
        s.cached("R_1", "runs/aa/2.jsonl").unwrap(),
        Some(seg("r2", true))
    );
}

/// ⚠ A file is cached as the bytes GitHub sent, exactly — through every
/// `remember`, whatever head it names. A store that keeps text would have to
/// decode a damaged line lossily, and two different damaged lines would
/// then compare equal (check 4).
fn a_cached_file_keeps_bytes_that_are_not_utf8_exactly<S: LedgerCache>(s: &S) {
    let seg = |oid: &str, bytes: &[u8]| CachedSegment {
        oid: oid.into(),
        bytes: bytes.to_vec(),
        closed: false,
    };
    let one = seg("o1", &[b'a', b'\n', 0xff, b'\n']);
    let two = seg("o2", &[0xfe, b'\n']);
    put(s, "R_1", "runs/aa/1.jsonl", one.clone());
    s.remember("R_1", "c1", &[("runs/aa/2.jsonl".to_string(), two.clone())])
        .unwrap();
    assert_eq!(
        s.cached("R_1", "runs/aa/1.jsonl").unwrap(),
        Some(one.clone())
    );
    assert_eq!(
        s.cached_under("R_1", "runs/aa").unwrap(),
        vec![
            ("runs/aa/1.jsonl".to_string(), one),
            ("runs/aa/2.jsonl".to_string(), two),
        ]
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

/// `set_record_state` and `update_finding` both take an
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

/// `add_finding` must store the referenced record's
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

/// `update_finding` keeps the STORED
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

/// Routing spec §1.1: an area given when an item is made is stored with it
/// and reads back.
fn an_area_given_at_creation_reads_back(roles: &Bound<'_>) {
    let p = roles.catalog.add_project("/p").unwrap();
    let r = roles
        .tracker
        .add_record_with_area(&p, "t", Some("code"))
        .unwrap();
    assert_eq!(
        roles
            .tracker
            .get_record(&r)
            .unwrap()
            .unwrap()
            .area
            .as_deref(),
        Some("code")
    );
    let mut f = Finding::raise(p, r, "reviewer", "claim");
    f.area = Some("code".into());
    let id = roles.tracker.add_finding(f).unwrap();
    assert_eq!(
        roles
            .tracker
            .get_finding(&id)
            .unwrap()
            .unwrap()
            .area
            .as_deref(),
        Some("code")
    );
}

/// Routing spec §1.1: an item keeps its area for its whole life — and a
/// finding its record, its raiser and its security mark: an update never
/// takes them from the caller.
fn an_update_keeps_what_a_finding_was_raised_with(roles: &Bound<'_>) {
    let p = roles.catalog.add_project("/p").unwrap();
    let r = roles
        .tracker
        .add_record_with_area(&p, "t", Some("code"))
        .unwrap();
    let other = roles
        .tracker
        .add_record_with_area(&p, "u", Some("code"))
        .unwrap();
    let mut f = Finding::raise(p, r.clone(), "reviewer", "claim");
    f.area = Some("code".into());
    let id = roles.tracker.add_finding(f).unwrap();
    let mut changed = roles.tracker.get_finding(&id).unwrap().unwrap();
    changed.area = Some("tests".into());
    changed.record = other;
    changed.security = true;
    changed.raised_by = "someone else".into();
    changed.withdraw("no").unwrap();
    roles.tracker.update_finding(&changed).unwrap();
    let back = roles.tracker.get_finding(&id).unwrap().unwrap();
    assert_eq!(
        back.state,
        FindingState::Withdrawn,
        "the caller's state is written"
    );
    assert_eq!(
        (
            back.area.as_deref(),
            back.record,
            back.security,
            back.raised_by.as_str()
        ),
        (Some("code"), r, false, "reviewer")
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
// lost may read, to fl, as an outright failure — or, if the remote rereads
// the ledger and finds its own commit, as a success. Either way, the run is
// never published twice.
fn a_commit_whose_answer_was_lost_is_not_duplicated_by_the_next_flush(
    roles: &Bound<'_>,
    ctl: &dyn RemoteControl,
) {
    let (_p, g, r) = record_world(roles);
    let run = sample_record_run(1, &g, Some(&r));
    let id = run.id.clone().unwrap();
    roles.ledger.append_gate_run(run).unwrap();
    ctl.lose_next_answer();
    let first = roles.ledger.flush(sample_decision(1, &r, vec![id.clone()]));
    if first.is_ok() {
        // A remote that reread the ledger already landed the run on this
        // first flush: the second flush below must find it already there,
        // not add a second copy of its own.
        assert_eq!(
            run_ids(ctl.remote().gate_runs(&g).unwrap()),
            vec![Some(id.clone())],
            "the run already landed before the second flush"
        );
    }
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

// An id this store DOES hold, but as another kind,
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

fn handles_are_per_kind_and_start_at_one<
    S: Catalog + Tracker + Ledger + Handles + StateStore + AgentRegistry,
>(
    s: &S,
) {
    let p = s.add_project("/p").unwrap();
    let g = s
        .add_gate(&p, "g", sample_kind(), sample_selector(), 1, "abc", "o")
        .unwrap();
    let r = s.add_record(&p, "t").unwrap();
    let f = s
        .add_finding(Finding::raise(p.clone(), r.clone(), "a", "c"))
        .unwrap();
    let st = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Workspace,
            slug: "rule".into(),
            sensitivity: crate::state::SensitivityPatch::Inherit,
            payload: crate::state::StatePayload::Handoff(crate::state::HandoffEntry {
                summary: "ok".into(),
                shipped: vec![],
                in_flight: vec![],
                blockers: vec![],
                next_steps: vec![],
                notes: None,
            }),
            at: At::from_unix_millis(1),
            by: "o".into(),
        })
        .unwrap();
    let ag = s
        .register_agent(crate::agent::RegisterAgentArgs {
            display_name: "Ferris".into(),
            email: None,
            project: "ferroloop".into(),
            role: crate::agent::AgentRole::Lead,
            extra_areas: vec![],
            create_area: true,
            replace: false,
            at: At::from_unix_millis(1),
            by: "o".into(),
        })
        .unwrap();
    assert_eq!(s.handle_of(Kind::Finding, f.iri()).unwrap(), Some(1));
    assert_eq!(s.handle_of(Kind::Project, p.iri()).unwrap(), Some(1));
    assert_eq!(s.handle_of(Kind::Gate, g.iri()).unwrap(), Some(1));
    assert_eq!(s.handle_of(Kind::Record, r.iri()).unwrap(), Some(1));
    assert_eq!(s.handle_of(Kind::State, st.id.iri()).unwrap(), Some(1));
    assert_eq!(s.handle_of(Kind::Agent, ag.id.iri()).unwrap(), Some(1));
    assert_eq!(
        s.resolve_handle(Kind::Gate, 1).unwrap().as_ref(),
        Some(g.iri())
    );
    assert_eq!(
        s.resolve_handle(Kind::State, 1).unwrap().as_ref(),
        Some(st.id.iri())
    );
    assert_eq!(
        s.resolve_handle(Kind::Agent, 1).unwrap().as_ref(),
        Some(ag.id.iri())
    );
    assert_eq!(s.resolve_handle(Kind::Gate, 2).unwrap(), None);
    assert_eq!(s.resolve_handle(Kind::Finding, 0).unwrap(), None);
}

// `refs::show` prints a handle only when
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
    let st = roles
        .state
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Workspace,
            slug: "rule".into(),
            sensitivity: crate::state::SensitivityPatch::Inherit,
            payload: crate::state::StatePayload::Handoff(crate::state::HandoffEntry {
                summary: "ok".into(),
                shipped: vec![],
                in_flight: vec![],
                blockers: vec![],
                next_steps: vec![],
                notes: None,
            }),
            at: At::from_unix_millis(1),
            by: "o".into(),
        })
        .unwrap();
    let ag = roles
        .agents
        .register_agent(crate::agent::RegisterAgentArgs {
            display_name: "Ferris".into(),
            email: None,
            project: "ferroloop".into(),
            role: crate::agent::AgentRole::Lead,
            extra_areas: vec![],
            create_area: true,
            replace: false,
            at: At::from_unix_millis(1),
            by: "o".into(),
        })
        .unwrap();
    let msg = roles
        .wire
        .send_wire(crate::ferrowire::SendWireArgs {
            id: None,
            sender_id: Some(ag.id.clone()),
            sender_name: "ferris".into(),
            sender_host: None,
            to: crate::ferrowire::WireAddress::All,
            kind: crate::ferrowire::MessageKind::Mail,
            urgency: crate::ferrowire::MessageUrgency::Normal,
            durable: false,
            subject: "ping".into(),
            body: "pong".into(),
            reply_to: None,
            thread_id: None,
            deadline_at: None,
            at: At::from_unix_millis(1),
        })
        .unwrap()
        .0;
    let wf = roles
        .workflows
        .upsert_workflow_def(sample_workflow_def(
            WorkflowId(seq_iri(9001)),
            p.clone(),
            "flow-a",
        ))
        .unwrap();
    let wfr = roles
        .workflows
        .create_workflow_run(
            crate::workflow::WorkflowRun::new(
                WorkflowRunId(seq_iri(9002)),
                &wf,
                "Run 1",
                At::from_unix_millis(1),
                "o",
            )
            .unwrap(),
        )
        .unwrap();
    let held = [
        (Kind::Project, p.0),
        (Kind::Gate, g.0),
        (Kind::Record, r.0),
        (Kind::Finding, f.0),
        (Kind::State, st.id.0),
        (Kind::Agent, ag.id.0),
        (Kind::Message, msg.id.0),
        (Kind::Workflow, wf.id.0),
        (Kind::WorkflowRun, wfr.id.0),
    ];
    for own in Kind::ALL {
        assert!(
            held.iter().any(|(k, _)| k == own),
            "no id of kind {} was minted, so its row was never checked",
            own.as_wire()
        );
    }
    for (own, id) in &held {
        if *own == Kind::Message {
            assert_eq!(
                roles.handles.handle_of(*own, id).unwrap(),
                None,
                "messages do not consume sequential numeric handles"
            );
        } else {
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
        }
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

/// A project, a record in it, and a finding about the record.
fn escalation_world(b: &EscalationBound<'_>) -> (ProjectId, RecordId, FindingId) {
    let p = b.catalog.add_project("/p").unwrap();
    let r = b.tracker.add_record(&p, "look at the layout").unwrap();
    let f = b
        .tracker
        .add_finding(Finding::raise(
            p.clone(),
            r.clone(),
            "rev",
            "the margin is off",
        ))
        .unwrap();
    (p, r, f)
}

fn sample_mark() -> Mark {
    Mark {
        by: "alice".into(),
        reason: "needs a design review".into(),
        at_ms: 1_000,
    }
}

fn sample_issue(n: u64) -> Iri {
    Iri::parse(&format!("https://github.com/acme/widgets/issues/{n}")).unwrap()
}

/// `Escalating`, naming `id` and the command that finishes its escalation.
fn assert_escalating(err: StoreError, id: &Iri, kind: Kind) {
    assert!(
        matches!(
            err,
            StoreError::Escalating { id: ref i, ref to_finish }
                if i == id && *to_finish == escalate_command(kind, id)
        ),
        "{err:?}"
    );
}

/// `Escalated`, from `from` to `to`.
fn assert_escalated(err: StoreError, from: &Iri, to: &Iri) {
    assert!(
        matches!(err, StoreError::Escalated { from: ref f, to: ref t } if f == from && t == to),
        "{err:?}"
    );
}

/// Routing spec §3.3 step 1: the mark holds who, why and when; an item is
/// marked once; only a record or a finding is escalated.
pub fn a_mark_reads_back_and_a_second_mark_is_refused(b: &EscalationBound<'_>) {
    let (p, r, f) = escalation_world(b);
    let later = Mark {
        by: "bob".into(),
        reason: "again".into(),
        at_ms: 2_000,
    };
    for (id, kind) in [(r.iri(), Kind::Record), (f.iri(), Kind::Finding)] {
        assert_eq!(b.escalations.mark_of(id).unwrap(), None);
        b.escalations.mark(id, &sample_mark()).unwrap();
        assert_eq!(b.escalations.mark_of(id).unwrap(), Some(sample_mark()));
        let err = b.escalations.mark(id, &later).unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::Escalation(EscalationFault::AlreadyMarked { id: ref i, kind: k })
                    if i == id && k == kind
            ),
            "{err:?}"
        );
        assert_eq!(
            b.escalations.mark_of(id).unwrap(),
            Some(sample_mark()),
            "the first mark is kept"
        );
    }
    let err = b.escalations.mark(p.iri(), &sample_mark()).unwrap_err();
    assert!(
        matches!(
            err,
            StoreError::WrongKind {
                found: Kind::Project,
                ..
            }
        ),
        "{err:?}"
    );
    assert_eq!(b.escalations.mark_of(p.iri()).unwrap(), None);
}

/// Routing spec §3.3, "Abandoning": the mark is removed, and an item with
/// no mark has nothing to abandon.
pub fn unmarking_clears_the_mark_and_a_second_unmark_is_refused(b: &EscalationBound<'_>) {
    let (_p, r, _f) = escalation_world(b);
    b.escalations.mark(r.iri(), &sample_mark()).unwrap();
    b.escalations.unmark(r.iri()).unwrap();
    assert_eq!(b.escalations.mark_of(r.iri()).unwrap(), None);
    let err = b.escalations.unmark(r.iri()).unwrap_err();
    assert!(
        matches!(
            err,
            StoreError::Escalation(EscalationFault::NotMarked { id: ref i }) if i == r.iri()
        ),
        "{err:?}"
    );
    b.tracker.set_record_state(&r, State::Doing).unwrap();
    b.escalations.mark(r.iri(), &sample_mark()).unwrap();
}

/// Routing spec §3.3 step 3: the tombstone takes the mark's who, why and
/// time, and replaces the mark in the same write; with no mark there is no
/// escalation to finish.
pub fn a_tombstone_is_made_from_the_mark_and_replaces_it(b: &EscalationBound<'_>) {
    let (_p, r, _f) = escalation_world(b);
    let err = b
        .escalations
        .tombstone(r.iri(), &sample_issue(7))
        .unwrap_err();
    assert!(
        matches!(
            err,
            StoreError::Escalation(EscalationFault::NotMarked { id: ref i }) if i == r.iri()
        ),
        "{err:?}"
    );
    assert_eq!(b.escalations.tombstone_of(r.iri()).unwrap(), None);
    b.escalations.mark(r.iri(), &sample_mark()).unwrap();
    let tomb = b.escalations.tombstone(r.iri(), &sample_issue(7)).unwrap();
    assert_eq!(
        tomb,
        Tombstone {
            from: r.iri().clone(),
            to: sample_issue(7),
            by: "alice".into(),
            reason: "needs a design review".into(),
            at_ms: 1_000,
        }
    );
    assert_eq!(
        b.escalations.tombstone_of(r.iri()).unwrap(),
        Some(tomb.clone())
    );
    assert_eq!(
        b.escalations.mark_of(r.iri()).unwrap(),
        None,
        "the mark is gone"
    );
    assert_eq!(
        b.catalog.kind_of(r.iri()).unwrap(),
        Kind::Record,
        "the id still chooses this store"
    );
    let err = b.escalations.mark(r.iri(), &sample_mark()).unwrap_err();
    assert_escalated(err, r.iri(), &sample_issue(7));
    let err = b
        .escalations
        .tombstone(r.iri(), &sample_issue(8))
        .unwrap_err();
    assert!(
        matches!(
            err,
            StoreError::Escalation(EscalationFault::NotMarked { .. })
        ),
        "{err:?}"
    );
    assert_eq!(b.escalations.tombstone_of(r.iri()).unwrap(), Some(tomb));
}

/// Routing spec §3.3 step 1, §3.6: a marked item reads as itself and is
/// listed, and every write to it is refused naming the command that
/// finishes the escalation. A finding raised about a marked record is not a
/// write to it.
pub fn a_marked_item_refuses_every_write_and_a_finding_about_it_is_allowed(
    b: &EscalationBound<'_>,
) {
    let (p, r, f) = escalation_world(b);
    let other = b.tracker.add_record(&p, "unmarked").unwrap();
    b.escalations.mark(r.iri(), &sample_mark()).unwrap();
    b.escalations.mark(f.iri(), &sample_mark()).unwrap();

    assert_eq!(b.tracker.get_record(&r).unwrap().unwrap().id, r);
    let held = b.tracker.get_finding(&f).unwrap().unwrap();
    assert_eq!(held.id, f);
    let records: Vec<RecordId> = b
        .tracker
        .list_records(&p)
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(records, vec![r.clone(), other.clone()]);
    assert_eq!(b.tracker.list_findings(&p).unwrap().len(), 1);

    let err = b.tracker.set_record_state(&r, State::Doing).unwrap_err();
    assert_escalating(err, r.iri(), Kind::Record);
    assert_eq!(
        b.tracker.get_record(&r).unwrap().unwrap().state,
        State::Todo
    );
    let mut changed = held;
    changed.withdraw("not concrete").unwrap();
    let err = b.tracker.update_finding(&changed).unwrap_err();
    assert_escalating(err, f.iri(), Kind::Finding);
    assert_eq!(
        b.tracker.get_finding(&f).unwrap().unwrap().state,
        FindingState::Raised
    );
    let err = b.tracker.add_alias(r.iri(), sample_issue(41)).unwrap_err();
    assert_escalating(err, r.iri(), Kind::Record);
    let err = b.tracker.add_alias(f.iri(), sample_issue(42)).unwrap_err();
    assert_escalating(err, f.iri(), Kind::Finding);
    assert!(
        b.tracker.get_record(&RecordId(sample_issue(41))).is_err(),
        "no alias was added"
    );

    let raised = b
        .tracker
        .add_finding(Finding::raise(p, r.clone(), "rev", "and the gutter"))
        .unwrap();
    assert_eq!(b.tracker.get_finding(&raised).unwrap().unwrap().record, r);
    b.tracker.set_record_state(&other, State::Doing).unwrap();
}

/// Routing spec §3.6: a tombstoned id reads as `Escalated`, every write to
/// it is refused the same way, and so is a finding raised about a
/// tombstoned record — the router follows the tombstone instead.
pub fn a_tombstoned_item_reads_as_escalated_and_refuses_every_write(b: &EscalationBound<'_>) {
    let (p, r, f) = escalation_world(b);
    let held = b.tracker.get_finding(&f).unwrap().unwrap();
    b.escalations.mark(r.iri(), &sample_mark()).unwrap();
    b.escalations.tombstone(r.iri(), &sample_issue(7)).unwrap();
    b.escalations.mark(f.iri(), &sample_mark()).unwrap();
    b.escalations.tombstone(f.iri(), &sample_issue(8)).unwrap();

    assert_escalated(
        b.tracker.get_record(&r).unwrap_err(),
        r.iri(),
        &sample_issue(7),
    );
    assert_escalated(
        b.tracker.get_finding(&f).unwrap_err(),
        f.iri(),
        &sample_issue(8),
    );
    let err = b.tracker.set_record_state(&r, State::Doing).unwrap_err();
    assert_escalated(err, r.iri(), &sample_issue(7));
    let mut changed = held;
    changed.withdraw("not concrete").unwrap();
    let err = b.tracker.update_finding(&changed).unwrap_err();
    assert_escalated(err, f.iri(), &sample_issue(8));
    let err = b.tracker.add_alias(r.iri(), sample_issue(41)).unwrap_err();
    assert_escalated(err, r.iri(), &sample_issue(7));
    let err = b.tracker.add_alias(f.iri(), sample_issue(42)).unwrap_err();
    assert_escalated(err, f.iri(), &sample_issue(8));
    let err = b
        .tracker
        .add_finding(Finding::raise(p, r.clone(), "rev", "and the gutter"))
        .unwrap_err();
    assert_escalated(err, r.iri(), &sample_issue(7));
}

/// Routing spec §2.4: tombstones are not listed, and a withdrawal is
/// counted once — by the tier the finding lives in now.
pub fn a_tombstoned_item_is_left_out_of_every_list(b: &EscalationBound<'_>) {
    let p = b.catalog.add_project("/p").unwrap();
    let kept = b.tracker.add_record(&p, "kept").unwrap();
    let gone = b.tracker.add_record(&p, "gone").unwrap();
    let mut findings = vec![];
    for claim in ["stays", "goes"] {
        let id = b
            .tracker
            .add_finding(Finding::raise(p.clone(), kept.clone(), "hasty", claim))
            .unwrap();
        let mut f = b.tracker.get_finding(&id).unwrap().unwrap();
        f.withdraw("not concrete").unwrap();
        b.tracker.update_finding(&f).unwrap();
        findings.push(id);
    }
    assert_eq!(b.tracker.withdrawals_by("hasty").unwrap(), 2);
    for (id, n) in [(gone.iri(), 7), (findings[1].iri(), 8)] {
        b.escalations.mark(id, &sample_mark()).unwrap();
        b.escalations.tombstone(id, &sample_issue(n)).unwrap();
    }
    let records: Vec<RecordId> = b
        .tracker
        .list_records(&p)
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(records, vec![kept]);
    let listed: Vec<FindingId> = b
        .tracker
        .list_findings(&p)
        .unwrap()
        .into_iter()
        .map(|f| f.id)
        .collect();
    assert_eq!(listed, vec![findings[0].clone()]);
    assert_eq!(b.tracker.withdrawals_by("hasty").unwrap(), 1);
}

/// `mark_of` and `tombstone_of` answer `None` for an id the store does not
/// hold — the router asks before it knows — while the writes refuse it as
/// `NotOwned`, never as "not marked".
pub fn an_id_this_store_never_held_has_no_mark_and_no_tombstone(b: &EscalationBound<'_>) {
    let _ = escalation_world(b);
    let id = stranger();
    assert_eq!(b.escalations.mark_of(&id).unwrap(), None);
    assert_eq!(b.escalations.tombstone_of(&id).unwrap(), None);
    assert_eq!(b.escalations.mark_of(&sample_issue(7)).unwrap(), None);
    assert_all_not_owned(
        &id,
        vec![
            ("mark", b.escalations.mark(&id, &sample_mark())),
            ("unmark", b.escalations.unmark(&id)),
            (
                "tombstone",
                b.escalations.tombstone(&id, &sample_issue(7)).map(|_| ()),
            ),
        ],
    );
}

/// Every escalation method resolves an alias to the item's primary IRI:
/// the mark, the tombstone and the refusals all name the primary.
pub fn an_alias_reaches_the_primary_in_every_escalation_method(b: &EscalationBound<'_>) {
    let (p, r, f) = escalation_world(b);
    let ra = sample_issue(41);
    let fa = sample_issue(42);
    b.tracker.add_alias(r.iri(), ra.clone()).unwrap();
    b.tracker.add_alias(f.iri(), fa.clone()).unwrap();

    b.escalations.mark(&ra, &sample_mark()).unwrap();
    assert_eq!(b.escalations.mark_of(r.iri()).unwrap(), Some(sample_mark()));
    assert_eq!(b.escalations.mark_of(&ra).unwrap(), Some(sample_mark()));
    let err = b.escalations.mark(r.iri(), &sample_mark()).unwrap_err();
    assert!(
        matches!(
            err,
            StoreError::Escalation(EscalationFault::AlreadyMarked { id: ref i, .. })
                if i == r.iri()
        ),
        "{err:?}"
    );
    let err = b
        .tracker
        .set_record_state(&RecordId(ra.clone()), State::Doing)
        .unwrap_err();
    assert_escalating(err, r.iri(), Kind::Record);
    b.escalations.unmark(&ra).unwrap();
    assert_eq!(b.escalations.mark_of(r.iri()).unwrap(), None);

    b.escalations.mark(&fa, &sample_mark()).unwrap();
    let tomb = b.escalations.tombstone(&fa, &sample_issue(8)).unwrap();
    assert_eq!(&tomb.from, f.iri(), "the tombstone names the primary");
    assert_eq!(
        b.escalations.tombstone_of(f.iri()).unwrap(),
        Some(tomb.clone())
    );
    assert_eq!(b.escalations.tombstone_of(&fa).unwrap(), Some(tomb));
    let err = b.tracker.get_finding(&FindingId(fa)).unwrap_err();
    assert_escalated(err, f.iri(), &sample_issue(8));

    b.escalations.mark(&ra, &sample_mark()).unwrap();
    b.escalations.tombstone(&ra, &sample_issue(7)).unwrap();
    let err = b.tracker.get_record(&RecordId(ra.clone())).unwrap_err();
    assert_escalated(err, r.iri(), &sample_issue(7));
    let err = b
        .tracker
        .add_finding(Finding::raise(p, RecordId(ra), "rev", "c"))
        .unwrap_err();
    assert_escalated(err, r.iri(), &sample_issue(7));
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
/// `fl_github`'s fixture implements it over `GithubLedger` and the fake
/// GitHub.
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

/// How many cases [`agent_registry`] runs. Update deliberately — see [`run_suite`].
const AGENT_REGISTRY_CASES: usize = 7;
/// How many cases [`state_store`] runs. Update deliberately — see [`run_suite`].
const STATE_STORE_CASES: usize = 10;
/// How many cases [`message_store`] runs. Update deliberately — see [`run_suite`].
const MESSAGE_STORE_CASES: usize = 7;
/// How many cases [`workflow_store`] runs. Update deliberately — see [`run_suite`].
const WORKFLOW_STORE_CASES: usize = 3;

pub fn agent_registry<S: AgentRegistry + Catalog + Handles, G>(make: impl Fn() -> (S, G)) {
    let cases: &[fn(&S)] = &[
        area_round_trips_and_normalizes_key::<S>,
        register_agent_requires_existing_area_unless_create_area_is_set::<S>,
        agent_resolves_by_case_insensitive_name_and_numeric_handle::<S>,
        agent_collision_refuses_active_and_retired_without_replace_and_preserves_iri_on_replace::<S>,
        one_lead_per_project_refuses_second_lead_and_retires_previous_on_replace::<S>,
        reports_to_tracks_active_project_lead_or_owner::<S>,
        registry_event_import_deduplicates_detects_tamper_and_aliases_offline_collision::<S>,
    ];
    run_suite("agent-registry", AGENT_REGISTRY_CASES, cases, make);
}

pub fn state_store<S: StateStore + Catalog + Handles, G>(make: impl Fn() -> (S, G)) {
    let cases: &[fn(&S)] = &[
        memory_handoff_and_runtime_crud_and_key_lookup::<S>,
        sensitive_flag_is_sticky_until_explicitly_cleared_and_ever_sensitive_persists::<S>,
        memory_superseded_validates_target_exists_and_is_memory::<S>,
        runtime_append_requires_array_and_rejects_scalar::<S>,
        scope_concern_and_slug_are_immutable_for_an_entry_id::<S>,
        fast_forward_and_already_ahead_revision_imports_converge::<S>,
        true_divergence_creates_conflict_blocks_writes_and_resolves_cleanly::<S>,
        mismatched_runtime_merge_modes_fail_closed_to_conflict::<S>,
        runtime_append_and_last_writer_auto_merge_without_conflict::<S>,
        offline_same_key_collision_aliases_unpublished_local_id_to_remote::<S>,
    ];
    run_suite("state-store", STATE_STORE_CASES, cases, make);
}

pub fn message_store<S: MessageStore + AgentRegistry + Catalog + Handles, G>(
    make: impl Fn() -> (S, G),
) {
    let cases: &[fn(&S)] = &[
        direct_and_unregistered_persona_addressing_round_trips_across_registration::<S>,
        group_selectors_expand_active_agents_and_exclude_sender::<S>,
        ask_reply_and_thread_indexing_validate_and_correlate::<S>,
        receipt_transitions_and_batch_imports_converge_monotonically::<S>,
        wire_import_deduplicates_and_refuses_tampered_message::<S>,
        offline_agent_id_collision_alias_preserves_wire_inbox_and_ack::<S>,
        prune_wire_removes_only_acked_or_expired_transient_messages_before_cutoff::<S>,
    ];
    run_suite("message-store", MESSAGE_STORE_CASES, cases, make);
}

pub fn workflow_store<S: WorkflowStore + Catalog + Handles, G>(make: impl Fn() -> (S, G)) {
    let cases: &[fn(&S)] = &[
        workflow_def_upserts_resolves_by_slug_and_handle_and_validates_graph::<S>,
        workflow_run_lifecycle_persists_and_filters_by_project_and_workflow::<S>,
        workflow_methods_refuse_wrong_kind_ids::<S>,
    ];
    run_suite("workflow-store", WORKFLOW_STORE_CASES, cases, make);
}

fn area_round_trips_and_normalizes_key<S: AgentRegistry>(s: &S) {
    let a = s
        .upsert_area(crate::agent::AreaRecord {
            area_key: " Ferro_Loop ".into(),
            name: "FerroLoop".into(),
            path: Some("FerroLoop/".into()),
            purpose: "Agent orchestration".into(),
            updated_at: At::from_unix_millis(10),
            by: "jarvis".into(),
        })
        .unwrap();
    assert_eq!(a.area_key, "ferro_loop");
    assert_eq!(s.get_area("FERRO_LOOP").unwrap(), Some(a.clone()));
    assert_eq!(s.list_areas().unwrap(), vec![a]);
}

fn register_agent_requires_existing_area_unless_create_area_is_set<S: AgentRegistry>(s: &S) {
    let err = s
        .register_agent(crate::agent::RegisterAgentArgs {
            display_name: "Ferris".into(),
            email: None,
            project: "ferroloop".into(),
            role: crate::agent::AgentRole::Lead,
            extra_areas: vec![],
            create_area: false,
            replace: false,
            at: At::from_unix_millis(10),
            by: "owner".into(),
        })
        .unwrap_err();
    assert!(matches!(err, StoreError::NoSuchArea(ref k) if k == "ferroloop"));

    let rec = s
        .register_agent(crate::agent::RegisterAgentArgs {
            display_name: "Ferris".into(),
            email: None,
            project: "ferroloop".into(),
            role: crate::agent::AgentRole::Lead,
            extra_areas: vec!["notes".into()],
            create_area: true,
            replace: false,
            at: At::from_unix_millis(11),
            by: "owner".into(),
        })
        .unwrap();
    assert_eq!(rec.name_key, "ferris");
    assert_eq!(rec.email, "ferris@artificialhumanity.io");
    assert!(s.get_area("ferroloop").unwrap().is_some());
    assert!(s.get_area("notes").unwrap().is_some());
}

fn agent_resolves_by_case_insensitive_name_and_numeric_handle<S: AgentRegistry + Handles>(s: &S) {
    let rec = s
        .register_agent(crate::agent::RegisterAgentArgs {
            display_name: "Ferris".into(),
            email: None,
            project: "ferroloop".into(),
            role: crate::agent::AgentRole::Lead,
            extra_areas: vec![],
            create_area: true,
            replace: false,
            at: At::from_unix_millis(10),
            by: "owner".into(),
        })
        .unwrap();
    assert_eq!(s.agent_by_name("FERRIS").unwrap(), Some(rec.clone()));
    assert_eq!(s.get_agent(&rec.id).unwrap(), Some(rec.clone()));
    let h = s.handle_of(Kind::Agent, rec.id.iri()).unwrap().unwrap();
    assert_eq!(s.resolve_handle(Kind::Agent, h).unwrap(), Some(rec.id.0));
}

fn agent_collision_refuses_active_and_retired_without_replace_and_preserves_iri_on_replace<
    S: AgentRegistry,
>(
    s: &S,
) {
    let first = s
        .register_agent(crate::agent::RegisterAgentArgs {
            display_name: "Ferris".into(),
            email: None,
            project: "ferroloop".into(),
            role: crate::agent::AgentRole::Assistant,
            extra_areas: vec![],
            create_area: true,
            replace: false,
            at: At::from_unix_millis(10),
            by: "owner".into(),
        })
        .unwrap();
    let err = s
        .register_agent(crate::agent::RegisterAgentArgs {
            display_name: "ferris".into(),
            email: None,
            project: "ferroloop".into(),
            role: crate::agent::AgentRole::Assistant,
            extra_areas: vec![],
            create_area: false,
            replace: false,
            at: At::from_unix_millis(11),
            by: "owner".into(),
        })
        .unwrap_err();
    assert!(matches!(
        err,
        StoreError::AgentCollision {
            status: crate::agent::AgentStatus::Active,
            ..
        }
    ));

    s.retire_agent(&first.id, At::from_unix_millis(12), "owner")
        .unwrap();
    let err_retired = s
        .register_agent(crate::agent::RegisterAgentArgs {
            display_name: "Ferris".into(),
            email: None,
            project: "ferroloop".into(),
            role: crate::agent::AgentRole::Assistant,
            extra_areas: vec![],
            create_area: false,
            replace: false,
            at: At::from_unix_millis(13),
            by: "owner".into(),
        })
        .unwrap_err();
    assert!(matches!(
        err_retired,
        StoreError::AgentCollision {
            status: crate::agent::AgentStatus::Retired,
            ..
        }
    ));

    let resumed = s
        .register_agent(crate::agent::RegisterAgentArgs {
            display_name: "Ferris".into(),
            email: None,
            project: "ferroloop".into(),
            role: crate::agent::AgentRole::Lead,
            extra_areas: vec![],
            create_area: false,
            replace: true,
            at: At::from_unix_millis(14),
            by: "owner".into(),
        })
        .unwrap();
    assert_eq!(resumed.id, first.id);
    assert_eq!(resumed.status, crate::agent::AgentStatus::Active);
    assert_eq!(resumed.role, crate::agent::AgentRole::Lead);
}

fn one_lead_per_project_refuses_second_lead_and_retires_previous_on_replace<S: AgentRegistry>(
    s: &S,
) {
    let first = s
        .register_agent(crate::agent::RegisterAgentArgs {
            display_name: "Ferris".into(),
            email: None,
            project: "ferroloop".into(),
            role: crate::agent::AgentRole::Lead,
            extra_areas: vec![],
            create_area: true,
            replace: false,
            at: At::from_unix_millis(10),
            by: "owner".into(),
        })
        .unwrap();
    let err = s
        .register_agent(crate::agent::RegisterAgentArgs {
            display_name: "Cogs".into(),
            email: None,
            project: "ferroloop".into(),
            role: crate::agent::AgentRole::Lead,
            extra_areas: vec![],
            create_area: false,
            replace: false,
            at: At::from_unix_millis(11),
            by: "owner".into(),
        })
        .unwrap_err();
    assert!(matches!(err, StoreError::LeadTaken { .. }));

    let second = s
        .register_agent(crate::agent::RegisterAgentArgs {
            display_name: "Cogs".into(),
            email: None,
            project: "ferroloop".into(),
            role: crate::agent::AgentRole::Lead,
            extra_areas: vec![],
            create_area: false,
            replace: true,
            at: At::from_unix_millis(12),
            by: "owner".into(),
        })
        .unwrap();
    assert_eq!(second.status, crate::agent::AgentStatus::Active);
    assert_eq!(
        s.get_agent(&first.id).unwrap().unwrap().status,
        crate::agent::AgentStatus::Retired
    );
}

fn reports_to_tracks_active_project_lead_or_owner<S: AgentRegistry>(s: &S) {
    let lead = s
        .register_agent(crate::agent::RegisterAgentArgs {
            display_name: "Ferris".into(),
            email: None,
            project: "ferroloop".into(),
            role: crate::agent::AgentRole::Lead,
            extra_areas: vec![],
            create_area: true,
            replace: false,
            at: At::from_unix_millis(10),
            by: "owner".into(),
        })
        .unwrap();
    let assistant = s
        .register_agent(crate::agent::RegisterAgentArgs {
            display_name: "Cogs".into(),
            email: None,
            project: "ferroloop".into(),
            role: crate::agent::AgentRole::Assistant,
            extra_areas: vec![],
            create_area: false,
            replace: false,
            at: At::from_unix_millis(11),
            by: "ferris".into(),
        })
        .unwrap();
    let active = s.list_agents(None, false).unwrap();
    assert_eq!(crate::agent::reports_to(&lead, &active), "Owner");
    assert_eq!(crate::agent::reports_to(&assistant, &active), "Ferris");
}

fn registry_event_import_deduplicates_detects_tamper_and_aliases_offline_collision<
    S: AgentRegistry + Handles,
>(
    s: &S,
) {
    let local = s
        .register_agent(crate::agent::RegisterAgentArgs {
            display_name: "Ferris".into(),
            email: None,
            project: "ferroloop".into(),
            role: crate::agent::AgentRole::Lead,
            extra_areas: vec![],
            create_area: true,
            replace: false,
            at: At::from_unix_millis(10),
            by: "owner".into(),
        })
        .unwrap();
    let events: Vec<_> = s
        .registry_events_after(0)
        .unwrap()
        .into_iter()
        .map(|(_, e)| e)
        .collect();
    // Deduplicate identical re-import.
    s.import_registry_events(&events).unwrap();

    // Tamper check on identical event_id with altered payload.
    let mut tampered = events.last().unwrap().clone();
    tampered.by = "mallory".into();
    assert!(matches!(
        s.import_registry_events(&[tampered]),
        Err(StoreError::Tampered { .. })
    ));

    // Offline collision: remote event has a different AgentId for "ferris".
    let remote_id = crate::ids::AgentId(crate::ids::seq_iri(999));
    let mut remote_rec = local.clone();
    remote_rec.id = remote_id.clone();
    remote_rec.display_name = "Ferris Remote".into();
    let collision_ev = crate::agent::RegistryEvent {
        event_id: crate::ids::seq_iri(998),
        at: At::from_unix_millis(20),
        by: "owner".into(),
        kind: crate::agent::RegistryEventKind::AgentUpsert(remote_rec),
    };
    s.import_registry_events(&[collision_ev]).unwrap();
    let resolved = s.get_agent(&local.id).unwrap().unwrap();
    assert_eq!(resolved.id, remote_id);
    assert_eq!(resolved.display_name, "Ferris Remote");
}

fn memory_handoff_and_runtime_crud_and_key_lookup<S: StateStore>(s: &S) {
    let mem = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Workspace,
            slug: "ste-rule".into(),
            sensitivity: crate::state::SensitivityPatch::Inherit,
            payload: crate::state::StatePayload::Memory(crate::state::MemoryEntry {
                category: crate::state::MemoryCategory::Rule,
                basis: crate::state::MemoryBasis::Ruling,
                lifecycle: crate::state::MemoryLifecycle::Active,
                superseded_by: None,
                pinned: true,
                summary: "Use STE for owner prose".into(),
                body: "Body".into(),
            }),
            at: At::from_unix_millis(10),
            by: "jarvis".into(),
        })
        .unwrap();
    assert_eq!(mem.concern, crate::state::StateConcern::Memory);
    assert_eq!(
        s.get_state_by_key(
            &crate::state::StateScope::Workspace,
            crate::state::StateConcern::Memory,
            "ste-rule",
        )
        .unwrap(),
        Some(mem.clone())
    );
    assert_eq!(s.state_revisions(&mem.id).unwrap().len(), 1);

    let handoff = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Project("ferroloop".into()),
            slug: "current".into(),
            sensitivity: crate::state::SensitivityPatch::Inherit,
            payload: crate::state::StatePayload::Handoff(crate::state::HandoffEntry {
                summary: "Task 1 green".into(),
                shipped: vec!["types".into()],
                in_flight: vec![],
                blockers: vec![],
                next_steps: vec!["Task 2".into()],
                notes: None,
            }),
            at: At::from_unix_millis(11),
            by: "ferris".into(),
        })
        .unwrap();
    assert_eq!(handoff.concern, crate::state::StateConcern::Handoff);

    let runtime = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Project("ferroloop".into()),
            slug: "gpu_lease".into(),
            sensitivity: crate::state::SensitivityPatch::Inherit,
            payload: crate::state::StatePayload::Runtime(crate::state::RuntimeEntry {
                value: serde_json::json!({"holder": "ferris"}),
                merge_mode: crate::state::MergeMode::LastWriter,
            }),
            at: At::from_unix_millis(12),
            by: "ferris".into(),
        })
        .unwrap();
    assert_eq!(runtime.concern, crate::state::StateConcern::Runtime);
    assert_eq!(
        s.list_state(
            Some(&crate::state::StateScope::Project("ferroloop".into())),
            None
        )
        .unwrap()
        .len(),
        2
    );
}

fn sensitive_flag_is_sticky_until_explicitly_cleared_and_ever_sensitive_persists<S: StateStore>(
    s: &S,
) {
    let created = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Workspace,
            slug: "token_hint".into(),
            sensitivity: crate::state::SensitivityPatch::SetSensitive,
            payload: crate::state::StatePayload::Runtime(crate::state::RuntimeEntry {
                value: serde_json::json!({"v": 1}),
                merge_mode: crate::state::MergeMode::Conflict,
            }),
            at: At::from_unix_millis(10),
            by: "jarvis".into(),
        })
        .unwrap();
    assert!(created.sensitive);
    assert!(created.ever_sensitive);

    // Omitting sensitivity flag (Inherit) preserves `sensitive == true`.
    let updated = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Workspace,
            slug: "token_hint".into(),
            sensitivity: crate::state::SensitivityPatch::Inherit,
            payload: crate::state::StatePayload::Runtime(crate::state::RuntimeEntry {
                value: serde_json::json!({"v": 2}),
                merge_mode: crate::state::MergeMode::Conflict,
            }),
            at: At::from_unix_millis(11),
            by: "jarvis".into(),
        })
        .unwrap();
    assert!(updated.sensitive);
    assert!(updated.ever_sensitive);

    // Explicit ClearSensitive sets `sensitive = false` while `ever_sensitive` remains true.
    let cleared = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Workspace,
            slug: "token_hint".into(),
            sensitivity: crate::state::SensitivityPatch::ClearSensitive,
            payload: crate::state::StatePayload::Runtime(crate::state::RuntimeEntry {
                value: serde_json::json!({"v": 3}),
                merge_mode: crate::state::MergeMode::Conflict,
            }),
            at: At::from_unix_millis(12),
            by: "jarvis".into(),
        })
        .unwrap();
    assert!(!cleared.sensitive);
    assert!(cleared.ever_sensitive);
}

fn memory_superseded_validates_target_exists_and_is_memory<S: StateStore>(s: &S) {
    let old = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Workspace,
            slug: "old_rule".into(),
            sensitivity: crate::state::SensitivityPatch::Inherit,
            payload: crate::state::StatePayload::Memory(crate::state::MemoryEntry {
                category: crate::state::MemoryCategory::Rule,
                basis: crate::state::MemoryBasis::Ruling,
                lifecycle: crate::state::MemoryLifecycle::Active,
                superseded_by: None,
                pinned: false,
                summary: "Old rule".into(),
                body: "Old".into(),
            }),
            at: At::from_unix_millis(10),
            by: "jarvis".into(),
        })
        .unwrap();

    // Superseding by self is rejected.
    let self_err = s
        .retire_memory(
            &crate::state::StateScope::Workspace,
            "old_rule",
            crate::state::MemoryLifecycle::Superseded,
            Some(old.id.clone()),
            At::from_unix_millis(11),
            "jarvis",
        )
        .unwrap_err();
    assert!(matches!(self_err, StoreError::InvalidState(_)));

    // Superseding by a non-Memory entry is rejected.
    let handoff = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Workspace,
            slug: "current".into(),
            sensitivity: crate::state::SensitivityPatch::Inherit,
            payload: crate::state::StatePayload::Handoff(crate::state::HandoffEntry {
                summary: "Handoff".into(),
                shipped: vec![],
                in_flight: vec![],
                blockers: vec![],
                next_steps: vec![],
                notes: None,
            }),
            at: At::from_unix_millis(12),
            by: "jarvis".into(),
        })
        .unwrap();
    let wrong_concern = s
        .retire_memory(
            &crate::state::StateScope::Workspace,
            "old_rule",
            crate::state::MemoryLifecycle::Superseded,
            Some(handoff.id),
            At::from_unix_millis(13),
            "jarvis",
        )
        .unwrap_err();
    assert!(matches!(wrong_concern, StoreError::InvalidState(_)));

    // Superseding by a valid Memory entry succeeds.
    let new_mem = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Workspace,
            slug: "new_rule".into(),
            sensitivity: crate::state::SensitivityPatch::Inherit,
            payload: crate::state::StatePayload::Memory(crate::state::MemoryEntry {
                category: crate::state::MemoryCategory::Rule,
                basis: crate::state::MemoryBasis::Ruling,
                lifecycle: crate::state::MemoryLifecycle::Active,
                superseded_by: None,
                pinned: false,
                summary: "New rule".into(),
                body: "New".into(),
            }),
            at: At::from_unix_millis(14),
            by: "jarvis".into(),
        })
        .unwrap();
    let retired = s
        .retire_memory(
            &crate::state::StateScope::Workspace,
            "old_rule",
            crate::state::MemoryLifecycle::Superseded,
            Some(new_mem.id.clone()),
            At::from_unix_millis(15),
            "jarvis",
        )
        .unwrap();
    match retired.payload {
        crate::state::StatePayload::Memory(m) => {
            assert_eq!(m.lifecycle, crate::state::MemoryLifecycle::Superseded);
            assert_eq!(m.superseded_by, Some(new_mem.id));
        }
        other => panic!("expected Memory payload, got {other:?}"),
    }
}

fn runtime_append_requires_array_and_rejects_scalar<S: StateStore>(s: &S) {
    let err = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Workspace,
            slug: "events".into(),
            sensitivity: crate::state::SensitivityPatch::Inherit,
            payload: crate::state::StatePayload::Runtime(crate::state::RuntimeEntry {
                value: serde_json::json!({"not": "an array"}),
                merge_mode: crate::state::MergeMode::Append,
            }),
            at: At::from_unix_millis(10),
            by: "jarvis".into(),
        })
        .unwrap_err();
    assert!(matches!(err, StoreError::InvalidState(_)));
}

fn scope_concern_and_slug_are_immutable_for_an_entry_id<S: StateStore>(s: &S) {
    let entry = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Workspace,
            slug: "immutable".into(),
            sensitivity: crate::state::SensitivityPatch::Inherit,
            payload: crate::state::StatePayload::Runtime(crate::state::RuntimeEntry {
                value: serde_json::json!(1),
                merge_mode: crate::state::MergeMode::Conflict,
            }),
            at: At::from_unix_millis(10),
            by: "jarvis".into(),
        })
        .unwrap();
    let bad_rev = crate::state::StateRevision {
        revision_id: crate::ids::RevisionId(crate::ids::seq_iri(900)),
        entry_id: entry.id.clone(),
        parent_revisions: vec![entry.head_revision.clone()],
        scope: crate::state::StateScope::Workspace,
        concern: crate::state::StateConcern::Runtime,
        slug: "renamed".into(),
        sensitive: false,
        ever_sensitive: false,
        payload: crate::state::StatePayload::Runtime(crate::state::RuntimeEntry {
            value: serde_json::json!(2),
            merge_mode: crate::state::MergeMode::Conflict,
        }),
        at: At::from_unix_millis(11),
        by: "jarvis".into(),
    };
    let err = s
        .import_state_revisions(&[bad_rev], At::from_unix_millis(12))
        .unwrap_err();
    assert!(matches!(err, StoreError::Tampered { .. }));
}

fn fast_forward_and_already_ahead_revision_imports_converge<S: StateStore>(s: &S) {
    let v1 = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Workspace,
            slug: "ff".into(),
            sensitivity: crate::state::SensitivityPatch::Inherit,
            payload: crate::state::StatePayload::Runtime(crate::state::RuntimeEntry {
                value: serde_json::json!(1),
                merge_mode: crate::state::MergeMode::Conflict,
            }),
            at: At::from_unix_millis(10),
            by: "jarvis".into(),
        })
        .unwrap();
    let r2_id = crate::ids::RevisionId(crate::ids::seq_iri(901));
    let r2 = crate::state::StateRevision {
        revision_id: r2_id.clone(),
        entry_id: v1.id.clone(),
        parent_revisions: vec![v1.head_revision.clone()],
        scope: v1.scope.clone(),
        concern: v1.concern,
        slug: v1.slug.clone(),
        sensitive: false,
        ever_sensitive: false,
        payload: crate::state::StatePayload::Runtime(crate::state::RuntimeEntry {
            value: serde_json::json!(2),
            merge_mode: crate::state::MergeMode::Conflict,
        }),
        at: At::from_unix_millis(20),
        by: "ferris".into(),
    };
    // Remote is descendant of local -> fast-forward.
    s.import_state_revisions(std::slice::from_ref(&r2), At::from_unix_millis(21))
        .unwrap();
    let after_ff = s.get_state(&v1.id).unwrap().unwrap();
    assert_eq!(after_ff.head_revision, r2_id);
    assert!(s.list_conflicts().unwrap().is_empty());

    // Re-importing the ancestor revision when local is already ahead is a no-op.
    let r1 = s
        .state_revisions(&v1.id)
        .unwrap()
        .into_iter()
        .find(|r| r.revision_id == v1.head_revision)
        .unwrap();
    s.import_state_revisions(&[r1], At::from_unix_millis(22))
        .unwrap();
    assert_eq!(s.get_state(&v1.id).unwrap().unwrap().head_revision, r2_id);
}

fn true_divergence_creates_conflict_blocks_writes_and_resolves_cleanly<S: StateStore>(s: &S) {
    let base = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Workspace,
            slug: "handoff".into(),
            sensitivity: crate::state::SensitivityPatch::Inherit,
            payload: crate::state::StatePayload::Handoff(crate::state::HandoffEntry {
                summary: "base".into(),
                shipped: vec![],
                in_flight: vec![],
                blockers: vec![],
                next_steps: vec![],
                notes: None,
            }),
            at: At::from_unix_millis(10),
            by: "jarvis".into(),
        })
        .unwrap();
    let local = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Workspace,
            slug: "handoff".into(),
            sensitivity: crate::state::SensitivityPatch::Inherit,
            payload: crate::state::StatePayload::Handoff(crate::state::HandoffEntry {
                summary: "local".into(),
                shipped: vec!["a".into()],
                in_flight: vec![],
                blockers: vec![],
                next_steps: vec![],
                notes: None,
            }),
            at: At::from_unix_millis(20),
            by: "jarvis".into(),
        })
        .unwrap();
    let remote_rev_id = crate::ids::RevisionId(crate::ids::seq_iri(902));
    let remote_rev = crate::state::StateRevision {
        revision_id: remote_rev_id.clone(),
        entry_id: base.id.clone(),
        parent_revisions: vec![base.head_revision.clone()],
        scope: base.scope.clone(),
        concern: base.concern,
        slug: base.slug.clone(),
        sensitive: false,
        ever_sensitive: false,
        payload: crate::state::StatePayload::Handoff(crate::state::HandoffEntry {
            summary: "remote".into(),
            shipped: vec!["b".into()],
            in_flight: vec![],
            blockers: vec![],
            next_steps: vec![],
            notes: None,
        }),
        at: At::from_unix_millis(25),
        by: "ferris".into(),
    };
    s.import_state_revisions(&[remote_rev], At::from_unix_millis(30))
        .unwrap();

    let conflicts = s.list_conflicts().unwrap();
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].entry_id, base.id);
    assert_eq!(conflicts[0].local_head, local.head_revision);
    assert_eq!(conflicts[0].remote_head, remote_rev_id);

    // Regular writes are blocked while conflicted.
    let blocked = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Workspace,
            slug: "handoff".into(),
            sensitivity: crate::state::SensitivityPatch::Inherit,
            payload: crate::state::StatePayload::Handoff(crate::state::HandoffEntry {
                summary: "blocked".into(),
                shipped: vec![],
                in_flight: vec![],
                blockers: vec![],
                next_steps: vec![],
                notes: None,
            }),
            at: At::from_unix_millis(31),
            by: "jarvis".into(),
        })
        .unwrap_err();
    assert!(matches!(blocked, StoreError::StateConflict { .. }));

    // Resolve keeping remote_rev_id produces a 2-parent merge revision.
    let resolved = s
        .resolve_conflict(&base.id, &remote_rev_id, At::from_unix_millis(40), "jarvis")
        .unwrap();
    assert!(s.list_conflicts().unwrap().is_empty());
    let revs = s.state_revisions(&base.id).unwrap();
    let merge_rev = revs
        .iter()
        .find(|r| r.revision_id == resolved.head_revision)
        .unwrap();
    assert_eq!(merge_rev.parent_revisions.len(), 2);
}

fn mismatched_runtime_merge_modes_fail_closed_to_conflict<S: StateStore>(s: &S) {
    let base = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Workspace,
            slug: "mode_mismatch".into(),
            sensitivity: crate::state::SensitivityPatch::Inherit,
            payload: crate::state::StatePayload::Runtime(crate::state::RuntimeEntry {
                value: serde_json::json!([1]),
                merge_mode: crate::state::MergeMode::Append,
            }),
            at: At::from_unix_millis(10),
            by: "jarvis".into(),
        })
        .unwrap();
    let _local = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Workspace,
            slug: "mode_mismatch".into(),
            sensitivity: crate::state::SensitivityPatch::Inherit,
            payload: crate::state::StatePayload::Runtime(crate::state::RuntimeEntry {
                value: serde_json::json!([1, 2]),
                merge_mode: crate::state::MergeMode::Append,
            }),
            at: At::from_unix_millis(20),
            by: "jarvis".into(),
        })
        .unwrap();
    let remote = crate::state::StateRevision {
        revision_id: crate::ids::RevisionId(crate::ids::seq_iri(903)),
        entry_id: base.id.clone(),
        parent_revisions: vec![base.head_revision.clone()],
        scope: base.scope.clone(),
        concern: base.concern,
        slug: base.slug.clone(),
        sensitive: false,
        ever_sensitive: false,
        payload: crate::state::StatePayload::Runtime(crate::state::RuntimeEntry {
            value: serde_json::json!([1, 3]),
            merge_mode: crate::state::MergeMode::LastWriter,
        }),
        at: At::from_unix_millis(25),
        by: "ferris".into(),
    };
    s.import_state_revisions(&[remote], At::from_unix_millis(30))
        .unwrap();
    assert_eq!(s.list_conflicts().unwrap().len(), 1);
}

fn runtime_append_and_last_writer_auto_merge_without_conflict<S: StateStore>(s: &S) {
    // 1. Append auto-merge.
    let base = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Workspace,
            slug: "log".into(),
            sensitivity: crate::state::SensitivityPatch::Inherit,
            payload: crate::state::StatePayload::Runtime(crate::state::RuntimeEntry {
                value: serde_json::json!(["a"]),
                merge_mode: crate::state::MergeMode::Append,
            }),
            at: At::from_unix_millis(10),
            by: "jarvis".into(),
        })
        .unwrap();
    let _local = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Workspace,
            slug: "log".into(),
            sensitivity: crate::state::SensitivityPatch::Inherit,
            payload: crate::state::StatePayload::Runtime(crate::state::RuntimeEntry {
                value: serde_json::json!(["a", "b"]),
                merge_mode: crate::state::MergeMode::Append,
            }),
            at: At::from_unix_millis(20),
            by: "jarvis".into(),
        })
        .unwrap();
    let remote = crate::state::StateRevision {
        revision_id: crate::ids::RevisionId(crate::ids::seq_iri(904)),
        entry_id: base.id.clone(),
        parent_revisions: vec![base.head_revision.clone()],
        scope: base.scope.clone(),
        concern: base.concern,
        slug: base.slug.clone(),
        sensitive: false,
        ever_sensitive: false,
        payload: crate::state::StatePayload::Runtime(crate::state::RuntimeEntry {
            value: serde_json::json!(["a", "c"]),
            merge_mode: crate::state::MergeMode::Append,
        }),
        at: At::from_unix_millis(25),
        by: "ferris".into(),
    };
    s.import_state_revisions(&[remote], At::from_unix_millis(30))
        .unwrap();
    assert!(s.list_conflicts().unwrap().is_empty());
    let merged = s.get_state(&base.id).unwrap().unwrap();
    assert_eq!(merged.by, "fl");
    match merged.payload {
        crate::state::StatePayload::Runtime(rt) => {
            assert_eq!(rt.value, serde_json::json!(["a", "b", "c"]));
        }
        other => panic!("expected Runtime, got {other:?}"),
    }

    // 2. LastWriter auto-merge.
    let lw_base = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Workspace,
            slug: "cursor".into(),
            sensitivity: crate::state::SensitivityPatch::Inherit,
            payload: crate::state::StatePayload::Runtime(crate::state::RuntimeEntry {
                value: serde_json::json!(1),
                merge_mode: crate::state::MergeMode::LastWriter,
            }),
            at: At::from_unix_millis(10),
            by: "jarvis".into(),
        })
        .unwrap();
    let _lw_local = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Workspace,
            slug: "cursor".into(),
            sensitivity: crate::state::SensitivityPatch::Inherit,
            payload: crate::state::StatePayload::Runtime(crate::state::RuntimeEntry {
                value: serde_json::json!(2),
                merge_mode: crate::state::MergeMode::LastWriter,
            }),
            at: At::from_unix_millis(20),
            by: "jarvis".into(),
        })
        .unwrap();
    let lw_remote = crate::state::StateRevision {
        revision_id: crate::ids::RevisionId(crate::ids::seq_iri(905)),
        entry_id: lw_base.id.clone(),
        parent_revisions: vec![lw_base.head_revision.clone()],
        scope: lw_base.scope.clone(),
        concern: lw_base.concern,
        slug: lw_base.slug.clone(),
        sensitive: false,
        ever_sensitive: false,
        payload: crate::state::StatePayload::Runtime(crate::state::RuntimeEntry {
            value: serde_json::json!(3),
            merge_mode: crate::state::MergeMode::LastWriter,
        }),
        at: At::from_unix_millis(25),
        by: "ferris".into(),
    };
    s.import_state_revisions(&[lw_remote], At::from_unix_millis(30))
        .unwrap();
    assert!(s.list_conflicts().unwrap().is_empty());
    let lw_merged = s.get_state(&lw_base.id).unwrap().unwrap();
    assert_eq!(lw_merged.by, "fl");
    match lw_merged.payload {
        crate::state::StatePayload::Runtime(rt) => {
            assert_eq!(rt.value, serde_json::json!(3));
        }
        other => panic!("expected Runtime, got {other:?}"),
    }
}

fn offline_same_key_collision_aliases_unpublished_local_id_to_remote<S: StateStore + Handles>(
    s: &S,
) {
    let local = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Workspace,
            slug: "shared_slug".into(),
            sensitivity: crate::state::SensitivityPatch::Inherit,
            payload: crate::state::StatePayload::Runtime(crate::state::RuntimeEntry {
                value: serde_json::json!({"side": "local"}),
                merge_mode: crate::state::MergeMode::LastWriter,
            }),
            at: At::from_unix_millis(10),
            by: "jarvis".into(),
        })
        .unwrap();
    let local_handle = s.handle_of(Kind::State, local.id.iri()).unwrap().unwrap();

    let remote_id = crate::ids::StateId(crate::ids::seq_iri(950));
    let remote_rev = crate::state::StateRevision {
        revision_id: crate::ids::RevisionId(crate::ids::seq_iri(951)),
        entry_id: remote_id.clone(),
        parent_revisions: vec![],
        scope: crate::state::StateScope::Workspace,
        concern: crate::state::StateConcern::Runtime,
        slug: "shared_slug".into(),
        sensitive: false,
        ever_sensitive: false,
        payload: crate::state::StatePayload::Runtime(crate::state::RuntimeEntry {
            value: serde_json::json!({"side": "remote"}),
            merge_mode: crate::state::MergeMode::LastWriter,
        }),
        at: At::from_unix_millis(20),
        by: "ferris".into(),
    };
    s.import_state_revisions(&[remote_rev], At::from_unix_millis(25))
        .unwrap();

    // `local.id` now resolves via `aliases` to `remote_id`, and numeric handle is preserved.
    let by_old_id = s.get_state(&local.id).unwrap().unwrap();
    assert_eq!(by_old_id.id, remote_id);
    assert_eq!(
        s.resolve_handle(Kind::State, local_handle).unwrap(),
        Some(remote_id.0)
    );
}

fn direct_and_unregistered_persona_addressing_round_trips_across_registration<
    S: MessageStore + AgentRegistry + Catalog + Handles,
>(
    s: &S,
) {
    use crate::ferrowire::{
        DeliveryStatus, MessageKind, MessageUrgency, SendWireArgs, WireAddress, WireFilter,
        fallback_agent_iri,
    };

    // 1. Send to an unregistered persona "ferris" -> uses fallback urn:fl:agent:ferris.
    let (msg1, rcpts1) = s
        .send_wire(SendWireArgs {
            id: None,
            sender_id: None,
            sender_name: "jarvis".into(),
            sender_host: Some("ai-lab-0".into()),
            to: WireAddress::AgentName("ferris".into()),
            kind: MessageKind::Mail,
            urgency: MessageUrgency::Normal,
            durable: false,
            subject: "Welcome".into(),
            body: "Cold roster mail".into(),
            reply_to: None,
            thread_id: None,
            deadline_at: None,
            at: At::from_unix_millis(10),
        })
        .unwrap();
    assert_eq!(s.kind_of(msg1.id.iri()).unwrap(), Kind::Message);
    assert_eq!(s.handle_of(Kind::Message, msg1.id.iri()).unwrap(), None);
    assert_eq!(rcpts1.len(), 1);
    assert_eq!(rcpts1[0].recipient_id, fallback_agent_iri("ferris"));
    assert_eq!(rcpts1[0].recipient_name, "ferris");

    // 2. Register Ferris in the AgentRegistry.
    let ferris = s
        .register_agent(crate::agent::RegisterAgentArgs {
            display_name: "Ferris".into(),
            email: None,
            project: "ferroloop".into(),
            role: crate::agent::AgentRole::Lead,
            extra_areas: vec![],
            create_area: true,
            replace: false,
            at: At::from_unix_millis(15),
            by: "owner".into(),
        })
        .unwrap();

    // 3. Send a second message to Ferris now that Ferris is registered.
    let (msg2, rcpts2) = s
        .send_wire(SendWireArgs {
            id: None,
            sender_id: None,
            sender_name: "jarvis".into(),
            sender_host: None,
            to: WireAddress::AgentName("ferris".into()),
            kind: MessageKind::Mail,
            urgency: MessageUrgency::Urgent,
            durable: false,
            subject: "Registered mail".into(),
            body: "Warm roster mail".into(),
            reply_to: None,
            thread_id: None,
            deadline_at: None,
            at: At::from_unix_millis(20),
        })
        .unwrap();
    assert_eq!(rcpts2[0].recipient_id, ferris.id.0);

    // 4. Querying inbox by name ("Ferris") or by AgentId URN returns BOTH messages.
    let by_name = s
        .list_wire_inbox(&WireFilter {
            recipient: Some("Ferris".into()),
            unread_only: true,
            ..WireFilter::default()
        })
        .unwrap();
    assert_eq!(by_name.len(), 2);
    assert_eq!(by_name[0].message.id, msg1.id);
    assert_eq!(by_name[1].message.id, msg2.id);

    let by_id = s
        .list_wire_inbox(&WireFilter {
            recipient: Some(ferris.id.to_string()),
            unread_only: true,
            ..WireFilter::default()
        })
        .unwrap();
    assert_eq!(by_id.len(), 2);

    // 5. Acking msg1 by Ferris's AgentId URN transitions the pre-registration receipt.
    let acked = s
        .update_wire_receipt(
            &msg1.id,
            ferris.id.as_str(),
            DeliveryStatus::Acked,
            false,
            At::from_unix_millis(25),
        )
        .unwrap()
        .unwrap();
    assert_eq!(acked.status, DeliveryStatus::Acked);
    assert_eq!(acked.acked_at, Some(At::from_unix_millis(25)));
}

fn group_selectors_expand_active_agents_and_exclude_sender<
    S: MessageStore + AgentRegistry + Catalog + Handles,
>(
    s: &S,
) {
    use crate::ferrowire::{MessageKind, MessageUrgency, SendWireArgs, WireAddress};

    let jarvis = s
        .register_agent(crate::agent::RegisterAgentArgs {
            display_name: "Jarvis".into(),
            email: None,
            project: "ai-lab-amd".into(),
            role: crate::agent::AgentRole::LabManager,
            extra_areas: vec!["ferroloop".into()],
            create_area: true,
            replace: false,
            at: At::from_unix_millis(10),
            by: "owner".into(),
        })
        .unwrap();
    let ferris = s
        .register_agent(crate::agent::RegisterAgentArgs {
            display_name: "Ferris".into(),
            email: None,
            project: "ferroloop".into(),
            role: crate::agent::AgentRole::Lead,
            extra_areas: vec![],
            create_area: true,
            replace: false,
            at: At::from_unix_millis(11),
            by: "owner".into(),
        })
        .unwrap();
    let cogs = s
        .register_agent(crate::agent::RegisterAgentArgs {
            display_name: "Cogs".into(),
            email: None,
            project: "ferroloop".into(),
            role: crate::agent::AgentRole::Assistant,
            extra_areas: vec![],
            create_area: false,
            replace: false,
            at: At::from_unix_millis(12),
            by: "ferris".into(),
        })
        .unwrap();
    let retired = s
        .register_agent(crate::agent::RegisterAgentArgs {
            display_name: "OldBot".into(),
            email: None,
            project: "ferroloop".into(),
            role: crate::agent::AgentRole::Assistant,
            extra_areas: vec![],
            create_area: false,
            replace: false,
            at: At::from_unix_millis(13),
            by: "ferris".into(),
        })
        .unwrap();
    s.retire_agent(&retired.id, At::from_unix_millis(14), "ferris")
        .unwrap();

    // @all from ferris -> jarvis and cogs (excludes sender ferris and retired oldbot).
    let (_, all_rcpts) = s
        .send_wire(SendWireArgs {
            id: None,
            sender_id: Some(ferris.id.clone()),
            sender_name: "ferris".into(),
            sender_host: None,
            to: WireAddress::All,
            kind: MessageKind::Mail,
            urgency: MessageUrgency::Normal,
            durable: false,
            subject: "Broadcast".into(),
            body: "Hello all".into(),
            reply_to: None,
            thread_id: None,
            deadline_at: None,
            at: At::from_unix_millis(20),
        })
        .unwrap();
    let names: Vec<_> = all_rcpts
        .iter()
        .map(|r| r.recipient_name.as_str())
        .collect();
    assert_eq!(names, vec!["cogs", "jarvis"]);

    // @area:ferroloop from jarvis -> cogs and ferris.
    let (_, area_rcpts) = s
        .send_wire(SendWireArgs {
            id: None,
            sender_id: Some(jarvis.id.clone()),
            sender_name: "jarvis".into(),
            sender_host: None,
            to: WireAddress::Area("ferroloop".into()),
            kind: MessageKind::Mail,
            urgency: MessageUrgency::Normal,
            durable: false,
            subject: "Area".into(),
            body: "FerroLoop team".into(),
            reply_to: None,
            thread_id: None,
            deadline_at: None,
            at: At::from_unix_millis(21),
        })
        .unwrap();
    let area_names: Vec<_> = area_rcpts
        .iter()
        .map(|r| r.recipient_name.as_str())
        .collect();
    assert_eq!(area_names, vec!["cogs", "ferris"]);

    // @area:ferroloop/lead from cogs -> ferris only.
    let (_, role_rcpts) = s
        .send_wire(SendWireArgs {
            id: None,
            sender_id: Some(cogs.id),
            sender_name: "cogs".into(),
            sender_host: None,
            to: WireAddress::AreaRole {
                area: "ferroloop".into(),
                role: crate::agent::AgentRole::Lead,
            },
            kind: MessageKind::Mail,
            urgency: MessageUrgency::Normal,
            durable: false,
            subject: "Lead only".into(),
            body: "PR ready".into(),
            reply_to: None,
            thread_id: None,
            deadline_at: None,
            at: At::from_unix_millis(22),
        })
        .unwrap();
    assert_eq!(role_rcpts.len(), 1);
    assert_eq!(role_rcpts[0].recipient_name, "ferris");
}

fn ask_reply_and_thread_indexing_validate_and_correlate<S: MessageStore>(s: &S) {
    use crate::ferrowire::{MessageKind, MessageUrgency, SendWireArgs, WireAddress};

    // Reply without parent is refused.
    let err = s
        .send_wire(SendWireArgs {
            id: None,
            sender_id: None,
            sender_name: "ferris".into(),
            sender_host: None,
            to: WireAddress::AgentName("jarvis".into()),
            kind: MessageKind::Reply,
            urgency: MessageUrgency::Normal,
            durable: false,
            subject: "Re: orphan".into(),
            body: "No parent".into(),
            reply_to: None,
            thread_id: None,
            deadline_at: None,
            at: At::from_unix_millis(10),
        })
        .unwrap_err();
    assert!(matches!(err, StoreError::InvalidState(_)));

    // Ask forces durable = true and sets thread_id == id.
    let (ask, _) = s
        .send_wire(SendWireArgs {
            id: None,
            sender_id: None,
            sender_name: "ferris".into(),
            sender_host: None,
            to: WireAddress::AgentName("jarvis".into()),
            kind: MessageKind::Ask,
            urgency: MessageUrgency::Urgent,
            durable: false,
            subject: "GPU lease?".into(),
            body: "May we run benchmarks?".into(),
            reply_to: None,
            thread_id: None,
            deadline_at: Some(At::from_unix_millis(60_000)),
            at: At::from_unix_millis(11),
        })
        .unwrap();
    assert!(ask.durable);
    assert_eq!(ask.thread_id, ask.id);

    // Reply inherits thread_id and sets correlation_id = Some(ask.id).
    let (reply, _) = s
        .send_wire(SendWireArgs {
            id: None,
            sender_id: None,
            sender_name: "jarvis".into(),
            sender_host: None,
            to: WireAddress::AgentName("ferris".into()),
            kind: MessageKind::Reply,
            urgency: MessageUrgency::Normal,
            durable: false,
            subject: "Re: GPU lease?".into(),
            body: "Approved".into(),
            reply_to: Some(ask.id.clone()),
            thread_id: None,
            deadline_at: None,
            at: At::from_unix_millis(12),
        })
        .unwrap();
    assert_eq!(reply.thread_id, ask.id);
    assert_eq!(reply.correlation_id, Some(ask.id.clone()));

    let thread = s.list_wire_thread(&ask.thread_id).unwrap();
    assert_eq!(thread, vec![ask.clone(), reply.clone()]);

    let replies = s.list_wire_replies(&ask.id).unwrap();
    assert_eq!(replies, vec![reply]);
}

fn receipt_transitions_and_batch_imports_converge_monotonically<S: MessageStore>(s: &S) {
    use crate::ferrowire::{
        DeliveryReceipt, DeliveryStatus, MessageKind, MessageUrgency, SendWireArgs, WireAddress,
    };

    let (msg, rcpts) = s
        .send_wire(SendWireArgs {
            id: None,
            sender_id: None,
            sender_name: "jarvis".into(),
            sender_host: None,
            to: WireAddress::AgentName("ferris".into()),
            kind: MessageKind::Mail,
            urgency: MessageUrgency::Normal,
            durable: true,
            subject: "Status".into(),
            body: "Check in".into(),
            reply_to: None,
            thread_id: None,
            deadline_at: None,
            at: At::from_unix_millis(10),
        })
        .unwrap();
    let r0 = &rcpts[0];
    assert_eq!(r0.status, DeliveryStatus::Pending);
    assert_eq!(r0.attempts, 0);

    // Failed push increments attempts while staying Pending.
    let r1 = s
        .update_wire_receipt(
            &msg.id,
            "ferris",
            DeliveryStatus::Pending,
            true,
            At::from_unix_millis(15),
        )
        .unwrap()
        .unwrap();
    assert_eq!(r1.status, DeliveryStatus::Pending);
    assert_eq!(r1.attempts, 1);

    // Ack transitions to Acked.
    let r2 = s
        .update_wire_receipt(
            &msg.id,
            "ferris",
            DeliveryStatus::Acked,
            false,
            At::from_unix_millis(20),
        )
        .unwrap()
        .unwrap();
    assert_eq!(r2.status, DeliveryStatus::Acked);
    assert_eq!(r2.acked_at, Some(At::from_unix_millis(20)));

    // Out-of-order Delivered receipt with higher attempts merges without downgrading Acked.
    let late_delivered = DeliveryReceipt {
        message_id: msg.id.clone(),
        recipient_id: r0.recipient_id.clone(),
        recipient_name: "ferris".into(),
        status: DeliveryStatus::Delivered,
        attempts: 4,
        updated_at: At::from_unix_millis(25),
        acked_at: None,
    };
    s.import_wire_batch(&[], &[late_delivered]).unwrap();
    let final_rcpts = s.list_wire_receipts(&msg.id).unwrap();
    assert_eq!(final_rcpts.len(), 1);
    assert_eq!(final_rcpts[0].status, DeliveryStatus::Acked);
    assert_eq!(final_rcpts[0].attempts, 4);
    assert_eq!(final_rcpts[0].updated_at, At::from_unix_millis(25));
    assert_eq!(final_rcpts[0].acked_at, Some(At::from_unix_millis(20)));
}

fn wire_import_deduplicates_and_refuses_tampered_message<S: MessageStore>(s: &S) {
    use crate::ferrowire::{MessageKind, MessageUrgency, SendWireArgs, WireAddress};

    let (msg, rcpts) = s
        .send_wire(SendWireArgs {
            id: None,
            sender_id: None,
            sender_name: "jarvis".into(),
            sender_host: None,
            to: WireAddress::AgentName("ferris".into()),
            kind: MessageKind::Handoff,
            urgency: MessageUrgency::Normal,
            durable: false,
            subject: "Handoff".into(),
            body: "Original body".into(),
            reply_to: None,
            thread_id: None,
            deadline_at: None,
            at: At::from_unix_millis(10),
        })
        .unwrap();
    assert!(msg.durable, "Handoff forces durable = true");

    let msg_log_len = s.wire_messages_after(0).unwrap().len();
    let rcpt_log_len = s.wire_receipts_after(0).unwrap().len();

    // Re-importing identical message and receipt is a no-op.
    s.import_wire_batch(std::slice::from_ref(&msg), &rcpts)
        .unwrap();
    assert_eq!(s.wire_messages_after(0).unwrap().len(), msg_log_len);
    assert_eq!(s.wire_receipts_after(0).unwrap().len(), rcpt_log_len);

    // Tampering with an existing message ID is refused.
    let mut tampered = msg.clone();
    tampered.body = "Tampered body".into();
    let err = s.import_wire_batch(&[tampered], &[]).unwrap_err();
    assert!(matches!(err, StoreError::WireMessageTampered { .. }));
}

fn offline_agent_id_collision_alias_preserves_wire_inbox_and_ack<
    S: MessageStore + AgentRegistry + Handles,
>(
    s: &S,
) {
    use crate::ferrowire::{
        DeliveryStatus, MessageKind, MessageUrgency, SendWireArgs, WireAddress, WireFilter,
    };

    let local_ferris = s
        .register_agent(crate::agent::RegisterAgentArgs {
            display_name: "Ferris".into(),
            email: None,
            project: "ferroloop".into(),
            role: crate::agent::AgentRole::Lead,
            extra_areas: vec![],
            create_area: true,
            replace: false,
            at: At::from_unix_millis(10),
            by: "owner".into(),
        })
        .unwrap();

    // Send a message to Ferris while Ferris has `local_ferris.id`.
    let (msg, _) = s
        .send_wire(SendWireArgs {
            id: None,
            sender_id: None,
            sender_name: "jarvis".into(),
            sender_host: None,
            to: WireAddress::AgentName("ferris".into()),
            kind: MessageKind::Mail,
            urgency: MessageUrgency::Normal,
            durable: true,
            subject: "Before alias".into(),
            body: "Sent to local AgentId".into(),
            reply_to: None,
            thread_id: None,
            deadline_at: None,
            at: At::from_unix_millis(12),
        })
        .unwrap();

    // Import a remote RegistryEvent that aliases `local_ferris.id` -> `remote_id`.
    let remote_id = crate::ids::AgentId(crate::ids::seq_iri(999));
    let mut remote_rec = local_ferris.clone();
    remote_rec.id = remote_id.clone();
    let ev = crate::agent::RegistryEvent {
        event_id: crate::ids::seq_iri(998),
        at: At::from_unix_millis(20),
        by: "owner".into(),
        kind: crate::agent::RegistryEventKind::AgentUpsert(remote_rec),
    };
    s.import_registry_events(&[ev]).unwrap();

    // Querying inbox by the new canonical `remote_id` or old `local_ferris.id` or `"ferris"` finds `msg`.
    for query in [remote_id.as_str(), local_ferris.id.as_str(), "ferris"] {
        let inbox = s
            .list_wire_inbox(&WireFilter {
                recipient: Some(query.to_string()),
                unread_only: true,
                ..WireFilter::default()
            })
            .unwrap();
        assert_eq!(inbox.len(), 1, "query {query} should find the message");
        assert_eq!(inbox[0].message.id, msg.id);
    }

    // Acking via the new canonical `remote_id` succeeds.
    let acked = s
        .update_wire_receipt(
            &msg.id,
            remote_id.as_str(),
            DeliveryStatus::Acked,
            false,
            At::from_unix_millis(30),
        )
        .unwrap()
        .unwrap();
    assert_eq!(acked.status, DeliveryStatus::Acked);
}

fn prune_wire_removes_only_acked_or_expired_transient_messages_before_cutoff<S: MessageStore>(
    s: &S,
) {
    use crate::ferrowire::{
        DeliveryStatus, MessageKind, MessageUrgency, SendWireArgs, WireAddress,
    };

    // 1. Acked transient message before cutoff -> pruned.
    let (m_acked, _) = s
        .send_wire(SendWireArgs {
            id: None,
            sender_id: None,
            sender_name: "jarvis".into(),
            sender_host: None,
            to: WireAddress::AgentName("ferris".into()),
            kind: MessageKind::Mail,
            urgency: MessageUrgency::Normal,
            durable: false,
            subject: "Transient acked".into(),
            body: "Prune me".into(),
            reply_to: None,
            thread_id: None,
            deadline_at: None,
            at: At::from_unix_millis(10),
        })
        .unwrap();
    s.update_wire_receipt(
        &m_acked.id,
        "ferris",
        DeliveryStatus::Acked,
        false,
        At::from_unix_millis(11),
    )
    .unwrap();

    // 2. Pending transient message before cutoff -> kept.
    let (m_pending, _) = s
        .send_wire(SendWireArgs {
            id: None,
            sender_id: None,
            sender_name: "jarvis".into(),
            sender_host: None,
            to: WireAddress::AgentName("ferris".into()),
            kind: MessageKind::Mail,
            urgency: MessageUrgency::Normal,
            durable: false,
            subject: "Transient pending".into(),
            body: "Keep me".into(),
            reply_to: None,
            thread_id: None,
            deadline_at: None,
            at: At::from_unix_millis(12),
        })
        .unwrap();

    // 3. Acked durable message before cutoff -> kept.
    let (m_durable, _) = s
        .send_wire(SendWireArgs {
            id: None,
            sender_id: None,
            sender_name: "jarvis".into(),
            sender_host: None,
            to: WireAddress::AgentName("ferris".into()),
            kind: MessageKind::Mail,
            urgency: MessageUrgency::Normal,
            durable: true,
            subject: "Durable acked".into(),
            body: "Keep durable".into(),
            reply_to: None,
            thread_id: None,
            deadline_at: None,
            at: At::from_unix_millis(13),
        })
        .unwrap();
    s.update_wire_receipt(
        &m_durable.id,
        "ferris",
        DeliveryStatus::Acked,
        false,
        At::from_unix_millis(14),
    )
    .unwrap();

    let pruned = s.prune_wire(At::from_unix_millis(20)).unwrap();
    assert_eq!(pruned, 1);
    assert!(s.get_wire_message(&m_acked.id).unwrap().is_none());
    assert!(s.get_wire_message(&m_pending.id).unwrap().is_some());
    assert!(s.get_wire_message(&m_durable.id).unwrap().is_some());
}

fn sample_workflow_def(
    id: WorkflowId,
    project: ProjectId,
    slug: &str,
) -> crate::workflow::WorkflowDef {
    use crate::workflow::{
        AssigneeRule, DEFAULT_MAX_TOTAL_STEPS, DEFAULT_STEP_MAX_ATTEMPTS, DEFAULT_STEP_MAX_VISITS,
         END_STEP, EdgeCondition, EdgeDef, JoinPolicy, StepCompletion, StepDef, WorkflowDef,
    };
    use std::collections::BTreeMap;

    WorkflowDef {
        id,
        project,
        slug: slug.to_string(),
        title: format!("Workflow {slug}"),
        start_step: "implement".to_string(),
        steps: vec![
            StepDef {
                step_id: "implement".to_string(),
                title: "Implement".to_string(),
                prompt: "Write the code.".to_string(),
                assignee: AssigneeRule::Agent {
                    name: "felicia".to_string(),
                },
                reviews_step: None,
                require_different_vendor: false,
                completion: StepCompletion::Ungated,
                join_policy: JoinPolicy::All,
                max_attempts: DEFAULT_STEP_MAX_ATTEMPTS,
                max_visits: DEFAULT_STEP_MAX_VISITS,
                budget_usd_micros: None,
                max_tokens: None,
                isolate_worktree: false,
            },
            StepDef {
                step_id: "review".to_string(),
                title: "Review".to_string(),
                prompt: "Review the code.".to_string(),
                assignee: AssigneeRule::Agent {
                    name: "ferris".to_string(),
                },
                reviews_step: Some("implement".to_string()),
                require_different_vendor: false,
                completion: StepCompletion::Ungated,
                join_policy: JoinPolicy::All,
                max_attempts: DEFAULT_STEP_MAX_ATTEMPTS,
                max_visits: DEFAULT_STEP_MAX_VISITS,
                budget_usd_micros: None,
                max_tokens: None,
                isolate_worktree: false,
            },
        ],
        edges: vec![
            EdgeDef {
                from_step: "implement".to_string(),
                to_step: "review".to_string(),
                on: EdgeCondition::Pass,
            },
            EdgeDef {
                from_step: "review".to_string(),
                to_step: "implement".to_string(),
                on: EdgeCondition::Fail,
            },
            EdgeDef {
                from_step: "review".to_string(),
                to_step: END_STEP.to_string(),
                on: EdgeCondition::Pass,
            },
        ],
        max_total_steps: DEFAULT_MAX_TOTAL_STEPS,
        budget_usd_micros: Some(5_000_000),
        max_tokens: Some(100_000),
        declared_keys: BTreeMap::new(),
        authored_at_commit: "0123456789abcdef0123456789abcdef01234567".to_string(),
        authored_by: "ferris".to_string(),
    }
}

fn workflow_def_upserts_resolves_by_slug_and_handle_and_validates_graph<
    S: WorkflowStore + Catalog + Handles,
>(
    s: &S,
) {
    let p = s.add_project("/p").unwrap();
    let def1 = sample_workflow_def(WorkflowId(seq_iri(801)), p.clone(), "review-loop");
    let stored = s.upsert_workflow_def(def1.clone()).unwrap();
    assert_eq!(stored, def1);

    // Handle 1 is assigned to the first workflow definition.
    assert_eq!(s.handle_of(Kind::Workflow, stored.id.iri()).unwrap(), Some(1));
    assert_eq!(
        s.resolve_handle(Kind::Workflow, 1).unwrap().as_ref(),
        Some(stored.id.iri())
    );
    assert_eq!(
        s.get_workflow_def(&stored.id).unwrap(),
        Some(stored.clone())
    );
    assert_eq!(
        s.get_workflow_def_by_slug(&p, "review-loop").unwrap(),
        Some(stored.clone())
    );
    assert_eq!(s.list_workflow_defs(Some(&p)).unwrap(), vec![stored.clone()]);
    assert_eq!(s.list_workflow_defs(None).unwrap(), vec![stored.clone()]);

    // Re-upserting with a different WorkflowId under the same (project, slug) preserves
    // the original WorkflowId and numeric handle 1 while updating the definition.
    let mut updated_input = sample_workflow_def(WorkflowId(seq_iri(802)), p.clone(), "review-loop");
    updated_input.title = "Updated Review Loop".to_string();
    let re_upserted = s.upsert_workflow_def(updated_input).unwrap();
    assert_eq!(re_upserted.id, stored.id);
    assert_eq!(re_upserted.title, "Updated Review Loop");
    assert_eq!(s.handle_of(Kind::Workflow, re_upserted.id.iri()).unwrap(), Some(1));
    assert_eq!(s.resolve_handle(Kind::Workflow, 2).unwrap(), None);
    assert_eq!(
        s.get_workflow_def_by_slug(&p, "review-loop").unwrap(),
        Some(re_upserted)
    );

    // Invalid workflow definition (missing END path) is refused with StoreError::InvalidWorkflow.
    let mut bad = sample_workflow_def(WorkflowId(seq_iri(803)), p, "bad-flow");
    bad.edges.clear();
    let err = s.upsert_workflow_def(bad).unwrap_err();
    assert!(matches!(err, StoreError::InvalidWorkflow(_)), "{err:?}");
}

fn workflow_run_lifecycle_persists_and_filters_by_project_and_workflow<
    S: WorkflowStore + Catalog + Handles,
>(
    s: &S,
) {
    use crate::workflow::{
        StepAttemptReport, StepOutcome, WorkflowRun, WorkflowRunStatus, advance_workflow,
    };

    let p1 = s.add_project("/p1").unwrap();
    let p2 = s.add_project("/p2").unwrap();
    let wf1 = s
        .upsert_workflow_def(sample_workflow_def(
            WorkflowId(seq_iri(810)),
            p1.clone(),
            "flow-one",
        ))
        .unwrap();
    let wf2 = s
        .upsert_workflow_def(sample_workflow_def(
            WorkflowId(seq_iri(811)),
            p2.clone(),
            "flow-two",
        ))
        .unwrap();

    let run1 = WorkflowRun::new(
        WorkflowRunId(seq_iri(820)),
        &wf1,
        "Run One",
        At::from_unix_millis(10),
        "ferris",
    )
    .unwrap();
    let created1 = s.create_workflow_run(run1.clone()).unwrap();
    assert_eq!(created1, run1);
    assert_eq!(
        s.handle_of(Kind::WorkflowRun, created1.id.iri()).unwrap(),
        Some(1)
    );
    assert_eq!(
        s.resolve_handle(Kind::WorkflowRun, 1).unwrap().as_ref(),
        Some(created1.id.iri())
    );

    // Duplicate create_workflow_run with the same ID is refused.
    let dup_err = s.create_workflow_run(run1).unwrap_err();
    assert!(matches!(dup_err, StoreError::AlreadyExists(_)), "{dup_err:?}");

    let run2 = WorkflowRun::new(
        WorkflowRunId(seq_iri(821)),
        &wf2,
        "Run Two",
        At::from_unix_millis(11),
        "ferris",
    )
    .unwrap();
    let created2 = s.create_workflow_run(run2).unwrap();
    assert_eq!(
        s.handle_of(Kind::WorkflowRun, created2.id.iri()).unwrap(),
        Some(2)
    );

    // Advance run1 through implement -> review -> END and persist via update_workflow_run.
    let mut advancing = s.get_workflow_run(&created1.id).unwrap().unwrap();
    advance_workflow(
        &wf1,
        &mut advancing,
        StepAttemptReport {
            step_id: "implement".to_string(),
            outcome: StepOutcome::Pass,
            attempt_iri: Some(seq_iri(830)),
            tokens_used: 1200,
            cost_usd_micros: 45_000,
            reason: None,
        },
        At::from_unix_millis(20),
    )
    .unwrap();
    assert_eq!(advancing.active_steps, vec!["review".to_string()]);
    s.update_workflow_run(advancing.clone()).unwrap();

    advance_workflow(
        &wf1,
        &mut advancing,
        StepAttemptReport {
            step_id: "review".to_string(),
            outcome: StepOutcome::Pass,
            attempt_iri: Some(seq_iri(831)),
            tokens_used: 800,
            cost_usd_micros: 30_000,
            reason: None,
        },
        At::from_unix_millis(30),
    )
    .unwrap();
    assert_eq!(advancing.status, WorkflowRunStatus::Completed);
    let saved = s.update_workflow_run(advancing.clone()).unwrap();
    assert_eq!(s.get_workflow_run(&created1.id).unwrap(), Some(saved.clone()));

    assert_eq!(
        s.list_workflow_runs(Some(&p1), None).unwrap(),
        vec![saved.clone()]
    );
    assert_eq!(
        s.list_workflow_runs(None, Some(&wf2.id)).unwrap(),
        vec![created2.clone()]
    );
    assert_eq!(
        s.list_workflow_runs(None, None).unwrap(),
        vec![saved, created2]
    );

    // Also verify import_workflow_def and import_workflow_run preserve exact IRIs.
    let imported_wf = sample_workflow_def(WorkflowId(seq_iri(840)), p1.clone(), "imported-flow");
    s.import_workflow_def(imported_wf.clone()).unwrap();
    assert_eq!(
        s.get_workflow_def(&imported_wf.id).unwrap(),
        Some(imported_wf.clone())
    );
    assert_eq!(
        s.handle_of(Kind::Workflow, imported_wf.id.iri()).unwrap(),
        Some(3)
    );
    let imported_run = WorkflowRun::new(
        WorkflowRunId(seq_iri(841)),
        &imported_wf,
        "Imported Run",
        At::from_unix_millis(40),
        "ferris",
    )
    .unwrap();
    s.import_workflow_run(imported_run.clone()).unwrap();
    assert_eq!(
        s.get_workflow_run(&imported_run.id).unwrap(),
        Some(imported_run)
    );
    assert_eq!(
        s.handle_of(Kind::WorkflowRun, &seq_iri(841)).unwrap(),
        Some(3)
    );
}

fn workflow_methods_refuse_wrong_kind_ids<S: WorkflowStore + Catalog + Handles>(s: &S) {
    let p = s.add_project("/p").unwrap();
    let wf = s
        .upsert_workflow_def(sample_workflow_def(
            WorkflowId(seq_iri(850)),
            p.clone(),
            "flow",
        ))
        .unwrap();

    // Passing a ProjectId IRI as a WorkflowId or WorkflowRunId returns WrongKind.
    let wrong_wf = s.get_workflow_def(&WorkflowId(p.0.clone())).unwrap_err();
    assert!(
        matches!(
            wrong_wf,
            StoreError::WrongKind {
                expected: Kind::Workflow,
                found: Kind::Project,
                ..
            }
        ),
        "{wrong_wf:?}"
    );

    let wrong_run = s
        .get_workflow_run(&WorkflowRunId(p.0.clone()))
        .unwrap_err();
    assert!(
        matches!(
            wrong_run,
            StoreError::WrongKind {
                expected: Kind::WorkflowRun,
                found: Kind::Project,
                ..
            }
        ),
        "{wrong_run:?}"
    );

    // Passing a WorkflowId IRI where a ProjectId is expected returns WrongKind.
    let wrong_proj = s
        .list_workflow_defs(Some(&ProjectId(wf.id.0.clone())))
        .unwrap_err();
    assert!(
        matches!(
            wrong_proj,
            StoreError::WrongKind {
                expected: Kind::Project,
                found: Kind::Workflow,
                ..
            }
        ),
        "{wrong_proj:?}"
    );
}
