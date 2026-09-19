use serde::{Deserialize, Serialize};

/// Why a gate did not pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FailReason {
    /// The gate examined a real population and the predicate was false.
    Predicate,
    /// The gate examined nothing. This is never a pass.
    EmptyPopulation,
    /// The gate's stamp is behind the code it covers, at a high-regret transition.
    Stale,
}

/// The outcome of one gate run.
///
/// ⚠ Every variant is `#[non_exhaustive]`, so no crate outside `fl-core` can
/// build one with struct syntax. The constructors below are the only way in,
/// and they are where the empty-population rule is enforced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Verdict {
    #[non_exhaustive]
    Pass { population: u64 },
    #[non_exhaustive]
    Fail { population: u64, reason: FailReason },
    #[non_exhaustive]
    Error { detail: String },
}

impl Verdict {
    /// The only route to a `Pass`.
    ///
    /// A zero population fails regardless of the predicate. A gate that
    /// examined nothing tells you nothing, and a no-op and a no-run must not
    /// produce the same answer.
    pub fn from_predicate(passed: bool, population: u64) -> Self {
        if population == 0 {
            return Verdict::Fail { population: 0, reason: FailReason::EmptyPopulation };
        }
        if passed {
            Verdict::Pass { population }
        } else {
            Verdict::Fail { population, reason: FailReason::Predicate }
        }
    }

    /// The gate itself broke. A broken instrument is not a clean bill of health.
    pub fn error(detail: impl Into<String>) -> Self {
        Verdict::Error { detail: detail.into() }
    }

    /// Force a failure for a reason the predicate cannot express, such as staleness.
    pub fn fail_for(reason: FailReason, population: u64) -> Self {
        Verdict::Fail { population, reason }
    }

    pub fn is_pass(&self) -> bool {
        matches!(self, Verdict::Pass { .. })
    }

    pub fn population(&self) -> Option<u64> {
        match self {
            Verdict::Pass { population } | Verdict::Fail { population, .. } => Some(*population),
            Verdict::Error { .. } => None,
        }
    }

    /// The CLI contract: 0 pass, 1 fail, 2 broken instrument or misuse.
    pub fn exit_code(&self) -> i32 {
        match self {
            Verdict::Pass { .. } => 0,
            Verdict::Fail { .. } => 1,
            Verdict::Error { .. } => 2,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_true_predicate_over_an_empty_population_does_not_pass() {
        let v = Verdict::from_predicate(true, 0);
        assert_eq!(v, Verdict::Fail { population: 0, reason: FailReason::EmptyPopulation });
        assert!(!v.is_pass());
    }

    #[test]
    fn a_true_predicate_over_a_real_population_passes() {
        assert_eq!(Verdict::from_predicate(true, 4), Verdict::Pass { population: 4 });
    }

    #[test]
    fn a_false_predicate_fails_on_the_predicate_not_the_population() {
        assert_eq!(
            Verdict::from_predicate(false, 4),
            Verdict::Fail { population: 4, reason: FailReason::Predicate }
        );
    }

    #[test]
    fn a_false_predicate_over_an_empty_population_still_blames_the_population() {
        assert_eq!(
            Verdict::from_predicate(false, 0),
            Verdict::Fail { population: 0, reason: FailReason::EmptyPopulation }
        );
    }

    #[test]
    fn an_error_is_not_a_pass_and_exits_two() {
        let v = Verdict::error("command `nope` not found");
        assert!(!v.is_pass());
        assert_eq!(v.exit_code(), 2);
        assert_eq!(v.population(), None);
    }

    #[test]
    fn exit_codes_follow_the_cli_contract() {
        assert_eq!(Verdict::from_predicate(true, 1).exit_code(), 0);
        assert_eq!(Verdict::from_predicate(false, 1).exit_code(), 1);
        assert_eq!(Verdict::from_predicate(true, 0).exit_code(), 1);
    }
}
