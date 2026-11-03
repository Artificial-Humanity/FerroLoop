//! Test fixtures: an EXTRACTED artifact tree for the host, and a signed-release manifest that
//! pins it with its tree digest. Used by this crate's tests and, through the `fixtures`
//! feature, by fl-cli's.

use crate::tree::scan_tree;
use fl_core::install::{
    Artifact, HostInfo, Product, ReleaseKind, ReleaseManifest, default_manifest, sha256_hex,
};
use std::fs;
use std::path::Path;

/// The files a release of `variant` holds (relative path, content).
pub fn payload(variant: &str) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = vec![
        ("bin/fl".into(), format!("#!/bin/sh\necho fl {variant}\n")),
        (
            "bin/ferrostore".into(),
            format!("#!/bin/sh\necho ferrostore {variant}\n"),
        ),
        (
            "contrib/units/ferrostore.service".into(),
            format!("[Unit]\nDescription=store {variant}\n"),
        ),
        (
            "contrib/units/ferrostore@.service".into(),
            format!("[Unit]\nDescription=store instance {variant}\n"),
        ),
        (
            "contrib/units/ferrowire.service".into(),
            format!("[Unit]\nDescription=wire {variant}\n"),
        ),
        (
            "contrib/units/ferroloop-hub.service".into(),
            format!("[Unit]\nDescription=hub {variant}\n"),
        ),
        (
            "ui/dist/index.html".into(),
            format!("<html>{variant}</html>\n"),
        ),
        (
            "ui/dist/assets/app.js".into(),
            format!("console.log('{variant}');\n"),
        ),
        // A file no product declares: covered by the digest, never staged.
        ("NOTICE".into(), format!("notice {variant}\n")),
    ];
    v.sort();
    v
}

/// Write the artifact tree `<root>/<name>/` for `variant`.
pub fn write_artifact(root: &Path, name: &str, variant: &str) {
    for (rel, content) in payload(variant) {
        let p = root.join(name).join(&rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }
}

/// A signed-release manifest for the host: the default manifest's products and profiles, one
/// artifact `<name>` that provides every product, pinned by the tree digest of what is on disk.
pub fn manifest_for(root: &Path, name: &str, version: &str) -> ReleaseManifest {
    let scanned = scan_tree(&root.join(name), u64::MAX / 2).expect("fixture tree scans");
    let host = HostInfo::from_consts(std::env::consts::OS, std::env::consts::ARCH)
        .expect("a supported host");
    let mut m = default_manifest();
    m.release.version = version.to_string();
    m.release.kind = ReleaseKind::Release;
    m.release.commit = Some(sha256_hex(version.as_bytes())[..40].to_string());
    m.artifacts = vec![Artifact {
        id: format!(
            "tree-{host_os}-{host_arch}",
            host_os = host.os.as_wire(),
            host_arch = host.arch.as_wire()
        ),
        target: host.target(),
        file: name.to_string(),
        sha256: scanned.digest,
        size: scanned.total_bytes,
        provides: Product::ALL.iter().map(|p| p.to_string()).collect(),
    }];
    m
}

/// Write the artifact tree and the manifest for one release in one go.
pub fn release(root: &Path, version: &str, variant: &str) -> ReleaseManifest {
    let name = format!("payload-{version}-{variant}");
    write_artifact(root, &name, variant);
    manifest_for(root, &name, version)
}

/// The release directory name a fixture manifest produces.
pub fn release_name(m: &ReleaseManifest) -> String {
    fl_core::install::release_dir_name(m)
}

// ------------------------------------------------------------------ source builds (A4b)
//
// A throwaway git repository with three commits and a fake `origin/main`, and fake `cargo`,
// `rustc` and `npm` executables for a constructed PATH. Nothing here runs the real toolchain.

pub mod gitrepo {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    /// Directories a fixture PATH needs for `sh`, `env`, `git`, `cp`, `sleep`, `touch`.
    pub fn system_path() -> String {
        let mut dirs: Vec<String> = vec!["/usr/bin".into(), "/bin".into()];
        if let Some(p) = std::env::var_os("PATH") {
            for d in std::env::split_paths(&p) {
                if d.join("git").is_file() && !dirs.iter().any(|x| Path::new(x) == d) {
                    dirs.push(d.display().to_string());
                }
            }
        }
        dirs.join(":")
    }

    /// Run git in `dir` with a clean environment and a fixed identity; the stdout, trimmed.
    pub fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env_clear()
            .env("PATH", system_path())
            .env("HOME", "/nonexistent")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Fixture")
            .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
            .env("GIT_COMMITTER_NAME", "Fixture")
            .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
            .env("GIT_AUTHOR_DATE", "1700000500 +0000")
            .env("GIT_COMMITTER_DATE", "1700000500 +0000")
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A repository: `commits[0]` is the root, `commits[1]` is `origin/main` (and carries a
    /// `gpgsig` header, so a `log.showSignature` config would run a `gpg.program`),
    /// `commits[2]` is HEAD and NOT reviewed.
    pub struct Repo {
        pub dir: PathBuf,
        pub commits: Vec<String>,
    }

    fn write(dir: &Path, rel: &str, text: &str) {
        let p = dir.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }

    pub fn make_repo(dir: &Path) -> Repo {
        fs::create_dir_all(dir).unwrap();
        git(dir, &["init", "-q", "-b", "main"]);
        write(
            dir,
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/core\"]\n\n[workspace.package]\nversion = \"0.1.0\"\n",
        );
        write(dir, "Cargo.lock", "version = 4\n");
        write(dir, "crates/core/build/stamp.rs", "// marker\n");
        write(
            dir,
            "contrib/units/ferrostore.service",
            "[Unit]\nDescription=store\n",
        );
        write(
            dir,
            "contrib/units/ferrostore@.service",
            "[Unit]\nDescription=store instance\n",
        );
        write(
            dir,
            "contrib/units/ferrowire.service",
            "[Unit]\nDescription=wire\n",
        );
        // Attributes that would change the bytes of a checkout or an archive.
        write(
            dir,
            ".gitattributes",
            "stamp.txt export-subst ident\n* filter=evil\n",
        );
        write(dir, "stamp.txt", "$Format:%H$ $Id$\n");
        write(dir, "README.md", "one\n");
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-q", "-m", "one"]);
        let c1 = git(dir, &["rev-parse", "HEAD"]);

        write(dir, "README.md", "two\n");
        write(dir, "docs/a.md", "doc\n");
        git(dir, &["add", "-A"]);
        let tree = git(dir, &["write-tree"]);
        let raw = format!(
            "tree {tree}\nparent {c1}\nauthor Fixture <fixture@example.invalid> 1700000200 +0000\n\
             committer Fixture <fixture@example.invalid> 1700000200 +0000\n\
             gpgsig -----BEGIN PGP SIGNATURE-----\n \n AAAA\n -----END PGP SIGNATURE-----\n\ntwo\n"
        );
        let c2 = {
            use std::io::Write;
            let mut ch = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["hash-object", "-t", "commit", "-w", "--stdin"])
                .env_clear()
                .env("PATH", system_path())
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            ch.stdin.take().unwrap().write_all(raw.as_bytes()).unwrap();
            let o = ch.wait_with_output().unwrap();
            assert!(o.status.success());
            String::from_utf8(o.stdout).unwrap().trim().to_string()
        };
        git(dir, &["update-ref", "refs/heads/main", &c2]);
        git(dir, &["reset", "-q", "--hard", &c2]);

        write(dir, "README.md", "three\n");
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-q", "-m", "three"]);
        let c3 = git(dir, &["rev-parse", "HEAD"]);
        git(dir, &["update-ref", "refs/remotes/origin/main", &c2]);
        Repo {
            dir: dir.to_path_buf(),
            commits: vec![c1, c2, c3],
        }
    }

    /// What the fake `cargo` does.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub enum FakeCargo {
        /// Builds stub `fl` and `ferrostore` that report the commit of the tree it ran in.
        Ok,
        /// Prints `boom` and exits 101.
        Fail,
        /// Starts a background `sleep` (pid in `pid_file`) and waits for it.
        Hang,
        /// Builds stubs that report a different commit.
        WrongCommit,
        /// Builds stubs that report a dirty tree.
        Dirty,
    }

    /// The fake toolchain on disk and the files it leaves evidence in.
    pub struct FakeTools {
        pub bin_dir: PathBuf,
        pub evidence: PathBuf,
    }

    impl FakeTools {
        pub fn env_dump(&self) -> PathBuf {
            self.evidence.join("cargo.env")
        }
        pub fn args_dump(&self) -> PathBuf {
            self.evidence.join("cargo.args")
        }
        pub fn cwd_dump(&self) -> PathBuf {
            self.evidence.join("cargo.cwd")
        }
        pub fn count_file(&self) -> PathBuf {
            self.evidence.join("cargo.count")
        }
        pub fn pid_file(&self) -> PathBuf {
            self.evidence.join("child.pid")
        }
        pub fn src_dump(&self) -> PathBuf {
            self.evidence.join("src")
        }
        /// How many times `cargo build` ran.
        pub fn builds(&self) -> u32 {
            fs::read_to_string(self.count_file())
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(0)
        }
        /// A PATH with the fakes first.
        pub fn path(&self) -> String {
            format!("{}:{}", self.bin_dir.display(), system_path())
        }
    }

    fn exe(path: &Path, text: &str) {
        fs::write(path, text).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Write fake `cargo`, `rustc` and `npm` under `dir/bin`, evidence under `dir/evidence`.
    pub fn fake_tools(dir: &Path, mode: FakeCargo) -> FakeTools {
        let bin_dir = dir.join("bin");
        let evidence = dir.join("evidence");
        fs::create_dir_all(&bin_dir).unwrap();
        fs::create_dir_all(evidence.join("src")).unwrap();
        let t = FakeTools {
            bin_dir: bin_dir.clone(),
            evidence: evidence.clone(),
        };
        let q = |p: PathBuf| p.display().to_string();
        let build = match mode {
            FakeCargo::Fail => "echo 'boom: could not compile' >&2\nexit 101\n".to_string(),
            FakeCargo::Hang => format!("sleep 300 &\necho $! > '{}'\nwait\n", q(t.pid_file())),
            FakeCargo::Ok | FakeCargo::WrongCommit | FakeCargo::Dirty => {
                let sha = if mode == FakeCargo::WrongCommit {
                    "echo 0000000000000000000000000000000000000000".to_string()
                } else {
                    "git rev-parse HEAD".to_string()
                };
                let dirty = if mode == FakeCargo::Dirty {
                    "true"
                } else {
                    "false"
                };
                format!(
                    r#"sha=$({sha})
ct=$(git log -1 --format=%ct HEAD)
dirty={dirty}
[ -n "$(git status --porcelain)" ] && dirty=true
mkdir -p target/release
for name in fl ferrostore; do
  {{
    echo '#!/bin/sh'
    echo 'if [ "$1" = "--version" ] && [ "$2" = "--json" ]; then'
    echo "  echo '{{\"name\":\"'$name'\",\"version\":\"0.1.0\",\"commit\":\"'$sha'\",\"commit_short\":\"'$(echo $sha | cut -c1-12)'\",\"dirty\":'$dirty',\"build_time_unix\":'$ct',\"source\":\"source-build\",\"release_id\":null,\"protocol\":{{\"ferrowire\":{{\"current\":2}}}},\"schema\":{{}},\"store_formats\":{{\"redb_schema\":{{\"min\":2,\"max\":8}}}}}}'"
    echo 'else'
    echo "  echo $name stub $sha"
    echo 'fi'
  }} > target/release/$name
  chmod 755 target/release/$name
done
"#
                )
            }
        };
        let cargo = format!(
            r#"#!/bin/sh
case "$1" in -V|--version) echo "cargo 9.9.9 (fake 2026-01-01)"; exit 0;; esac
n=$(cat '{count}' 2>/dev/null || echo 0)
echo $((n+1)) > '{count}'
echo "$@" > '{args}'
env | sort > '{env}'
pwd > '{cwd}'
cp stamp.txt '{src}/stamp.txt' 2>/dev/null
cp .gitattributes '{src}/gitattributes' 2>/dev/null
ls -la > '{src}/ls.txt'
{build}"#,
            count = q(t.count_file()),
            args = q(t.args_dump()),
            env = q(t.env_dump()),
            cwd = q(t.cwd_dump()),
            src = q(t.src_dump()),
        );
        exe(&bin_dir.join("cargo"), &cargo);
        exe(
            &bin_dir.join("rustc"),
            "#!/bin/sh\necho 'rustc 9.9.9 (fake 2026-01-01)'\n",
        );
        exe(
            &bin_dir.join("npm"),
            r#"#!/bin/sh
case "$1" in
  ci) exit 0;;
  run) mkdir -p dist/assets && echo '<html>ui</html>' > dist/index.html && echo 'x' > dist/assets/app.js; exit 0;;
esac
exit 1
"#,
        );
        t
    }

    /// Add a minimal UI project to the repository and commit it; returns the new commit.
    pub fn add_ui_commit(repo: &Repo) -> String {
        write(&repo.dir, "ui/package.json", "{}\n");
        write(&repo.dir, "ui/package-lock.json", "{}\n");
        git(&repo.dir, &["add", "-A"]);
        git(&repo.dir, &["commit", "-q", "-m", "ui"]);
        let c = git(&repo.dir, &["rev-parse", "HEAD"]);
        git(&repo.dir, &["update-ref", "refs/remotes/origin/main", &c]);
        c
    }
}
