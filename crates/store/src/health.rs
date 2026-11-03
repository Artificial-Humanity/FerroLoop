//! Verify a store without changing it: what `fl doctor` asks (#205 A3).
//!
//! Nothing here migrates, creates, repairs, initialises, checkpoints or takes a write lock:
//!
//! - a `.fls` is read with [`ferrostore::inspect_at`] (`O_RDONLY`, no lock, both superblock
//!   slots and the live state);
//! - a `.redb` is opened with [`RedbStore::open_read_only`] (a shared lock; the file's bytes
//!   and mtime stay as they were), waiting at most [`REDB_LOCK_RETRIES`] x 15 ms for a writer.
//!
//! Which engine a path means follows [`choose_backend`], like a normal open. A lone `.redb`
//! under the FerroStore selection is a store a normal open would MIGRATE: that is reported as
//! [`StoreOutcome::WouldMigrate`], and the migration is not run.

use crate::any::{BackendKind, choose_backend, is_redb_busy, physical_path};
use crate::ferrostore::inspect_at;
use crate::{BACKEND_ENV, FORMAT_WITH_WORKFLOW, RedbStore, check_format};
use fl_core::StoreError;
use fl_core::doctor::StoreOutcome;
use std::path::Path;

/// How many 15 ms waits a locked `.redb` gets: about 300 ms in all.
pub const REDB_LOCK_RETRIES: u32 = 20;

fn exists(path: &Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.kind().to_string()),
    }
}

/// Whether `path` (a store file, a symlink to one counted) is a regular file.
fn regular(path: &Path) -> Result<(), String> {
    match std::fs::metadata(path) {
        Ok(m) if m.is_file() => Ok(()),
        Ok(_) => Err("is not a regular file".into()),
        Err(e) => Err(e.kind().to_string()),
    }
}

/// Verify the store named by the logical `path`. `forced` overrides the engine choice (the
/// hub's `store_backend`); `None` reads `FL_STORE_BACKEND` like a normal open.
pub fn verify_read_only(path: &Path, forced: Option<BackendKind>) -> StoreOutcome {
    let kind = match forced {
        Some(k) => k,
        None => {
            let env = std::env::var(BACKEND_ENV).ok();
            match choose_backend(path, env.as_deref()) {
                Ok(k) => k,
                Err(e) => {
                    return StoreOutcome::Invalid {
                        reason: e.to_string(),
                    };
                }
            }
        }
    };
    match kind {
        BackendKind::Redb => match exists(path) {
            Ok(false) => StoreOutcome::Absent,
            Ok(true) => redb_outcome(path),
            Err(reason) => StoreOutcome::Invalid { reason },
        },
        BackendKind::Ferro => {
            let fls = physical_path(path, BackendKind::Ferro);
            let beside = path != fls;
            match exists(&fls) {
                Err(reason) => StoreOutcome::Invalid { reason },
                Ok(true) => {
                    let backup = beside && exists(path).unwrap_or(false);
                    match fls_outcome(&fls) {
                        StoreOutcome::Verified {
                            engine,
                            format,
                            newest,
                            ..
                        } => StoreOutcome::Verified {
                            engine,
                            format,
                            newest,
                            backup_beside: backup,
                        },
                        other => other,
                    }
                }
                // A lone .redb: a normal open would migrate it.
                Ok(false) => match exists(path) {
                    Ok(true) if beside => match redb_outcome(path) {
                        StoreOutcome::Verified { format, .. } => {
                            StoreOutcome::WouldMigrate { format }
                        }
                        other => other,
                    },
                    Ok(_) => StoreOutcome::Absent,
                    Err(reason) => StoreOutcome::Invalid { reason },
                },
            }
        }
    }
}

fn format_outcome(engine: &str, found: Option<u64>) -> StoreOutcome {
    match found {
        // A file from before versioning opens, as a normal open decides.
        None if engine == "fls" => StoreOutcome::Verified {
            engine: engine.into(),
            format: None,
            newest: FORMAT_WITH_WORKFLOW,
            backup_beside: false,
        },
        found => match check_format(found) {
            Ok(()) => StoreOutcome::Verified {
                engine: engine.into(),
                format: found,
                newest: FORMAT_WITH_WORKFLOW,
                backup_beside: false,
            },
            Err(e) => StoreOutcome::Unsupported {
                reason: e.to_string(),
            },
        },
    }
}

fn fls_outcome(path: &Path) -> StoreOutcome {
    if let Err(reason) = regular(path) {
        return StoreOutcome::Invalid { reason };
    }
    let ins = match inspect_at(path) {
        Ok(i) => i,
        Err(e) => {
            return StoreOutcome::Invalid {
                reason: e.to_string(),
            };
        }
    };
    let Some(contents) = ins.contents else {
        let why = ins
            .error
            .unwrap_or_else(|| "no readable FerroStore state".into());
        return if ins.slots.iter().any(|s| s.state == "newer") {
            StoreOutcome::Unsupported {
                reason: format!("written by a newer fl: {why}"),
            }
        } else {
            StoreOutcome::Invalid { reason: why }
        };
    };
    if let Some(e) = contents.schema_error {
        return StoreOutcome::Invalid { reason: e };
    }
    format_outcome("fls", contents.schema_version)
}

fn redb_outcome(path: &Path) -> StoreOutcome {
    if let Err(reason) = regular(path) {
        return StoreOutcome::Invalid { reason };
    }
    match RedbStore::open_read_only(path, REDB_LOCK_RETRIES) {
        Ok(store) => match store.format_version() {
            Ok(found) => format_outcome("redb", found),
            Err(e) => StoreOutcome::Invalid {
                reason: e.to_string(),
            },
        },
        Err(e) if is_redb_busy(&e) => StoreOutcome::Locked,
        Err(e @ StoreError::FormatVersion { .. }) | Err(e @ StoreError::ContainerFormat { .. }) => {
            StoreOutcome::Unsupported {
                reason: e.to_string(),
            }
        }
        Err(e) => StoreOutcome::Invalid {
            reason: e.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fls_with_no_recorded_schema_opens_but_a_redb_without_one_does_not() {
        assert!(matches!(
            format_outcome("fls", None),
            StoreOutcome::Verified { format: None, .. }
        ));
        assert!(matches!(
            format_outcome("redb", None),
            StoreOutcome::Unsupported { .. }
        ));
        assert!(matches!(
            format_outcome("redb", Some(FORMAT_WITH_WORKFLOW)),
            StoreOutcome::Verified { .. }
        ));
    }
}
