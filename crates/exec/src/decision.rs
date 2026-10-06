//! The `Decision` each command flushes (GitHub ledger spec §2.3), composed
//! from the reports the command already holds. The id and the time are
//! minted here, as every entry's are.

use crate::evaluate::{GateReport, TransitionReport};
use crate::stamp;
use fl_core::decision::{Decision, Outcome, TransitionOutcome};
use fl_core::finding::Finding;
use fl_core::ids::{FindingId, GateId, RecordId};
use fl_core::iri::Iri;
use fl_core::log::Attempt;
use fl_core::model::{Record, State};

fn outcome_of(t: &TransitionReport) -> TransitionOutcome {
    TransitionOutcome {
        transition: t.transition.clone(),
        passed: t.passed(),
    }
}

fn stamped(
    record: RecordId,
    finding: Option<FindingId>,
    outcome: Outcome,
    rests_on: Vec<Iri>,
) -> Decision {
    Decision {
        id: stamp::entry_id(),
        at: stamp::now(),
        record,
        finding,
        outcome,
        rests_on,
    }
}

/// `record move`: every transition that covered it, and whether the move
/// was allowed. Rests on every run, in the order they ran.
pub fn for_move(
    record: &Record,
    to: State,
    transitions: &[TransitionReport],
    allowed: bool,
) -> Decision {
    stamped(
        record.id.clone(),
        None,
        Outcome::Move {
            from: record.state,
            to,
            transitions: transitions.iter().map(outcome_of).collect(),
            allowed,
        },
        transitions
            .iter()
            .flat_map(|t| t.gates.iter().map(|g| g.run.clone()))
            .collect(),
    )
}

/// `check --record`.
pub fn for_check(record: &RecordId, report: &TransitionReport) -> Decision {
    stamped(
        record.clone(),
        None,
        Outcome::Check {
            transition: outcome_of(report),
        },
        report.gates.iter().map(|g| g.run.clone()).collect(),
    )
}

/// `finding reproduce`: whether the gate was accepted as the reproduction.
pub fn for_reproduce(
    finding: &Finding,
    gate: &GateId,
    report: &GateReport,
    accepted: bool,
) -> Decision {
    stamped(
        finding.record.clone(),
        Some(finding.id.clone()),
        Outcome::Reproduce {
            gate: gate.clone(),
            accepted,
        },
        vec![report.run.clone()],
    )
}

/// `finding verify`: the reproduction, every neighbour that did not pass,
/// and whether the finding closed. Rests on every gate it ran.
pub fn for_verify(
    finding: &Finding,
    reproduction: &GateReport,
    neighbours: &[GateReport],
    closed: bool,
) -> Decision {
    stamped(
        finding.record.clone(),
        Some(finding.id.clone()),
        Outcome::Verify {
            reproduction: reproduction.gate.clone(),
            reproduction_passed: reproduction.verdict.is_pass(),
            regressions: neighbours
                .iter()
                .filter(|n| !n.verdict.is_pass())
                .map(|n| n.gate.clone())
                .collect(),
            closed,
        },
        std::iter::once(reproduction)
            .chain(neighbours)
            .map(|g| g.run.clone())
            .collect(),
    )
}

/// `fl attempt`: its status, resting on the attempt recorded as `id`.
pub fn for_attempt(id: &Iri, attempt: &Attempt) -> Decision {
    stamped(
        attempt.record.clone(),
        None,
        Outcome::Attempt {
            status: attempt.status,
        },
        vec![id.clone()],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::ids::{ProjectId, seq_iri};
    use fl_core::log::{AttemptStatus, PathsTouched};
    use fl_core::model::Regret;
    use fl_core::stale::Staleness;
    use fl_core::verdict::Verdict;

    fn gate_report(n: u64, pass: bool) -> GateReport {
        GateReport {
            gate: GateId(seq_iri(n)),
            name: format!("g{n}"),
            verdict: Verdict::from_predicate(pass, 1),
            staleness: Staleness::Fresh,
            output_excerpt: String::new(),
            duration_ms: 1,
            run: seq_iri(100 + n),
        }
    }

    fn finding() -> Finding {
        let mut f = Finding::raise(
            ProjectId(seq_iri(2)),
            RecordId(seq_iri(1)),
            "reviewer",
            "claim",
        );
        f.id = FindingId(seq_iri(5));
        f
    }

    #[test]
    fn a_moves_decision_names_each_transition_and_rests_on_every_run_in_order() {
        let record = Record {
            id: RecordId(seq_iri(1)),
            project: ProjectId(seq_iri(2)),
            title: "t".into(),
            state: State::Review,
            also_known_as: vec![],
            area: None,
        };
        let transitions = vec![
            TransitionReport {
                transition: "launch".into(),
                regret: Regret::Low,
                gates: vec![gate_report(1, true), gate_report(2, true)],
            },
            TransitionReport {
                transition: "ship".into(),
                regret: Regret::High,
                gates: vec![gate_report(3, false)],
            },
        ];
        let d = for_move(&record, State::Done, &transitions, false);
        assert_eq!(d.record, record.id);
        assert_eq!(d.finding, None);
        assert_eq!(
            d.outcome,
            Outcome::Move {
                from: State::Review,
                to: State::Done,
                transitions: vec![
                    TransitionOutcome {
                        transition: "launch".into(),
                        passed: true
                    },
                    TransitionOutcome {
                        transition: "ship".into(),
                        passed: false
                    },
                ],
                allowed: false,
            }
        );
        assert_eq!(d.rests_on, vec![seq_iri(101), seq_iri(102), seq_iri(103)]);
        assert!(d.id.as_str().starts_with("urn:uuid:"));
    }

    // ⚠ The empty-population rule one level up: a transition with no gates
    // verified nothing, and the ledger must not record it as passed.
    #[test]
    fn a_transition_with_no_gates_is_recorded_as_not_passed() {
        let empty = TransitionReport {
            transition: "launch".into(),
            regret: Regret::Low,
            gates: vec![],
        };
        let d = for_check(&RecordId(seq_iri(1)), &empty);
        assert_eq!(
            d.outcome,
            Outcome::Check {
                transition: TransitionOutcome {
                    transition: "launch".into(),
                    passed: false
                }
            }
        );
        assert!(d.rests_on.is_empty());
    }

    #[test]
    fn a_verifys_decision_names_its_regressions_and_rests_on_every_gate_it_ran() {
        let f = finding();
        let d = for_verify(
            &f,
            &gate_report(1, true),
            &[gate_report(2, true), gate_report(3, false)],
            false,
        );
        assert_eq!(
            (d.record.clone(), d.finding.clone()),
            (f.record, Some(f.id))
        );
        assert_eq!(
            d.outcome,
            Outcome::Verify {
                reproduction: GateId(seq_iri(1)),
                reproduction_passed: true,
                regressions: vec![GateId(seq_iri(3))],
                closed: false,
            }
        );
        assert_eq!(d.rests_on, vec![seq_iri(101), seq_iri(102), seq_iri(103)]);
    }

    #[test]
    fn a_reproduction_and_an_attempt_each_rest_on_their_one_entry() {
        let f = finding();
        let d = for_reproduce(&f, &GateId(seq_iri(1)), &gate_report(1, false), true);
        assert_eq!(d.finding, Some(f.id.clone()));
        assert_eq!(d.record, f.record);
        assert_eq!(
            d.outcome,
            Outcome::Reproduce {
                gate: GateId(seq_iri(1)),
                accepted: true
            }
        );
        assert_eq!(d.rests_on, vec![seq_iri(101)]);

        let attempt = Attempt {
            id: Some(seq_iri(50)),
            at: None,
            project: ProjectId(seq_iri(2)),
            record: RecordId(seq_iri(1)),
            adapter: "claude".into(),
            status: AttemptStatus::Crashed,
            duration_ms: 1,
            tokens_in: 0,
            tokens_out: 0,
            cost_usd_micros: 0,
            paths_touched: PathsTouched::Listed(vec![]),
            output_excerpt: Some(String::new()),
        };
        let d = for_attempt(&seq_iri(50), &attempt);
        assert_eq!(d.record, attempt.record);
        assert_eq!(
            d.outcome,
            Outcome::Attempt {
                status: AttemptStatus::Crashed
            }
        );
        assert_eq!(d.rests_on, vec![seq_iri(50)]);
    }

    #[test]
    fn two_decisions_never_share_an_id() {
        let empty = TransitionReport {
            transition: "launch".into(),
            regret: Regret::Low,
            gates: vec![],
        };
        let r = RecordId(seq_iri(1));
        assert_ne!(for_check(&r, &empty).id, for_check(&r, &empty).id);
    }
}
