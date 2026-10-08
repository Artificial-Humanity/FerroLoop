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

    /// Whether `rel` (relative to `root`) is tracked and has no uncommitted
    /// change. Untracked is `false`, not an error; a git that cannot answer
    /// is an error, never `false`.
    pub fn is_committed(root: &Path, rel: &str) -> Result<bool, ExecError> {
        let tracked = Self::is_tracked(root, rel)?;
        let clean = git(root, &["status", "--porcelain", "--", rel])?.is_empty();
        Ok(tracked && clean)
    }

    /// Whether `rel` (relative to `root`) is in git's index: committed, or
    /// added and not yet committed, ignored or not. A path git does not know
    /// is `false`; a git that cannot answer is an error, never `false`.
    pub fn is_tracked(root: &Path, rel: &str) -> Result<bool, ExecError> {
        Ok(!git(root, &["ls-files", "--", rel])?.is_empty())
    }

    /// Whether `rel` (relative to `root`) is excluded by a `.gitignore`
    /// pattern. `git check-ignore` exits 0 when the path is ignored and 1
    /// when it is not; anything else is a git failure and is never read as
    /// "not ignored".
    pub fn is_ignored(root: &Path, rel: &str) -> Result<bool, ExecError> {
        let out = Command::new("git")
            .args(["check-ignore", "-q", "--", rel])
            .current_dir(root)
            .output()
            .map_err(|e| ExecError::Git(format!("could not run git: {e}")))?;
        match out.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(ExecError::Git(format!(
                "git check-ignore -- {rel} failed in {}: {}",
                root.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            ))),
        }
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

    #[test]
    fn is_committed_tells_untracked_modified_and_committed_apart() {
        let d = repo();
        fs::create_dir_all(d.path().join(".fl")).unwrap();
        fs::write(d.path().join(".fl/manifest.json"), "{}").unwrap();
        assert!(
            !Git::is_committed(d.path(), ".fl/manifest.json").unwrap(),
            "untracked"
        );
        commit(d.path(), "manifest");
        assert!(
            Git::is_committed(d.path(), ".fl/manifest.json").unwrap(),
            "committed"
        );
        fs::write(d.path().join(".fl/manifest.json"), "{ }").unwrap();
        assert!(
            !Git::is_committed(d.path(), ".fl/manifest.json").unwrap(),
            "modified"
        );
    }

    #[test]
    fn is_committed_outside_a_repository_is_an_error_not_false() {
        let d = tempfile::tempdir().unwrap();
        assert!(matches!(
            Git::is_committed(d.path(), ".fl/manifest.json"),
            Err(ExecError::Git(_))
        ));
    }

    #[test]
    fn is_ignored_tells_a_tracked_path_from_an_ignored_one() {
        let d = repo();
        assert!(!Git::is_ignored(d.path(), "README.md").unwrap());
        fs::write(d.path().join(".gitignore"), "*.log\n").unwrap();
        fs::write(d.path().join("out.log"), "x").unwrap();
        assert!(Git::is_ignored(d.path(), "out.log").unwrap());
    }

    #[test]
    fn is_ignored_outside_a_repository_is_an_error_not_false() {
        let d = tempfile::tempdir().unwrap();
        assert!(matches!(
            Git::is_ignored(d.path(), "x"),
            Err(ExecError::Git(_))
        ));
    }

    #[test]
    fn is_tracked_tells_a_tracked_path_from_an_untracked_or_ignored_one() {
        let d = repo();
        assert!(Git::is_tracked(d.path(), "README.md").unwrap(), "committed");
        fs::write(d.path().join(".gitignore"), ".mcp.json\n").unwrap();
        fs::write(d.path().join(".mcp.json"), "{}").unwrap();
        assert!(!Git::is_tracked(d.path(), ".mcp.json").unwrap(), "ignored");
        assert!(!Git::is_tracked(d.path(), "absent.json").unwrap(), "absent");
        fs::write(d.path().join("new.rs"), "fn b() {}").unwrap();
        assert!(!Git::is_tracked(d.path(), "new.rs").unwrap(), "untracked");
        // Ignored, and tracked anyway: a `git add -f` makes it tracked.
        let run = Command::new("git")
            .args(["add", "-f", ".mcp.json"])
            .current_dir(d.path())
            .output()
            .unwrap();
        assert!(run.status.success());
        assert!(Git::is_tracked(d.path(), ".mcp.json").unwrap(), "added");
    }

    #[test]
    fn is_tracked_outside_a_repository_is_an_error_not_false() {
        let d = tempfile::tempdir().unwrap();
        assert!(matches!(
            Git::is_tracked(d.path(), ".mcp.json"),
            Err(ExecError::Git(_))
        ));
    }
}
