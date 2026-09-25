use crate::refs;
use anyhow::{Result, bail};
use clap::Subcommand;
use fl_core::Kind;
use fl_core::store::Catalog;
use fl_store::RedbStore;

#[derive(Subcommand)]
pub enum Cmd {
    /// Register a project directory.
    Add { path: String },
    /// List registered projects.
    List,
}

pub fn run(store: &RedbStore, cmd: Cmd) -> Result<i32> {
    match cmd {
        Cmd::Add { path } => {
            // ⚠ This used to store the string unexamined, so a typo became a
            // registered project and surfaced later as a git error from
            // `gate add` — a refusal about the wrong thing, at the wrong
            // time, naming neither the path nor the mistake.
            let given = std::path::Path::new(&path);
            if !given.is_dir() {
                bail!(
                    "`{path}` does not exist as a directory, so it cannot be a project root. \
                     A project is a git working tree on this machine."
                );
            }
            // Store an absolute, symlink-resolved root: gates resolve their
            // population against it from whatever directory the CLI is run
            // in, so a relative root would name a different tree each time.
            let root = given.canonicalize().map_err(|e| {
                anyhow::anyhow!("`{path}` could not be resolved to a real path: {e}")
            })?;
            let root = root.display().to_string();
            fl_exec::git::Git::head(std::path::Path::new(&root)).map_err(|e| {
                anyhow::anyhow!(
                    "`{root}` is not a git working tree: {e}. A project must be one, because a \
                     gate's provenance is a commit — there has to be a HEAD to stamp it against."
                )
            })?;
            let id = store.add_project(&root)?;
            println!("{}\t{root}", refs::show(store, Kind::Project, id.iri())?);
        }
        Cmd::List => {
            for p in store.list_projects()? {
                println!(
                    "{}\t{}",
                    refs::show(store, Kind::Project, p.id.iri())?,
                    p.root
                );
            }
        }
    }
    Ok(0)
}
