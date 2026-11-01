//! `fl store`: look at, tend, serve and query the store behind a project.
//!
//! The commands themselves live in [`fl_store::cli`], which the `ferrostore`
//! binary shares, so the two never drift; see its documentation for what each
//! command reads and writes, and what the private copies of `migrate
//! --dry-run` and `verify` cost. The arguments of `serve`, `get`, `put`,
//! `query`, `inspect` and `bench` are declared there too
//! ([`fl_store::cli::args`]), so both programs take the same flags. This file
//! holds only what is `fl`'s own: the arguments of the older commands, the
//! engine chosen from `$FL_STORE_BACKEND`, the store (the bound one, or a
//! `--connect` URI for `get`, `put` and `query`), the wording of `fl store`,
//! and the private directory in the system temp directory (`$TMPDIR`, else
//! `/tmp`) the copies are made in.

use anyhow::{Result, bail};
use clap::Subcommand;
use fl_store::cli::args::{BenchArgs, GetArgs, InspectArgs, PutArgs, QueryArgs, ServeArgs, emit};
use fl_store::cli::{self as store_cli, Create, Scratch, Target, Wording};
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
    /// Serve the bound store on a Unix socket, or on TCP as ws:// or wss://,
    /// until SIGINT or SIGTERM. Creates the `.fls` when nothing is there.
    Serve {
        #[command(flatten)]
        args: ServeArgs,
    },
    /// Print one document of the bound store as JSON. Exit 1 when there is
    /// none.
    Get {
        #[command(flatten)]
        connect: Connect,
        #[command(flatten)]
        args: GetArgs,
    },
    /// Put one document into the bound store, given as JSON or on stdin (`-`).
    Put {
        #[command(flatten)]
        connect: Connect,
        #[command(flatten)]
        args: PutArgs,
    },
    /// Query a collection of the bound store by field, by BM25, or by vector;
    /// JSON on stdout.
    Query {
        #[command(flatten)]
        connect: Connect,
        #[command(flatten)]
        args: QueryArgs,
    },
    /// The superblocks, versions, extents, live and dead bytes, and the counts
    /// of every collection, column table and vector collection of the bound
    /// `.fls`. Strictly read-only. Exit 1 when the live state cannot be read.
    Inspect {
        #[command(flatten)]
        args: InspectArgs,
    },
    /// Time put, get, scan and vector-search workloads on fresh FerroStore
    /// and redb stores (never the bound one; an existing store only with
    /// --path and --i-know-it-writes): p50, p99 and throughput.
    Bench {
        #[command(flatten)]
        args: BenchArgs,
    },
}

/// Where `get`, `put` and `query` find the store.
#[derive(clap::Args)]
pub struct Connect {
    /// Use the served store at this unix://, ws:// or wss:// URI instead of
    /// the bound store.
    #[arg(long, value_name = "URI")]
    connect: Option<String>,
}

impl Connect {
    /// The bound store `path` (on the ferro engine, see [`on_ferro`]), or the
    /// `--connect` URI.
    fn target(&self, path: &Path, what: &str) -> Result<Target> {
        match &self.connect {
            None => {
                on_ferro(path, what)?;
                Ok(Target::Path(path.to_path_buf()))
            }
            Some(uri) => match Target::parse(uri)? {
                Target::Path(_) => bail!(
                    "--connect takes a unix://, ws:// or wss:// URI; a path is given with --db"
                ),
                target => Ok(target),
            },
        }
    }
}

/// The engine `path` selects under the process environment.
fn engine(path: &Path) -> Result<BackendKind> {
    Ok(choose_backend(
        path,
        std::env::var(BACKEND_ENV).ok().as_deref(),
    )?)
}

/// Refuse `fl store <what>` on the bound store `path` when the environment
/// puts it on redb (`FL_STORE_BACKEND=redb`, the rollback switch): those
/// commands work on its `.fls`, which every other `fl` command then leaves
/// alone, so a write there would split the store.
fn on_ferro(path: &Path, what: &str) -> Result<()> {
    if engine(path)? == BackendKind::Redb {
        bail!(
            "the bound store is on redb ({BACKEND_ENV}=redb); `fl store {what}` works on \
             FerroStore (put, serve, get and query all do): unset {BACKEND_ENV}, or use --connect"
        );
    }
    Ok(())
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
        Cmd::Serve { args } => {
            on_ferro(path, "serve")?;
            Ok(args.run(path, Create::WithParents, &FL)?)
        }
        Cmd::Get { connect, args } => printed(args.run(&connect.target(path, "get")?, &FL)?),
        Cmd::Put { connect, args } => {
            printed(args.run(&connect.target(path, "put")?, Create::WithParents, &FL)?)
        }
        Cmd::Query { connect, args } => printed(args.run(&connect.target(path, "query")?, &FL)?),
        Cmd::Inspect { args } => printed(args.run(path, &FL)?),
        Cmd::Bench { args } => printed(args.run(scratch_dir, &FL)?),
    }
}

/// Print a command's text; its exit code.
fn printed((out, code): (store_cli::Output, i32)) -> Result<i32> {
    emit(&out);
    Ok(code)
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
