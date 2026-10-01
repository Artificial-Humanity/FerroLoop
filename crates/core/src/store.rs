use crate::decision::{Decision, Flushed};
use crate::fault::LedgerFault;
use crate::finding::Finding;
use crate::ids::{FindingId, GateId, Kind, ProjectId, RecordId};
use crate::iri::Iri;
use crate::log::{Attempt, GateRun};
use crate::model::{GateDef, GateKind, Project, Record, Selector, State, Transition};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("no such gate: {0}")]
    NoSuchGate(GateId),
    #[error("no such record: {0}")]
    NoSuchRecord(RecordId),
    #[error("no such finding: {0}")]
    NoSuchFinding(FindingId),
    /// ⚠ This store never held the id. It did not look anywhere else, so this
    /// is never "not found" — `searched` says exactly where it looked. An
    /// EMPTY `searched` means there was no store to look in at all, and the
    /// message says so rather than printing an empty list that reads like a
    /// search that ran.
    #[error(
        "no store holds {id} ({})",
        if searched.is_empty() {
            "no store exists yet, so there was nothing to search".to_string()
        } else {
            format!("searched: {}", searched.join(", "))
        }
    )]
    NotOwned { id: Iri, searched: Vec<String> },
    /// ⚠ The store holds `id`, but as a different kind of item than the
    /// method needs — a gate's IRI passed where a project is expected. This
    /// is neither "nothing there" (an empty list would claim the store
    /// looked at the right item and found nothing) nor `NotOwned` (the store
    /// does hold it).
    #[error(
        "{id} is a {} in this store, not a {}, so it cannot be used where a {} is needed",
        found.as_wire(),
        expected.as_wire(),
        expected.as_wire()
    )]
    WrongKind {
        id: Iri,
        expected: Kind,
        found: Kind,
    },
    #[error("{0} already exists; an insert never overwrites")]
    AlreadyExists(Iri),
    /// ⚠ The item belongs to a project this store imported from a manifest
    /// (GitHub tracker spec §4.2). Its definition is authored elsewhere, and
    /// a local edit would make this copy disagree with the manifest every
    /// other reader resolves.
    #[error(
        "{id}'s project was imported from a manifest, so this store cannot {action} it. \
         Change it in the store that authors the project, run `fl manifest export` there, \
         commit, then run `fl manifest import` here."
    )]
    Imported { id: Iri, action: &'static str },
    #[error("backend failure: {0}")]
    Backend(String),
    /// ⚠ The store could not be reached at all. This is "didn't look", and it
    /// must never read as an empty store.
    #[error("the store at {store} could not be opened: {cause}")]
    Unreachable { store: String, cause: String },
    #[error("{}", format_version_message(*found, *oldest, *newest))]
    FormatVersion {
        found: Option<u64>,
        oldest: u64,
        newest: u64,
    },
    /// ⚠ A stored record could not be read back into its type. This is what a
    /// wire-format change looks like from the other side, so the message names
    /// that cause and the remedy — a refusal that only reports serde's
    /// complaint tells the reader what broke but not what to do.
    #[error(
        "a stored record could not be read: {0}. \
         Two things produce this and the message cannot tell them apart. \
         Either the store was written by a version of this tool whose wire \
         format has since changed — there is no migration, so start a new \
         store or keep using the version that wrote this one — or the stored \
         bytes are damaged, in which case restore the file from a backup."
    )]
    Decode(String),
    /// ⚠ The store holds `to`, but not as the kind of item `from` expects. A
    /// reference that never resolves is one failure; a reference that once
    /// resolved and now points at the wrong kind is a different one, and the
    /// difference matters to whoever is debugging it.
    #[error(
        "{from} refers to {to}, which is dangling: the store holds that id, but not as this kind of item"
    )]
    Dangling { from: String, to: Iri },
    /// ⚠ The owner deleted the item (a GitHub issue answered 410). "It was
    /// here and is gone" is not "no such item".
    #[error(
        "{0} was deleted where it was held, so nothing can be read or written through it. If \
         it should still exist, raise it again: fl keeps no copy of it."
    )]
    Deleted(Iri),
    /// ⚠ fl's own record of an item disagrees with the item's visible state
    /// (GitHub tracker spec §3.4). fl adopts neither side silently.
    #[error(
        "{id} is diverged: {detail}. Run `fl github repair {id} --by <name>` to rewrite its \
         labels and status from fl's record — or, if the issue was never fl's, remove its fl \
         labels."
    )]
    Diverged { id: Iri, detail: String },
    /// ⚠ Another actor wrote the item while fl wrote it (spec §3.3). That
    /// write has landed, and fl's may have overwritten part of it.
    #[error(
        "{id} was changed by another actor while fl wrote it: {detail}. Read it again, check \
         it, and retry."
    )]
    Conflict { id: Iri, detail: String },
    /// ⚠ One entry id, two contents: this machine's copy and the shared
    /// ledger's (GitHub ledger spec §2.5). An entry never changes once it is
    /// written, so one of the two was altered afterwards.
    #[error(
        "{id} is recorded differently on this machine and in the shared ledger: {detail}. An \
         entry never changes once it is written, so one copy was altered afterwards, and fl \
         uses neither. Find out who changed it before trusting either copy."
    )]
    Tampered { id: Iri, detail: String },
    /// ⚠ A ledger's anchor — its branch's first commit — never changes
    /// (GitHub ledger spec §3.5).
    #[error(
        "the GitHub ledger of repository node {node_id} is anchored at commit {held}, and this \
         names {found}. A ledger's first commit never changes: a different one means the ledger \
         was deleted and created again, or the manifest was edited. Find out which before \
         trusting either."
    )]
    LedgerRootChanged {
        node_id: String,
        held: String,
        found: String,
    },
    /// ⚠ The cut-over is recorded once, when a repository's GitHub ledger
    /// is switched on (spec §2.1): moving it would strand every entry
    /// recorded between the two.
    #[error(
        "the GitHub ledger of repository node {node_id} was switched on at {held}, and this \
         names {found}. The cut-over never moves: entries between the two would never be \
         published. Keep the recorded one."
    )]
    CutoverChanged {
        node_id: String,
        held: Iri,
        found: Iri,
    },
    /// The id names something that exists but that fl did not create.
    #[error(
        "{id} is {what}, not an item fl created, so fl neither reads nor changes it. Name an \
         fl record or finding instead."
    )]
    NotAnFlItem { id: Iri, what: String },
    /// ⚠ The item moved out of the store that owned it (a transferred issue).
    #[error(
        "{id} was moved to {to}, outside the repository this tracker binds. Bind that \
         repository, or raise the item again here."
    )]
    Moved { id: Iri, to: String },
    /// ⚠ Reported, never waited out in silence (spec §7).
    #[error("GitHub's rate limit is spent until {reset}. Retry after that time.")]
    RateLimited { reset: String },
    /// ⚠ The configured name now reaches another repository (spec §2.4).
    #[error(
        "`{name}` now reaches a different repository (node {found}) from the one this \
         tracker is bound to (node {bound}). Correct the `github` binding in config.toml; \
         fl refuses every read and write until then."
    )]
    RepositoryReplaced {
        name: String,
        bound: String,
        found: String,
    },
    /// A credential is missing or refused. There is no fallback source.
    #[error("no usable GitHub credential: {0}")]
    Credential(String),
    /// ⚠ Spec §6: a security finding is written only where the public
    /// cannot read it.
    #[error(
        "refused: a security finding cannot be written to {repo}, whose visibility is \
         `{visibility}`. Use a local tracker for it, or bind a private repository."
    )]
    SecurityNotPrivate { repo: String, visibility: String },
    /// ⚠ The shared ledger is not there, or not as fl wrote it (GitHub
    /// ledger spec §3.5, §7).
    #[error("{0}")]
    Ledger(#[from] LedgerFault),
    /// ⚠ A decision may rest only on entries it publishes or that are
    /// already published (plan A ruling 8).
    #[error(
        "decision {decision} rests on {entry}, which is neither being published nor published. \
         A run tied to no record, one recorded before the GitHub ledger was switched on, or one \
         tied to another repository's record stays local, so a decision cannot rest on it. \
         Nothing was published."
    )]
    RestsOnLocalEntry { decision: Iri, entry: Iri },
    /// ⚠ Others appended to the ledger before each of fl's tries landed
    /// (GitHub ledger spec §3.2 step 5). Nothing was lost.
    #[error(
        "{store} was appended to by someone else before each of fl's {tries} tries could land, \
         so fl's own append did not. Nothing was lost; retry"
    )]
    Contended { store: String, tries: u32 },
}

/// Follow a stored reference. `Ok(None)` from the owning store means the
/// reference points at nothing of this kind: dangling. `NotOwned` passes
/// through unchanged — "no store holds it" is not the same as "gone".
pub fn follow<T>(
    from: &str,
    to: &Iri,
    got: Result<Option<T>, StoreError>,
) -> Result<T, StoreError> {
    match got {
        Ok(Some(t)) => Ok(t),
        Ok(None) => Err(StoreError::Dangling {
            from: from.to_string(),
            to: to.clone(),
        }),
        Err(e) => Err(e),
    }
}

/// `FormatVersion`'s message. A store from a NEWER fl is not damaged and
/// needs no new store: importing a manifest that carries a ledger root, for
/// one, raises a store to format 4, and the remedy on an older build is to
/// upgrade.
fn format_version_message(found: Option<u64>, oldest: u64, newest: u64) -> String {
    match found {
        Some(v) if v > newest => format!(
            "the store holds format {v}, which a newer fl wrote, and this version of fl reads \
             formats {oldest} to {newest}. Upgrade fl to open it: nothing is wrong with the store."
        ),
        _ => format!(
            "the store holds format {}, and this version of fl reads formats {oldest} to \
             {newest}. There is no migration: start a new store, or keep using the version of fl \
             that wrote this one.",
            match found {
                Some(v) => v.to_string(),
                None => "none (written before format versioning)".to_string(),
            }
        ),
    }
}

impl StoreError {
    /// Whether retrying later can succeed with nothing fixed first: GitHub
    /// could not be reached, its rate limit is spent, or others kept
    /// appending to the ledger. Only these may promise that the next
    /// decision publishes what this one could not (GitHub ledger spec §7).
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            StoreError::Unreachable { .. }
                | StoreError::RateLimited { .. }
                | StoreError::Contended { .. }
        )
    }
}

/// Definitions: projects, gates, transitions. Rarely changed; each belongs
/// to a repository.
///
/// ⚠ There is no method that stores a population, and adding one would break
/// the design. A population is enumerated fresh from the working tree at run
/// time so it cannot go stale in the store.
///
/// ⚠ Every method here, in [`Tracker`] and in [`Ledger`] that takes a
/// `ProjectId` refuses an id this store holds under another kind with
/// [`StoreError::WrongKind`] — never an empty answer. `add_finding` does the
/// same for its record.
pub trait Catalog {
    fn add_project(&self, root: &str) -> Result<ProjectId, StoreError>;
    fn get_project(&self, id: &ProjectId) -> Result<Option<Project>, StoreError>;
    fn list_projects(&self) -> Result<Vec<Project>, StoreError>;

    #[allow(clippy::too_many_arguments)]
    fn add_gate(
        &self,
        project: &ProjectId,
        name: &str,
        kind: GateKind,
        selector: Selector,
        min_population: u64,
        authored_at_commit: &str,
        authored_by: &str,
    ) -> Result<GateId, StoreError>;
    fn get_gate(&self, id: &GateId) -> Result<Option<GateDef>, StoreError>;
    fn list_gates(&self, project: &ProjectId) -> Result<Vec<GateDef>, StoreError>;
    fn update_gate(&self, def: &GateDef) -> Result<(), StoreError>;

    /// Stores a transition, keyed by `(project, name)`. Overwrites any existing transition with the same key (upsert semantics).
    fn add_transition(&self, t: Transition) -> Result<(), StoreError>;
    fn get_transition(
        &self,
        project: &ProjectId,
        name: &str,
    ) -> Result<Option<Transition>, StoreError>;
    /// Every transition a project declares.
    ///
    /// Needed because a transition is addressed by NAME, but a record move is
    /// addressed by the (from, to) pair it performs — so the move has to ask
    /// which declarations cover it.
    fn list_transitions(&self, project: &ProjectId) -> Result<Vec<Transition>, StoreError>;

    /// The kind this store holds `id` under, following an alias; `NotOwned`
    /// if it never held it. A split binding asks this to check a reference
    /// that crosses into another store (GitHub tracker spec §1.3).
    fn kind_of(&self, id: &Iri) -> Result<Kind, StoreError>;
}

/// Mutable state that people discuss: records and findings.
pub trait Tracker {
    fn add_record(&self, project: &ProjectId, title: &str) -> Result<RecordId, StoreError>;
    fn get_record(&self, id: &RecordId) -> Result<Option<Record>, StoreError>;
    fn list_records(&self, project: &ProjectId) -> Result<Vec<Record>, StoreError>;
    fn set_record_state(&self, id: &RecordId, state: State) -> Result<(), StoreError>;

    fn add_finding(&self, finding: Finding) -> Result<FindingId, StoreError>;
    fn get_finding(&self, id: &FindingId) -> Result<Option<Finding>, StoreError>;
    /// Replace a stored finding with `finding`, keyed by its primary id even
    /// when `finding.id` is an alias.
    ///
    /// ⚠ `also_known_as` is NOT taken from `finding`: the stored list is
    /// kept and the caller's is ignored. Only `add_alias` adds a name, and it
    /// updates the alias index and this list together. A caller holding a
    /// copy read before an `add_alias` would otherwise erase that alias from
    /// the list while the index still resolves it; a caller that edited the
    /// list would add a name the index cannot resolve.
    fn update_finding(&self, finding: &Finding) -> Result<(), StoreError>;
    fn list_findings(&self, project: &ProjectId) -> Result<Vec<Finding>, StoreError>;

    /// How many findings this actor raised and then withdrew.
    ///
    /// ⚠ Decision 27 puts a cost on a claim the reviewer cannot support. A
    /// cost nobody can read is not a cost, so this is part of the trait and
    /// not a report bolted on later.
    fn withdrawals_by(&self, actor: &str) -> Result<u64, StoreError>;

    /// Name `primary` by `alias` as well (spec §2.5). A lookup by `alias`
    /// then answers as `primary` would. `alias` must not already be used by
    /// this store, whether as a primary id or as another alias — an id
    /// space where identity is exact-string comparison has exactly one
    /// namespace, and an insert into it never overwrites.
    fn add_alias(&self, primary: &Iri, alias: Iri) -> Result<(), StoreError>;
}

/// Append-only evidence: gate runs and attempts.
pub trait Ledger {
    fn append_gate_run(&self, run: GateRun) -> Result<(), StoreError>;
    fn append_attempt(&self, attempt: Attempt) -> Result<(), StoreError>;
    fn gate_runs(&self, gate: &GateId) -> Result<Vec<GateRun>, StoreError>;
    fn attempts(&self, project: &ProjectId) -> Result<Vec<Attempt>, StoreError>;

    /// Publish a decision and the evidence it rests on (GitHub ledger spec
    /// §1.4, §2.2).
    ///
    /// ⚠⚠ Call it BEFORE the state change the decision supports, and treat
    /// an error as a refusal of the decision: no state change. A refused
    /// decision is flushed too (decision 11).
    ///
    /// A ledger with nowhere to publish — every local store — does nothing,
    /// which is this default.
    fn flush(&self, _decision: Decision) -> Result<Flushed, StoreError> {
        Ok(Flushed::NOTHING)
    }
}

/// Short names a person types and reads (spec §4). Display only: a handle
/// never enters a stored item or the wire. Per store and per kind.
pub trait Handles {
    fn handle_of(&self, kind: Kind, id: &Iri) -> Result<Option<u64>, StoreError>;
    fn resolve_handle(&self, kind: Kind, handle: u64) -> Result<Option<Iri>, StoreError>;
}

/// Binds each role to the store that backs it (spec §3.3). A store is the
/// backing entity; a role is what it backs. One store can back all three.
#[derive(Clone, Copy)]
pub struct Roles<'a> {
    pub catalog: &'a dyn Catalog,
    pub tracker: &'a dyn Tracker,
    pub ledger: &'a dyn Ledger,
}

impl<'a> Roles<'a> {
    /// One store backing all three roles.
    pub fn single<S: Catalog + Tracker + Ledger>(store: &'a S) -> Self {
        Self {
            catalog: store,
            tracker: store,
            ledger: store,
        }
    }
}

/// A tracker whose project and record references are checked against a
/// catalog held by ANOTHER store (GitHub tracker spec §1.3, §8.1 item 2).
///
/// ⚠ For a split binding only. It reads "the catalog holds this id" as "it
/// is not a record", which is true only when the catalog's store backs no
/// tracker. A store that backs both roles checks its own references.
pub struct CatalogChecked<'a> {
    pub catalog: &'a dyn Catalog,
    pub tracker: &'a dyn Tracker,
}

impl CatalogChecked<'_> {
    fn project(&self, p: &ProjectId) -> Result<(), StoreError> {
        match self.catalog.kind_of(p.iri())? {
            Kind::Project => Ok(()),
            found => Err(StoreError::WrongKind {
                id: p.iri().clone(),
                expected: Kind::Project,
                found,
            }),
        }
    }

    fn record(&self, r: &RecordId) -> Result<(), StoreError> {
        match self.catalog.kind_of(r.iri()) {
            Ok(found) => Err(StoreError::WrongKind {
                id: r.iri().clone(),
                expected: Kind::Record,
                found,
            }),
            // Not the catalog's: the tracker decides whether it is a record.
            Err(StoreError::NotOwned { .. }) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

impl Tracker for CatalogChecked<'_> {
    fn add_record(&self, project: &ProjectId, title: &str) -> Result<RecordId, StoreError> {
        self.project(project)?;
        self.tracker.add_record(project, title)
    }
    fn get_record(&self, id: &RecordId) -> Result<Option<Record>, StoreError> {
        self.tracker.get_record(id)
    }
    fn list_records(&self, project: &ProjectId) -> Result<Vec<Record>, StoreError> {
        self.project(project)?;
        self.tracker.list_records(project)
    }
    fn set_record_state(&self, id: &RecordId, state: State) -> Result<(), StoreError> {
        self.tracker.set_record_state(id, state)
    }
    fn add_finding(&self, finding: Finding) -> Result<FindingId, StoreError> {
        self.project(&finding.project)?;
        self.record(&finding.record)?;
        self.tracker.add_finding(finding)
    }
    fn get_finding(&self, id: &FindingId) -> Result<Option<Finding>, StoreError> {
        self.tracker.get_finding(id)
    }
    fn update_finding(&self, finding: &Finding) -> Result<(), StoreError> {
        self.tracker.update_finding(finding)
    }
    fn list_findings(&self, project: &ProjectId) -> Result<Vec<Finding>, StoreError> {
        self.project(project)?;
        self.tracker.list_findings(project)
    }
    fn withdrawals_by(&self, actor: &str) -> Result<u64, StoreError> {
        self.tracker.withdrawals_by(actor)
    }
    fn add_alias(&self, primary: &Iri, alias: Iri) -> Result<(), StoreError> {
        self.tracker.add_alias(primary, alias)
    }
}

/// Handles for a split binding: projects and gates are the catalog's to
/// number, records and findings the tracker's.
pub struct KindRouted<'a> {
    pub catalog: &'a dyn Handles,
    pub tracker: &'a dyn Handles,
}

impl KindRouted<'_> {
    fn owner(&self, kind: Kind) -> &dyn Handles {
        // Every kind by name: a new kind must be routed on purpose.
        match kind {
            Kind::Project | Kind::Gate => self.catalog,
            Kind::Record | Kind::Finding => self.tracker,
        }
    }
}

impl Handles for KindRouted<'_> {
    fn handle_of(&self, kind: Kind, id: &Iri) -> Result<Option<u64>, StoreError> {
        self.owner(kind).handle_of(kind, id)
    }
    fn resolve_handle(&self, kind: Kind, handle: u64) -> Result<Option<Iri>, StoreError> {
        self.owner(kind).resolve_handle(kind, handle)
    }
}

/// What a local store remembers about the GitHub repositories a tracker is
/// bound to (GitHub tracker spec §2.4): the repository's `node_id`, keyed by
/// the configured `owner/repo`, compared without regard to case; and the
/// ledger root of each repository whose GitHub ledger it knows (GitHub
/// ledger spec §6.1 step 4), keyed by `node_id`.
pub trait Bindings {
    fn bound_node_id(&self, repo: &str) -> Result<Option<String>, StoreError>;
    fn bind_node_id(&self, repo: &str, node_id: &str) -> Result<(), StoreError>;
    fn ledger_root(&self, node_id: &str) -> Result<Option<String>, StoreError>;
    /// ⚠ An anchor never changes: a different root for a `node_id` that has
    /// one is `LedgerRootChanged`; the same root again is a no-op.
    fn set_ledger_root(&self, node_id: &str, commit: &str) -> Result<(), StoreError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemStore;
    use crate::ids::seq_iri;
    use crate::model::{CommandSpec, GateKind, PopulationDelivery};

    /// A tracker that must never be asked: every refusal below has to come
    /// from the binding's own check, before the tracker is reached.
    struct NeverAsked;
    impl Tracker for NeverAsked {
        fn add_record(&self, _: &ProjectId, _: &str) -> Result<RecordId, StoreError> {
            unreachable!("the binding must refuse before the tracker is asked")
        }
        fn get_record(&self, _: &RecordId) -> Result<Option<Record>, StoreError> {
            unreachable!("not used")
        }
        fn list_records(&self, _: &ProjectId) -> Result<Vec<Record>, StoreError> {
            Ok(vec![])
        }
        fn set_record_state(&self, _: &RecordId, _: State) -> Result<(), StoreError> {
            unreachable!("not used")
        }
        fn add_finding(&self, _: Finding) -> Result<FindingId, StoreError> {
            unreachable!("the binding must refuse before the tracker is asked")
        }
        fn get_finding(&self, _: &FindingId) -> Result<Option<Finding>, StoreError> {
            unreachable!("not used")
        }
        fn update_finding(&self, _: &Finding) -> Result<(), StoreError> {
            unreachable!("not used")
        }
        fn list_findings(&self, _: &ProjectId) -> Result<Vec<Finding>, StoreError> {
            unreachable!("the binding must refuse before the tracker is asked")
        }
        fn withdrawals_by(&self, _: &str) -> Result<u64, StoreError> {
            unreachable!("not used")
        }
        fn add_alias(&self, _: &Iri, _: Iri) -> Result<(), StoreError> {
            unreachable!("not used")
        }
    }

    fn gate(catalog: &MemStore, p: &ProjectId) -> GateId {
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
        catalog.add_gate(p, "g", kind, sel, 1, "c", "o").unwrap()
    }

    #[test]
    fn a_split_binding_refuses_a_project_the_catalog_never_held() {
        let catalog = MemStore::default();
        let t = CatalogChecked {
            catalog: &catalog,
            tracker: &NeverAsked,
        };
        let stranger = ProjectId(seq_iri(99));
        let err = t.add_record(&stranger, "t").unwrap_err();
        assert!(matches!(err, StoreError::NotOwned { .. }), "{err:?}");
        let err = t.list_findings(&stranger).unwrap_err();
        assert!(matches!(err, StoreError::NotOwned { .. }), "{err:?}");
    }

    #[test]
    fn a_split_binding_refuses_another_kind_where_a_project_or_record_is_needed() {
        let catalog = MemStore::default();
        let p = catalog.add_project("/p").unwrap();
        let g = gate(&catalog, &p);
        let t = CatalogChecked {
            catalog: &catalog,
            tracker: &NeverAsked,
        };
        let err = t.add_record(&ProjectId(g.0.clone()), "t").unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::WrongKind {
                    expected: Kind::Project,
                    found: Kind::Gate,
                    ..
                }
            ),
            "{err:?}"
        );
        let err = t
            .add_finding(Finding::raise(p, RecordId(g.0.clone()), "a", "c"))
            .unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::WrongKind {
                    expected: Kind::Record,
                    found: Kind::Gate,
                    ..
                }
            ),
            "{err:?}"
        );
    }

    #[test]
    fn a_split_binding_passes_a_held_project_through_to_the_tracker() {
        let catalog = MemStore::default();
        let p = catalog.add_project("/p").unwrap();
        let t = CatalogChecked {
            catalog: &catalog,
            tracker: &NeverAsked,
        };
        assert_eq!(t.list_records(&p).unwrap(), vec![]);
    }

    #[test]
    fn kind_routed_asks_the_store_that_holds_each_kind() {
        let catalog = MemStore::default();
        let tracker = MemStore::default();
        let p = catalog.add_project("/p").unwrap();
        // MemStore ids are sequential, so both stores would mint the same
        // first id. Burn one in the tracker's store, so `tp` is an id the
        // catalog does not hold.
        tracker.add_project("/burned").unwrap();
        let tp = tracker.add_project("/q").unwrap();
        let r = tracker.add_record(&tp, "t").unwrap();
        let h = KindRouted {
            catalog: &catalog,
            tracker: &tracker,
        };
        assert_eq!(h.handle_of(Kind::Project, p.iri()).unwrap(), Some(1));
        assert_eq!(h.handle_of(Kind::Record, r.iri()).unwrap(), Some(1));
        assert_ne!(p, tp, "the routing check below needs two different ids");
        // A project the TRACKER's store holds is not asked of the tracker.
        assert_eq!(h.handle_of(Kind::Project, tp.iri()).unwrap(), None);
        assert_eq!(
            h.resolve_handle(Kind::Record, 1).unwrap().as_ref(),
            Some(r.iri())
        );
    }

    #[test]
    fn a_memory_store_remembers_a_binding_by_case_insensitive_name() {
        let s = MemStore::default();
        assert_eq!(s.bound_node_id("Acme/Widgets").unwrap(), None);
        s.bind_node_id("Acme/Widgets", "R_1").unwrap();
        assert_eq!(
            s.bound_node_id("ACME/widgets").unwrap().as_deref(),
            Some("R_1")
        );
    }

    // Spec §1.4: a ledger with nowhere to publish — every local store —
    // answers a flush with `Nothing`, and changes nothing.
    #[test]
    fn a_local_store_has_nowhere_to_publish_so_its_flush_does_nothing() {
        use crate::at::At;
        use crate::decision::{Decision, Flushed, Outcome};
        use crate::log::AttemptStatus;
        let s = MemStore::default();
        let p = s.add_project("/p").unwrap();
        let r = s.add_record(&p, "t").unwrap();
        let d = Decision {
            id: seq_iri(90),
            at: At::from_unix_millis(1),
            record: r,
            finding: None,
            outcome: Outcome::Attempt {
                status: AttemptStatus::Completed,
            },
            rests_on: vec![],
        };
        assert_eq!(s.flush(d).unwrap(), Flushed::NOTHING);
    }

    // Spec §3.5: the anchor never changes. A different root for a
    // repository that has one is refused, the same one again is not.
    #[test]
    fn a_memory_store_records_a_ledger_root_once() {
        let s = MemStore::default();
        assert_eq!(s.ledger_root("R_1").unwrap(), None);
        s.set_ledger_root("R_1", "abc").unwrap();
        s.set_ledger_root("R_1", "abc").unwrap();
        let err = s.set_ledger_root("R_1", "def").unwrap_err();
        assert!(
            matches!(err, StoreError::LedgerRootChanged { ref held, ref found, .. } if held == "abc" && found == "def"),
            "{err:?}"
        );
        assert_eq!(s.ledger_root("R_1").unwrap().as_deref(), Some("abc"));
        assert_eq!(s.ledger_root("R_2").unwrap(), None);
    }

    // Spec §7: only a GitHub that could not be reached, a spent rate limit,
    // or a ledger others kept appending to clears up by waiting. Every
    // other refusal names something to fix first.
    #[test]
    fn only_an_unreachable_a_rate_limited_or_a_contended_ledger_is_transient() {
        use crate::fault::LedgerFault;
        let transient = [
            StoreError::Unreachable {
                store: "s".into(),
                cause: "c".into(),
            },
            StoreError::RateLimited { reset: "r".into() },
            StoreError::Contended {
                store: "s".into(),
                tries: 5,
            },
        ];
        for e in &transient {
            assert!(e.is_transient(), "{e}");
        }
        let lasting = [
            StoreError::Backend("b".into()),
            StoreError::Credential("c".into()),
            StoreError::Tampered {
                id: seq_iri(1),
                detail: "d".into(),
            },
            StoreError::NotOwned {
                id: seq_iri(1),
                searched: vec![],
            },
            StoreError::RestsOnLocalEntry {
                decision: seq_iri(1),
                entry: seq_iri(2),
            },
            StoreError::Ledger(LedgerFault::NotSetUp {
                repo: "acme/widgets".into(),
            }),
        ];
        for e in &lasting {
            assert!(!e.is_transient(), "{e}");
        }
    }

    #[test]
    fn a_ledger_fault_reads_as_itself() {
        use crate::fault::LedgerFault;
        let fault = LedgerFault::NotSetUp {
            repo: "acme/widgets".into(),
        };
        let e: StoreError = fault.clone().into();
        assert_eq!(e.to_string(), fault.to_string());
    }
}
