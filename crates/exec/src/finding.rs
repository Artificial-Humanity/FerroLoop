use crate::evaluate::{GateReport, run_single_gate};
use crate::population::ExecError;
use fl_core::finding::FindingState;
use fl_core::ids::{FindingId, GateId};
use fl_core::iri::Iri;
use fl_core::model::Selector;
use fl_core::store::{Roles, StoreError, follow};
use fl_core::verdict::{FailReason, Verdict};

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
    #[error(
        "gate `{name}` examined nothing: its selector ({selector}) matched zero paths under \
         `{root}`. That is not a reproduction — a check that looked at nothing cannot tell you \
         whether the defect is present, and `empty_population` is refused as evidence for the \
         same reason a passing gate is. Point the selector at files that exist under this \
         project's root, then reproduce again: a reproduction must be observed failing over \
         something."
    )]
    ReproductionEmptyPopulation {
        name: String,
        selector: String,
        root: String,
    },
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

/// Follow a stored reference belonging to a finding, the same way
/// `evaluate_transition` follows a transition's gates: `NotOwned` is wrapped
/// so the message names the finding, since bare `NotOwned` only says where
/// nothing was found, not which stored reference sent us looking.
/// `Dangling` already names `label` (it is built from it), so it passes
/// through unchanged.
fn follow_ref<T>(
    label: String,
    to: &Iri,
    got: Result<Option<T>, StoreError>,
) -> Result<T, FindingExecError> {
    follow(&label, to, got).map_err(|e| match e {
        StoreError::NotOwned { .. } => FindingExecError::Store(format!("{label}: {e}")),
        other => FindingExecError::Store(other.to_string()),
    })
}

/// Render a selector the way a refusal message names it: readable, and
/// specific enough that the person refused can go fix the thing named.
fn describe_selector(selector: &Selector) -> String {
    match selector {
        Selector::Glob { pattern } => format!("glob `{pattern}`"),
        Selector::Changed { base } => format!("changed-since `{base}`"),
        Selector::Command { program, args } => {
            format!("command `{program} {}`", args.join(" "))
        }
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
    roles: Roles<'_>,
    finding: &FindingId,
    gate: &GateId,
) -> Result<GateReport, FindingExecError> {
    let mut f = roles
        .tracker
        .get_finding(finding)
        .map_err(|e| FindingExecError::Store(e.to_string()))?
        .ok_or_else(|| FindingExecError::NoSuchFinding(finding.clone()))?;
    let def = roles
        .catalog
        .get_gate(gate)
        .map_err(|e| FindingExecError::Store(e.to_string()))?
        .ok_or_else(|| FindingExecError::NoSuchGate(gate.clone()))?;

    let report = run_single_gate(roles.catalog, roles.ledger, &f.project, gate)?;
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
        // An empty-population fail examined nothing, so it is refused for
        // the same reason a Pass is: it is not evidence the defect is
        // present. This must be checked BEFORE the catch-all below, which
        // would otherwise accept it as a legitimate reproduction — and
        // because Verdict::Pass requires a non-zero population by
        // construction, a finding reproduced this way could never close
        // through any repair.
        Verdict::Fail {
            reason: FailReason::EmptyPopulation,
            ..
        } => {
            let root = roles
                .catalog
                .get_project(&f.project)
                .map_err(|e| FindingExecError::Store(e.to_string()))?
                .map(|p| p.root)
                .unwrap_or_default();
            return Err(FindingExecError::ReproductionEmptyPopulation {
                name: def.name,
                selector: describe_selector(&def.selector),
                root,
            });
        }
        Verdict::Fail { .. } => {}
    }

    f.attach_reproduction(gate.clone())?;
    roles
        .tracker
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
    roles: Roles<'_>,
    finding: &FindingId,
) -> Result<FixReport, FindingExecError> {
    let mut f = roles
        .tracker
        .get_finding(finding)
        .map_err(|e| FindingExecError::Store(e.to_string()))?
        .ok_or_else(|| FindingExecError::NoSuchFinding(finding.clone()))?;
    if f.state != FindingState::Assigned {
        return Err(FindingExecError::NotAssigned(
            finding.clone(),
            f.state.as_wire(),
        ));
    }

    // The finding's own stored references, followed explicitly so a
    // dangling or never-owned project or reproduction is named as such —
    // by the finding, not surfaced as whatever `run_single_gate` happens to
    // say about a bare id it was handed.
    follow_ref(
        format!("finding {finding}'s project"),
        f.project.iri(),
        roles.catalog.get_project(&f.project),
    )?;

    let gate = f
        .reproduction
        .clone()
        .ok_or_else(|| FindingExecError::NoReproduction(finding.clone()))?;

    follow_ref(
        format!("finding {finding}'s reproduction"),
        gate.iri(),
        roles.catalog.get_gate(&gate),
    )?;

    let reproduction = run_single_gate(roles.catalog, roles.ledger, &f.project, &gate)?;

    // The baseline is already on disk: a gate with a last_pass_commit passed
    // at some point, so a failure now is a regression rather than news.
    let neighbours: Vec<_> = roles
        .catalog
        .list_gates(&f.project)
        .map_err(|e| FindingExecError::Store(e.to_string()))?
        .into_iter()
        .filter(|g| g.id != gate && g.last_pass_commit.is_some())
        .map(|g| g.id)
        .collect();

    // Every qualifying neighbour is run here, exactly once, whether it ends
    // up passing or failing. `regressions` is derived from this same pass —
    // not a second one — by filtering out whatever did not pass.
    let mut neighbour_reports = Vec::new();
    for id in &neighbours {
        let r = run_single_gate(roles.catalog, roles.ledger, &f.project, id)?;
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
        roles
            .tracker
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
    use fl_core::MemStore;
    use fl_core::finding::{Finding, FindingState};
    use fl_core::ids::{ProjectId, RecordId};
    use fl_core::model::{CommandSpec, GateKind, PopulationDelivery, Selector};
    use fl_core::store::{Catalog, Roles, Tracker};
    use fl_core::verdict::FailReason;
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
        s: &MemStore,
        p: &ProjectId,
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
        let s = MemStore::default();
        let p = s.add_project(&d.path().display().to_string()).unwrap();
        let r = s.add_record(&p, "t").unwrap();
        let g = gate(&s, &p, d.path(), "already-green", "true");
        let f = s
            .add_finding(Finding::raise(p.clone(), r.clone(), "reviewer", "claim"))
            .unwrap();

        let err = attach_reproduction(Roles::single(&s), &f, &g).unwrap_err();
        assert!(
            matches!(err, FindingExecError::ReproductionPasses { .. }),
            "got {err}"
        );
        assert_eq!(
            s.get_finding(&f).unwrap().unwrap().state,
            FindingState::Raised
        );
    }

    // ⚠⚠ CRITICAL fix-wave finding 1: a gate whose selector matches zero
    // paths FAILS with `FailReason::EmptyPopulation`, not `Pass`. Before
    // this test existed, `attach_reproduction`'s catch-all `Verdict::Fail {
    // .. } => {}` accepted that as a reproduction — a check that examined
    // nothing, accepted as evidence the defect is present. Because
    // `Verdict::Pass` requires a non-zero population by construction, a
    // finding reproduced this way could never close through any repair:
    // `verify_finding` re-runs the same selector, which still matches
    // nothing, which still fails `EmptyPopulation`, forever.
    #[test]
    fn a_reproduction_whose_selector_matches_nothing_is_refused_and_leaves_the_finding_raised() {
        let d = repo();
        let s = MemStore::default();
        let p = s.add_project(&d.path().display().to_string()).unwrap();
        let r = s.add_record(&p, "t").unwrap();
        let head = crate::git::Git::head(d.path()).unwrap();
        let g = s
            .add_gate(
                &p,
                "empty",
                GateKind::Command(CommandSpec {
                    program: "true".into(),
                    args: vec![],
                    delivery: PopulationDelivery::Args,
                    timeout_secs: 10,
                    pass_codes: vec![0],
                }),
                Selector::Glob {
                    pattern: "nowhere/**/*.rs".into(),
                },
                1,
                &head,
                "tester",
            )
            .unwrap();
        let f = s
            .add_finding(Finding::raise(p.clone(), r.clone(), "reviewer", "claim"))
            .unwrap();

        let err = attach_reproduction(Roles::single(&s), &f, &g).unwrap_err();
        assert!(
            matches!(err, FindingExecError::ReproductionEmptyPopulation { .. }),
            "got {err}"
        );
        let msg = err.to_string();
        assert!(msg.contains("`empty`"), "must name the gate: {msg}");
        // ⚠ The refusal must speak the wire spelling. It used to say
        // `EmptyPopulation`, a Rust identifier the tool emits nowhere, which
        // points the reader at a value that does not exist.
        assert!(
            msg.contains(FailReason::EmptyPopulation.as_wire()),
            "must name the failure in its wire spelling: {msg}"
        );
        assert!(
            !msg.contains("EmptyPopulation"),
            "names a Rust identifier the tool never emits: {msg}"
        );
        assert!(
            msg.contains("nowhere/**/*.rs"),
            "must name the selector: {msg}"
        );
        assert!(
            msg.contains(&d.path().display().to_string()),
            "must name the root it matched zero paths under: {msg}"
        );
        assert_eq!(
            s.get_finding(&f).unwrap().unwrap().state,
            FindingState::Raised,
            "a reproduction that examined nothing must not move the finding"
        );
    }

    #[test]
    fn a_failing_gate_is_accepted_and_moves_the_finding_to_reproduced() {
        let d = repo();
        let s = MemStore::default();
        let p = s.add_project(&d.path().display().to_string()).unwrap();
        let r = s.add_record(&p, "t").unwrap();
        let g = gate(&s, &p, d.path(), "red", "false");
        let f = s
            .add_finding(Finding::raise(p.clone(), r.clone(), "reviewer", "claim"))
            .unwrap();

        let report = attach_reproduction(Roles::single(&s), &f, &g).unwrap();
        assert!(!report.verdict.is_pass());
        let back = s.get_finding(&f).unwrap().unwrap();
        assert_eq!(back.state, FindingState::Reproduced);
        assert_eq!(back.reproduction, Some(g));
    }

    #[test]
    fn a_broken_gate_is_refused_as_a_reproduction_too() {
        let d = repo();
        let s = MemStore::default();
        let p = s.add_project(&d.path().display().to_string()).unwrap();
        let r = s.add_record(&p, "t").unwrap();
        let g = gate(
            &s,
            &p,
            d.path(),
            "broken",
            "definitely-not-a-real-program-9f3x",
        );
        let f = s
            .add_finding(Finding::raise(p.clone(), r.clone(), "reviewer", "claim"))
            .unwrap();

        // An Error is not a failure. It proves nothing either way.
        let err = attach_reproduction(Roles::single(&s), &f, &g).unwrap_err();
        assert!(
            matches!(err, FindingExecError::ReproductionErrored { .. }),
            "got {err}"
        );
        assert_eq!(
            s.get_finding(&f).unwrap().unwrap().state,
            FindingState::Raised
        );
    }

    // ⚠⚠ REQUIRED TEST 7 (spec §10): a repair that breaks a neighbour does
    // not close the finding. Decision 24's defect, caught mechanically.
    #[test]
    fn a_regression_in_a_neighbour_keeps_the_finding_open_and_names_the_gate() {
        let d = repo();
        let s = MemStore::default();
        let p = s.add_project(&d.path().display().to_string()).unwrap();
        let r = s.add_record(&p, "t").unwrap();

        // The neighbour passes first, so it earns a last_pass_commit.
        let neighbour = gate(&s, &p, d.path(), "neighbour", "true");
        let rep = gate(&s, &p, d.path(), "reproduction", "false");
        let _ = crate::evaluate::run_single_gate(&s, &s, &p, &neighbour).unwrap();
        assert!(
            s.get_gate(&neighbour)
                .unwrap()
                .unwrap()
                .last_pass_commit
                .is_some()
        );

        let f = s
            .add_finding(Finding::raise(p.clone(), r.clone(), "reviewer", "claim"))
            .unwrap();
        attach_reproduction(Roles::single(&s), &f, &rep).unwrap();
        let mut fin = s.get_finding(&f).unwrap().unwrap();
        fin.assign("fixer").unwrap();
        s.update_finding(&fin).unwrap();

        // The "repair": the reproduction now passes, and the neighbour breaks.
        let mut rep_def = s.get_gate(&rep).unwrap().unwrap();
        rep_def.kind = GateKind::Command(CommandSpec {
            program: "true".into(),
            args: vec![],
            delivery: PopulationDelivery::Args,
            timeout_secs: 10,
            pass_codes: vec![0],
        });
        s.update_gate(&rep_def).unwrap();
        let mut n_def = s.get_gate(&neighbour).unwrap().unwrap();
        n_def.kind = GateKind::Command(CommandSpec {
            program: "false".into(),
            args: vec![],
            delivery: PopulationDelivery::Args,
            timeout_secs: 10,
            pass_codes: vec![0],
        });
        s.update_gate(&n_def).unwrap();

        let report = verify_finding(Roles::single(&s), &f).unwrap();
        assert!(
            report.reproduction.verdict.is_pass(),
            "the reproduction did pass"
        );
        assert!(!report.closed, "a regression must keep it open");
        assert_eq!(report.regressions.len(), 1);
        assert_eq!(report.regressions[0].name, "neighbour");
        assert_eq!(report.exit_code(), 1);
        assert_eq!(
            s.get_finding(&f).unwrap().unwrap().state,
            FindingState::Assigned
        );
    }

    #[test]
    fn a_clean_repair_closes_the_finding() {
        let d = repo();
        let s = MemStore::default();
        let p = s.add_project(&d.path().display().to_string()).unwrap();
        let r = s.add_record(&p, "t").unwrap();
        let neighbour = gate(&s, &p, d.path(), "neighbour", "true");
        let rep = gate(&s, &p, d.path(), "reproduction", "false");
        let _ = crate::evaluate::run_single_gate(&s, &s, &p, &neighbour).unwrap();

        let f = s
            .add_finding(Finding::raise(p.clone(), r.clone(), "reviewer", "claim"))
            .unwrap();
        attach_reproduction(Roles::single(&s), &f, &rep).unwrap();
        let mut fin = s.get_finding(&f).unwrap().unwrap();
        fin.assign("fixer").unwrap();
        s.update_finding(&fin).unwrap();

        let mut rep_def = s.get_gate(&rep).unwrap().unwrap();
        rep_def.kind = GateKind::Command(CommandSpec {
            program: "true".into(),
            args: vec![],
            delivery: PopulationDelivery::Args,
            timeout_secs: 10,
            pass_codes: vec![0],
        });
        s.update_gate(&rep_def).unwrap();

        let report = verify_finding(Roles::single(&s), &f).unwrap();
        assert!(report.closed);
        assert_eq!(report.exit_code(), 0);
        assert_eq!(
            s.get_finding(&f).unwrap().unwrap().state,
            FindingState::Fixed
        );
    }

    #[test]
    fn a_finding_that_was_never_assigned_cannot_be_verified() {
        let d = repo();
        let s = MemStore::default();
        let p = s.add_project(&d.path().display().to_string()).unwrap();
        let r = s.add_record(&p, "t").unwrap();
        let f = s
            .add_finding(Finding::raise(p.clone(), r.clone(), "reviewer", "claim"))
            .unwrap();
        assert!(verify_finding(Roles::single(&s), &f).is_err());
    }

    /// Brings a finding to `Assigned` through the normal flow, then hands
    /// back the fresh copy so a test can corrupt one field and persist it
    /// with `update_finding` — the only way to get a stored, cross-kind
    /// dangling reference onto an already-assigned finding, since neither
    /// `add_finding` nor `update_finding` checks that a reference resolves
    /// to the kind it claims (that is exactly what `Dangling` is for).
    fn assigned(s: &MemStore, p: &ProjectId, r: &RecordId, rep: &GateId) -> FindingId {
        let f = s
            .add_finding(Finding::raise(p.clone(), r.clone(), "reviewer", "claim"))
            .unwrap();
        attach_reproduction(Roles::single(s), &f, rep).unwrap();
        let mut fin = s.get_finding(&f).unwrap().unwrap();
        fin.assign("fixer").unwrap();
        s.update_finding(&fin).unwrap();
        f
    }

    // Fix round 1, item 3: `verify_finding`'s two `follow_ref` calls (for
    // `f.project` and `f.reproduction`) had no test — deleting either left
    // the whole suite green. Each is pinned here by corrupting the
    // finding's own stored reference, after it is legitimately `Assigned`,
    // to an id this store holds as a DIFFERENT kind (a record's id, reused
    // as a `ProjectId`/`GateId`) — never a stranger id, since that is
    // exactly what `Dangling` means, as opposed to `NotOwned`.
    #[test]
    fn verify_finding_refuses_a_dangling_project_reference() {
        let d = repo();
        let s = MemStore::default();
        let p = s.add_project(&d.path().display().to_string()).unwrap();
        let r = s.add_record(&p, "t").unwrap();
        let rep = gate(&s, &p, d.path(), "reproduction", "false");
        let f = assigned(&s, &p, &r, &rep);

        let mut corrupted = s.get_finding(&f).unwrap().unwrap();
        corrupted.project = ProjectId(r.0.clone());
        s.update_finding(&corrupted).unwrap();

        // `FixReport` (the `Ok` side) has no `Debug`, so `unwrap_err` cannot
        // be used here.
        let err = match verify_finding(Roles::single(&s), &f) {
            Err(e) => e,
            Ok(_) => panic!("a dangling project reference must be refused"),
        };
        assert!(err.to_string().contains("dangling"), "got {err}");
        assert!(err.to_string().contains(&f.to_string()), "got {err}");
    }

    #[test]
    fn verify_finding_refuses_a_dangling_reproduction_reference() {
        let d = repo();
        let s = MemStore::default();
        let p = s.add_project(&d.path().display().to_string()).unwrap();
        let r = s.add_record(&p, "t").unwrap();
        let rep = gate(&s, &p, d.path(), "reproduction", "false");
        let f = assigned(&s, &p, &r, &rep);

        let mut corrupted = s.get_finding(&f).unwrap().unwrap();
        corrupted.reproduction = Some(GateId(r.0.clone()));
        s.update_finding(&corrupted).unwrap();

        let err = match verify_finding(Roles::single(&s), &f) {
            Err(e) => e,
            Ok(_) => panic!("a dangling reproduction reference must be refused"),
        };
        assert!(err.to_string().contains("dangling"), "got {err}");
        assert!(err.to_string().contains(&f.to_string()), "got {err}");
    }
}
