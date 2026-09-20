use fl_core::ids::RecordId;
use fl_core::log::AttemptStatus;
use std::path::PathBuf;

/// What a runner is asked to do.
///
/// ⚠ Nothing here says "subscription session". The trait describes producing
/// an outcome for a proposed piece of work. How the outcome was obtained —
/// a subscription CLI, an API key, a local model — belongs to the adapter.
pub struct AttemptSpec {
    pub project_root: PathBuf,
    pub record: RecordId,
    pub instruction: String,
    pub timeout_secs: u64,
    /// The hard ceiling for this attempt. An adapter that cannot observe cost
    /// must still refuse rather than run unbounded.
    pub budget_usd_micros: u64,
}

/// What came back.
///
/// ⚠ `Crashed` and `Timeout` are outcomes that cost, not errors that are
/// retried for free. The ladder that prices them is a later milestone, but the
/// accounting has to be right from the first attempt or it will have nothing
/// to price.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptOutcome {
    pub status: AttemptStatus,
    pub output_excerpt: String,
    pub duration_ms: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cost_usd_micros: u64,
    pub paths_touched: Vec<String>,
}

impl AttemptOutcome {
    pub fn refused(reason: impl Into<String>) -> Self {
        Self {
            status: AttemptStatus::Refused,
            output_excerpt: reason.into(),
            duration_ms: 0,
            tokens_in: 0,
            tokens_out: 0,
            cost_usd_micros: 0,
            paths_touched: vec![],
        }
    }
}

/// Pre-flight failures only: errors that occur before an attempt begins.
///
/// Anything that happens during or after the attempt starts — including crashes
/// and timeouts — must be reported as `Ok(AttemptOutcome)` with a status and cost.
///
/// ⚠ This type blocks **accidental** propagation of runtime failures via the `?`
/// operator on `ExecError` and `io::Error`. It does **not** prevent deliberate
/// misuse: an adapter author can still report a runtime failure by formatting it
/// into the `UnknownAdapter` string. The type enforces that you *must write it
/// down deliberately*, not that you cannot do it. The real guard is the test suite
/// and code review.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AttemptError {
    #[error("adapter `{0}` is not known")]
    UnknownAdapter(String),
}

pub trait Runner {
    fn id(&self) -> &str;
    /// Attempt a piece of work.
    ///
    /// `Err` means the attempt never started (e.g., adapter not found). Anything
    /// that happens after the attempt starts — including crashes and timeouts —
    /// is reported as `Ok(AttemptOutcome)` with a status and cost.
    ///
    /// The `AttemptError` type blocks accidental `?`-propagation of `ExecError`
    /// and `io::Error` into this channel. It does not prevent deliberate misuse.
    ///
    /// ⚠ This trait is not `dyn`-compatible due to the `impl Trait` return type;
    /// use `impl Runner` or generic `R: Runner` instead of `Box<dyn Runner>`.
    fn attempt(
        &self,
        spec: AttemptSpec,
    ) -> impl std::future::Future<Output = Result<AttemptOutcome, AttemptError>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::ids::RecordId;

    struct Stub;

    impl Runner for Stub {
        fn id(&self) -> &str {
            "stub"
        }

        async fn attempt(&self, _spec: AttemptSpec) -> Result<AttemptOutcome, AttemptError> {
            Ok(AttemptOutcome {
                status: AttemptStatus::Crashed,
                output_excerpt: "boom".into(),
                duration_ms: 12,
                tokens_in: 100,
                tokens_out: 5,
                cost_usd_micros: 3_200,
                paths_touched: vec![],
            })
        }
    }

    #[tokio::test]
    async fn a_crash_still_reports_what_it_cost() {
        let out = Stub
            .attempt(AttemptSpec {
                project_root: std::path::PathBuf::from("/tmp"),
                record: RecordId(1),
                instruction: "do the thing".into(),
                timeout_secs: 5,
                budget_usd_micros: 10_000,
            })
            .await
            .unwrap();
        assert_eq!(out.status, AttemptStatus::Crashed);
        assert_eq!(out.cost_usd_micros, 3_200);
    }

    #[test]
    fn an_outcome_that_costs_nothing_is_still_a_recorded_outcome() {
        let out = AttemptOutcome::refused("no adapter configured");
        assert_eq!(out.status, AttemptStatus::Refused);
        assert_eq!(out.cost_usd_micros, 0);
    }

    #[test]
    fn attempt_error_variant_exhaustiveness_is_enforced() {
        // Non-wildcard match means adding a variant to AttemptError will fail
        // to compile with E0004 (non-exhaustive patterns). This catches silent
        // regressions where a runtime-failure variant (Timeout, Spawn, etc.)
        // gets added to the enum.
        let err = AttemptError::UnknownAdapter("foo".into());
        match err {
            AttemptError::UnknownAdapter(id) => {
                assert_eq!(id, "foo");
            }
        }
    }
}
