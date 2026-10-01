//! What fl decided, as the shared ledger records it (GitHub ledger spec
//! §2.3). The audit trail keeps verdicts as well as runs, and a decision
//! comment is rendered from the ledger alone (§4.2).

use crate::at::At;
use crate::ids::{FindingId, GateId, RecordId};
use crate::iri::Iri;
use crate::log::AttemptStatus;
use crate::model::State;
use serde::{Deserialize, Serialize};

/// One transition a decision evaluated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransitionOutcome {
    pub transition: String,
    /// `TransitionReport::passed`: false for a transition with no gates,
    /// which verified nothing.
    pub passed: bool,
}

/// What was decided, composed by the caller from the reports it already
/// holds.
///
/// ⚠ It records the decision, not whether a state change that followed it
/// completed: the flush comes BEFORE the state change (spec §2.2), so it
/// cannot know. A comment adds that line when it is posted live (§4.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// `record move`. `transitions` is empty for an ungated move; `allowed`
    /// is false when any covering transition refused.
    Move {
        from: State,
        to: State,
        transitions: Vec<TransitionOutcome>,
        allowed: bool,
    },
    /// `check --record`.
    Check { transition: TransitionOutcome },
    /// `finding reproduce`: whether the gate was accepted as the
    /// reproduction.
    Reproduce { gate: GateId, accepted: bool },
    /// `finding verify`.
    Verify {
        reproduction: GateId,
        reproduction_passed: bool,
        regressions: Vec<GateId>,
        closed: bool,
    },
    /// `fl attempt`.
    Attempt { status: AttemptStatus },
}

crate::wire::wire_tags!(Outcome as outcome_wire {
    Outcome::Move { .. } => "move", Outcome::Move {
        from: State::Review,
        to: State::Done,
        transitions: vec![],
        allowed: true,
    };
    Outcome::Check { .. } => "check", Outcome::Check {
        transition: TransitionOutcome { transition: "launch".into(), passed: true },
    };
    Outcome::Reproduce { .. } => "reproduce", Outcome::Reproduce {
        gate: GateId(crate::ids::seq_iri(1)),
        accepted: true,
    };
    Outcome::Verify { .. } => "verify", Outcome::Verify {
        reproduction: GateId(crate::ids::seq_iri(1)),
        reproduction_passed: true,
        regressions: vec![],
        closed: true,
    };
    Outcome::Attempt { .. } => "attempt", Outcome::Attempt { status: AttemptStatus::Completed };
});

/// The kind of a decision (spec §2.3): `move`, `check`, `reproduce`,
/// `verify` or `attempt`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    Move,
    Check,
    Reproduce,
    Verify,
    Attempt,
}

crate::wire::wire_names!(DecisionKind as decision_kind_wire {
    Move => "move",
    Check => "check",
    Reproduce => "reproduce",
    Verify => "verify",
    Attempt => "attempt",
});

/// One decision, as its flush publishes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    pub id: Iri,
    pub at: At,
    /// The record the decision concerns. For `reproduce` and `verify`, the
    /// finding's record: the ledger files every decision under a record.
    pub record: RecordId,
    /// The finding, for `reproduce` and `verify`.
    pub finding: Option<FindingId>,
    pub outcome: Outcome,
    /// The ids of the runs, or of the attempt, it rests on, in the order
    /// they ran.
    pub rests_on: Vec<Iri>,
}

impl Decision {
    /// Derived from the outcome and never stored beside it, so the two
    /// cannot disagree.
    pub fn kind(&self) -> DecisionKind {
        match self.outcome {
            Outcome::Move { .. } => DecisionKind::Move,
            Outcome::Check { .. } => DecisionKind::Check,
            Outcome::Reproduce { .. } => DecisionKind::Reproduce,
            Outcome::Verify { .. } => DecisionKind::Verify,
            Outcome::Attempt { .. } => DecisionKind::Attempt,
        }
    }
}

/// What a flush did (spec §1.4), and what it left local and why (§2.1).
///
/// ⚠ A struct, not the `Nothing | Commit` pair §1.4 sketches: §2.1 says a
/// skipped entry and a missing cut-over are REPORTED, and the report has to
/// reach the command that prints it. `Flushed::NOTHING` is §1.4's
/// `Flushed::Nothing`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Flushed {
    /// The ledger commit that holds the decision, when one was made.
    pub commit: Option<String>,
    /// Entries that stayed local, and why. Reported, never a refusal.
    pub left_local: Vec<LeftLocal>,
}

impl Flushed {
    /// Nothing published and nothing to report: every local store's flush.
    pub const NOTHING: Flushed = Flushed {
        commit: None,
        left_local: Vec::new(),
    };
}

/// Why a flush left something local (GitHub ledger spec §2.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeftLocal {
    /// The binding has no cut-over: its GitHub ledger was never switched on
    /// (`fl github ledger init`), so the flush published nothing at all.
    NoCutover,
    /// A pending entry tied to a record another repository owns.
    OtherRepository { entry: Iri, record: RecordId },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::seq_iri;

    fn decision(outcome: Outcome) -> Decision {
        Decision {
            id: seq_iri(90),
            at: At::from_unix_millis(1),
            record: RecordId(seq_iri(1)),
            finding: None,
            outcome,
            rests_on: vec![seq_iri(91)],
        }
    }

    fn one_of_each() -> Vec<Outcome> {
        vec![
            Outcome::Move {
                from: State::Review,
                to: State::Done,
                transitions: vec![TransitionOutcome {
                    transition: "launch".into(),
                    passed: false,
                }],
                allowed: false,
            },
            Outcome::Check {
                transition: TransitionOutcome {
                    transition: "launch".into(),
                    passed: true,
                },
            },
            Outcome::Reproduce {
                gate: GateId(seq_iri(2)),
                accepted: true,
            },
            Outcome::Verify {
                reproduction: GateId(seq_iri(2)),
                reproduction_passed: true,
                regressions: vec![GateId(seq_iri(3))],
                closed: false,
            },
            Outcome::Attempt {
                status: AttemptStatus::Timeout,
            },
        ]
    }

    // The kind is derived, never stored beside the outcome, so the two
    // cannot disagree; and every kind has a sample here, so a kind added
    // without one fails rather than passing over nothing.
    #[test]
    fn a_decisions_kind_is_its_outcomes_tag_and_every_kind_is_sampled() {
        let mut seen = Vec::new();
        for outcome in one_of_each() {
            let d = decision(outcome);
            let json = serde_json::to_value(&d.outcome).unwrap();
            let tag = json
                .as_object()
                .and_then(|o| o.keys().next().cloned())
                .expect("an externally tagged enum");
            assert_eq!(tag, d.kind().as_wire());
            seen.push(d.kind());
        }
        for k in DecisionKind::ALL {
            assert!(seen.contains(k), "no sample of `{}`", k.as_wire());
        }
    }

    #[test]
    fn a_decision_round_trips_through_its_wire_form() {
        for outcome in one_of_each() {
            let d = decision(outcome);
            let back: Decision = serde_json::from_str(&serde_json::to_string(&d).unwrap()).unwrap();
            assert_eq!(back, d);
        }
    }
}
