use anyhow::Result;
use clap::Subcommand;
use fl_core::store::Store;

#[derive(Subcommand)]
pub enum Cmd {
    /// Register a project directory.
    Add { path: String },
    /// List registered projects.
    List,
}

pub fn run(store: &mut impl Store, cmd: Cmd) -> Result<i32> {
    match cmd {
        Cmd::Add { path } => {
            let id = store.add_project(&path)?;
            println!("{id}\t{path}");
        }
        Cmd::List => {
            for p in store.list_projects()? {
                println!("{}\t{}", p.id, p.root);
            }
        }
    }
    Ok(0)
}
