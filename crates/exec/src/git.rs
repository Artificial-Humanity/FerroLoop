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

    /// Query `git ls-remote --exit-code --refs <remote> refs/heads/<branch>` and return the remote
    /// commit SHA if present. Distinguishes exit code `2` (branch absent -> `Ok(None)`) from
    /// transport/remote errors (`Err(ExecError::Git)`).
    pub fn ls_remote_branch(
        root: &Path,
        remote: &str,
        branch: &str,
    ) -> Result<Option<String>, ExecError> {
        let refspec = format!("refs/heads/{branch}");
        let out = Command::new("git")
            .args(["ls-remote", "--exit-code", "--refs", remote, &refspec])
            .current_dir(root)
            .output()
            .map_err(|e| ExecError::Git(format!("could not run git: {e}")))?;
        match out.status.code() {
            Some(0) => {
                let raw = String::from_utf8_lossy(&out.stdout);
                let Some(first_line) = raw.lines().next() else {
                    return Ok(None);
                };
                let Some((sha, _)) = first_line.split_once('\t') else {
                    return Ok(None);
                };
                let trimmed = sha.trim();
                if trimmed.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(trimmed.to_owned()))
                }
            }
            Some(2) => Ok(None),
            _ => Err(ExecError::Git(format!(
                "git ls-remote --exit-code --refs {remote} {refspec} failed in {}: {}",
                root.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            ))),
        }
    }

    /// Fetch `+refs/heads/<branch>:<local_ref>` from `<remote>` without touching `HEAD` or the working tree.
    pub fn fetch_branch(
        root: &Path,
        remote: &str,
        branch: &str,
        local_ref: &str,
    ) -> Result<(), ExecError> {
        let spec = format!("+refs/heads/{branch}:{local_ref}");
        git(root, &["fetch", "--no-tags", "--quiet", remote, &spec])?;
        Ok(())
    }

    /// Return `true` iff `ancestor` is an ancestor of `descendant` in `root`'s object database.
    pub fn is_ancestor(root: &Path, ancestor: &str, descendant: &str) -> Result<bool, ExecError> {
        let out = Command::new("git")
            .args(["merge-base", "--is-ancestor", ancestor, descendant])
            .current_dir(root)
            .output()
            .map_err(|e| ExecError::Git(format!("could not run git: {e}")))?;
        match out.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(ExecError::Git(format!(
                "git merge-base --is-ancestor {ancestor} {descendant} failed in {}: {}",
                root.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            ))),
        }
    }

    /// Return commits in oldest-to-newest first-parent order along `tip` (or `after..tip` when `after` is `Some`).
    pub fn rev_list_first_parent(
        root: &Path,
        after: Option<&str>,
        tip: &str,
    ) -> Result<Vec<String>, ExecError> {
        let range = match after {
            Some(base) => format!("{base}..{tip}"),
            None => tip.to_owned(),
        };
        let raw = git(root, &["rev-list", "--first-parent", "--reverse", &range])?;
        Ok(raw
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(ToOwned::to_owned)
            .collect())
    }

    /// List all objects recursively under `treeish` (`git ls-tree -r <treeish>`).
    pub fn ls_tree(root: &Path, treeish: &str) -> Result<Vec<GitTreeEntry>, ExecError> {
        let raw = git(root, &["ls-tree", "-r", treeish])?;
        let mut entries = Vec::new();
        for line in raw.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let Some((meta, path)) = line.split_once('\t') else {
                continue;
            };
            let mut parts = meta.split_whitespace();
            let (Some(mode), Some(kind), Some(oid)) = (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            entries.push(GitTreeEntry {
                mode: mode.to_owned(),
                kind: kind.to_owned(),
                oid: oid.to_owned(),
                path: path.to_owned(),
            });
        }
        Ok(entries)
    }

    /// Read raw blob bytes by object ID (`git cat-file blob <oid>`).
    pub fn cat_blob(root: &Path, oid: &str) -> Result<Vec<u8>, ExecError> {
        let out = Command::new("git")
            .args(["cat-file", "blob", oid])
            .current_dir(root)
            .output()
            .map_err(|e| ExecError::Git(format!("could not run git: {e}")))?;
        if !out.status.success() {
            return Err(ExecError::Git(format!(
                "git cat-file blob {oid} failed in {}: {}",
                root.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(out.stdout)
    }

    /// Write a blob into the object database via `git hash-object -w --stdin`.
    pub fn hash_object(root: &Path, content: &[u8]) -> Result<String, ExecError> {
        git_with_stdin(root, &["hash-object", "-w", "--stdin"], content)
    }

    /// Build a single-level tree object via `git mktree` from `(mode, kind, oid, name)` entries.
    pub fn mktree(root: &Path, entries: &[GitTreeEntry]) -> Result<String, ExecError> {
        let mut sorted = entries.to_vec();
        sorted.sort_by(|a, b| a.path.cmp(&b.path));
        let mut input = String::new();
        for e in &sorted {
            input.push_str(&format!("{} {} {}\t{}\n", e.mode, e.kind, e.oid, e.path));
        }
        git_with_stdin(root, &["mktree"], input.as_bytes())
    }

    /// Create a commit object via `git commit-tree` without touching `HEAD` or any working tree.
    pub fn commit_tree(
        root: &Path,
        tree_oid: &str,
        parent_oid: Option<&str>,
        message: &str,
        by: &str,
    ) -> Result<String, ExecError> {
        let author = if by.trim().is_empty() {
            "fl"
        } else {
            by.trim()
        };
        let email = format!("{author}@fl.local");
        let mut cmd = Command::new("git");
        cmd.current_dir(root)
            .env("GIT_AUTHOR_NAME", author)
            .env("GIT_AUTHOR_EMAIL", &email)
            .env("GIT_COMMITTER_NAME", author)
            .env("GIT_COMMITTER_EMAIL", &email)
            .arg("commit-tree")
            .arg(tree_oid);
        if let Some(p) = parent_oid {
            cmd.arg("-p").arg(p);
        }
        cmd.arg("-m").arg(message);
        let out = cmd
            .output()
            .map_err(|e| ExecError::Git(format!("could not run git: {e}")))?;
        if !out.status.success() {
            return Err(ExecError::Git(format!(
                "git commit-tree failed in {}: {}",
                root.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }

    /// Push `new_oid` to `refs/heads/<branch>` on `<remote>` with `--force-with-lease` compare-and-swap.
    pub fn push_cas(
        root: &Path,
        remote: &str,
        branch: &str,
        expected_old_oid: Option<&str>,
        new_oid: &str,
    ) -> Result<PushCasOutcome, ExecError> {
        let lease = match expected_old_oid {
            Some(old) => format!("--force-with-lease=refs/heads/{branch}:{old}"),
            None => format!("--force-with-lease=refs/heads/{branch}:"),
        };
        let refspec = format!("{new_oid}:refs/heads/{branch}");
        let out = Command::new("git")
            .args(["push", "--porcelain", &lease, remote, &refspec])
            .current_dir(root)
            .output()
            .map_err(|e| ExecError::Git(format!("could not run git: {e}")))?;
        if out.status.success() {
            return Ok(PushCasOutcome::Updated);
        }
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let combined = format!("{stdout}\n{stderr}");
        if combined.contains("stale info")
            || combined.contains("rejected")
            || combined.contains("fetch first")
            || combined.contains("non-fast-forward")
        {
            Ok(PushCasOutcome::Rejected)
        } else {
            Err(ExecError::Git(format!(
                "git push --force-with-lease failed in {}: {}",
                root.display(),
                stderr.trim()
            )))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitTreeEntry {
    pub mode: String,
    pub kind: String,
    pub oid: String,
    pub path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushCasOutcome {
    Updated,
    Rejected,
}

fn git_with_stdin(root: &Path, args: &[&str], stdin_bytes: &[u8]) -> Result<String, ExecError> {
    use std::io::Write;
    use std::process::Stdio;

    let mut child = Command::new("git")
        .args(args)
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| ExecError::Git(format!("could not run git: {e}")))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(stdin_bytes)
            .map_err(|e| ExecError::Git(format!("could not write to git stdin: {e}")))?;
    }
    let out = child
        .wait_with_output()
        .map_err(|e| ExecError::Git(format!("could not wait on git: {e}")))?;
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

    #[test]
    fn plumbing_hash_cat_mktree_commit_tree_and_ls_tree_work() {
        let d = repo();
        let blob_oid = Git::hash_object(d.path(), b"hello world\n").unwrap();
        assert_eq!(blob_oid.len(), 40);
        let content = Git::cat_blob(d.path(), &blob_oid).unwrap();
        assert_eq!(content, b"hello world\n");

        let entry = GitTreeEntry {
            mode: "100644".into(),
            kind: "blob".into(),
            oid: blob_oid.clone(),
            path: "greeting.txt".into(),
        };
        let tree_oid = Git::mktree(d.path(), &[entry]).unwrap();
        assert_eq!(tree_oid.len(), 40);

        let parent = Git::head(d.path()).unwrap();
        let commit_oid = Git::commit_tree(
            d.path(),
            &tree_oid,
            Some(&parent),
            "custom plumbing commit",
            "tester",
        )
        .unwrap();
        assert_eq!(commit_oid.len(), 40);

        let is_anc = Git::is_ancestor(d.path(), &parent, &commit_oid).unwrap();
        assert!(is_anc);
        let revs = Git::rev_list_first_parent(d.path(), Some(&parent), &commit_oid).unwrap();
        assert_eq!(revs, vec![commit_oid.clone()]);

        let listed = Git::ls_tree(d.path(), &commit_oid).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].path, "greeting.txt");
        assert_eq!(listed[0].oid, blob_oid);
    }
}
