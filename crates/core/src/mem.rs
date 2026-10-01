use crate::finding::{Finding, FindingState};
use crate::ids::{FindingId, GateId, Kind, ProjectId, RecordId, seq_iri};
use crate::iri::Iri;
use crate::log::{Attempt, GateRun};
use crate::model::{GateDef, GateKind, Project, Record, Selector, State, Transition};
use crate::split::{Outbox, Pending};
use crate::store::{Catalog, Handles, Ledger, StoreError, Tracker};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

/// An in-memory store for tests. Deliberately lives in `fl-core` so the
/// engine's own tests need no backend at all. Backs all three roles.
///
/// Ids are minted with [`seq_iri`], not UUIDv7: this store must not use a
/// clock or randomness, so its ids are deterministic.
#[derive(Default)]
pub struct MemStore {
    inner: RefCell<Inner>,
}

const LABEL: &str = "memory";

#[derive(Default)]
struct Inner {
    next_id: u64,
    /// Every id this store ever minted, with its kind. Append-only: nothing
    /// removes an entry. Ownership is membership (spec §2.6).
    owned: BTreeMap<Iri, Kind>,
    next_handle: BTreeMap<Kind, u64>,
    handles: BTreeMap<(Kind, u64), Iri>,
    handle_of: BTreeMap<Iri, u64>,
    projects: BTreeMap<Iri, Project>,
    gates: BTreeMap<Iri, GateDef>,
    transitions: BTreeMap<(Iri, String), Transition>,
    records: BTreeMap<Iri, Record>,
    runs: Vec<GateRun>,
    attempts: Vec<Attempt>,
    findings: BTreeMap<Iri, Finding>,
    /// alias → primary. `"alias"` is not a `Kind`: it is an index marker, so
    /// an alias never enters `owned`. `check` follows it before deciding.
    aliases: BTreeMap<Iri, Iri>,
    /// configured `owner/repo` (lowercase) → the repository's `node_id`.
    bindings: BTreeMap<String, String>,
    /// (repository `node_id`, entry id) for every entry marked published.
    published: BTreeSet<(String, Iri)>,
    /// Every entry published to any repository, or set aside: it waits no
    /// more (spec §2.1).
    settled: BTreeSet<Iri>,
    /// repository `node_id` → the id after which entries are publishable.
    cutovers: BTreeMap<String, Iri>,
    /// repository `node_id` → its GitHub ledger's first commit.
    ledger_roots: BTreeMap<String, String>,
}

impl Inner {
    fn mint(&mut self, kind: Kind) -> Iri {
        self.next_id += 1;
        let id = seq_iri(self.next_id);
        self.owned.insert(id.clone(), kind);
        let n = self.next_handle.entry(kind).or_insert(0);
        *n += 1;
        self.handles.insert((kind, *n), id.clone());
        self.handle_of.insert(id.clone(), *n);
        id
    }

    /// ⚠ Every method that takes an id asks this first — list methods too.
    /// A list over a project this store never held is "didn't look", and an
    /// empty list would say "looked, found nothing".
    ///
    /// Follows one alias hop: an alias is never in `owned`, so this looks it
    /// up in `aliases` and reports the PRIMARY's kind. This is what makes an
    /// alias "owned" for every other check in this module.
    fn check(&self, id: &Iri) -> Result<Kind, StoreError> {
        if let Some(kind) = self.owned.get(id) {
            return Ok(*kind);
        }
        if let Some(kind) = self
            .aliases
            .get(id)
            .and_then(|primary| self.owned.get(primary))
        {
            return Ok(*kind);
        }
        Err(StoreError::NotOwned {
            id: id.clone(),
            searched: vec![LABEL.to_string()],
        })
    }

    /// `check`, and then refuse an id held under any kind but `expected`
    /// with `WrongKind`. Every method that takes a project (and
    /// `add_finding`'s record) asks this rather than `check`: answering an
    /// empty list for a gate's IRI would say "looked, found nothing" about
    /// an item that was never a project.
    fn check_kind(&self, id: &Iri, expected: Kind) -> Result<(), StoreError> {
        let found = self.check(id)?;
        if found != expected {
            return Err(StoreError::WrongKind {
                id: id.clone(),
                expected,
                found,
            });
        }
        Ok(())
    }

    /// `id` itself, or the primary it aliases. Used wherever a lookup needs
    /// the key a row is actually stored under — `check` alone answers
    /// ownership, not which key to read.
    fn resolve(&self, id: &Iri) -> Iri {
        self.aliases.get(id).cloned().unwrap_or_else(|| id.clone())
    }
}

impl Catalog for MemStore {
    fn add_project(&self, root: &str) -> Result<ProjectId, StoreError> {
        let mut s = self.inner.borrow_mut();
        let id = ProjectId(s.mint(Kind::Project));
        s.projects.insert(
            id.0.clone(),
            Project {
                id: id.clone(),
                root: root.to_string(),
            },
        );
        Ok(id)
    }

    fn get_project(&self, id: &ProjectId) -> Result<Option<Project>, StoreError> {
        let s = self.inner.borrow();
        s.check(&id.0)?;
        Ok(s.projects.get(&id.0).cloned())
    }

    fn list_projects(&self) -> Result<Vec<Project>, StoreError> {
        Ok(self.inner.borrow().projects.values().cloned().collect())
    }

    fn add_gate(
        &self,
        project: &ProjectId,
        name: &str,
        kind: GateKind,
        selector: Selector,
        min_population: u64,
        authored_at_commit: &str,
        authored_by: &str,
    ) -> Result<GateId, StoreError> {
        let mut s = self.inner.borrow_mut();
        s.check_kind(&project.0, Kind::Project)?;
        let id = GateId(s.mint(Kind::Gate));
        s.gates.insert(
            id.0.clone(),
            GateDef {
                id: id.clone(),
                project: project.clone(),
                name: name.to_string(),
                kind,
                selector,
                min_population,
                authored_at_commit: authored_at_commit.to_string(),
                authored_by: authored_by.to_string(),
                last_pass_commit: None,
            },
        );
        Ok(id)
    }

    fn get_gate(&self, id: &GateId) -> Result<Option<GateDef>, StoreError> {
        let s = self.inner.borrow();
        s.check(&id.0)?;
        Ok(s.gates.get(&id.0).cloned())
    }

    fn list_gates(&self, project: &ProjectId) -> Result<Vec<GateDef>, StoreError> {
        let s = self.inner.borrow();
        s.check_kind(&project.0, Kind::Project)?;
        Ok(s.gates
            .values()
            .filter(|g| g.project == *project)
            .cloned()
            .collect())
    }

    fn update_gate(&self, def: &GateDef) -> Result<(), StoreError> {
        let mut s = self.inner.borrow_mut();
        s.check(&def.id.0)?;
        if !s.gates.contains_key(&def.id.0) {
            return Err(StoreError::NoSuchGate(def.id.clone()));
        }
        s.gates.insert(def.id.0.clone(), def.clone());
        Ok(())
    }

    fn add_transition(&self, t: Transition) -> Result<(), StoreError> {
        let mut s = self.inner.borrow_mut();
        s.check_kind(&t.project.0, Kind::Project)?;
        s.transitions
            .insert((t.project.0.clone(), t.name.clone()), t);
        Ok(())
    }

    fn get_transition(
        &self,
        project: &ProjectId,
        name: &str,
    ) -> Result<Option<Transition>, StoreError> {
        let s = self.inner.borrow();
        s.check_kind(&project.0, Kind::Project)?;
        Ok(s.transitions
            .get(&(project.0.clone(), name.to_string()))
            .cloned())
    }

    fn list_transitions(&self, project: &ProjectId) -> Result<Vec<Transition>, StoreError> {
        let s = self.inner.borrow();
        s.check_kind(&project.0, Kind::Project)?;
        Ok(s.transitions
            .values()
            .filter(|t| t.project == *project)
            .cloned()
            .collect())
    }

    fn kind_of(&self, id: &Iri) -> Result<Kind, StoreError> {
        self.inner.borrow().check(id)
    }
}

impl Tracker for MemStore {
    fn add_record(&self, project: &ProjectId, title: &str) -> Result<RecordId, StoreError> {
        let mut s = self.inner.borrow_mut();
        s.check_kind(&project.0, Kind::Project)?;
        let id = RecordId(s.mint(Kind::Record));
        s.records.insert(
            id.0.clone(),
            Record {
                id: id.clone(),
                project: project.clone(),
                title: title.to_string(),
                state: State::Todo,
                also_known_as: vec![],
            },
        );
        Ok(id)
    }

    fn get_record(&self, id: &RecordId) -> Result<Option<Record>, StoreError> {
        let s = self.inner.borrow();
        s.check(&id.0)?;
        let target = s.resolve(&id.0);
        Ok(s.records.get(&target).cloned())
    }

    fn list_records(&self, project: &ProjectId) -> Result<Vec<Record>, StoreError> {
        let s = self.inner.borrow();
        s.check_kind(&project.0, Kind::Project)?;
        Ok(s.records
            .values()
            .filter(|r| r.project == *project)
            .cloned()
            .collect())
    }

    fn set_record_state(&self, id: &RecordId, state: State) -> Result<(), StoreError> {
        let mut s = self.inner.borrow_mut();
        s.check(&id.0)?;
        // `id` may be an alias: resolve to the primary key `records` is
        // actually keyed by.
        let target = s.resolve(&id.0);
        let rec = s
            .records
            .get_mut(&target)
            .ok_or_else(|| StoreError::NoSuchRecord(id.clone()))?;
        rec.state = state;
        Ok(())
    }

    fn add_finding(&self, finding: Finding) -> Result<FindingId, StoreError> {
        let mut s = self.inner.borrow_mut();
        s.check_kind(&finding.project.0, Kind::Project)?;
        s.check_kind(&finding.record.0, Kind::Record)?;
        // `record` may have been given as an alias: resolve to the primary,
        // so two findings raised against the same record always agree on
        // which IRI names it.
        let record_primary = s.resolve(&finding.record.0);
        let id = FindingId(s.mint(Kind::Finding));
        let mut finding = finding;
        finding.id = id.clone();
        finding.record = RecordId(record_primary);
        s.findings.insert(id.0.clone(), finding);
        Ok(id)
    }

    fn get_finding(&self, id: &FindingId) -> Result<Option<Finding>, StoreError> {
        let s = self.inner.borrow();
        s.check(&id.0)?;
        let target = s.resolve(&id.0);
        Ok(s.findings.get(&target).cloned())
    }

    fn update_finding(&self, finding: &Finding) -> Result<(), StoreError> {
        let mut s = self.inner.borrow_mut();
        s.check(&finding.id.0)?;
        // `finding.id` (what the caller passed) may be an alias: resolve to
        // the primary key `findings` is actually keyed by, and pin the
        // written row's own `id` to that primary too — even if the caller's
        // struct still carries the alias — so an update through an alias
        // lands on, and stays keyed by, the primary, never a second row
        // under the alias.
        let target = s.resolve(&finding.id.0);
        if !s.findings.contains_key(&target) {
            return Err(StoreError::NoSuchFinding(finding.id.clone()));
        }
        // The stored `also_known_as` is kept and the caller's ignored (see
        // the trait): only `add_alias` adds a name.
        let also_known_as = s.findings[&target].also_known_as.clone();
        let mut stored = finding.clone();
        stored.id = FindingId(target.clone());
        stored.also_known_as = also_known_as;
        s.findings.insert(target, stored);
        Ok(())
    }

    fn list_findings(&self, project: &ProjectId) -> Result<Vec<Finding>, StoreError> {
        let s = self.inner.borrow();
        s.check_kind(&project.0, Kind::Project)?;
        Ok(s.findings
            .values()
            .filter(|f| f.project == *project)
            .cloned()
            .collect())
    }

    fn withdrawals_by(&self, actor: &str) -> Result<u64, StoreError> {
        Ok(self
            .inner
            .borrow()
            .findings
            .values()
            .filter(|f| f.raised_by == actor && f.state == FindingState::Withdrawn)
            .count() as u64)
    }

    fn add_alias(&self, primary: &Iri, alias: Iri) -> Result<(), StoreError> {
        let mut s = self.inner.borrow_mut();
        // `alias` must be unused, whether as a primary id or as another
        // alias: this store has exactly one id namespace.
        if s.owned.contains_key(&alias) || s.aliases.contains_key(&alias) {
            return Err(StoreError::AlreadyExists(alias));
        }
        // `primary` may itself be an alias; resolve to the true primary so
        // `aliases` never chains and the row update below finds the row.
        let resolved = s.resolve(primary);
        let kind = s.check(&resolved)?;
        match kind {
            Kind::Record => {
                s.records
                    .get_mut(&resolved)
                    .expect("a Record kind means records holds this row")
                    .also_known_as
                    .push(alias.clone());
            }
            Kind::Finding => {
                s.findings
                    .get_mut(&resolved)
                    .expect("a Finding kind means findings holds this row")
                    .also_known_as
                    .push(alias.clone());
            }
            other => {
                return Err(StoreError::Backend(format!(
                    "{resolved} is a {}, and only a record or finding can carry an alias",
                    other.as_wire()
                )));
            }
        }
        s.aliases.insert(alias, resolved);
        Ok(())
    }
}

impl Ledger for MemStore {
    // `append_*` checks nothing (spec §3.4): the engine already resolved the
    // references it is recording.
    fn append_gate_run(&self, run: GateRun) -> Result<(), StoreError> {
        self.inner.borrow_mut().runs.push(run);
        Ok(())
    }

    fn append_attempt(&self, attempt: Attempt) -> Result<(), StoreError> {
        self.inner.borrow_mut().attempts.push(attempt);
        Ok(())
    }

    fn gate_runs(&self, gate: &GateId) -> Result<Vec<GateRun>, StoreError> {
        let s = self.inner.borrow();
        s.check(&gate.0)?;
        Ok(s.runs.iter().filter(|r| r.gate == *gate).cloned().collect())
    }

    fn attempts(&self, project: &ProjectId) -> Result<Vec<Attempt>, StoreError> {
        let s = self.inner.borrow();
        s.check_kind(&project.0, Kind::Project)?;
        Ok(s.attempts
            .iter()
            .filter(|a| a.project == *project)
            .cloned()
            .collect())
    }
}

impl Outbox for MemStore {
    fn unpublished(&self, _repo: &str, after: &Iri) -> Result<Pending, StoreError> {
        let s = self.inner.borrow();
        let waiting = |id: &Option<Iri>| {
            id.as_ref()
                .is_some_and(|id| id > after && !s.settled.contains(id))
        };
        let mut runs: Vec<GateRun> = s
            .runs
            .iter()
            .filter(|r| r.record.is_some() && waiting(&r.id))
            .cloned()
            .collect();
        let mut attempts: Vec<Attempt> = s
            .attempts
            .iter()
            .filter(|a| waiting(&a.id))
            .cloned()
            .collect();
        runs.sort_by(|a, b| a.id.cmp(&b.id));
        attempts.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(Pending { runs, attempts })
    }

    fn is_published(&self, repo: &str, id: &Iri) -> Result<bool, StoreError> {
        Ok(self
            .inner
            .borrow()
            .published
            .contains(&(repo.to_string(), id.clone())))
    }

    fn mark_published(&self, repo: &str, ids: &[Iri]) -> Result<(), StoreError> {
        self.settle(repo, ids, &[])
    }

    fn settle(&self, repo: &str, published: &[Iri], set_aside: &[Iri]) -> Result<(), StoreError> {
        let mut s = self.inner.borrow_mut();
        for id in published {
            s.published.insert((repo.to_string(), id.clone()));
        }
        s.settled.extend(published.iter().chain(set_aside).cloned());
        Ok(())
    }

    fn cutover(&self, repo: &str) -> Result<Option<Iri>, StoreError> {
        Ok(self.inner.borrow().cutovers.get(repo).cloned())
    }

    fn set_cutover(&self, repo: &str, id: &Iri) -> Result<(), StoreError> {
        let mut s = self.inner.borrow_mut();
        match s.cutovers.get(repo) {
            Some(held) if held == id => Ok(()),
            Some(held) => Err(StoreError::CutoverChanged {
                node_id: repo.to_string(),
                held: held.clone(),
                found: id.clone(),
            }),
            None => {
                s.cutovers.insert(repo.to_string(), id.clone());
                Ok(())
            }
        }
    }
}

impl Handles for MemStore {
    fn handle_of(&self, kind: Kind, id: &Iri) -> Result<Option<u64>, StoreError> {
        let s = self.inner.borrow();
        Ok(match s.owned.get(id) {
            Some(k) if *k == kind => s.handle_of.get(id).copied(),
            _ => None,
        })
    }

    fn resolve_handle(&self, kind: Kind, handle: u64) -> Result<Option<Iri>, StoreError> {
        Ok(self.inner.borrow().handles.get(&(kind, handle)).cloned())
    }
}

impl crate::store::Bindings for MemStore {
    fn bound_node_id(&self, repo: &str) -> Result<Option<String>, StoreError> {
        Ok(self
            .inner
            .borrow()
            .bindings
            .get(&repo.to_ascii_lowercase())
            .cloned())
    }
    fn bind_node_id(&self, repo: &str, node_id: &str) -> Result<(), StoreError> {
        self.inner
            .borrow_mut()
            .bindings
            .insert(repo.to_ascii_lowercase(), node_id.to_string());
        Ok(())
    }
    fn ledger_root(&self, node_id: &str) -> Result<Option<String>, StoreError> {
        Ok(self.inner.borrow().ledger_roots.get(node_id).cloned())
    }
    fn set_ledger_root(&self, node_id: &str, commit: &str) -> Result<(), StoreError> {
        let mut s = self.inner.borrow_mut();
        match s.ledger_roots.get(node_id) {
            Some(held) if held == commit => Ok(()),
            Some(held) => Err(StoreError::LedgerRootChanged {
                node_id: node_id.to_string(),
                held: held.clone(),
                found: commit.to_string(),
            }),
            None => {
                s.ledger_roots
                    .insert(node_id.to_string(), commit.to_string());
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mem_store_meets_every_role_contract() {
        use crate::conformance::Single;
        crate::conformance::catalog(|| (MemStore::default(), ()));
        crate::conformance::tracker(|| Single(MemStore::default(), ()));
        crate::conformance::ledger(|| Single(MemStore::default(), ()));
        crate::conformance::all_roles(|| Single(MemStore::default(), ()));
        crate::conformance::local_handles(|| (MemStore::default(), ()));
    }

    #[test]
    fn a_split_ledger_over_a_mem_store_meets_the_ledger_contracts() {
        use crate::conformance::SplitOver;
        crate::conformance::ledger(|| SplitOver(MemStore::default(), ()));
        crate::conformance::split_ledger(|| SplitOver(MemStore::default(), ()));
    }

    /// A project with one gate and one record.
    fn outbox_world() -> (
        MemStore,
        crate::ids::GateId,
        crate::ids::RecordId,
        ProjectId,
    ) {
        let s = MemStore::default();
        let p = s.add_project("/p").unwrap();
        let kind = crate::model::GateKind::Command(crate::model::CommandSpec {
            program: "true".into(),
            args: vec![],
            delivery: crate::model::PopulationDelivery::Args,
            timeout_secs: 5,
            pass_codes: vec![0],
        });
        let sel = crate::model::Selector::Glob {
            pattern: "**/*".into(),
        };
        let g = s.add_gate(&p, "g", kind, sel, 1, "c", "o").unwrap();
        let r = s.add_record(&p, "t").unwrap();
        (s, g, r, p)
    }

    // Spec §1.3, §2.1, §3.2 step 6: only an entry with an id, tied to a
    // record, after the cut-over and not yet marked can be waiting; a mark
    // is per repository; the list is in id order.
    #[test]
    fn unpublished_lists_only_record_tied_entries_after_the_cut_over_with_no_mark() {
        use crate::conformance::{entry_iri, sample_attempt, sample_record_run};
        use crate::split::Outbox;
        let (s, g, r, p) = outbox_world();
        let before = sample_record_run(2, &g, Some(&r));
        let marked = sample_record_run(4, &g, Some(&r));
        let no_record = sample_record_run(5, &g, None);
        let mut no_id = sample_record_run(6, &g, Some(&r));
        no_id.id = None;
        let late = sample_record_run(8, &g, Some(&r));
        let waiting = sample_record_run(7, &g, Some(&r));
        for run in [&before, &marked, &no_record, &no_id, &late, &waiting] {
            s.append_gate_run(run.clone()).unwrap();
        }
        let attempt = sample_attempt(9, &p, &r);
        s.append_attempt(attempt.clone()).unwrap();
        s.mark_published("R_1", &[marked.id.clone().unwrap()])
            .unwrap();

        let pending = s.unpublished("R_1", &entry_iri(3)).unwrap();
        assert_eq!(
            pending.runs,
            vec![waiting.clone(), late.clone()],
            "id order"
        );
        assert_eq!(pending.attempts, vec![attempt]);
        assert_eq!(
            s.unpublished("R_2", &entry_iri(3)).unwrap().runs,
            vec![waiting, late],
            "the waiting set is the store's: an entry published anywhere waits for no repository"
        );
        assert!(
            !s.is_published("R_2", marked.id.as_ref().unwrap()).unwrap(),
            "a mark is per repository"
        );
    }

    // Spec §2.1: an entry set aside waits no more, and is not published.
    #[test]
    fn an_entry_set_aside_waits_no_more_and_is_not_published() {
        use crate::conformance::{entry_iri, sample_record_run};
        use crate::split::Outbox;
        let (s, g, r, _p) = outbox_world();
        let aside = sample_record_run(4, &g, Some(&r));
        s.append_gate_run(aside.clone()).unwrap();
        s.settle("R_1", &[], &[aside.id.clone().unwrap()]).unwrap();
        s.settle("R_1", &[], &[aside.id.clone().unwrap()]).unwrap();
        assert!(s.unpublished("R_1", &entry_iri(0)).unwrap().runs.is_empty());
        assert!(!s.is_published("R_1", aside.id.as_ref().unwrap()).unwrap());
    }

    // Spec §2.1: the cut-over is recorded once, when the GitHub ledger is
    // switched on; moving it would strand every entry in between.
    #[test]
    fn a_cut_over_is_recorded_once_per_repository() {
        use crate::conformance::entry_iri;
        use crate::split::Outbox;
        let s = MemStore::default();
        assert_eq!(s.cutover("R_1").unwrap(), None);
        s.set_cutover("R_1", &entry_iri(3)).unwrap();
        s.set_cutover("R_1", &entry_iri(3)).unwrap();
        let err = s.set_cutover("R_1", &entry_iri(4)).unwrap_err();
        assert!(
            matches!(err, StoreError::CutoverChanged { ref held, .. } if *held == entry_iri(3)),
            "{err:?}"
        );
        assert_eq!(s.cutover("R_1").unwrap(), Some(entry_iri(3)));
        assert_eq!(s.cutover("R_2").unwrap(), None);
    }
}
