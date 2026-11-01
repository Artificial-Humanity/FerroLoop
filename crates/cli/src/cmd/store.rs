//! `fl store`: look at, and tend, the engine file behind a store.
//!
//! Only what the engines do today is here. Not yet present, because each
//! needs engine work that has not landed: `compact`, `bench` and `serve`.
//!
//! No command creates a store: a path that names nothing is reported
//! (`status`) or refused (`checkpoint`, `migrate`, `verify`), never opened.
//! `migrate` creates the one `.fls` it is asked to; `migrate --dry-run` and
//! `verify` work on a private copy of the `.redb` and never write the real
//! files (opening a redb file rewrites its internal pages, so they do not
//! open the real one).

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use fl_store::migrate::{ParityReport, TableParity, migrate_verified, verify_redb_vs_ferrostore};
use fl_store::{
    BACKEND_ENV, BackendKind, FerroStore, RedbSharedLock, RedbStore, StoreOptions, choose_backend,
    is_redb_busy, physical_path,
};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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
    /// Copy the bound `.redb` store into a new `.fls`, verified; the `.redb` is
    /// kept, read-only (0400). Exit 1 when the copy does not verify.
    Migrate {
        /// Migrate a private copy in the system temp directory, report, and
        /// delete it. The real files are not written.
        #[arg(long)]
        dry_run: bool,
        /// Print one JSON object instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Compare an existing `.redb` and `.fls` pair, table by table and view by
    /// view. Exit 1 when they differ.
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
        Cmd::Status { json } => status(path, json),
        Cmd::Checkpoint => checkpoint(path),
        Cmd::Migrate { dry_run, json } => migrate(path, dry_run, json),
        Cmd::Verify { json } => verify(path, json),
    }
}

fn status(path: &Path, json_out: bool) -> Result<i32> {
    let kind = engine(path)?;
    let physical = physical_path(path, kind);
    let meta = std::fs::metadata(&physical).ok();
    let exists = meta.is_some();
    let size = meta.as_ref().map(std::fs::Metadata::len);
    // A lone `.redb` beside a ferro selection: `open_store` migrates it, so say so.
    let redb_beside = kind == BackendKind::Ferro && !exists && path != physical && path.exists();

    // FerroStore has no format version to read yet; redb's is read from the
    // file, which is opened only when it exists.
    let (format_version, format_note): (Value, Option<String>) = match (kind, exists) {
        (BackendKind::Ferro, _) => (json!("n/a"), Some("not available on ferro yet".into())),
        (BackendKind::Redb, false) => (Value::Null, None),
        // A 0-byte file is not a redb database yet; opening it would
        // initialise it, so report it without opening.
        (BackendKind::Redb, true) if size == Some(0) => (Value::Null, Some("empty file".into())),
        (BackendKind::Redb, true) => {
            // No retries: a store another process holds is reported, not waited on.
            let options = StoreOptions {
                retries: 0,
                ..StoreOptions::default()
            };
            match fl_store::open_store_with(path, &options).and_then(|s| s.format_version()) {
                Ok(Some(v)) => (json!(v), None),
                Ok(None) => (Value::Null, Some("the file records none".into())),
                Err(e) => (Value::Null, Some(format!("could not read it: {e}"))),
            }
        }
    };

    if json_out {
        let mut out = json!({
            "engine": kind.as_str(),
            "logical_path": path.display().to_string(),
            "physical_path": physical.display().to_string(),
            "exists": exists,
            "size_bytes": size,
            "format_version": format_version,
        });
        if redb_beside {
            out["redb_beside_ferro"] = json!(true);
        }
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(0);
    }
    println!("engine:         {}", kind.as_str());
    println!("logical path:   {}", path.display());
    println!("physical path:  {}", physical.display());
    println!("exists:         {}", if exists { "yes" } else { "no" });
    match size {
        Some(n) => println!("size:           {n} bytes"),
        None => println!("size:           -"),
    }
    let shown = match &format_version {
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        _ => "-".to_string(),
    };
    match format_note {
        Some(note) => println!("format version: {shown} ({note})"),
        None => println!("format version: {shown}"),
    }
    if redb_beside {
        println!(
            "note: {} holds a redb store and the selected engine is ferro; \
             the next command that opens it migrates it (or run `fl store migrate`)",
            path.display()
        );
    }
    Ok(0)
}

fn checkpoint(path: &Path) -> Result<i32> {
    let kind = engine(path)?;
    if kind == BackendKind::Redb {
        println!("redb has no checkpoint; nothing to do");
        return Ok(0);
    }
    let physical = physical_path(path, kind);
    // The physical file, not `store_exists`: a lone `.redb` would be migrated
    // by the open below, and a checkpoint must not do that.
    if !physical.exists() {
        bail!(
            "no store at {}; `fl store checkpoint` does not create one",
            physical.display()
        );
    }
    let store = fl_store::open_store(path)?;
    let Some(ferro) = store.as_ferro() else {
        bail!("the store at {} is not a FerroStore", physical.display());
    };
    ferro.checkpoint()?;
    println!("checkpointed {}", physical.display());
    Ok(0)
}

/// The `.redb` and `.fls` of the store named `path`, which must name the
/// `.redb`: a path that is itself a `.fls` names no redb to migrate or compare.
fn pair(path: &Path, what: &str) -> Result<(PathBuf, PathBuf)> {
    let fls = physical_path(path, BackendKind::Ferro);
    if fls == path {
        bail!(
            "{} names a .fls file; `fl store {what}` takes the store's .redb path (the bound \
             store, or --db <file>.redb)",
            path.display()
        );
    }
    Ok((path.to_path_buf(), fls))
}

/// A file or directory that exists, symlinks not followed: a dangling link is
/// something too.
fn present(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

fn require_redb(redb: &Path, what: &str) -> Result<()> {
    if !redb.is_file() {
        bail!(
            "no redb store at {}; `fl store {what}` does not create one",
            redb.display()
        );
    }
    Ok(())
}

const BUSY: &str = "is held by another process (a hub or daemon still running on redb?); stop it \
                    and run this again";

/// Refuse in words when another process holds the redb; any other failure to
/// read it is reported as it is. The lock is released when the guard drops.
fn lock_redb(redb: &Path) -> Result<RedbSharedLock> {
    RedbSharedLock::acquire(redb).map_err(|e| {
        if is_redb_busy(&e) {
            anyhow::anyhow!("{} {BUSY}", redb.display())
        } else {
            anyhow::Error::new(e).context(format!("could not read {}", redb.display()))
        }
    })
}

/// A private (0700) directory in the system temp directory, removed on drop.
fn scratch_dir() -> Result<tempfile::TempDir> {
    tempfile::Builder::new()
        .prefix("fl-store-")
        .tempdir()
        .context("could not make a private directory in the system temp directory")
}

/// A writable copy of `redb` as `<dir>/<name>`, made while this process holds
/// a shared lock on the original, so no writer changes it mid-copy. The
/// original is only read.
fn copy_redb(redb: &Path, dir: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let _lock = lock_redb(redb)?;
    let name = redb
        .file_name()
        .with_context(|| format!("{} has no file name", redb.display()))?;
    let copy = dir.join(name);
    std::fs::copy(redb, &copy)
        .with_context(|| format!("could not copy {} to {}", redb.display(), copy.display()))?;
    std::fs::set_permissions(&copy, std::fs::Permissions::from_mode(0o600))?;
    Ok(copy)
}

fn migrate(path: &Path, dry_run: bool, json_out: bool) -> Result<i32> {
    let (redb, fls) = pair(path, "migrate")?;
    require_redb(&redb, "migrate")?;
    if present(&fls) {
        bail!(
            "{} already exists: this store is already migrated (`fl store verify` compares the \
             pair); `fl store migrate` only creates a new .fls",
            fls.display()
        );
    }

    let started = Instant::now();
    let (report, scratch) = if dry_run {
        let dir = scratch_dir()?;
        let copy = copy_redb(&redb, dir.path())?;
        let copy_fls = physical_path(&copy, BackendKind::Ferro);
        let report = migrate_verified(&copy, &copy_fls)
            .with_context(|| format!("dry run on a copy of {}", redb.display()))?;
        (report, Some(dir))
    } else {
        // Ask once, so a held redb is refused now and not after the
        // migration's own wait. Only "held" is refused here; a file that
        // cannot be read for another reason is the migration's to report (it
        // also repairs a file left by a crash). The guard is released before
        // the migration opens the file.
        if let Err(e) = RedbSharedLock::acquire(&redb)
            && is_redb_busy(&e)
        {
            bail!("{} {BUSY}", redb.display());
        }
        let report = migrate_verified(&redb, &fls).map_err(|e| {
            if is_redb_busy(&e) {
                anyhow::anyhow!("{} {BUSY}", redb.display())
            } else {
                anyhow::Error::new(e).context(format!("could not migrate {}", redb.display()))
            }
        })?;
        (report, None)
    };
    let elapsed = started.elapsed();
    // Delete the scratch copy before reporting.
    if let Some(dir) = scratch {
        dir.close().context("could not remove the dry-run copy")?;
    }

    let clean = report.is_clean();
    let kept_read_only = !dry_run && clean;
    if json_out {
        let mut out = report_json(&report, Some(elapsed));
        out["command"] = json!("migrate");
        out["dry_run"] = json!(dry_run);
        out["redb"] = json!(redb.display().to_string());
        out["fls"] = json!((!dry_run && clean).then(|| fls.display().to_string()));
        out["redb_kept_read_only"] = json!(kept_read_only);
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(i32::from(!clean));
    }
    if dry_run {
        println!(
            "dry run: migrated a private copy of {}; no real file was written",
            redb.display()
        );
    } else {
        println!("migrate {} -> {}", redb.display(), fls.display());
    }
    print_report(&report);
    println!("time: {:.2}s", elapsed.as_secs_f64());
    if clean {
        if dry_run {
            println!("clean: a real `fl store migrate` would succeed; the copy was deleted");
        } else {
            println!(
                "clean: {} is the store; {} is kept, read-only (0400)",
                fls.display(),
                redb.display()
            );
        }
        return Ok(0);
    }
    print_mismatches(&report);
    eprintln!(
        "not clean: no {} was created and {} was not made read-only",
        fls.display(),
        redb.display()
    );
    Ok(1)
}

fn verify(path: &Path, json_out: bool) -> Result<i32> {
    let (redb, fls) = pair(path, "verify")?;
    if !redb.is_file() {
        bail!(
            "no redb store at {}; `fl store verify` compares a .redb with its .fls",
            redb.display()
        );
    }
    if !fls.is_file() {
        bail!(
            "no FerroStore at {}; `fl store verify` compares a .redb with its .fls \
             (`fl store migrate` makes one)",
            fls.display()
        );
    }
    let started = Instant::now();
    // The redb is compared as a private copy: opening it for the comparison
    // would rewrite the real file, which a migration left read-only.
    let scratch = scratch_dir()?;
    let copy = copy_redb(&redb, scratch.path())?;
    let report = {
        let src = RedbStore::open_with_retries(&copy, 0)
            .with_context(|| format!("could not open a copy of {}", redb.display()))?;
        let dst =
            FerroStore::open(&fls).with_context(|| format!("could not open {}", fls.display()))?;
        verify_redb_vs_ferrostore(&src, &dst).with_context(|| {
            format!(
                "could not compare {} with {}",
                redb.display(),
                fls.display()
            )
        })?
    };
    scratch
        .close()
        .context("could not remove the verify copy")?;
    let elapsed = started.elapsed();

    let clean = report.is_clean();
    if json_out {
        let mut out = report_json(&report, Some(elapsed));
        out["command"] = json!("verify");
        out["redb"] = json!(redb.display().to_string());
        out["fls"] = json!(fls.display().to_string());
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(i32::from(!clean));
    }
    println!("verify {} against {}", redb.display(), fls.display());
    print_report(&report);
    println!("time: {:.2}s", elapsed.as_secs_f64());
    if clean {
        println!("clean: every table and view matches");
        return Ok(0);
    }
    print_mismatches(&report);
    Ok(1)
}

fn hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn parity_json(p: &TableParity) -> Value {
    json!({
        "name": p.table,
        "redb_rows": p.redb_rows,
        "ferro_rows": p.ferro_rows,
        "redb_hash": hex(&p.redb_hash),
        "ferro_hash": hex(&p.ferro_hash),
        "clean": p.is_clean(),
    })
}

fn report_json(report: &ParityReport, elapsed: Option<Duration>) -> Value {
    json!({
        "clean": report.is_clean(),
        "elapsed_ms": elapsed.map(|d| d.as_millis() as u64),
        "tables": report.tables.iter().map(parity_json).collect::<Vec<_>>(),
        "trait_views": report.trait_views.iter().map(parity_json).collect::<Vec<_>>(),
        "mismatches": report.mismatches().iter().map(|p| p.table.clone()).collect::<Vec<_>>(),
    })
}

/// Per-table rows, then a line for the views.
fn print_report(report: &ParityReport) {
    let width = report
        .tables
        .iter()
        .map(|p| p.table.len())
        .max()
        .unwrap_or(5)
        .max(5);
    println!(
        "{:<width$}  {:>10}  {:>10}  status",
        "table", "redb", "ferro"
    );
    for p in &report.tables {
        println!(
            "{:<width$}  {:>10}  {:>10}  {}",
            p.table,
            p.redb_rows,
            p.ferro_rows,
            if p.is_clean() { "ok" } else { "MISMATCH" }
        );
    }
    let bad = report.trait_views.iter().filter(|p| !p.is_clean()).count();
    println!(
        "views: {} compared, {} differ",
        report.trait_views.len(),
        bad
    );
}

fn print_mismatches(report: &ParityReport) {
    eprintln!("mismatches:");
    for p in report.mismatches() {
        eprintln!(
            "  {}: redb {} rows {}, ferro {} rows {}",
            p.table,
            p.redb_rows,
            &hex(&p.redb_hash)[..12],
            p.ferro_rows,
            &hex(&p.ferro_hash)[..12],
        );
    }
}
