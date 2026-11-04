use fl_exec::worktree::{WorktreeRequest, create_worktree, read_worktree_registration};
use std::path::Path;
use std::process::Command;

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn fixture() -> (
    tempfile::TempDir,
    std::path::PathBuf,
    std::path::PathBuf,
    String,
) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    let parent = temp.path().join("worktrees");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(&parent).unwrap();
    git(&root, &["init", "-q"]);
    git(&root, &["config", "user.email", "a@example.com"]);
    git(&root, &["config", "user.name", "A"]);
    std::fs::write(root.join("file"), "fixture").unwrap();
    git(&root, &["add", "file"]);
    git(&root, &["commit", "-qm", "fixture"]);
    let oid = git(&root, &["rev-parse", "HEAD"]);
    (temp, root, parent, oid)
}

#[test]
fn creates_and_verifies_exact_registration() {
    let (_temp, root, parent, oid) = fixture();
    let common = root.join(".git").canonicalize().unwrap();
    let path = parent.join("with space");
    let request = WorktreeRequest {
        project_root: root.clone(),
        git_common_dir: common.clone(),
        approved_parent: parent.clone(),
        path: path.clone(),
        branch_ref: "refs/heads/test/fresh".into(),
        base_oid: oid,
    };
    let registered = create_worktree(&request).unwrap();
    assert_eq!(registered.path, path.canonicalize().unwrap());
    assert_eq!(registered.branch_ref, request.branch_ref);
    assert_eq!(registered.git_common_dir, common);
    assert_eq!(
        std::fs::read_to_string(path.join("file")).unwrap(),
        "fixture"
    );
    assert_eq!(read_worktree_registration(&request).unwrap(), registered);
}

#[test]
fn rejects_hostile_inputs_and_existing_paths_without_side_effects() {
    let (_temp, root, parent, oid) = fixture();
    let common = root.join(".git").canonicalize().unwrap();
    let base = WorktreeRequest {
        project_root: root.clone(),
        git_common_dir: common,
        approved_parent: parent.clone(),
        path: parent.join("new"),
        branch_ref: "refs/heads/safe".into(),
        base_oid: oid,
    };
    for bad in [
        "-danger",
        "refs/heads/-danger",
        "refs/heads/a..b",
        "refs/heads/a\nb",
        "refs/heads/a.lock",
    ] {
        let mut request = base.clone();
        request.branch_ref = bad.into();
        assert!(create_worktree(&request).is_err(), "{bad:?}");
    }
    let mut escaped = base.clone();
    escaped.path = parent.join("../escape");
    assert!(create_worktree(&escaped).is_err());
    std::fs::create_dir(&base.path).unwrap();
    assert!(create_worktree(&base).is_err());
    assert_eq!(
        git(&root, &["worktree", "list", "--porcelain"])
            .matches("worktree ")
            .count(),
        1
    );
}

#[cfg(unix)]
#[test]
fn rejects_symlinked_parent() {
    use std::os::unix::fs::symlink;
    let (temp, root, parent, oid) = fixture();
    let link = temp.path().join("link");
    symlink(&parent, &link).unwrap();
    let request = WorktreeRequest {
        git_common_dir: root.join(".git").canonicalize().unwrap(),
        project_root: root,
        approved_parent: link.clone(),
        path: link.join("new"),
        branch_ref: "refs/heads/safe".into(),
        base_oid: oid,
    };
    assert!(create_worktree(&request).is_err());
}

#[cfg(unix)]
#[test]
fn suppresses_repo_hooks_and_refuses_external_filter_drivers() {
    use std::os::unix::fs::PermissionsExt;
    let (temp, root, parent, oid) = fixture();
    let marker = temp.path().join("hook-ran");
    let hook = root.join(".git/hooks/post-checkout");
    std::fs::write(&hook, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    let request = WorktreeRequest {
        git_common_dir: root.join(".git").canonicalize().unwrap(),
        project_root: root.clone(),
        approved_parent: parent.clone(),
        path: parent.join("a"),
        branch_ref: "refs/heads/safe-a".into(),
        base_oid: oid.clone(),
    };
    create_worktree(&request).unwrap();
    assert!(!marker.exists(), "Git hook must not run");
    git(
        &root,
        &["config", "filter.bad.smudge", "touch /tmp/unwanted"],
    );
    let second = WorktreeRequest {
        path: parent.join("b"),
        branch_ref: "refs/heads/safe-b".into(),
        ..request
    };
    assert!(create_worktree(&second).is_err());
    assert!(!second.path.exists());
}

#[test]
fn refuses_different_common_directory_and_registration_branch() {
    let (_temp, root, parent, oid) = fixture();
    let request = WorktreeRequest {
        git_common_dir: root.join(".git").canonicalize().unwrap(),
        project_root: root.clone(),
        approved_parent: parent.clone(),
        path: parent.join("a"),
        branch_ref: "refs/heads/safe-a".into(),
        base_oid: oid,
    };
    create_worktree(&request).unwrap();
    let mut wrong_branch = request.clone();
    wrong_branch.branch_ref = "refs/heads/different".into();
    assert!(read_worktree_registration(&wrong_branch).is_err());
    let mut wrong_common = request.clone();
    wrong_common.git_common_dir = parent;
    assert!(read_worktree_registration(&wrong_common).is_err());
    assert!(request.path.exists());
}

#[test]
fn rejects_non_commit_oids_and_nested_repositories() {
    let (_temp, root, parent, _oid) = fixture();
    let blob = git(&root, &["hash-object", "-w", "file"]);
    let request = WorktreeRequest {
        git_common_dir: root.join(".git").canonicalize().unwrap(),
        project_root: root.clone(),
        approved_parent: parent.clone(),
        path: parent.join("child"),
        branch_ref: "refs/heads/safe".into(),
        base_oid: blob,
    };
    assert!(create_worktree(&request).is_err());
    assert!(!request.path.exists());
    let nested = root.join("nested");
    std::fs::create_dir(&nested).unwrap();
    git(&nested, &["init", "-q"]);
    let nested_request = WorktreeRequest {
        project_root: nested,
        ..request
    };
    assert!(create_worktree(&nested_request).is_err());
}

#[test]
fn refuses_conditional_config_that_could_enable_filters_in_new_gitdir() {
    let (_temp, root, parent, oid) = fixture();
    git(
        &root,
        &[
            "config",
            "includeIf.gitdir:/unlikely/location/.path",
            "/tmp/untrusted-gitconfig",
        ],
    );
    let request = WorktreeRequest {
        git_common_dir: root.join(".git").canonicalize().unwrap(),
        project_root: root,
        approved_parent: parent.clone(),
        path: parent.join("a"),
        branch_ref: "refs/heads/safe-a".into(),
        base_oid: oid,
    };
    assert!(create_worktree(&request).is_err());
    assert!(!request.path.exists());
}

#[cfg(unix)]
#[test]
fn refuses_partial_clone_without_running_remote_for_missing_commit_or_blob() {
    use std::os::unix::fs::PermissionsExt;

    for missing_blob in [false, true] {
        let (temp, root, parent, oid) = fixture();
        let blob = git(&root, &["rev-parse", "HEAD:file"]);
        let marker = temp.path().join("remote-executed");
        let upload = temp.path().join("upload-pack");
        std::fs::write(
            &upload,
            format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&upload, std::fs::Permissions::from_mode(0o755)).unwrap();
        git(&root, &["config", "remote.origin.url", "/missing-remote"]);
        git(
            &root,
            &[
                "config",
                "remote.origin.uploadpack",
                upload.to_str().unwrap(),
            ],
        );
        git(&root, &["config", "remote.origin.promisor", "true"]);
        git(&root, &["config", "extensions.partialClone", "origin"]);
        if missing_blob {
            let object = root.join(".git/objects").join(&blob[..2]).join(&blob[2..]);
            std::fs::remove_file(object).unwrap();
        }
        let request = WorktreeRequest {
            git_common_dir: root.join(".git").canonicalize().unwrap(),
            project_root: root,
            approved_parent: parent.clone(),
            path: parent.join("child"),
            branch_ref: "refs/heads/safe".into(),
            base_oid: if missing_blob { oid } else { "1".repeat(40) },
        };
        assert!(create_worktree(&request).is_err());
        assert!(
            !marker.exists(),
            "missing_blob={missing_blob}: remote command ran"
        );
        assert!(
            !request.path.exists(),
            "missing_blob={missing_blob}: worktree created"
        );
    }
}

#[test]
fn refuses_each_partial_clone_configuration_even_with_all_objects_present() {
    for (name, value) in [
        ("extensions.partialClone", "origin"),
        ("remote.origin.promisor", "false"),
        ("remote.origin.partialCloneFilter", "blob:none"),
    ] {
        let (_temp, root, parent, oid) = fixture();
        git(&root, &["config", name, value]);
        let request = WorktreeRequest {
            git_common_dir: root.join(".git").canonicalize().unwrap(),
            project_root: root,
            approved_parent: parent.clone(),
            path: parent.join("child"),
            branch_ref: "refs/heads/safe".into(),
            base_oid: oid,
        };
        let error = create_worktree(&request).unwrap_err();
        assert!(
            error.to_string().contains("partial-clone or promisor"),
            "{name}: {error}"
        );
        assert!(!request.path.exists());
    }
}
