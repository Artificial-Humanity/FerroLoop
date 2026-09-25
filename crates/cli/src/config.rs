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

/// The XDG config base directory: `xdg_config_home` if it is a non-empty,
/// ABSOLUTE path, else `home/.config`. Per the XDG base directory spec, a
/// relative `$XDG_CONFIG_HOME` (including empty, which is not absolute)
/// must be treated as unset rather than used as-is (Fix round 1, item 9).
/// Pure and dependency-free so it can be unit-tested without touching this
/// process's own environment.
fn base_dir(xdg_config_home: Option<PathBuf>, home: Option<PathBuf>) -> Option<PathBuf> {
    xdg_base(xdg_config_home, home, ".config")
}

/// The XDG data base directory, by the same rule as [`base_dir`]:
/// `xdg_data_home` if it is an ABSOLUTE path, else `home/.local/share`. An
/// empty or relative `$XDG_DATA_HOME` is treated as unset (Final review,
/// item 5) — used as-is it would put the store under the current directory.
pub fn data_dir(xdg_data_home: Option<PathBuf>, home: Option<PathBuf>) -> Option<PathBuf> {
    xdg_base(xdg_data_home, home, ".local/share")
}

/// The one rule both XDG bases follow: the variable if it is absolute, else
/// `home` joined with the spec's default for that base.
fn xdg_base(var: Option<PathBuf>, home: Option<PathBuf>, default: &str) -> Option<PathBuf> {
    var.filter(|p| p.is_absolute())
        .or_else(|| home.map(|h| h.join(default)))
}

pub fn path() -> Option<PathBuf> {
    let base = base_dir(
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
    )?;
    Some(base.join("fl").join("config.toml"))
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

/// Canonicalize `path`. A path that does not exist is not an error here —
/// `Ok(None)` — that is simply a project root (or a `cwd`) not yet created.
/// Any OTHER failure, such as a permission error partway down the tree, IS
/// an error naming the path: silently treating it the same as "does not
/// exist" would fall through to the default store, putting a project's
/// records in a store nobody chose (Fix round 1, item 7).
fn canonicalize(path: &Path) -> Result<Option<PathBuf>> {
    match path.canonicalize() {
        Ok(p) => Ok(Some(p)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("could not resolve {}", path.display())),
    }
}

/// The store bound to the project containing `cwd`: the entry whose root is
/// the longest ancestor of `cwd`. Both sides are canonicalized, so a
/// symlinked path binds like the real one.
///
/// §2.6 binds a project to exactly one store. Two (or more) entries whose
/// canonical `root` is identical — the longest match is therefore tied —
/// but whose `store` differs are refused rather than silently picking one:
/// nothing chose between them (Fix round 1, item 8).
pub fn bound(entries: &[Entry], cwd: &Path) -> Result<Option<PathBuf>> {
    let Some(cwd) = canonicalize(cwd)? else {
        return Ok(None);
    };
    let mut matches: Vec<(PathBuf, &Entry)> = Vec::new();
    for e in entries {
        let Some(root) = canonicalize(&e.root)? else {
            continue;
        };
        if cwd.starts_with(&root) {
            matches.push((root, e));
        }
    }
    let Some(longest) = matches.iter().map(|(r, _)| r.components().count()).max() else {
        return Ok(None);
    };
    let winners: Vec<&(PathBuf, &Entry)> = matches
        .iter()
        .filter(|(r, _)| r.components().count() == longest)
        .collect();
    let mut distinct_stores: Vec<&PathBuf> = winners.iter().map(|(_, e)| &e.store).collect();
    distinct_stores.sort();
    distinct_stores.dedup();
    if distinct_stores.len() > 1 {
        let names = winners
            .iter()
            .map(|(_, e)| format!("{} -> {}", e.root.display(), e.store.display()))
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "the project at {} is bound to more than one store in the config: {names}",
            cwd.display()
        );
    }
    Ok(Some(winners[0].1.store.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_or_relative_xdg_config_home_falls_back_to_home() {
        let home = Some(PathBuf::from("/home/u"));
        assert_eq!(
            base_dir(Some(PathBuf::from("")), home.clone()),
            Some(PathBuf::from("/home/u/.config")),
            "an empty XDG_CONFIG_HOME must be treated as unset"
        );
        assert_eq!(
            base_dir(Some(PathBuf::from("relative/config")), home.clone()),
            Some(PathBuf::from("/home/u/.config")),
            "a relative XDG_CONFIG_HOME must be treated as unset"
        );
        assert_eq!(
            base_dir(Some(PathBuf::from("/abs/config")), home),
            Some(PathBuf::from("/abs/config")),
            "an absolute XDG_CONFIG_HOME must still win"
        );
    }

    #[test]
    fn an_empty_or_relative_xdg_data_home_falls_back_to_home() {
        let home = Some(PathBuf::from("/home/u"));
        assert_eq!(
            data_dir(Some(PathBuf::from("")), home.clone()),
            Some(PathBuf::from("/home/u/.local/share")),
            "an empty XDG_DATA_HOME must be treated as unset"
        );
        assert_eq!(
            data_dir(Some(PathBuf::from("relative/data")), home.clone()),
            Some(PathBuf::from("/home/u/.local/share")),
            "a relative XDG_DATA_HOME must be treated as unset"
        );
        assert_eq!(
            data_dir(Some(PathBuf::from("/abs/data")), home.clone()),
            Some(PathBuf::from("/abs/data")),
            "an absolute XDG_DATA_HOME must still win"
        );
        assert_eq!(
            data_dir(None, home),
            Some(PathBuf::from("/home/u/.local/share"))
        );
        assert_eq!(data_dir(Some(PathBuf::from("rel")), None), None);
    }

    #[cfg(unix)]
    #[test]
    fn bound_resolves_a_symlinked_cwd_passed_directly() {
        // Task 6's own "symlinked working directory" CLI test drives `fl`
        // as a subprocess; `std::env::current_dir()` (`getcwd(3)`) already
        // resolves a symlinked process cwd before `bound` ever sees it on
        // Linux, so that test cannot tell `bound`'s own `cwd.canonicalize()`
        // apart from a naive string comparison. Calling `bound` directly
        // here, with a raw symlink path nothing else has resolved first,
        // closes that gap (Fix round 1, item 2).
        let real = tempfile::tempdir().unwrap();
        let stores = tempfile::tempdir().unwrap();
        let store = stores.path().join("a.redb");
        let links = tempfile::tempdir().unwrap();
        let link = links.path().join("via-link");
        std::os::unix::fs::symlink(real.path(), &link).unwrap();

        let entries = vec![Entry {
            root: real.path().to_path_buf(),
            store: store.clone(),
        }];
        let got = bound(&entries, &link).unwrap();
        assert_eq!(got, Some(store));
    }

    #[test]
    fn two_entries_with_the_same_root_but_different_stores_are_refused() {
        let root = tempfile::tempdir().unwrap();
        let entries = vec![
            Entry {
                root: root.path().to_path_buf(),
                store: PathBuf::from("/tmp/fl-config-test-one.redb"),
            },
            Entry {
                root: root.path().to_path_buf(),
                store: PathBuf::from("/tmp/fl-config-test-two.redb"),
            },
        ];
        let err = bound(&entries, root.path()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("fl-config-test-one.redb") && msg.contains("fl-config-test-two.redb"),
            "the refusal must name both entries: {msg}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_permission_error_while_canonicalizing_is_an_error_not_a_silent_skip() {
        use std::os::unix::fs::PermissionsExt;
        let parent = tempfile::tempdir().unwrap();
        let blocked = parent.path().join("blocked");
        std::fs::create_dir(&blocked).unwrap();
        let inner = blocked.join("root");
        std::fs::create_dir(&inner).unwrap();
        // No execute permission on `blocked`: the kernel refuses to resolve
        // anything under it — for anyone but root.
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000)).unwrap();

        // Ask the standard library directly, first, to find out whether
        // this process (root, or a filesystem that does not enforce the
        // bit) can even be made to see the failure this test exists to
        // pin — a soft `Err(_) => {}` / `Ok(_) => eprintln!(...)` on OUR
        // wrapper's result would never fail no matter what `canonicalize`
        // does with the error, so it could not catch a regression either.
        let raw = inner.canonicalize();
        let result = canonicalize(&inner);

        // Restore before the TempDir's own cleanup has to walk it again.
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o755)).unwrap();

        match raw {
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                assert!(
                    result.is_err(),
                    "a permission failure must be reported as an error, not treated the \
                     same as \"does not exist\""
                );
            }
            _ => eprintln!(
                "skipping the assertion: this process could still resolve a 0o000 \
                 directory, so it is likely running as root"
            ),
        }
    }
}
