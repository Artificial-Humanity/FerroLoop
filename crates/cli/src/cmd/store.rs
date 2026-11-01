//! `fl store`: look at, and tend, the engine file behind a store.
//!
//! Only what the engines do today is here. Not yet present, because each
//! needs engine work that has not landed: `bench` and `serve`.
//!
//! No command creates a store: a path that names nothing is reported
//! (`status`) or refused (`checkpoint`, `migrate`, `verify`), never opened.
//! `migrate` creates the one `.fls` it is asked to; `migrate --dry-run` and
//! `verify` work on a private copy of the `.redb` and never write the real
//! files (opening a redb file rewrites its internal pages, so they do not
//! open the real one).
//!
//! Cost of the private copy: it is made in the system temp directory
//! (`$TMPDIR`, else `/tmp`, which is often RAM-backed), and it is made while
//! this process holds a shared lock on the `.redb`, which stops any writer
//! (a hub or daemon on redb) until the copy is done. `verify` makes one copy
//! of the redb; `migrate --dry-run` makes the copy and then a `.fls` of the
//! same size. Each command first checks that the filesystem has room (the
//! size of each file plus 10%) and refuses, naming `$TMPDIR`, when it does not.
//! A real `migrate` copies nothing: it writes the new `.fls` beside the
//! `.redb` and takes the same lock as an automatic migration, so a ferro
//! process that opens the store meanwhile waits for it.

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use fl_store::migrate::{ParityReport, TableParity, migrate_verified, verify_redb_vs_ferrostore};
use fl_store::{
    BACKEND_ENV, BackendKind, COMPACT_FERRO_ONLY, FerroStore, RedbSharedLock, RedbStore,
    StoreOptions, choose_backend, ferro_stats_at, is_redb_busy, migrate_locked, physical_path,
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
        Cmd::Status { json } => status(path, json),
        Cmd::Checkpoint => checkpoint(path),
        Cmd::Compact { json } => compact(path, json),
        Cmd::Migrate { dry_run, json } => migrate(path, dry_run, json),
        Cmd::Verify { json } => verify(path, json),
    }
}

fn status(path: &Path, json_out: bool) -> Result<i32> {
    let kind = engine(path)?;
    let physical = physical_path(path, kind);
    let meta = std::fs::metadata(&physical).ok();
    let exists = meta.is_some();
    let mut size = meta.as_ref().map(std::fs::Metadata::len);
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

    // FerroStore knows which bytes of its file are live and which an earlier
    // checkpoint left dead; redb does not say, and a missing store has none.
    // Read with `ferro_stats_at`: read-only, no lock, nothing created or
    // initialised, so status is safe beside a live store and on a 0400 file,
    // and a file that is not a store yet is reported, not turned into one.
    // The size comes from the same read, so the three figures agree.
    let (live_bytes, dead_bytes, sizes_note) = match (kind, exists) {
        (BackendKind::Ferro, true) => match ferro_stats_at(&physical) {
            Ok(st) => {
                size = Some(st.file_bytes);
                (st.live_bytes, st.dead_bytes, None)
            }
            Err(e) => (None, None, Some(format!("could not read them: {e}"))),
        },
        _ => (None, None, None),
    };

    if json_out {
        let mut out = json!({
            "engine": kind.as_str(),
            "logical_path": path.display().to_string(),
            "physical_path": physical.display().to_string(),
            "exists": exists,
            "size_bytes": size,
            "format_version": format_version,
            "live_bytes": live_bytes,
            "dead_bytes": dead_bytes,
        });
        if redb_beside {
            out["redb_beside_ferro"] = json!(true);
        }
        if let Some(note) = &sizes_note {
            out["sizes_note"] = json!(note);
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
    if kind == BackendKind::Ferro && exists {
        match (live_bytes, dead_bytes) {
            (Some(live), Some(dead)) => {
                println!("live:           {live} bytes");
                println!("dead:           {dead} bytes (`fl store compact` gives them back)");
            }
            _ => println!(
                "live/dead:      - ({})",
                sizes_note.as_deref().unwrap_or("unavailable")
            ),
        }
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

/// `fl store compact`. Mirrors `checkpoint`: redb is reported and left alone,
/// a missing `.fls` is refused (a lone `.redb` is not migrated just to be
/// compacted), and the physical file is the one opened.
fn compact(path: &Path, json_out: bool) -> Result<i32> {
    let kind = engine(path)?;
    let physical = physical_path(path, kind);
    if kind == BackendKind::Redb {
        let msg = format!("{COMPACT_FERRO_ONLY}, nothing to do");
        if json_out {
            let out = json!({
                "command": "compact",
                "engine": kind.as_str(),
                "physical_path": physical.display().to_string(),
                "compacted": false,
                "reason": msg,
            });
            println!("{}", serde_json::to_string_pretty(&out)?);
        } else {
            println!("{msg}");
        }
        return Ok(0);
    }
    if !physical.exists() {
        bail!(
            "no store at {}; `fl store compact` does not create one",
            physical.display()
        );
    }
    let store = fl_store::open_store(path)?;
    if store.as_ferro().is_none() {
        bail!("the store at {} is not a FerroStore", physical.display());
    }
    let before = store.stats()?;
    store.compact()?;
    // The store is compacted from here on. A failed second reading is not a
    // failed compaction: report the compaction and leave `after` unknown.
    let after = store.stats().ok();
    if json_out {
        let out = compact_json(&physical, &before, after.as_ref());
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        println!("{}", compact_text(&physical, &before, after.as_ref()));
    }
    Ok(0)
}

/// The `--json` report of a compaction that ran. `after` is `null` when the
/// reading after it failed.
fn compact_json(
    physical: &Path,
    before: &fl_store::AnyStats,
    after: Option<&fl_store::AnyStats>,
) -> Value {
    let size = |s: &fl_store::AnyStats| {
        json!({
            "file_bytes": s.file_bytes,
            "live_bytes": s.live_bytes,
            "dead_bytes": s.dead_bytes,
        })
    };
    json!({
        "command": "compact",
        "engine": BackendKind::Ferro.as_str(),
        "physical_path": physical.display().to_string(),
        "compacted": true,
        "before": size(before),
        "after": after.map_or(Value::Null, size),
    })
}

/// The text report of a compaction that ran; `-` where `after` is unknown.
fn compact_text(
    physical: &Path,
    before: &fl_store::AnyStats,
    after: Option<&fl_store::AnyStats>,
) -> String {
    let dash = || "-".to_string();
    let dead = |s: &fl_store::AnyStats| s.dead_bytes.map_or_else(dash, |d| d.to_string());
    format!(
        "compacted {}\nfile bytes: {} -> {}\ndead bytes: {} -> {}",
        physical.display(),
        before.file_bytes,
        after.map_or_else(dash, |a| a.file_bytes.to_string()),
        dead(before),
        after.map_or_else(dash, dead),
    )
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

/// The line printed when a clean migration could not make the kept redb read-only.
fn still_writable_notice(fls: &Path, redb: &Path) -> String {
    format!(
        "clean: {} is the store; {} is kept but is still writable (its mode could not be \
         set to 0400): make it read-only with chmod 0400, or a write to it is lost",
        fls.display(),
        redb.display()
    )
}

/// Refuse, in words, when `free` bytes cannot hold `copies` files of `size`
/// bytes plus 10% each.
fn ensure_room(dir: &Path, free: u64, size: u64, copies: u64) -> Result<()> {
    let need = copies.saturating_mul(size.saturating_add(size / 10));
    if free < need {
        bail!(
            "not enough free space in {} for the private copy: {free} bytes free, {need} needed \
             ({copies} file(s) of {size} bytes, plus 10%); point $TMPDIR at a bigger filesystem",
            dir.display()
        );
    }
    Ok(())
}

/// A writable copy of `redb` as `<dir>/<name>`, made while this process holds
/// a shared lock on the original, so no writer changes it mid-copy (a hub or
/// daemon on redb waits for the copy). The original is only read. `copies` is
/// how many files of that size the command will keep in `dir` in all; the
/// filesystem must have room for them, plus 10%, or nothing is copied.
fn copy_redb(redb: &Path, dir: &Path, copies: u64) -> Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let _lock = lock_redb(redb)?;
    let size = std::fs::metadata(redb)
        .with_context(|| format!("could not read {}", redb.display()))?
        .len();
    // A filesystem that cannot report its free space is not a reason to refuse.
    if let Ok(free) = fl_store::private::available_bytes(dir) {
        ensure_room(dir, free, size, copies)?;
    }
    let name = redb
        .file_name()
        .with_context(|| format!("{} has no file name", redb.display()))?;
    let copy = dir.join(name);
    std::fs::copy(redb, &copy)
        .with_context(|| format!("could not copy {} to {}", redb.display(), copy.display()))?;
    std::fs::set_permissions(&copy, std::fs::Permissions::from_mode(0o600))?;
    Ok(copy)
}

/// Whether nobody may write `path` by its mode bits (what `stat` says; root
/// ignores the bits, which is why the mode is read, not assumed).
fn mode_is_read_only(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o222 == 0)
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
        let copy = copy_redb(&redb, dir.path(), 2)?;
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
        let report = migrate_locked(&redb, &fls).map_err(|e| {
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
    // The file's real mode: a chmod that failed leaves it writable.
    let kept_read_only = !dry_run && clean && mode_is_read_only(&redb);
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
        } else if kept_read_only {
            println!(
                "clean: {} is the store; {} is kept, read-only (0400)",
                fls.display(),
                redb.display()
            );
        } else {
            println!("{}", still_writable_notice(&fls, &redb));
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
    let copy = copy_redb(&redb, scratch.path(), 1)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn ensure_room_needs_the_size_plus_ten_percent_per_copy() {
        let dir = Path::new("/scratch");
        // 1000 bytes + 10% = 1100 per copy.
        assert!(ensure_room(dir, 1100, 1000, 1).is_ok());
        assert!(ensure_room(dir, 2200, 1000, 2).is_ok());
        let err = ensure_room(dir, 1099, 1000, 1).unwrap_err().to_string();
        assert!(err.contains("not enough free space in /scratch"), "{err}");
        assert!(err.contains("1100 needed"), "{err}");
        assert!(err.contains("$TMPDIR"), "{err}");
        assert!(!err.contains("  "), "a run of spaces in the message: {err}");
        assert!(
            err.contains("1100 needed (1 file(s) of 1000 bytes, plus 10%)"),
            "{err}"
        );
        let err = ensure_room(dir, 2199, 1000, 2).unwrap_err().to_string();
        assert!(err.contains("2200 needed"), "{err}");
        // An empty file needs nothing; sizes near u64::MAX do not wrap.
        assert!(ensure_room(dir, 0, 0, 2).is_ok());
        assert!(ensure_room(dir, u64::MAX - 1, u64::MAX, 2).is_err());
    }

    #[test]
    fn the_still_writable_notice_has_no_runs_of_spaces() {
        let text = still_writable_notice(Path::new("/d/s.fls"), Path::new("/d/s.redb"));
        assert!(
            !text.contains("  "),
            "a run of spaces in the message: {text}"
        );
        assert!(
            text.contains("(its mode could not be set to 0400): make it read-only"),
            "{text}"
        );
        assert!(
            text.starts_with("clean: /d/s.fls is the store; /d/s.redb is kept"),
            "{text}"
        );
    }

    #[test]
    fn copy_redb_checks_the_scratch_space_before_it_copies() {
        let src = tempfile::tempdir().unwrap();
        let redb = src.path().join("s.redb");
        drop(RedbStore::open(&redb).unwrap());
        let scratch = tempfile::tempdir().unwrap();
        // Room enough on a test machine for one copy: it is made, writable.
        let copy = copy_redb(&redb, scratch.path(), 1).unwrap();
        assert_eq!(
            std::fs::metadata(&copy).unwrap().permissions().mode() & 0o777,
            0o600
        );
        // More copies than any filesystem has room for: refused, nothing copied.
        let other = tempfile::tempdir().unwrap();
        let err = copy_redb(&redb, other.path(), u64::MAX / 4)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not enough free space"), "{err}");
        assert_eq!(std::fs::read_dir(other.path()).unwrap().count(), 0);
    }

    #[test]
    fn the_kept_redb_is_read_only_by_its_real_mode() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("s.redb");
        std::fs::write(&file, b"x").unwrap();
        for (mode, read_only) in [(0o600, false), (0o644, false), (0o400, true), (0o444, true)] {
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(mode)).unwrap();
            assert_eq!(mode_is_read_only(&file), read_only, "{mode:o}");
        }
        assert!(!mode_is_read_only(&dir.path().join("missing")));
    }

    use fl_store::AnyStats;

    fn stats(file: u64, dead: u64) -> AnyStats {
        AnyStats {
            engine: BackendKind::Ferro,
            file_bytes: file,
            live_bytes: Some(file - dead),
            dead_bytes: Some(dead),
        }
    }

    #[test]
    fn a_compaction_whose_second_reading_failed_is_reported_with_after_unknown() {
        let path = Path::new("/s/fl.fls");
        let before = stats(1000, 600);
        let v = compact_json(path, &before, None);
        assert_eq!(v["compacted"], true);
        assert_eq!(v["before"]["dead_bytes"], 600);
        assert_eq!(v["after"], Value::Null);
        let text = compact_text(path, &before, None);
        assert!(text.contains("file bytes: 1000 -> -"), "{text}");
        assert!(text.contains("dead bytes: 600 -> -"), "{text}");
        let after = stats(400, 0);
        let v = compact_json(path, &before, Some(&after));
        assert_eq!(v["after"]["file_bytes"], 400);
        assert!(compact_text(path, &before, Some(&after)).contains("dead bytes: 600 -> 0"));
    }
}
