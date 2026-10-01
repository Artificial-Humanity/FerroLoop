use crate::decision;
use crate::evaluate::{TransitionReport, evaluate_transition};
use crate::population::{ExecError, refused_publish};
use fl_core::decision::Flushed;
use fl_core::model::{Record, State, Transition};
use fl_core::store::Roles;

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
        roles
            .tracker
            .set_record_state(&record.id, to)
            .map_err(store_err)?;
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
    use fl_core::iri::Iri;
    use fl_core::log::{Attempt, GateRun};
    use fl_core::model::{CommandSpec, GateKind, PopulationDelivery, Regret, Selector, Transition};
    use fl_core::store::{Catalog, Ledger, StoreError, Tracker};
    use fl_core::{GateId, MemStore, ProjectId};
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
}
