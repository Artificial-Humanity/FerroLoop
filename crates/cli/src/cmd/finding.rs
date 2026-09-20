use anyhow::{Result, bail};
use clap::Subcommand;
use fl_core::finding::{Finding, FindingState};
use fl_core::ids::{FindingId, GateId, ProjectId, RecordId};
use fl_core::stale::Staleness;
use fl_core::store::Store;
use fl_exec::finding::{attach_reproduction, verify_finding};
use std::collections::BTreeSet;

#[derive(Subcommand)]
pub enum Cmd {
    Raise {
        #[arg(long)]
        record: u64,
        #[arg(long)]
        claim: String,
        #[arg(long)]
        by: String,
    },
    /// Attach a reproduction. REFUSED unless the gate currently fails.
    Reproduce {
        finding: u64,
        #[arg(long)]
        gate: u64,
    },
    Assign {
        finding: u64,
        #[arg(long = "to")]
        to: String,
    },
    /// The reproduction must now pass, and every neighbour must still pass.
    Verify {
        finding: u64,
    },
    Withdraw {
        finding: u64,
        #[arg(long)]
        reason: String,
    },
    List {
        #[arg(long)]
        project: u64,
        #[arg(long)]
        state: Option<String>,
    },
}

/// A short marker for a staleness reading, appended to a printed line. Text
/// output here always uses `FindingState`/`Staleness`' own wire vocabulary,
/// never a bespoke string, per the standing rule that casing on these wire
/// forms is undecided and not this task's to settle.
fn stale_note(s: Staleness) -> &'static str {
    match s {
        Staleness::Fresh => "",
        Staleness::StaleWarn => "  (stale: not re-validated since the code beneath it moved)",
        Staleness::StaleFail => "  (stale)",
    }
}

pub fn run(store: &mut impl Store, cmd: Cmd) -> Result<i32> {
    match cmd {
        Cmd::Raise { record, claim, by } => {
            let r = RecordId(record);
            let Some(rec) = store.get_record(r)? else {
                bail!(
                    "no record with id {record}. Use `flctl record list --project <id>` to see records that exist."
                );
            };
            let id = store.add_finding(Finding::raise(rec.project, r, &by, &claim))?;
            println!("{id}\traised\t{claim}");
        }
        Cmd::Reproduce { finding, gate } => {
            let report = attach_reproduction(store, FindingId(finding), GateId(gate))
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            println!(
                "{finding}\treproduced\tgate {gate} failed over {} items",
                report.verdict.population().unwrap_or(0)
            );
        }
        Cmd::Assign { finding, to } => {
            let id = FindingId(finding);
            let Some(mut f) = store.get_finding(id)? else {
                bail!(
                    "no finding with id {finding}. Use `flctl finding list --project <id>` to see findings that exist."
                );
            };
            f.assign(&to).map_err(|e| anyhow::anyhow!("{e}"))?;
            store.update_finding(&f)?;
            println!("{finding}\tassigned\t{to}");
        }
        Cmd::Verify { finding } => {
            let id = FindingId(finding);
            let report = verify_finding(store, id).map_err(|e| anyhow::anyhow!("{e}"))?;

            if report.reproduction.verdict.is_pass() {
                println!("REPRODUCTION\tpasses{}", stale_note(report.reproduction.staleness));
            } else {
                println!(
                    "REPRODUCTION\tstill fails: {:?}{}",
                    report.reproduction.verdict,
                    stale_note(report.reproduction.staleness)
                );
            }
            for r in &report.regressions {
                println!("REGRESSION\t{}\t{:?}{}", r.name, r.verdict, stale_note(r.staleness));
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
            for r in &report.neighbours {
                if r.verdict.is_pass() && r.staleness != Staleness::Fresh {
                    println!("NEIGHBOUR\t{}\tpasses{}", r.name, stale_note(r.staleness));
                }
            }

            if report.closed {
                println!("CLOSED\t{finding}");
            } else {
                println!("OPEN\t{finding}\tthe repair is not done");
            }
            return Ok(report.exit_code());
        }
        Cmd::Withdraw { finding, reason } => {
            let id = FindingId(finding);
            let Some(mut f) = store.get_finding(id)? else {
                bail!(
                    "no finding with id {finding}. Use `flctl finding list --project <id>` to see findings that exist."
                );
            };
            f.withdraw(&reason).map_err(|e| anyhow::anyhow!("{e}"))?;
            store.update_finding(&f)?;
            println!("{finding}\twithdrawn\t{reason}");
        }
        Cmd::List { project, state } => {
            let want = match state.as_deref() {
                None => None,
                Some(s) => Some(FindingState::from_wire(s).ok_or_else(|| {
                    anyhow::anyhow!(
                        "`{s}` is not a finding state. Valid states are: \
                         raised, reproduced, assigned, fixed, withdrawn."
                    )
                })?),
            };
            let all = store.list_findings(ProjectId(project))?;
            let mut raisers: BTreeSet<String> = Default::default();
            for f in all.iter().filter(|f| want.is_none_or(|w| f.state == w)) {
                println!("{}\t{}\t{}\t{}", f.id, f.state.as_wire(), f.raised_by, f.claim);
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

#[cfg(test)]
mod tests {
    use super::*;

    // `stale_note` is a three-arm match with no prior test, and nothing
    // else in the suite exercises its `StaleWarn` arm (only `Fresh`, via
    // silence, and `StaleFail`, via the `(stale)` suffix on a REGRESSION
    // line elsewhere).
    #[test]
    fn stale_note_is_empty_for_fresh() {
        assert_eq!(stale_note(Staleness::Fresh), "");
    }

    #[test]
    fn stale_note_names_the_moved_code_for_stale_warn() {
        assert_eq!(
            stale_note(Staleness::StaleWarn),
            "  (stale: not re-validated since the code beneath it moved)"
        );
    }

    #[test]
    fn stale_note_is_the_short_form_for_stale_fail() {
        assert_eq!(stale_note(Staleness::StaleFail), "  (stale)");
    }
}
