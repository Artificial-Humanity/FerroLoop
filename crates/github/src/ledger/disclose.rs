//! Decision 2 (GitHub ledger spec §5): on a repository that is not
//! private, nothing machine-specific is published.

use fl_core::log::{Attempt, GateRun, PathsTouched, WITHHELD_ERROR_DETAIL};
use fl_core::verdict::Verdict;

/// Who can read the repository, as decision 2 needs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    Private,
    NotPrivate,
}

impl Visibility {
    /// GitHub's `visibility`. ⚠ Only `private` is private: `internal`,
    /// `public`, and anything this fl does not know are not.
    pub fn from_github(v: &str) -> Visibility {
        if v == "private" {
            Visibility::Private
        } else {
            Visibility::NotPrivate
        }
    }
}

/// The copy of `run` a repository of visibility `v` may hold: not private,
/// no excerpt, and an error's detail replaced by the one withheld text.
pub fn run(run: &GateRun, v: Visibility) -> GateRun {
    let mut out = run.clone();
    if v == Visibility::NotPrivate {
        out.output_excerpt = None;
        if let Verdict::Error { .. } = out.verdict {
            out.verdict = Verdict::error(WITHHELD_ERROR_DETAIL);
        }
    }
    out
}

/// The copy of `a` a repository of visibility `v` may hold: not private,
/// no excerpt, and only how many paths it touched.
pub fn attempt(a: &Attempt, v: Visibility) -> Attempt {
    let mut out = a.clone();
    if v == Visibility::NotPrivate {
        out.output_excerpt = None;
        out.paths_touched = PathsTouched::Counted(a.paths_touched.count());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::conformance::{sample_attempt, sample_record_run};
    use fl_core::ids::{GateId, ProjectId, RecordId, seq_iri};

    // Decision 2: `internal` counts as not private, as anything but
    // `private` does.
    #[test]
    fn only_private_is_private() {
        assert_eq!(Visibility::from_github("private"), Visibility::Private);
        for v in ["public", "internal", "", "Private"] {
            assert_eq!(Visibility::from_github(v), Visibility::NotPrivate, "{v}");
        }
    }

    #[test]
    fn a_private_repository_gets_every_field() {
        let mut r = sample_record_run(1, &GateId(seq_iri(1)), None);
        r.verdict = Verdict::error("spawn failed at /home/someone/bin/lint");
        assert_eq!(run(&r, Visibility::Private), r);
        let a = sample_attempt(2, &ProjectId(seq_iri(2)), &RecordId(seq_iri(3)));
        assert_eq!(attempt(&a, Visibility::Private), a);
    }

    // Decision 2, field by field.
    #[test]
    fn a_repository_that_is_not_private_gets_no_excerpt_no_error_detail_and_a_path_count() {
        let mut r = sample_record_run(1, &GateId(seq_iri(1)), None);
        r.verdict = Verdict::error("spawn failed at /home/someone/bin/lint");
        let p = run(&r, Visibility::NotPrivate);
        assert_eq!(p.output_excerpt, None);
        assert_eq!(p.verdict, Verdict::error(WITHHELD_ERROR_DETAIL));
        assert_eq!(
            (p.id.clone(), p.population, p.commit.clone()),
            (r.id.clone(), r.population, r.commit.clone()),
            "nothing else changes"
        );
        let pass = sample_record_run(2, &GateId(seq_iri(1)), None);
        assert_eq!(
            run(&pass, Visibility::NotPrivate).verdict,
            pass.verdict,
            "a pass has no detail to withhold"
        );
        let a = sample_attempt(3, &ProjectId(seq_iri(2)), &RecordId(seq_iri(3)));
        let q = attempt(&a, Visibility::NotPrivate);
        assert_eq!(q.output_excerpt, None);
        assert_eq!(q.paths_touched, PathsTouched::Counted(1));
        assert_eq!(q.tokens_in, a.tokens_in);
    }
}
