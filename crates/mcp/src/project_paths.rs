//! Bounded, vendor-neutral Git project layout discovery.

use std::path::{Path, PathBuf};

/// Canonical paths for the nearest Git marker above the supplied directory.
///
/// This discovers layout, not repository health: HEAD, objects and refs are not
/// inspected. A directory `.git` marker supplies the repository identity itself;
/// only gitfile markers follow the optional `commondir` indirection. This matches
/// the existing Codex trust resolver, not every possible Git configuration.
#[derive(Debug, PartialEq, Eq)]
pub struct GitProjectLayout {
    /// Canonical directory containing the nearest `.git` marker.
    pub worktree_root: PathBuf,
    /// Canonical `.git` directory or gitfile target.
    pub git_dir: PathBuf,
    /// Canonical gitfile `commondir` target when present, otherwise `git_dir`.
    pub common_dir: PathBuf,
    /// Parent of a gitfile's resolved `commondir`, for the existing Codex fallback.
    /// This is a layout convention, not proof that the parent is a checkout.
    pub main_checkout_root: Option<PathBuf>,
}

/// A bounded diagnostic that never includes Git marker contents or parser input.
#[derive(Debug, thiserror::Error)]
#[error("{reason}")]
pub struct GitProjectError {
    // Preserve the project-key suggestion for Codex refusals after discovery.
    pub(crate) worktree_root: Option<PathBuf>,
    reason: String,
}

/// Find the nearest `.git` marker without executing Git or modifying any files.
///
/// `None` means no marker was found. Invalid cwd or present invalid metadata is
/// an error, including dangling symlinks. Gitfiles and their optional `commondir`
/// files must resolve to regular UTF-8 files of at most 8 KiB; their targets must
/// be directories. Directory `.git` markers do not inspect `commondir`.
pub fn resolve_git_project(cwd: &Path) -> Result<Option<GitProjectLayout>, GitProjectError> {
    let mut worktree_root = None;
    let mut resolve = || -> Result<Option<GitProjectLayout>, String> {
        let cwd = cwd
            .canonicalize()
            .map_err(|_| "cwd cannot be resolved".to_string())?;
        if !cwd.is_dir() {
            return Err("cwd is not a directory".into());
        }
        for ancestor in cwd.ancestors() {
            match ancestor.join(".git").symlink_metadata() {
                Ok(_) => {
                    worktree_root = Some(ancestor.to_path_buf());
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err("cannot inspect Git directory marker".into()),
            }
        }
        let Some(project) = &worktree_root else {
            return Ok(None);
        };
        let marker = project.join(".git");
        let directory_marker = marker.is_dir();
        let gitdir = if directory_marker {
            marker
                .canonicalize()
                .map_err(|_| "cannot resolve Git directory")?
        } else {
            let text = read_bounded_file(&marker, 8192)
                .map_err(|why| format!("Git directory marker: {why}"))?;
            let gitdir = text
                .strip_prefix("gitdir:")
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or("invalid Git directory marker")?;
            let gitdir = project
                .join(gitdir)
                .canonicalize()
                .map_err(|_| "cannot resolve Git directory")?;
            if !gitdir.is_dir() {
                return Err("Git directory is not a directory".into());
            }
            gitdir
        };
        let mut layout = GitProjectLayout {
            worktree_root: project.clone(),
            common_dir: gitdir.clone(),
            git_dir: gitdir.clone(),
            main_checkout_root: None,
        };
        if !directory_marker {
            let common_marker = gitdir.join("commondir");
            match common_marker.symlink_metadata() {
                // Submodules and --separate-git-dir repositories do not need
                // commondir. Its absence leaves only the explicit project key.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err("cannot inspect Git common directory marker".into()),
                Ok(_) => {
                    // Inspect without following symlinks above, so a dangling
                    // marker is refused rather than treated as absent.
                    let common = read_bounded_file(&common_marker, 8192)
                        .map_err(|why| format!("Git common directory marker: {why}"))?;
                    if common.trim().is_empty() {
                        return Err("invalid Git common directory".into());
                    }
                    let common = gitdir
                        .join(common.trim())
                        .canonicalize()
                        .map_err(|_| "cannot resolve Git common directory")?;
                    if !common.is_dir() {
                        return Err("Git common directory is not a directory".into());
                    }
                    let main = common
                        .parent()
                        .ok_or("Git common directory has no parent")?;
                    layout.main_checkout_root = Some(main.into());
                    layout.common_dir = common;
                }
            }
        }
        Ok(Some(layout))
    };
    resolve().map_err(|reason| GitProjectError {
        worktree_root,
        reason,
    })
}

/// Metadata prevents opening devices/FIFOs; the limited reader also detects growth or
/// understated metadata, without ever allocating an unbounded configuration string.
pub(crate) fn read_bounded_file(path: &Path, limit: u64) -> Result<String, String> {
    use std::io::Read;
    let unreadable = |e: std::io::Error| {
        if e.kind() == std::io::ErrorKind::NotFound {
            "input does not exist".to_string()
        } else {
            "fl could not read input".to_string()
        }
    };
    let metadata = path.metadata().map_err(unreadable)?;
    if !metadata.is_file() {
        return Err("fl could not read input: not a regular file".into());
    }
    if metadata.len() > limit {
        return Err(format!("input exceeds {limit} byte limit"));
    }
    let file = std::fs::File::open(path).map_err(unreadable)?;
    if !file.metadata().map_err(unreadable)?.is_file() {
        return Err("input is not a regular file".into());
    }
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(unreadable)?;
    if bytes.len() as u64 > limit {
        return Err(format!("input exceeds {limit} byte limit"));
    }
    String::from_utf8(bytes).map_err(|_| "input is not UTF-8".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("project");
        fs::create_dir(&root).unwrap();
        (dir, root)
    }

    fn gitfile(root: &Path, target: &str) -> PathBuf {
        let gitdir = root.join(target);
        fs::create_dir_all(&gitdir).unwrap();
        fs::write(root.join(".git"), format!("gitdir: {target}\n")).unwrap();
        gitdir.canonicalize().unwrap()
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bounded_read_detects_bytes_when_metadata_reports_zero() {
        // procfs reports zero bytes even though this process has a nonempty command line.
        let file = Path::new("/proc/self/cmdline");
        assert_eq!(std::fs::metadata(file).unwrap().len(), 0);
        assert!(
            super::read_bounded_file(file, 1)
                .unwrap_err()
                .contains("1 byte limit")
        );
    }

    #[test]
    fn normal_repository_subdirectory_uses_nearest_root() {
        let (_dir, root) = fixture();
        fs::create_dir(root.join(".git")).unwrap();
        let sub = root.join("src/nested");
        fs::create_dir_all(&sub).unwrap();
        for cwd in [&root, &sub] {
            assert_eq!(
                resolve_git_project(cwd).unwrap(),
                Some(GitProjectLayout {
                    worktree_root: root.clone(),
                    git_dir: root.join(".git"),
                    common_dir: root.join(".git"),
                    main_checkout_root: None,
                })
            );
        }
        fs::create_dir(sub.join(".git")).unwrap();
        assert_eq!(
            resolve_git_project(&sub).unwrap().unwrap().worktree_root,
            sub
        );
    }

    #[test]
    fn no_marker_is_distinct_from_invalid_cwd() {
        let (_dir, root) = fixture();
        assert_eq!(resolve_git_project(&root).unwrap(), None);
        assert!(resolve_git_project(&root.join("missing")).is_err());
        let file = root.join("file");
        fs::write(&file, "content").unwrap();
        assert!(resolve_git_project(&file).is_err());
    }

    #[test]
    fn linked_worktree_resolves_relative_gitdir_and_common_directory() {
        let (dir, root) = fixture();
        let gitdir = gitfile(&root, "../main/.git/worktrees/project");
        fs::write(gitdir.join("commondir"), "../..\n").unwrap();
        let main = dir.path().canonicalize().unwrap().join("main");
        let sub = root.join("src");
        fs::create_dir(&sub).unwrap();
        assert_eq!(
            resolve_git_project(&sub).unwrap(),
            Some(GitProjectLayout {
                worktree_root: root,
                git_dir: gitdir,
                common_dir: main.join(".git"),
                main_checkout_root: Some(main.clone()),
            })
        );
        assert_eq!(
            resolve_git_project(&main).unwrap().unwrap().common_dir,
            main.join(".git")
        );
    }

    #[test]
    fn standalone_and_submodule_gitfiles_need_no_common_or_health_files() {
        for target in ["../separate.git", "../super/.git/modules/project"] {
            let (_dir, root) = fixture();
            let gitdir = gitfile(&root, target);
            assert_eq!(
                resolve_git_project(&root).unwrap(),
                Some(GitProjectLayout {
                    worktree_root: root,
                    git_dir: gitdir.clone(),
                    common_dir: gitdir,
                    main_checkout_root: None,
                })
            );
        }
    }

    #[test]
    fn gitfile_and_common_targets_must_be_existing_directories() {
        for common in [false, true] {
            let (_dir, root) = fixture();
            let marker = if common {
                gitfile(&root, "../gitdir").join("commondir")
            } else {
                root.join(".git")
            };
            let target = root.join("target");
            let prefix = if common { "" } else { "gitdir: " };
            fs::write(&marker, format!("{prefix}{}\n", target.display())).unwrap();
            assert!(resolve_git_project(&root).is_err());
            fs::write(&target, "not a directory").unwrap();
            assert!(resolve_git_project(&root).is_err());
        }
    }

    #[test]
    fn malformed_marker_errors_never_quote_contents() {
        for common in [false, true] {
            let (_dir, root) = fixture();
            let marker = if common {
                gitfile(&root, "../gitdir").join("commondir")
            } else {
                root.join(".git")
            };
            for bytes in [
                b"PRIVATE_TOKEN".to_vec(),
                b"\n".to_vec(),
                vec![0xff],
                vec![b'x'; 8193],
            ] {
                fs::write(&marker, bytes).unwrap();
                let error = resolve_git_project(&root).unwrap_err();
                assert!(!error.to_string().contains("PRIVATE_TOKEN"));
                assert!(!format!("{error:?}").contains("PRIVATE_TOKEN"));
            }
        }
    }

    #[test]
    fn marker_limits_accept_exactly_eight_kib_and_refuse_one_more() {
        let (_dir, root) = fixture();
        let gitdir = gitfile(&root, "../main/.git/worktrees/project");
        for (marker, prefix) in [
            (root.join(".git"), "gitdir: ../main/.git/worktrees/project"),
            (gitdir.join("commondir"), "../.."),
        ] {
            let mut text = prefix.to_string();
            text.push_str(&" ".repeat(8192 - text.len()));
            fs::write(&marker, &text).unwrap();
            assert!(resolve_git_project(&root).is_ok());
            text.push(' ');
            fs::write(&marker, &text).unwrap();
            assert!(
                resolve_git_project(&root)
                    .unwrap_err()
                    .to_string()
                    .contains("8192 byte limit")
            );
            fs::write(&marker, prefix).unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlink_cwd_markers_and_targets_are_canonicalized() {
        use std::os::unix::fs::symlink;
        let (dir, root) = fixture();
        let base = dir.path().canonicalize().unwrap();
        let gitdir = gitfile(&root, "../main/.git/worktrees/project");
        let main = base.join("main");
        symlink(main.join(".git"), base.join("common-alias")).unwrap();
        fs::write(
            base.join("common-text"),
            base.join("common-alias").to_str().unwrap(),
        )
        .unwrap();
        symlink(base.join("common-text"), gitdir.join("commondir")).unwrap();
        symlink(&gitdir, base.join("gitdir-alias")).unwrap();
        fs::write(base.join("git-text"), "gitdir: ../gitdir-alias\n").unwrap();
        fs::remove_file(root.join(".git")).unwrap();
        symlink(base.join("git-text"), root.join(".git")).unwrap();
        symlink(&root, base.join("cwd-alias")).unwrap();
        assert_eq!(
            resolve_git_project(&base.join("cwd-alias")).unwrap(),
            Some(GitProjectLayout {
                worktree_root: root.clone(),
                git_dir: gitdir,
                common_dir: main.join(".git"),
                main_checkout_root: Some(main.clone()),
            })
        );
        fs::remove_file(root.join(".git")).unwrap();
        symlink(main.join(".git"), root.join(".git")).unwrap();
        let layout = resolve_git_project(&root).unwrap().unwrap();
        assert_eq!(layout.git_dir, main.join(".git"));
        assert_eq!(layout.common_dir, main.join(".git"));
        assert_eq!(layout.main_checkout_root, None);
    }

    #[cfg(unix)]
    #[test]
    fn dangling_unreadable_and_nonregular_markers_are_refused() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        use std::os::unix::net::UnixListener;
        for common in [false, true] {
            let (_dir, root) = fixture();
            let gitdir = gitfile(&root, "../main/.git/worktrees/project");
            let (marker, text) = if common {
                (gitdir.join("commondir"), "../..")
            } else {
                fs::remove_file(root.join(".git")).unwrap();
                (root.join(".git"), "gitdir: ../main/.git/worktrees/project")
            };
            symlink("missing-target", &marker).unwrap();
            assert!(resolve_git_project(&root).is_err());
            fs::remove_file(&marker).unwrap();
            let socket = UnixListener::bind(&marker).unwrap();
            assert!(resolve_git_project(&root).is_err());
            drop(socket);
            fs::remove_file(&marker).unwrap();
            fs::write(&marker, text).unwrap();
            fs::set_permissions(&marker, fs::Permissions::from_mode(0o000)).unwrap();
            // Root may bypass mode bits; exercise the actual read boundary.
            if fs::File::open(&marker).is_err() {
                assert!(resolve_git_project(&root).is_err());
            }
            fs::set_permissions(&marker, fs::Permissions::from_mode(0o600)).unwrap();
            if common {
                fs::remove_file(&marker).unwrap();
                fs::create_dir(&marker).unwrap();
                assert!(resolve_git_project(&root).is_err());
            }
        }
    }
}
