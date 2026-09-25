use crate::command::run_command_gate;
use crate::git::Git;
use crate::population::{ExecError, resolve};
use fl_core::ids::{GateId, ProjectId, RecordId};
use fl_core::log::GateRun;
use fl_core::model::{GateDef, GateKind, Project, Regret, Selector, Transition};
use fl_core::stale::{Staleness, apply_staleness, is_stale};
use fl_core::store::{Catalog, Ledger, StoreError, follow};
use fl_core::verdict::Verdict;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
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
        self.gates
            .iter()
            .map(|g| g.verdict.exit_code())
            .max()
            .unwrap_or(1)
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
            changed.iter().any(|p| {
                p.strip_prefix(root)
                    .map(|rel| m.is_match(rel))
                    .unwrap_or(false)
            })
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
    catalog: &dyn Catalog,
    ledger: &dyn Ledger,
    root: &Path,
    head: &str,
    def: &GateDef,
    regret: Regret,
    record: Option<&RecordId>,
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

    ledger
        .append_gate_run(GateRun {
            gate: def.id.clone(),
            record: record.cloned(),
            commit: head.to_string(),
            verdict: verdict.clone(),
            population: verdict.population().unwrap_or(0),
            output_excerpt: excerpt.clone(),
            duration_ms,
            cost_usd_micros: 0,
        })
        .map_err(|e| ExecError::Store(e.to_string()))?;

    if verdict.is_pass() {
        let mut updated = def.clone();
        updated.last_pass_commit = Some(head.to_string());
        let _ = catalog.update_gate(&updated);
    }

    Ok(GateReport {
        gate: def.id.clone(),
        name: def.name.clone(),
        verdict,
        staleness,
        output_excerpt: excerpt,
        duration_ms,
    })
}

/// The project `project` names. An id the store never held is the store's
/// own `NotOwned`, propagated as it is: that refusal names where it looked,
/// and "no project" would claim a search that never happened.
fn project_of(catalog: &dyn Catalog, project: &ProjectId) -> Result<Project, ExecError> {
    catalog
        .get_project(project)
        .map_err(|e| ExecError::Store(e.to_string()))?
        .ok_or_else(|| {
            ExecError::BadSelector(format!(
                "{project} is held by this store, but it is not a project"
            ))
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
    catalog: &dyn Catalog,
    ledger: &dyn Ledger,
    project: &ProjectId,
    gate: &GateId,
) -> Result<GateReport, ExecError> {
    let proj = project_of(catalog, project)?;
    let root = Path::new(&proj.root);

    let Some(def) = catalog
        .get_gate(gate)
        .map_err(|e| ExecError::Store(e.to_string()))?
    else {
        return Err(ExecError::BadSelector(format!("no gate with id {gate}")));
    };

    let head = Git::head(root)?;
    run_gate(catalog, ledger, root, &head, &def, Regret::Low, None)
}

pub fn evaluate_transition(
    catalog: &dyn Catalog,
    ledger: &dyn Ledger,
    project: &ProjectId,
    transition_name: &str,
    record: Option<&RecordId>,
) -> Result<TransitionReport, ExecError> {
    let proj = project_of(catalog, project)?;
    let root = Path::new(&proj.root);

    let transition: Transition = catalog
        .get_transition(project, transition_name)
        .map_err(|e| ExecError::Store(e.to_string()))?
        .ok_or_else(|| {
            ExecError::BadSelector(format!(
                "the project at {} declares no transition named `{transition_name}`. \
                 Add it with `fl transition add`, or name one of the existing ones.",
                proj.root
            ))
        })?;

    let head = Git::head(root)?;
    let mut reports = Vec::new();

    for gate_id in &transition.gates {
        let def = match follow(
            &format!("transition `{transition_name}`"),
            gate_id.iri(),
            catalog.get_gate(gate_id),
        ) {
            Ok(def) => def,
            // `NotOwned` carries no mention of the transition on its own —
            // "no store holds it" says where nothing was found, not which
            // reference sent us looking. `Dangling` already names both (it
            // was built from `from` above), so it passes through unchanged.
            Err(e @ StoreError::NotOwned { .. }) => {
                return Err(ExecError::BadSelector(format!(
                    "transition `{transition_name}` names gate {gate_id}: {e}"
                )));
            }
            Err(e) => return Err(ExecError::Store(e.to_string())),
        };

        let report = run_gate(
            catalog,
            ledger,
            root,
            &head,
            &def,
            transition.regret,
            record,
        )?;
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
    use fl_core::MemStore;
    use fl_core::finding::Finding;
    use fl_core::ids::{FindingId, ProjectId, RecordId, seq_iri};
    use fl_core::log::{Attempt, GateRun};
    use fl_core::model::{CommandSpec, GateKind, PopulationDelivery, Regret, Selector, State};
    use fl_core::model::{GateDef, Project, Record, Transition};
    use fl_core::store::{Catalog, Ledger, StoreError, Tracker};
    use fl_core::verdict::FailReason;
    use std::fs;
    use std::process::Command;

    fn repo_with(files: &[(&str, &str)]) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(d.path())
                .output()
                .unwrap();
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
        store: &MemStore,
        root: &std::path::Path,
        program: &str,
        pattern: &str,
        regret: Regret,
    ) -> ProjectId {
        let head = crate::git::Git::head(root).unwrap();
        let p = store.add_project(&root.display().to_string()).unwrap();
        let g = store
            .add_gate(
                &p,
                "g",
                cmd(program),
                Selector::Glob {
                    pattern: pattern.into(),
                },
                1,
                &head,
                "tester",
            )
            .unwrap();
        store
            .add_transition(Transition {
                project: p.clone(),
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
        let s = MemStore::default();
        let p = setup(&s, d.path(), "true", "src/**/*.rs", Regret::Low);
        let r = evaluate_transition(&s, &s, &p, "launch", None).unwrap();
        assert!(r.passed());
        assert_eq!(r.exit_code(), 0);
        assert_eq!(r.gates[0].verdict.population(), Some(1));
    }

    #[test]
    fn a_selector_matching_nothing_fails_the_transition_and_exits_one() {
        let d = repo_with(&[("src/a.rs", "fn a() {}")]);
        let s = MemStore::default();
        let p = setup(&s, d.path(), "true", "nowhere/**/*.rs", Regret::Low);
        let r = evaluate_transition(&s, &s, &p, "launch", None).unwrap();
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
        let s = MemStore::default();
        let p = setup(&s, d.path(), "false", "src/**/*.rs", Regret::Low);
        let _ = evaluate_transition(&s, &s, &p, "launch", None).unwrap();
        let gate = s.list_gates(&p).unwrap()[0].id.clone();
        let runs = s.gate_runs(&gate).unwrap();
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
        let s = MemStore::default();
        let p = setup(&s, d.path(), "true", "src/**/*.rs", Regret::High);

        fs::remove_file(d.path().join("src/b.rs")).unwrap();
        let run = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(d.path())
                .output()
                .unwrap();
        };
        run(&["add", "-A"]);
        run(&["commit", "-qm", "delete b"]);

        let r = evaluate_transition(&s, &s, &p, "launch", None).unwrap();
        assert!(
            !r.passed(),
            "a deletion inside the gate's own glob must make it stale"
        );
        assert_eq!(r.gates[0].staleness, Staleness::StaleFail);
    }

    // The other half: a change OUTSIDE the gate's glob must leave it fresh.
    #[test]
    fn a_change_outside_the_gates_glob_leaves_it_fresh() {
        let d = repo_with(&[("src/a.rs", "fn a() {}"), ("docs.md", "one")]);
        let s = MemStore::default();
        let p = setup(&s, d.path(), "true", "src/**/*.rs", Regret::High);

        fs::write(d.path().join("docs.md"), "two").unwrap();
        let run = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(d.path())
                .output()
                .unwrap();
        };
        run(&["add", "-A"]);
        run(&["commit", "-qm", "docs only"]);

        let r = evaluate_transition(&s, &s, &p, "launch", None).unwrap();
        assert!(
            r.passed(),
            "a gate whose own population did not move is fresh"
        );
        assert_eq!(r.gates[0].staleness, Staleness::Fresh);
    }

    #[test]
    fn an_unknown_transition_is_refused_and_not_treated_as_passing() {
        let d = repo_with(&[("src/a.rs", "x")]);
        let s = MemStore::default();
        let p = setup(&s, d.path(), "true", "src/**/*.rs", Regret::Low);
        let err = evaluate_transition(&s, &s, &p, "nonexistent", None).unwrap_err();
        assert!(err.to_string().contains("nonexistent"), "got {err}");
    }

    // ⚠⚠ The empty-population rule one level up, pinned directly rather than
    // only asserted in a doc comment. Nothing in `fl-core` forbids
    // constructing a `Transition` with no gates, so this must not be left to
    // be "discovered" and "fixed" by someone reading `passed()` cold.
    #[test]
    fn a_transition_with_no_gates_has_not_been_verified() {
        let r = TransitionReport {
            transition: "launch".into(),
            regret: Regret::Low,
            gates: vec![],
        };
        assert!(
            !r.passed(),
            "an empty gate list ran nothing and must not read as a pass"
        );
    }

    // A transition can name a gate id that was never stored (never created,
    // or deleted out from under it). That must be refused by name, the same
    // as an unknown transition, never silently treated as passing because
    // the loop over `transition.gates` had nothing to iterate distinctly.
    //
    // Two different ways for a gate reference to fail to resolve, each
    // refused with a different shape (spec §5): a gate id no store has ever
    // minted is `NotOwned` — "never looked" — and the refusal is wrapped so
    // it still names the transition. A gate id this same store holds, but
    // under another kind, is `Dangling` — "gone" — and the refusal already
    // names both the transition and "dangling" without any wrapping, since
    // `follow` built it from the label `evaluate_transition` passed in.
    #[test]
    fn a_transition_naming_a_gate_that_does_not_exist_is_refused() {
        let d = repo_with(&[("src/a.rs", "x")]);
        let s = MemStore::default();
        let p = s.add_project(&d.path().display().to_string()).unwrap();

        // A gate id that no store holds: "never looked" — NotOwned, with the
        // transition named so the reader knows where the reference came from.
        let stranger = GateId(seq_iri(9999));
        s.add_transition(Transition {
            project: p.clone(),
            name: "launch".into(),
            from: State::Review,
            to: State::Done,
            regret: Regret::Low,
            gates: vec![stranger.clone()],
        })
        .unwrap();
        let err = evaluate_transition(&s, &s, &p, "launch", None).unwrap_err();
        assert!(
            err.to_string().contains(stranger.iri().as_str()),
            "got {err}"
        );
        assert!(err.to_string().contains("launch"), "got {err}");

        // An id the store holds as another kind: "gone" — Dangling.
        let record = s.add_record(&p, "t").unwrap();
        s.add_transition(Transition {
            project: p.clone(),
            name: "ship".into(),
            from: State::Review,
            to: State::Done,
            regret: Regret::Low,
            gates: vec![GateId(record.0.clone())],
        })
        .unwrap();
        let err = evaluate_transition(&s, &s, &p, "ship", None).unwrap_err();
        assert!(err.to_string().contains("dangling"), "got {err}");
    }

    /// A store where every read fails, so the error a caller sees is the
    /// only thing under test.
    struct BrokenStore;

    impl Catalog for BrokenStore {
        fn add_project(&self, _: &str) -> Result<ProjectId, StoreError> {
            Err(broken())
        }
        fn get_project(&self, _: &ProjectId) -> Result<Option<Project>, StoreError> {
            Err(broken())
        }
        fn list_projects(&self) -> Result<Vec<Project>, StoreError> {
            Err(broken())
        }
        #[allow(clippy::too_many_arguments)]
        fn add_gate(
            &self,
            _: &ProjectId,
            _: &str,
            _: GateKind,
            _: Selector,
            _: u64,
            _: &str,
            _: &str,
        ) -> Result<GateId, StoreError> {
            Err(broken())
        }
        fn get_gate(&self, _: &GateId) -> Result<Option<GateDef>, StoreError> {
            Err(broken())
        }
        fn list_gates(&self, _: &ProjectId) -> Result<Vec<GateDef>, StoreError> {
            Err(broken())
        }
        fn update_gate(&self, _: &GateDef) -> Result<(), StoreError> {
            Err(broken())
        }
        fn add_transition(&self, _: Transition) -> Result<(), StoreError> {
            Err(broken())
        }
        fn get_transition(&self, _: &ProjectId, _: &str) -> Result<Option<Transition>, StoreError> {
            Err(broken())
        }
        fn list_transitions(&self, _: &ProjectId) -> Result<Vec<Transition>, StoreError> {
            Err(broken())
        }
    }

    impl Tracker for BrokenStore {
        fn add_record(&self, _: &ProjectId, _: &str) -> Result<RecordId, StoreError> {
            Err(broken())
        }
        fn get_record(&self, _: &RecordId) -> Result<Option<Record>, StoreError> {
            Err(broken())
        }
        fn list_records(&self, _: &ProjectId) -> Result<Vec<Record>, StoreError> {
            Err(broken())
        }
        fn set_record_state(&self, _: &RecordId, _: State) -> Result<(), StoreError> {
            Err(broken())
        }
        fn add_finding(&self, _: Finding) -> Result<FindingId, StoreError> {
            Err(broken())
        }
        fn get_finding(&self, _: &FindingId) -> Result<Option<Finding>, StoreError> {
            Err(broken())
        }
        fn update_finding(&self, _: &Finding) -> Result<(), StoreError> {
            Err(broken())
        }
        fn list_findings(&self, _: &ProjectId) -> Result<Vec<Finding>, StoreError> {
            Err(broken())
        }
        fn withdrawals_by(&self, _: &str) -> Result<u64, StoreError> {
            Err(broken())
        }
        fn add_alias(&self, _: &fl_core::Iri, _: fl_core::Iri) -> Result<(), StoreError> {
            Err(broken())
        }
    }

    impl Ledger for BrokenStore {
        fn append_gate_run(&self, _: GateRun) -> Result<(), StoreError> {
            Err(broken())
        }
        fn append_attempt(&self, _: Attempt) -> Result<(), StoreError> {
            Err(broken())
        }
        fn gate_runs(&self, _: &GateId) -> Result<Vec<GateRun>, StoreError> {
            Err(broken())
        }
        fn attempts(&self, _: &ProjectId) -> Result<Vec<Attempt>, StoreError> {
            Err(broken())
        }
    }

    fn broken() -> StoreError {
        StoreError::Decode("unknown variant `Review`".into())
    }

    // ⚠ Six sites here used to wrap a `StoreError` in `ExecError::Git`, so a
    // store failure announced itself as `git failed:` from `check` — the
    // flagship command naming the wrong cause. A one-token revert restores
    // that silently, which is why it is gated rather than trusted.
    #[test]
    fn a_store_failure_is_reported_as_a_store_failure_and_never_as_git() {
        let store = BrokenStore;
        let err = run_single_gate(&store, &store, &ProjectId(seq_iri(1)), &GateId(seq_iri(1)))
            .expect_err("a broken store cannot produce a gate report");
        assert!(
            matches!(err, ExecError::Store(_)),
            "a store failure surfaced as {err:?}"
        );
        let msg = err.to_string();
        assert!(!msg.contains("git"), "blames git: {msg}");
        assert!(msg.contains("unknown variant"), "loses the cause: {msg}");
    }

    #[test]
    fn a_store_failure_during_a_transition_is_also_a_store_failure() {
        let store = BrokenStore;
        let err = evaluate_transition(&store, &store, &ProjectId(seq_iri(1)), "launch", None)
            .expect_err("a broken store cannot produce a transition report");
        assert!(
            matches!(err, ExecError::Store(_)),
            "a store failure surfaced as {err:?}"
        );
        assert!(!err.to_string().contains("git"), "blames git: {err}");
    }
}
