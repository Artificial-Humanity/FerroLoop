//! `fl store`: look at, and tend, the engine file behind a store.
//!
//! Only what the engines do today is here. Not yet present, because each
//! needs engine work that has not landed: `compact`, `migrate`, `verify`
//! (a parity verifier), `bench` and `serve`.
//!
//! Neither command creates a store: a path that names nothing is reported
//! (`status`) or refused (`checkpoint`), never opened.

use anyhow::{Result, bail};
use clap::Subcommand;
use fl_store::{BACKEND_ENV, BackendKind, StoreOptions, choose_backend, physical_path};
use serde_json::{Value, json};
use std::path::Path;

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
    }
}

fn status(path: &Path, json_out: bool) -> Result<i32> {
    let kind = engine(path)?;
    let physical = physical_path(path, kind);
    let meta = std::fs::metadata(&physical).ok();
    let exists = meta.is_some();
    let size = meta.as_ref().map(std::fs::Metadata::len);
    // A lone `.redb` beside a ferro selection: `open_store` refuses it with a
    // migrate message, so say so here too.
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
             migrate it first, or set {BACKEND_ENV}=redb to keep using it",
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
    if !fl_store::store_exists(path) {
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
