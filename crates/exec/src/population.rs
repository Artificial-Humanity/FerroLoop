use fl_core::model::Selector;
use globset::{Glob, GlobMatcher};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error("selector is not valid: {0}")]
    BadSelector(String),
    #[error("walking {0} failed: {1}")]
    Walk(String, String),
    #[error("git failed: {0}")]
    Git(String),
    #[error("command `{0}` could not be run: {1}")]
    Spawn(String, String),
    #[error("command `{0}` exceeded its {1}s timeout")]
    Timeout(String, u64),
    #[error("adapter `{0}` is not known")]
    UnknownAdapter(String),
}

/// Paths that differ from a base ref. Separated behind a trait so population
/// resolution is testable without a git repository.
pub trait ChangedPaths {
    fn changed_since(&self, root: &Path, base: &str) -> Result<Vec<PathBuf>, ExecError>;
}

fn matcher(pattern: &str) -> Result<GlobMatcher, ExecError> {
    Glob::new(pattern)
        .map(|g| g.compile_matcher())
        .map_err(|e| ExecError::BadSelector(format!("{pattern}: {e}")))
}

/// Resolve a selector against a working tree, right now.
///
/// ⚠ The result is never cached and never stored. The whole point of
/// enumerating here is that the population cannot go stale in a database.
pub fn resolve(
    root: &Path,
    selector: &Selector,
    changed: &dyn ChangedPaths,
) -> Result<Vec<PathBuf>, ExecError> {
    match selector {
        Selector::Glob { pattern } => {
            let m = matcher(pattern)?;
            let mut out = Vec::new();
            for entry in WalkDir::new(root).into_iter() {
                let entry = entry
                    .map_err(|e| ExecError::Walk(root.display().to_string(), e.to_string()))?;
                if !entry.file_type().is_file() {
                    continue;
                }
                let Ok(rel) = entry.path().strip_prefix(root) else {
                    continue;
                };
                if rel.starts_with(".git") {
                    continue;
                }
                if m.is_match(rel) {
                    out.push(entry.path().to_path_buf());
                }
            }
            out.sort();
            Ok(out)
        }
        Selector::Changed { base } => {
            let mut out = changed.changed_since(root, base)?;
            out.sort();
            Ok(out)
        }
        Selector::Command { program, args } => {
            let output = std::process::Command::new(program)
                .args(args)
                .current_dir(root)
                .output()
                .map_err(|e| ExecError::Spawn(program.clone(), e.to_string()))?;
            let text = String::from_utf8_lossy(&output.stdout);
            let mut out: Vec<PathBuf> = text
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(|l| {
                    let p = PathBuf::from(l);
                    if p.is_absolute() { p } else { root.join(p) }
                })
                .collect();
            out.sort();
            Ok(out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tree() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir_all(d.path().join("src/deep")).unwrap();
        fs::write(d.path().join("src/a.rs"), "").unwrap();
        fs::write(d.path().join("src/deep/b.rs"), "").unwrap();
        fs::write(d.path().join("README.md"), "").unwrap();
        d
    }

    struct NoChanges;
    impl ChangedPaths for NoChanges {
        fn changed_since(&self, _root: &Path, _base: &str) -> Result<Vec<PathBuf>, ExecError> {
            Ok(vec![])
        }
    }

    #[test]
    fn a_glob_finds_nested_matches_and_ignores_the_rest() {
        let d = tree();
        let sel = Selector::Glob { pattern: "src/**/*.rs".into() };
        let mut got = resolve(d.path(), &sel, &NoChanges).unwrap();
        got.sort();
        assert_eq!(got.len(), 2, "expected src/a.rs and src/deep/b.rs, got {got:?}");
        assert!(got.iter().all(|p| p.extension().unwrap() == "rs"));
    }

    #[test]
    fn a_glob_that_matches_nothing_resolves_to_an_empty_population_not_an_error() {
        let d = tree();
        let sel = Selector::Glob { pattern: "nowhere/**/*.rs".into() };
        assert_eq!(resolve(d.path(), &sel, &NoChanges).unwrap().len(), 0);
    }

    #[test]
    fn an_invalid_glob_is_an_error_and_never_an_empty_population() {
        let d = tree();
        let sel = Selector::Glob { pattern: "src/**/[".into() };
        assert!(matches!(resolve(d.path(), &sel, &NoChanges), Err(ExecError::BadSelector(_))));
    }

    #[test]
    fn resolved_paths_are_absolute_so_a_command_can_be_run_from_anywhere() {
        let d = tree();
        let sel = Selector::Glob { pattern: "src/a.rs".into() };
        let got = resolve(d.path(), &sel, &NoChanges).unwrap();
        assert!(got[0].is_absolute(), "got {got:?}");
    }
}
