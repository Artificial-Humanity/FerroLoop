use crate::at::At;
use crate::ids::{GateId, ProjectId, RecordId};
use crate::iri::Iri;
use crate::verdict::Verdict;
use serde::{Deserialize, Serialize};

/// One execution of one gate. Append-only.
///
/// ⚠ `population` is not optional on a recorded run. A verdict without a
/// count cannot be written, which is why the field is not an `Option`.
///
/// ⚠ `id` and `at` (GitHub ledger spec §1.3) are `None` only on a run
/// recorded before entries carried them. Such a run still reads, and is
/// never published: a shared ledger de-duplicates by id, and an entry with
/// none cannot be told from a second copy of itself. Every run recorded now
/// has both — `fl_exec::stamp` mints them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateRun {
    #[serde(default)]
    pub id: Option<Iri>,
    #[serde(default)]
    pub at: Option<At>,
    pub gate: GateId,
    pub record: Option<RecordId>,
    pub commit: String,
    pub verdict: Verdict,
    pub population: u64,
    /// `Some` in the local store, always. `None` only on a copy published to
    /// a repository that is not private, which withholds it (decision 2).
    pub output_excerpt: Option<String>,
    pub duration_ms: u64,
    pub cost_usd_micros: u64,
}

/// What an `error` verdict's detail reads on a copy published to a
/// repository that is not private (decision 2): the detail can name paths
/// and hosts, and a verdict carries no error class to publish instead, so
/// the copy says only that the gate errored. The merge (`split.rs`) accepts
/// exactly this text in place of the local detail, and nothing else.
pub const WITHHELD_ERROR_DETAIL: &str =
    "the gate errored; its detail is withheld because the repository is not private";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptStatus {
    Completed,
    Timeout,
    Crashed,
    Refused,
}

crate::wire::wire_names!(AttemptStatus as attempt_status_wire {
    Completed => "completed",
    Timeout => "timeout",
    Crashed => "crashed",
    Refused => "refused",
});

/// The paths an attempt changed: the list, in the local store; only how
/// many, on a copy published to a repository that is not private
/// (decision 2).
///
/// Untagged on purpose: a list is what every store has always held, so a
/// stored attempt reads unchanged, and a count can only be a number.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PathsTouched {
    Listed(Vec<String>),
    Counted(u64),
}

impl PathsTouched {
    pub fn count(&self) -> u64 {
        match self {
            PathsTouched::Listed(paths) => paths.len() as u64,
            PathsTouched::Counted(n) => *n,
        }
    }
}

/// One runner invocation. Append-only. `id`, `at` and the two withheld
/// fields follow [`GateRun`]'s rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attempt {
    #[serde(default)]
    pub id: Option<Iri>,
    #[serde(default)]
    pub at: Option<At>,
    pub project: ProjectId,
    pub record: RecordId,
    pub adapter: String,
    pub status: AttemptStatus,
    pub duration_ms: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cost_usd_micros: u64,
    pub paths_touched: PathsTouched,
    pub output_excerpt: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::seq_iri;

    #[test]
    fn a_run_stored_before_entry_ids_still_reads_and_has_neither() {
        // Exactly what every store held before this change: no `id`, no
        // `at`, and the excerpt a plain string.
        let json = format!(
            r#"{{"gate":"{}","record":null,"commit":"abc","verdict":{{"pass":{{"population":1}}}},"population":1,"output_excerpt":"ok","duration_ms":1,"cost_usd_micros":0}}"#,
            seq_iri(1)
        );
        let run: GateRun = serde_json::from_str(&json).unwrap();
        assert_eq!((run.id, run.at), (None, None));
        assert_eq!(run.output_excerpt.as_deref(), Some("ok"));
    }

    #[test]
    fn an_attempt_stored_before_entry_ids_still_reads_its_path_list() {
        let json = format!(
            r#"{{"project":"{}","record":"{}","adapter":"claude","status":"completed","duration_ms":1,"tokens_in":0,"tokens_out":0,"cost_usd_micros":0,"paths_touched":["a.rs"],"output_excerpt":""}}"#,
            seq_iri(1),
            seq_iri(2)
        );
        let a: Attempt = serde_json::from_str(&json).unwrap();
        assert_eq!((a.id, a.at), (None, None));
        assert_eq!(a.paths_touched, PathsTouched::Listed(vec!["a.rs".into()]));
        assert_eq!(a.paths_touched.count(), 1);
        assert_eq!(a.output_excerpt.as_deref(), Some(""));
    }

    // Decision 2: on a repository that is not private, a published copy
    // carries `null` for the excerpt and a count for the paths.
    #[test]
    fn a_published_copy_can_withhold_the_excerpt_and_count_the_paths() {
        let json = format!(
            r#"{{"id":"{}","at":"2026-09-30T00:00:00.000Z","project":"{}","record":"{}","adapter":"claude","status":"completed","duration_ms":1,"tokens_in":0,"tokens_out":0,"cost_usd_micros":0,"paths_touched":3,"output_excerpt":null}}"#,
            seq_iri(9),
            seq_iri(1),
            seq_iri(2)
        );
        let a: Attempt = serde_json::from_str(&json).unwrap();
        assert_eq!(a.paths_touched, PathsTouched::Counted(3));
        assert_eq!(a.paths_touched.count(), 3);
        assert_eq!(a.output_excerpt, None);
        assert_eq!(
            a.at.as_ref().map(At::as_str),
            Some("2026-09-30T00:00:00.000Z")
        );
    }

    /// `GateRun` and `Attempt` as the current fl declares them, frozen here:
    /// no `deny_unknown_fields`, a string excerpt, a path list. An older fl
    /// must still read what this one writes into the same store.
    #[derive(Deserialize)]
    #[allow(dead_code)]
    struct OlderRun {
        gate: GateId,
        record: Option<RecordId>,
        commit: String,
        verdict: Verdict,
        population: u64,
        output_excerpt: String,
        duration_ms: u64,
        cost_usd_micros: u64,
    }

    #[derive(Deserialize)]
    #[allow(dead_code)]
    struct OlderAttempt {
        project: ProjectId,
        record: RecordId,
        adapter: String,
        status: AttemptStatus,
        duration_ms: u64,
        tokens_in: u64,
        tokens_out: u64,
        cost_usd_micros: u64,
        paths_touched: Vec<String>,
        output_excerpt: String,
    }

    #[test]
    fn an_older_fl_still_reads_the_entries_this_one_stores() {
        let run = GateRun {
            id: Some(seq_iri(9)),
            at: Some(At::from_unix_millis(1)),
            gate: GateId(seq_iri(1)),
            record: Some(RecordId(seq_iri(2))),
            commit: "abc".into(),
            verdict: Verdict::from_predicate(true, 1),
            population: 1,
            output_excerpt: Some("ok".into()),
            duration_ms: 1,
            cost_usd_micros: 0,
        };
        let old: OlderRun = serde_json::from_str(&serde_json::to_string(&run).unwrap())
            .expect("an older fl reads a run this one stored");
        assert_eq!(old.output_excerpt, "ok");

        let attempt = Attempt {
            id: Some(seq_iri(10)),
            at: Some(At::from_unix_millis(2)),
            project: ProjectId(seq_iri(3)),
            record: RecordId(seq_iri(2)),
            adapter: "claude".into(),
            status: AttemptStatus::Completed,
            duration_ms: 1,
            tokens_in: 0,
            tokens_out: 0,
            cost_usd_micros: 0,
            paths_touched: PathsTouched::Listed(vec!["a.rs".into()]),
            output_excerpt: Some(String::new()),
        };
        let old: OlderAttempt = serde_json::from_str(&serde_json::to_string(&attempt).unwrap())
            .expect("an older fl reads an attempt this one stored");
        assert_eq!(old.paths_touched, vec!["a.rs".to_string()]);
    }
}
