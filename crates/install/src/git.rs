//! The git side of `fl install --from-git` (#205 A4b).
//!
//! The checkout the user names is hostile territory: its `.git/config` and `.git/hooks`, its
//! `.gitattributes` and its working tree are whatever anybody put there. So:
//!
//! * Every git command is an argument vector run by [`Git::cmd`]: an EMPTY environment (no
//!   `GIT_DIR`, no `GIT_EXTERNAL_DIFF`, no credentials), the system and global config switched
//!   off, and `-c` overrides that disarm the config keys that make git run a program
//!   (`core.hooksPath`, `core.fsmonitor`, `log.showSignature`, `protocol.*.allow`, ...).
//! * Only plumbing that reads objects runs against that checkout: `rev-parse`, `merge-base`,
//!   `ls-tree`, `cat-file`, `log`. Nothing checks out, merges, commits or fetches there.
//! * The build tree is NOT a checkout of it. [`extract_tree`] writes the commit's blobs itself
//!   (no smudge filter, no `.gitattributes`, no `export-subst`, no hooks, byte-for-byte what
//!   the commit holds), refusing any path that could leave the directory, and a FRESH
//!   repository ([`init_stamp_repo`]) whose only link to the user's one is a read-only
//!   `objects/info/alternates` line gives the build scripts a `HEAD` to stamp.

use crate::error::{Ctx, InstallError, Result};
use crate::process::{self, Spec, absolute_path_entries, which};
use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// Most files one commit may hold for a source build.
pub const MAX_SOURCE_FILES: usize = 50_000;
/// Most bytes one source file may hold.
pub const MAX_SOURCE_FILE_BYTES: u64 = 256 * 1024 * 1024;
/// Most bytes a whole source tree may hold.
pub const MAX_SOURCE_TREE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_LISTING_BYTES: usize = 32 * 1024 * 1024;
const GIT_TIMEOUT: Duration = Duration::from_secs(120);
/// A `refs/remotes/origin/main` older than this many days gets a "may be stale" note.
pub const STALE_DAYS: u64 = 7;

/// The `-c` overrides every git command runs under. Each key is one that makes git execute a
/// program or read from the network when a repository's own config sets it.
pub const HARDENING: &[&str] = &[
    "core.hooksPath=/dev/null",
    "core.fsmonitor=false",
    "core.attributesFile=/dev/null",
    "core.untrackedCache=false",
    "log.showSignature=false",
    "protocol.ext.allow=never",
    "protocol.allow=never",
    "advice.detachedHead=false",
];

/// The environment every git command runs with. NOTHING else is inherited.
pub fn hardened_env(path_var: &OsStr) -> Vec<(OsString, OsString)> {
    let kv = |k: &str, v: &str| (OsString::from(k), OsString::from(v));
    vec![
        (OsString::from("PATH"), absolute_path_entries(path_var)),
        kv("HOME", "/nonexistent-fl-install"),
        kv("GIT_CONFIG_NOSYSTEM", "1"),
        kv("GIT_CONFIG_GLOBAL", "/dev/null"),
        kv("GIT_ATTR_NOSYSTEM", "1"),
        kv("GIT_TERMINAL_PROMPT", "0"),
        kv("GIT_OPTIONAL_LOCKS", "0"),
        kv("GIT_PAGER", "cat"),
        kv("LC_ALL", "C"),
    ]
}

/// A rev is passed to git as ONE argument after `--end-of-options`; it must also be a plain
/// ref-like word: no leading `-`, `^` or `!`, no whitespace or control character, bounded.
pub fn rev_problem(rev: &str) -> Option<&'static str> {
    if rev.is_empty() {
        Some("is empty")
    } else if rev.len() > 200 {
        Some("is too long")
    } else if rev.starts_with('-') {
        Some("starts with `-`")
    } else if rev.starts_with('^') || rev.starts_with('!') {
        Some("starts with `^` or `!`")
    } else if rev.chars().any(|c| c.is_whitespace() || c.is_control()) {
        Some("has whitespace or a control character")
    } else if rev.contains("..") {
        Some("is a range, not one commit")
    } else {
        None
    }
}

/// Why a path inside a commit cannot be written into the build tree.
pub fn tree_path_problem(path: &str) -> Option<&'static str> {
    if path.is_empty() {
        return Some("is empty");
    }
    if path.len() > 1024 {
        return Some("is too long");
    }
    if path.starts_with('/') {
        return Some("is absolute");
    }
    if path.contains('\\') {
        return Some("contains a backslash");
    }
    if path.chars().any(|c| c.is_control()) {
        return Some("contains a control character");
    }
    for seg in path.split('/') {
        if seg.is_empty() || seg == "." || seg == ".." {
            return Some("has an empty, `.` or `..` segment");
        }
        if seg.eq_ignore_ascii_case(".git") {
            return Some("names a `.git` directory");
        }
        if seg.len() > 255 {
            return Some("has a segment that is too long");
        }
    }
    None
}

/// One entry of a commit's tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    pub mode: u32,
    pub oid: String,
    pub path: String,
}

impl TreeEntry {
    pub fn is_blob(&self) -> bool {
        self.mode == 0o100644 || self.mode == 0o100755
    }

    pub fn is_symlink(&self) -> bool {
        self.mode == 0o120000
    }

    pub fn is_submodule(&self) -> bool {
        self.mode == 0o160000
    }
}

/// Whether the commit is reviewed, and what the answer rests on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Review {
    /// The commit `refs/remotes/origin/main` names here, if the ref exists.
    pub origin_main: Option<String>,
    /// The commit is an ancestor of (or is) `origin/main`.
    pub reviewed: bool,
    /// Things to tell the user (stale ref, absent ref).
    pub notes: Vec<String>,
}

/// A git executable and the PATH its commands see.
#[derive(Debug, Clone)]
pub struct Git {
    exe: PathBuf,
    path_var: OsString,
}

/// A checkout, opened.
#[derive(Debug, Clone)]
pub struct Repo {
    git: Git,
    /// Canonical top-level directory of the work tree.
    pub top: PathBuf,
    /// The object directory (shared with linked worktrees).
    pub objects: PathBuf,
}

fn git_err(what: &str, o: &process::Outcome) -> InstallError {
    let tail: String = o.text().trim().chars().take(300).collect();
    InstallError::refused(format!("git {what} failed ({}): {tail}", o.describe()))
}

impl Git {
    /// Find `git` on the absolute entries of `path_var`.
    pub fn find(path_var: &OsStr) -> Result<Git> {
        let exe = which("git", path_var)
            .ok_or_else(|| InstallError::refused("git is not on PATH (needed for --from-git)"))?;
        Ok(Git {
            exe,
            path_var: path_var.to_os_string(),
        })
    }

    /// The arguments every command starts with: `-c` overrides, then nothing else.
    pub fn base_args() -> Vec<OsString> {
        let mut a: Vec<OsString> = vec!["--no-pager".into(), "--no-optional-locks".into()];
        for kv in HARDENING {
            a.push("-c".into());
            a.push((*kv).into());
        }
        a
    }

    /// A command in `dir` with the hardened environment and arguments.
    pub fn cmd(&self, dir: &Path) -> Command {
        let mut c = Command::new(&self.exe);
        c.env_clear().envs(hardened_env(&self.path_var));
        c.arg("-C").arg(dir).args(Self::base_args());
        c.stdin(Stdio::null());
        c
    }

    fn run(&self, dir: &Path, args: &[&str], cap: usize) -> Result<process::Outcome> {
        let mut full: Vec<OsString> = vec!["-C".into(), dir.as_os_str().to_os_string()];
        full.extend(Self::base_args());
        full.extend(args.iter().map(OsString::from));
        let env = hardened_env(&self.path_var);
        process::run(&Spec {
            program: &self.exe,
            args: full,
            cwd: dir,
            env: &env,
            timeout: GIT_TIMEOUT,
            log: None,
            capture_cap: cap,
        })
        .ctx(|| format!("cannot run git {}", args.first().copied().unwrap_or("")))
    }

    /// Open the checkout at (or above) `dir`: a work tree, not a bare repository.
    pub fn open(&self, dir: &Path) -> Result<Repo> {
        let o = self.run(dir, &["rev-parse", "--show-toplevel"], 4096)?;
        if !o.success() {
            return Err(InstallError::refused(format!(
                "{} is not inside a git checkout (git rev-parse --show-toplevel: {})",
                dir.display(),
                o.text().trim()
            )));
        }
        let top = PathBuf::from(o.text().trim_end_matches('\n'));
        let top = fs::canonicalize(&top).ctx(|| format!("cannot resolve {}", top.display()))?;
        let o = self.run(
            &top,
            &[
                "rev-parse",
                "--path-format=absolute",
                "--git-path",
                "objects",
            ],
            4096,
        )?;
        if !o.success() {
            return Err(git_err("rev-parse --git-path objects", &o));
        }
        let objects = PathBuf::from(o.text().trim_end_matches('\n'));
        if !objects.is_absolute() {
            return Err(InstallError::refused(
                "git did not name an absolute object directory",
            ));
        }
        Ok(Repo {
            git: self.clone(),
            top,
            objects,
        })
    }
}

impl Repo {
    fn run(&self, args: &[&str], cap: usize) -> Result<process::Outcome> {
        self.git.run(&self.top, args, cap)
    }

    /// The commit `rev` names, as 40 lowercase hex digits.
    pub fn resolve(&self, rev: &str) -> Result<String> {
        if let Some(why) = rev_problem(rev) {
            return Err(InstallError::Usage(format!(
                "revision `{}` {why}",
                rev.chars().take(60).collect::<String>()
            )));
        }
        let spec = format!("{rev}^{{commit}}");
        let o = self.run(
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                "--end-of-options",
                &spec,
            ],
            4096,
        )?;
        let sha = o.text().trim().to_string();
        if !o.success() || !is_sha1_hex(&sha) {
            return Err(InstallError::refused(format!(
                "`{rev}` does not name a commit in {}",
                self.top.display()
            )));
        }
        Ok(sha)
    }

    fn commit_time_of(&self, commit: &str) -> Result<u64> {
        let o = self.run(
            &["log", "-1", "--format=%ct", "--end-of-options", commit],
            64,
        )?;
        o.text()
            .trim()
            .parse()
            .ok()
            .filter(|_| o.success())
            .ok_or_else(|| git_err("log -1 --format=%ct", &o))
    }

    /// The committer time of `sha`, unix seconds.
    pub fn commit_time(&self, sha: &str) -> Result<u64> {
        self.commit_time_of(sha)
    }

    /// Whether `sha` is an ancestor of the local `refs/remotes/origin/main`. No fetch ever
    /// happens: an absent ref is "not reviewed".
    pub fn review(&self, sha: &str, now_unix: u64) -> Result<Review> {
        let mut notes = Vec::new();
        let o = self.run(
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                "--end-of-options",
                "refs/remotes/origin/main^{commit}",
            ],
            4096,
        )?;
        let origin = o.text().trim().to_string();
        if !o.success() || !is_sha1_hex(&origin) {
            notes.push(
                "refs/remotes/origin/main does not exist here, so no commit can be shown to be \
                 reviewed (this command never fetches; run `git fetch origin` first)"
                    .to_string(),
            );
            return Ok(Review {
                origin_main: None,
                reviewed: false,
                notes,
            });
        }
        let o = self.run(
            &[
                "merge-base",
                "--is-ancestor",
                "--end-of-options",
                sha,
                &origin,
            ],
            4096,
        )?;
        let reviewed = match o.code {
            Some(0) => true,
            Some(1) => false,
            _ => return Err(git_err("merge-base --is-ancestor", &o)),
        };
        if let Ok(t) = self.commit_time_of(&origin)
            && now_unix > t
            && (now_unix - t) / 86_400 >= STALE_DAYS
        {
            notes.push(format!(
                "origin/main ({}) was committed {} days ago and may be stale; this command \
                 never fetches (run `git fetch origin` to see newer reviewed commits)",
                &origin[..12],
                (now_unix - t) / 86_400
            ));
        }
        Ok(Review {
            origin_main: Some(origin),
            reviewed,
            notes,
        })
    }

    /// Every entry of the commit's tree, files and submodules, in git's order.
    pub fn list_tree(&self, sha: &str) -> Result<Vec<TreeEntry>> {
        let o = self.run(
            &[
                "ls-tree",
                "-r",
                "-z",
                "--full-tree",
                "--end-of-options",
                sha,
            ],
            MAX_LISTING_BYTES,
        )?;
        if !o.success() {
            return Err(git_err("ls-tree", &o));
        }
        if o.output.len() >= MAX_LISTING_BYTES {
            return Err(InstallError::refused("the commit's file list is too large"));
        }
        let mut out = Vec::new();
        for rec in o.output.split(|b| *b == 0).filter(|r| !r.is_empty()) {
            let s = std::str::from_utf8(rec)
                .map_err(|_| InstallError::refused("a path in the commit is not UTF-8"))?;
            let (meta, path) = s
                .split_once('\t')
                .ok_or_else(|| InstallError::refused("unreadable git ls-tree record"))?;
            let mut it = meta.split(' ');
            let mode = u32::from_str_radix(it.next().unwrap_or(""), 8)
                .map_err(|_| InstallError::refused("unreadable mode in git ls-tree"))?;
            let _kind = it.next();
            let oid = it.next().unwrap_or("").to_string();
            if !is_sha1_hex(&oid) {
                return Err(InstallError::refused(
                    "git ls-tree gave an object id that is not 40 hex digits",
                ));
            }
            out.push(TreeEntry {
                mode,
                oid,
                path: path.to_string(),
            });
        }
        Ok(out)
    }

    /// The bytes of one blob, at most `max` of them.
    pub fn blob(&self, oid: &str, max: u64) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        self.cat_blobs(&[oid.to_string()], max, &mut |_, size, r| {
            out.reserve(size as usize);
            r.read_to_end(&mut out)
                .ctx(|| "cannot read a blob from git".to_string())?;
            Ok(())
        })?;
        Ok(out)
    }

    /// Stream blobs through one `git cat-file --batch`.
    fn cat_blobs(
        &self,
        oids: &[String],
        max_each: u64,
        f: &mut dyn FnMut(&str, u64, &mut dyn Read) -> Result<()>,
    ) -> Result<()> {
        if oids.is_empty() {
            return Ok(());
        }
        let mut c = self.git.cmd(&self.top);
        c.args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = c.spawn().ctx(|| "cannot run git cat-file".to_string())?;
        let mut stdin = child.stdin.take().expect("piped");
        let want: Vec<String> = oids.to_vec();
        let feeder = std::thread::spawn(move || {
            for oid in want {
                if writeln!(stdin, "{oid}").is_err() {
                    break;
                }
            }
        });
        let mut rd = BufReader::new(child.stdout.take().expect("piped"));
        let mut result = Ok(());
        for oid in oids {
            let mut head = String::new();
            let n = rd
                .read_line(&mut head)
                .ctx(|| "cannot read from git cat-file".to_string());
            let step = (|| -> Result<()> {
                if n? == 0 {
                    return Err(InstallError::refused("git cat-file ended early"));
                }
                let mut p = head.trim_end().split(' ');
                let (got, kind, size) = (p.next(), p.next(), p.next());
                if got != Some(oid.as_str()) || kind != Some("blob") {
                    return Err(InstallError::refused(format!(
                        "object {oid} is not a blob that git can read"
                    )));
                }
                let size: u64 = size
                    .and_then(|s| s.parse().ok())
                    .ok_or_else(|| InstallError::refused("unreadable size from git cat-file"))?;
                if size > max_each {
                    return Err(InstallError::refused(format!(
                        "object {oid} is {size} bytes; the limit is {max_each}"
                    )));
                }
                let mut limited = (&mut rd).take(size);
                f(oid, size, &mut limited)?;
                if limited.limit() != 0 {
                    return Err(InstallError::refused("a blob was not read to its end"));
                }
                let mut nl = [0u8; 1];
                rd.read_exact(&mut nl)
                    .ctx(|| "cannot read from git cat-file".to_string())?;
                Ok(())
            })();
            if let Err(e) = step {
                result = Err(e);
                break;
            }
        }
        drop(rd);
        let _ = child.kill();
        let _ = child.wait();
        let _ = feeder.join();
        result
    }
}

fn is_sha1_hex(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// What [`extract_tree`] wrote and left out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Extracted {
    pub files: usize,
    pub bytes: u64,
    /// Symlinks in the commit: never written.
    pub symlinks_skipped: Vec<String>,
    /// Submodule paths: an empty directory is made for each.
    pub submodules: Vec<String>,
}

/// Write the commit's files under `dest` (an existing empty directory). Every path is checked
/// first; a bad one refuses the whole extraction. Nothing the commit says (attributes,
/// filters, hooks, symlinks) is acted on: a blob's bytes are the file's bytes.
pub fn extract_tree(repo: &Repo, entries: &[TreeEntry], dest: &Path) -> Result<Extracted> {
    use std::os::unix::fs::DirBuilderExt;
    let mut out = Extracted::default();
    let mut blobs: Vec<&TreeEntry> = Vec::new();
    for e in entries {
        if let Some(why) = tree_path_problem(&e.path) {
            return Err(InstallError::refused(format!(
                "the commit holds the path `{}` which {why}; it is not built",
                e.path.chars().take(120).collect::<String>()
            )));
        }
        if e.is_blob() {
            blobs.push(e);
        } else if e.is_symlink() {
            out.symlinks_skipped.push(e.path.clone());
        } else if e.is_submodule() {
            out.submodules.push(e.path.clone());
        } else {
            return Err(InstallError::refused(format!(
                "the commit holds `{}` with mode {:o}, which is not a file, symlink or submodule",
                e.path, e.mode
            )));
        }
    }
    if blobs.len() > MAX_SOURCE_FILES {
        return Err(InstallError::refused(format!(
            "the commit holds more than {MAX_SOURCE_FILES} files"
        )));
    }
    let mkdirs = |dir: &Path| -> Result<()> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o755)
            .create(dir)
            .ctx(|| format!("cannot create {}", dir.display()))
    };
    for s in &out.submodules {
        mkdirs(&dest.join(s))?;
    }
    let oids: Vec<String> = blobs.iter().map(|b| b.oid.clone()).collect();
    let mut idx = 0usize;
    let mut total = 0u64;
    let mut err: Option<InstallError> = None;
    repo.cat_blobs(&oids, MAX_SOURCE_FILE_BYTES, &mut |_oid, size, r| {
        let e = blobs[idx];
        idx += 1;
        total += size;
        if total > MAX_SOURCE_TREE_BYTES {
            return Err(InstallError::refused(format!(
                "the commit holds more than {MAX_SOURCE_TREE_BYTES} bytes"
            )));
        }
        let target = dest.join(&e.path);
        if let Some(p) = target.parent() {
            mkdirs(p)?;
        }
        let mode = if e.mode == 0o100755 { 0o755 } else { 0o644 };
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&target)
            .ctx(|| format!("cannot create {}", target.display()))?;
        std::io::copy(r, &mut f).ctx(|| format!("cannot write {}", target.display()))?;
        out.files += 1;
        out.bytes += size;
        Ok(())
    })
    .unwrap_or_else(|e| err = Some(e));
    if let Some(e) = err {
        return Err(e);
    }
    Ok(out)
}

/// Give the extracted tree at `dest` a `.git` the build scripts can stamp from: a FRESH
/// repository (no template, no hooks, empty config) whose `HEAD` is `sha`, whose index is the
/// commit's tree, and whose only link to the user's repository is an `alternates` line naming
/// its object directory (read-only use). Nothing is fetched and nothing in the user's
/// repository is written.
///
/// `unwritten` are the paths of the commit that [`extract_tree`] did not write (symlinks): they
/// are marked skip-worktree, so their absence is not "dirty".
pub fn init_stamp_repo(repo: &Repo, dest: &Path, sha: &str, unwritten: &[String]) -> Result<()> {
    let git = &repo.git;
    let step = |args: &[&str]| -> Result<()> {
        let mut full: Vec<OsString> = vec!["-C".into(), dest.as_os_str().to_os_string()];
        full.extend(Git::base_args());
        full.extend(args.iter().map(OsString::from));
        let env = hardened_env(&git.path_var);
        let o = process::run(&Spec {
            program: &git.exe,
            args: full,
            cwd: dest,
            env: &env,
            timeout: GIT_TIMEOUT,
            log: None,
            capture_cap: 4096,
        })
        .ctx(|| format!("cannot run git {}", args.first().copied().unwrap_or("")))?;
        if o.success() {
            Ok(())
        } else {
            Err(git_err(args.first().copied().unwrap_or(""), &o))
        }
    };
    step(&["init", "--quiet", "--template="])?;
    let info = dest.join(".git/objects/info");
    fs::create_dir_all(&info).ctx(|| format!("cannot create {}", info.display()))?;
    let mut line = repo.objects.as_os_str().to_os_string();
    line.push("\n");
    fs::write(
        info.join("alternates"),
        std::os::unix::ffi::OsStrExt::as_bytes(line.as_os_str()),
    )
    .ctx(|| "cannot write the alternates file".to_string())?;
    step(&["update-ref", "--no-deref", "HEAD", sha])?;
    step(&["read-tree", sha])?;
    for chunk in unwritten.chunks(100) {
        let mut a: Vec<&str> = vec!["update-index", "--skip-worktree", "--"];
        a.extend(chunk.iter().map(String::as_str));
        step(&a)?;
    }
    step(&["update-index", "-q", "--refresh"])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revs_that_could_be_options_or_ranges_are_refused() {
        for bad in [
            "",
            "-x",
            "--output=/tmp/x",
            "a b",
            "a\tb",
            "a\nb",
            "^abc",
            "a..b",
            "!x",
            &"a".repeat(201),
        ] {
            assert!(rev_problem(bad).is_some(), "{bad:?}");
        }
        for ok in [
            "HEAD",
            "main",
            "origin/main",
            "v1.2.3",
            "abc123",
            "HEAD~2",
            "feira/x",
        ] {
            assert_eq!(rev_problem(ok), None, "{ok}");
        }
    }

    #[test]
    fn paths_that_could_leave_the_tree_are_refused() {
        for bad in [
            "",
            "/etc/passwd",
            "../x",
            "a/../b",
            "a//b",
            "a/./b",
            ".git/config",
            "a/.GIT/hooks/pre-commit",
            "a\\b",
            "a\nb",
            "a/",
        ] {
            assert!(tree_path_problem(bad).is_some(), "{bad:?}");
        }
        for ok in [
            "Cargo.toml",
            "crates/core/src/lib.rs",
            ".github/x.yml",
            "a b/c",
            ".gitignore",
        ] {
            assert_eq!(tree_path_problem(ok), None, "{ok}");
        }
    }

    #[test]
    fn the_hardening_covers_the_keys_that_run_programs() {
        let all = HARDENING.join(" ");
        for key in [
            "core.hooksPath",
            "core.fsmonitor",
            "log.showSignature",
            "protocol.ext.allow",
        ] {
            assert!(all.contains(key), "{key}");
        }
        let env = hardened_env(OsStr::new("/usr/bin:."));
        let get = |k: &str| {
            env.iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.to_string_lossy().into_owned())
        };
        assert_eq!(get("GIT_CONFIG_GLOBAL").as_deref(), Some("/dev/null"));
        assert_eq!(get("GIT_CONFIG_NOSYSTEM").as_deref(), Some("1"));
        assert_eq!(get("PATH").as_deref(), Some("/usr/bin"));
        assert!(get("GIT_DIR").is_none());
    }
}
