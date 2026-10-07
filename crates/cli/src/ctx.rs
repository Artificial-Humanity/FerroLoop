//! What a command works with (GitHub tracker spec §1.3; GitHub ledger spec
//! §2.6): the local store for the catalog, whichever tracker the project is
//! bound to, and whichever ledger it is bound to.

use fl_core::decision::{Decision, Flushed, LeftLocal};
use fl_core::ids::{GateId, Kind, ProjectId};
use fl_core::iri::Iri;
use fl_core::log::{Attempt, GateRun};
use fl_core::routing::Tier;
use fl_core::store::{Handles, Ledger, Roles, StoreError, Tracker};
use fl_store::RedbStore;
use std::cell::RefCell;

pub struct Ctx<'a> {
    pub store: &'a RedbStore,
    /// The local store, or the GitHub tracker behind `CatalogChecked`.
    pub tracker: &'a dyn Tracker,
    /// Where runs, attempts and decisions are recorded: the local store,
    /// or — when the binding names the GitHub ledger — a `SplitLedger` over
    /// the local store and `github_ledger`.
    pub ledger: &'a dyn Ledger,
    /// The local store's handles, or `KindRouted` over the store and GitHub.
    pub handles: &'a dyn Handles,
    /// The GitHub tracker, for `fl github` and the publish check.
    pub github: Option<&'a fl_github::GithubTracker>,
    /// The GitHub ledger, when the binding names it (GitHub ledger spec
    /// §1.5): over the tracker's client and the local store.
    pub github_ledger: Option<&'a fl_github::GithubLedger<'a>>,
    /// The bound ledger as decisions see it, remembering each decision
    /// whose flush landed, for its comment (GitHub ledger spec §4.1). Set
    /// with the GitHub ledger, and only then.
    pub witness: Option<&'a Witness<'a>>,
    /// Where records and findings live, for messages.
    pub tracker_label: String,
    /// A routed store's tiers (routing spec §1.3): the router — which is
    /// also `tracker` — and the GitHub tier it opens lazily. `None` in an
    /// unrouted store.
    pub tiers: Option<&'a crate::tiers::Tiers<'a>>,
}

impl Ctx<'_> {
    pub fn roles(&self) -> Roles<'_> {
        Roles {
            catalog: self.store,
            tracker: self.tracker,
            ledger: self.ledger,
        }
    }

    /// How a person reads a record's or finding's id (routing spec §2.3):
    /// in a routed store a GitHub item is `#41` and a local one `41`, so
    /// every printed handle can be typed back. Elsewhere, as before.
    pub fn show_item(&self, kind: Kind, id: &Iri) -> anyhow::Result<String> {
        let Some(t) = self.tiers else {
            return crate::refs::show(self.handles, kind, id);
        };
        match t.router.tier_of(id) {
            Tier::Local => crate::refs::show(self.store, kind, id),
            Tier::Github => Ok(match t.github.open()?.handle_of(kind, id)? {
                Some(n) => format!("#{n}"),
                None => id.to_string(),
            }),
        }
    }

    /// Whether `id` is written where another machine reads it — on GitHub
    /// — so a gate it names must be in the committed manifest (GitHub
    /// tracker spec §4.3).
    pub fn on_github(&self, id: &Iri) -> bool {
        match self.tiers {
            Some(t) => t.router.tier_of(id) == Tier::Github,
            None => self.github.is_some(),
        }
    }
}

/// One decision this command flushed, and what its flush did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flush {
    pub decision: Decision,
    pub flushed: Flushed,
}

/// The bound ledger, remembering each decision whose flush landed, so the
/// command can post its comment once the state change is done (GitHub
/// ledger spec §4.1). Every call goes through to the ledger it wraps.
pub struct Witness<'a> {
    inner: &'a dyn Ledger,
    seen: RefCell<Vec<Flush>>,
}

impl<'a> Witness<'a> {
    pub fn new(inner: &'a dyn Ledger) -> Self {
        Self {
            inner,
            seen: RefCell::new(Vec::new()),
        }
    }

    /// The decisions flushed since the last call, oldest first.
    pub fn take(&self) -> Vec<Flush> {
        std::mem::take(&mut *self.seen.borrow_mut())
    }
}

impl Ledger for Witness<'_> {
    fn append_gate_run(&self, run: GateRun) -> Result<(), StoreError> {
        self.inner.append_gate_run(run)
    }

    fn append_attempt(&self, attempt: Attempt) -> Result<(), StoreError> {
        self.inner.append_attempt(attempt)
    }

    fn gate_runs(&self, gate: &GateId) -> Result<Vec<GateRun>, StoreError> {
        self.inner.gate_runs(gate)
    }

    fn attempts(&self, project: &ProjectId) -> Result<Vec<Attempt>, StoreError> {
        self.inner.attempts(project)
    }

    /// ⚠ Remembered only once the flush returned: a refused flush
    /// published nothing for a comment to show.
    fn flush(&self, decision: Decision) -> Result<Flushed, StoreError> {
        let kept = decision.clone();
        let flushed = self.inner.flush(decision)?;
        self.seen.borrow_mut().push(Flush {
            decision: kept,
            flushed: flushed.clone(),
        });
        Ok(flushed)
    }
}

/// What a flush left local, one line each, for a person. Reported, never a
/// refusal (GitHub ledger spec §2.1).
pub fn flush_notes(flushed: &Flushed) -> Vec<String> {
    flushed
        .left_local
        .iter()
        .map(|left| match left {
            LeftLocal::NoCutover => "note: this repository's GitHub ledger was never switched \
                                      on (`fl github ledger init`), so nothing was published; \
                                      the runs stay in the local store"
                .to_string(),
            LeftLocal::OtherRepository { entry, record } => format!(
                "note: {entry} is tied to {record}, which is not an issue of this repository, \
                 so it stays in the local store"
            ),
        })
        .collect()
}

/// [`flush_notes`], on stderr.
pub fn report_flush(flushed: &Flushed) {
    for line in flush_notes(flushed) {
        eprintln!("{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Flushes;
    use fl_core::Iri;
    use fl_core::ids::RecordId;

    // Spec §2.6: `Ctx::roles()` binds the ledger the command was given —
    // never the local store behind its back.
    #[test]
    fn the_roles_bind_the_ledger_the_command_was_given() {
        let dir = tempfile::tempdir().unwrap();
        let store = RedbStore::open(&dir.path().join("s.redb")).unwrap();
        let flushes = Flushes::default();
        let ctx = Ctx {
            store: &store,
            tracker: &store,
            ledger: &flushes,
            handles: &store,
            github: None,
            github_ledger: None,
            witness: None,
            tracker_label: String::new(),
            tiers: None,
        };
        assert!(
            std::ptr::addr_eq(ctx.roles().ledger, &flushes as &dyn Ledger),
            "the roles carry the bound ledger"
        );
    }

    fn check_decision() -> fl_core::decision::Decision {
        fl_core::decision::Decision {
            id: fl_core::ids::seq_iri(9),
            at: fl_core::at::At::from_unix_millis(1),
            record: RecordId(Iri::parse("https://github.com/acme/widgets/issues/1").unwrap()),
            finding: None,
            outcome: fl_core::decision::Outcome::Check {
                transition: fl_core::decision::TransitionOutcome {
                    transition: "launch".into(),
                    passed: true,
                },
            },
            rests_on: vec![],
        }
    }

    // Spec §4.1: only a decision whose flush landed has a comment to post;
    // the flush itself goes through unchanged.
    #[test]
    fn the_witness_remembers_only_a_flush_that_landed_and_forwards_it() {
        let landed = Flushes::default();
        let w = Witness::new(&landed);
        assert_eq!(
            w.flush(check_decision()).unwrap().commit.as_deref(),
            Some("c1")
        );
        assert_eq!(landed.decisions.borrow().len(), 1, "forwarded");
        let seen = w.take();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].decision, check_decision());
        assert!(w.take().is_empty(), "taken once");
        let refused = Flushes::refusing();
        let w = Witness::new(&refused);
        assert!(w.flush(check_decision()).is_err());
        assert!(w.take().is_empty());
    }

    #[test]
    fn a_flush_that_left_nothing_local_says_nothing() {
        assert!(flush_notes(&Flushed::NOTHING).is_empty());
    }

    // Spec §2.1: what stays local is reported, one line per cause.
    #[test]
    fn what_a_flush_left_local_is_reported_by_cause() {
        let entry = fl_core::ids::seq_iri(7);
        let record = RecordId(Iri::parse("https://github.com/acme/other/issues/3").unwrap());
        let lines = flush_notes(&Flushed {
            commit: None,
            left_local: vec![
                LeftLocal::NoCutover,
                LeftLocal::OtherRepository {
                    entry: entry.clone(),
                    record: record.clone(),
                },
            ],
        });
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].contains("never switched on"), "{lines:?}");
        assert!(
            lines[1].contains(entry.as_str()) && lines[1].contains(record.iri().as_str()),
            "{lines:?}"
        );
    }
}
