use crate::population::ExecError;
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

pub trait Runner {
    fn id(&self) -> &str;
    fn attempt(
        &self,
        spec: AttemptSpec,
    ) -> impl std::future::Future<Output = Result<AttemptOutcome, ExecError>> + Send;
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

        async fn attempt(&self, _spec: AttemptSpec) -> Result<AttemptOutcome, ExecError> {
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
}
