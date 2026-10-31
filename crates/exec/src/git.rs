use crate::population::{ChangedPaths, ExecError};
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Git;

fn git_raw(root: &Path, args: &[&str]) -> Result<Vec<u8>, ExecError> {
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
    Ok(out.stdout)
}

fn git(root: &Path, args: &[&str]) -> Result<String, ExecError> {
    Ok(String::from_utf8_lossy(&git_raw(root, args)?)
        .trim()
        .to_string())
}

/// A revision, object id, remote or branch name that is safe to put on a git command line.
///
/// Such a value is passed as a positional argument, so one starting with `-` would be read as an
/// option (`--output=<file>` on `git diff`, `--upload-pack=<cmd>` on `git fetch`). Git ref names
/// cannot start with `-` or hold control characters, so refusing both loses nothing.
fn plain_arg<'a>(what: &str, value: &'a str) -> Result<&'a str, ExecError> {
    if value.is_empty() || value.starts_with('-') || value.bytes().any(|b| b.is_ascii_control()) {
        return Err(ExecError::Git(format!(
            "refusing to pass {what} `{}` to git: it is empty, starts with `-`, or has a control character",
            value.escape_debug()
        )));
    }
    Ok(value)
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
    ///
    /// Read with `-z`: without it git quotes a path holding non-ASCII bytes, a quote or a
    /// newline (`"caf\303\251.rs"`), and the quoted text would name a file that does not exist.
    pub fn changed_between(root: &Path, from: &str, to: &str) -> Result<Vec<PathBuf>, ExecError> {
        let (from, to) = (plain_arg("revision", from)?, plain_arg("revision", to)?);
        let raw = git_raw(root, &["diff", "--name-only", "-z", from, to])?;
        Ok(raw
            .split(|b| *b == 0)
            .filter(|name| !name.is_empty())
            .map(|name| {
                use std::os::unix::ffi::OsStrExt;
                root.join(std::ffi::OsStr::from_bytes(name))
            })
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
        let (remote, branch) = (plain_arg("remote", remote)?, plain_arg("branch", branch)?);
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
        let remote = plain_arg("remote", remote)?;
        let (branch, local_ref) = (
            plain_arg("branch", branch)?,
            plain_arg("local ref", local_ref)?,
        );
        let spec = format!("+refs/heads/{branch}:{local_ref}");
        git(root, &["fetch", "--no-tags", "--quiet", remote, &spec])?;
        Ok(())
    }

    /// Return `true` iff `ancestor` is an ancestor of `descendant` in `root`'s object database.
    pub fn is_ancestor(root: &Path, ancestor: &str, descendant: &str) -> Result<bool, ExecError> {
        let (ancestor, descendant) = (
            plain_arg("revision", ancestor)?,
            plain_arg("revision", descendant)?,
        );
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
        let tip = plain_arg("revision", tip)?;
        let range = match after {
            Some(base) => format!("{}..{tip}", plain_arg("revision", base)?),
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

    /// List all objects recursively under `treeish` (`git ls-tree -r -z <treeish>`).
    ///
    /// Read with `-z` so a path with a non-ASCII byte, quote, tab or newline comes back as the
    /// file's real name, not git's C-quoted form.
    ///
    /// Limit: [`GitTreeEntry::path`] is a `String`, so a path that is not valid UTF-8 is read
    /// lossily (invalid bytes become U+FFFD) and is not the file's exact name. Such a name cannot
    /// round-trip through [`Git::mktree`].
    pub fn ls_tree(root: &Path, treeish: &str) -> Result<Vec<GitTreeEntry>, ExecError> {
        let treeish = plain_arg("tree-ish", treeish)?;
        let raw = git_raw(root, &["ls-tree", "-r", "-z", treeish])?;
        let mut entries = Vec::new();
        for record in raw.split(|b| *b == 0) {
            if record.is_empty() {
                continue;
            }
            // Lossy on purpose: see the UTF-8 limit in the doc comment.
            let record = String::from_utf8_lossy(record);
            let Some((meta, path)) = record.split_once('\t') else {
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
        let oid = plain_arg("object id", oid)?;
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
        // `-z`: NUL-terminated records whose names are raw, so a name holding a quote, tab or
        // newline is stored as written and not mangled by C-style (un)quoting.
        let mut input = String::new();
        for e in &sorted {
            input.push_str(&format!("{} {} {}\t{}\0", e.mode, e.kind, e.oid, e.path));
        }
        git_with_stdin(root, &["mktree", "-z"], input.as_bytes())
    }

    /// Create a commit object via `git commit-tree` without touching `HEAD` or any working tree.
    pub fn commit_tree(
        root: &Path,
        tree_oid: &str,
        parent_oid: Option<&str>,
        message: &str,
        by: &str,
    ) -> Result<String, ExecError> {
        let tree_oid = plain_arg("tree id", tree_oid)?;
        let parent_oid = parent_oid.map(|p| plain_arg("parent id", p)).transpose()?;
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
        let (remote, branch, new_oid) = (
            plain_arg("remote", remote)?,
            plain_arg("branch", branch)?,
            plain_arg("object id", new_oid)?,
        );
        let expected_old_oid = expected_old_oid
            .map(|old| plain_arg("object id", old))
            .transpose()?;
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

    #[test]
    fn changed_between_reports_real_names_for_paths_git_would_quote() {
        let d = repo();
        let before = Git::head(d.path()).unwrap();
        // Non-ASCII, a space, a double quote and a leading dash: all forms git C-quotes or that
        // an option parser could misread.
        let names = ["café.rs", "with space.rs", "q\"uote.rs", "-dash.rs"];
        for name in names {
            fs::write(d.path().join(name), "x").unwrap();
        }
        let after = commit(d.path(), "odd names");
        let changed = Git::changed_between(d.path(), &before, &after).unwrap();
        for name in names {
            let want = d.path().join(name);
            assert!(changed.contains(&want), "{name:?} missing from {changed:?}");
            assert!(want.exists());
        }
        assert_eq!(changed.len(), names.len());
    }

    #[test]
    fn ls_tree_and_mktree_round_trip_names_git_would_quote() {
        let d = repo();
        let names = ["café.rs", "tab\there.rs", "q\"uote.rs", "sp ace.rs"];
        for name in names {
            let Ok(()) = fs::write(d.path().join(name), "x") else {
                continue;
            };
        }
        let commit_oid = commit(d.path(), "odd names");
        let entries = Git::ls_tree(d.path(), &commit_oid).unwrap();
        for name in names {
            assert!(
                entries.iter().any(|e| e.path == name),
                "{name:?} not listed verbatim: {:?}",
                entries.iter().map(|e| &e.path).collect::<Vec<_>>()
            );
        }
        // Rebuild a flat tree from the listing and read it back unchanged.
        let flat: Vec<GitTreeEntry> = entries
            .iter()
            .filter(|e| e.kind == "blob" && !e.path.contains('/'))
            .cloned()
            .collect();
        let tree = Git::mktree(d.path(), &flat).unwrap();
        let mut back: Vec<String> = Git::ls_tree(d.path(), &tree)
            .unwrap()
            .into_iter()
            .map(|e| e.path)
            .collect();
        let mut want: Vec<String> = flat.iter().map(|e| e.path.clone()).collect();
        back.sort();
        want.sort();
        assert_eq!(back, want);
    }

    #[test]
    fn values_that_git_would_read_as_options_are_refused_before_git_runs() {
        let d = repo();
        let head = Git::head(d.path()).unwrap();
        let sentinel = d.path().join("pwned");
        let evil_output = format!("--output={}", sentinel.display());
        let evil_pack = "--upload-pack=touch pwned";
        assert!(Git::changed_between(d.path(), &evil_output, &head).is_err());
        assert!(Git::changed_between(d.path(), &head, &evil_output).is_err());
        assert!(Git::ls_tree(d.path(), "-r").is_err());
        assert!(Git::cat_blob(d.path(), "--batch").is_err());
        assert!(Git::is_ancestor(d.path(), "--all", &head).is_err());
        assert!(Git::rev_list_first_parent(d.path(), None, "--all").is_err());
        assert!(Git::rev_list_first_parent(d.path(), Some("--all"), &head).is_err());
        assert!(Git::ls_remote_branch(d.path(), evil_pack, "main").is_err());
        assert!(Git::fetch_branch(d.path(), evil_pack, "main", "refs/x").is_err());
        assert!(Git::push_cas(d.path(), evil_pack, "main", None, &head).is_err());
        assert!(Git::push_cas(d.path(), "origin", "-f", None, &head).is_err());
        assert!(Git::commit_tree(d.path(), "--help", None, "m", "t").is_err());
        assert!(Git::commit_tree(d.path(), &head, Some("-S"), "m", "t").is_err());
        assert!(Git::changed_between(d.path(), "", &head).is_err());
        assert!(Git::changed_between(d.path(), "a\nb", &head).is_err());
        assert!(!sentinel.exists(), "an option-shaped argument reached git");
    }

    #[test]
    fn rev_list_first_parent_orders_oldest_first_and_honours_the_lower_bound() {
        let d = repo();
        let first = Git::head(d.path()).unwrap();
        fs::write(d.path().join("b.rs"), "b").unwrap();
        let second = commit(d.path(), "second");
        fs::write(d.path().join("c.rs"), "c").unwrap();
        let third = commit(d.path(), "third");
        assert_eq!(
            Git::rev_list_first_parent(d.path(), None, &third).unwrap(),
            vec![first.clone(), second.clone(), third.clone()]
        );
        assert_eq!(
            Git::rev_list_first_parent(d.path(), Some(&first), &third).unwrap(),
            vec![second, third.clone()]
        );
        assert!(
            Git::rev_list_first_parent(d.path(), Some(&third), &third)
                .unwrap()
                .is_empty()
        );
        assert!(Git::is_ancestor(d.path(), &first, &third).unwrap());
        assert!(!Git::is_ancestor(d.path(), &third, &first).unwrap());
    }
}
