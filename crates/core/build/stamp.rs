//! Build stamping: the commit, the dirty flag and the build time a build script embeds.
//!
//! ⚠ This file uses `std` ONLY. It is compiled twice: by `crates/core/tests/build_stamp.rs`
//! (where its tests run; `fl-core` itself stays free of IO) and, through
//! `#[path = "../core/build/stamp.rs"] mod stamp;`, inside the `build.rs` of `fl-cli`,
//! `fl-store` and `fl-hub`. Every build script therefore runs the code these tests cover. Do
//! not add a dependency: a build script has none of the crate's.
//!
//! A build script calls [`emit`]; it prints these `cargo:` lines:
//! - `rustc-env=FL_BUILD_COMMIT`: the 40-hex `HEAD` commit, or `unknown`.
//! - `rustc-env=FL_BUILD_DIRTY`: `true`, `false` or `unknown`. Dirty means: a file the binary is
//!   built from (the `src`, `build`, `build.rs` and `Cargo.toml` of every crate in the
//!   package's path-dependency closure, and the workspace `Cargo.toml` and `Cargo.lock`)
//!   differs from `HEAD`, staged or not, or is new and not ignored. Nothing else counts.
//! - `rustc-env=FL_BUILD_TIME`: unix seconds. `SOURCE_DATE_EPOCH` when it is set and valid,
//!   else the `HEAD` commit time, else (no git) the clock. The commit time, not the clock, so
//!   that the same sources always stamp the same bytes (a reproducible binary).
//! - `rustc-env=FL_RELEASE_ID`: only when `$FL_RELEASE_ID` is set and not empty AND the tree
//!   is known to be clean: a dirty or unreadable tree never reports a release id.
//!
//! ⚠ Cargo recompiles a package, and everything that depends on it, whenever its build script
//! RERUNS, even when the script prints exactly what it printed before (measured; see
//! `docs/build-provenance.md`). So the only defence against needless rebuilds is to rerun the
//! script only when a path it names changed: each script names the sources of its own
//! closure and `HEAD`, never the whole `crates/` directory and never the git index.
//!
//! A tree that is not a git checkout of this workspace (a tarball, `cargo publish`, a sandbox
//! without `git`, a copy under some unrelated repository) stamps `unknown` instead of failing
//! the build.

#![allow(dead_code)] // each consumer uses a subset

use std::path::{Path, PathBuf};
use std::process::Command;

/// What a stamp holds when git could not answer.
pub const UNKNOWN: &str = "unknown";

/// The values a build script embeds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stamp {
    /// A full lowercase hex commit id, or [`UNKNOWN`].
    pub commit: String,
    /// `None` when git could not say.
    pub dirty: Option<bool>,
    /// Unix seconds.
    pub time: u64,
}

/// `true` for a full object id: 40 (sha1) or 64 (sha256) lowercase hex digits.
pub fn is_commit_hash(s: &str) -> bool {
    matches!(s.len(), 40 | 64)
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// `git rev-parse HEAD` output → the commit, or `None` when it is not a full id.
pub fn parse_commit(out: &str) -> Option<String> {
    let c = out.trim();
    is_commit_hash(c).then(|| c.to_string())
}

/// `git status --porcelain` output → whether the tree is dirty (any line at all).
pub fn dirty_from_porcelain(out: &str) -> bool {
    !out.trim().is_empty()
}

/// `$SOURCE_DATE_EPOCH` → seconds, when it is a plain non-negative integer.
pub fn parse_epoch(v: Option<&str>) -> Option<u64> {
    let s = v?.trim();
    (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        .then(|| s.parse().ok())
        .flatten()
}

/// The build time: `SOURCE_DATE_EPOCH` when valid, else the commit time when git gave one,
/// else `now`.
pub fn resolve_time(source_date_epoch: Option<&str>, commit_time: Option<&str>, now: u64) -> u64 {
    parse_epoch(source_date_epoch)
        .or_else(|| parse_epoch(commit_time))
        .unwrap_or(now)
}

/// A file every checkout of this workspace holds. A `.git` whose root lacks it belongs to
/// something else (a registry or tarball copy under a git-tracked home directory).
pub const WORKSPACE_MARKER: &str = "crates/core/build/stamp.rs";

/// Remove from `cmd` every git variable that would point it at another repository.
pub fn scrub_git_env(cmd: &mut Command) -> &mut Command {
    for var in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_COMMON_DIR",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_NAMESPACE",
    ] {
        cmd.env_remove(var);
    }
    cmd
}

/// The stamp from what git answered (`None` = git did not run or failed): `rev-parse HEAD`,
/// `status --porcelain` and `log -1 --format=%ct`; and the clock.
///
/// Dirty is only known when the commit is: `git status` in a tree with no commit, or output
/// from a failed `rev-parse`, says nothing about this build.
pub fn make_stamp(
    rev_parse: Option<&str>,
    porcelain: Option<&str>,
    commit_time: Option<&str>,
    source_date_epoch: Option<&str>,
    now: u64,
) -> Stamp {
    let commit = rev_parse.and_then(parse_commit);
    let dirty = match (&commit, porcelain) {
        (Some(_), Some(p)) => Some(dirty_from_porcelain(p)),
        _ => None,
    };
    // A commit time from a failed or missing `rev-parse` belongs to no known commit.
    let time = resolve_time(source_date_epoch, commit.as_ref().and(commit_time), now);
    Stamp {
        commit: commit.unwrap_or_else(|| UNKNOWN.to_string()),
        dirty,
        time,
    }
}

/// `"true"`, `"false"` or [`UNKNOWN`].
pub fn dirty_str(d: Option<bool>) -> &'static str {
    match d {
        Some(true) => "true",
        Some(false) => "false",
        None => UNKNOWN,
    }
}

/// The git directory of the checkout holding `start`: `<dir>/.git` as a directory, or as a
/// file `gitdir: <path>` (a linked worktree or a submodule), searched from `start` upward.
/// The first `.git` found decides, and only a checkout whose root holds [`WORKSPACE_MARKER`]
/// counts: any other is not this workspace, so `None`.
pub fn git_dir(start: &Path) -> Option<PathBuf> {
    git_dir_until(start, &[])
}

/// [`git_dir`] that does not ascend into a directory of `ceilings` (git's
/// `GIT_CEILING_DIRECTORIES`): `start` itself is always checked.
pub fn git_dir_until(start: &Path, ceilings: &[PathBuf]) -> Option<PathBuf> {
    for dir in start.ancestors() {
        if dir != start && ceilings.iter().any(|c| c == dir) {
            break;
        }
        let dot = dir.join(".git");
        if (dot.is_dir() || dot.is_file()) && !dir.join(WORKSPACE_MARKER).is_file() {
            return None;
        }
        if dot.is_dir() {
            return Some(dot);
        }
        if dot.is_file() {
            let text = std::fs::read_to_string(&dot).ok()?;
            let target = text.trim().strip_prefix("gitdir:")?.trim();
            let p = Path::new(target);
            return Some(if p.is_absolute() {
                p.to_path_buf()
            } else {
                dir.join(p)
            });
        }
    }
    None
}

/// The directory that holds `refs/` and `packed-refs`: `commondir` of a linked worktree
/// (a path in the file `commondir`), else the git dir itself.
pub fn common_dir(git_dir: &Path) -> PathBuf {
    match std::fs::read_to_string(git_dir.join("commondir")) {
        Ok(t) if !t.trim().is_empty() => {
            let p = Path::new(t.trim());
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                git_dir.join(p)
            }
        }
        _ => git_dir.to_path_buf(),
    }
}

/// The files whose change moves the commit: `HEAD`, the ref it names and `packed-refs`. Not
/// the index: a `git add` changes no commit and no dirty state, and a rerun costs a rebuild.
/// Only those that exist: a path that is missing makes cargo rerun the script on every build.
pub fn watch_paths(git_dir: &Path) -> Vec<PathBuf> {
    let common = common_dir(git_dir);
    let mut out = vec![git_dir.join("HEAD")];
    if let Ok(head) = std::fs::read_to_string(git_dir.join("HEAD"))
        && let Some(r) = head.trim().strip_prefix("ref:")
    {
        let r = r.trim();
        // A ref name is a relative path under `refs/`; refuse anything that climbs out.
        if r.starts_with("refs/") && !r.split('/').any(|c| c == ".." || c.is_empty()) {
            out.push(common.join(r));
        }
    }
    out.push(common.join("packed-refs"));
    out.retain(|p| p.exists());
    out
}

/// What of a crate's directory the compiler reads; the dirty flag and the rerun list use
/// exactly these.
pub const CRATE_INPUTS: [&str; 4] = ["src", "build", "build.rs", "Cargo.toml"];
/// Workspace files every stamped crate builds with, including this shared build helper.
pub const WORKSPACE_INPUTS: [&str; 3] = ["Cargo.toml", "Cargo.lock", WORKSPACE_MARKER];

/// The existing source paths of `crates` (directories relative to `manifest_dir`, such as
/// `"."` and `"../core"`) plus the shared workspace inputs rooted two levels above
/// `manifest_dir`: absolute, without `..`, sorted.
pub fn inputs(manifest_dir: &Path, crates: &[&str]) -> Vec<PathBuf> {
    let root = manifest_dir.join("../..");
    let mut out: Vec<PathBuf> = crates
        .iter()
        .flat_map(|c| {
            CRATE_INPUTS
                .iter()
                .map(move |i| manifest_dir.join(c).join(i))
        })
        .chain(WORKSPACE_INPUTS.iter().map(|i| root.join(i)))
        .filter_map(|p| std::fs::canonicalize(p).ok())
        .collect();
    out.sort();
    out.dedup();
    out
}

fn run_git(dir: &Path, args: &[&str]) -> Option<String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir).args(args);
    let out = scrub_git_env(&mut cmd).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Stamp the checkout holding `manifest_dir` with the real git, clock and environment.
pub fn collect(manifest_dir: &Path, inputs: &[PathBuf]) -> Stamp {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let ceilings: Vec<PathBuf> = std::env::var_os("GIT_CEILING_DIRECTORIES")
        .map(|v| std::env::split_paths(&v).collect())
        .unwrap_or_default();
    collect_in(
        manifest_dir,
        &ceilings,
        inputs,
        std::env::var("SOURCE_DATE_EPOCH").ok().as_deref(),
        now,
    )
}

/// [`collect`] with the ceilings, `SOURCE_DATE_EPOCH` and clock passed in.
///
/// Our own probe ([`git_dir_until`]) decides whether this is a checkout at all: git is asked
/// only inside one. No `.git` at or above `manifest_dir` (a tarball, `cargo publish`) stamps
/// [`UNKNOWN`]; so does a `git` that is missing or fails.
pub fn collect_in(
    manifest_dir: &Path,
    ceilings: &[PathBuf],
    inputs: &[PathBuf],
    source_date_epoch: Option<&str>,
    now: u64,
) -> Stamp {
    let have_git = git_dir_until(manifest_dir, ceilings).is_some();
    let rev = have_git
        .then(|| run_git(manifest_dir, &["rev-parse", "HEAD"]))
        .flatten();
    // Dirty is the state of the binary's own inputs; with none given, of the whole tree.
    let mut status_args: Vec<&str> = vec!["--no-optional-locks", "status", "--porcelain"];
    let paths: Vec<String> = inputs
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    if !paths.is_empty() {
        status_args.push("--");
        status_args.extend(paths.iter().map(String::as_str));
    }
    let status = have_git
        .then(|| run_git(manifest_dir, &status_args))
        .flatten();
    let ct = have_git
        .then(|| run_git(manifest_dir, &["log", "-1", "--format=%ct", "HEAD"]))
        .flatten();
    make_stamp(
        rev.as_deref(),
        status.as_deref(),
        ct.as_deref(),
        source_date_epoch,
        now,
    )
}

/// The `cargo:` lines for `stamp`. `release_id` is `$FL_RELEASE_ID`; `watch` are the paths
/// cargo reruns the script for.
pub fn cargo_lines(stamp: &Stamp, release_id: Option<&str>, watch: &[PathBuf]) -> Vec<String> {
    let mut lines = vec![
        format!("cargo:rustc-env=FL_BUILD_COMMIT={}", stamp.commit),
        format!("cargo:rustc-env=FL_BUILD_DIRTY={}", dirty_str(stamp.dirty)),
        format!("cargo:rustc-env=FL_BUILD_TIME={}", stamp.time),
    ];
    // Env values are one line: a newline would inject a cargo directive. A release id needs
    // a tree known to be clean.
    if let Some(id) = release_id.map(str::trim).filter(|s| !s.is_empty())
        && !id.contains(['\n', '\r'])
        && stamp.dirty == Some(false)
    {
        lines.push(format!("cargo:rustc-env=FL_RELEASE_ID={id}"));
    }
    lines.push("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH".to_string());
    lines.push("cargo:rerun-if-env-changed=FL_RELEASE_ID".to_string());
    for p in watch {
        lines.push(format!("cargo:rerun-if-changed={}", p.display()));
    }
    lines
}

/// The whole build script: stamp the checkout holding `$CARGO_MANIFEST_DIR` and print the
/// `cargo:` lines. `crates` are the directories (relative to the manifest directory) of the
/// package and of every crate it depends on by path, such as `[".", "../core"]`: their sources
/// decide the dirty flag and are what cargo reruns the script for, with `HEAD` and its ref.
pub fn emit(crates: &[&str]) {
    let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default());
    let inputs = inputs(&manifest_dir, crates);
    let stamp = collect(&manifest_dir, &inputs);
    let mut watch = git_dir(&manifest_dir)
        .map(|g| watch_paths(&g))
        .unwrap_or_default();
    watch.extend(inputs);
    let release = std::env::var("FL_RELEASE_ID").ok();
    for line in cargo_lines(&stamp, release.as_deref(), &watch) {
        println!("{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const C: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn commit_must_be_a_full_lowercase_id() {
        assert!(is_commit_hash(C));
        assert!(is_commit_hash(&"a".repeat(64)));
        assert!(!is_commit_hash("0123456"));
        assert!(!is_commit_hash(&C.to_uppercase()));
        assert!(!is_commit_hash(&format!("{}g", &C[..39])));
        assert_eq!(parse_commit(&format!("{C}\n")), Some(C.to_string()));
        assert_eq!(parse_commit("fatal: not a git repository"), None);
    }

    #[test]
    fn porcelain_output_decides_dirty() {
        assert!(!dirty_from_porcelain(""));
        assert!(!dirty_from_porcelain("\n"));
        assert!(dirty_from_porcelain(" M crates/cli/src/main.rs\n"));
        assert!(dirty_from_porcelain("?? new.rs\n"));
    }

    /// Make `root` look like a checkout of this workspace to [`git_dir_until`].
    fn add_marker(root: &Path) {
        let m = root.join(WORKSPACE_MARKER);
        std::fs::create_dir_all(m.parent().unwrap()).unwrap();
        std::fs::write(m, "// marker\n").unwrap();
    }

    /// Run git in `dir` with a clean git environment; `false` when it failed or is missing.
    fn git(dir: &Path, args: &[&str]) -> bool {
        let mut cmd = Command::new("git");
        cmd.arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
            .args(["-c", "commit.gpgsign=false"])
            .args(args);
        scrub_git_env(&mut cmd)
            .output()
            .is_ok_and(|o| o.status.success())
    }

    #[test]
    fn source_date_epoch_then_commit_time_then_the_clock() {
        assert_eq!(
            resolve_time(Some("1700000000"), Some("9"), 5),
            1_700_000_000
        );
        assert_eq!(resolve_time(Some(" 42\n"), None, 5), 42);
        assert_eq!(resolve_time(Some("0"), Some("9"), 5), 0);
        // Not valid: fall to the commit time (with git's trailing newline), then the clock.
        for bad in ["", "abc", "-1", "1.5", "99999999999999999999999"] {
            assert_eq!(resolve_time(Some(bad), Some("9\n"), 5), 9, "{bad:?}");
            assert_eq!(resolve_time(Some(bad), None, 5), 5, "{bad:?}");
        }
        assert_eq!(resolve_time(None, Some("9\n"), 5), 9);
        assert_eq!(resolve_time(None, Some("junk"), 5), 5);
        assert_eq!(resolve_time(None, None, 5), 5);
    }

    #[test]
    fn clean_dirty_and_no_git_stamps() {
        let clean = make_stamp(Some(&format!("{C}\n")), Some(""), Some("1234\n"), None, 9);
        assert_eq!(
            clean,
            Stamp {
                commit: C.into(),
                dirty: Some(false),
                time: 1234
            }
        );
        let dirty = make_stamp(Some(C), Some(" M a.rs\n"), Some("1234"), Some("7"), 9);
        assert_eq!(dirty.dirty, Some(true));
        assert_eq!(dirty.time, 7);
        // No git at all, or a failed git: unknown, and the build still gets the clock.
        let none = make_stamp(None, None, None, None, 9);
        assert_eq!(none.commit, UNKNOWN);
        assert_eq!(none.dirty, None);
        assert_eq!(none.time, 9);
        // A commit time is only believed with the commit it belongs to.
        assert_eq!(make_stamp(None, None, Some("1234"), None, 9).time, 9);
        // A commit without a status answer is not "clean"; a status without a commit is not
        // "dirty" either.
        assert_eq!(make_stamp(Some(C), None, None, None, 1).dirty, None);
        assert_eq!(make_stamp(None, Some(" M a\n"), None, None, 1).dirty, None);
        assert_eq!(
            make_stamp(Some("garbage"), Some(""), None, None, 1).commit,
            UNKNOWN
        );
    }

    #[test]
    fn the_output_does_not_depend_on_the_clock_when_the_commit_is_known() {
        // The same HEAD, a different `now` (a rerun after a `git add`, a branch switch, an
        // edit elsewhere): byte-identical cargo lines, so cargo has nothing to rebuild.
        let watch = [PathBuf::from("/x/HEAD")];
        let at = |now| {
            let s = make_stamp(Some(C), Some(""), Some("1234"), None, now);
            cargo_lines(&s, None, &watch)
        };
        assert_eq!(at(1), at(999_999));
        // Without a commit there is only the clock to stamp.
        let nogit = |now| cargo_lines(&make_stamp(None, None, None, None, now), None, &watch);
        assert_ne!(nogit(1), nogit(2));
    }

    #[test]
    fn cargo_lines_carry_the_stamp_and_never_inject() {
        let s = make_stamp(Some(C), Some(""), None, Some("5"), 9);
        let lines = cargo_lines(&s, None, &[PathBuf::from("/x/HEAD")]);
        assert!(lines.contains(&format!("cargo:rustc-env=FL_BUILD_COMMIT={C}")));
        assert!(lines.contains(&"cargo:rustc-env=FL_BUILD_DIRTY=false".to_string()));
        assert!(lines.contains(&"cargo:rustc-env=FL_BUILD_TIME=5".to_string()));
        assert!(lines.contains(&"cargo:rerun-if-changed=/x/HEAD".to_string()));
        assert!(!lines.iter().any(|l| l.contains("FL_RELEASE_ID=")));
        let with = cargo_lines(&s, Some(" 2026.11 "), &[]);
        assert!(with.contains(&"cargo:rustc-env=FL_RELEASE_ID=2026.11".to_string()));
        for bad in ["", "  ", "a\ncargo:rustc-cfg=evil"] {
            let l = cargo_lines(&s, Some(bad), &[]);
            assert!(!l.iter().any(|x| x.contains("FL_RELEASE_ID=")), "{bad:?}");
            assert!(!l.iter().any(|x| x.contains("evil")));
        }
        let unknown = cargo_lines(&make_stamp(None, None, None, None, 1), None, &[]);
        assert!(unknown.contains(&"cargo:rustc-env=FL_BUILD_COMMIT=unknown".to_string()));
        assert!(unknown.contains(&"cargo:rustc-env=FL_BUILD_DIRTY=unknown".to_string()));
    }

    #[test]
    fn a_release_id_needs_a_tree_known_to_be_clean() {
        let has_release = |s: &Stamp| {
            cargo_lines(s, Some("2026.11"), &[])
                .iter()
                .any(|l| l.contains("FL_RELEASE_ID="))
        };
        assert!(has_release(&make_stamp(Some(C), Some(""), None, None, 1)));
        assert!(!has_release(&make_stamp(
            Some(C),
            Some(" M a\n"),
            None,
            None,
            1
        )));
        assert!(!has_release(&make_stamp(Some(C), None, None, None, 1)));
        assert!(!has_release(&make_stamp(None, None, None, None, 1)));
    }

    #[test]
    fn git_env_variables_are_scrubbed() {
        let mut cmd = Command::new("true");
        for var in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_COMMON_DIR",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_NAMESPACE",
        ] {
            cmd.env(var, "/elsewhere");
        }
        scrub_git_env(&mut cmd);
        // `get_envs` lists an `env_remove`d variable as `(name, None)`.
        let removed: Vec<_> = cmd
            .get_envs()
            .filter(|(k, v)| k.to_string_lossy().starts_with("GIT_") && v.is_none())
            .collect();
        assert_eq!(removed.len(), 7, "{removed:?}");
    }

    #[test]
    fn git_dir_finds_directories_files_and_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let plain = tmp.path().join("repo");
        std::fs::create_dir_all(plain.join(".git/refs/heads")).unwrap();
        std::fs::create_dir_all(plain.join("crates/x")).unwrap();
        add_marker(&plain);
        assert_eq!(git_dir(&plain.join("crates/x")), Some(plain.join(".git")));

        // A linked worktree: `.git` is a file naming the per-worktree dir, which names the
        // common dir.
        let main = tmp.path().join("main");
        let wt_git = main.join(".git/worktrees/w");
        std::fs::create_dir_all(main.join(".git/refs/heads")).unwrap();
        std::fs::create_dir_all(&wt_git).unwrap();
        std::fs::write(wt_git.join("commondir"), "../..\n").unwrap();
        std::fs::write(wt_git.join("HEAD"), "ref: refs/heads/feat\n").unwrap();
        std::fs::write(wt_git.join("index"), "").unwrap();
        std::fs::write(main.join(".git/refs/heads/feat"), format!("{C}\n")).unwrap();
        let wt = tmp.path().join("wt");
        std::fs::create_dir_all(wt.join("sub")).unwrap();
        add_marker(&wt);
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", wt_git.display())).unwrap();
        let found = git_dir(&wt.join("sub")).unwrap();
        assert_eq!(found, wt_git);
        let watch = watch_paths(&found);
        assert!(watch.contains(&wt_git.join("HEAD")));
        // Not the index: a `git add` must not rerun the script (a rerun rebuilds).
        assert!(!watch.contains(&wt_git.join("index")));
        // The branch ref lives in the COMMON dir, not the worktree's own.
        assert!(
            watch
                .iter()
                .any(|p| p.ends_with("refs/heads/feat") && p.starts_with(wt_git.join("../..")))
        );

        // A relative `gitdir:` resolves from the directory holding the file.
        let rel = tmp.path().join("rel");
        std::fs::create_dir_all(rel.join("real-git")).unwrap();
        add_marker(&rel);
        std::fs::write(rel.join(".git"), "gitdir: real-git\n").unwrap();
        assert_eq!(git_dir(&rel), Some(rel.join("real-git")));
    }

    #[test]
    fn a_git_root_without_the_workspace_marker_is_not_our_checkout() {
        // A source copy (registry, tarball) under a git-tracked home directory.
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(home.join(".git")).unwrap();
        let pkg = home.join(".cargo/registry/src/fl-cli-0.1.0");
        std::fs::create_dir_all(&pkg).unwrap();
        assert_eq!(git_dir(&pkg), None);
        assert_eq!(git_dir(&home), None);
        // Even a `.git` file is refused without the marker...
        let wt = tmp.path().join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".git"), "gitdir: /nowhere\n").unwrap();
        assert_eq!(git_dir(&wt), None);
        // ...and the marker alone, with no `.git`, is not a checkout.
        let loose = tmp.path().join("loose");
        add_marker(&loose);
        assert_eq!(git_dir_until(&loose, &[tmp.path().to_path_buf()]), None);
        // The stamp of such a tree is unknown even though git would answer for `home`.
        let s = collect_in(&pkg, &[], &[], None, 5);
        assert_eq!((s.commit.as_str(), s.dirty, s.time), (UNKNOWN, None, 5));
        // With the marker the same `.git` counts.
        add_marker(&home);
        assert_eq!(git_dir(&pkg), Some(home.join(".git")));
    }

    #[test]
    fn watch_paths_refuse_a_ref_that_climbs_out_and_skip_missing_files() {
        let tmp = tempfile::tempdir().unwrap();
        let g = tmp.path().join("g");
        std::fs::create_dir_all(&g).unwrap();
        std::fs::write(g.join("HEAD"), "ref: refs/../../secret\n").unwrap();
        assert_eq!(watch_paths(&g), vec![g.join("HEAD")]);
        // A detached HEAD names no ref.
        std::fs::write(g.join("HEAD"), format!("{C}\n")).unwrap();
        assert_eq!(watch_paths(&g), vec![g.join("HEAD")]);
    }

    #[test]
    fn collect_outside_a_checkout_is_unknown_not_a_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("tarball/crates/x");
        std::fs::create_dir_all(&dir).unwrap();
        // The ceiling keeps the search inside the temp tree, whatever sits above it.
        let s = collect_in(&dir, &[tmp.path().to_path_buf()], &[], Some("123"), 1);
        assert_eq!((s.commit.as_str(), s.dirty, s.time), (UNKNOWN, None, 123));
        let s = collect_in(&dir, &[tmp.path().to_path_buf()], &[], None, 77);
        assert_eq!((s.commit.as_str(), s.dirty, s.time), (UNKNOWN, None, 77));
    }

    #[test]
    fn collect_reads_a_real_checkout_clean_then_dirty() {
        let tmp = tempfile::tempdir().unwrap();
        let ceil = [tmp.path().to_path_buf()];
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        if !git(&repo, &["init", "-q"]) {
            eprintln!("SKIP collect_reads_a_real_checkout_clean_then_dirty: `git init` failed");
            return;
        }
        add_marker(&repo);
        std::fs::write(repo.join("a.txt"), "a").unwrap();
        assert!(git(&repo, &["add", "."]));
        assert!(git(&repo, &["commit", "-q", "-m", "one"]));
        // No SOURCE_DATE_EPOCH: the commit time, not the clock.
        let clean = collect_in(&repo, &ceil, &[], None, 5);
        assert!(is_commit_hash(&clean.commit), "{clean:?}");
        assert_eq!(clean.dirty, Some(false));
        assert!(clean.time > 1_600_000_000 && clean.time != 5, "{clean:?}");
        // A rerun on the same HEAD with another clock reads the same stamp.
        assert_eq!(clean, collect_in(&repo, &ceil, &[], None, 999_999_999_999));
        // SOURCE_DATE_EPOCH wins over the commit time.
        assert_eq!(collect_in(&repo, &ceil, &[], Some("1000"), 5).time, 1000);
        std::fs::write(repo.join("a.txt"), "changed").unwrap();
        let dirty = collect_in(&repo, &ceil, &[], Some("1000"), 5);
        assert_eq!(dirty.commit, clean.commit);
        assert_eq!(dirty.dirty, Some(true));
        // A subdirectory of the checkout is stamped with the checkout's commit.
        let sub = collect_in(&repo.join("crates/core"), &ceil, &[], None, 5);
        assert_eq!(sub.commit, clean.commit);
        // A linked worktree reads its own HEAD, and its watch list names the shared ref.
        let wt = tmp.path().join("wt");
        assert!(git(
            &repo,
            &["worktree", "add", "-q", "-b", "side", wt.to_str().unwrap()]
        ));
        let g = git_dir_until(&wt, &ceil).unwrap();
        let watch = watch_paths(&g);
        assert!(
            watch.iter().any(|p| p.ends_with("refs/heads/side")),
            "{watch:?}"
        );
        assert!(watch.iter().any(|p| p.ends_with("HEAD")));
        let w = collect_in(&wt, &ceil, &[], None, 5);
        assert_eq!(
            (w.commit.clone(), w.dirty),
            (clean.commit.clone(), Some(false))
        );
        assert_eq!(w.time, clean.time);
    }

    #[test]
    fn dirty_is_the_state_of_the_binarys_own_inputs() {
        let tmp = tempfile::tempdir().unwrap();
        let ceil = [tmp.path().to_path_buf()];
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        if !git(&repo, &["init", "-q"]) {
            eprintln!("SKIP dirty_is_the_state_of_the_binarys_own_inputs: `git init` failed");
            return;
        }
        add_marker(&repo);
        for f in [
            "crates/a/src/lib.rs",
            "crates/b/src/lib.rs",
            "crates/a/tests/t.rs",
            "Cargo.toml",
            "Cargo.lock",
        ] {
            let p = repo.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "").unwrap();
        }
        assert!(git(&repo, &["add", "."]));
        assert!(git(&repo, &["commit", "-q", "-m", "one"]));
        let a = repo.join("crates/a");
        let in_a = inputs(&a, &["."]);
        // The existing sources of the crate and the workspace files, absolute and normalised.
        assert!(
            in_a.iter()
                .all(|p| p.is_absolute() && !p.to_string_lossy().contains(".."))
        );
        assert!(in_a.iter().any(|p| p.ends_with("crates/a/src")));
        assert!(in_a.iter().any(|p| p.ends_with("Cargo.lock")));
        assert!(!in_a.iter().any(|p| p.ends_with("crates/b/src")));
        let dirty = |dir: &Path, crates: &[&str]| {
            collect_in(dir, &ceil, &inputs(dir, crates), None, 1).dirty
        };
        assert_eq!(dirty(&a, &["."]), Some(false));
        // A change in a crate this binary does not build from does not count...
        std::fs::write(repo.join("crates/b/src/lib.rs"), "// edit").unwrap();
        assert_eq!(dirty(&a, &["."]), Some(false));
        // ...nor does one in its tests...
        std::fs::write(repo.join("crates/a/tests/t.rs"), "// edit").unwrap();
        assert_eq!(dirty(&a, &["."]), Some(false));
        // ...but it does once that crate is in the closure.
        assert_eq!(dirty(&a, &[".", "../b"]), Some(true));
        // Own source, staged or not, and the lock file count; a new file counts.
        std::fs::write(repo.join("crates/b/src/lib.rs"), "").unwrap();
        std::fs::write(repo.join("Cargo.lock"), "# edit").unwrap();
        assert_eq!(dirty(&a, &["."]), Some(true));
        assert!(git(&repo, &["checkout", "--", "Cargo.lock"]));
        assert_eq!(dirty(&a, &["."]), Some(false));
        std::fs::write(repo.join("crates/a/src/new.rs"), "").unwrap();
        assert_eq!(dirty(&a, &["."]), Some(true));
        assert!(git(&repo, &["add", "crates/a/src/new.rs"]));
        assert_eq!(dirty(&a, &["."]), Some(true));
    }
}
