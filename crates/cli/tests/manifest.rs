//! `fl manifest` and the currency guard, driven as a black box across two
//! "machines": two private config/data homes, one authoring clone and one
//! importing clone of the same repository.

use assert_cmd::Command;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;
use std::path::Path;
use std::process::Command as Sys;

fn git(dir: &Path, args: &[&str]) {
    let out = Sys::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn repo() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    git(d.path(), &["init", "-q"]);
    git(d.path(), &["config", "user.email", "t@example.com"]);
    git(d.path(), &["config", "user.name", "t"]);
    std::fs::write(d.path().join("a.rs"), "fn a() {}").unwrap();
    git(d.path(), &["add", "-A"]);
    git(d.path(), &["commit", "-qm", "first"]);
    d
}

/// One machine: private config and data homes.
struct Machine {
    config: tempfile::TempDir,
    data: tempfile::TempDir,
}

impl Machine {
    fn new() -> Self {
        Self {
            config: tempfile::tempdir().unwrap(),
            data: tempfile::tempdir().unwrap(),
        }
    }
    fn fl(&self, cwd: &Path) -> Command {
        let mut c = Command::cargo_bin("fl").unwrap();
        c.env("XDG_CONFIG_HOME", self.config.path())
            .env("XDG_DATA_HOME", self.data.path())
            .env_remove("FL_DB")
            .current_dir(cwd);
        c
    }
}

/// The authoring machine registers the repo, adds a gate and a transition,
/// exports, and commits the manifest.
fn authored() -> (Machine, tempfile::TempDir) {
    let m = Machine::new();
    let r = repo();
    m.fl(r.path())
        .args(["project", "add", "."])
        .assert()
        .success();
    m.fl(r.path())
        .args([
            "gate",
            "add",
            "--project",
            "1",
            "--name",
            "rs",
            "--glob",
            "*.rs",
            "--program",
            "true",
        ])
        .assert()
        .success();
    m.fl(r.path())
        .args([
            "transition",
            "add",
            "--project",
            "1",
            "--name",
            "ship",
            "--from",
            "review",
            "--to",
            "done",
            "--regret",
            "high",
            "--gate",
            "1",
        ])
        .assert()
        .success();
    m.fl(r.path())
        .args(["manifest", "export", "--project", "1"])
        .assert()
        .success()
        .stdout(contains("\trs\ttrue"));
    git(r.path(), &["add", "-A"]);
    git(r.path(), &["commit", "-qm", "manifest"]);
    (m, r)
}

fn clone_of(src: &Path) -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    git(d.path(), &["clone", "-q", src.to_str().unwrap(), "."]);
    d
}

#[test]
fn a_clean_committed_manifest_checks_as_current() {
    let (m, r) = authored();
    m.fl(r.path())
        .args(["manifest", "check", "--project", "1"])
        .assert()
        .success()
        .stdout(contains("current"));
}

#[test]
fn an_uncommitted_manifest_is_refused_by_check() {
    let m = Machine::new();
    let r = repo();
    m.fl(r.path())
        .args(["project", "add", "."])
        .assert()
        .success();
    m.fl(r.path())
        .args([
            "gate",
            "add",
            "--project",
            "1",
            "--name",
            "rs",
            "--glob",
            "*.rs",
            "--program",
            "true",
        ])
        .assert()
        .success();
    m.fl(r.path())
        .args(["manifest", "export", "--project", "1"])
        .assert()
        .success();
    m.fl(r.path())
        .args(["manifest", "check", "--project", "1"])
        .assert()
        .failure()
        .stderr(contains("commit"));
}

#[test]
fn a_gate_changed_since_export_is_refused_by_check() {
    let (m, r) = authored();
    std::fs::write(r.path().join("b.rs"), "fn b() {}").unwrap();
    git(r.path(), &["add", "-A"]);
    git(r.path(), &["commit", "-qm", "more"]);
    m.fl(r.path())
        .args(["gate", "affirm", "1"])
        .assert()
        .success();
    m.fl(r.path())
        .args(["manifest", "check", "--project", "1"])
        .assert()
        .failure()
        .stderr(contains("fl manifest export"));
}

#[test]
fn another_machine_imports_and_runs_the_gate() {
    let (_author, r) = authored();
    let other = Machine::new();
    let c = clone_of(r.path());
    other
        .fl(c.path())
        .args(["manifest", "import"])
        .assert()
        .success()
        .stdout(contains("1 added"))
        // This store numbers handles on its own, so the person needs to be
        // told which handle each imported gate landed on, by name.
        .stdout(contains("gate\t1\trs"));
    other
        .fl(c.path())
        .args(["gate", "run", "1"])
        .assert()
        .success()
        .stdout(contains("PASS"));
}

#[test]
fn export_prints_a_population_commands_program() {
    let m = Machine::new();
    let r = repo();
    m.fl(r.path())
        .args(["project", "add", "."])
        .assert()
        .success();
    m.fl(r.path())
        .args([
            "gate",
            "add",
            "--project",
            "1",
            "--name",
            "files",
            "--population-from",
            "list-files-for-export-test",
            "--program",
            "true",
        ])
        .assert()
        .success();
    m.fl(r.path())
        .args(["manifest", "export", "--project", "1"])
        .assert()
        .success()
        .stdout(contains("list-files-for-export-test"));
}

#[test]
fn a_gitignored_manifest_is_refused_by_check_naming_gitignore() {
    let m = Machine::new();
    let r = repo();
    m.fl(r.path())
        .args(["project", "add", "."])
        .assert()
        .success();
    m.fl(r.path())
        .args([
            "gate",
            "add",
            "--project",
            "1",
            "--name",
            "rs",
            "--glob",
            "*.rs",
            "--program",
            "true",
        ])
        .assert()
        .success();
    m.fl(r.path())
        .args(["manifest", "export", "--project", "1"])
        .assert()
        .success();
    std::fs::write(r.path().join(".gitignore"), ".fl/manifest.json\n").unwrap();
    git(r.path(), &["add", "-A"]);
    git(r.path(), &["commit", "-qm", "ignore the manifest"]);
    m.fl(r.path())
        .args(["manifest", "check", "--project", "1"])
        .assert()
        .failure()
        .stderr(contains(".gitignore"));
}

#[test]
fn an_imported_gate_cannot_be_affirmed_locally() {
    let (_author, r) = authored();
    let other = Machine::new();
    let c = clone_of(r.path());
    other
        .fl(c.path())
        .args(["manifest", "import"])
        .assert()
        .success();
    other
        .fl(c.path())
        .args(["gate", "affirm", "1"])
        .assert()
        .failure()
        .stderr(contains("imported from a manifest"));
}

#[test]
fn a_manifest_edited_after_import_stops_the_gate_from_running() {
    let (_author, r) = authored();
    let other = Machine::new();
    let c = clone_of(r.path());
    other
        .fl(c.path())
        .args(["manifest", "import"])
        .assert()
        .success();
    let path = c.path().join(".fl/manifest.json");
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, text.replace("\"name\": \"rs\"", "\"name\": \"rs2\"")).unwrap();
    other
        .fl(c.path())
        .args(["gate", "run", "1"])
        .assert()
        .failure()
        .stderr(contains("edited by hand"));
}

#[test]
fn a_manifest_that_moved_on_since_import_stops_the_gate_until_reimport() {
    let (author, r) = authored();
    let other = Machine::new();
    let c = clone_of(r.path());
    other
        .fl(c.path())
        .args(["manifest", "import"])
        .assert()
        .success();

    std::fs::write(r.path().join("b.rs"), "fn b() {}").unwrap();
    git(r.path(), &["add", "-A"]);
    git(r.path(), &["commit", "-qm", "more"]);
    author
        .fl(r.path())
        .args(["gate", "affirm", "1"])
        .assert()
        .success();
    author
        .fl(r.path())
        .args(["manifest", "export", "--project", "1"])
        .assert()
        .success();
    git(r.path(), &["add", "-A"]);
    git(r.path(), &["commit", "-qm", "re-export"]);
    git(c.path(), &["pull", "-q"]);

    other
        .fl(c.path())
        .args(["check", "ship", "--project", "1"])
        .assert()
        .failure()
        .stderr(contains("fl manifest import"));
    other
        .fl(c.path())
        .args(["manifest", "import"])
        .assert()
        .success()
        .stdout(contains("1 changed"));
    other
        .fl(c.path())
        .args(["gate", "run", "1"])
        .assert()
        .success();
}

/// An importing machine whose working-tree manifest moved on since import.
fn stale_import() -> (Machine, tempfile::TempDir, Machine, tempfile::TempDir) {
    let (author, r) = authored();
    let other = Machine::new();
    let c = clone_of(r.path());
    other
        .fl(c.path())
        .args(["manifest", "import"])
        .assert()
        .success();
    std::fs::write(r.path().join("b.rs"), "fn b() {}").unwrap();
    git(r.path(), &["add", "-A"]);
    git(r.path(), &["commit", "-qm", "more"]);
    author
        .fl(r.path())
        .args(["gate", "affirm", "1"])
        .assert()
        .success();
    author
        .fl(r.path())
        .args(["manifest", "export", "--project", "1"])
        .assert()
        .success();
    git(r.path(), &["add", "-A"]);
    git(r.path(), &["commit", "-qm", "re-export"]);
    git(c.path(), &["pull", "-q"]);
    (author, r, other, c)
}

#[test]
fn a_gated_move_is_refused_on_a_stale_import_and_an_ungated_one_is_not() {
    let (_a, _r, other, c) = stale_import();
    other
        .fl(c.path())
        .args(["record", "add", "--project", "1", "--title", "t"])
        .assert()
        .success();
    // todo → review: no transition covers it, so no gate runs.
    other
        .fl(c.path())
        .args(["record", "move", "1", "--to", "review"])
        .assert()
        .success();
    // review → done: `ship` covers it.
    other
        .fl(c.path())
        .args(["record", "move", "1", "--to", "done"])
        .assert()
        .failure()
        .stderr(contains("fl manifest import"));
}

#[test]
fn reproduce_and_verify_are_refused_on_a_stale_import() {
    let (_a, _r, other, c) = stale_import();
    other
        .fl(c.path())
        .args(["record", "add", "--project", "1", "--title", "t"])
        .assert()
        .success();
    other
        .fl(c.path())
        .args([
            "finding", "raise", "--record", "1", "--claim", "c", "--by", "r",
        ])
        .assert()
        .success();
    other
        .fl(c.path())
        .args(["finding", "reproduce", "1", "--gate", "1"])
        .assert()
        .failure()
        .stderr(contains("fl manifest import"));
    other
        .fl(c.path())
        .args(["finding", "verify", "1"])
        .assert()
        .failure()
        .stderr(contains("fl manifest import"));
}

#[test]
fn check_refuses_a_gate_or_transition_added_since_export() {
    let (m, r) = authored();
    m.fl(r.path())
        .args([
            "gate",
            "add",
            "--project",
            "1",
            "--name",
            "late",
            "--glob",
            "*.rs",
            "--program",
            "true",
        ])
        .assert()
        .success();
    m.fl(r.path())
        .args(["manifest", "check", "--project", "1"])
        .assert()
        .failure()
        .stderr(contains("not in the manifest"));

    let (m, r) = authored();
    m.fl(r.path())
        .args([
            "transition",
            "add",
            "--project",
            "1",
            "--name",
            "late",
            "--from",
            "todo",
            "--to",
            "doing",
            "--regret",
            "low",
            "--gate",
            "1",
        ])
        .assert()
        .success();
    m.fl(r.path())
        .args(["manifest", "check", "--project", "1"])
        .assert()
        .failure()
        .stderr(contains("transitions changed"));
}

#[test]
fn a_reimport_from_another_checkout_says_the_root_moved() {
    let (_author, r) = authored();
    let other = Machine::new();
    let c1 = clone_of(r.path());
    let c2 = clone_of(r.path());
    other
        .fl(c1.path())
        .args(["manifest", "import"])
        .assert()
        .success();
    other
        .fl(c2.path())
        .args(["manifest", "import"])
        .assert()
        .success()
        .stdout(contains("moved"));
}

#[test]
fn a_missing_manifest_on_an_importing_machine_is_refused_by_path() {
    let (_author, r) = authored();
    let other = Machine::new();
    let c = clone_of(r.path());
    other
        .fl(c.path())
        .args(["manifest", "import"])
        .assert()
        .success();
    std::fs::remove_file(c.path().join(".fl/manifest.json")).unwrap();
    other
        .fl(c.path())
        .args(["gate", "run", "1"])
        .assert()
        .failure()
        .stderr(contains(".fl/manifest.json"));
}

/// Put a ledger root for `node` into the store at `db`, bound to `repo`, as
/// `fl github ledger init` will (plan B).
fn with_ledger_root(db: &Path, repo: &str, node: &str, commit: &str) {
    use fl_core::store::Bindings;
    let s = fl_store::RedbStore::open(db).unwrap();
    s.bind_node_id(repo, node).unwrap();
    s.set_ledger_root(node, commit).unwrap();
}

// Spec §6.1 step 4: every export writes `ledger_root` from the store, and an
// import records it — how every machine gets its anchor.
#[test]
fn a_bound_projects_export_carries_its_ledger_root_and_an_import_records_it() {
    let m = Machine::new();
    let r = repo();
    let db = m.data.path().join("bound.redb");
    let root = r.path().canonicalize().unwrap();
    std::fs::create_dir_all(m.config.path().join("fl")).unwrap();
    std::fs::write(
        m.config.path().join("fl/config.toml"),
        format!(
            "[[project]]\nroot = \"{}\"\nstore = \"{}\"\ntracker = {{ github = \"acme/widgets\", credential = \"env\" }}\n",
            root.display(),
            db.display()
        ),
    )
    .unwrap();
    m.fl(r.path())
        .args(["project", "add", "."])
        .assert()
        .success();
    with_ledger_root(&db, "acme/widgets", "R_1", "abc123");

    m.fl(r.path())
        .args(["manifest", "export", "--project", "1"])
        .assert()
        .success()
        .stdout(contains("ledger_root\tabc123"));
    let text = std::fs::read_to_string(r.path().join(".fl/manifest.json")).unwrap();
    assert!(
        text.contains("\"format_version\": 2") && text.contains("\"repository_node_id\": \"R_1\""),
        "{text}"
    );
    git(r.path(), &["add", "-A"]);
    git(r.path(), &["commit", "-qm", "manifest"]);

    let other = Machine::new();
    let c = clone_of(r.path());
    other
        .fl(c.path())
        .args(["manifest", "import"])
        .assert()
        .success();
    use fl_core::store::Bindings;
    let imported = fl_store::RedbStore::open(&other.data.path().join("fl/fl.redb")).unwrap();
    assert_eq!(
        imported.ledger_root("R_1").unwrap().as_deref(),
        Some("abc123")
    );
}

// Spec §6.1 step 4: with --db the config entry is not read, so the export cannot
// know which repository's root belongs in it — and writing none would drop
// every machine's anchor.
#[test]
fn an_export_under_db_from_a_store_with_a_ledger_root_is_refused_and_writes_nothing() {
    let m = Machine::new();
    let r = repo();
    let db = m.data.path().join("s.redb");
    let dbs = db.display().to_string();
    m.fl(r.path())
        .args(["--db", &dbs, "project", "add", "."])
        .assert()
        .success();
    with_ledger_root(&db, "acme/widgets", "R_1", "abc123");

    m.fl(r.path())
        .args(["--db", &dbs, "manifest", "export", "--project", "1"])
        .assert()
        .code(2)
        .stderr(contains("records a GitHub ledger root").and(contains("without --db")));
    assert!(
        !r.path().join(".fl/manifest.json").exists(),
        "nothing was written"
    );
}

// Spec §6.1 step 4: a store that holds a root but no node for the name the
// config binds cannot tell which root is this project's either.
#[test]
fn an_export_whose_configured_repository_has_no_node_in_a_store_with_a_root_is_refused() {
    let m = Machine::new();
    let r = repo();
    let db = m.data.path().join("bound.redb");
    let root = r.path().canonicalize().unwrap();
    std::fs::create_dir_all(m.config.path().join("fl")).unwrap();
    std::fs::write(
        m.config.path().join("fl/config.toml"),
        format!(
            "[[project]]\nroot = \"{}\"\nstore = \"{}\"\ntracker = {{ github = \"acme/widgets\", credential = \"env\" }}\n",
            root.display(),
            db.display()
        ),
    )
    .unwrap();
    m.fl(r.path())
        .args(["project", "add", "."])
        .assert()
        .success();
    // A root for a repository the store knows under another name only.
    with_ledger_root(&db, "acme/gadgets", "R_1", "abc123");

    m.fl(r.path())
        .args(["manifest", "export", "--project", "1"])
        .assert()
        .code(2)
        .stderr(
            contains("no repository node for `acme/widgets`").and(contains("fl github whoami")),
        );
    assert!(
        !r.path().join(".fl/manifest.json").exists(),
        "nothing was written"
    );
}

// Spec §6.1 step 4: "for a store another IRI selected" the export cannot
// know the repository either — even when that store happens to hold a node
// under the current directory's configured name.
#[test]
fn an_export_of_a_project_in_another_projects_store_is_refused_when_that_store_holds_a_root() {
    use fl_core::store::Catalog;
    let m = Machine::new();
    let bound_repo = repo();
    let other_repo = repo();
    let a = m.data.path().join("a.redb");
    let b = m.data.path().join("b.redb");
    std::fs::create_dir_all(m.config.path().join("fl")).unwrap();
    std::fs::write(
        m.config.path().join("fl/config.toml"),
        format!(
            "[[project]]\nroot = \"{}\"\nstore = \"{}\"\ntracker = {{ github = \"acme/widgets\", credential = \"env\" }}\n\n[[project]]\nroot = \"{}\"\nstore = \"{}\"\n",
            bound_repo.path().canonicalize().unwrap().display(),
            a.display(),
            other_repo.path().canonicalize().unwrap().display(),
            b.display()
        ),
    )
    .unwrap();
    m.fl(other_repo.path())
        .args(["project", "add", "."])
        .assert()
        .success();
    let other_project = {
        let s = fl_store::RedbStore::open(&b).unwrap();
        s.list_projects().unwrap()[0].id.iri().to_string()
    };
    with_ledger_root(&b, "acme/widgets", "R_9", "abc123");

    m.fl(bound_repo.path())
        .args(["manifest", "export", "--project", &other_project])
        .assert()
        .code(2)
        .stderr(contains("records a GitHub ledger root"));
    assert!(
        !other_repo.path().join(".fl/manifest.json").exists(),
        "nothing was written"
    );
}

// Whole-branch review, finding 4: `manifest` never asks for a tracker
// (unlike `record`, `finding`, `attempt`, `check --record` or `fl github`),
// so the one-store-one-tracker check those commands run never ran for it
// either — `manifest export` trusted the current directory's config entry
// outright. With an inconsistent config, two entries sharing one store but
// naming different trackers, that could write one repository's ledger root
// into another project's manifest. Once the store holds a root, export must
// refuse exactly as a tracker command would.
#[test]
fn an_export_from_a_store_bound_to_two_trackers_in_the_config_is_refused() {
    let m = Machine::new();
    let r = repo();
    let other_repo = repo();
    let db = m.data.path().join("shared.redb");
    std::fs::create_dir_all(m.config.path().join("fl")).unwrap();
    std::fs::write(
        m.config.path().join("fl/config.toml"),
        format!(
            "[[project]]\nroot = \"{}\"\nstore = \"{}\"\ntracker = {{ github = \"acme/widgets\", credential = \"env\" }}\n\n[[project]]\nroot = \"{}\"\nstore = \"{}\"\ntracker = {{ github = \"acme/gadgets\", credential = \"env\" }}\n",
            r.path().canonicalize().unwrap().display(),
            db.display(),
            other_repo.path().canonicalize().unwrap().display(),
            db.display(),
        ),
    )
    .unwrap();
    m.fl(r.path())
        .args(["project", "add", "."])
        .assert()
        .success();
    with_ledger_root(&db, "acme/widgets", "R_1", "abc123");

    m.fl(r.path())
        .args(["manifest", "export", "--project", "1"])
        .assert()
        .code(2)
        .stderr(contains("bound to more than one tracker"));
    assert!(
        !r.path().join(".fl/manifest.json").exists(),
        "nothing was written"
    );
}

// The same inconsistency, but the store holds no ledger root yet: nothing
// an export writes can be attributed to the wrong repository, so the
// ambiguity check must not fire here — only a tracker command would need
// to know which tracker is right, and `manifest` does not ask for one.
#[test]
fn an_export_from_a_store_bound_to_two_trackers_but_with_no_ledger_root_still_exports() {
    let m = Machine::new();
    let r = repo();
    let other_repo = repo();
    let db = m.data.path().join("shared.redb");
    std::fs::create_dir_all(m.config.path().join("fl")).unwrap();
    std::fs::write(
        m.config.path().join("fl/config.toml"),
        format!(
            "[[project]]\nroot = \"{}\"\nstore = \"{}\"\ntracker = {{ github = \"acme/widgets\", credential = \"env\" }}\n\n[[project]]\nroot = \"{}\"\nstore = \"{}\"\ntracker = {{ github = \"acme/gadgets\", credential = \"env\" }}\n",
            r.path().canonicalize().unwrap().display(),
            db.display(),
            other_repo.path().canonicalize().unwrap().display(),
            db.display(),
        ),
    )
    .unwrap();
    m.fl(r.path())
        .args(["project", "add", "."])
        .assert()
        .success();

    m.fl(r.path())
        .args(["manifest", "export", "--project", "1"])
        .assert()
        .success();
    assert!(r.path().join(".fl/manifest.json").exists());
}
