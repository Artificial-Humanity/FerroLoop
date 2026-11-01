//! `fl store status`, `checkpoint`, `migrate` and `verify`, black box, on both
//! engines.
//!
//! Every run gets private HOME, XDG and `$FL_DB` so nothing reads the
//! developer's own stores, and none of these commands may create a store.

use assert_cmd::Command;
use fl_store::{BackendKind, StoreOptions};
use predicates::str::contains;
use serde_json::Value;
use std::path::{Path, PathBuf};

struct Env {
    home: tempfile::TempDir,
    stores: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        Self {
            home: tempfile::tempdir().unwrap(),
            stores: tempfile::tempdir().unwrap(),
        }
    }

    /// `fl` with no engine selected in the environment.
    fn fl(&self) -> Command {
        let mut c = Command::cargo_bin("fl").unwrap();
        c.env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.home.path().join("config"))
            .env("XDG_DATA_HOME", self.home.path().join("data"))
            .env_remove("FL_DB")
            .env_remove("FL_STORE_BACKEND")
            .current_dir(self.home.path());
        c
    }

    fn ferro(&self) -> Command {
        let mut c = self.fl();
        c.env("FL_STORE_BACKEND", "ferro");
        c
    }

    fn logical(&self) -> PathBuf {
        self.stores.path().join("fl.redb")
    }

    fn physical_ferro(&self) -> PathBuf {
        self.stores.path().join("fl.fls")
    }

    fn files(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.stores.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
}

/// What a store directory holds after an automatic migration: the pair, and
/// the empty migration lock file that is left in place (documented in
/// `fl_store::MigrationLock`).
fn with_lock() -> Vec<String> {
    vec![
        "fl.fls".to_string(),
        "fl.fls.migrate.lock".to_string(),
        "fl.redb".to_string(),
    ]
}

fn open_with(path: &Path, backend: BackendKind) {
    drop(
        fl_store::open_store_with(
            path,
            &StoreOptions {
                backend: Some(backend),
                ..StoreOptions::default()
            },
        )
        .unwrap(),
    );
}

fn status_json(mut cmd: Command, db: &Path) -> Value {
    let out = cmd
        .args(["--db"])
        .arg(db)
        .args(["store", "status", "--json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn status_on_redb_reports_a_store_that_does_not_exist_and_creates_nothing() {
    let env = Env::new();
    let v = status_json(env.fl(), &env.logical());
    assert_eq!(v["engine"], "redb");
    assert_eq!(v["exists"], false);
    assert_eq!(v["size_bytes"], Value::Null);
    assert_eq!(v["format_version"], Value::Null);
    assert_eq!(v["logical_path"], env.logical().display().to_string());
    assert_eq!(v["physical_path"], env.logical().display().to_string());
    assert!(env.files().is_empty(), "{:?}", env.files());
}

#[test]
fn status_on_redb_reports_size_and_format_version() {
    let env = Env::new();
    open_with(&env.logical(), BackendKind::Redb);
    let v = status_json(env.fl(), &env.logical());
    assert_eq!(v["engine"], "redb");
    assert_eq!(v["exists"], true);
    assert_eq!(
        v["size_bytes"].as_u64().unwrap(),
        std::fs::metadata(env.logical()).unwrap().len()
    );
    assert!(v["format_version"].is_u64(), "{v}");
}

#[test]
fn status_in_text_names_each_field() {
    let env = Env::new();
    open_with(&env.logical(), BackendKind::Redb);
    env.fl()
        .arg("--db")
        .arg(env.logical())
        .args(["store", "status"])
        .assert()
        .success()
        .stdout(contains("engine:         redb"))
        .stdout(contains("exists:         yes"))
        .stdout(contains("format version: "));
}

#[test]
fn status_on_ferro_maps_the_logical_path_and_says_the_format_version_is_not_available() {
    let env = Env::new();
    let v = status_json(env.ferro(), &env.logical());
    assert_eq!(v["engine"], "ferro");
    assert_eq!(v["exists"], false);
    assert_eq!(v["logical_path"], env.logical().display().to_string());
    assert_eq!(
        v["physical_path"],
        env.physical_ferro().display().to_string()
    );
    assert_eq!(v["format_version"], "n/a");
    assert!(env.files().is_empty(), "{:?}", env.files());

    open_with(&env.logical(), BackendKind::Ferro);
    let v = status_json(env.ferro(), &env.logical());
    assert_eq!(v["exists"], true);
    assert_eq!(
        v["size_bytes"].as_u64().unwrap(),
        std::fs::metadata(env.physical_ferro()).unwrap().len()
    );
    assert_eq!(v["format_version"], "n/a");
}

#[test]
fn status_on_an_fls_path_is_ferro_without_the_environment() {
    let env = Env::new();
    let v = status_json(env.fl(), &env.physical_ferro());
    assert_eq!(v["engine"], "ferro");
    assert_eq!(
        v["physical_path"],
        env.physical_ferro().display().to_string()
    );
}

#[test]
fn status_flags_a_redb_beside_a_ferro_selection() {
    let env = Env::new();
    open_with(&env.logical(), BackendKind::Redb);
    let v = status_json(env.ferro(), &env.logical());
    assert_eq!(v["exists"], false);
    assert_eq!(v["redb_beside_ferro"], true);
    assert_eq!(env.files(), vec!["fl.redb".to_string()]);
}

#[test]
fn status_refuses_an_unknown_engine() {
    let env = Env::new();
    let mut c = env.fl();
    c.env("FL_STORE_BACKEND", "sqlite")
        .arg("--db")
        .arg(env.logical())
        .args(["store", "status"])
        .assert()
        .failure()
        .stderr(contains("unknown store backend"));
}

#[test]
fn checkpoint_on_redb_does_nothing_and_succeeds() {
    let env = Env::new();
    env.fl()
        .arg("--db")
        .arg(env.logical())
        .args(["store", "checkpoint"])
        .assert()
        .success()
        .stdout(contains("redb has no checkpoint; nothing to do"));
    assert!(env.files().is_empty(), "{:?}", env.files());
}

#[test]
fn checkpoint_on_ferro_checkpoints_an_existing_store() {
    let env = Env::new();
    open_with(&env.logical(), BackendKind::Ferro);
    env.ferro()
        .arg("--db")
        .arg(env.logical())
        .args(["store", "checkpoint"])
        .assert()
        .success()
        .stdout(contains("checkpointed"))
        .stdout(contains("fl.fls"));
}

#[test]
fn checkpoint_on_ferro_does_not_create_a_missing_store() {
    let env = Env::new();
    env.ferro()
        .arg("--db")
        .arg(env.logical())
        .args(["store", "checkpoint"])
        .assert()
        .failure()
        .stderr(contains("no store at"));
    assert!(env.files().is_empty(), "{:?}", env.files());
}

// ---- ordinary commands with ferro selected, and the missing-directory rule ----

const STRANGER: &str = "urn:uuid:0190a1b2-c3d4-7e5f-8a6b-7c8d9e0f1a2b";

/// One entry of a [`snapshot`]: path, is-a-directory, size, mtime, file bytes.
type Entry = (PathBuf, bool, u64, Option<std::time::SystemTime>, Vec<u8>);

/// Every entry under `roots` with its kind, size, mtime and, for a file, its
/// bytes: a tree that compares equal before and after was not touched.
fn snapshot(roots: &[&Path]) -> Vec<Entry> {
    fn walk(dir: &Path, out: &mut Vec<Entry>) {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        entries.sort();
        for p in entries {
            let meta = std::fs::symlink_metadata(&p).unwrap();
            let is_dir = meta.is_dir();
            let bytes = if meta.is_file() {
                std::fs::read(&p).unwrap()
            } else {
                Vec::new()
            };
            out.push((p.clone(), is_dir, meta.len(), meta.modified().ok(), bytes));
            if is_dir {
                walk(&p, out);
            }
        }
    }
    let mut out = Vec::new();
    for r in roots {
        walk(r, &mut out);
    }
    out
}

/// A config that binds a project directory to `store`; returns the project.
fn bind_project(env: &Env, store: &Path) -> tempfile::TempDir {
    let proj = tempfile::tempdir().unwrap();
    let dir = env.home.path().join("config").join("fl");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("config.toml"),
        format!(
            "[[project]]\nroot = \"{}\"\nstore = \"{}\"\n",
            proj.path().display(),
            store.display()
        ),
    )
    .unwrap();
    proj
}

#[test]
fn ferro_with_neither_file_present_an_iri_lookup_is_not_owned_and_creates_nothing() {
    let env = Env::new();
    let proj = bind_project(&env, &env.logical());
    let before = snapshot(&[env.home.path(), env.stores.path(), proj.path()]);
    env.ferro()
        .current_dir(proj.path())
        .args(["gate", "show", STRANGER])
        .assert()
        .code(2)
        .stderr(contains("no store"));
    assert_eq!(
        before,
        snapshot(&[env.home.path(), env.stores.path(), proj.path()]),
        "the lookup touched the tree"
    );
    assert!(env.files().is_empty(), "{:?}", env.files());
}

#[test]
fn ferro_with_an_explicit_db_an_iri_lookup_is_not_owned_and_creates_nothing() {
    let env = Env::new();
    let before = snapshot(&[env.home.path(), env.stores.path()]);
    env.ferro()
        .arg("--db")
        .arg(env.logical())
        .args(["gate", "show", STRANGER])
        .assert()
        .code(2)
        .stderr(contains("no store holds"));
    assert_eq!(
        before,
        snapshot(&[env.home.path(), env.stores.path()]),
        "the lookup touched the tree"
    );
}

#[test]
fn ferro_migrates_a_stray_redb_on_first_use_and_keeps_it_read_only() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new();
    {
        let store = fl_store::open_redb(&env.logical()).unwrap();
        fl_core::Catalog::add_project(&store, "/work/app").unwrap();
    }
    let proj = bind_project(&env, &env.logical());

    env.ferro()
        .current_dir(proj.path())
        .args(["project", "list"])
        .assert()
        .success()
        .stdout(contains("/work/app"));

    assert_eq!(env.files(), with_lock());
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&env.physical_ferro()), 0o600);
    assert_eq!(mode(&env.logical()), 0o400);
}

#[test]
fn status_and_checkpoint_do_not_create_a_missing_directory_on_either_engine() {
    for ferro in [false, true] {
        for sub in ["status", "checkpoint"] {
            let env = Env::new();
            let missing = env.stores.path().join("not-yet");
            let db = missing.join("fl.redb");
            let mut cmd = if ferro { env.ferro() } else { env.fl() };
            cmd.arg("--db").arg(&db).args(["store", sub]);
            let out = cmd.output().unwrap();
            // status succeeds; checkpoint succeeds on redb and is refused on ferro.
            let expect_ok = !(ferro && sub == "checkpoint");
            assert_eq!(
                out.status.success(),
                expect_ok,
                "ferro={ferro} {sub}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(
                !missing.exists(),
                "ferro={ferro} {sub} created the directory"
            );
            assert!(env.files().is_empty(), "{:?}", env.files());
        }
    }
}

#[test]
fn status_on_an_empty_redb_file_reports_it_and_does_not_modify_it() {
    let env = Env::new();
    std::fs::write(env.logical(), b"").unwrap();
    let mtime = std::fs::metadata(env.logical())
        .unwrap()
        .modified()
        .unwrap();
    let v = status_json(env.fl(), &env.logical());
    assert_eq!(v["exists"], true);
    assert_eq!(v["size_bytes"], 0);
    assert_eq!(v["format_version"], Value::Null);
    let meta = std::fs::metadata(env.logical()).unwrap();
    assert_eq!(meta.len(), 0, "status initialised the file");
    assert_eq!(meta.modified().unwrap(), mtime);

    env.fl()
        .arg("--db")
        .arg(env.logical())
        .args(["store", "status"])
        .assert()
        .success()
        .stdout(contains("empty file"));
    assert_eq!(std::fs::metadata(env.logical()).unwrap().len(), 0);
}

#[test]
fn ferro_treats_a_redb_and_an_fls_alias_of_one_store_as_one_candidate() {
    let env = Env::new();
    let repo = tempfile::tempdir().unwrap();
    for args in [
        &["init", "-q"][..],
        &["config", "user.email", "t@example.com"],
        &["config", "user.name", "t"],
        &["commit", "-q", "--allow-empty", "-m", "first"],
    ] {
        assert!(
            std::process::Command::new("git")
                .args(args)
                .current_dir(repo.path())
                .status()
                .unwrap()
                .success()
        );
    }
    let other = tempfile::tempdir().unwrap();
    let dir = env.home.path().join("config").join("fl");
    std::fs::create_dir_all(&dir).unwrap();
    // Two config entries, one store: `fl.redb` is `fl.fls` under ferro.
    std::fs::write(
        dir.join("config.toml"),
        format!(
            "[[project]]\nroot = \"{}\"\nstore = \"{}\"\n\n[[project]]\nroot = \"{}\"\nstore = \"{}\"\n",
            repo.path().display(),
            env.logical().display(),
            other.path().display(),
            env.physical_ferro().display(),
        ),
    )
    .unwrap();
    env.ferro()
        .current_dir(repo.path())
        .args(["project", "add", repo.path().to_str().unwrap()])
        .assert()
        .success();
    env.ferro()
        .current_dir(repo.path())
        .args([
            "gate",
            "add",
            "--project",
            "1",
            "--name",
            "g",
            "--glob",
            "*.rs",
            "--program",
            "true",
        ])
        .assert()
        .success();
    let shown = env
        .ferro()
        .current_dir(repo.path())
        .args(["gate", "show", "1"])
        .output()
        .unwrap();
    let iri = serde_json::from_slice::<Value>(&shown.stdout).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    env.ferro()
        .current_dir(repo.path())
        .args(["gate", "show", &iri])
        .assert()
        .success();
}

// ---- fl store migrate / verify, and auto-migration ----

/// A git repo to register, so `project add` accepts it.
fn git_repo() -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    for args in [
        &["init", "-q"][..],
        &["config", "user.email", "t@example.com"],
        &["config", "user.name", "t"],
        &["commit", "-q", "--allow-empty", "-m", "first"],
    ] {
        assert!(
            std::process::Command::new("git")
                .args(args)
                .current_dir(repo.path())
                .status()
                .unwrap()
                .success()
        );
    }
    repo
}

/// A redb store with a project and a gate in it, made with the CLI.
fn populate_redb(env: &Env, repo: &Path) {
    env.fl()
        .arg("--db")
        .arg(env.logical())
        .args(["project", "add", repo.to_str().unwrap()])
        .assert()
        .success();
    env.fl()
        .arg("--db")
        .arg(env.logical())
        .args([
            "gate",
            "add",
            "--project",
            "1",
            "--name",
            "g",
            "--glob",
            "*.rs",
            "--program",
            "true",
        ])
        .assert()
        .success();
}

fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn json_of(out: &std::process::Output) -> Value {
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&out.stdout)))
}

#[test]
fn migrate_dry_run_then_migrate_then_verify_then_refuse_a_second_migration() {
    let env = Env::new();
    let repo = git_repo();
    populate_redb(&env, repo.path());
    let scratch = tempfile::tempdir().unwrap();
    let before = snapshot(&[env.stores.path()]);

    // Dry run: reports a clean migration, writes no real file, and its private
    // copy is gone afterwards.
    let out = env
        .fl()
        .env("TMPDIR", scratch.path())
        .arg("--db")
        .arg(env.logical())
        .args(["store", "migrate", "--dry-run", "--json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = json_of(&out);
    assert_eq!(v["dry_run"], true);
    assert_eq!(v["clean"], true);
    assert_eq!(v["fls"], Value::Null);
    assert_eq!(v["redb_kept_read_only"], false);
    let projects = v["tables"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "projects")
        .expect("a projects row");
    assert_eq!(projects["redb_rows"], 1);
    assert_eq!(projects["ferro_rows"], 1);
    assert_eq!(
        before,
        snapshot(&[env.stores.path()]),
        "the dry run wrote a real file"
    );
    assert!(
        std::fs::read_dir(scratch.path()).unwrap().next().is_none(),
        "the private copy was not deleted"
    );
    assert_eq!(env.files(), vec!["fl.redb".to_string()]);

    // The real migration.
    env.fl()
        .arg("--db")
        .arg(env.logical())
        .args(["store", "migrate"])
        .assert()
        .success()
        .stdout(contains("projects"))
        .stdout(contains("clean:"))
        .stdout(contains("read-only (0400)"));
    assert_eq!(env.files(), with_lock());
    assert_eq!(mode(&env.physical_ferro()), 0o600);
    assert_eq!(mode(&env.logical()), 0o400);

    // The pair verifies, and verify leaves the read-only redb as it is.
    let pair = snapshot(&[env.stores.path()]);
    let out = env
        .fl()
        .env("TMPDIR", scratch.path())
        .arg("--db")
        .arg(env.logical())
        .args(["store", "verify", "--json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = json_of(&out);
    assert_eq!(v["clean"], true);
    assert_eq!(v["mismatches"].as_array().unwrap().len(), 0);
    env.fl()
        .arg("--db")
        .arg(env.logical())
        .args(["store", "verify"])
        .assert()
        .success()
        .stdout(contains("every table and view matches"));
    assert!(std::fs::read_dir(scratch.path()).unwrap().next().is_none());
    // Opening the FerroStore may fold its log, so only the redb is compared.
    let after = snapshot(&[env.stores.path()]);
    let redb = |t: &[Entry]| t.iter().find(|e| e.0 == env.logical()).cloned();
    assert_eq!(redb(&pair), redb(&after), "verify touched the redb");
    assert_eq!(mode(&env.logical()), 0o400);

    // A second migration is refused and changes nothing.
    env.fl()
        .arg("--db")
        .arg(env.logical())
        .args(["store", "migrate"])
        .assert()
        .failure()
        .stderr(contains("already exists"));
    env.fl()
        .arg("--db")
        .arg(env.logical())
        .args(["store", "migrate", "--dry-run"])
        .assert()
        .failure()
        .stderr(contains("already exists"));
    assert_eq!(redb(&pair), redb(&snapshot(&[env.stores.path()])));
}

#[test]
fn verify_exits_1_and_names_the_table_when_the_pair_differs() {
    use fl_core::Catalog;
    let env = Env::new();
    let repo = git_repo();
    populate_redb(&env, repo.path());
    env.fl()
        .arg("--db")
        .arg(env.logical())
        .args(["store", "migrate"])
        .assert()
        .success();
    {
        let ferro = fl_store::open_store_with(
            &env.logical(),
            &StoreOptions {
                backend: Some(BackendKind::Ferro),
                ..StoreOptions::default()
            },
        )
        .unwrap();
        ferro.add_project("/only/in/ferro").unwrap();
    }
    let out = env
        .fl()
        .arg("--db")
        .arg(env.logical())
        .args(["store", "verify", "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let v = json_of(&out);
    assert_eq!(v["clean"], false);
    assert!(
        v["mismatches"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m == "projects"),
        "{v}"
    );
    env.fl()
        .arg("--db")
        .arg(env.logical())
        .args(["store", "verify"])
        .assert()
        .code(1)
        .stdout(contains("MISMATCH"))
        .stderr(contains("mismatches:"));
}

#[test]
fn migrate_and_verify_refuse_what_they_cannot_do_and_create_nothing() {
    let env = Env::new();
    for args in [
        &["store", "migrate"][..],
        &["store", "migrate", "--dry-run"],
        &["store", "verify"],
    ] {
        // No store at all.
        env.fl()
            .arg("--db")
            .arg(env.logical())
            .args(args)
            .assert()
            .failure()
            .stderr(contains("no redb store at"));
        assert!(env.files().is_empty(), "{args:?}: {:?}", env.files());
        // An explicit .fls path names no redb.
        env.fl()
            .arg("--db")
            .arg(env.physical_ferro())
            .args(args)
            .assert()
            .failure()
            .stderr(contains("names a .fls file"));
        assert!(env.files().is_empty(), "{args:?}: {:?}", env.files());
    }

    // verify needs both files.
    open_with(&env.logical(), BackendKind::Redb);
    let before = snapshot(&[env.stores.path()]);
    env.fl()
        .arg("--db")
        .arg(env.logical())
        .args(["store", "verify"])
        .assert()
        .failure()
        .stderr(contains("no FerroStore at"));
    assert_eq!(before, snapshot(&[env.stores.path()]));
    std::fs::remove_file(env.logical()).unwrap();
    open_with(&env.logical(), BackendKind::Ferro);
    env.fl()
        .arg("--db")
        .arg(env.logical())
        .args(["store", "verify"])
        .assert()
        .failure()
        .stderr(contains("no redb store at"));
}

#[test]
fn migrate_and_verify_refuse_a_redb_another_process_holds() {
    let env = Env::new();
    let repo = git_repo();
    populate_redb(&env, repo.path());
    // The test process is "another process" to the CLI: redb's file lock is
    // exclusive across processes.
    let holder = fl_store::open_redb(&env.logical()).unwrap();
    let before = snapshot(&[env.stores.path()]);
    for args in [
        &["store", "migrate"][..],
        &["store", "migrate", "--dry-run"],
    ] {
        env.fl()
            .arg("--db")
            .arg(env.logical())
            .args(args)
            .assert()
            .failure()
            .stderr(contains("held by another process"));
    }
    assert_eq!(before, snapshot(&[env.stores.path()]));
    drop(holder);
    env.fl()
        .arg("--db")
        .arg(env.logical())
        .args(["store", "migrate"])
        .assert()
        .success();
}

#[test]
fn an_ordinary_ferro_command_migrates_a_redb_only_home() {
    let env = Env::new();
    let repo = git_repo();
    populate_redb(&env, repo.path());
    assert_eq!(env.files(), vec!["fl.redb".to_string()]);

    env.ferro()
        .arg("--db")
        .arg(env.logical())
        .args(["project", "list"])
        .assert()
        .success()
        .stdout(contains(repo.path().to_str().unwrap()));
    assert_eq!(env.files(), with_lock());
    assert_eq!(mode(&env.physical_ferro()), 0o600);
    assert_eq!(mode(&env.logical()), 0o400);

    // Later ferro commands read the .fls; nothing migrates again.
    let fls = std::fs::metadata(env.physical_ferro()).unwrap();
    env.ferro()
        .arg("--db")
        .arg(env.logical())
        .args(["gate", "list", "--project", "1"])
        .assert()
        .success()
        .stdout(contains("g"));
    assert_eq!(
        std::os::unix::fs::MetadataExt::ino(&fls),
        std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(env.physical_ferro()).unwrap())
    );

    // The redb engine now meets a read-only file. Unless the tests run as
    // root (which ignores the mode), it cannot open it for write and says so,
    // rather than reading a store that has moved on.
    if std::fs::OpenOptions::new()
        .write(true)
        .open(env.logical())
        .is_err()
    {
        env.fl()
            .arg("--db")
            .arg(env.logical())
            .args(["project", "list"])
            .assert()
            .failure();
        assert_eq!(mode(&env.logical()), 0o400);
    } else {
        eprintln!(
            "SKIPPED the redb-engine-refuses-a-0400-file assertion: this process can write a \
             0400 file (running as root), so the refusal cannot be observed"
        );
    }
}

#[test]
fn an_ordinary_ferro_command_beside_a_held_redb_stays_on_redb_and_warns() {
    let env = Env::new();
    let repo = git_repo();
    populate_redb(&env, repo.path());
    let holder = fl_store::open_redb(&env.logical()).unwrap();
    // The CLI cannot migrate and cannot open the redb either while the holder
    // has it: it fails after its wait, and creates no .fls.
    env.ferro()
        .arg("--db")
        .arg(env.logical())
        .args(["project", "list"])
        .assert()
        .failure()
        .stderr(contains("stays on redb"))
        .stderr(contains("still holds the redb"));
    // (The empty migration lock file was made by the attempt.)
    assert_eq!(
        env.files(),
        vec!["fl.fls.migrate.lock".to_string(), "fl.redb".to_string()]
    );
    drop(holder);
    // Free again: the same command migrates.
    env.ferro()
        .arg("--db")
        .arg(env.logical())
        .args(["project", "list"])
        .assert()
        .success();
    assert_eq!(env.files(), with_lock());
}

// ---- scans do not migrate -------------------------------------------------

/// Every file in `dir` with its bytes, mtime and mode: equal before and after
/// means untouched.
fn dir_state(dir: &Path) -> Vec<(String, Vec<u8>, std::time::SystemTime, u32)> {
    let mut out: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| {
            let path = e.unwrap().path();
            let meta = std::fs::metadata(&path).unwrap();
            (
                path.file_name().unwrap().to_string_lossy().into_owned(),
                std::fs::read(&path).unwrap(),
                meta.modified().unwrap(),
                mode(&path),
            )
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

#[test]
fn a_read_only_lookup_does_not_migrate_another_configured_redb_store() {
    let env = Env::new();
    let repo = git_repo();
    let other_repo = git_repo();
    let other_stores = tempfile::tempdir().unwrap();
    let other = other_stores.path().join("other.redb");
    {
        let store = fl_store::open_redb(&other).unwrap();
        fl_core::Catalog::add_project(&store, "/work/other").unwrap();
    }
    // The bound store is a ferro store of its own; the other project's store
    // is a lone redb that is configured but not the one in use.
    populate_redb_ferro(&env, repo.path());
    let dir = env.home.path().join("config").join("fl");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("config.toml"),
        format!(
            "[[project]]\nroot = \"{}\"\nstore = \"{}\"\n\n[[project]]\nroot = \"{}\"\nstore = \"{}\"\n",
            repo.path().display(),
            env.logical().display(),
            other_repo.path().display(),
            other.display(),
        ),
    )
    .unwrap();
    let shown = env
        .ferro()
        .current_dir(repo.path())
        .args(["gate", "show", "1"])
        .output()
        .unwrap();
    let iri = json_of(&shown)["id"].as_str().unwrap().to_string();

    let before = dir_state(other_stores.path());
    assert_eq!(before.len(), 1, "{before:?}");
    // The lookup visits every configured store that exists, to find the owner.
    env.ferro()
        .current_dir(repo.path())
        .args(["gate", "show", &iri])
        .assert()
        .success()
        .stdout(contains(iri.as_str()));
    assert_eq!(
        dir_state(other_stores.path()),
        before,
        "the lookup changed the other store (bytes, mtime or mode), or made a file beside it"
    );
    assert_eq!(mode(&other), 0o600, "not made read-only");
    assert!(!other_stores.path().join("other.fls").exists());

    // The store that is USED does migrate: a command that works on `other`.
    env.ferro()
        .current_dir(other_repo.path())
        .args(["project", "list"])
        .assert()
        .success()
        .stdout(contains("/work/other"))
        .stderr(contains("migrated "))
        .stderr(contains("kept read-only"));
    assert!(other_stores.path().join("other.fls").exists());
    assert_eq!(mode(&other), 0o400);
}

/// A ferro store at the test's logical path holding a project and a gate.
fn populate_redb_ferro(env: &Env, repo: &Path) {
    env.ferro()
        .arg("--db")
        .arg(env.logical())
        .args(["project", "add", repo.to_str().unwrap()])
        .assert()
        .success();
    env.ferro()
        .arg("--db")
        .arg(env.logical())
        .args([
            "gate",
            "add",
            "--project",
            "1",
            "--name",
            "g",
            "--glob",
            "*.rs",
            "--program",
            "true",
        ])
        .assert()
        .success();
}

#[test]
fn migrate_json_reports_the_real_mode_of_the_kept_redb() {
    let env = Env::new();
    let repo = git_repo();
    populate_redb(&env, repo.path());
    let out = env
        .fl()
        .arg("--db")
        .arg(env.logical())
        .args(["store", "migrate", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let v = json_of(&out);
    assert_eq!(v["redb_kept_read_only"], true, "{v}");
    assert_eq!(mode(&env.logical()), 0o400);
}
