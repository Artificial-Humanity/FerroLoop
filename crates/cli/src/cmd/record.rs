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
    /// Every item this command names, by `Ref` — the single source `iris()`
    /// and `has_handle()` both derive from, so a `Ref` field added to a
    /// variant here is picked up by both at once (Fix round 2, item 5). A
    /// target state is not an id and never appears here.
    fn refs(&self) -> Vec<&Ref> {
        match self {
            Cmd::Add { project, .. } => vec![project],
            Cmd::List { project } => vec![project],
            Cmd::Move { id, .. } => vec![id],
        }
    }

    pub fn iris(&self) -> Vec<Iri> {
        refs::iris(&self.refs())
    }

    /// Whether this command names any item by handle rather than IRI.
    pub fn has_handle(&self) -> bool {
        refs::has_handle(&self.refs())
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
            // What the person reads back: the record's handle (or its
            // primary IRI), never the alias or IRI they typed.
            let shown = refs::show(store, Kind::Record, record.id.iri())?;

            if let MoveOutcome::Ungated = report.outcome {
                println!(
                    "{shown}\t{}\tungated: project {} declares no transition from `{}` to `{}`",
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

            // Every variant by name: a new outcome must be a compile error
            // here, not something a catch-all prints as "moved".
            match report.outcome {
                MoveOutcome::Refused { code } => {
                    println!("REFUSED\t{shown}\tstays `{}`", record.state.as_wire());
                    return Ok(code);
                }
                MoveOutcome::Moved => {
                    println!("{shown}\t{}", state.as_wire());
                }
                MoveOutcome::Ungated => {
                    unreachable!("an ungated move returns above, before any transition is printed")
                }
            }
        }
    }
    Ok(0)
}
