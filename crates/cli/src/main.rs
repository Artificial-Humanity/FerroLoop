mod cmd;
mod config;
mod refs;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use fl_core::{Iri, StoreError};
use fl_store::RedbStore;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(name = "fl", version, about = "Gate an action before it costs you")]
struct Cli {
    /// Path to the store. Falls back to $FL_DB, then the XDG data directory.
    #[arg(long, global = true)]
    db: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    #[command(subcommand)]
    Project(cmd::project::Cmd),
    #[command(subcommand)]
    Gate(cmd::gate::Cmd),
    #[command(subcommand)]
    Transition(cmd::transition::Cmd),
    #[command(subcommand)]
    Record(cmd::record::Cmd),
    /// Evaluate a transition's gates and exit per the check contract.
    Check(cmd::check::Cmd),
    #[command(subcommand)]
    Finding(cmd::finding::Cmd),
    /// Run one adapter attempt against a record and record the outcome.
    Attempt(cmd::attempt::Cmd),
    /// Report what a project's recorded attempts cost.
    Stats(cmd::stats::Cmd),
}

impl Command {
    /// Every item the chosen subcommand names, dispatched to its own
    /// `iris()` (spec §2.6).
    fn iris(&self) -> Vec<Iri> {
        match self {
            Command::Project(c) => c.iris(),
            Command::Gate(c) => c.iris(),
            Command::Transition(c) => c.iris(),
            Command::Record(c) => c.iris(),
            Command::Check(c) => c.iris(),
            Command::Finding(c) => c.iris(),
            Command::Attempt(c) => c.iris(),
            Command::Stats(c) => c.iris(),
        }
    }
}

/// Resolve the store path from `--db`, then `$FL_DB`, then the project
/// bound to `cwd` in the user's config, then the XDG data directory, then
/// `~/.local/share`.
///
/// ⚠ Every tier ends the same way: the parent directory of the resolved path
/// is created if it does not exist. Earlier this only happened on the
/// `$XDG_DATA_HOME`/`$HOME` branch, so setting `$FL_DB` (or `--db`) to a path
/// whose directory did not exist yet fell straight through to a raw redb I/O
/// error — the same class of silent inconsistency this task's actionable-
/// refusal rule exists to close, just moved one layer down into the
/// database open call instead of being refused here. Rather than add a
/// fourth distinct refusal message for that case, every tier now gets the
/// same treatment `$XDG_DATA_HOME`/`$HOME` already had: create the directory
/// that will hold the store. `--db path/to/db` and `$FL_DB=path/to/db`
/// behave identically to each other and to the XDG fallback again.
fn db_path(explicit: Option<PathBuf>, entries: &[config::Entry], cwd: &Path) -> Result<PathBuf> {
    let path = if let Some(p) = explicit {
        p
    } else if let Ok(p) = std::env::var("FL_DB") {
        PathBuf::from(p)
    } else if let Some(p) = config::bound(entries, cwd) {
        p
    } else {
        let base = std::env::var("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".local/share")))
            .context("neither --db, $FL_DB, a project bound in the config nor $XDG_DATA_HOME/$HOME is set, so there is nowhere to put the store")?;
        base.join("fl").join("fl.redb")
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("could not create {}", dir.display()))?;
    }
    Ok(path)
}

/// A full IRI on the command line selects the store that holds it (spec
/// §2.6). Handles resolve only in the bound store, so a command with no IRI
/// uses the bound store.
fn choose_store(bound: &Path, entries: &[config::Entry], iris: &[Iri]) -> Result<PathBuf> {
    if iris.is_empty() {
        return Ok(bound.to_path_buf());
    }
    let mut candidates = vec![bound.to_path_buf()];
    for e in entries {
        if !candidates.contains(&e.store) {
            candidates.push(e.store.clone());
        }
    }
    // Never create a store while searching: only files that exist are stores.
    candidates.retain(|c| c.exists());

    let mut chosen: Option<PathBuf> = None;
    for id in iris {
        let mut owners = Vec::new();
        for c in &candidates {
            // ⚠ A store that cannot be opened is an ERROR here, not a
            // "doesn't have it": skipping it would search less than it says.
            let s = RedbStore::open(c)
                .with_context(|| format!("could not open the store at {}", c.display()))?;
            if s.owns(id)? {
                owners.push(c.clone());
            }
        }
        match owners.as_slice() {
            [] => {
                return Err(StoreError::NotOwned {
                    id: id.clone(),
                    searched: candidates.iter().map(|c| c.display().to_string()).collect(),
                }
                .into());
            }
            [one] => match &chosen {
                Some(prev) if prev != one => bail!(
                    "this command names items in two different stores ({} and {}); name items from one store",
                    prev.display(),
                    one.display()
                ),
                _ => chosen = Some(one.clone()),
            },
            many => bail!(
                "{id} is held by more than one store: {}. Refusing to pick one.",
                many.iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
    Ok(chosen.expect("iris is non-empty"))
}

fn main() {
    let cli = Cli::parse();
    let code = match run(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            2
        }
    };
    std::process::exit(code);
}

fn run(cli: Cli) -> Result<i32> {
    let cwd = std::env::current_dir().context("could not determine the current directory")?;
    let entries = config::load(config::path().as_deref())?;
    let bound = db_path(cli.db, &entries, &cwd)?;
    let iris = cli.command.iris();
    let path = choose_store(&bound, &entries, &iris)?;
    let store = RedbStore::open(&path)
        .with_context(|| format!("could not open the store at {}", path.display()))?;
    match cli.command {
        Command::Project(c) => cmd::project::run(&store, c),
        Command::Gate(c) => cmd::gate::run(&store, c),
        Command::Transition(c) => cmd::transition::run(&store, c),
        Command::Record(c) => cmd::record::run(&store, c),
        Command::Check(c) => cmd::check::run(&store, c),
        Command::Finding(c) => cmd::finding::run(&store, c),
        Command::Attempt(c) => cmd::attempt::run(&store, c),
        Command::Stats(c) => cmd::stats::run(&store, c),
    }
}
