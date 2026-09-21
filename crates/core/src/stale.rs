use crate::model::Regret;
use crate::verdict::{FailReason, Verdict};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Staleness {
    Fresh,
    StaleWarn,
    StaleFail,
}

impl Staleness {
    /// The marker appended to a printed line. One wording, in one place —
    /// `check` and `finding verify` used to carry two different sentences for
    /// `StaleWarn`, which is the same defect as two spellings of a wire name.
    pub fn note(self) -> &'static str {
        match self {
            Staleness::Fresh => "",
            Staleness::StaleWarn => "  (stale: the gate's population moved since it was stamped)",
            Staleness::StaleFail => "  (stale)",
        }
    }
}

/// A gate is stale when the things it covers have moved and the gate has not.
///
/// ⚠ Both inputs are computed over the gate's OWN population, never over the
/// whole repository. A gate covering `src/training/**` does not go stale
/// because the documentation changed.
pub fn is_stale(population_changed: bool, definition_changed: bool) -> bool {
    population_changed && !definition_changed
}

/// Decision 20: a stale gate fails where regret is high and warns elsewhere.
///
/// The verdict is only ever made worse. A stale gate cannot turn a failure
/// into a pass, and it cannot paper over a broken instrument.
pub fn apply_staleness(verdict: Verdict, stale: bool, regret: Regret) -> (Verdict, Staleness) {
    if !stale {
        return (verdict, Staleness::Fresh);
    }
    match regret {
        Regret::Low => (verdict, Staleness::StaleWarn),
        Regret::High => match verdict {
            Verdict::Pass { population } => (
                Verdict::fail_for(FailReason::Stale, population.get()),
                Staleness::StaleFail,
            ),
            other => (other, Staleness::StaleFail),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verdict::FailReason;

    #[test]
    fn a_gate_is_stale_when_its_population_moved_and_it_did_not() {
        assert!(is_stale(true, false));
    }

    #[test]
    fn a_gate_updated_alongside_its_population_is_fresh() {
        assert!(!is_stale(true, true));
    }

    #[test]
    fn a_gate_whose_population_never_moved_is_fresh() {
        assert!(!is_stale(false, false));
        assert!(!is_stale(false, true));
    }

    #[test]
    fn a_stale_gate_fails_at_a_high_regret_transition_whatever_it_returned() {
        let (v, s) = apply_staleness(Verdict::from_predicate(true, 7), true, Regret::High);
        assert_eq!(
            v,
            Verdict::Fail {
                population: 7,
                reason: FailReason::Stale
            }
        );
        assert_eq!(s, Staleness::StaleFail);
    }

    #[test]
    fn a_stale_gate_only_warns_at_a_low_regret_transition() {
        let (v, s) = apply_staleness(Verdict::from_predicate(true, 7), true, Regret::Low);
        assert_eq!(v, Verdict::from_predicate(true, 7));
        assert_eq!(s, Staleness::StaleWarn);
    }

    #[test]
    fn staleness_never_rescues_a_real_failure() {
        let failing = Verdict::from_predicate(false, 3);
        let (v, _) = apply_staleness(failing.clone(), true, Regret::High);
        assert_eq!(v, failing);
    }

    #[test]
    fn staleness_never_overwrites_an_error() {
        let broken = Verdict::error("no such command");
        let (v, s) = apply_staleness(broken.clone(), true, Regret::High);
        assert_eq!(v, broken);
        assert_eq!(s, Staleness::StaleFail);
    }

    // ⚠ `note` is a three-arm match, and its `StaleWarn` arm is the one no
    // integration test reaches. It used to exist twice with two different
    // wordings — `check` said the population moved, `finding verify` said the
    // code beneath it moved — which is the same defect as two spellings of a
    // wire name. One wording now, pinned here.
    #[test]
    fn a_fresh_gate_gets_no_note() {
        assert_eq!(Staleness::Fresh.note(), "");
    }

    #[test]
    fn a_warning_names_what_moved_and_why_that_matters() {
        assert_eq!(
            Staleness::StaleWarn.note(),
            "  (stale: the gate's population moved since it was stamped)"
        );
    }

    #[test]
    fn a_failing_staleness_gets_the_short_form() {
        assert_eq!(Staleness::StaleFail.note(), "  (stale)");
    }
}
