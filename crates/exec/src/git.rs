use crate::population::{ChangedPaths, ExecError};
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Git;

fn git(root: &Path, args: &[&str]) -> Result<String, ExecError> {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|e| ExecError::Git(format!("could not run git: {e}")))?;
    if !out.status.success() {
        return Err(ExecError::Git(format!(
            "git {} failed in {}: {}",
            args.join(" "),
            root.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

impl Git {
    pub fn head(root: &Path) -> Result<String, ExecError> {
        git(root, &["rev-parse", "HEAD"])
    }

    /// Absolute paths that differ between two commits.
    pub fn changed_between(root: &Path, from: &str, to: &str) -> Result<Vec<PathBuf>, ExecError> {
        let text = git(root, &["diff", "--name-only", from, to])?;
        Ok(text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(|l| root.join(l))
            .collect())
    }
}

impl ChangedPaths for Git {
    fn changed_since(&self, root: &Path, base: &str) -> Result<Vec<PathBuf>, ExecError> {
        let head = Self::head(root)?;
        Self::changed_between(root, base, &head)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    fn repo() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            let ok = Command::new("git")
                .args(args)
                .current_dir(d.path())
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?} failed");
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "t"]);
        fs::create_dir_all(d.path().join("src")).unwrap();
        fs::write(d.path().join("src/a.rs"), "fn a() {}").unwrap();
        fs::write(d.path().join("README.md"), "one").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-qm", "first"]);
        d
    }

    fn commit(dir: &Path, msg: &str) -> String {
        let run = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .unwrap();
        };
        run(&["add", "-A"]);
        run(&["commit", "-qm", msg]);
        Git::head(dir).unwrap()
    }

    #[test]
    fn head_is_a_full_sha() {
        let d = repo();
        let head = Git::head(d.path()).unwrap();
        assert_eq!(head.len(), 40, "got {head}");
        assert!(head.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn changed_between_lists_only_what_actually_changed() {
        let d = repo();
        let first = Git::head(d.path()).unwrap();
        fs::write(d.path().join("README.md"), "two").unwrap();
        let second = commit(d.path(), "docs");

        let changed = Git::changed_between(d.path(), &first, &second).unwrap();
        assert_eq!(changed.len(), 1, "got {changed:?}");
        assert!(changed[0].ends_with("README.md"));
    }

    #[test]
    fn changed_paths_are_absolute() {
        let d = repo();
        let first = Git::head(d.path()).unwrap();
        fs::write(d.path().join("src/a.rs"), "fn a() { }").unwrap();
        let second = commit(d.path(), "code");
        let changed = Git::changed_between(d.path(), &first, &second).unwrap();
        assert!(changed[0].is_absolute(), "got {changed:?}");
    }

    #[test]
    fn a_non_repository_is_an_error_and_never_an_empty_answer() {
        let d = tempfile::tempdir().unwrap();
        assert!(matches!(Git::head(d.path()), Err(ExecError::Git(_))));
    }
}
