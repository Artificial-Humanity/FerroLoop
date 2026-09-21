use crate::ids::{GateId, ProjectId, RecordId};
use crate::verdict::Verdict;
use serde::{Deserialize, Serialize};

/// One execution of one gate. Append-only.
///
/// ⚠ `population` is not optional on a recorded run. A verdict without a
/// count cannot be written, which is why the field is not an `Option`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateRun {
    pub gate: GateId,
    pub record: Option<RecordId>,
    pub commit: String,
    pub verdict: Verdict,
    pub population: u64,
    pub output_excerpt: String,
    pub duration_ms: u64,
    pub cost_usd_micros: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptStatus {
    Completed,
    Timeout,
    Crashed,
    Refused,
}

impl AttemptStatus {
    /// The wire name, which is also what `attempt` and `stats` print. See
    /// [`crate::verdict::FailReason::as_wire`] for why this is not `Debug`.
    pub fn as_wire(self) -> &'static str {
        match self {
            AttemptStatus::Completed => "completed",
            AttemptStatus::Timeout => "timeout",
            AttemptStatus::Crashed => "crashed",
            AttemptStatus::Refused => "refused",
        }
    }
}

/// One runner invocation. Append-only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attempt {
    pub project: ProjectId,
    pub record: RecordId,
    pub adapter: String,
    pub status: AttemptStatus,
    pub duration_ms: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cost_usd_micros: u64,
    pub paths_touched: Vec<String>,
    pub output_excerpt: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_attempt_statuss_serde_form_is_the_same_string_as_its_wire_name() {
        for st in [
            AttemptStatus::Completed,
            AttemptStatus::Timeout,
            AttemptStatus::Crashed,
            AttemptStatus::Refused,
        ] {
            assert_eq!(
                serde_json::to_string(&st).expect("serialize"),
                format!("\"{}\"", st.as_wire())
            );
        }
    }
}
