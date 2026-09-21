use crate::finding::{Finding, FindingState};
use crate::ids::{FindingId, GateId, ProjectId, RecordId};
use crate::log::{Attempt, GateRun};
use crate::model::{GateDef, GateKind, Project, Record, Selector, State, Transition};
use std::collections::BTreeMap;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("no such gate: {0}")]
    NoSuchGate(GateId),
    #[error("no such record: {0}")]
    NoSuchRecord(RecordId),
    #[error("no such finding: {0}")]
    NoSuchFinding(FindingId),
    #[error("backend failure: {0}")]
    Backend(String),
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
}

/// Everything the engine needs from persistence. `fl-core` is generic over
/// this and names no engine.
///
/// ⚠ There is no method that stores a population, and adding one would break
/// the design. A population is enumerated fresh from the working tree at run
/// time so it cannot go stale in the store.
pub trait Store {
    fn add_project(&mut self, root: &str) -> Result<ProjectId, StoreError>;
    fn get_project(&self, id: ProjectId) -> Result<Option<Project>, StoreError>;
    fn list_projects(&self) -> Result<Vec<Project>, StoreError>;

    #[allow(clippy::too_many_arguments)]
    fn add_gate(
        &mut self,
        project: ProjectId,
        name: &str,
        kind: GateKind,
        selector: Selector,
        min_population: u64,
        authored_at_commit: &str,
        authored_by: &str,
    ) -> Result<GateId, StoreError>;
    fn get_gate(&self, id: GateId) -> Result<Option<GateDef>, StoreError>;
    fn list_gates(&self, project: ProjectId) -> Result<Vec<GateDef>, StoreError>;
    fn update_gate(&mut self, def: &GateDef) -> Result<(), StoreError>;

    /// Stores a transition, keyed by `(project, name)`. Overwrites any existing transition with the same key (upsert semantics).
    fn add_transition(&mut self, t: Transition) -> Result<(), StoreError>;
    fn get_transition(
        &self,
        project: ProjectId,
        name: &str,
    ) -> Result<Option<Transition>, StoreError>;
    /// Every transition a project declares.
    ///
    /// Needed because a transition is addressed by NAME, but a record move is
    /// addressed by the (from, to) pair it performs — so the move has to ask
    /// which declarations cover it.
    fn list_transitions(&self, project: ProjectId) -> Result<Vec<Transition>, StoreError>;

    fn add_record(&mut self, project: ProjectId, title: &str) -> Result<RecordId, StoreError>;
    fn get_record(&self, id: RecordId) -> Result<Option<Record>, StoreError>;
    fn list_records(&self, project: ProjectId) -> Result<Vec<Record>, StoreError>;
    fn set_record_state(&mut self, id: RecordId, state: State) -> Result<(), StoreError>;

    fn append_gate_run(&mut self, run: GateRun) -> Result<(), StoreError>;
    fn append_attempt(&mut self, attempt: Attempt) -> Result<(), StoreError>;
    fn gate_runs(&self, gate: GateId) -> Result<Vec<GateRun>, StoreError>;
    fn attempts(&self, project: ProjectId) -> Result<Vec<Attempt>, StoreError>;

    fn add_finding(&mut self, finding: Finding) -> Result<FindingId, StoreError>;
    fn get_finding(&self, id: FindingId) -> Result<Option<Finding>, StoreError>;
    fn update_finding(&mut self, finding: &Finding) -> Result<(), StoreError>;
    fn list_findings(&self, project: ProjectId) -> Result<Vec<Finding>, StoreError>;

    /// How many findings this actor raised and then withdrew.
    ///
    /// ⚠ Decision 27 puts a cost on a claim the reviewer cannot support. A
    /// cost nobody can read is not a cost, so this is part of the trait and
    /// not a report bolted on later.
    fn withdrawals_by(&self, actor: &str) -> Result<u64, StoreError>;
}

/// An in-memory `Store` for tests. Deliberately lives in `fl-core` so the
/// engine's own tests need no backend at all.
#[derive(Default)]
pub struct MemStore {
    next_id: u64,
    projects: BTreeMap<u64, Project>,
    gates: BTreeMap<u64, GateDef>,
    transitions: BTreeMap<(u64, String), Transition>,
    records: BTreeMap<u64, Record>,
    runs: Vec<GateRun>,
    attempts: Vec<Attempt>,
    findings: BTreeMap<u64, Finding>,
}

impl MemStore {
    fn next(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }
}

impl Store for MemStore {
    fn add_project(&mut self, root: &str) -> Result<ProjectId, StoreError> {
        let id = ProjectId(self.next());
        self.projects.insert(
            id.0,
            Project {
                id,
                root: root.to_string(),
            },
        );
        Ok(id)
    }

    fn get_project(&self, id: ProjectId) -> Result<Option<Project>, StoreError> {
        Ok(self.projects.get(&id.0).cloned())
    }

    fn list_projects(&self) -> Result<Vec<Project>, StoreError> {
        Ok(self.projects.values().cloned().collect())
    }

    fn add_gate(
        &mut self,
        project: ProjectId,
        name: &str,
        kind: GateKind,
        selector: Selector,
        min_population: u64,
        authored_at_commit: &str,
        authored_by: &str,
    ) -> Result<GateId, StoreError> {
        let id = GateId(self.next());
        self.gates.insert(
            id.0,
            GateDef {
                id,
                project,
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

    fn get_gate(&self, id: GateId) -> Result<Option<GateDef>, StoreError> {
        Ok(self.gates.get(&id.0).cloned())
    }

    fn list_gates(&self, project: ProjectId) -> Result<Vec<GateDef>, StoreError> {
        Ok(self
            .gates
            .values()
            .filter(|g| g.project == project)
            .cloned()
            .collect())
    }

    fn update_gate(&mut self, def: &GateDef) -> Result<(), StoreError> {
        if !self.gates.contains_key(&def.id.0) {
            return Err(StoreError::NoSuchGate(def.id));
        }
        self.gates.insert(def.id.0, def.clone());
        Ok(())
    }

    fn add_transition(&mut self, t: Transition) -> Result<(), StoreError> {
        self.transitions.insert((t.project.0, t.name.clone()), t);
        Ok(())
    }

    fn get_transition(
        &self,
        project: ProjectId,
        name: &str,
    ) -> Result<Option<Transition>, StoreError> {
        Ok(self
            .transitions
            .get(&(project.0, name.to_string()))
            .cloned())
    }

    fn list_transitions(&self, project: ProjectId) -> Result<Vec<Transition>, StoreError> {
        Ok(self
            .transitions
            .values()
            .filter(|t| t.project == project)
            .cloned()
            .collect())
    }

    fn add_record(&mut self, project: ProjectId, title: &str) -> Result<RecordId, StoreError> {
        let id = RecordId(self.next());
        self.records.insert(
            id.0,
            Record {
                id,
                project,
                title: title.to_string(),
                state: State::Todo,
            },
        );
        Ok(id)
    }

    fn get_record(&self, id: RecordId) -> Result<Option<Record>, StoreError> {
        Ok(self.records.get(&id.0).cloned())
    }

    fn list_records(&self, project: ProjectId) -> Result<Vec<Record>, StoreError> {
        Ok(self
            .records
            .values()
            .filter(|r| r.project == project)
            .cloned()
            .collect())
    }

    fn set_record_state(&mut self, id: RecordId, state: State) -> Result<(), StoreError> {
        let rec = self
            .records
            .get_mut(&id.0)
            .ok_or(StoreError::NoSuchRecord(id))?;
        rec.state = state;
        Ok(())
    }

    fn append_gate_run(&mut self, run: GateRun) -> Result<(), StoreError> {
        self.runs.push(run);
        Ok(())
    }

    fn append_attempt(&mut self, attempt: Attempt) -> Result<(), StoreError> {
        self.attempts.push(attempt);
        Ok(())
    }

    fn gate_runs(&self, gate: GateId) -> Result<Vec<GateRun>, StoreError> {
        Ok(self
            .runs
            .iter()
            .filter(|r| r.gate == gate)
            .cloned()
            .collect())
    }

    fn attempts(&self, project: ProjectId) -> Result<Vec<Attempt>, StoreError> {
        Ok(self
            .attempts
            .iter()
            .filter(|a| a.project == project)
            .cloned()
            .collect())
    }

    fn add_finding(&mut self, finding: Finding) -> Result<FindingId, StoreError> {
        let id = FindingId(self.next());
        let mut finding = finding;
        finding.id = id;
        self.findings.insert(id.0, finding);
        Ok(id)
    }

    fn get_finding(&self, id: FindingId) -> Result<Option<Finding>, StoreError> {
        Ok(self.findings.get(&id.0).cloned())
    }

    fn update_finding(&mut self, finding: &Finding) -> Result<(), StoreError> {
        if !self.findings.contains_key(&finding.id.0) {
            return Err(StoreError::NoSuchFinding(finding.id));
        }
        self.findings.insert(finding.id.0, finding.clone());
        Ok(())
    }

    fn list_findings(&self, project: ProjectId) -> Result<Vec<Finding>, StoreError> {
        Ok(self
            .findings
            .values()
            .filter(|f| f.project == project)
            .cloned()
            .collect())
    }

    fn withdrawals_by(&self, actor: &str) -> Result<u64, StoreError> {
        Ok(self
            .findings
            .values()
            .filter(|f| f.raised_by == actor && f.state == FindingState::Withdrawn)
            .count() as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CommandSpec, PopulationDelivery};
    use crate::verdict::Verdict;

    fn project(store: &mut impl Store) -> ProjectId {
        store.add_project("/tmp/p").unwrap()
    }

    #[test]
    fn a_project_round_trips() {
        let mut s = MemStore::default();
        let id = project(&mut s);
        assert_eq!(s.get_project(id).unwrap().unwrap().root, "/tmp/p");
        assert_eq!(s.list_projects().unwrap().len(), 1);
    }

    #[test]
    fn ids_are_handed_out_in_sequence_and_never_reused() {
        let mut s = MemStore::default();
        let a = s.add_project("/a").unwrap();
        let b = s.add_project("/b").unwrap();
        assert_ne!(a, b);
        assert_eq!(b.get(), a.get() + 1);
    }

    #[test]
    fn a_missing_project_is_none_and_not_an_error() {
        let s = MemStore::default();
        assert!(s.get_project(ProjectId(42)).unwrap().is_none());
    }

    #[test]
    fn list_records_returns_only_the_named_projects_records() {
        let mut s = MemStore::default();
        let p1 = project(&mut s);
        let p2 = s.add_project("/tmp/q").unwrap();
        let r1 = s.add_record(p1, "fix the thing").unwrap();
        let _r2 = s.add_record(p2, "unrelated").unwrap();
        let recs = s.list_records(p1).unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].id, r1);
        assert_eq!(recs[0].title, "fix the thing");
    }

    #[test]
    fn a_record_state_change_is_visible_on_the_next_read() {
        let mut s = MemStore::default();
        let p = project(&mut s);
        let r = s.add_record(p, "fix the thing").unwrap();
        s.set_record_state(r, State::Doing).unwrap();
        assert_eq!(s.get_record(r).unwrap().unwrap().state, State::Doing);
    }

    #[test]
    fn the_logs_are_append_only_and_read_back_in_order() {
        let mut s = MemStore::default();
        let p = project(&mut s);
        let g = s
            .add_gate(
                p,
                "fmt",
                sample_kind(),
                sample_selector(),
                1,
                "abc",
                "owner",
            )
            .unwrap();
        s.append_gate_run(sample_run(g, "abc", 3)).unwrap();
        s.append_gate_run(sample_run(g, "def", 5)).unwrap();
        let runs = s.gate_runs(g).unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].commit, "abc");
        assert_eq!(runs[1].population, 5);
    }

    #[test]
    fn affirming_a_gate_moves_only_its_stamp() {
        let mut s = MemStore::default();
        let p = project(&mut s);
        let g = s
            .add_gate(
                p,
                "fmt",
                sample_kind(),
                sample_selector(),
                1,
                "abc",
                "owner",
            )
            .unwrap();
        let mut def = s.get_gate(g).unwrap().unwrap();
        def.authored_at_commit = "def".into();
        s.update_gate(&def).unwrap();
        let back = s.get_gate(g).unwrap().unwrap();
        assert_eq!(back.authored_at_commit, "def");
        assert_eq!(back.name, "fmt");
    }

    #[test]
    fn a_finding_round_trips_and_gets_a_real_id() {
        let mut s = MemStore::default();
        let p = s.add_project("/p").unwrap();
        let r = s.add_record(p, "t").unwrap();
        let id = s
            .add_finding(Finding::raise(p, r, "reviewer", "wrong on empty"))
            .unwrap();
        assert_ne!(id.get(), 0, "the store must replace the placeholder id");
        let back = s.get_finding(id).unwrap().unwrap();
        assert_eq!(back.id, id);
        assert_eq!(back.state, FindingState::Raised);
    }

    #[test]
    fn withdrawals_are_counted_against_whoever_raised_the_finding() {
        let mut s = MemStore::default();
        let p = s.add_project("/p").unwrap();
        let r = s.add_record(p, "t").unwrap();

        for claim in ["a", "b"] {
            let id = s.add_finding(Finding::raise(p, r, "hasty", claim)).unwrap();
            let mut f = s.get_finding(id).unwrap().unwrap();
            f.withdraw("not concrete").unwrap();
            s.update_finding(&f).unwrap();
        }
        let id = s.add_finding(Finding::raise(p, r, "careful", "c")).unwrap();
        let mut f = s.get_finding(id).unwrap().unwrap();
        f.attach_reproduction(GateId(1)).unwrap();
        s.update_finding(&f).unwrap();

        assert_eq!(s.withdrawals_by("hasty").unwrap(), 2);
        assert_eq!(s.withdrawals_by("careful").unwrap(), 0);
        assert_eq!(s.withdrawals_by("nobody").unwrap(), 0);
    }

    #[test]
    fn findings_are_listed_per_project() {
        let mut s = MemStore::default();
        let a = s.add_project("/a").unwrap();
        let b = s.add_project("/b").unwrap();
        let ra = s.add_record(a, "t").unwrap();
        let rb = s.add_record(b, "t").unwrap();
        s.add_finding(Finding::raise(a, ra, "r", "one")).unwrap();
        s.add_finding(Finding::raise(b, rb, "r", "two")).unwrap();
        assert_eq!(s.list_findings(a).unwrap().len(), 1);
    }

    fn sample_kind() -> GateKind {
        GateKind::Command(CommandSpec {
            program: "true".into(),
            args: vec![],
            delivery: PopulationDelivery::Args,
            timeout_secs: 5,
            pass_codes: vec![0],
        })
    }

    fn sample_selector() -> Selector {
        Selector::Glob {
            pattern: "**/*.rs".into(),
        }
    }

    fn sample_run(gate: GateId, commit: &str, population: u64) -> GateRun {
        GateRun {
            gate,
            record: None,
            commit: commit.into(),
            verdict: Verdict::from_predicate(true, population),
            population,
            output_excerpt: String::new(),
            duration_ms: 1,
            cost_usd_micros: 0,
        }
    }
}
