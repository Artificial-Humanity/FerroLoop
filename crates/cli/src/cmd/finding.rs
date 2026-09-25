use anyhow::{Result, bail};
use clap::Subcommand;
use fl_core::finding::{Finding, FindingState};
use fl_core::ids::{FindingId, GateId, ProjectId, RecordId};
use fl_core::store::{Roles, Tracker};
use fl_exec::finding::{attach_reproduction, verify_finding};
use fl_store::RedbStore;
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
    Verify { finding: u64 },
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

pub fn run(store: &RedbStore, cmd: Cmd) -> Result<i32> {
    match cmd {
        Cmd::Raise { record, claim, by } => {
            let r = RecordId(record);
            let Some(rec) = store.get_record(&r)? else {
                bail!(
                    "no record with id {record}. Use `fl record list --project <id>` to see records that exist."
                );
            };
            let id = store.add_finding(Finding::raise(rec.project, r, &by, &claim))?;
            println!("{id}\traised\t{claim}");
        }
        Cmd::Reproduce { finding, gate } => {
            let report =
                attach_reproduction(Roles::single(store), &FindingId(finding), &GateId(gate))
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
            println!(
                "{finding}\treproduced\tgate {gate} failed over {} items",
                report.verdict.population().unwrap_or(0)
            );
        }
        Cmd::Assign { finding, to } => {
            let id = FindingId(finding);
            let Some(mut f) = store.get_finding(&id)? else {
                bail!(
                    "no finding with id {finding}. Use `fl finding list --project <id>` to see findings that exist."
                );
            };
            f.assign(&to).map_err(|e| anyhow::anyhow!("{e}"))?;
            store.update_finding(&f)?;
            println!("{finding}\tassigned\t{to}");
        }
        Cmd::Verify { finding } => {
            let id = FindingId(finding);
            let report =
                verify_finding(Roles::single(store), &id).map_err(|e| anyhow::anyhow!("{e}"))?;

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
        Cmd::Withdraw { finding, reason } => {
            let id = FindingId(finding);
            let Some(mut f) = store.get_finding(&id)? else {
                bail!(
                    "no finding with id {finding}. Use `fl finding list --project <id>` to see findings that exist."
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
                        "`{s}` is not a finding state. Valid states are: {}.",
                        FindingState::wire_values()
                    )
                })?),
            };
            let all = store.list_findings(&ProjectId(project))?;
            let mut raisers: BTreeSet<String> = Default::default();
            for f in all.iter().filter(|f| want.is_none_or(|w| f.state == w)) {
                println!(
                    "{}\t{}\t{}\t{}",
                    f.id,
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
