use crate::refs::{self, Ref};
use anyhow::{Result, bail};
use clap::Subcommand;
use fl_core::finding::{Finding, FindingState};
use fl_core::ids::{FindingId, GateId, ProjectId, RecordId};
use fl_core::store::{Roles, Tracker};
use fl_core::{Iri, Kind};
use fl_exec::finding::{FindingExecError, attach_reproduction, verify_finding};
use fl_store::RedbStore;
use std::collections::BTreeSet;

#[derive(Subcommand)]
pub enum Cmd {
    Raise {
        #[arg(long)]
        record: Ref,
        #[arg(long)]
        claim: String,
        #[arg(long)]
        by: String,
    },
    /// Attach a reproduction. REFUSED unless the gate currently fails.
    Reproduce {
        finding: Ref,
        #[arg(long)]
        gate: Ref,
    },
    Assign {
        finding: Ref,
        #[arg(long = "to")]
        to: String,
    },
    /// The reproduction must now pass, and every neighbour must still pass.
    Verify { finding: Ref },
    Withdraw {
        finding: Ref,
        #[arg(long)]
        reason: String,
    },
    List {
        #[arg(long)]
        project: Ref,
        #[arg(long)]
        state: Option<String>,
    },
}

impl Cmd {
    /// Every item this command names — a claim, an assignee and a
    /// withdrawal reason are strings, not ids.
    pub fn iris(&self) -> Vec<Iri> {
        match self {
            Cmd::Raise { record, .. } => refs::iris(&[record]),
            Cmd::Reproduce { finding, gate } => refs::iris(&[finding, gate]),
            Cmd::Assign { finding, .. } => refs::iris(&[finding]),
            Cmd::Verify { finding } => refs::iris(&[finding]),
            Cmd::Withdraw { finding, .. } => refs::iris(&[finding]),
            Cmd::List { project, .. } => refs::iris(&[project]),
        }
    }
}

fn finding_id(store: &RedbStore, r: &Ref) -> Result<FindingId> {
    Ok(FindingId(refs::resolve(
        store,
        store.label(),
        Kind::Finding,
        r,
    )?))
}

/// The finding `r` names, or a refusal that echoes what was typed.
fn finding(store: &RedbStore, r: &Ref) -> Result<Finding> {
    let Some(f) = store.get_finding(&finding_id(store, r)?)? else {
        bail!(
            "`{r}` is not a finding in the store at {}. Use \
             `fl finding list --project <project>` to see findings that exist.",
            store.label()
        );
    };
    Ok(f)
}

/// Render a fl-exec refusal for a person. fl-exec knows items only by IRI,
/// so the variants that name an item are re-spelled here with what the user
/// typed (`finding`, and `gate` where the command took one). Every other
/// variant names no id and passes through unchanged.
fn explain(e: FindingExecError, finding: &Ref, gate: Option<&Ref>) -> anyhow::Error {
    match e {
        FindingExecError::NoSuchFinding(_) => anyhow::anyhow!("no finding {finding}"),
        FindingExecError::NoSuchGate(_) => match gate {
            Some(g) => anyhow::anyhow!("no gate {g}"),
            None => anyhow::anyhow!("{e}"),
        },
        FindingExecError::NotAssigned(_, state) => anyhow::anyhow!(
            "finding {finding} is in state {state}, and only an assigned finding can be verified"
        ),
        FindingExecError::NoReproduction(_) => {
            anyhow::anyhow!("finding {finding} has no reproduction")
        }
        other => anyhow::anyhow!("{other}"),
    }
}

pub fn run(store: &RedbStore, cmd: Cmd) -> Result<i32> {
    match cmd {
        Cmd::Raise { record, claim, by } => {
            let r = RecordId(refs::resolve(store, store.label(), Kind::Record, &record)?);
            let Some(rec) = store.get_record(&r)? else {
                bail!(
                    "`{record}` is not a record in the store at {}. Use \
                     `fl record list --project <project>` to see records that exist.",
                    store.label()
                );
            };
            let id = store.add_finding(Finding::raise(rec.project, r, &by, &claim))?;
            println!(
                "{}\traised\t{claim}",
                refs::show(store, Kind::Finding, id.iri())?
            );
        }
        Cmd::Reproduce { finding, gate } => {
            let fid = finding_id(store, &finding)?;
            let gid = GateId(refs::resolve(store, store.label(), Kind::Gate, &gate)?);
            let report = attach_reproduction(Roles::single(store), &fid, &gid)
                .map_err(|e| explain(e, &finding, Some(&gate)))?;
            println!(
                "{finding}\treproduced\tgate {gate} failed over {} items",
                report.verdict.population().unwrap_or(0)
            );
        }
        Cmd::Assign { finding: arg, to } => {
            let mut f = finding(store, &arg)?;
            f.assign(&to).map_err(|e| anyhow::anyhow!("{e}"))?;
            store.update_finding(&f)?;
            println!("{arg}\tassigned\t{to}");
        }
        Cmd::Verify { finding } => {
            let id = finding_id(store, &finding)?;
            let report = verify_finding(Roles::single(store), &id)
                .map_err(|e| explain(e, &finding, None))?;

            if report.reproduction.verdict.is_pass() {
                println!(
                    "REPRODUCTION\tpasses over {} items{}",
                    report.reproduction.verdict.population().unwrap_or(0),
                    report.reproduction.staleness.note()
                );
            } else {
                // ⚠ The LABEL is carried, not dropped. An ERROR is a broken
                // instrument and proves nothing in either direction, so
                // narrating it as "still fails" would claim evidence the run
                // did not produce. `check` and `gate run` print the same
                // label for the same verdict.
                let (label, detail) = report.reproduction.verdict.describe();
                println!(
                    "REPRODUCTION\t{label}\t{detail}{}",
                    report.reproduction.staleness.note()
                );
            }

            // ⚠ Fix-wave finding 2 (spec §7): "a run that examined nothing
            // must never be indistinguishable from a run that examined
            // everything." Printed unconditionally, including at zero,
            // because a verify that ran no neighbours and a verify that ran
            // several must never produce byte-identical output. This line
            // does not change `closed` or the exit code — it is visibility
            // only, and zero neighbours is not itself a failure (the first
            // finding in a fresh project has none, and must stay closable).
            println!(
                "NEIGHBOURS\t{} checked, {} regressed",
                report.neighbours.len(),
                report.regressions.len()
            );

            for r in &report.regressions {
                let (label, detail) = r.verdict.describe();
                println!(
                    "REGRESSION\t{}\t{label}\t{detail}{}",
                    r.name,
                    r.staleness.note()
                );
            }

            // Task 16b: `verify_finding`'s `FixReport` now carries every
            // neighbour it evaluated, not just the regressions, so a
            // passing-but-stale neighbour is read straight out of the
            // report instead of being re-derived and re-run here. Before
            // this, `finding verify` invoked every qualifying neighbour
            // gate — any gate with a `last_pass_commit`, whether or not it
            // was stale, whether or not anything ended up printed for it —
            // a second time purely to recover its staleness. That is not
            // scoped to stale neighbours: a Fresh neighbour was run twice
            // too, it simply never produced an extra line, so the doubled
            // cost was invisible in the output. None of this feeds back
            // into `closed` or the exit code: those stay exactly what
            // `verify_finding` decided.
            //
            // ⚠ Fix-wave finding 2 also completes what this left half-done:
            // a stale-but-passing neighbour printed a note, but a *fresh*
            // passing neighbour printed nothing at all — the majority case
            // was invisible. Every evaluated neighbour now gets a line.
            for r in &report.neighbours {
                if r.verdict.is_pass() {
                    println!("NEIGHBOUR\t{}\tpasses{}", r.name, r.staleness.note());
                }
            }

            if report.closed {
                println!("CLOSED\t{finding}");
            } else {
                println!("OPEN\t{finding}\tthe repair is not done");
            }
            return Ok(report.exit_code());
        }
        Cmd::Withdraw {
            finding: arg,
            reason,
        } => {
            let mut f = finding(store, &arg)?;
            f.withdraw(&reason).map_err(|e| anyhow::anyhow!("{e}"))?;
            store.update_finding(&f)?;
            println!("{arg}\twithdrawn\t{reason}");
        }
        Cmd::List { project, state } => {
            let want = match state.as_deref() {
                None => None,
                Some(s) => Some(FindingState::from_wire(s).ok_or_else(|| {
                    anyhow::anyhow!(
                        "`{s}` is not a finding state. Valid states are: {}.",
                        FindingState::wire_values()
                    )
                })?),
            };
            let p = ProjectId(refs::resolve(
                store,
                store.label(),
                Kind::Project,
                &project,
            )?);
            let all = store.list_findings(&p)?;
            let mut raisers: BTreeSet<String> = Default::default();
            for f in all.iter().filter(|f| want.is_none_or(|w| f.state == w)) {
                println!(
                    "{}\t{}\t{}\t{}",
                    refs::show(store, Kind::Finding, f.id.iri())?,
                    f.state.as_wire(),
                    f.raised_by,
                    f.claim
                );
                raisers.insert(f.raised_by.clone());
            }
            // ⚠ Decision 27's cost, printed where it can be seen. A cost
            // nobody reads is not a cost.
            for actor in raisers {
                let n = store.withdrawals_by(&actor)?;
                if n > 0 {
                    println!("{actor}\twithdrawn: {n}");
                }
            }
        }
    }
    Ok(0)
}
