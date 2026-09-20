use anyhow::{Result, bail};
use clap::Subcommand;
use fl_core::ids::{ProjectId, RecordId};
use fl_core::model::State;
use fl_core::store::Store;

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

const STATES: &str = "todo, doing, review, done, needs_human";

pub fn run(store: &mut impl Store, cmd: Cmd) -> Result<i32> {
    match cmd {
        Cmd::Add { project, title } => {
            let p = ProjectId(project);
            if store.get_project(p)?.is_none() {
                bail!(
                    "no project with id {project}. Run `flctl project list` to see the ids that exist."
                );
            }
            let id = store.add_record(p, &title)?;
            println!("{id}\t{title}");
        }
        Cmd::List { project } => {
            for r in store.list_records(ProjectId(project))? {
                println!("{}\t{}\t{}", r.id, r.state.as_wire(), r.title);
            }
        }
        Cmd::Move { id, to } => {
            let Some(state) = State::from_wire(&to) else {
                bail!("`{to}` is not a state. Valid states are: {STATES}.");
            };
            let r = RecordId(id);
            if store.get_record(r)?.is_none() {
                bail!(
                    "no record with id {id}. Use `flctl record list --project <id>` to see records that exist."
                );
            }
            store.set_record_state(r, state)?;
            println!("{id}\t{}", state.as_wire());
        }
    }
    Ok(0)
}
