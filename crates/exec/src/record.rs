use crate::decision;
use crate::evaluate::{TransitionReport, evaluate_transition};
use crate::population::{ExecError, refused_publish};
use fl_core::decision::Flushed;
use fl_core::ids::RecordId;
use fl_core::model::{Record, State, Transition};
use fl_core::store::{Roles, StoreError};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::{Mutex, MutexGuard, PoisonError};

pub struct MoveReport {
    pub transitions: Vec<TransitionReport>,
    pub outcome: MoveOutcome,
    /// What the move's flush published, and what it left local and why
    /// (GitHub ledger spec §2.1) — for the command to report.
    pub flushed: Flushed,
}

pub enum MoveOutcome {
    /// No transition covers this move, so nothing was bypassed and nothing
    /// was verified. The caller must say so: "allowed" and "not checked"
    /// must not look alike.
    Ungated,
    Moved,
    /// At least one transition did not pass; the record did not move.
    Refused {
        code: i32,
    },
}

fn store_err(e: impl std::fmt::Display) -> ExecError {
    ExecError::Store(e.to_string())
}

const MOVE_LOCK_STRIPES: usize = 64;

/// In-process locks that serialise the last step of a move: the re-read of the record's state
/// and the write of the new one. One lock per hash stripe of the record id, so two moves of the
/// SAME record always meet on one lock, and moves of different records only collide by chance
/// (a stripe held a moment longer, never a deadlock: a move holds one stripe and nothing else).
static MOVE_LOCKS: [Mutex<()>; MOVE_LOCK_STRIPES] = [const { Mutex::new(()) }; MOVE_LOCK_STRIPES];

fn lock_record_for_move(id: &RecordId) -> MutexGuard<'static, ()> {
    let mut hasher = DefaultHasher::new();
    id.as_str().hash(&mut hasher);
    let stripe = (hasher.finish() % MOVE_LOCK_STRIPES as u64) as usize;
    MOVE_LOCKS[stripe]
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

fn stale_move(began: &Record, now: &Record) -> ExecError {
    ExecError::StaleMove(format!(
        "record {} is now `{}`, not `{}` as when the move began; nothing was moved",
        began.id,
        now.state.as_wire(),
        began.state.as_wire()
    ))
}

/// Move a record, running every transition that covers `(record.state, to)`.
///
/// ⚠ This used to be `set_record_state` and nothing else. `check` would
/// refuse the transition and `record move` would perform the very state
/// change those gates exist to protect — reading nothing, running nothing,
/// exiting 0. A gate that the guarded action does not consult is decoration.
///
/// A transition is addressed by name; a move is addressed by the pair it
/// performs. So the move asks which declarations cover (from, to) and runs
/// every one of them.
///
/// ⚠⚠ Evidence before state (spec §3.5; GitHub ledger spec §2.2,
/// Invariant). The gate runs are appended to the ledger inside
/// `evaluate_transition`; then the move's decision is flushed — one flush
/// per move, however many transitions, a refused or ungated move included;
/// only after both does the tracker state change. A failed flush refuses
/// the move. A crash in between leaves evidence and no move, which is safe
/// to retry. The reverse order would leave a move with no evidence.
///
/// ⚠ The state check and the state write are one step. The record's state is read once before
/// the flush (a stale move is refused before it writes any evidence) and read AGAIN, under a
/// per-record lock, immediately before `set_record_state`; the lock is held across just that
/// re-read and write, never across the gates or the flush, which can run for a long time and
/// reach the network. Two concurrent moves of one record therefore cannot both pass the check
/// and both write: the second sees the first's state and gets `StaleMove`. Because the flush
/// comes before the write, a move that loses at that point has already flushed its decision:
/// evidence with no move, the state this ordering is designed to allow.
///
/// What this does NOT guarantee: the lock lives in this process. Two `fl` processes (or a
/// process and a hub) moving one record can still interleave between the re-read and the write,
/// because the tracker offers a read and a write, not a compare-and-set. Closing that needs a
/// conditional `set_record_state` on every tracker, GitHub's included.
pub fn move_record(roles: Roles<'_>, record: &Record, to: State) -> Result<MoveReport, ExecError> {
    let declared: Vec<Transition> = roles
        .catalog
        .list_transitions(&record.project)
        .map_err(store_err)?
        .into_iter()
        .filter(|t| t.from == record.state && t.to == to)
        .collect();

    let mut transitions = Vec::new();
    let mut worst = 0;
    for t in &declared {
        let report = evaluate_transition(
            roles.catalog,
            roles.ledger,
            &record.project,
            &t.name,
            Some(&record.id),
        )?;
        // Same rule `check` applies: a transition that declares no gates
        // verified nothing, so it cannot authorise a move.
        let code = if report.gates.is_empty() {
            1
        } else {
            report.exit_code()
        };
        worst = worst.max(code);
        transitions.push(report);
    }

    // The gates may have run for a long time. The transitions were chosen from `record.state`;
    // if the record has since been moved (or deleted) they no longer describe this move, and
    // writing `to` would skip whatever gates cover the record's real state.
    let current = roles
        .tracker
        .get_record(&record.id)
        .map_err(store_err)?
        .ok_or_else(|| store_err(StoreError::NoSuchRecord(record.id.clone())))?;
    if current.state != record.state {
        return Err(stale_move(record, &current));
    }

    let outcome = if declared.is_empty() {
        MoveOutcome::Ungated
    } else if worst != 0 {
        MoveOutcome::Refused { code: worst }
    } else {
        MoveOutcome::Moved
    };
    let allowed = !matches!(outcome, MoveOutcome::Refused { .. });

    let flushed = roles
        .ledger
        .flush(decision::for_move(record, to, &transitions, allowed))
        .map_err(refused_publish)?;

    if allowed {
        // Held until the write returns. `current.id` is the primary id, so a move that names
        // the record by an alias takes the same lock as one that names it by its id.
        let _one_move_at_a_time = lock_record_for_move(&current.id);
        let latest = roles
            .tracker
            .get_record(&record.id)
            .map_err(store_err)?
            .ok_or_else(|| store_err(StoreError::NoSuchRecord(record.id.clone())))?;
        if latest.state != record.state {
            return Err(stale_move(record, &latest));
        }
        roles
            .tracker
            .set_record_state(&record.id, to)
            .map_err(|e| match e {
                StoreError::Escalating { .. } | StoreError::Escalated { .. } => {
                    ExecError::Escalation(e.to_string())
                }
                e => store_err(e),
            })?;
    }
    Ok(MoveReport {
        transitions,
        outcome,
        flushed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::Journal;
    use fl_core::decision::Outcome;
    use fl_core::finding::Finding;
    use fl_core::iri::Iri;
    use fl_core::log::{Attempt, GateRun};
    use fl_core::model::{CommandSpec, GateKind, PopulationDelivery, Regret, Selector, Transition};
    use fl_core::store::{Catalog, Ledger, StoreError, Tracker};
    use fl_core::{FindingId, GateId, MemStore, ProjectId};
    use std::process::Command;
    use std::sync::Mutex;

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
        std::fs::write(d.path().join("a.rs"), "fn a() {}").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "first"]);
        d
    }

    /// A ledger that refuses every append: the evidence cannot be written.
    struct DownLedger;
    impl Ledger for DownLedger {
        fn append_gate_run(&self, _: GateRun) -> Result<(), StoreError> {
            Err(StoreError::Backend("ledger down".into()))
        }
        fn append_attempt(&self, _: Attempt) -> Result<(), StoreError> {
            Err(StoreError::Backend("ledger down".into()))
        }
        fn gate_runs(&self, _: &GateId) -> Result<Vec<GateRun>, StoreError> {
            Ok(vec![])
        }
        fn attempts(&self, _: &ProjectId) -> Result<Vec<Attempt>, StoreError> {
            Ok(vec![])
        }
    }

    fn gated_record(store: &MemStore, root: &std::path::Path) -> Record {
        let head = crate::git::Git::head(root).unwrap();
        let p = store.add_project(&root.display().to_string()).unwrap();
        let g = store
            .add_gate(
                &p,
                "passes",
                GateKind::Command(CommandSpec {
                    program: "true".into(),
                    args: vec![],
                    delivery: PopulationDelivery::Args,
                    timeout_secs: 10,
                    pass_codes: vec![0],
                }),
                Selector::Glob {
                    pattern: "*.rs".into(),
                },
                1,
                &head,
                "tester",
            )
            .unwrap();
        store
            .add_transition(Transition {
                project: p.clone(),
                name: "launch".into(),
                from: State::Todo,
                to: State::Done,
                regret: Regret::Low,
                gates: vec![g],
            })
            .unwrap();
        let r = store.add_record(&p, "t").unwrap();
        store.get_record(&r).unwrap().unwrap()
    }

    // ⚠⚠ Spec §3.5 (Invariant): evidence before state. If the gate run
    // cannot be recorded, the record must not move — otherwise the state
    // says "verified" with no evidence behind it.
    #[test]
    fn a_ledger_that_cannot_record_the_evidence_leaves_the_state_unchanged() {
        let d = repo();
        let store = MemStore::default();
        let record = gated_record(&store, d.path());
        let roles = Roles {
            catalog: &store,
            tracker: &store,
            ledger: &DownLedger,
        };

        let err = move_record(roles, &record, State::Done);

        assert!(
            err.is_err(),
            "a move whose evidence was not written must not succeed"
        );
        assert_eq!(
            store.get_record(&record.id).unwrap().unwrap().state,
            State::Todo
        );
    }

    #[test]
    fn a_passing_move_writes_the_evidence_and_then_the_state() {
        let d = repo();
        let store = MemStore::default();
        let record = gated_record(&store, d.path());

        let report = move_record(Roles::single(&store), &record, State::Done).unwrap();

        assert!(matches!(report.outcome, MoveOutcome::Moved));
        assert_eq!(
            store.get_record(&record.id).unwrap().unwrap().state,
            State::Done
        );
        let gate = &store.list_gates(&record.project).unwrap()[0].id;
        assert_eq!(store.gate_runs(gate).unwrap().len(), 1);
    }

    #[test]
    fn an_undeclared_move_is_ungated_and_says_so() {
        let d = repo();
        let store = MemStore::default();
        let record = gated_record(&store, d.path());

        let report = move_record(Roles::single(&store), &record, State::Doing).unwrap();

        assert!(matches!(report.outcome, MoveOutcome::Ungated));
        assert!(report.transitions.is_empty());
    }

    /// A project whose `todo → done` is covered by one transition per
    /// program, each with its own gate over `*.rs`.
    fn covered_by(store: &MemStore, root: &std::path::Path, programs: &[&str]) -> Record {
        let head = crate::git::Git::head(root).unwrap();
        let p = store.add_project(&root.display().to_string()).unwrap();
        for (i, program) in programs.iter().enumerate() {
            let g = store
                .add_gate(
                    &p,
                    &format!("g{i}"),
                    GateKind::Command(CommandSpec {
                        program: (*program).into(),
                        args: vec![],
                        delivery: PopulationDelivery::Args,
                        timeout_secs: 10,
                        pass_codes: vec![0],
                    }),
                    Selector::Glob {
                        pattern: "*.rs".into(),
                    },
                    1,
                    &head,
                    "tester",
                )
                .unwrap();
            store
                .add_transition(Transition {
                    project: p.clone(),
                    name: format!("t{i}"),
                    from: State::Todo,
                    to: State::Done,
                    regret: Regret::Low,
                    gates: vec![g],
                })
                .unwrap();
        }
        let r = store.add_record(&p, "t").unwrap();
        store.get_record(&r).unwrap().unwrap()
    }

    // ⚠⚠ Spec §2.2 (Invariant): evidence before state. Confirmed by
    // mutation: moving the flush below the state change turns this test
    // red.
    #[test]
    fn the_decision_is_flushed_before_the_record_moves() {
        let d = repo();
        let store = MemStore::default();
        let record = gated_record(&store, d.path());
        let j = Journal::new(&store);

        let report = move_record(j.roles(), &record, State::Done).unwrap();

        assert!(matches!(report.outcome, MoveOutcome::Moved));
        assert_eq!(j.events(), vec!["flush", "set_record_state"]);
        assert_eq!(
            report.flushed.commit.as_deref(),
            Some("c1"),
            "the report carries what the flush did, for the command to print"
        );
    }

    // ⚠ Spec §2.2: a flush failure refuses the decision — no state change —
    // and the runs stay in the local store.
    #[test]
    fn a_move_whose_flush_fails_is_refused_and_the_record_stays() {
        let d = repo();
        let store = MemStore::default();
        let record = gated_record(&store, d.path());
        let j = Journal::refusing(&store);

        let err = match move_record(j.roles(), &record, State::Done) {
            Err(e) => e,
            Ok(_) => panic!("a move whose flush failed must be refused"),
        };

        assert!(matches!(err, ExecError::Unpublished(_)), "{err:?}");
        assert!(err.to_string().contains("nothing changed"), "{err}");
        assert_eq!(
            store.get_record(&record.id).unwrap().unwrap().state,
            State::Todo
        );
        let gate = &store.list_gates(&record.project).unwrap()[0].id;
        assert_eq!(
            store.gate_runs(gate).unwrap().len(),
            1,
            "the run is kept in the local store"
        );
        assert_eq!(j.events(), vec!["flush"]);
    }

    // Decision 11: a refused decision is flushed too.
    #[test]
    fn a_refused_move_is_flushed_too() {
        let d = repo();
        let store = MemStore::default();
        let record = covered_by(&store, d.path(), &["false"]);
        let j = Journal::new(&store);

        let report = move_record(j.roles(), &record, State::Done).unwrap();

        assert!(matches!(report.outcome, MoveOutcome::Refused { code: 1 }));
        assert_eq!(
            j.events(),
            vec!["flush"],
            "flushed, and the state untouched"
        );
        match &j.decisions()[0].outcome {
            Outcome::Move {
                allowed,
                transitions,
                ..
            } => {
                assert!(!allowed);
                assert_eq!(transitions.len(), 1);
                assert!(!transitions[0].passed);
            }
            other => panic!("{other:?}"),
        }
    }

    // Spec §2.2: one flush per move, however many transitions cover it; the
    // runs carry the record, and the decision rests on every one of them.
    #[test]
    fn one_move_is_one_flush_however_many_transitions_cover_it() {
        let d = repo();
        let store = MemStore::default();
        let record = covered_by(&store, d.path(), &["true", "true"]);
        let j = Journal::new(&store);

        move_record(j.roles(), &record, State::Done).unwrap();

        assert_eq!(j.events(), vec!["flush", "set_record_state"]);
        let decisions = j.decisions();
        assert_eq!(decisions.len(), 1);
        let runs: Vec<GateRun> = store
            .list_gates(&record.project)
            .unwrap()
            .iter()
            .flat_map(|g| store.gate_runs(&g.id).unwrap())
            .collect();
        assert_eq!(runs.len(), 2);
        assert!(runs.iter().all(|r| r.record.as_ref() == Some(&record.id)));
        let mut cited = decisions[0].rests_on.clone();
        cited.sort();
        let mut ran: Vec<Iri> = runs.iter().map(|r| r.id.clone().unwrap()).collect();
        ran.sort();
        assert_eq!(cited, ran);
        assert_eq!(decisions[0].record, record.id);
    }

    #[test]
    fn an_ungated_move_is_flushed_with_no_transitions() {
        let d = repo();
        let store = MemStore::default();
        let record = gated_record(&store, d.path());
        let j = Journal::new(&store);

        let report = move_record(j.roles(), &record, State::Doing).unwrap();

        assert!(matches!(report.outcome, MoveOutcome::Ungated));
        assert_eq!(j.events(), vec!["flush", "set_record_state"]);
        assert_eq!(
            j.decisions()[0].outcome,
            Outcome::Move {
                from: State::Todo,
                to: State::Doing,
                transitions: vec![],
                allowed: true,
            }
        );
    }

    // Spec §7: only an unreachable or rate-limited GitHub may be promised a
    // later publish. A ledger that refused for any other cause names what
    // to fix, and the refusal must not tell the person to wait it out.
    #[test]
    fn a_move_refused_for_a_cause_a_retry_cannot_cure_promises_no_retry() {
        let d = repo();
        let store = MemStore::default();
        let record = gated_record(&store, d.path());
        let j = Journal::refusing_with(&store, || StoreError::Tampered {
            id: fl_core::ids::seq_iri(99),
            detail: "edited".into(),
        });

        let err = match move_record(j.roles(), &record, State::Done) {
            Err(e) => e,
            Ok(_) => panic!("a move whose flush failed must be refused"),
        };

        assert!(matches!(err, ExecError::PublishRefused(_)), "{err:?}");
        let msg = err.to_string();
        assert!(msg.contains("nothing changed"), "{msg}");
        assert!(msg.contains("edited"), "the cause is named: {msg}");
        assert!(!msg.contains("next decision"), "{msg}");
        assert_eq!(
            store.get_record(&record.id).unwrap().unwrap().state,
            State::Todo
        );
    }

    /// Audit R-1: the transitions were picked from the caller's snapshot of the record and the
    /// new state was then written without looking again, so a record that had moved meanwhile
    /// was moved on by gates that belong to a state it was no longer in.
    #[test]
    fn a_move_from_a_stale_snapshot_is_refused_and_writes_nothing() {
        let d = repo();
        let store = MemStore::default();
        let stale = gated_record(&store, d.path());
        // Someone else moves the record on while this move's snapshot is in flight.
        store.set_record_state(&stale.id, State::Doing).unwrap();
        let journal = Journal::new(&store);
        let err = match move_record(journal.roles(), &stale, State::Done) {
            Err(e) => e,
            Ok(_) => panic!("a stale move was carried out"),
        };
        assert!(matches!(err, ExecError::StaleMove(_)), "{err:?}");
        assert_eq!(
            err.to_string(),
            format!(
                "record {} is now `doing`, not `todo` as when the move began; nothing was moved",
                stale.id
            )
        );
        assert_eq!(
            store.get_record(&stale.id).unwrap().unwrap().state,
            State::Doing
        );
        assert!(
            !journal.events().contains(&"flush"),
            "a refused-as-stale move must not flush a decision: {:?}",
            journal.events()
        );
    }

    // --- the check and the write are one step ------------------------------------------------

    /// A store that can be shared between threads: one record behind a mutex, no catalog (every
    /// move is ungated), and a flush that can rendezvous the movers or move the record.
    struct Shared {
        record: Mutex<Record>,
        /// Every mover waits here during its flush, so all have passed the pre-flush check
        /// before any of them writes.
        at_flush: Option<std::sync::Barrier>,
        /// Run during the flush, as if another mover finished meanwhile.
        on_flush: Option<fn(&Shared)>,
        /// Widens the window between a read and the write that follows it.
        read_pause: std::time::Duration,
    }

    impl Shared {
        fn new(at_flush: Option<std::sync::Barrier>, on_flush: Option<fn(&Shared)>) -> Self {
            Self {
                record: Mutex::new(Record {
                    id: RecordId(fl_core::ids::seq_iri(1)),
                    project: ProjectId(fl_core::ids::seq_iri(2)),
                    title: "t".into(),
                    state: State::Todo,
                    also_known_as: vec![],
                    area: None,
                }),
                at_flush,
                on_flush,
                read_pause: std::time::Duration::from_millis(2),
            }
        }
        fn snapshot(&self) -> Record {
            self.record.lock().unwrap().clone()
        }
        fn roles(&self) -> Roles<'_> {
            Roles {
                catalog: self,
                tracker: self,
                ledger: self,
            }
        }
    }

    impl fl_core::store::Catalog for Shared {
        fn add_project(&self, _: &str) -> Result<ProjectId, StoreError> {
            unreachable!("not used")
        }
        fn get_project(
            &self,
            _: &ProjectId,
        ) -> Result<Option<fl_core::model::Project>, StoreError> {
            unreachable!("not used")
        }
        fn list_projects(&self) -> Result<Vec<fl_core::model::Project>, StoreError> {
            unreachable!("not used")
        }
        fn add_gate(
            &self,
            _: &ProjectId,
            _: &str,
            _: GateKind,
            _: Selector,
            _: u64,
            _: &str,
            _: &str,
        ) -> Result<GateId, StoreError> {
            unreachable!("not used")
        }
        fn get_gate(&self, _: &GateId) -> Result<Option<fl_core::model::GateDef>, StoreError> {
            unreachable!("not used")
        }
        fn list_gates(&self, _: &ProjectId) -> Result<Vec<fl_core::model::GateDef>, StoreError> {
            unreachable!("not used")
        }
        fn update_gate(&self, _: &fl_core::model::GateDef) -> Result<(), StoreError> {
            unreachable!("not used")
        }
        fn add_transition(&self, _: Transition) -> Result<(), StoreError> {
            unreachable!("not used")
        }
        fn get_transition(&self, _: &ProjectId, _: &str) -> Result<Option<Transition>, StoreError> {
            unreachable!("not used")
        }
        fn list_transitions(&self, _: &ProjectId) -> Result<Vec<Transition>, StoreError> {
            Ok(vec![])
        }
        fn kind_of(&self, _: &Iri) -> Result<fl_core::ids::Kind, StoreError> {
            unreachable!("not used")
        }
    }

    impl Tracker for Shared {
        fn add_record_with_area(
            &self,
            _: &ProjectId,
            _: &str,
            _: Option<&str>,
        ) -> Result<RecordId, StoreError> {
            unreachable!("not used")
        }
        fn get_record(&self, _: &RecordId) -> Result<Option<Record>, StoreError> {
            let seen = self.snapshot();
            // The stale answer is returned after a pause, as a slow tracker would.
            std::thread::sleep(self.read_pause);
            Ok(Some(seen))
        }
        fn list_records(&self, _: &ProjectId) -> Result<Vec<Record>, StoreError> {
            unreachable!("not used")
        }
        fn set_record_state(&self, _: &RecordId, state: State) -> Result<(), StoreError> {
            self.record.lock().unwrap().state = state;
            Ok(())
        }
        fn add_finding(&self, _: Finding) -> Result<FindingId, StoreError> {
            unreachable!("not used")
        }
        fn add_finding_checked(
            &self,
            _: Finding,
            _: fl_core::routing::ForeignRecord,
        ) -> Result<FindingId, StoreError> {
            unreachable!("not used")
        }
        fn get_finding(&self, _: &FindingId) -> Result<Option<Finding>, StoreError> {
            unreachable!("not used")
        }
        fn update_finding(&self, _: &Finding) -> Result<(), StoreError> {
            unreachable!("not used")
        }
        fn list_findings(&self, _: &ProjectId) -> Result<Vec<Finding>, StoreError> {
            unreachable!("not used")
        }
        fn withdrawals_by(&self, _: &str) -> Result<u64, StoreError> {
            unreachable!("not used")
        }
        fn add_alias(&self, _: &Iri, _: Iri) -> Result<(), StoreError> {
            unreachable!("not used")
        }
    }

    impl Ledger for Shared {
        fn append_gate_run(&self, _: GateRun) -> Result<(), StoreError> {
            unreachable!("not used")
        }
        fn append_attempt(&self, _: Attempt) -> Result<(), StoreError> {
            unreachable!("not used")
        }
        fn gate_runs(&self, _: &GateId) -> Result<Vec<GateRun>, StoreError> {
            unreachable!("not used")
        }
        fn attempts(&self, _: &ProjectId) -> Result<Vec<Attempt>, StoreError> {
            unreachable!("not used")
        }
        fn flush(&self, _: fl_core::decision::Decision) -> Result<Flushed, StoreError> {
            if let Some(hook) = self.on_flush {
                hook(self);
            }
            if let Some(barrier) = &self.at_flush {
                barrier.wait();
            }
            Ok(Flushed::NOTHING)
        }
    }

    /// The window the old code left open: the state was checked BEFORE the flush and written
    /// after it with no second look, so a mover that finished during the flush was overwritten.
    #[test]
    fn a_record_moved_during_the_flush_is_not_overwritten() {
        let store = Shared::new(
            None,
            Some(|s| s.record.lock().unwrap().state = State::Doing),
        );
        let began = store.snapshot();
        let err = match move_record(store.roles(), &began, State::Done) {
            Err(e) => e,
            Ok(_) => panic!("a move over a state that changed during the flush was written"),
        };
        assert!(matches!(err, ExecError::StaleMove(_)), "{err:?}");
        assert_eq!(store.snapshot().state, State::Doing);
    }

    /// Two movers start from one stale snapshot (`todo`) toward different targets. Both pass the
    /// pre-flush check and meet at the flush, then race for the write. Exactly one may win; the
    /// other must get `StaleMove`. Repeated, because without the lock the loser only sometimes
    /// reads the record before the winner has written.
    #[test]
    fn two_concurrent_moves_of_one_record_let_exactly_one_write() {
        for round in 0..100 {
            let store = Shared::new(Some(std::sync::Barrier::new(2)), None);
            let began = store.snapshot();
            let results: Vec<Result<MoveReport, ExecError>> = std::thread::scope(|scope| {
                let handles: Vec<_> = [State::Doing, State::Done]
                    .into_iter()
                    .map(|to| {
                        let (store, began) = (&store, &began);
                        scope.spawn(move || move_record(store.roles(), began, to))
                    })
                    .collect();
                handles.into_iter().map(|h| h.join().unwrap()).collect()
            });
            let won: Vec<State> = [State::Doing, State::Done]
                .into_iter()
                .zip(&results)
                .filter(|(_, r)| r.is_ok())
                .map(|(to, _)| to)
                .collect();
            let stale = results
                .iter()
                .filter(|r| matches!(r, Err(ExecError::StaleMove(_))))
                .count();
            assert_eq!(won.len(), 1, "round {round}: both movers wrote ({won:?})");
            assert_eq!(stale, 1, "round {round}: the loser did not get StaleMove");
            assert_eq!(store.snapshot().state, won[0], "round {round}");
        }
    }
}
