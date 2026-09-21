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
    /// ⚠ A store read or write failed. Kept separate from [`ExecError::Git`]
    /// because it used to be folded into it, and a deserialization failure
    /// then announced itself as `git failed:` from `check`, the flagship
    /// command — a refusal naming the wrong cause.
    #[error("{0}")]
    Store(String),
    #[error("command `{0}` could not be run: {1}")]
    Spawn(String, String),
    /// ⚠ A population command that exits non-zero has told you nothing about
    /// the population, which is not the same as telling you it is empty.
    /// Reporting it as an empty population would blame the file tree for a
    /// fault in the listing program — the same shape as [`ExecError::Store`]
    /// once announcing itself as `git failed:`.
    #[error(
        "population command `{program}` exited {code}, so the population is unknown, not empty: {stderr}"
    )]
    PopulationCommand {
        program: String,
        code: String,
        stderr: String,
    },
    #[error("command `{0}` exceeded its {1}s timeout")]
    Timeout(String, u64),
}

/// Paths that differ from a base ref. Separated behind a trait so population
/// resolution is testable without a git repository.
pub trait ChangedPaths {
    fn changed_since(&self, root: &Path, base: &str) -> Result<Vec<PathBuf>, ExecError>;
}

pub(crate) fn matcher(pattern: &str) -> Result<GlobMatcher, ExecError> {
    Glob::new(pattern)
        .map(|g| g.compile_matcher())
        .map_err(|e| ExecError::BadSelector(format!("{pattern}: {e}")))
}

/// Resolve a selector against a working tree, right now.
///
/// ⚠ The result is never cached and never stored. The whole point of
/// enumerating here is that the population cannot go stale in a database.
///
/// All returned paths are absolute, regardless of whether root is relative
/// or whether the underlying selector (e.g., a ChangedPaths impl) returns relative paths.
pub fn resolve(
    root: &Path,
    selector: &Selector,
    changed: &dyn ChangedPaths,
) -> Result<Vec<PathBuf>, ExecError> {
    // Absolutize root if needed, so all results can be absolute
    let abs_root = if root.is_absolute() {
        root.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| ExecError::Walk(root.display().to_string(), e.to_string()))?
            .join(root)
    };

    match selector {
        Selector::Glob { pattern } => {
            let m = matcher(pattern)?;
            let mut out = Vec::new();
            for entry in WalkDir::new(&abs_root).into_iter() {
                let entry = entry
                    .map_err(|e| ExecError::Walk(abs_root.display().to_string(), e.to_string()))?;
                // Skip directories and symlinks. Symlinks are excluded deliberately:
                // following them invites cycle hangs, and this product's rule is that
                // a population must be knowable, not comprehensive.
                if !entry.file_type().is_file() {
                    continue;
                }
                let Ok(rel) = entry.path().strip_prefix(&abs_root) else {
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
            let mut out = changed.changed_since(&abs_root, base)?;
            // Absolutize paths returned by the trait impl, which may be relative
            out = out
                .into_iter()
                .map(|p| if p.is_absolute() { p } else { abs_root.join(p) })
                .collect();
            out.sort();
            Ok(out)
        }
        Selector::Command { program, args } => {
            let output = std::process::Command::new(program)
                .args(args)
                .current_dir(&abs_root)
                .output()
                .map_err(|e| ExecError::Spawn(program.clone(), e.to_string()))?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(ExecError::PopulationCommand {
                    program: program.clone(),
                    code: output
                        .status
                        .code()
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "on a signal".into()),
                    stderr: stderr.trim().to_string(),
                });
            }
            let text = String::from_utf8_lossy(&output.stdout);
            let mut out: Vec<PathBuf> = text
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(|l| {
                    let p = PathBuf::from(l);
                    if p.is_absolute() { p } else { abs_root.join(p) }
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

    // REPRODUCTION (C6): `Selector::Command` reads the program's stdout and
    // never looks at its exit status, so a population command that FAILS
    // resolves to zero paths. The gate then refuses with `empty_population`
    // — the right answer for the wrong reason, and it points the reader at
    // their file tree when the fault is in the listing program. An unknown
    // population is not an empty one.
    #[test]
    fn a_population_command_that_fails_is_an_error_not_an_empty_population() {
        let d = tree();
        let sel = Selector::Command {
            program: "sh".into(),
            args: vec![
                "-c".into(),
                "echo 'fatal: not a git repository' >&2; exit 128".into(),
            ],
        };
        let err = resolve(d.path(), &sel, &NoChanges)
            .expect_err("a failed population command cannot report a population");
        let msg = err.to_string();
        assert!(msg.contains("128"), "does not name the exit status: {msg}");
        assert!(
            msg.contains("not a git repository"),
            "does not carry the program's own complaint: {msg}"
        );
    }

    #[test]
    fn a_glob_finds_nested_matches_and_ignores_the_rest() {
        let d = tree();
        let sel = Selector::Glob {
            pattern: "src/**/*.rs".into(),
        };
        let mut got = resolve(d.path(), &sel, &NoChanges).unwrap();
        got.sort();
        assert_eq!(
            got.len(),
            2,
            "expected src/a.rs and src/deep/b.rs, got {got:?}"
        );
        assert!(got.iter().all(|p| p.extension().unwrap() == "rs"));
    }

    #[test]
    fn a_glob_that_matches_nothing_resolves_to_an_empty_population_not_an_error() {
        let d = tree();
        let sel = Selector::Glob {
            pattern: "nowhere/**/*.rs".into(),
        };
        assert_eq!(resolve(d.path(), &sel, &NoChanges).unwrap().len(), 0);
    }

    #[test]
    fn an_invalid_glob_is_an_error_and_never_an_empty_population() {
        let d = tree();
        let sel = Selector::Glob {
            pattern: "src/**/[".into(),
        };
        assert!(matches!(
            resolve(d.path(), &sel, &NoChanges),
            Err(ExecError::BadSelector(_))
        ));
    }

    #[test]
    fn resolved_paths_are_absolute_so_a_command_can_be_run_from_anywhere() {
        let d = tree();
        let sel = Selector::Glob {
            pattern: "src/a.rs".into(),
        };
        let got = resolve(d.path(), &sel, &NoChanges).unwrap();
        assert!(got[0].is_absolute(), "got {got:?}");
    }

    // Builds its fixture inside the real cwd (via `tempdir_in`) instead of
    // chdir-ing the process into it. `cargo test` runs test threads in
    // parallel and cwd is process-global state, so a test that calls
    // `std::env::set_current_dir` is racing every other test that ever
    // reads the cwd — inert today only because nothing else does yet.
    #[test]
    fn glob_absolutizes_paths_even_when_given_a_relative_root() {
        let cwd = std::env::current_dir().unwrap();
        let d = tempfile::Builder::new().tempdir_in(&cwd).unwrap();
        fs::create_dir_all(d.path().join("src/deep")).unwrap();
        fs::write(d.path().join("src/a.rs"), "").unwrap();
        fs::write(d.path().join("src/deep/b.rs"), "").unwrap();

        // A root relative to the (unmodified) real cwd exercises the same
        // `!root.is_absolute()` branch with no global mutation.
        let rel_root = d.path().strip_prefix(&cwd).unwrap();
        assert!(
            !rel_root.is_absolute(),
            "fixture root must be relative: {rel_root:?}"
        );

        let sel = Selector::Glob {
            pattern: "src/**/*.rs".into(),
        };
        let got = resolve(rel_root, &sel, &NoChanges).unwrap();
        assert_eq!(got.len(), 2);
        assert!(
            got.iter().all(|p| p.is_absolute()),
            "glob with relative root returned relative paths: {got:?}"
        );
    }

    // Carried over from Task 6's review: the Command branch got the same
    // absolute-path fix as Glob and Changed, but only those two got tests.
    #[test]
    fn command_selector_absolutizes_paths_even_when_given_a_relative_root() {
        let cwd = std::env::current_dir().unwrap();
        let d = tempfile::Builder::new().tempdir_in(&cwd).unwrap();
        fs::write(d.path().join("a.rs"), "").unwrap();

        let rel_root = d.path().strip_prefix(&cwd).unwrap();
        assert!(
            !rel_root.is_absolute(),
            "fixture root must be relative: {rel_root:?}"
        );

        let sel = Selector::Command {
            program: "sh".into(),
            args: vec!["-c".into(), "echo a.rs".into()],
        };
        let got = resolve(rel_root, &sel, &NoChanges).unwrap();
        assert_eq!(got.len(), 1, "got {got:?}");
        assert!(
            got[0].is_absolute(),
            "command selector with relative root returned relative paths: {got:?}"
        );
        assert!(got[0].ends_with("a.rs"), "got {got:?}");
    }

    struct RelativeChanges;
    impl ChangedPaths for RelativeChanges {
        fn changed_since(&self, _root: &Path, _base: &str) -> Result<Vec<PathBuf>, ExecError> {
            // Simulate git diff --name-only output: repo-relative paths
            Ok(vec![
                PathBuf::from("src/a.rs"),
                PathBuf::from("src/deep/b.rs"),
            ])
        }
    }

    #[test]
    fn changed_absolutizes_paths_returned_by_the_implementor() {
        let d = tree();
        let sel = Selector::Changed {
            base: "main".into(),
        };
        let got = resolve(d.path(), &sel, &RelativeChanges).unwrap();
        assert_eq!(got.len(), 2);
        assert!(
            got.iter().all(|p| p.is_absolute()),
            "changed selector returned relative paths: {got:?}"
        );
    }

    #[test]
    fn symlinks_are_excluded_from_glob_results() {
        let d = tree();
        let target = d.path().join("src/deep/b.rs");
        let link = d.path().join("src/link.rs");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&target, &link).unwrap();

        let sel = Selector::Glob {
            pattern: "src/**/*.rs".into(),
        };
        let got = resolve(d.path(), &sel, &NoChanges).unwrap();
        // Should have src/a.rs and src/deep/b.rs, but NOT the symlink
        assert_eq!(got.len(), 2, "symlink should be excluded from glob results");
        assert!(
            !got.iter().any(|p| p.ends_with("link.rs")),
            "symlink appeared in results: {got:?}"
        );
    }
}
