//! Bounded Git worktree operations. These primitives do not authenticate callers or reserve
//! paths: the coordinator must authorize and serialize an exact request before invoking them.
//! Filesystem names and Git configuration can change between checks and Git's effect; callers
//! must fence concurrent writers to the repository and approved parent until reconciliation.

use std::ffi::{OsStr, OsString};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const DEADLINE: Duration = Duration::from_secs(30);
const OUTPUT_LIMIT: usize = 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum WorktreeError {
    #[error("invalid worktree request: {0}")]
    Invalid(&'static str),
    #[error("worktree operation could not be verified: {0}")]
    Verification(&'static str),
    #[error("git worktree operation failed: {0}")]
    Git(String),
    #[error("git operation timed out; its outcome may be ambiguous")]
    Timeout,
    #[error("git output exceeded its capture limit")]
    OutputLimit,
    #[error("filesystem or process error: {0}")]
    Io(#[from] io::Error),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeRequest {
    /// Exact, already-authorized project root; caller verifies project membership.
    pub project_root: PathBuf,
    pub git_common_dir: PathBuf,
    /// Existing, authorized parent. Its direct child must not yet exist.
    pub approved_parent: PathBuf,
    pub path: PathBuf,
    pub branch_ref: String,
    pub base_oid: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeRegistration {
    pub path: PathBuf,
    pub git_common_dir: PathBuf,
    pub branch_ref: String,
}

fn safe_path(path: &Path) -> Result<(), WorktreeError> {
    if !path.is_absolute()
        || path
            .as_os_str()
            .as_encoded_bytes()
            .iter()
            .any(u8::is_ascii_control)
        || path.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::CurDir | Component::Prefix(_)
            )
        })
    {
        return Err(WorktreeError::Invalid(
            "path must be absolute without controls or traversal",
        ));
    }
    Ok(())
}

/// Every existing component must itself be a directory and not a symbolic link.
fn canonical_dir_no_symlinks(path: &Path) -> Result<PathBuf, WorktreeError> {
    safe_path(path)?;
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        let meta = std::fs::symlink_metadata(&current)?;
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return Err(WorktreeError::Invalid(
                "directory contains a symlink or non-directory",
            ));
        }
    }
    let canonical = path.canonicalize()?;
    if canonical != path {
        return Err(WorktreeError::Invalid("directory is not canonical"));
    }
    Ok(canonical)
}

fn branch_name(full: &str) -> Result<&str, WorktreeError> {
    let name = full
        .strip_prefix("refs/heads/")
        .ok_or(WorktreeError::Invalid("branch must be under refs/heads"))?;
    if name.is_empty()
        || name.starts_with('-')
        || name.starts_with('/')
        || name.ends_with('/')
        || name.ends_with('.')
        || name.contains("..")
        || name.contains("@{")
        || name.contains("//")
        || name.bytes().any(|b| {
            b <= b' ' || b == 127 || matches!(b, b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
        })
        || name
            .split('/')
            .any(|s| s.is_empty() || s.starts_with('.') || s.ends_with(".lock"))
    {
        return Err(WorktreeError::Invalid("invalid branch ref"));
    }
    Ok(name)
}

fn validate(request: &WorktreeRequest) -> Result<&str, WorktreeError> {
    canonical_dir_no_symlinks(&request.project_root)?;
    canonical_dir_no_symlinks(&request.git_common_dir)?;
    canonical_dir_no_symlinks(&request.approved_parent)?;
    safe_path(&request.path)?;
    if request.path.parent() != Some(request.approved_parent.as_path())
        || request.path.file_name().is_none()
    {
        return Err(WorktreeError::Invalid(
            "worktree path must be a direct child of approved parent",
        ));
    }
    if !(request.base_oid.len() == 40 || request.base_oid.len() == 64)
        || !request.base_oid.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(WorktreeError::Invalid(
            "base oid must be a full hexadecimal Git object id",
        ));
    }
    branch_name(&request.branch_ref)
}

struct GitOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Drain both pipes concurrently, bounding both retained bytes and elapsed time. A stuck child or
/// descendants holding a pipe are killed as one process group on Unix. Output truncation refuses.
fn run_git(root: &Path, args: &[OsString]) -> Result<GitOutput, WorktreeError> {
    let mut cmd = Command::new("git");
    cmd.current_dir(root)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.attributesFile=/dev/null",
            "-c",
            "submodule.recurse=false",
        ])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    run_bounded(cmd, DEADLINE, OUTPUT_LIMIT)
}

fn run_bounded(
    mut cmd: Command,
    limit: Duration,
    output_limit: usize,
) -> Result<GitOutput, WorktreeError> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn()?;
    let pgid = child.id();
    let (tx, rx) = mpsc::channel();
    for (idx, pipe) in [
        (
            0,
            child
                .stdout
                .take()
                .map(|p| Box::new(p) as Box<dyn Read + Send>),
        ),
        (
            1,
            child
                .stderr
                .take()
                .map(|p| Box::new(p) as Box<dyn Read + Send>),
        ),
    ] {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let result = pipe
                .ok_or_else(|| io::Error::other("missing pipe"))
                .and_then(|p| p.take((output_limit + 1) as u64).read_to_end(&mut bytes));
            let _ = tx.send((idx, result.map(|_| bytes)));
        });
    }
    drop(tx);
    let deadline = Instant::now() + limit;
    let mut status = None;
    let mut pipes: [Option<Vec<u8>>; 2] = [None, None];
    loop {
        while let Ok((idx, result)) = rx.try_recv() {
            match result {
                Ok(bytes) => pipes[idx] = Some(bytes),
                Err(error) => {
                    kill(&mut child, pgid);
                    return Err(error.into());
                }
            }
        }
        if pipes.iter().flatten().any(|v| v.len() > output_limit) {
            kill(&mut child, pgid);
            return Err(WorktreeError::OutputLimit);
        }
        if status.is_none() {
            match child.try_wait() {
                Ok(value) => status = value,
                Err(error) => {
                    kill(&mut child, pgid);
                    return Err(error.into());
                }
            }
        }
        if let (Some(status), [Some(stdout), Some(stderr)]) = (status, &pipes) {
            return Ok(GitOutput {
                status,
                stdout: stdout.clone(),
                stderr: stderr.clone(),
            });
        }
        if Instant::now() >= deadline {
            kill(&mut child, pgid);
            return Err(WorktreeError::Timeout);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn kill(child: &mut std::process::Child, pgid: u32) {
    #[cfg(unix)]
    {
        // SAFETY: this process was spawned as a new process group with the child's PID as PGID.
        unsafe {
            libc::kill(-(pgid as i32), libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    let _ = pgid;
    let _ = child.kill();
    let _ = child.wait();
}

fn git(root: &Path, args: &[&OsStr]) -> Result<Vec<u8>, WorktreeError> {
    let args: Vec<_> = args.iter().map(|s| s.to_os_string()).collect();
    let out = run_git(root, &args)?;
    if !out.status.success() {
        return Err(WorktreeError::Git(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }
    Ok(out.stdout)
}
fn text_path(bytes: Vec<u8>) -> Result<PathBuf, WorktreeError> {
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| WorktreeError::Verification("non-UTF8 Git path"))?;
    let path = PathBuf::from(text.trim_end_matches('\n'));
    canonical_dir_no_symlinks(&path)
}
fn common_dir(root: &Path) -> Result<PathBuf, WorktreeError> {
    text_path(git(
        root,
        &[
            OsStr::new("rev-parse"),
            OsStr::new("--path-format=absolute"),
            OsStr::new("--git-common-dir"),
        ],
    )?)
}
fn verify_project(request: &WorktreeRequest) -> Result<(), WorktreeError> {
    let top = text_path(git(
        &request.project_root,
        &[
            OsStr::new("rev-parse"),
            OsStr::new("--path-format=absolute"),
            OsStr::new("--show-toplevel"),
        ],
    )?)?;
    if top != request.project_root || common_dir(&request.project_root)? != request.git_common_dir {
        return Err(WorktreeError::Verification(
            "project root or Git common directory differs",
        ));
    }
    Ok(())
}

/// Create a fresh branch and worktree. Errors after the Git effect may mean it was created:
/// reconcile with [`read_worktree_registration`]; never blindly retry or remove it.
pub fn create_worktree(request: &WorktreeRequest) -> Result<WorktreeRegistration, WorktreeError> {
    let branch = validate(request)?;
    verify_project(request)?;
    match request.path.symlink_metadata() {
        Ok(_) => return Err(WorktreeError::Invalid("worktree path already exists")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    // Conditional includes can activate different filters for the newly created Git directory.
    // Refuse them because checking configuration only in the originating checkout cannot prove
    // which filter commands the checkout will load for the new worktree.
    let conditional = run_git(
        &request.project_root,
        &[
            "config".into(),
            "--null".into(),
            "--get-regexp".into(),
            r"^includeif\.".into(),
        ],
    )?;
    match conditional.status.code() {
        Some(1) => {}
        Some(0) => {
            return Err(WorktreeError::Invalid(
                "conditional Git configuration includes are configured",
            ));
        }
        _ => {
            return Err(WorktreeError::Git(
                String::from_utf8_lossy(&conditional.stderr).into_owned(),
            ));
        }
    }
    // Checkout filter drivers can execute arbitrary code. Refuse repositories with an external
    // smudge/process filter rather than invoking one under this primitive.
    let out = run_git(
        &request.project_root,
        &[
            "config".into(),
            "--null".into(),
            "--get-regexp".into(),
            r"^filter\..*\.(smudge|process)$".into(),
        ],
    )?;
    match out.status.code() {
        Some(1) => {}
        Some(0) => {
            return Err(WorktreeError::Invalid(
                "external Git checkout filters are configured",
            ));
        }
        _ => {
            return Err(WorktreeError::Git(
                String::from_utf8_lossy(&out.stderr).into_owned(),
            ));
        }
    }
    let format = git(
        &request.project_root,
        &[OsStr::new("rev-parse"), OsStr::new("--show-object-format")],
    )?;
    let expected_len = match format.as_slice() {
        b"sha1\n" => 40,
        b"sha256\n" => 64,
        _ => return Err(WorktreeError::Verification("unsupported Git object format")),
    };
    if request.base_oid.len() != expected_len {
        return Err(WorktreeError::Invalid(
            "base oid length does not match repository object format",
        ));
    }
    let kind = git(
        &request.project_root,
        &[
            OsStr::new("cat-file"),
            OsStr::new("-t"),
            OsStr::new(&request.base_oid),
        ],
    )?;
    if kind != b"commit\n" {
        return Err(WorktreeError::Invalid("base oid must identify a commit"));
    }
    git(
        &request.project_root,
        &[
            OsStr::new("worktree"),
            OsStr::new("add"),
            OsStr::new("-b"),
            OsStr::new(branch),
            request.path.as_os_str(),
            OsStr::new(&request.base_oid),
        ],
    )?;
    read_worktree_registration(request)
}

/// Read Git's NUL-framed registration and compare path, branch, and common Git directory.
pub fn read_worktree_registration(
    request: &WorktreeRequest,
) -> Result<WorktreeRegistration, WorktreeError> {
    validate(request)?;
    verify_project(request)?;
    let path = canonical_dir_no_symlinks(&request.path)?;
    let data = git(
        &request.project_root,
        &[
            OsStr::new("worktree"),
            OsStr::new("list"),
            OsStr::new("--porcelain"),
            OsStr::new("-z"),
        ],
    )?;
    let mut found = false;
    for record in data
        .split(|b| *b == 0)
        .collect::<Vec<_>>()
        .split(|item| item.is_empty())
    {
        let mut rec_path = None;
        let mut rec_branch = None;
        for field in record {
            if let Some(v) = field.strip_prefix(b"worktree ") {
                rec_path = Some(v);
            }
            if let Some(v) = field.strip_prefix(b"branch ") {
                rec_branch = Some(v);
            }
        }
        if rec_path == Some(path.as_os_str().as_encoded_bytes()) {
            if found || rec_branch != Some(request.branch_ref.as_bytes()) {
                return Err(WorktreeError::Verification(
                    "branch or duplicate registration differs",
                ));
            }
            found = true;
        }
    }
    if !found || common_dir(&path)? != request.git_common_dir {
        return Err(WorktreeError::Verification(
            "worktree registration or common directory differs",
        ));
    }
    Ok(WorktreeRegistration {
        path,
        git_common_dir: request.git_common_dir.clone(),
        branch_ref: request.branch_ref.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn process_timeout_and_output_cap_fail_closed() {
        let mut sleeper = Command::new("sh");
        sleeper
            .args(["-c", "sleep 5"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        assert!(matches!(
            run_bounded(sleeper, Duration::from_millis(100), 1024),
            Err(WorktreeError::Timeout)
        ));
        let mut noisy = Command::new("sh");
        noisy
            .args(["-c", "head -c 4096 /dev/zero"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        assert!(matches!(
            run_bounded(noisy, Duration::from_secs(2), 100),
            Err(WorktreeError::OutputLimit)
        ));
    }
}
