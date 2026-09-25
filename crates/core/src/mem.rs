use crate::finding::{Finding, FindingState};
use crate::ids::{FindingId, GateId, Kind, ProjectId, RecordId, seq_iri};
use crate::iri::Iri;
use crate::log::{Attempt, GateRun};
use crate::model::{GateDef, GateKind, Project, Record, Selector, State, Transition};
use crate::store::{Catalog, Handles, Ledger, StoreError, Tracker};
use std::cell::RefCell;
use std::collections::BTreeMap;

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
        s.check(&project.0)?;
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
        s.check(&project.0)?;
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
        s.check(&t.project.0)?;
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
        s.check(&project.0)?;
        Ok(s.transitions
            .get(&(project.0.clone(), name.to_string()))
            .cloned())
    }

    fn list_transitions(&self, project: &ProjectId) -> Result<Vec<Transition>, StoreError> {
        let s = self.inner.borrow();
        s.check(&project.0)?;
        Ok(s.transitions
            .values()
            .filter(|t| t.project == *project)
            .cloned()
            .collect())
    }
}

impl Tracker for MemStore {
    fn add_record(&self, project: &ProjectId, title: &str) -> Result<RecordId, StoreError> {
        let mut s = self.inner.borrow_mut();
        s.check(&project.0)?;
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
        s.check(&project.0)?;
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
        s.check(&finding.project.0)?;
        s.check(&finding.record.0)?;
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
        let mut stored = finding.clone();
        stored.id = FindingId(target.clone());
        s.findings.insert(target, stored);
        Ok(())
    }

    fn list_findings(&self, project: &ProjectId) -> Result<Vec<Finding>, StoreError> {
        let s = self.inner.borrow();
        s.check(&project.0)?;
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
        s.check(&project.0)?;
        Ok(s.attempts
            .iter()
            .filter(|a| a.project == *project)
            .cloned()
            .collect())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mem_store_meets_every_role_contract() {
        crate::conformance::catalog(|| (MemStore::default(), ()));
        crate::conformance::tracker(|| (MemStore::default(), ()));
        crate::conformance::ledger(|| (MemStore::default(), ()));
        crate::conformance::all_roles(|| (MemStore::default(), ()));
    }
}
