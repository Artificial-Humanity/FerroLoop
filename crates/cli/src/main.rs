mod cmd;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use fl_store::RedbStore;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "flctl", version, about = "Gate an action before it costs you")]
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
}

/// Resolve the store path from `--db`, then `$FL_DB`, then the XDG data
/// directory, then `~/.local/share`.
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
fn db_path(explicit: Option<PathBuf>) -> Result<PathBuf> {
    let path = if let Some(p) = explicit {
        p
    } else if let Ok(p) = std::env::var("FL_DB") {
        PathBuf::from(p)
    } else {
        let base = std::env::var("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".local/share")))
            .context("neither --db, $FL_DB, $XDG_DATA_HOME nor $HOME is set, so there is nowhere to put the store")?;
        base.join("fl").join("fl.redb")
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("could not create {}", dir.display()))?;
    }
    Ok(path)
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
    let path = db_path(cli.db)?;
    let mut store = RedbStore::open(&path)
        .with_context(|| format!("could not open the store at {}", path.display()))?;
    match cli.command {
        Command::Project(c) => cmd::project::run(&mut store, c),
        Command::Gate(c) => cmd::gate::run(&mut store, c),
        Command::Transition(c) => cmd::transition::run(&mut store, c),
        Command::Record(c) => cmd::record::run(&mut store, c),
    }
}
