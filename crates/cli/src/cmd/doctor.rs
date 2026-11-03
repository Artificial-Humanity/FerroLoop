//! `fl doctor`: an observational check of this install (#205 phase A3).
//!
//! It reads: the install receipt, `lstat`/`readlink` and bounded hashes of the paths the
//! receipt owns, the stores (opened read-only, never migrated, see `fl_store::health`), the
//! vault files' modes (their contents are never read) and the UI bundle's `build-info.json`.
//! It writes nothing, takes no write lock, makes no network call, starts no process, loads no
//! model, generates no key and prints no secret.
//!
//! Exit status: `0` when no check failed (`warn`, `not-installed` and `unsupported` do not
//! fail the run), `1` when at least one check failed, `3` for `--fix` (repairs are not
//! implemented in this release).

use clap::Args;
use fl_core::doctor::{Observe, StoreFact, StoreFacts, UiDir, observe};
use fl_core::install::resolve_layout;
use fl_store::store_identity;
use std::path::PathBuf;
use std::time::Duration;

/// Exit status of `--fix`: the repair is refused.
pub const EXIT_REFUSED: i32 = 3;
/// The most stores one run verifies; the rest are named in a note.
const MAX_STORES: usize = 64;
/// The most time the receipt's paths may take to hash.
const PATH_BUDGET: Duration = Duration::from_secs(60);

#[derive(Args, Debug)]
pub struct Cmd {
    /// Print the report as JSON (schema_version 1).
    #[arg(long)]
    pub json: bool,
    /// Look for the install under this absolute directory instead of ~/.local/share/fl.
    /// `$FL_INSTALL_ROOT` isolates the whole default layout under one root.
    #[arg(long, value_name = "PATH")]
    pub prefix: Option<PathBuf>,
    /// Repair one check by id. Not implemented in this release: doctor only observes.
    #[arg(long, value_name = "CHECK_ID")]
    pub fix: Option<String>,
}

pub fn run(cmd: Cmd, db: Option<PathBuf>) -> anyhow::Result<i32> {
    if cmd.fix.is_some() {
        eprintln!(
            "error: repairs are not implemented in this release; `fl doctor` only observes \
             (a repair would be an explicit, separate action with its own plan)"
        );
        return Ok(EXIT_REFUSED);
    }
    let layout = match resolve_layout(cmd.prefix.as_deref(), std::env::var_os("FL_INSTALL_ROOT")) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("error: {e}");
            return Ok(EXIT_REFUSED);
        }
    };
    let report_stores = stores(db);
    let config_ui = crate::config::load(crate::config::path().as_deref())
        .ok()
        .and_then(|c| c.hub)
        .and_then(|h| h.ui_dir);
    let report = observe(Observe {
        binary: crate::version::build_info(),
        binary_name: crate::version::NAME,
        home: std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from),
        layout,
        ui_dir: config_ui.map_or(UiDir::ReleaseCurrent, UiDir::Explicit),
        vault_path: fl_gate::vault::resolve_vault_path(None),
        key_path: fl_gate::vault::resolve_master_key_path(None),
        key_in_env: std::env::var_os("FL_VAULT_MASTER_KEY").is_some_and(|v| !v.is_empty()),
        stores: report_stores,
        budget: PATH_BUDGET,
    });
    if cmd.json {
        print!("{}", report.to_json());
    } else {
        print!("{}", report.render_text());
    }
    Ok(report.exit_code())
}

/// The stores to verify: the one `--db`/`$FL_DB` names, else the workspace state store and
/// every project store the config binds, each once.
fn stores(db: Option<PathBuf>) -> StoreFacts {
    let mut facts = StoreFacts::default();
    let mut paths: Vec<PathBuf> = Vec::new();
    if let Some(p) = crate::explicit_db(db) {
        paths.push(p);
    } else {
        let default = crate::default_state_store_path();
        match crate::config::load(crate::config::path().as_deref()) {
            Ok(cfg) => {
                match &default {
                    Ok(d) => paths.push(cfg.workspace_state_store(d)),
                    Err(_) => {
                        if let Some(s) = cfg.state.as_ref().and_then(|s| s.store.clone()) {
                            paths.push(s);
                        } else {
                            facts.notes.push(
                                "no store location: neither $XDG_DATA_HOME nor $HOME is set".into(),
                            );
                        }
                    }
                }
                paths.extend(cfg.projects.iter().map(|e| e.store.clone()));
            }
            Err(_) => {
                facts.notes.push(
                    "the config could not be read, so only the default store was checked".into(),
                );
                if let Ok(d) = default {
                    paths.push(d);
                }
            }
        }
    }
    let mut seen: Vec<PathBuf> = Vec::new();
    for p in paths {
        let id = store_identity(&p);
        if seen.contains(&id) {
            continue;
        }
        seen.push(id);
        if facts.stores.len() >= MAX_STORES {
            facts
                .notes
                .push(format!("only the first {MAX_STORES} stores were verified"));
            break;
        }
        facts.stores.push(StoreFact {
            path: p.display().to_string(),
            outcome: fl_store::health::verify_read_only(&p, None),
        });
    }
    facts
}
