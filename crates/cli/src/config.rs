//! The user-level binding of projects to stores (spec §2.6).
//!
//! User-level, not in the repository: a store path belongs to a machine, and
//! a public repository would publish it.
//!
//! ⚠ A config file that exists but cannot be read is an ERROR, never a
//! silent fall-through to the default store. Falling through would put a
//! project's records in a store nobody chose.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub root: PathBuf,
    pub store: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    project: Vec<Entry>,
}

pub fn path() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .map(|base| base.join("fl").join("config.toml"))
}

pub fn load(path: Option<&Path>) -> Result<Vec<Entry>> {
    let Some(path) = path else { return Ok(vec![]) };
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e).with_context(|| format!("could not read {}", path.display())),
    };
    let file: File = toml::from_str(&text)
        .with_context(|| format!("{} is not a valid fl config", path.display()))?;
    for e in &file.project {
        if !e.root.is_absolute() || !e.store.is_absolute() {
            bail!(
                "{}: `root` and `store` must be absolute paths (got root `{}`, store `{}`)",
                path.display(),
                e.root.display(),
                e.store.display()
            );
        }
    }
    Ok(file.project)
}

/// The store bound to the project containing `cwd`: the entry whose root is
/// the longest ancestor of `cwd`. Both sides are canonicalized, so a
/// symlinked path binds like the real one.
pub fn bound(entries: &[Entry], cwd: &Path) -> Option<PathBuf> {
    let cwd = cwd.canonicalize().ok()?;
    entries
        .iter()
        .filter_map(|e| e.root.canonicalize().ok().map(|r| (r, e)))
        .filter(|(r, _)| cwd.starts_with(r))
        .max_by_key(|(r, _)| r.components().count())
        .map(|(_, e)| e.store.clone())
}
