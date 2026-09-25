use crate::evaluate::{TransitionReport, evaluate_transition};
use crate::population::ExecError;
use fl_core::model::{Record, State, Transition};
use fl_core::store::Roles;

pub struct MoveReport {
    pub transitions: Vec<TransitionReport>,
    pub outcome: MoveOutcome,
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
/// ⚠⚠ Evidence before state (spec §3.5, Invariant). The gate runs are
/// appended to the ledger inside `evaluate_transition`; only after every one
/// of them is written does the tracker state change. A crash in between
/// leaves evidence and no move, which is safe to retry. The reverse order
/// would leave a move with no evidence.
pub fn move_record(roles: Roles<'_>, record: &Record, to: State) -> Result<MoveReport, ExecError> {
    let declared: Vec<Transition> = roles
        .catalog
        .list_transitions(&record.project)
        .map_err(store_err)?
        .into_iter()
        .filter(|t| t.from == record.state && t.to == to)
        .collect();

    if declared.is_empty() {
        roles
            .tracker
            .set_record_state(&record.id, to)
            .map_err(store_err)?;
        return Ok(MoveReport {
            transitions: vec![],
            outcome: MoveOutcome::Ungated,
        });
    }

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

    if worst != 0 {
        return Ok(MoveReport {
            transitions,
            outcome: MoveOutcome::Refused { code: worst },
        });
    }

    roles
        .tracker
        .set_record_state(&record.id, to)
        .map_err(store_err)?;
    Ok(MoveReport {
        transitions,
        outcome: MoveOutcome::Moved,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
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
}
