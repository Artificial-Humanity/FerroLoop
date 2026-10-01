//! What a command works with (GitHub tracker spec §1.3; GitHub ledger spec
//! §2.6): the local store for the catalog, whichever tracker the project is
//! bound to, and whichever ledger it is bound to.

use fl_core::decision::{Flushed, LeftLocal};
use fl_core::store::{Handles, Ledger, Roles, Tracker};
use fl_store::RedbStore;

pub struct Ctx<'a> {
    pub store: &'a RedbStore,
    /// The local store, or the GitHub tracker behind `CatalogChecked`.
    pub tracker: &'a dyn Tracker,
    /// Where runs, attempts and decisions are recorded: the local store,
    /// or — in mode B — a `SplitLedger` over it (plan B binds that).
    pub ledger: &'a dyn Ledger,
    /// The local store's handles, or `KindRouted` over the store and GitHub.
    pub handles: &'a dyn Handles,
    /// The GitHub tracker, for `fl github` and the publish check.
    pub github: Option<&'a fl_github::GithubTracker>,
    /// Where records and findings live, for messages.
    pub tracker_label: String,
}

impl Ctx<'_> {
    pub fn roles(&self) -> Roles<'_> {
        Roles {
            catalog: self.store,
            tracker: self.tracker,
            ledger: self.ledger,
        }
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
            tracker_label: String::new(),
        };
        assert!(
            std::ptr::addr_eq(ctx.roles().ledger, &flushes as &dyn Ledger),
            "the roles carry the bound ledger"
        );
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
