//! Mode B's ledger (GitHub ledger spec §1.2, §2): every entry goes to the
//! local store at once, and at each decision's flush the entries tied to a
//! record this binding owns, recorded after its cut-over, go to GitHub with
//! the decision.
//!
//! The GitHub side is a [`RemoteLedger`]: `fl_github::GithubLedger`
//! implements it (plan B), and `conformance::MemRemote` is the in-memory
//! double every test here uses.

use crate::at::At;
use crate::decision::{Decision, Flushed, LeftLocal};
use crate::ids::{GateId, ProjectId, RecordId};
use crate::iri::Iri;
use crate::log::{Attempt, GateRun, PathsTouched, WITHHELD_ERROR_DETAIL};
use crate::store::{Ledger, StoreError};
use crate::verdict::Verdict;
use std::collections::BTreeMap;

/// Local entries waiting to be published to one repository.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pending {
    pub runs: Vec<GateRun>,
    pub attempts: Vec<Attempt>,
}

/// The local store's half of publishing (spec §2.1, §3.2 step 6), keyed by
/// the repository's `node_id`: its cut-over, and what it already has.
pub trait Outbox {
    /// Every entry with an id greater than `after`, tied to a record (every
    /// attempt is), and not marked published to `repo` — in id order. A
    /// run with no record is never listed (spec §2.1), nor is an entry with
    /// no id (§1.3).
    fn unpublished(&self, repo: &str, after: &Iri) -> Result<Pending, StoreError>;
    fn is_published(&self, repo: &str, id: &Iri) -> Result<bool, StoreError>;
    /// Idempotent: marking an id twice is not an error.
    fn mark_published(&self, repo: &str, ids: &[Iri]) -> Result<(), StoreError>;
    /// The id after which entries are publishable to `repo`, if its GitHub
    /// ledger was switched on.
    fn cutover(&self, repo: &str) -> Result<Option<Iri>, StoreError>;
    /// Recorded once, by `fl github ledger init` (plan B). ⚠ The same id
    /// again is a no-op; a different one is `CutoverChanged`.
    fn set_cutover(&self, repo: &str, id: &Iri) -> Result<(), StoreError>;
}

/// What a split ledger's local side must be.
pub trait LocalLedger: Ledger + Outbox {}
impl<T: Ledger + Outbox> LocalLedger for T {}

/// One flush: the decision and every entry it publishes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Batch {
    pub decision: Decision,
    pub runs: Vec<GateRun>,
    pub attempts: Vec<Attempt>,
}

/// The GitHub side of mode B.
pub trait RemoteLedger {
    /// The repository's `node_id`: the key of the local cut-over and marks.
    fn repo_node_id(&self) -> &str;
    /// Whether `record` belongs to this binding's repository.
    ///
    /// ⚠ A LOCAL answer, from the record's IRI and what the local store
    /// remembers — never a network call (spec §2.1). `GithubLedger` answers
    /// with `fl_github::owner::issue_of_repository` (Task 9).
    fn owns_record(&self, record: &RecordId) -> Result<bool, StoreError>;
    /// Append `batch` in one commit, and return only once it has landed:
    /// the commit, or `None` when nothing was left to add. An entry whose
    /// id is already there is not added twice.
    fn publish(&self, batch: &Batch) -> Result<Option<String>, StoreError>;
    fn gate_runs(&self, gate: &GateId) -> Result<Vec<GateRun>, StoreError>;
    fn attempts(&self, project: &ProjectId) -> Result<Vec<Attempt>, StoreError>;
}

/// How much of the ledger a report covers (spec §2.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Coverage {
    /// The local store and GitHub, merged.
    Complete,
    /// The local store only, and why.
    LocalOnly { reason: String },
}

/// Mode B's `Ledger`: `local` keeps every entry, `github` holds what the
/// decisions published.
pub struct SplitLedger<'a> {
    pub local: &'a dyn LocalLedger,
    pub github: &'a dyn RemoteLedger,
}

/// What the merge needs of an entry.
trait Entry: Clone + PartialEq {
    fn entry_id(&self) -> Option<&Iri>;
    fn entry_at(&self) -> Option<&At>;
    /// `self` with each field decision 2 withholds taken from `published`
    /// wherever `published` holds the withheld form. Equal to `published`
    /// exactly when the two copies agree.
    fn as_published(&self, published: &Self) -> Self;
}

impl Entry for GateRun {
    fn entry_id(&self) -> Option<&Iri> {
        self.id.as_ref()
    }
    fn entry_at(&self) -> Option<&At> {
        self.at.as_ref()
    }
    fn as_published(&self, published: &Self) -> Self {
        let mut mine = self.clone();
        if published.output_excerpt.is_none() {
            mine.output_excerpt = None;
        }
        // An error's detail is withheld with ONE text (decision 2); any
        // other text on a published error is an altered copy.
        if let (Verdict::Error { .. }, Verdict::Error { detail, .. }) =
            (&mine.verdict, &published.verdict)
            && detail == WITHHELD_ERROR_DETAIL
        {
            mine.verdict = published.verdict.clone();
        }
        mine
    }
}

impl Entry for Attempt {
    fn entry_id(&self) -> Option<&Iri> {
        self.id.as_ref()
    }
    fn entry_at(&self) -> Option<&At> {
        self.at.as_ref()
    }
    fn as_published(&self, published: &Self) -> Self {
        let mut mine = self.clone();
        if published.output_excerpt.is_none() {
            mine.output_excerpt = None;
        }
        if let PathsTouched::Counted(n) = published.paths_touched
            && mine.paths_touched.count() == n
        {
            mine.paths_touched = PathsTouched::Counted(n);
        }
        mine
    }
}

/// Local and published entries, de-duplicated by id and ordered by `at`,
/// then id (spec §2.5). An entry with no id is local, from before ids, and
/// comes first.
///
/// ⚠ The same id with different content is `Tampered`, except in the
/// fields decision 2 withholds, which the local copy supplies. A published
/// entry with no id is damage, never a legacy entry.
fn merge<T: Entry>(local: Vec<T>, remote: Vec<T>) -> Result<Vec<T>, StoreError> {
    let mut out: Vec<T> = Vec::with_capacity(local.len() + remote.len());
    let mut seen: BTreeMap<Iri, usize> = BTreeMap::new();
    for e in local {
        if let Some(id) = e.entry_id() {
            seen.insert(id.clone(), out.len());
        }
        out.push(e);
    }
    for e in remote {
        let Some(id) = e.entry_id().cloned() else {
            return Err(StoreError::Backend(
                "the shared ledger holds an entry with no id, and every published entry carries \
                 one"
                .into(),
            ));
        };
        match seen.get(&id) {
            Some(&i) => {
                if out[i].as_published(&e) != e {
                    return Err(StoreError::Tampered {
                        id,
                        detail: "the two copies differ in more than the fields a repository \
                                 that is not private withholds"
                            .into(),
                    });
                }
            }
            None => {
                seen.insert(id, out.len());
                out.push(e);
            }
        }
    }
    out.sort_by(|a, b| (a.entry_at(), a.entry_id()).cmp(&(b.entry_at(), b.entry_id())));
    Ok(out)
}

impl SplitLedger<'_> {
    /// `attempts` for `fl stats` (spec §2.5): when GitHub cannot be read,
    /// the local store's attempts and a coverage that says so — never an
    /// error, and never a short count that reads as the total.
    pub fn attempts_for_stats(
        &self,
        project: &ProjectId,
    ) -> Result<(Vec<Attempt>, Coverage), StoreError> {
        let local = self.local.attempts(project)?;
        match self.github.attempts(project) {
            Ok(remote) => Ok((merge(local, remote)?, Coverage::Complete)),
            Err(e) => Ok((
                local,
                Coverage::LocalOnly {
                    reason: format!("GitHub could not be read: {e}"),
                },
            )),
        }
    }
}

impl Ledger for SplitLedger<'_> {
    /// Local, at once (spec §2.1): the local store keeps every run.
    fn append_gate_run(&self, run: GateRun) -> Result<(), StoreError> {
        self.local.append_gate_run(run)
    }

    fn append_attempt(&self, attempt: Attempt) -> Result<(), StoreError> {
        self.local.append_attempt(attempt)
    }

    fn gate_runs(&self, gate: &GateId) -> Result<Vec<GateRun>, StoreError> {
        // Local first: a gate the local catalog never held is `NotOwned`
        // (spec §8.2), whatever GitHub holds.
        let local = self.local.gate_runs(gate)?;
        // ⚠ Unreachable is not empty (spec §2.5, Invariant).
        let remote = self.github.gate_runs(gate)?;
        merge(local, remote)
    }

    fn attempts(&self, project: &ProjectId) -> Result<Vec<Attempt>, StoreError> {
        let local = self.local.attempts(project)?;
        let remote = self.github.attempts(project)?;
        merge(local, remote)
    }

    /// ⚠⚠ Returns only after the commit has landed (spec §2.2).
    ///
    /// Before anything is sent: a decision about a record this repository
    /// does not hold is `NotOwned`; with no cut-over, nothing is published
    /// and `LeftLocal::NoCutover` says so; a pending entry of another
    /// repository is skipped and reported; a decision resting on an entry
    /// that is neither being published nor published is refused.
    fn flush(&self, decision: Decision) -> Result<Flushed, StoreError> {
        let repo = self.github.repo_node_id().to_string();
        if !self.github.owns_record(&decision.record)? {
            return Err(StoreError::NotOwned {
                id: decision.record.iri().clone(),
                searched: vec![format!(
                    "the repository this ledger publishes to (node {repo})"
                )],
            });
        }
        let Some(cutover) = self.local.cutover(&repo)? else {
            return Ok(Flushed {
                commit: None,
                left_local: vec![LeftLocal::NoCutover],
            });
        };
        let pending = self.local.unpublished(&repo, &cutover)?;
        let mut left_local = Vec::new();
        let mut runs = Vec::new();
        for run in pending.runs {
            // `Outbox` lists only runs tied to a record, each with an id.
            let (Some(record), Some(id)) = (run.record.clone(), run.id.clone()) else {
                continue;
            };
            if self.github.owns_record(&record)? {
                runs.push(run);
            } else {
                left_local.push(LeftLocal::OtherRepository { entry: id, record });
            }
        }
        let mut attempts = Vec::new();
        for a in pending.attempts {
            let Some(id) = a.id.clone() else {
                continue;
            };
            if self.github.owns_record(&a.record)? {
                attempts.push(a);
            } else {
                left_local.push(LeftLocal::OtherRepository {
                    entry: id,
                    record: a.record.clone(),
                });
            }
        }
        let ids: Vec<Iri> = runs
            .iter()
            .filter_map(|r| r.id.clone())
            .chain(attempts.iter().filter_map(|a| a.id.clone()))
            .collect();
        for cited in &decision.rests_on {
            if !ids.contains(cited) && !self.local.is_published(&repo, cited)? {
                return Err(StoreError::Backend(format!(
                    "decision {} rests on {cited}, which is neither being published nor \
                     published. A run tied to no record, one recorded before the GitHub ledger \
                     was switched on, or one tied to another repository's record stays local, \
                     so a decision cannot rest on it. Nothing was published.",
                    decision.id
                )));
            }
        }
        let commit = self.github.publish(&Batch {
            decision,
            runs,
            attempts,
        })?;
        // Only after the commit landed: a mark written first would hide an
        // entry GitHub never received.
        self.local.mark_published(&repo, &ids)?;
        Ok(Flushed { commit, left_local })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemStore;
    use crate::conformance::{
        MemRemote, RemoteControl, entry_iri, sample_attempt, sample_decision, sample_record_run,
    };
    use crate::ids::seq_iri;
    use crate::model::{CommandSpec, GateKind, PopulationDelivery, Selector};
    use crate::store::{Catalog, Tracker};

    /// A project with one gate and one record, with no cut-over recorded.
    fn bare_world() -> (MemStore, ProjectId, GateId, RecordId) {
        let s = MemStore::default();
        let p = s.add_project("/p").unwrap();
        let kind = GateKind::Command(CommandSpec {
            program: "true".into(),
            args: vec![],
            delivery: PopulationDelivery::Args,
            timeout_secs: 5,
            pass_codes: vec![0],
        });
        let sel = Selector::Glob {
            pattern: "**/*".into(),
        };
        let g = s.add_gate(&p, "g", kind, sel, 1, "c", "o").unwrap();
        let r = s.add_record(&p, "t").unwrap();
        (s, p, g, r)
    }

    /// As [`bare_world`], with `R_1`'s GitHub ledger switched on before
    /// every sample entry.
    fn world() -> (MemStore, ProjectId, GateId, RecordId) {
        let w = bare_world();
        w.0.set_cutover("R_1", &entry_iri(0)).unwrap();
        w
    }

    fn published_ids(remote: &MemRemote, g: &GateId) -> Vec<Option<Iri>> {
        remote
            .gate_runs(g)
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect()
    }

    // ⚠ Spec §2.5: the fields decision 2 withholds are supplied by the local
    // copy — and those are the only fields that may differ.
    #[test]
    fn a_copy_that_withholds_only_what_a_public_repository_withholds_merges_to_the_local_copy() {
        let (s, _p, g, r) = world();
        let remote = MemRemote::new("R_1");
        let l = SplitLedger {
            local: &s,
            github: &remote,
        };
        let mut run = sample_record_run(1, &g, Some(&r));
        run.verdict = Verdict::error("could not spawn the lint program");
        l.append_gate_run(run.clone()).unwrap();
        let mut published = run.clone();
        published.output_excerpt = None;
        published.verdict = Verdict::error(WITHHELD_ERROR_DETAIL);
        remote.insert_run(published);

        assert_eq!(
            l.gate_runs(&g).unwrap(),
            vec![run],
            "one entry, and the local copy supplies what was withheld"
        );
    }

    #[test]
    fn a_copy_that_differs_in_anything_else_is_an_error_naming_the_entry() {
        let edits: [fn(&mut GateRun); 5] = [
            |r| r.population = 2,
            |r| r.commit = "def".into(),
            |r| r.verdict = Verdict::from_predicate(false, 1),
            |r| r.verdict = Verdict::error(WITHHELD_ERROR_DETAIL),
            |r| r.output_excerpt = Some("something else".into()),
        ];
        for edit in edits {
            let (s, _p, g, r) = world();
            let remote = MemRemote::new("R_1");
            let l = SplitLedger {
                local: &s,
                github: &remote,
            };
            let run = sample_record_run(1, &g, Some(&r));
            l.append_gate_run(run.clone()).unwrap();
            let mut copy = run.clone();
            edit(&mut copy);
            remote.insert_run(copy);

            let err = l.gate_runs(&g).unwrap_err();
            let id = run.id.clone().unwrap();
            assert!(
                matches!(err, StoreError::Tampered { id: ref got, .. } if *got == id),
                "{err:?}"
            );
            assert!(err.to_string().contains(id.as_str()), "{err}");
        }
    }

    // Decision 2 withholds an error's detail with ONE text. Any other text
    // on a published error is an altered copy, not a withheld one.
    #[test]
    fn a_published_error_with_any_text_but_the_withheld_one_is_an_error() {
        let (s, _p, g, r) = world();
        let remote = MemRemote::new("R_1");
        let l = SplitLedger {
            local: &s,
            github: &remote,
        };
        let mut run = sample_record_run(1, &g, Some(&r));
        run.verdict = Verdict::error("could not spawn the lint program");
        l.append_gate_run(run.clone()).unwrap();
        let mut copy = run.clone();
        copy.verdict = Verdict::error("spawn");
        remote.insert_run(copy);
        let err = l.gate_runs(&g).unwrap_err();
        assert!(matches!(err, StoreError::Tampered { .. }), "{err:?}");
    }

    #[test]
    fn an_attempts_path_count_merges_only_when_it_counts_the_same_paths() {
        for (count, merges) in [(1, true), (2, false)] {
            let (s, p, _g, r) = world();
            let remote = MemRemote::new("R_1");
            let l = SplitLedger {
                local: &s,
                github: &remote,
            };
            let a = sample_attempt(1, &p, &r);
            l.append_attempt(a.clone()).unwrap();
            let mut copy = a.clone();
            copy.paths_touched = PathsTouched::Counted(count);
            copy.output_excerpt = None;
            remote.insert_attempt(copy);

            match (l.attempts(&p), merges) {
                (Ok(got), true) => assert_eq!(got, vec![a]),
                (Err(StoreError::Tampered { .. }), false) => {}
                (other, _) => panic!("a count of {count} answered {other:?}"),
            }
        }
    }

    // Spec §2.5: ordered by `at`, then `id`. An entry from before ids has
    // neither, and is older than every entry that has them.
    #[test]
    fn merged_entries_are_ordered_by_time_then_id_and_entries_without_ids_come_first() {
        let (s, _p, g, r) = world();
        let remote = MemRemote::new("R_1");
        let l = SplitLedger {
            local: &s,
            github: &remote,
        };
        let mut legacy = sample_record_run(0, &g, Some(&r));
        legacy.id = None;
        legacy.at = None;
        let late = sample_record_run(5, &g, Some(&r));
        l.append_gate_run(legacy).unwrap();
        l.append_gate_run(late.clone()).unwrap();
        let elsewhere = sample_record_run(3, &g, Some(&r));
        let mut same_time = sample_record_run(4, &g, Some(&r));
        same_time.at = late.at.clone();
        remote.insert_run(same_time.clone());
        remote.insert_run(elsewhere.clone());

        let ids: Vec<Option<Iri>> = l.gate_runs(&g).unwrap().into_iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![None, elsewhere.id, same_time.id, late.id]);
    }

    // ⚠ Spec §2.5 (Invariant): unreachable is not empty.
    #[test]
    fn reading_while_github_is_down_is_an_error_not_the_local_half() {
        let (s, p, g, r) = world();
        let remote = MemRemote::new("R_1");
        let l = SplitLedger {
            local: &s,
            github: &remote,
        };
        l.append_gate_run(sample_record_run(1, &g, Some(&r)))
            .unwrap();
        l.append_attempt(sample_attempt(2, &p, &r)).unwrap();
        remote.set_down(true);
        let err = l.gate_runs(&g).unwrap_err();
        assert!(matches!(err, StoreError::Unreachable { .. }), "{err:?}");
        let err = l.attempts(&p).unwrap_err();
        assert!(matches!(err, StoreError::Unreachable { .. }), "{err:?}");
    }

    #[test]
    fn stats_fall_back_to_the_local_store_and_say_why() {
        let (s, p, _g, r) = world();
        let remote = MemRemote::new("R_1");
        let l = SplitLedger {
            local: &s,
            github: &remote,
        };
        l.append_attempt(sample_attempt(1, &p, &r)).unwrap();
        remote.set_down(true);
        let (attempts, coverage) = l.attempts_for_stats(&p).unwrap();
        assert_eq!(attempts.len(), 1);
        assert!(
            matches!(coverage, Coverage::LocalOnly { ref reason } if reason.contains("could not be read")),
            "{coverage:?}"
        );
        // Local-only is for an unreadable GitHub, never for a project the
        // local catalog never held.
        let err = l.attempts_for_stats(&ProjectId(seq_iri(999))).unwrap_err();
        assert!(matches!(err, StoreError::NotOwned { .. }), "{err:?}");
        remote.set_down(false);
        assert_eq!(l.attempts_for_stats(&p).unwrap().1, Coverage::Complete);
    }

    // Spec §2.1: a flush publishes the pending entries tied to a record this
    // binding owns, and marks them.
    #[test]
    fn a_flush_publishes_the_entries_tied_to_records_this_repository_holds_and_marks_them() {
        let (s, _p, g, r) = world();
        let remote = MemRemote::new("R_1");
        let l = SplitLedger {
            local: &s,
            github: &remote,
        };
        let tied = sample_record_run(1, &g, Some(&r));
        let untied = sample_record_run(2, &g, None);
        l.append_gate_run(tied.clone()).unwrap();
        l.append_gate_run(untied.clone()).unwrap();
        let flushed = l
            .flush(sample_decision(1, &r, vec![tied.id.clone().unwrap()]))
            .unwrap();
        assert!(flushed.commit.is_some(), "{flushed:?}");
        assert!(flushed.left_local.is_empty(), "{flushed:?}");
        assert_eq!(published_ids(&remote, &g), vec![tied.id.clone()]);
        assert_eq!(remote.decisions().len(), 1);
        assert!(s.is_published("R_1", tied.id.as_ref().unwrap()).unwrap());
        assert!(!s.is_published("R_1", untied.id.as_ref().unwrap()).unwrap());
    }

    // ⚠ Spec §2.1: an entry whose record another binding owns is skipped
    // and reported — never an error that blocks the decision.
    #[test]
    fn a_pending_entry_of_another_repository_is_skipped_and_reported_not_an_error() {
        let (s, _p, g, r) = world();
        let remote = MemRemote::new("R_1");
        let l = SplitLedger {
            local: &s,
            github: &remote,
        };
        let theirs = remote.foreign_record();
        let tied = sample_record_run(1, &g, Some(&r));
        let elsewhere = sample_record_run(2, &g, Some(&theirs));
        l.append_gate_run(tied.clone()).unwrap();
        l.append_gate_run(elsewhere.clone()).unwrap();

        let flushed = l
            .flush(sample_decision(1, &r, vec![tied.id.clone().unwrap()]))
            .unwrap();

        assert_eq!(published_ids(&remote, &g), vec![tied.id.clone()]);
        assert_eq!(
            flushed.left_local,
            vec![LeftLocal::OtherRepository {
                entry: elsewhere.id.clone().unwrap(),
                record: theirs,
            }]
        );
        assert!(
            !s.is_published("R_1", elsewhere.id.as_ref().unwrap())
                .unwrap()
        );
    }

    // ⚠ Spec §2.1: entries recorded before the cut-over stay local; they
    // are not even scanned, so nothing reports them.
    #[test]
    fn entries_before_the_cut_over_stay_local_and_those_after_are_published() {
        let (s, _p, g, r) = bare_world();
        s.set_cutover("R_1", &entry_iri(5)).unwrap();
        let remote = MemRemote::new("R_1");
        let l = SplitLedger {
            local: &s,
            github: &remote,
        };
        let before = sample_record_run(3, &g, Some(&r));
        let after = sample_record_run(7, &g, Some(&r));
        l.append_gate_run(before.clone()).unwrap();
        l.append_gate_run(after.clone()).unwrap();

        let flushed = l
            .flush(sample_decision(1, &r, vec![after.id.clone().unwrap()]))
            .unwrap();

        assert_eq!(published_ids(&remote, &g), vec![after.id.clone()]);
        assert!(flushed.left_local.is_empty(), "{flushed:?}");
        assert!(!s.is_published("R_1", before.id.as_ref().unwrap()).unwrap());
    }

    // Spec §2.1 and ruling 5: no cut-over, no GitHub ledger — the flush
    // publishes nothing, not even the decision, and says so. Not a refusal.
    #[test]
    fn with_no_cut_over_a_flush_publishes_nothing_and_says_so() {
        let (s, p, _g, r) = bare_world();
        let remote = MemRemote::new("R_1");
        let l = SplitLedger {
            local: &s,
            github: &remote,
        };
        l.append_attempt(sample_attempt(1, &p, &r)).unwrap();

        let flushed = l.flush(sample_decision(1, &r, vec![entry_iri(1)])).unwrap();

        assert_eq!(
            flushed,
            Flushed {
                commit: None,
                left_local: vec![LeftLocal::NoCutover],
            }
        );
        assert!(remote.decisions().is_empty());
        assert!(remote.attempts(&p).unwrap().is_empty());
        assert!(!s.is_published("R_1", &entry_iri(1)).unwrap());
    }

    // Spec §1.3: an entry from before ids is never published.
    #[test]
    fn an_entry_without_an_id_is_never_published() {
        let (s, _p, g, r) = world();
        let remote = MemRemote::new("R_1");
        let l = SplitLedger {
            local: &s,
            github: &remote,
        };
        let mut legacy = sample_record_run(1, &g, Some(&r));
        legacy.id = None;
        l.append_gate_run(legacy).unwrap();
        l.flush(sample_decision(1, &r, vec![])).unwrap();
        assert!(remote.gate_runs(&g).unwrap().is_empty());
    }

    // ⚠ Spec §2.2 and decision 8: a flush that fails publishes nothing and
    // marks nothing, and the next flush that succeeds carries what was left.
    #[test]
    fn a_flush_that_fails_publishes_and_marks_nothing_and_the_next_one_carries_it() {
        let (s, p, g, r) = world();
        let remote = MemRemote::new("R_1");
        let l = SplitLedger {
            local: &s,
            github: &remote,
        };
        let run = sample_record_run(1, &g, Some(&r));
        let attempt = sample_attempt(2, &p, &r);
        l.append_gate_run(run.clone()).unwrap();
        l.append_attempt(attempt.clone()).unwrap();

        remote.fail_publish(true);
        let refused = l.flush(sample_decision(1, &r, vec![attempt.id.clone().unwrap()]));
        assert!(refused.is_err(), "{refused:?}");
        assert!(!s.is_published("R_1", run.id.as_ref().unwrap()).unwrap());
        assert!(!s.is_published("R_1", attempt.id.as_ref().unwrap()).unwrap());

        remote.fail_publish(false);
        l.flush(sample_decision(2, &r, vec![run.id.clone().unwrap()]))
            .unwrap();
        assert_eq!(
            remote.attempts(&p).unwrap().len(),
            1,
            "the attempt left behind is published by the next flush"
        );
        assert!(s.is_published("R_1", run.id.as_ref().unwrap()).unwrap());
        assert!(s.is_published("R_1", attempt.id.as_ref().unwrap()).unwrap());
    }

    #[test]
    fn a_decision_resting_on_an_entry_that_is_not_being_published_is_refused_before_anything_is_sent()
     {
        let (s, _p, g, r) = world();
        let remote = MemRemote::new("R_1");
        let l = SplitLedger {
            local: &s,
            github: &remote,
        };
        let untied = sample_record_run(1, &g, None);
        l.append_gate_run(untied.clone()).unwrap();
        let err = l
            .flush(sample_decision(1, &r, vec![untied.id.clone().unwrap()]))
            .unwrap_err();
        assert!(
            err.to_string()
                .contains(untied.id.as_ref().unwrap().as_str()),
            "{err}"
        );
        assert!(remote.decisions().is_empty(), "nothing was sent");
    }

    // Ruling 6: the decision's OWN record must be this repository's.
    #[test]
    fn a_decision_about_a_record_this_repository_does_not_hold_is_refused() {
        let (s, _p, _g, _r) = world();
        let remote = MemRemote::new("R_1");
        let l = SplitLedger {
            local: &s,
            github: &remote,
        };
        let err = l
            .flush(sample_decision(1, &remote.foreign_record(), vec![]))
            .unwrap_err();
        assert!(matches!(err, StoreError::NotOwned { .. }), "{err:?}");
        assert!(remote.decisions().is_empty());
    }
}
