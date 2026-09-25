use anyhow::{Result, bail};
use clap::Subcommand;
use fl_core::ids::{ProjectId, RecordId};
use fl_core::model::State;
use fl_core::store::{Catalog, Roles, Tracker};
use fl_exec::record::{MoveOutcome, move_record};
use fl_store::RedbStore;

#[derive(Subcommand)]
pub enum Cmd {
    Add {
        #[arg(long)]
        project: u64,
        #[arg(long)]
        title: String,
    },
    List {
        #[arg(long)]
        project: u64,
    },
    Move {
        id: u64,
        #[arg(long = "to")]
        to: String,
    },
}

pub fn run(store: &RedbStore, cmd: Cmd) -> Result<i32> {
    match cmd {
        Cmd::Add { project, title } => {
            let p = ProjectId(project);
            if store.get_project(&p)?.is_none() {
                bail!(
                    "no project with id {project}. Run `fl project list` to see the ids that exist."
                );
            }
            let id = store.add_record(&p, &title)?;
            println!("{id}\t{title}");
        }
        Cmd::List { project } => {
            for r in store.list_records(&ProjectId(project))? {
                println!("{}\t{}\t{}", r.id, r.state.as_wire(), r.title);
            }
        }
        Cmd::Move { id, to } => {
            let Some(state) = State::from_wire(&to) else {
                bail!(
                    "`{to}` is not a state. Valid states are: {}.",
                    State::wire_values()
                );
            };
            let r = RecordId(id);
            let Some(record) = store.get_record(&r)? else {
                bail!(
                    "no record with id {id}. Use `fl record list --project <id>` to see records that exist."
                );
            };

            let report = move_record(Roles::single(store), &record, state)
                .map_err(|e| anyhow::anyhow!("{e}"))?;

            if let MoveOutcome::Ungated = report.outcome {
                println!(
                    "{id}\t{}\tungated: project {} declares no transition from `{}` to `{}`",
                    state.as_wire(),
                    record.project,
                    record.state.as_wire(),
                    state.as_wire()
                );
                return Ok(0);
            }

            for t in &report.transitions {
                for g in &t.gates {
                    let (label, detail) = g.verdict.describe();
                    println!(
                        "{label}\t{}\t{}\t{detail}\t{}ms{}",
                        t.transition,
                        g.name,
                        g.duration_ms,
                        g.staleness.note()
                    );
                    if !g.verdict.is_pass() && !g.output_excerpt.is_empty() {
                        for line in g.output_excerpt.lines().take(20) {
                            println!("\t| {line}");
                        }
                    }
                }
                // Same rule `check` applies: a transition that declares no
                // gates verified nothing, so it cannot authorise a move.
                if t.gates.is_empty() {
                    println!(
                        "FAIL\t{}\tthe transition declares no gates, so nothing was verified",
                        t.transition
                    );
                }
            }

            match report.outcome {
                MoveOutcome::Refused { code } => {
                    println!("REFUSED\t{id}\tstays `{}`", record.state.as_wire());
                    return Ok(code);
                }
                _ => {
                    println!("{id}\t{}", state.as_wire());
                }
            }
        }
    }
    Ok(0)
}
