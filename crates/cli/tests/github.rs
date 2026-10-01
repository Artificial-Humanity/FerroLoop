//! The CLI with a project bound to GitHub Issues, against the in-process
//! fake. The `fl` binary reaches the fake through `FL_GITHUB_API_URL`.

use assert_cmd::Command;
use fl_github::fake::FakeGithub;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;
use std::fs;
use std::os::unix::fs::PermissionsExt;
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

struct G {
    home: tempfile::TempDir,
    repo: tempfile::TempDir,
    fake: FakeGithub,
}

/// A git working tree with `src/a.rs` and a `check.sh` that fails while a
/// file named `bug` exists.
fn git_tree() -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q"]);
    git(repo.path(), &["config", "user.email", "t@example.com"]);
    git(repo.path(), &["config", "user.name", "t"]);
    fs::create_dir_all(repo.path().join("src")).unwrap();
    fs::write(repo.path().join("src/a.rs"), "fn a() {}").unwrap();
    let check = repo.path().join("check.sh");
    fs::write(&check, "#!/bin/sh\n[ ! -e bug ]\n").unwrap();
    fs::set_permissions(&check, fs::Permissions::from_mode(0o755)).unwrap();
    git(repo.path(), &["add", "-A"]);
    git(repo.path(), &["commit", "-qm", "first"]);
    repo
}

/// A git repository whose `check.sh` fails while a file named `bug` exists,
/// bound to the fake's `acme/widgets` (or to nothing, when `bound` is false).
fn fixture_with(bound: bool) -> G {
    fixture_storing_at(bound, "fl.redb")
}

/// As [`fixture_with`], with the store at `store` under the fixture's home.
fn fixture_storing_at(bound: bool, store: &str) -> G {
    let home = tempfile::tempdir().unwrap();
    let repo = git_tree();
    let fake = FakeGithub::start("acme/widgets");
    let tracker = if bound {
        "tracker = { github = \"acme/widgets\", credential = \"env\" }\n"
    } else {
        ""
    };
    let cfg = format!(
        "[[project]]\nroot = \"{}\"\nstore = \"{}\"\n{tracker}",
        repo.path().canonicalize().unwrap().display(),
        home.path().join(store).display()
    );
    fs::create_dir_all(home.path().join("config/fl")).unwrap();
    fs::write(home.path().join("config/fl/config.toml"), cfg).unwrap();
    G { home, repo, fake }
}

fn fixture() -> G {
    fixture_with(true)
}

impl G {
    fn fl(&self) -> Command {
        let mut c = Command::cargo_bin("fl").unwrap();
        c.env("XDG_CONFIG_HOME", self.home.path().join("config"))
            .env("XDG_DATA_HOME", self.home.path().join("data"))
            .env("FL_GITHUB_TOKEN", "t")
            .env("FL_GITHUB_API_URL", self.fake.url())
            .env_remove("GITHUB_TOKEN")
            .env_remove("FL_DB")
            .current_dir(self.repo.path());
        c
    }

    fn project(&self) {
        self.fl().args(["project", "add", "."]).assert().success();
    }
}

#[test]
fn records_live_in_github_issues() {
    let g = fixture();
    g.project();
    g.fl()
        .args(["record", "add", "--project", "1", "--title", "fix it"])
        .assert()
        .success()
        .stdout(contains("1\tfix it"));
    assert_eq!(g.fake.issue(1).labels, vec!["fl:record", "fl:record/todo"]);
    g.fl()
        .args(["record", "list", "--project", "1"])
        .assert()
        .success()
        .stdout(contains("todo\tfix it"));
    g.fl()
        .args(["record", "move", "1", "--to", "doing"])
        .assert()
        .success();
    assert!(
        g.fake
            .issue(1)
            .labels
            .contains(&"fl:record/doing".to_string())
    );
}

#[test]
fn a_handle_may_carry_a_hash_and_an_issue_url_skips_the_local_stores() {
    let g = fixture();
    g.project();
    g.fl()
        .args(["record", "add", "--project", "1", "--title", "t"])
        .assert()
        .success();
    g.fl()
        .args(["record", "move", "#1", "--to", "doing"])
        .assert()
        .success();
    g.fl()
        .args([
            "record",
            "move",
            "https://github.com/acme/widgets/issues/1",
            "--to",
            "review",
        ])
        .assert()
        .success();
    assert!(
        g.fake
            .issue(1)
            .labels
            .contains(&"fl:record/review".to_string())
    );
}

#[test]
fn a_command_that_needs_no_tracker_never_contacts_github() {
    let g = fixture();
    g.project();
    g.fl()
        .args([
            "gate",
            "add",
            "--project",
            "1",
            "--name",
            "g",
            "--glob",
            "src/**/*.rs",
            "--program",
            "true",
        ])
        .assert()
        .success();
    g.fl()
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
            "low",
            "--gate",
            "1",
        ])
        .assert()
        .success();
    g.fl()
        .args(["gate", "list", "--project", "1"])
        .assert()
        .success();
    g.fl().args(["project", "list"]).assert().success();
    g.fl()
        .args(["check", "ship", "--project", "1"])
        .assert()
        .success();
    g.fl()
        .args(["manifest", "export", "--project", "1"])
        .assert()
        .success();
    git(g.repo.path(), &["add", ".fl"]);
    git(g.repo.path(), &["commit", "-qm", "manifest"]);
    g.fl()
        .args(["manifest", "check", "--project", "1"])
        .assert()
        .success();
    assert!(
        g.fake.state().requests.is_empty(),
        "{:?}",
        g.fake.state().requests
    );
}

#[test]
fn an_api_override_off_this_machine_is_refused() {
    let g = fixture();
    g.project();
    g.fl()
        // `.invalid` never resolves (RFC 2606), so a broken guard sends the
        // test token nowhere off this machine.
        .env("FL_GITHUB_API_URL", "http://github.invalid")
        .args(["record", "list", "--project", "1"])
        .assert()
        .failure()
        .stderr(contains("https://"));
    assert!(g.fake.state().requests.is_empty());
}

#[test]
fn db_cannot_be_combined_with_a_github_binding() {
    let g = fixture();
    g.project();
    g.fl()
        .arg("--db")
        .arg(g.home.path().join("other.redb"))
        .args(["record", "list", "--project", "1"])
        .assert()
        .failure()
        .stderr(contains("Drop --db"));
}

/// The refusal comes before any store is searched, created or opened: an
/// IRI is not refused as not owned first, and the `--db` path — and its
/// directory — is never created.
#[test]
fn db_with_a_github_binding_is_refused_before_any_store_is_touched() {
    let g = fixture();
    let dir = g.home.path().join("absent");
    let db = dir.join("other.redb");
    g.fl()
        .arg("--db")
        .arg(&db)
        .args([
            "record",
            "list",
            "--project",
            "urn:uuid:00000000-0000-7000-8000-000000000001",
        ])
        .assert()
        .failure()
        .stderr(contains("Drop --db"));
    assert!(!db.exists(), "the refused command created {}", db.display());
    assert!(
        !dir.exists(),
        "the refused command created {}",
        dir.display()
    );
    assert!(g.fake.state().requests.is_empty());
}

#[test]
fn a_renamed_repository_is_announced_on_stderr() {
    let g = fixture();
    g.project();
    g.fl()
        .args(["record", "add", "--project", "1", "--title", "t"])
        .assert()
        .success();
    g.fake.rename("acme/gadgets");
    g.fl()
        .args(["record", "list", "--project", "1"])
        .assert()
        .success()
        .stderr(contains("now `acme/gadgets`"));
}

#[test]
fn a_missing_credential_is_refused_naming_where_fl_looked() {
    let g = fixture();
    g.project();
    g.fl()
        .env_remove("FL_GITHUB_TOKEN")
        .args(["record", "list", "--project", "1"])
        .assert()
        .failure()
        .stderr(contains("FL_GITHUB_TOKEN or GITHUB_TOKEN"));
}

#[test]
fn an_unknown_tracker_key_is_refused_not_ignored() {
    let g = fixture();
    let path = g.home.path().join("config/fl/config.toml");
    let cfg = fs::read_to_string(&path).unwrap().replace(
        "credential = \"env\" }",
        "credential = \"env\", extra = 1 }",
    );
    assert!(cfg.contains("extra = 1"), "the edit must land");
    fs::write(&path, cfg).unwrap();
    g.fl()
        .args(["project", "list"])
        .assert()
        .failure()
        .stderr(contains("extra"));
}

#[test]
fn an_unbound_project_keeps_its_local_tracker() {
    let g = fixture_with(false);
    g.project();
    g.fl()
        .args(["record", "add", "--project", "1", "--title", "t"])
        .assert()
        .success();
    assert!(g.fake.state().requests.is_empty());
}

#[test]
fn an_api_override_with_a_user_name_before_the_host_is_refused() {
    let g = fixture();
    g.project();
    // Starts like loopback, but the host is after the `@`. Refused before
    // any request. The real host is a closed port on this machine (and
    // `.invalid` never resolves), so even a broken guard contacts nothing
    // off it; the guard's unit tests cover the same shapes with a public
    // host.
    for url in [
        "http://127.0.0.1:1@127.0.0.2:1",
        "http://localhost:x@127.0.0.2:1/",
    ] {
        g.fl()
            .env("FL_GITHUB_API_URL", url)
            .args(["record", "list", "--project", "1"])
            .assert()
            .failure()
            .stderr(contains("user name or password"));
    }
    g.fl()
        .env("FL_GITHUB_API_URL", "http://127.0.0.1.invalid")
        .args(["record", "list", "--project", "1"])
        .assert()
        .failure()
        .stderr(contains("is not this machine"));
    assert!(g.fake.state().requests.is_empty());
}

#[test]
fn an_api_override_to_localhost_is_accepted_and_announced() {
    let g = fixture();
    g.project();
    let url = g.fake.url().replace("127.0.0.1", "localhost");
    g.fl()
        .env("FL_GITHUB_API_URL", &url)
        .args(["record", "list", "--project", "1"])
        .assert()
        .success()
        .stderr(contains("talking to localhost, not GitHub"));
}

#[test]
fn an_app_credential_without_a_github_section_is_refused() {
    let g = fixture();
    let path = g.home.path().join("config/fl/config.toml");
    let cfg = fs::read_to_string(&path)
        .unwrap()
        .replace("credential = \"env\"", "credential = \"app\"");
    assert!(cfg.contains("credential = \"app\""), "the edit must land");
    fs::write(&path, cfg).unwrap();
    g.project();
    g.fl()
        .args(["record", "list", "--project", "1"])
        .assert()
        .failure()
        .stderr(contains("needs a `[github]` section"));
    assert!(g.fake.state().requests.is_empty());
}

/// The fixture's config with a second entry for the same root and store
/// but no tracker: the project's tracker is ambiguous.
fn make_ambiguous(g: &G) {
    let path = g.home.path().join("config/fl/config.toml");
    let cfg = fs::read_to_string(&path).unwrap();
    let local = cfg
        .lines()
        .filter(|l| !l.starts_with("tracker"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&path, format!("{cfg}{local}\n")).unwrap();
}

#[test]
fn db_still_escapes_an_ambiguous_config_for_a_command_that_needs_no_tracker() {
    let g = fixture();
    make_ambiguous(&g);
    let db = g.home.path().join("other.redb");
    g.fl()
        .arg("--db")
        .arg(&db)
        .args(["project", "add", "."])
        .assert()
        .success();
    g.fl()
        .arg("--db")
        .arg(&db)
        .args(["project", "list"])
        .assert()
        .success();
    g.fl()
        .env("FL_DB", &db)
        .args(["project", "list"])
        .assert()
        .success();
}

#[test]
fn a_tracker_command_with_an_ambiguous_config_is_refused_even_with_db() {
    let g = fixture();
    make_ambiguous(&g);
    let db = g.home.path().join("other.redb");
    g.fl()
        .arg("--db")
        .arg(&db)
        .args(["project", "add", "."])
        .assert()
        .success();
    g.fl()
        .arg("--db")
        .arg(&db)
        .args(["record", "list", "--project", "1"])
        .assert()
        .failure()
        .stderr(contains("more than one store or tracker"));
    assert!(g.fake.state().requests.is_empty());
}

#[test]
fn a_finding_walks_raise_reproduce_assign_verify_through_github_issues() {
    let g = fixture();
    g.project();
    fs::write(g.repo.path().join("bug"), "").unwrap();
    g.fl()
        .args([
            "gate",
            "add",
            "--project",
            "1",
            "--name",
            "no-bug",
            "--glob",
            "src/**/*.rs",
            "--program",
            "./check.sh",
        ])
        .assert()
        .success();
    g.fl()
        .args(["manifest", "export", "--project", "1"])
        .assert()
        .success();
    git(g.repo.path(), &["add", ".fl"]);
    git(g.repo.path(), &["commit", "-qm", "manifest"]);
    g.fl()
        .args(["record", "add", "--project", "1", "--title", "work"])
        .assert()
        .success();
    g.fl()
        .args([
            "finding",
            "raise",
            "--record",
            "1",
            "--claim",
            "a bug exists",
            "--by",
            "rev",
        ])
        .assert()
        .success()
        .stdout(contains("2\traised"));
    g.fl()
        .args(["finding", "reproduce", "2", "--gate", "1"])
        .assert()
        .success();
    g.fl()
        .args(["finding", "assign", "2", "--to", "fixer"])
        .assert()
        .success();
    fs::remove_file(g.repo.path().join("bug")).unwrap();
    g.fl()
        .args(["finding", "verify", "2"])
        .assert()
        .success()
        .stdout(contains("CLOSED"));
    let issue = g.fake.issue(2);
    assert_eq!(
        (issue.state.as_str(), issue.state_reason.as_deref()),
        ("closed", Some("completed"))
    );
    assert!(issue.labels.contains(&"fl:finding/fixed".to_string()));
}

#[test]
fn a_reproduction_is_refused_until_the_manifest_carries_the_gate() {
    let g = fixture();
    g.project();
    fs::write(g.repo.path().join("bug"), "").unwrap();
    g.fl()
        .args([
            "gate",
            "add",
            "--project",
            "1",
            "--name",
            "no-bug",
            "--glob",
            "src/**/*.rs",
            "--program",
            "./check.sh",
        ])
        .assert()
        .success();
    g.fl()
        .args(["record", "add", "--project", "1", "--title", "work"])
        .assert()
        .success();
    g.fl()
        .args([
            "finding", "raise", "--record", "1", "--claim", "c", "--by", "rev",
        ])
        .assert()
        .success();
    g.fl()
        .args(["finding", "reproduce", "2", "--gate", "1"])
        .assert()
        .failure()
        .stderr(contains("manifest"));
    assert!(
        g.fake
            .issue(2)
            .labels
            .contains(&"fl:finding/raised".to_string()),
        "unchanged"
    );
}

#[test]
fn a_security_finding_is_refused_on_a_public_repository() {
    let g = fixture();
    g.project();
    g.fl()
        .args(["record", "add", "--project", "1", "--title", "t"])
        .assert()
        .success();
    g.fake.state().repos[0].visibility = "public".into();
    g.fl()
        .args([
            "finding",
            "raise",
            "--record",
            "1",
            "--claim",
            "c",
            "--by",
            "rev",
            "--security",
        ])
        .assert()
        .failure()
        .stderr(contains("security finding"));
    assert_eq!(g.fake.issue_count(), 1);
}

#[test]
fn whoami_names_the_credential_and_the_repository() {
    let g = fixture();
    g.fl().args(["github", "whoami"]).assert().success().stdout(
        contains(fl_github::fake::USER_LOGIN)
            .and(contains("$FL_GITHUB_TOKEN"))
            .and(contains("acme/widgets")),
    );
}

#[test]
fn a_web_edit_is_diverged_until_repaired() {
    let g = fixture();
    g.project();
    g.fl()
        .args(["record", "add", "--project", "1", "--title", "t"])
        .assert()
        .success();
    g.fake.web_edit(1, |i| {
        i.labels = vec!["fl:record".into(), "fl:record/done".into()];
        i.state = "closed".into();
    });
    g.fl()
        .args(["record", "move", "1", "--to", "doing"])
        .assert()
        .failure()
        .stderr(contains("fl github repair"));
    g.fl()
        .args(["github", "repair", "1", "--by", "owner"])
        .assert()
        .success()
        .stdout(contains("repaired"));
    g.fl()
        .args(["record", "move", "1", "--to", "doing"])
        .assert()
        .success();
}

#[test]
fn fl_github_without_a_binding_is_refused_naming_the_config() {
    let g = fixture_with(false);
    g.fl()
        .args(["github", "whoami"])
        .assert()
        .failure()
        .stderr(contains("tracker"));
}

#[test]
fn fl_github_without_a_binding_is_refused_even_with_db() {
    let g = fixture_with(false);
    let db = g.home.path().join("new/other.redb");
    g.fl()
        .arg("--db")
        .arg(&db)
        .args(["github", "whoami"])
        .assert()
        .failure()
        .stderr(contains("needs a tracker binding"));
    g.fl()
        .env("FL_DB", &db)
        .args(["github", "repair", "1", "--by", "owner"])
        .assert()
        .failure()
        .stderr(contains("needs a tracker binding"));
    assert!(
        !g.home.path().join("new").exists(),
        "refused before any store's directory is created"
    );
    assert!(g.fake.state().requests.is_empty());
}

/// The full IRI of the project registered at `root`, read back through a
/// gate's JSON (a project's own listing prints its handle).
fn project_iri(g: &G, root: &Path) -> String {
    g.fl()
        .current_dir(root)
        .args([
            "gate",
            "add",
            "--project",
            "1",
            "--name",
            "iri-probe",
            "--glob",
            "src/**/*.rs",
            "--program",
            "true",
        ])
        .assert()
        .success();
    let shown = g
        .fl()
        .current_dir(root)
        .args(["gate", "show", "1"])
        .output()
        .unwrap();
    assert!(shown.status.success(), "{shown:?}");
    let json: serde_json::Value = serde_json::from_slice(&shown.stdout).unwrap();
    let iri = json["project"].as_str().unwrap().to_string();
    assert!(iri.starts_with("urn:uuid:"), "fixture: {iri}");
    iri
}

/// The records the store at `store` holds in its OWN (local) tracker for
/// `project`.
fn local_records(store: &Path, project: &str) -> usize {
    use fl_core::Tracker;
    let s = fl_store::RedbStore::open(store).unwrap();
    let p = fl_core::ProjectId(fl_core::Iri::parse(project).unwrap());
    s.list_records(&p).unwrap().len()
}

/// Outside a GitHub-bound project's root, an IRI
/// of that project sends the command to its store. Its records live in
/// GitHub, so the store's local tracker must never take the write — and
/// GitHub is not opened from a directory that is not the project's.
#[test]
fn an_iri_of_a_github_bound_project_named_from_outside_its_root_is_refused() {
    let g = fixture();
    g.project();
    let iri = project_iri(&g, g.repo.path());
    let outside = tempfile::tempdir().unwrap();
    for args in [
        vec!["record", "add", "--project", &iri, "--title", "t"],
        vec!["record", "list", "--project", &iri],
    ] {
        g.fl()
            .current_dir(outside.path())
            .args(&args)
            .assert()
            .failure()
            .stderr(
                contains("Run the command from the root of the project")
                    .and(contains("GitHub `acme/widgets`")),
            );
    }
    assert_eq!(local_records(&g.home.path().join("fl.redb"), &iri), 0);
    assert!(
        g.fake.state().requests.is_empty(),
        "{:?}",
        g.fake.state().requests
    );
}

/// Inside a GitHub-bound project's root, an IRI of
/// a LOCALLY bound project sends the command to that project's store. GitHub
/// must not be opened with it: the repository's node binding would be
/// written into the other store, and its records would become issues.
#[test]
fn an_iri_of_a_local_project_named_from_a_github_bound_root_is_refused() {
    let g = fixture();
    g.project();
    let q = git_tree();
    let q_store = g.home.path().join("q.redb");
    let path = g.home.path().join("config/fl/config.toml");
    let cfg = fs::read_to_string(&path).unwrap();
    fs::write(
        &path,
        format!(
            "{cfg}[[project]]\nroot = \"{}\"\nstore = \"{}\"\n",
            q.path().canonicalize().unwrap().display(),
            q_store.display()
        ),
    )
    .unwrap();
    g.fl()
        .current_dir(q.path())
        .args(["project", "add", "."])
        .assert()
        .success();
    let q_iri = project_iri(&g, q.path());
    for args in [
        vec!["record", "add", "--project", &q_iri, "--title", "t"],
        vec!["record", "list", "--project", &q_iri],
    ] {
        g.fl().args(&args).assert().failure().stderr(
            contains("Run the command from the root of the project")
                .and(contains("the store's own tracker")),
        );
    }
    assert!(
        g.fake.state().requests.is_empty(),
        "{:?}",
        g.fake.state().requests
    );
    assert_eq!(g.fake.issue_count(), 0);
    assert_eq!(local_records(&q_store, &q_iri), 0);
    // Q itself, from its own root, still uses its local tracker as before.
    g.fl()
        .current_dir(q.path())
        .args(["record", "add", "--project", "1", "--title", "local"])
        .assert()
        .success();
    assert_eq!(local_records(&q_store, &q_iri), 1);
    assert!(g.fake.state().requests.is_empty());
}

/// A GitHub-bound project whose store is the default
/// store. From a directory no entry covers, a command with no IRI uses the
/// default store — that project's — and must not write it locally.
#[test]
fn a_github_bound_default_store_is_refused_from_a_directory_no_entry_covers() {
    let g = fixture_storing_at(true, "data/fl/fl.redb");
    g.project();
    let iri = project_iri(&g, g.repo.path());
    let outside = tempfile::tempdir().unwrap();
    g.fl()
        .current_dir(outside.path())
        .args(["record", "add", "--project", "1", "--title", "t"])
        .assert()
        .failure()
        .stderr(contains(
            "the current directory belongs to no project in the config",
        ));
    assert_eq!(
        local_records(&g.home.path().join("data/fl/fl.redb"), &iri),
        0
    );
    assert!(g.fake.state().requests.is_empty());
    // From its root, the same command reaches GitHub.
    g.fl()
        .args(["record", "add", "--project", "1", "--title", "t"])
        .assert()
        .success();
    assert_eq!(g.fake.issue_count(), 1);
}

/// A gate over `src/**/*.rs`, a transition `launch` (review → done) over
/// it, the manifest committed, and one record.
fn gated_record(g: &G) {
    g.project();
    g.fl()
        .args([
            "gate",
            "add",
            "--project",
            "1",
            "--name",
            "no-bug",
            "--glob",
            "src/**/*.rs",
            "--program",
            "./check.sh",
        ])
        .assert()
        .success();
    g.fl()
        .args([
            "transition",
            "add",
            "--project",
            "1",
            "--name",
            "launch",
            "--from",
            "review",
            "--to",
            "done",
            "--regret",
            "low",
            "--gate",
            "1",
        ])
        .assert()
        .success();
    g.fl()
        .args(["record", "add", "--project", "1", "--title", "work"])
        .assert()
        .success();
}

/// The runs the local store holds for the project's only gate.
fn runs_of_the_only_gate(g: &G) -> Vec<fl_core::GateRun> {
    use fl_core::store::{Catalog, Ledger};
    let store = fl_store::RedbStore::open(&g.home.path().join("fl.redb")).unwrap();
    let p = store.list_projects().unwrap()[0].id.clone();
    let gate = store.list_gates(&p).unwrap()[0].id.clone();
    store.gate_runs(&gate).unwrap()
}

// GitHub ledger spec §3.2 step 3: an entry is filed by its own record
// field, so every run of one record names it by the issue's primary URL.
#[test]
fn check_with_a_record_in_github_mode_ties_its_runs_to_the_issue() {
    let g = fixture();
    gated_record(&g);
    g.fl()
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .success();
    let runs = runs_of_the_only_gate(&g);
    assert_eq!(runs.len(), 1);
    assert_eq!(
        runs[0]
            .record
            .as_ref()
            .map(|r| r.iri().as_str().to_string()),
        Some("https://github.com/acme/widgets/issues/1".to_string())
    );
}

#[test]
fn check_with_a_record_github_deleted_is_refused_before_any_gate_runs() {
    let g = fixture();
    gated_record(&g);
    g.fake.state().issues.get_mut(&1).unwrap().gone = true;
    g.fl()
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .code(2)
        .stderr(contains("was deleted"));
    assert!(runs_of_the_only_gate(&g).is_empty(), "no gate ran");
}
