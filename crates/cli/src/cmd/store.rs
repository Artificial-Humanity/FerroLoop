//! `fl store`: look at, and tend, the engine file behind a store.
//!
//! Only what the engines do today is here. Not yet present, because each
//! needs engine work that has not landed: `bench` and `serve`.
//!
//! The commands themselves live in [`fl_store::cli`], which the `ferrostore`
//! binary shares, so the two never drift; see its documentation for what each
//! command reads and writes, and what the private copies of `migrate
//! --dry-run` and `verify` cost. This file holds only what is `fl`'s own: the
//! arguments, the engine chosen from `$FL_STORE_BACKEND`, the wording of `fl
//! store`, and the private directory in the system temp directory (`$TMPDIR`,
//! else `/tmp`) the copies are made in.

use anyhow::Result;
use clap::Subcommand;
use fl_store::cli::{self as store_cli, Scratch, Wording};
use fl_store::{BACKEND_ENV, BackendKind, choose_backend};
use std::path::Path;

/// How `fl store` names itself in what the commands print.
const FL: Wording<'static> = Wording {
    command: "fl store",
    redb_path_hint: "the bound store, or --db <file>.redb",
};

#[derive(Subcommand)]
pub enum Cmd {
    /// The engine, files and format version of the bound store.
    Status {
        /// Print one JSON object instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Fold a FerroStore write-ahead log into the store file. redb has none.
    Checkpoint,
    /// Rewrite a FerroStore as one snapshot and an empty log, giving back the
    /// space earlier checkpoints left dead; prints the file and dead bytes
    /// before and after. Applies to the `.fls` engine only: on redb it says so
    /// and does nothing. Never creates or migrates a store.
    Compact {
        /// Print one JSON object instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Copy the bound `.redb` store into a new `.fls`, verified; the `.redb` is
    /// kept, read-only (0400). Exit 1 when the copy does not verify.
    Migrate {
        /// Migrate a private copy, report, and delete it; the real files are
        /// not written. The copy is made in the system temp directory ($TMPDIR,
        /// else /tmp, often RAM-backed) and needs room for two files the size
        /// of the .redb (the command checks, and refuses when short); the
        /// .redb is locked against writers while it is copied.
        #[arg(long)]
        dry_run: bool,
        /// Print one JSON object instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Compare an existing `.redb` and `.fls` pair, table by table and view by
    /// view. Exit 1 when they differ. Works on a private copy of the .redb in
    /// the system temp directory ($TMPDIR, else /tmp, often RAM-backed; the
    /// command checks for room and refuses when short); the .redb is locked
    /// against writers while it is copied.
    Verify {
        /// Print one JSON object instead of text.
        #[arg(long)]
        json: bool,
    },
}

/// The engine `path` selects under the process environment.
fn engine(path: &Path) -> Result<BackendKind> {
    Ok(choose_backend(
        path,
        std::env::var(BACKEND_ENV).ok().as_deref(),
    )?)
}

pub fn run(path: &Path, cmd: Cmd) -> Result<i32> {
    match cmd {
        Cmd::Status { json } => {
            let report = store_cli::status(path, engine(path)?);
            if json {
                println!("{}", store_cli::to_json(&report)?);
            } else {
                print!("{}", report.text(&FL));
            }
            Ok(0)
        }
        Cmd::Checkpoint => {
            let report = store_cli::checkpoint(path, engine(path)?, &FL)?;
            print!("{}", report.text());
            Ok(0)
        }
        Cmd::Compact { json } => {
            let report = store_cli::compact(path, engine(path)?, &FL)?;
            if json {
                println!("{}", store_cli::to_json(&report)?);
            } else {
                print!("{}", report.text());
            }
            Ok(0)
        }
        Cmd::Migrate { dry_run, json } => {
            let pair = store_cli::migrate_pair(path, &FL)?;
            let report = store_cli::migrate(&pair, dry_run, scratch_dir)?;
            if json {
                println!("{}", store_cli::to_json(&report)?);
            } else {
                emit(&report.text(&FL));
            }
            Ok(report.exit_code())
        }
        Cmd::Verify { json } => {
            let pair = store_cli::verify_pair(path, &FL)?;
            let report = store_cli::verify(&pair, scratch_dir)?;
            if json {
                println!("{}", store_cli::to_json(&report)?);
            } else {
                emit(&report.text());
            }
            Ok(report.exit_code())
        }
    }
}

/// A command's text: its stdout, then its stderr.
fn emit(out: &store_cli::Output) {
    print!("{}", out.stdout);
    eprint!("{}", out.stderr);
}

/// A private (0700) directory in the system temp directory, removed by
/// [`Scratch::close`] (or on drop, when a command fails first).
struct TempScratch(tempfile::TempDir);

impl Scratch for TempScratch {
    fn path(&self) -> &Path {
        self.0.path()
    }

    fn close(self) -> std::io::Result<()> {
        self.0.close()
    }
}

fn scratch_dir() -> store_cli::Result<TempScratch> {
    tempfile::Builder::new()
        .prefix("fl-store-")
        .tempdir()
        .map(TempScratch)
        .map_err(|e| store_cli::Error::Context {
            context: "could not make a private directory in the system temp directory".into(),
            source: Box::new(e),
        })
}
