use crate::refs::{self, Ref};
use anyhow::{Result, bail};
use clap::Subcommand;
use fl_core::ids::{ProjectId, RecordId};
use fl_core::model::State;
use fl_core::store::{Catalog, Roles, Tracker};
use fl_core::{Iri, Kind};
use fl_exec::record::{MoveOutcome, move_record};
use fl_store::RedbStore;

#[derive(Subcommand)]
pub enum Cmd {
    Add {
        #[arg(long)]
        project: Ref,
        #[arg(long)]
        title: String,
    },
    List {
        #[arg(long)]
        project: Ref,
    },
    Move {
        id: Ref,
        #[arg(long = "to")]
        to: String,
    },
}

impl Cmd {
    /// Every item this command names — a target state is not an id and
    /// never appears here.
    pub fn iris(&self) -> Vec<Iri> {
        match self {
            Cmd::Add { project, .. } => refs::iris(&[project]),
            Cmd::List { project } => refs::iris(&[project]),
            Cmd::Move { id, .. } => refs::iris(&[id]),
        }
    }
}

pub fn run(store: &RedbStore, cmd: Cmd) -> Result<i32> {
    match cmd {
        Cmd::Add { project, title } => {
            let p = ProjectId(refs::resolve(
                store,
                store.label(),
                Kind::Project,
                &project,
            )?);
            if store.get_project(&p)?.is_none() {
                bail!(
                    "`{project}` is not a project in the store at {}. Run `fl project list` to \
                     see the ones that exist.",
                    store.label()
                );
            }
            let id = store.add_record(&p, &title)?;
            println!("{}\t{title}", refs::show(store, Kind::Record, id.iri())?);
        }
        Cmd::List { project } => {
            let p = ProjectId(refs::resolve(
                store,
                store.label(),
                Kind::Project,
                &project,
            )?);
            for r in store.list_records(&p)? {
                println!(
                    "{}\t{}\t{}",
                    refs::show(store, Kind::Record, r.id.iri())?,
                    r.state.as_wire(),
                    r.title
                );
            }
        }
        Cmd::Move { id, to } => {
            let Some(state) = State::from_wire(&to) else {
                bail!(
                    "`{to}` is not a state. Valid states are: {}.",
                    State::wire_values()
                );
            };
            let r = RecordId(refs::resolve(store, store.label(), Kind::Record, &id)?);
            let Some(record) = store.get_record(&r)? else {
                bail!(
                    "`{id}` is not a record in the store at {}. Use \
                     `fl record list --project <project>` to see records that exist.",
                    store.label()
                );
            };

            let report = move_record(Roles::single(store), &record, state)
                .map_err(|e| anyhow::anyhow!("{e}"))?;

            if let MoveOutcome::Ungated = report.outcome {
                println!(
                    "{id}\t{}\tungated: project {} declares no transition from `{}` to `{}`",
                    state.as_wire(),
                    refs::show(store, Kind::Project, record.project.iri())?,
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
