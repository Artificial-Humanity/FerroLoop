//! Runs the tests of `build/stamp.rs`, the std-only source every `build.rs` that stamps a
//! binary (`fl-cli`, `fl-store`, `fl-hub`, `fl-host`) includes by path. `fl-core` itself stays free of
//! IO: the stamping code is not part of its library.

#[path = "../build/stamp.rs"]
mod stamp;

/// The build script as cargo runs it: compile `stamp::emit` into a program and read the
/// `cargo:` lines it prints.
mod as_a_build_script {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    fn compile(dir: &Path) -> PathBuf {
        let stamp = Path::new(env!("CARGO_MANIFEST_DIR")).join("build/stamp.rs");
        let main = dir.join("main.rs");
        std::fs::write(
            &main,
            format!(
                "#[path = {:?}]\nmod stamp;\nfn main() {{ stamp::emit(&[\".\"]); }}\n",
                stamp.display().to_string()
            ),
        )
        .unwrap();
        let exe = dir.join("stamper");
        let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
        let out = Command::new(rustc)
            .args(["--edition", "2024", "-o"])
            .arg(&exe)
            .arg(&main)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        exe
    }

    fn run(exe: &Path, manifest_dir: &Path, ceiling: &Path, env: &[(&str, &str)]) -> Vec<String> {
        let mut cmd = Command::new(exe);
        cmd.env_remove("SOURCE_DATE_EPOCH")
            .env_remove("FL_RELEASE_ID")
            .env("CARGO_MANIFEST_DIR", manifest_dir)
            .env("GIT_CEILING_DIRECTORIES", ceiling);
        for (k, v) in env {
            cmd.env(k, v);
        }
        let out = cmd.output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .map(String::from)
            .collect()
    }

    fn value<'a>(lines: &'a [String], key: &str) -> Option<&'a str> {
        let prefix = format!("cargo:rustc-env={key}=");
        lines.iter().find_map(|l| l.strip_prefix(&prefix))
    }

    #[test]
    fn a_tree_that_is_no_checkout_builds_with_unknown_stamps() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = compile(tmp.path());
        let pkg = tmp.path().join("tarball/crates/x");
        std::fs::create_dir_all(&pkg).unwrap();

        let lines = run(
            &exe,
            &pkg,
            tmp.path(),
            &[("SOURCE_DATE_EPOCH", "1700000000")],
        );
        assert_eq!(value(&lines, "FL_BUILD_COMMIT"), Some("unknown"));
        assert_eq!(value(&lines, "FL_BUILD_DIRTY"), Some("unknown"));
        // SOURCE_DATE_EPOCH, not the clock.
        assert_eq!(value(&lines, "FL_BUILD_TIME"), Some("1700000000"));
        assert_eq!(value(&lines, "FL_RELEASE_ID"), None);
        assert!(lines.contains(&"cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH".to_string()));

        // Without it (or with a malformed one) the build time is the clock.
        for env in [&[][..], &[("SOURCE_DATE_EPOCH", "not-a-number")][..]] {
            let lines = run(&exe, &pkg, tmp.path(), env);
            let t: u64 = value(&lines, "FL_BUILD_TIME").unwrap().parse().unwrap();
            assert!(t > 1_700_000_000, "{t}");
        }

        // Never a release id: without a checkout the tree is not known to be clean.
        let lines = run(&exe, &pkg, tmp.path(), &[("FL_RELEASE_ID", "2026.11.0")]);
        assert_eq!(value(&lines, "FL_RELEASE_ID"), None);
    }

    fn git(dir: &Path, args: &[&str]) -> bool {
        let mut cmd = Command::new("git");
        cmd.arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
            .args(["-c", "commit.gpgsign=false"])
            .args(args);
        super::stamp::scrub_git_env(&mut cmd)
            .output()
            .is_ok_and(|o| o.status.success())
    }

    /// A committed checkout shaped like this workspace (the marker file is what makes it
    /// one) with one crate, `crates/x`, whose directory is the returned manifest directory.
    /// `None`, with a SKIP line, when git is not usable here.
    fn checkout(tmp: &Path, test: &str) -> Option<(PathBuf, PathBuf)> {
        let repo = tmp.join("repo");
        let krate = repo.join("crates/x");
        std::fs::create_dir_all(krate.join("src")).unwrap();
        std::fs::create_dir_all(repo.join("crates/core/build")).unwrap();
        if !git(&repo, &["init", "-q"]) {
            eprintln!("SKIP {test}: `git init` failed (no usable git)");
            return None;
        }
        std::fs::write(repo.join("crates/core/build/stamp.rs"), "").unwrap();
        std::fs::write(krate.join("src/a.rs"), "").unwrap();
        std::fs::write(krate.join("Cargo.toml"), "").unwrap();
        std::fs::write(repo.join("Cargo.lock"), "").unwrap();
        std::fs::write(repo.join("Cargo.toml"), "").unwrap();
        assert!(git(&repo, &["add", "."]));
        assert!(git(&repo, &["commit", "-q", "-m", "one"]));
        Some((repo, krate))
    }

    #[test]
    fn a_git_checkout_is_stamped_with_its_commit_and_watched() {
        let tmp = tempfile::tempdir().unwrap();
        let Some((repo, krate)) = checkout(tmp.path(), "a_git_checkout_is_stamped") else {
            return;
        };
        let exe = compile(tmp.path());
        let lines = run(&exe, &krate, tmp.path(), &[("SOURCE_DATE_EPOCH", "5")]);
        let commit = value(&lines, "FL_BUILD_COMMIT").unwrap();
        assert_eq!(commit.len(), 40, "{lines:?}");
        assert_eq!(value(&lines, "FL_BUILD_DIRTY"), Some("false"));
        assert_eq!(value(&lines, "FL_BUILD_TIME"), Some("5"));
        let watched = |suffix: &str| {
            lines
                .iter()
                .any(|l| l.starts_with("cargo:rerun-if-changed=") && l.ends_with(suffix))
        };
        assert!(watched(".git/HEAD"), "{lines:?}");
        assert!(watched("crates/x/src"));
        assert!(watched("Cargo.lock"));
        // Never the index and never the whole crates directory: either one reruns the script
        // on edits and `git add`s that cannot change what it prints, and a rerun rebuilds.
        assert!(!watched(".git/index"));
        assert!(!watched("crates"));
        std::fs::write(krate.join("src/a.rs"), "// edit").unwrap();
        let lines = run(&exe, &krate, tmp.path(), &[]);
        assert_eq!(value(&lines, "FL_BUILD_DIRTY"), Some("true"));
        assert_eq!(value(&lines, "FL_BUILD_COMMIT"), Some(commit));
        let _ = repo;
    }

    #[test]
    fn shared_stamp_changes_mark_a_binary_without_core_dirty() {
        let tmp = tempfile::tempdir().unwrap();
        let Some((repo, krate)) = checkout(tmp.path(), "shared_stamp_changes") else {
            return;
        };
        let core_source = repo.join("crates/core/src/lib.rs");
        std::fs::create_dir_all(core_source.parent().unwrap()).unwrap();
        std::fs::write(&core_source, "").unwrap();
        assert!(git(&repo, &["add", "."]));
        assert!(git(&repo, &["commit", "-q", "-m", "core runtime"]));
        let exe = compile(tmp.path());
        let dirty =
            || value(&run(&exe, &krate, tmp.path(), &[]), "FL_BUILD_DIRTY").map(String::from);
        assert_eq!(dirty().as_deref(), Some("false"));
        // This binary shares the build helper but does not depend on core's runtime.
        std::fs::write(core_source, "// unrelated runtime edit").unwrap();
        assert_eq!(dirty().as_deref(), Some("false"));
        std::fs::write(repo.join("crates/core/build/stamp.rs"), "// helper edit").unwrap();
        assert_eq!(dirty().as_deref(), Some("true"));
    }

    #[test]
    fn shared_stamp_is_watched_without_core_runtime_sources() {
        let tmp = tempfile::tempdir().unwrap();
        let Some((repo, krate)) = checkout(tmp.path(), "shared_stamp_is_watched") else {
            return;
        };
        std::fs::create_dir_all(repo.join("crates/core/src")).unwrap();
        let exe = compile(tmp.path());
        let lines = run(&exe, &krate, tmp.path(), &[]);
        let watched = |relative: &str| {
            lines.contains(&format!(
                "cargo:rerun-if-changed={}",
                repo.join(relative).canonicalize().unwrap().display()
            ))
        };
        assert!(watched("crates/core/build/stamp.rs"), "{lines:?}");
        assert!(!watched("crates/core/src"));
    }

    /// The same sources always stamp the same bytes, whatever the clock, and the status read
    /// does not write the index (which would make the script rerun for its own write).
    #[test]
    fn a_rerun_on_the_same_head_prints_the_same_bytes_whatever_the_clock_or_the_index() {
        let tmp = tempfile::tempdir().unwrap();
        let Some((repo, krate)) = checkout(tmp.path(), "a_rerun_on_the_same_head") else {
            return;
        };
        let exe = compile(tmp.path());
        let first = run(&exe, &krate, tmp.path(), &[]);
        let t: u64 = value(&first, "FL_BUILD_TIME").unwrap().parse().unwrap();
        // The commit's own time, not the clock: it is in the past, and it stays put.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(t > 1_600_000_000 && t <= now, "{t} vs {now}");
        std::thread::sleep(std::time::Duration::from_millis(1100));
        assert_eq!(first, run(&exe, &krate, tmp.path(), &[]));

        // A change of the index only (stage an edit): identical output.
        std::fs::write(krate.join("src/a.rs"), "// edit").unwrap();
        let dirty = run(&exe, &krate, tmp.path(), &[]);
        assert_eq!(value(&dirty, "FL_BUILD_DIRTY"), Some("true"));
        assert!(git(&repo, &["add", "crates/x/src/a.rs"]));
        std::thread::sleep(std::time::Duration::from_millis(1100));
        assert_eq!(dirty, run(&exe, &krate, tmp.path(), &[]));

        // The status read leaves the index file alone.
        let index = repo.join(".git/index");
        let before = std::fs::metadata(&index).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        run(&exe, &krate, tmp.path(), &[]);
        assert_eq!(
            std::fs::metadata(&index).unwrap().modified().unwrap(),
            before
        );

        // Committing moves HEAD: the commit changes, as it must, and the tree is clean.
        assert!(git(&repo, &["commit", "-q", "-m", "two"]));
        let after = run(&exe, &krate, tmp.path(), &[]);
        assert_ne!(
            value(&after, "FL_BUILD_COMMIT"),
            value(&first, "FL_BUILD_COMMIT")
        );
        assert_eq!(value(&after, "FL_BUILD_DIRTY"), Some("false"));
    }

    #[test]
    fn a_release_id_is_forwarded_for_a_clean_tree_only() {
        let tmp = tempfile::tempdir().unwrap();
        let Some((_, krate)) = checkout(tmp.path(), "a_release_id_is_forwarded") else {
            return;
        };
        let exe = compile(tmp.path());
        let env = [("FL_RELEASE_ID", "2026.11.0")];
        let release = |dir: &Path| {
            value(&run(&exe, dir, tmp.path(), &env), "FL_RELEASE_ID").map(String::from)
        };
        assert_eq!(release(&krate).as_deref(), Some("2026.11.0"));
        std::fs::write(krate.join("src/a.rs"), "// edit").unwrap();
        assert_eq!(release(&krate), None);
        // No checkout at all: the dirty flag is unknown, so no release id either.
        let pkg = tmp.path().join("tarball");
        std::fs::create_dir_all(&pkg).unwrap();
        assert_eq!(release(&pkg), None);
    }
}

/// Every `build.rs` names the crates its package builds from. A crate added to the package's
/// path dependencies and not to the list would leave the dirty flag stale for edits in it.
mod closure {
    use std::collections::BTreeSet;
    use std::path::Path;

    fn crates_dir() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
    }

    /// The `../name` path dependencies of `[dependencies]` and `[build-dependencies]`.
    fn path_deps(krate: &str) -> BTreeSet<String> {
        let text = std::fs::read_to_string(crates_dir().join(krate).join("Cargo.toml")).unwrap();
        let mut in_deps = false;
        let mut out = BTreeSet::new();
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('[') {
                let name = line.trim_matches(|c| c == '[' || c == ']');
                in_deps = name == "dependencies"
                    || name == "build-dependencies"
                    || (name.starts_with("target.")
                        && name.ends_with("dependencies")
                        && !name.contains("dev-"));
            } else if in_deps && let Some(rest) = line.split("path = \"../").nth(1) {
                out.insert(rest.split('"').next().unwrap().to_string());
            }
        }
        out
    }

    fn closure(krate: &str) -> BTreeSet<String> {
        let mut seen = BTreeSet::from([krate.to_string()]);
        let mut todo = vec![krate.to_string()];
        while let Some(k) = todo.pop() {
            for d in path_deps(&k) {
                if seen.insert(d.clone()) {
                    todo.push(d);
                }
            }
        }
        seen
    }

    /// The crates a `build.rs` hands to `stamp::emit(&[...])`: `"."` is the package itself.
    fn listed(krate: &str) -> BTreeSet<String> {
        let text = std::fs::read_to_string(crates_dir().join(krate).join("build.rs")).unwrap();
        let call = text.split("stamp::emit(&[").nth(1).expect("an emit call");
        call.split("])")
            .next()
            .unwrap()
            .split(',')
            .map(|e| e.trim().trim_matches('"'))
            .filter(|e| !e.is_empty())
            .map(|e| match e {
                "." => krate.to_string(),
                other => other.strip_prefix("../").unwrap_or(other).to_string(),
            })
            .collect()
    }

    #[test]
    fn each_build_script_names_its_whole_path_dependency_closure() {
        for krate in ["cli", "store", "hub", "host"] {
            let want = closure(krate);
            assert!(want.len() > 1, "{krate}: {want:?}");
            assert_eq!(listed(krate), want, "crates/{krate}/build.rs");
        }
    }

    #[test]
    fn the_closure_reader_sees_the_real_dependencies() {
        // fl-store builds from fl-core only; fl-cli from the whole workspace.
        assert_eq!(
            closure("store"),
            BTreeSet::from(["store".into(), "core".into()])
        );
        assert_eq!(
            closure("host"),
            BTreeSet::from(["host".into(), "host-proto".into()])
        );
        assert!(closure("cli").contains("hub") && closure("hub").contains("store"));
    }
}
