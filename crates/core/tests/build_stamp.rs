//! Runs the tests of `build/stamp.rs`, the std-only source every `build.rs` that stamps a
//! binary (`fl-cli`, `fl-store`, `fl-hub`) includes by path. `fl-core` itself stays free of
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
                "#[path = {:?}]\nmod stamp;\nfn main() {{ stamp::emit(&[\"src\"]); }}\n",
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

        // A release id is forwarded only when set.
        let lines = run(&exe, &pkg, tmp.path(), &[("FL_RELEASE_ID", "2026.11.0")]);
        assert_eq!(value(&lines, "FL_RELEASE_ID"), Some("2026.11.0"));
    }

    #[test]
    fn a_git_checkout_is_stamped_with_its_commit_and_watched() {
        let git = |dir: &Path, args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
                .args(["-c", "commit.gpgsign=false"])
                .args(args)
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .output()
                .is_ok_and(|o| o.status.success())
        };
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join("src")).unwrap();
        if !git(&repo, &["init", "-q"]) {
            return; // no git here: the other test covers the fallback
        }
        std::fs::write(repo.join("src/a.rs"), "").unwrap();
        assert!(git(&repo, &["add", "."]));
        assert!(git(&repo, &["commit", "-q", "-m", "one"]));
        let exe = compile(tmp.path());
        let lines = run(&exe, &repo, tmp.path(), &[("SOURCE_DATE_EPOCH", "5")]);
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
        assert!(watched(".git/index"));
        assert!(watched("/src"));
        std::fs::write(repo.join("src/a.rs"), "// edit").unwrap();
        let lines = run(&exe, &repo, tmp.path(), &[]);
        assert_eq!(value(&lines, "FL_BUILD_DIRTY"), Some("true"));
        assert_eq!(value(&lines, "FL_BUILD_COMMIT"), Some(commit));
    }
}
