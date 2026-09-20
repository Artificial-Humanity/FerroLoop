use crate::evaluate::{GateReport, run_single_gate};
use crate::population::ExecError;
use fl_core::finding::FindingState;
use fl_core::ids::{FindingId, GateId};
use fl_core::store::Store;
use fl_core::verdict::Verdict;

#[derive(Debug, thiserror::Error)]
pub enum FindingExecError {
    #[error("no finding with id {0}")]
    NoSuchFinding(FindingId),
    #[error("no gate with id {0}")]
    NoSuchGate(GateId),
    #[error(
        "gate `{name}` currently PASSES over {population} items, so it is not a reproduction. \
         A check that already passes cannot tell you whether the defect is absent or whether \
         the check simply does not exercise it. Write one that fails because of the defect."
    )]
    ReproductionPasses { name: String, population: u64 },
    #[error(
        "gate `{name}` ERRORED, so it is not a reproduction: {detail}. \
         A broken instrument proves nothing in either direction."
    )]
    ReproductionErrored { name: String, detail: String },
    #[error("finding {0} is in state {1}, and only an assigned finding can be verified")]
    NotAssigned(FindingId, &'static str),
    #[error("finding {0} has no reproduction")]
    NoReproduction(FindingId),
    #[error("{0}")]
    Exec(#[from] ExecError),
    #[error("{0}")]
    Finding(#[from] fl_core::finding::FindingError),
    #[error("{0}")]
    Store(String),
}

/// The report of a fix-completion check: the reproduction's own verdict,
/// every neighbour that was actually evaluated, whichever of those
/// regressed, and whether the finding closed.
///
/// ⚠⚠ `neighbours` carries **every** gate `verify_finding` ran to check for
/// regressions — passing and failing alike, stale or fresh — because that is
/// the complete set of gates this verification already executed. A caller
/// that wants to display something about a passing neighbour (its
/// staleness, for instance) reads it from here. It must never re-run a gate
/// to recover information this struct could have carried instead: that is
/// the exact defect this type was widened to close (Task 16b) — `finding
/// verify` was invoking every qualifying neighbour gate twice, once inside
/// `verify_finding` and once more at the CLI purely to re-derive what this
/// struct now already holds.
///
/// `regressions` is kept as a convenience view: exactly the members of
/// `neighbours` whose verdict did not pass. It is not additional data, and
/// it is not re-run to produce — `verify_finding` filters it out of the same
/// single pass over `neighbours`. It stays a first-class field (rather than
/// a method) because `closed`/`exit_code()`'s contract is pinned by a
/// reviewer's mutation testing and existing callers already read it as a
/// field.
pub struct FixReport {
    pub reproduction: GateReport,
    pub neighbours: Vec<GateReport>,
    pub regressions: Vec<GateReport>,
    pub closed: bool,
}

impl FixReport {
    /// The CLI contract: 0 once closed, otherwise the worst verdict among
    /// the reproduction and every regression, floored at 1 (a repair that
    /// is not done is never a clean pass even if nothing regressed yet).
    pub fn exit_code(&self) -> i32 {
        if self.closed {
            return 0;
        }
        let worst = std::iter::once(&self.reproduction)
            .chain(self.regressions.iter())
            .map(|g| g.verdict.exit_code())
            .max()
            .unwrap_or(1);
        worst.max(1)
    }
}

/// Attach a reproduction to a finding, by RUNNING it first.
///
/// ⚠⚠ The attachment is refused unless the gate is observed FAILING. This is
/// the sibling of the empty-population rule: a check that already passes is
/// not evidence, and accepting one would put back the artifact-that-looks-
/// like-evidence hole the whole protocol exists to close. An `Error` is
/// refused too — a broken instrument proves nothing in either direction.
pub fn attach_reproduction(
    store: &mut dyn Store,
    finding: FindingId,
    gate: GateId,
) -> Result<GateReport, FindingExecError> {
    let mut f = store
        .get_finding(finding)
        .map_err(|e| FindingExecError::Store(e.to_string()))?
        .ok_or(FindingExecError::NoSuchFinding(finding))?;
    let def = store
        .get_gate(gate)
        .map_err(|e| FindingExecError::Store(e.to_string()))?
        .ok_or(FindingExecError::NoSuchGate(gate))?;

    let report = run_single_gate(store, f.project, gate)?;
    match &report.verdict {
        Verdict::Pass { population, .. } => {
            return Err(FindingExecError::ReproductionPasses {
                name: def.name,
                population: population.get(),
            });
        }
        Verdict::Error { detail, .. } => {
            return Err(FindingExecError::ReproductionErrored {
                name: def.name,
                detail: detail.clone(),
            });
        }
        Verdict::Fail { .. } => {}
    }

    f.attach_reproduction(gate)?;
    store
        .update_finding(&f)
        .map_err(|e| FindingExecError::Store(e.to_string()))?;
    Ok(report)
}

/// The fix-completion check: the reproduction must now pass, and every gate
/// that passed before must still pass.
///
/// ⚠ The second set is what catches a repair that replaces one defect with a
/// different kind in the same location: `last_pass_commit` is stamped only
/// on a pass, so it is already the baseline of everything the project once
/// demonstrated working, with no new state needed to hold it.
///
/// ⚠ Never moves the finding on failure: it stays `Assigned` so the same
/// fixer keeps it, and the report names what is still wrong. There is no
/// "failed" state — only a repair that is not done yet.
pub fn verify_finding(
    store: &mut dyn Store,
    finding: FindingId,
) -> Result<FixReport, FindingExecError> {
    let mut f = store
        .get_finding(finding)
        .map_err(|e| FindingExecError::Store(e.to_string()))?
        .ok_or(FindingExecError::NoSuchFinding(finding))?;
    if f.state != FindingState::Assigned {
        return Err(FindingExecError::NotAssigned(finding, f.state.as_wire()));
    }
    let gate = f
        .reproduction
        .ok_or(FindingExecError::NoReproduction(finding))?;

    let reproduction = run_single_gate(store, f.project, gate)?;

    // The baseline is already on disk: a gate with a last_pass_commit passed
    // at some point, so a failure now is a regression rather than news.
    let neighbours: Vec<_> = store
        .list_gates(f.project)
        .map_err(|e| FindingExecError::Store(e.to_string()))?
        .into_iter()
        .filter(|g| g.id != gate && g.last_pass_commit.is_some())
        .map(|g| g.id)
        .collect();

    // Every qualifying neighbour is run here, exactly once, whether it ends
    // up passing or failing. `regressions` is derived from this same pass —
    // not a second one — by filtering out whatever did not pass.
    let mut neighbour_reports = Vec::new();
    for id in neighbours {
        let r = run_single_gate(store, f.project, id)?;
        neighbour_reports.push(r);
    }
    let regressions: Vec<GateReport> = neighbour_reports
        .iter()
        .filter(|r| !r.verdict.is_pass())
        .cloned()
        .collect();

    let closed = reproduction.verdict.is_pass() && regressions.is_empty();
    if closed {
        f.mark_fixed()?;
        store
            .update_finding(&f)
            .map_err(|e| FindingExecError::Store(e.to_string()))?;
    }

    Ok(FixReport {
        reproduction,
        neighbours: neighbour_reports,
        regressions,
        closed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::finding::{Finding, FindingState};
    use fl_core::ids::ProjectId;
    use fl_core::model::{CommandSpec, GateKind, PopulationDelivery, Selector};
    use fl_core::store::MemStore;
    use std::fs;
    use std::process::Command;

    /// A repo with one file, and a gate whose program is swapped by rewriting
    /// a marker file the command reads. `test -f` gives a predicate whose
    /// answer we control from the test.
    fn repo() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let run = |a: &[&str]| {
            Command::new("git")
                .args(a)
                .current_dir(d.path())
                .output()
                .unwrap();
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "t"]);
        fs::create_dir_all(d.path().join("src")).unwrap();
        fs::write(d.path().join("src/a.rs"), "fn a() {}").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-qm", "first"]);
        d
    }

    fn gate(
        s: &mut MemStore,
        p: ProjectId,
        root: &std::path::Path,
        name: &str,
        program: &str,
    ) -> GateId {
        let head = crate::git::Git::head(root).unwrap();
        s.add_gate(
            p,
            name,
            GateKind::Command(CommandSpec {
                program: program.into(),
                args: vec![],
                delivery: PopulationDelivery::Args,
                timeout_secs: 10,
                pass_codes: vec![0],
            }),
            Selector::Glob {
                pattern: "src/**/*.rs".into(),
            },
            1,
            &head,
            "tester",
        )
        .unwrap()
    }

    // ⚠⚠ REQUIRED TEST 5 (spec §10): a passing gate is not a reproduction.
    #[test]
    fn a_gate_that_currently_passes_is_refused_as_a_reproduction() {
        let d = repo();
        let mut s = MemStore::default();
        let p = s.add_project(&d.path().display().to_string()).unwrap();
        let r = s.add_record(p, "t").unwrap();
        let g = gate(&mut s, p, d.path(), "already-green", "true");
        let f = s
            .add_finding(Finding::raise(p, r, "reviewer", "claim"))
            .unwrap();

        let err = attach_reproduction(&mut s, f, g).unwrap_err();
        assert!(
            matches!(err, FindingExecError::ReproductionPasses { .. }),
            "got {err}"
        );
        assert_eq!(
            s.get_finding(f).unwrap().unwrap().state,
            FindingState::Raised
        );
    }

    #[test]
    fn a_failing_gate_is_accepted_and_moves_the_finding_to_reproduced() {
        let d = repo();
        let mut s = MemStore::default();
        let p = s.add_project(&d.path().display().to_string()).unwrap();
        let r = s.add_record(p, "t").unwrap();
        let g = gate(&mut s, p, d.path(), "red", "false");
        let f = s
            .add_finding(Finding::raise(p, r, "reviewer", "claim"))
            .unwrap();

        let report = attach_reproduction(&mut s, f, g).unwrap();
        assert!(!report.verdict.is_pass());
        let back = s.get_finding(f).unwrap().unwrap();
        assert_eq!(back.state, FindingState::Reproduced);
        assert_eq!(back.reproduction, Some(g));
    }

    #[test]
    fn a_broken_gate_is_refused_as_a_reproduction_too() {
        let d = repo();
        let mut s = MemStore::default();
        let p = s.add_project(&d.path().display().to_string()).unwrap();
        let r = s.add_record(p, "t").unwrap();
        let g = gate(
            &mut s,
            p,
            d.path(),
            "broken",
            "definitely-not-a-real-program-9f3x",
        );
        let f = s
            .add_finding(Finding::raise(p, r, "reviewer", "claim"))
            .unwrap();

        // An Error is not a failure. It proves nothing either way.
        let err = attach_reproduction(&mut s, f, g).unwrap_err();
        assert!(
            matches!(err, FindingExecError::ReproductionErrored { .. }),
            "got {err}"
        );
        assert_eq!(
            s.get_finding(f).unwrap().unwrap().state,
            FindingState::Raised
        );
    }

    // ⚠⚠ REQUIRED TEST 7 (spec §10): a repair that breaks a neighbour does
    // not close the finding. Decision 24's defect, caught mechanically.
    #[test]
    fn a_regression_in_a_neighbour_keeps_the_finding_open_and_names_the_gate() {
        let d = repo();
        let mut s = MemStore::default();
        let p = s.add_project(&d.path().display().to_string()).unwrap();
        let r = s.add_record(p, "t").unwrap();

        // The neighbour passes first, so it earns a last_pass_commit.
        let neighbour = gate(&mut s, p, d.path(), "neighbour", "true");
        let rep = gate(&mut s, p, d.path(), "reproduction", "false");
        let _ = crate::evaluate::run_single_gate(&mut s, p, neighbour).unwrap();
        assert!(
            s.get_gate(neighbour)
                .unwrap()
                .unwrap()
                .last_pass_commit
                .is_some()
        );

        let f = s
            .add_finding(Finding::raise(p, r, "reviewer", "claim"))
            .unwrap();
        attach_reproduction(&mut s, f, rep).unwrap();
        let mut fin = s.get_finding(f).unwrap().unwrap();
        fin.assign("fixer").unwrap();
        s.update_finding(&fin).unwrap();

        // The "repair": the reproduction now passes, and the neighbour breaks.
        let mut rep_def = s.get_gate(rep).unwrap().unwrap();
        rep_def.kind = GateKind::Command(CommandSpec {
            program: "true".into(),
            args: vec![],
            delivery: PopulationDelivery::Args,
            timeout_secs: 10,
            pass_codes: vec![0],
        });
        s.update_gate(&rep_def).unwrap();
        let mut n_def = s.get_gate(neighbour).unwrap().unwrap();
        n_def.kind = GateKind::Command(CommandSpec {
            program: "false".into(),
            args: vec![],
            delivery: PopulationDelivery::Args,
            timeout_secs: 10,
            pass_codes: vec![0],
        });
        s.update_gate(&n_def).unwrap();

        let report = verify_finding(&mut s, f).unwrap();
        assert!(
            report.reproduction.verdict.is_pass(),
            "the reproduction did pass"
        );
        assert!(!report.closed, "a regression must keep it open");
        assert_eq!(report.regressions.len(), 1);
        assert_eq!(report.regressions[0].name, "neighbour");
        assert_eq!(report.exit_code(), 1);
        assert_eq!(
            s.get_finding(f).unwrap().unwrap().state,
            FindingState::Assigned
        );
    }

    #[test]
    fn a_clean_repair_closes_the_finding() {
        let d = repo();
        let mut s = MemStore::default();
        let p = s.add_project(&d.path().display().to_string()).unwrap();
        let r = s.add_record(p, "t").unwrap();
        let neighbour = gate(&mut s, p, d.path(), "neighbour", "true");
        let rep = gate(&mut s, p, d.path(), "reproduction", "false");
        let _ = crate::evaluate::run_single_gate(&mut s, p, neighbour).unwrap();

        let f = s
            .add_finding(Finding::raise(p, r, "reviewer", "claim"))
            .unwrap();
        attach_reproduction(&mut s, f, rep).unwrap();
        let mut fin = s.get_finding(f).unwrap().unwrap();
        fin.assign("fixer").unwrap();
        s.update_finding(&fin).unwrap();

        let mut rep_def = s.get_gate(rep).unwrap().unwrap();
        rep_def.kind = GateKind::Command(CommandSpec {
            program: "true".into(),
            args: vec![],
            delivery: PopulationDelivery::Args,
            timeout_secs: 10,
            pass_codes: vec![0],
        });
        s.update_gate(&rep_def).unwrap();

        let report = verify_finding(&mut s, f).unwrap();
        assert!(report.closed);
        assert_eq!(report.exit_code(), 0);
        assert_eq!(
            s.get_finding(f).unwrap().unwrap().state,
            FindingState::Fixed
        );
    }

    #[test]
    fn a_finding_that_was_never_assigned_cannot_be_verified() {
        let d = repo();
        let mut s = MemStore::default();
        let p = s.add_project(&d.path().display().to_string()).unwrap();
        let r = s.add_record(p, "t").unwrap();
        let f = s
            .add_finding(Finding::raise(p, r, "reviewer", "claim"))
            .unwrap();
        assert!(verify_finding(&mut s, f).is_err());
    }
}
