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
    /// Path to the store. CONFINES the command to it: an IRI it does not
    /// hold is refused, not searched for elsewhere. Falls back to $FL_DB
    /// (same), then the project bound in ~/.config/fl/config.toml, then the
    /// XDG data directory.
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

    /// Whether the chosen subcommand names any item by handle rather than
    /// IRI, dispatched to its own `has_handle()` (Fix round 1, item 1).
    fn has_handle(&self) -> bool {
        match self {
            Command::Project(c) => c.has_handle(),
            Command::Gate(c) => c.has_handle(),
            Command::Transition(c) => c.has_handle(),
            Command::Record(c) => c.has_handle(),
            Command::Check(c) => c.has_handle(),
            Command::Finding(c) => c.has_handle(),
            Command::Attempt(c) => c.has_handle(),
            Command::Stats(c) => c.has_handle(),
        }
    }
}

/// Resolve the store path from `--db`, then `$FL_DB`, then the project
/// bound to `cwd` in the user's config, then the XDG data directory, then
/// `~/.local/share` — and whether that tier CONFINES the command to this
/// one store.
///
/// ⚠ Ruling (Fix round 1, item 0): `--db` and `$FL_DB` CONFINE the command
/// to exactly the store they name. An IRI that store does not hold is
/// `NotOwned` naming only that one store — never a search across every
/// store any project happens to be bound to in the config. Only the
/// config-binding and XDG-default tiers search; `choose_store` reads this
/// back to decide whether to expand its candidate list at all.
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
fn db_path(
    explicit: Option<PathBuf>,
    entries: &[config::Entry],
    cwd: &Path,
) -> Result<(PathBuf, bool)> {
    let (path, confined) = if let Some(p) = explicit {
        (p, true)
    } else if let Ok(p) = std::env::var("FL_DB") {
        (PathBuf::from(p), true)
    } else if let Some(p) = config::bound(entries, cwd)? {
        (p, false)
    } else {
        let base = std::env::var("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".local/share")))
            .context("neither --db, $FL_DB, a project bound in the config nor $XDG_DATA_HOME/$HOME is set, so there is nowhere to put the store")?;
        (base.join("fl").join("fl.redb"), false)
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("could not create {}", dir.display()))?;
    }
    Ok((path, confined))
}

/// A full IRI on the command line selects the store that holds it (spec
/// §2.6). Handles resolve only in the bound store, so a command with no IRI
/// uses the bound store. `confined` is true only for `--db`/`$FL_DB` (Fix
/// round 1, item 0, ruling): an explicit store CONFINES the search to
/// itself — the config's other stores are never even considered — so an IRI
/// it does not hold is `NotOwned` naming only that one store.
fn choose_store(
    bound: &Path,
    entries: &[config::Entry],
    iris: &[Iri],
    confined: bool,
) -> Result<PathBuf> {
    if iris.is_empty() {
        return Ok(bound.to_path_buf());
    }
    if !confined {
        let mut candidates = vec![bound.to_path_buf()];
        for e in entries {
            if !candidates.contains(&e.store) {
                candidates.push(e.store.clone());
            }
        }
        // Never create a store while searching: only files that exist are
        // stores.
        candidates.retain(|c| c.exists());
        return choose_among(&candidates, iris);
    }

    // ⚠ Fix round 2, item 2: confined mode has exactly one candidate —
    // `bound` itself — but looking up an IRI must never create a store
    // (`RedbStore::open` calls `Database::create`, which does). If `bound`
    // does not exist yet, every id in `iris` is `NotOwned` by construction;
    // report the first one, naming `bound` as searched, without ever
    // opening it.
    if !bound.exists() {
        return Err(StoreError::NotOwned {
            id: iris[0].clone(),
            searched: vec![bound.display().to_string()],
        }
        .into());
    }
    choose_among(&[bound.to_path_buf()], iris)
}

/// The common search loop, over whatever candidate list the caller already
/// decided on (confined: `[bound]`, unconfined: `bound` plus every
/// configured store that exists).
fn choose_among(candidates: &[PathBuf], iris: &[Iri]) -> Result<PathBuf> {
    let mut chosen: Option<PathBuf> = None;
    for id in iris {
        let mut owners = Vec::new();
        for c in candidates {
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
    let (bound, confined) = db_path(cli.db, &entries, &cwd)?;
    let iris = cli.command.iris();
    let path = choose_store(&bound, &entries, &iris, confined)?;
    // ⚠ Fix round 1, item 1: a handle resolves only in the store it was
    // read from. If an IRI elsewhere in this same command sent the search
    // to a DIFFERENT store than the bound one, a handle alongside it would
    // silently resolve against that other store's numbering instead —
    // refuse rather than guess which store the person meant.
    if path != bound && cli.command.has_handle() {
        bail!(
            "this command names a handle as well as an IRI held by a different store ({}); \
             a handle resolves only in the bound store at {}. Name every item by its full \
             IRI instead of a handle.",
            path.display(),
            bound.display()
        );
    }
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
