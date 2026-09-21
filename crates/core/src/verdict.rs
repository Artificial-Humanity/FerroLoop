use serde::{Deserialize, Deserializer, Serialize, de};

/// Why a gate did not pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailReason {
    /// The gate examined a real population and the predicate was false.
    Predicate,
    /// The gate examined nothing. This is never a pass.
    EmptyPopulation,
    /// The gate's stamp is behind the code it covers, at a high-regret transition.
    Stale,
}

crate::wire::wire_names!(FailReason as fail_reason_wire {
    Predicate => "predicate",
    EmptyPopulation => "empty_population",
    Stale => "stale",
});

/// A population that was actually examined: strictly positive.
///
/// The inner value is private, so the only way to produce one is
/// [`Population::new`], which refuses zero. That closes the route
/// `#[non_exhaustive]` cannot: `#[non_exhaustive]` stops construction and
/// exhaustive matching from outside `fl-core`, but a struct-variant field
/// inherits the enum's visibility, so external code holding a `&mut
/// Verdict::Pass` could otherwise write straight through the `population`
/// field. With `population` typed as `Population` instead of `u64`, that
/// same external assignment can only substitute another `Population` — and
/// no `Population` holding zero can exist, because its field is private to
/// this module and every path to one (the constructor, and `Verdict`'s
/// `Deserialize`, which builds it through the same constructor) validates
/// first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct Population(u64);

impl Population {
    /// Build a `Population`, refusing zero: a gate that examined nothing
    /// has no population to report.
    pub fn new(value: u64) -> Option<Self> {
        if value == 0 {
            None
        } else {
            Some(Population(value))
        }
    }

    /// The examined count.
    pub fn get(self) -> u64 {
        self.0
    }
}

/// Private raw mirror for controlled deserialization.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
enum VerdictRaw {
    Pass { population: u64 },
    Fail { population: u64, reason: FailReason },
    Error { detail: String },
}

/// The outcome of one gate run.
///
/// ⚠ Every variant is `#[non_exhaustive]`, so no crate outside `fl-core` can
/// build one with struct syntax. The constructors below and deserialization
/// are the only ways in, and they are where the empty-population rule is
/// enforced. `Pass` additionally types its `population` field as
/// [`Population`], whose own private field seals the one route
/// `#[non_exhaustive]` leaves open: mutation through a `&mut Verdict::Pass`
/// obtained from a match. See [`Population`] for why that route is closed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    #[non_exhaustive]
    Pass { population: Population },
    #[non_exhaustive]
    Fail { population: u64, reason: FailReason },
    #[non_exhaustive]
    Error { detail: String },
}

impl<'de> Deserialize<'de> for Verdict {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = VerdictRaw::deserialize(deserializer)?;
        match raw {
            VerdictRaw::Pass { population } => {
                let population = Population::new(population).ok_or_else(|| {
                    de::Error::custom(
                        "cannot deserialize Pass over zero population: \
                        no gate examined nothing can pass",
                    )
                })?;
                Ok(Verdict::Pass { population })
            }
            VerdictRaw::Fail { population, reason } => Ok(Verdict::Fail { population, reason }),
            VerdictRaw::Error { detail } => Ok(Verdict::Error { detail }),
        }
    }
}

impl Verdict {
    /// The only route to a `Pass`.
    ///
    /// A zero population fails regardless of the predicate. A gate that
    /// examined nothing tells you nothing, and a no-op and a no-run must not
    /// produce the same answer.
    pub fn from_predicate(passed: bool, population: u64) -> Self {
        let Some(nonzero) = Population::new(population) else {
            return Verdict::Fail {
                population: 0,
                reason: FailReason::EmptyPopulation,
            };
        };
        if passed {
            Verdict::Pass {
                population: nonzero,
            }
        } else {
            Verdict::Fail {
                population,
                reason: FailReason::Predicate,
            }
        }
    }

    /// The gate itself broke. A broken instrument is not a clean bill of health.
    pub fn error(detail: impl Into<String>) -> Self {
        Verdict::Error {
            detail: detail.into(),
        }
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
            Verdict::Pass { population } => Some(population.get()),
            Verdict::Fail { population, .. } => Some(*population),
            Verdict::Error { .. } => None,
        }
    }

    /// Render this verdict for a human: a label and a detail line.
    ///
    /// The single rendering used by every command that prints a verdict, so
    /// `check`, `gate run` and `finding verify` cannot drift apart. The
    /// detail never contains a Rust identifier — see [`FailReason::as_wire`].
    pub fn describe(&self) -> (&'static str, String) {
        match self {
            Verdict::Pass { population, .. } => ("PASS", format!("{} examined", population.get())),
            Verdict::Fail {
                population, reason, ..
            } => (
                "FAIL",
                format!("{}, {population} examined", reason.as_wire()),
            ),
            Verdict::Error { detail, .. } => ("ERROR", detail.clone()),
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

crate::wire::wire_tags!(Verdict as verdict_wire {
    Verdict::Pass { .. } => "pass", Verdict::from_predicate(true, 1);
    Verdict::Fail { .. } => "fail", Verdict::from_predicate(false, 1);
    Verdict::Error { .. } => "error", Verdict::error("e");
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_true_predicate_over_an_empty_population_does_not_pass() {
        let v = Verdict::from_predicate(true, 0);
        assert_eq!(
            v,
            Verdict::Fail {
                population: 0,
                reason: FailReason::EmptyPopulation
            }
        );
        assert!(!v.is_pass());
    }

    #[test]
    fn a_true_predicate_over_a_real_population_passes() {
        assert_eq!(
            Verdict::from_predicate(true, 4),
            Verdict::Pass {
                population: Population::new(4).unwrap()
            }
        );
    }

    #[test]
    fn a_false_predicate_fails_on_the_predicate_not_the_population() {
        assert_eq!(
            Verdict::from_predicate(false, 4),
            Verdict::Fail {
                population: 4,
                reason: FailReason::Predicate
            }
        );
    }

    #[test]
    fn a_false_predicate_over_an_empty_population_still_blames_the_population() {
        assert_eq!(
            Verdict::from_predicate(false, 0),
            Verdict::Fail {
                population: 0,
                reason: FailReason::EmptyPopulation
            }
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

    #[test]
    fn deserialization_rejects_pass_over_empty_population() {
        let result = serde_json::from_str::<Verdict>(r#"{"pass":{"population":0}}"#);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("cannot deserialize Pass"));
    }

    #[test]
    fn deserialization_accepts_pass_over_real_population() {
        let v =
            serde_json::from_str::<Verdict>(r#"{"pass":{"population":4}}"#).expect("valid pass");
        assert_eq!(
            v,
            Verdict::Pass {
                population: Population::new(4).unwrap()
            }
        );
        assert!(v.is_pass());
    }

    #[test]
    fn deserialization_accepts_fail_over_any_population() {
        let v = serde_json::from_str::<Verdict>(
            r#"{"fail":{"population":0,"reason":"empty_population"}}"#,
        )
        .expect("valid fail");
        assert_eq!(
            v,
            Verdict::Fail {
                population: 0,
                reason: FailReason::EmptyPopulation
            }
        );
        assert!(!v.is_pass());
    }

    #[test]
    fn deserialization_accepts_error() {
        let v = serde_json::from_str::<Verdict>(r#"{"error":{"detail":"test error"}}"#)
            .expect("valid error");
        assert_eq!(
            v,
            Verdict::Error {
                detail: "test error".to_string()
            }
        );
        assert!(!v.is_pass());
    }

    #[test]
    fn roundtrip_pass_through_json() {
        let original = Verdict::from_predicate(true, 5);
        let serialized = serde_json::to_string(&original).expect("serialize");
        let deserialized = serde_json::from_str::<Verdict>(&serialized).expect("deserialize");
        assert_eq!(original, deserialized);
    }

    #[test]
    fn roundtrip_fail_through_json() {
        let original = Verdict::fail_for(FailReason::Predicate, 3);
        let serialized = serde_json::to_string(&original).expect("serialize");
        let deserialized = serde_json::from_str::<Verdict>(&serialized).expect("deserialize");
        assert_eq!(original, deserialized);
    }

    #[test]
    fn roundtrip_error_through_json() {
        let original = Verdict::error("test failure");
        let serialized = serde_json::to_string(&original).expect("serialize");
        let deserialized = serde_json::from_str::<Verdict>(&serialized).expect("deserialize");
        assert_eq!(original, deserialized);
    }

    #[test]
    fn population_refuses_zero() {
        assert_eq!(Population::new(0), None);
    }

    #[test]
    fn population_reports_back_a_positive_value() {
        let p = Population::new(7).expect("7 is a valid population");
        assert_eq!(p.get(), 7);
    }

    #[test]
    fn pass_serializes_to_the_exact_wire_form_and_back() {
        let original = Verdict::from_predicate(true, 3);
        let serialized = serde_json::to_string(&original).expect("serialize");
        assert_eq!(serialized, r#"{"pass":{"population":3}}"#);
        let deserialized = serde_json::from_str::<Verdict>(&serialized).expect("deserialize");
        assert_eq!(original, deserialized);
    }

    // `FailReason` is printed and stored, never typed in, so it carries no
    // `from_wire`. Serde IS its parser — that is the direction a store
    // written by an older build comes back through, so that is the
    // direction the PascalCase refusal has to be asserted in.
    #[test]
    fn every_fail_reason_reads_back_under_its_wire_name_and_no_other() {
        for (reason, wire) in [
            (FailReason::Predicate, "predicate"),
            (FailReason::EmptyPopulation, "empty_population"),
            (FailReason::Stale, "stale"),
        ] {
            assert_eq!(reason.as_wire(), wire);
            assert_eq!(
                serde_json::to_string(&reason).expect("serialize"),
                format!("\"{wire}\"")
            );
            assert_eq!(
                serde_json::from_str::<FailReason>(&format!("\"{wire}\"")).expect("deserialize"),
                reason
            );
        }
        assert!(serde_json::from_str::<FailReason>("\"Predicate\"").is_err());
    }

    #[test]
    fn every_verdict_variant_serializes_under_its_snake_case_tag() {
        assert_eq!(
            serde_json::to_string(&Verdict::from_predicate(true, 2)).expect("serialize"),
            r#"{"pass":{"population":2}}"#
        );
        assert_eq!(
            serde_json::to_string(&Verdict::fail_for(FailReason::Stale, 2)).expect("serialize"),
            r#"{"fail":{"population":2,"reason":"stale"}}"#
        );
        assert_eq!(
            serde_json::to_string(&Verdict::error("boom")).expect("serialize"),
            r#"{"error":{"detail":"boom"}}"#
        );
    }

    #[test]
    fn the_old_pascal_case_wire_form_is_refused_rather_than_silently_accepted() {
        // The casing moved. A store or a script still speaking the old form
        // must be told so, not quietly reinterpreted.
        assert!(serde_json::from_str::<Verdict>(r#"{"Pass":{"population":3}}"#).is_err());
        assert!(
            serde_json::from_str::<Verdict>(r#"{"fail":{"population":1,"reason":"Stale"}}"#)
                .is_err()
        );
    }

    #[test]
    fn describe_renders_a_verdict_for_a_human_without_leaking_rust_identifiers() {
        assert_eq!(
            Verdict::from_predicate(true, 4).describe(),
            ("PASS", "4 examined".to_string())
        );
        assert_eq!(
            Verdict::from_predicate(false, 4).describe(),
            ("FAIL", "predicate, 4 examined".to_string())
        );
        assert_eq!(
            Verdict::from_predicate(true, 0).describe(),
            ("FAIL", "empty_population, 0 examined".to_string())
        );
        assert_eq!(
            Verdict::error("no such command").describe(),
            ("ERROR", "no such command".to_string())
        );
    }
}
