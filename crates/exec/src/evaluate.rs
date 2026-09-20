use crate::command::run_command_gate;
use crate::git::Git;
use crate::population::{ExecError, resolve};
use fl_core::ids::{GateId, ProjectId, RecordId};
use fl_core::log::GateRun;
use fl_core::model::{GateDef, GateKind, Regret, Selector, Transition};
use fl_core::stale::{Staleness, apply_staleness, is_stale};
use fl_core::store::Store;
use fl_core::verdict::Verdict;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct GateReport {
    pub gate: GateId,
    pub name: String,
    pub verdict: Verdict,
    pub staleness: Staleness,
    pub output_excerpt: String,
    pub duration_ms: u64,
}

#[derive(Debug)]
pub struct TransitionReport {
    pub transition: String,
    pub regret: Regret,
    pub gates: Vec<GateReport>,
}

impl TransitionReport {
    /// ⚠⚠ A transition with no gates has not been verified. This is the
    /// empty-population rule one level up: a no-run is not a pass, so an
    /// empty `gates` list must never read as success. Do not "fix" this to
    /// return `true`.
    pub fn passed(&self) -> bool {
        !self.gates.is_empty() && self.gates.iter().all(|g| g.verdict.is_pass())
    }

    /// The worst verdict wins: any error is 2, any failure is 1.
    pub fn exit_code(&self) -> i32 {
        self.gates.iter().map(|g| g.verdict.exit_code()).max().unwrap_or(1)
    }
}

/// Decide whether a gate is stale, by asking whether anything it covers moved
/// between its stamp and `HEAD`.
///
/// ⚠⚠ **Do NOT implement this as an intersection against the live population.**
/// Measured 2026-09-20 in a scratch repository: `git diff --name-only` lists a
/// **deleted** path, and lists a **rename only under its new name**. A deleted
/// file can never appear in a population resolved from the working tree, so
/// `population.iter().any(|p| changed.contains(p))` silently drops exactly the
/// change most likely to invalidate a gate — and returns the same answer as
/// "nothing relevant changed". That is an empty answer masquerading as a real
/// one, inside the check whose whole job is noticing when a gate stopped
/// meaning anything.
///
/// So: test the gate's **selector** against the changed paths directly.
///
/// `population` is the result of resolving `def.selector` once, handed in by
/// the caller rather than recomputed here: for a `Selector::Command`, a
/// second `resolve()` call would run the user's own command a second time
/// per gate per evaluation, which is both a cost and an idempotency hazard
/// this function has no business creating.
fn staleness_for(
    root: &Path,
    def: &GateDef,
    head: &str,
    population: &Result<Vec<PathBuf>, ExecError>,
) -> bool {
    if def.authored_at_commit == head {
        return false;
    }
    let Ok(changed) = Git::changed_between(root, &def.authored_at_commit, head) else {
        // A stamp we cannot resolve is treated as stale. An unreadable
        // provenance is not evidence of freshness.
        return true;
    };
    if changed.is_empty() {
        return false;
    }

    let touched = match &def.selector {
        Selector::Glob { pattern } => {
            // Requires `population::matcher` to become `pub(crate)`.
            let Ok(m) = crate::population::matcher(pattern) else {
                // A gate whose own selector no longer compiles cannot vouch
                // for anything. Stale, not fresh.
                return true;
            };
            changed
                .iter()
                .any(|p| p.strip_prefix(root).map(|rel| m.is_match(rel)).unwrap_or(false))
        }
        // ⚠ `Changed` and `Command` define their population by RUNNING
        // something, so there is no pattern to test a vanished path against;
        // this falls back to intersecting the changed set with the already-
        // resolved population instead.
        //
        // Measured 2026-09-20 in scratch repositories, not reasoned about —
        // this comment has been wrong twice already on reasoning alone. For
        // `Selector::Changed`, the population is `diff(selector.base,
        // head)`. A plain, never-restored deletion shows up there the same
        // as it does in `changed`, so it is not missed. The one confirmed
        // miss: a file whose content is identical at `base` and `head` but
        // differed at `authored_at_commit` in between — e.g. changed, then
        // reverted after the gate's stamp. `diff(authored_at_commit, head)`
        // lists that file; `diff(base, head)` does not, so the intersection
        // never sees it (verified with raw `git diff --name-only` on both
        // pairs). A delete followed by an identical recreate, entirely
        // between `base` and `authored_at_commit`, is NOT an instance of
        // this: both diffs agreed the file was unchanged when this was
        // tested directly, so there was nothing there to miss. Beyond the
        // revert-after-stamp case, this fallback's boundary is not
        // characterised. For `Selector::Command`, the population is
        // whatever the command prints, so nothing can be promised about it
        // either way. Recorded rather than hidden: if this matters later,
        // these selector kinds need a targeted mechanism, not a cleverer
        // intersection.
        _ => {
            let Ok(population) = population.as_ref() else {
                return true;
            };
            population.iter().any(|p| changed.contains(p))
        }
    };

    is_stale(touched, false)
}

/// The shared body behind [`run_single_gate`] and [`evaluate_transition`]'s
/// per-gate loop: resolve the project root and the gate's selector against
/// the live tree (once), run the gate, apply staleness under `regret`,
/// append the [`GateRun`] tagged with `record`, and stamp `last_pass_commit`
/// on a pass.
///
/// Not `pub`: the two callers reach it through [`run_single_gate`] (which
/// fixes `regret` at [`Regret::Low`] and `record` at `None`, since a bare
/// gate run is not a transition) and `evaluate_transition` (which supplies
/// the transition's own regret and record). Extracted here so neither caller
/// keeps its own copy of this logic.
fn run_gate(
    store: &mut dyn Store,
    root: &Path,
    head: &str,
    def: &GateDef,
    regret: Regret,
    record: Option<RecordId>,
) -> Result<GateReport, ExecError> {
    // Resolved once. `staleness_for`'s fallback branch reuses this same
    // result instead of calling `resolve` again — a second call would run a
    // `Selector::Command` gate's own command a second time per evaluation.
    let population_result = resolve(root, &def.selector, &Git);

    let (raw, excerpt, duration_ms) = match &population_result {
        Ok(population) => match &def.kind {
            GateKind::Command(spec) => {
                let out = run_command_gate(root, spec, population, def.min_population);
                (out.verdict, out.output_excerpt, out.duration_ms)
            }
            GateKind::Agent(spec) => (
                Verdict::error(format!(
                    "agent gates are not implemented in milestone 1 \
                     (gate `{}` asks adapter `{}`)",
                    def.name, spec.adapter
                )),
                String::new(),
                0,
            ),
        },
        Err(e) => (Verdict::error(e.to_string()), e.to_string(), 0),
    };

    let stale = staleness_for(root, def, head, &population_result);
    let (verdict, staleness) = apply_staleness(raw, stale, regret);

    store
        .append_gate_run(GateRun {
            gate: def.id,
            record,
            commit: head.to_string(),
            verdict: verdict.clone(),
            population: verdict.population().unwrap_or(0),
            output_excerpt: excerpt.clone(),
            duration_ms,
            cost_usd_micros: 0,
        })
        .map_err(|e| ExecError::Git(e.to_string()))?;

    if verdict.is_pass() {
        let mut updated = def.clone();
        updated.last_pass_commit = Some(head.to_string());
        let _ = store.update_gate(&updated);
    }

    Ok(GateReport {
        gate: def.id,
        name: def.name.clone(),
        verdict,
        staleness,
        output_excerpt: excerpt,
        duration_ms,
    })
}

/// Run one gate against the live working tree, exactly once, and record the
/// result.
///
/// Applies staleness at [`Regret::Low`] — a bare gate run is not a
/// transition, so it warns and never fails for staleness alone — and tags
/// the appended [`GateRun`] with no record. This is what `attach_reproduction`
/// and `verify_finding` use to run a gate ad hoc, outside any transition.
pub fn run_single_gate(
    store: &mut dyn Store,
    project: ProjectId,
    gate: GateId,
) -> Result<GateReport, ExecError> {
    let proj = store
        .get_project(project)
        .map_err(|e| ExecError::Git(e.to_string()))?
        .ok_or_else(|| ExecError::BadSelector(format!("no project with id {project}")))?;
    let root = Path::new(&proj.root);

    let Some(def) = store.get_gate(gate).map_err(|e| ExecError::Git(e.to_string()))? else {
        return Err(ExecError::BadSelector(format!("no gate with id {gate}")));
    };

    let head = Git::head(root)?;
    run_gate(store, root, &head, &def, Regret::Low, None)
}

pub fn evaluate_transition(
    store: &mut dyn Store,
    project: ProjectId,
    transition_name: &str,
    record: Option<RecordId>,
) -> Result<TransitionReport, ExecError> {
    let proj = store
        .get_project(project)
        .map_err(|e| ExecError::Git(e.to_string()))?
        .ok_or_else(|| ExecError::BadSelector(format!("no project with id {project}")))?;
    let root = Path::new(&proj.root);

    let transition: Transition = store
        .get_transition(project, transition_name)
        .map_err(|e| ExecError::Git(e.to_string()))?
        .ok_or_else(|| {
            ExecError::BadSelector(format!(
                "project {project} declares no transition named `{transition_name}`. \
                 Add it with `flctl transition add`, or name one of the existing ones."
            ))
        })?;

    let head = Git::head(root)?;
    let mut reports = Vec::new();

    for gate_id in &transition.gates {
        let Some(def) = store.get_gate(*gate_id).map_err(|e| ExecError::Git(e.to_string()))?
        else {
            return Err(ExecError::BadSelector(format!(
                "transition `{transition_name}` names gate {gate_id}, which does not exist"
            )));
        };

        let report = run_gate(store, root, &head, &def, transition.regret, record)?;
        reports.push(report);
    }

    Ok(TransitionReport {
        transition: transition.name,
        regret: transition.regret,
        gates: reports,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::model::{CommandSpec, GateKind, PopulationDelivery, Regret, Selector, State};
    use fl_core::store::MemStore;
    use fl_core::verdict::FailReason;
    use std::fs;
    use std::process::Command;

    fn repo_with(files: &[(&str, &str)]) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            Command::new("git").args(args).current_dir(d.path()).output().unwrap();
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "t"]);
        for (p, body) in files {
            let full = d.path().join(p);
            fs::create_dir_all(full.parent().unwrap()).unwrap();
            fs::write(full, body).unwrap();
        }
        run(&["add", "-A"]);
        run(&["commit", "-qm", "first"]);
        d
    }

    fn cmd(program: &str) -> GateKind {
        GateKind::Command(CommandSpec {
            program: program.into(),
            args: vec![],
            delivery: PopulationDelivery::Args,
            timeout_secs: 10,
            pass_codes: vec![0],
        })
    }

    fn setup(
        store: &mut MemStore,
        root: &std::path::Path,
        program: &str,
        pattern: &str,
        regret: Regret,
    ) -> ProjectId {
        let head = crate::git::Git::head(root).unwrap();
        let p = store.add_project(&root.display().to_string()).unwrap();
        let g = store
            .add_gate(
                p,
                "g",
                cmd(program),
                Selector::Glob { pattern: pattern.into() },
                1,
                &head,
                "tester",
            )
            .unwrap();
        store
            .add_transition(Transition {
                project: p,
                name: "launch".into(),
                from: State::Review,
                to: State::Done,
                regret,
                gates: vec![g],
            })
            .unwrap();
        p
    }

    #[test]
    fn a_passing_gate_over_a_real_population_passes_the_transition() {
        let d = repo_with(&[("src/a.rs", "fn a() {}")]);
        let mut s = MemStore::default();
        let p = setup(&mut s, d.path(), "true", "src/**/*.rs", Regret::Low);
        let r = evaluate_transition(&mut s, p, "launch", None).unwrap();
        assert!(r.passed());
        assert_eq!(r.exit_code(), 0);
        assert_eq!(r.gates[0].verdict.population(), Some(1));
    }

    #[test]
    fn a_selector_matching_nothing_fails_the_transition_and_exits_one() {
        let d = repo_with(&[("src/a.rs", "fn a() {}")]);
        let mut s = MemStore::default();
        let p = setup(&mut s, d.path(), "true", "nowhere/**/*.rs", Regret::Low);
        let r = evaluate_transition(&mut s, p, "launch", None).unwrap();
        assert!(!r.passed());
        assert_eq!(r.exit_code(), 1);
        assert_eq!(
            r.gates[0].verdict,
            Verdict::fail_for(FailReason::EmptyPopulation, 0)
        );
    }

    #[test]
    fn every_run_is_recorded_with_its_population_whatever_the_verdict() {
        let d = repo_with(&[("src/a.rs", "fn a() {}")]);
        let mut s = MemStore::default();
        let p = setup(&mut s, d.path(), "false", "src/**/*.rs", Regret::Low);
        let _ = evaluate_transition(&mut s, p, "launch", None).unwrap();
        let gate = s.list_gates(p).unwrap()[0].id;
        let runs = s.gate_runs(gate).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].population, 1);
        assert!(!runs[0].commit.is_empty());
    }

    // ⚠⚠ The regression test for the measured git behaviour. A DELETED file
    // is listed by `git diff --name-only` but can never be in a population
    // resolved from the working tree, so an intersection-based staleness
    // check would report this gate fresh.
    #[test]
    fn deleting_a_file_the_gate_covers_makes_it_stale() {
        let d = repo_with(&[("src/a.rs", "fn a() {}"), ("src/b.rs", "fn b() {}")]);
        let mut s = MemStore::default();
        let p = setup(&mut s, d.path(), "true", "src/**/*.rs", Regret::High);

        fs::remove_file(d.path().join("src/b.rs")).unwrap();
        let run = |args: &[&str]| {
            Command::new("git").args(args).current_dir(d.path()).output().unwrap();
        };
        run(&["add", "-A"]);
        run(&["commit", "-qm", "delete b"]);

        let r = evaluate_transition(&mut s, p, "launch", None).unwrap();
        assert!(!r.passed(), "a deletion inside the gate's own glob must make it stale");
        assert_eq!(r.gates[0].staleness, Staleness::StaleFail);
    }

    // The other half: a change OUTSIDE the gate's glob must leave it fresh.
    #[test]
    fn a_change_outside_the_gates_glob_leaves_it_fresh() {
        let d = repo_with(&[("src/a.rs", "fn a() {}"), ("docs.md", "one")]);
        let mut s = MemStore::default();
        let p = setup(&mut s, d.path(), "true", "src/**/*.rs", Regret::High);

        fs::write(d.path().join("docs.md"), "two").unwrap();
        let run = |args: &[&str]| {
            Command::new("git").args(args).current_dir(d.path()).output().unwrap();
        };
        run(&["add", "-A"]);
        run(&["commit", "-qm", "docs only"]);

        let r = evaluate_transition(&mut s, p, "launch", None).unwrap();
        assert!(r.passed(), "a gate whose own population did not move is fresh");
        assert_eq!(r.gates[0].staleness, Staleness::Fresh);
    }

    #[test]
    fn an_unknown_transition_is_refused_and_not_treated_as_passing() {
        let d = repo_with(&[("src/a.rs", "x")]);
        let mut s = MemStore::default();
        let p = setup(&mut s, d.path(), "true", "src/**/*.rs", Regret::Low);
        let err = evaluate_transition(&mut s, p, "nonexistent", None).unwrap_err();
        assert!(err.to_string().contains("nonexistent"), "got {err}");
    }

    // ⚠⚠ The empty-population rule one level up, pinned directly rather than
    // only asserted in a doc comment. Nothing in `fl-core` forbids
    // constructing a `Transition` with no gates, so this must not be left to
    // be "discovered" and "fixed" by someone reading `passed()` cold.
    #[test]
    fn a_transition_with_no_gates_has_not_been_verified() {
        let r = TransitionReport { transition: "launch".into(), regret: Regret::Low, gates: vec![] };
        assert!(!r.passed(), "an empty gate list ran nothing and must not read as a pass");
    }

    // A transition can name a gate id that was never stored (never created,
    // or deleted out from under it). That must be refused by name, the same
    // as an unknown transition, never silently treated as passing because
    // the loop over `transition.gates` had nothing to iterate distinctly.
    #[test]
    fn a_transition_naming_a_gate_that_does_not_exist_is_refused() {
        let d = repo_with(&[("src/a.rs", "x")]);
        let mut s = MemStore::default();
        let p = s.add_project(&d.path().display().to_string()).unwrap();
        let dangling = GateId(9999);
        s.add_transition(Transition {
            project: p,
            name: "launch".into(),
            from: State::Review,
            to: State::Done,
            regret: Regret::Low,
            gates: vec![dangling],
        })
        .unwrap();

        let err = evaluate_transition(&mut s, p, "launch", None).unwrap_err();
        assert!(err.to_string().contains(&dangling.to_string()), "got {err}");
    }
}
