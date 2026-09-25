use assert_cmd::Command;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;
use std::path::{Path, PathBuf};
use std::process::Command as Sys;

fn git(dir: &Path, args: &[&str]) {
    assert!(
        Sys::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap()
            .status
            .success(),
        "git {args:?} failed"
    );
}

fn git_repo() -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q"]);
    git(repo.path(), &["config", "user.email", "t@example.com"]);
    git(repo.path(), &["config", "user.name", "t"]);
    std::fs::write(repo.path().join("a.rs"), "fn a() {}").unwrap();
    git(repo.path(), &["add", "-A"]);
    git(repo.path(), &["commit", "-qm", "first"]);
    repo
}

/// Private config and data homes, so no test reads the developer's own.
struct Env {
    config: tempfile::TempDir,
    data: tempfile::TempDir,
}

impl Env {
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
    fn default_store(&self) -> PathBuf {
        self.data.path().join("fl").join("fl.redb")
    }
    fn write_config(&self, body: &str) {
        let dir = self.config.path().join("fl");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), body).unwrap();
    }
}

fn bind(repo: &Path, store: &Path) -> String {
    format!(
        "[[project]]\nroot = \"{}\"\nstore = \"{}\"\n",
        repo.display(),
        store.display()
    )
}

const STRANGER: &str = "urn:uuid:0190a1b2-c3d4-7e5f-8a6b-7c8d9e0f1a2b";

// Review Focus 4.
#[test]
fn a_project_bound_in_config_uses_its_store_from_a_subdirectory() {
    let env = Env::new();
    let repo = git_repo();
    let stores = tempfile::tempdir().unwrap();
    let a = stores.path().join("a.redb");
    env.write_config(&bind(repo.path(), &a));
    let sub = repo.path().join("sub");
    std::fs::create_dir(&sub).unwrap();
    env.fl(&sub)
        .args(["project", "add", repo.path().to_str().unwrap()])
        .assert()
        .success();
    assert!(a.exists(), "the bound store was not used");
    assert!(
        !env.default_store().exists(),
        "the default store was created anyway"
    );
}

// Review Focus 4.
#[cfg(unix)]
#[test]
fn a_symlinked_working_directory_binds_like_the_real_one() {
    let env = Env::new();
    let repo = git_repo();
    let stores = tempfile::tempdir().unwrap();
    let a = stores.path().join("a.redb");
    env.write_config(&bind(repo.path(), &a));
    let links = tempfile::tempdir().unwrap();
    let link = links.path().join("via-link");
    std::os::unix::fs::symlink(repo.path(), &link).unwrap();
    env.fl(&link).args(["project", "list"]).assert().success();
    assert!(a.exists(), "a symlinked cwd did not bind");
    assert!(!env.default_store().exists());
}

#[test]
fn precedence_is_db_then_env_then_config_then_default() {
    let env = Env::new();
    let repo = git_repo();
    let stores = tempfile::tempdir().unwrap();
    let flag = stores.path().join("flag.redb");
    let var = stores.path().join("env.redb");
    let cfg = stores.path().join("cfg.redb");
    env.write_config(&bind(repo.path(), &cfg));

    env.fl(repo.path())
        .env("FL_DB", &var)
        .args(["--db", flag.to_str().unwrap(), "project", "list"])
        .assert()
        .success();
    assert!(
        flag.exists() && !var.exists() && !cfg.exists(),
        "--db did not win"
    );

    env.fl(repo.path())
        .env("FL_DB", &var)
        .args(["project", "list"])
        .assert()
        .success();
    assert!(
        var.exists() && !cfg.exists(),
        "$FL_DB did not beat the config"
    );

    env.fl(repo.path())
        .args(["project", "list"])
        .assert()
        .success();
    assert!(
        cfg.exists() && !env.default_store().exists(),
        "the config did not beat the default"
    );

    let elsewhere = tempfile::tempdir().unwrap();
    env.fl(elsewhere.path())
        .args(["project", "list"])
        .assert()
        .success();
    assert!(
        env.default_store().exists(),
        "an unbound directory did not fall to the default"
    );
}

// Review Focus 3.
#[test]
fn a_malformed_config_is_an_error_naming_the_file() {
    let env = Env::new();
    let repo = git_repo();
    env.write_config("[[project]\nroot =");
    env.fl(repo.path())
        .args(["project", "list"])
        .assert()
        .code(2)
        .stderr(contains("config.toml"));
    assert!(
        !env.default_store().exists(),
        "a broken config fell through to the default store"
    );
}

// Review Focus 3.
#[test]
fn a_relative_store_path_in_config_is_refused() {
    let env = Env::new();
    let repo = git_repo();
    env.write_config(&format!(
        "[[project]]\nroot = \"{}\"\nstore = \"stores/a.redb\"\n",
        repo.path().display()
    ));
    env.fl(repo.path())
        .args(["project", "list"])
        .assert()
        .code(2)
        .stderr(contains("absolute"));
}

/// Two repos, each bound to its own store, each registered in it.
fn two_bound_stores(
    env: &Env,
) -> (
    tempfile::TempDir,
    tempfile::TempDir,
    tempfile::TempDir,
    PathBuf,
    PathBuf,
) {
    let (ra, rb) = (git_repo(), git_repo());
    let stores = tempfile::tempdir().unwrap();
    let (a, b) = (stores.path().join("a.redb"), stores.path().join("b.redb"));
    env.write_config(&format!("{}{}", bind(ra.path(), &a), bind(rb.path(), &b)));
    env.fl(ra.path())
        .args(["project", "add", ra.path().to_str().unwrap()])
        .assert()
        .success();
    env.fl(rb.path())
        .args(["project", "add", rb.path().to_str().unwrap()])
        .assert()
        .success();
    (ra, rb, stores, a, b)
}

#[test]
fn an_iri_selects_the_store_that_holds_it() {
    let env = Env::new();
    let (ra, rb, _stores, _a, _b) = two_bound_stores(&env);
    env.fl(ra.path())
        .args([
            "gate",
            "add",
            "--project",
            "1",
            "--name",
            "in-a",
            "--glob",
            "*.rs",
            "--program",
            "true",
        ])
        .assert()
        .success();
    let shown = env
        .fl(ra.path())
        .args(["gate", "show", "1"])
        .output()
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&shown.stdout).unwrap();
    let iri = json["id"].as_str().unwrap().to_string();

    env.fl(rb.path())
        .args(["gate", "show", &iri])
        .assert()
        .success()
        .stdout(contains("in-a"));
}

// Spec §6.4a.
#[test]
fn an_iri_no_store_holds_is_not_owned_and_names_every_store_searched() {
    let env = Env::new();
    let (_ra, rb, _stores, a, b) = two_bound_stores(&env);
    env.fl(rb.path())
        .args(["gate", "show", STRANGER])
        .assert()
        .code(2)
        .stderr(contains(a.to_str().unwrap()))
        .stderr(contains(b.to_str().unwrap()));
}

#[test]
fn a_store_that_cannot_be_opened_during_an_iri_search_is_an_error_not_a_skip() {
    let env = Env::new();
    let (ra, rb) = (git_repo(), git_repo());
    let stores = tempfile::tempdir().unwrap();
    let (a, b) = (stores.path().join("a.redb"), stores.path().join("b.redb"));
    std::fs::write(&b, b"this is not a database").unwrap();
    env.write_config(&format!("{}{}", bind(ra.path(), &a), bind(rb.path(), &b)));
    env.fl(ra.path())
        .args(["project", "add", ra.path().to_str().unwrap()])
        .assert()
        .success();
    env.fl(ra.path())
        .args(["gate", "show", STRANGER])
        .assert()
        .code(2)
        .stderr(contains("could not open"))
        .stderr(contains(b.to_str().unwrap()));
}

// Fix round 1 — Important 2 (integration half): guard `bound`'s
// canonicalization of a config entry's `root` (config.rs's unit test
// `bound_resolves_a_symlinked_cwd_passed_directly` guards the `cwd` side;
// Task 6's `a_symlinked_working_directory_binds_like_the_real_one` above
// cannot guard either, since `std::env::current_dir()` already resolves a
// symlinked process cwd before this binary ever sees it).
#[cfg(unix)]
#[test]
fn a_symlinked_config_root_binds_like_the_real_one() {
    let env = Env::new();
    let repo = git_repo();
    let stores = tempfile::tempdir().unwrap();
    let a = stores.path().join("a.redb");
    let links = tempfile::tempdir().unwrap();
    let link = links.path().join("via-link");
    std::os::unix::fs::symlink(repo.path(), &link).unwrap();
    env.write_config(&bind(&link, &a));
    env.fl(repo.path())
        .args(["project", "list"])
        .assert()
        .success();
    assert!(a.exists(), "a symlinked config root did not bind");
    assert!(!env.default_store().exists());
}

/// A gate in `rb`'s store, and its full IRI, via a JSON dump so the id
/// comes back literal rather than through `refs::show`'s handle
/// preference.
fn a_gate_iri_in(env: &Env, repo: &Path) -> String {
    env.fl(repo)
        .args([
            "gate",
            "add",
            "--project",
            "1",
            "--name",
            "in-b",
            "--glob",
            "*.rs",
            "--program",
            "true",
        ])
        .assert()
        .success();
    let shown = env.fl(repo).args(["gate", "show", "1"]).output().unwrap();
    let json: serde_json::Value = serde_json::from_slice(&shown.stdout).unwrap();
    json["id"].as_str().unwrap().to_string()
}

// Fix round 1 — Ruling (item 0), extended in fix round 2 (items 2, 3):
// `--db`/`$FL_DB` CONFINE the command to the one store they name. An IRI
// that store does not hold is `NotOwned` naming only that store — never a
// search across every store any project happens to be bound to in the
// config — and looking it up must never CREATE that store file either.
#[test]
fn an_explicit_db_confines_the_search_and_never_names_another_configured_store() {
    let env = Env::new();
    let (ra, rb, stores, _a, b) = two_bound_stores(&env);
    let iri = a_gate_iri_in(&env, rb.path());

    // `other.redb` is a THIRD path, not registered in the config at all —
    // confinement must hold whether or not it happens to overlap with a
    // configured store, and it must not itself get created merely by being
    // looked in (Fix round 2, item 2): `RedbStore::open` calls
    // `Database::create`, so opening a nonexistent confined store to check
    // whether it owns an id would leave a fresh, empty store file behind —
    // exactly the "created a store while searching" bug item 3 already
    // refuses for the config-binding tier, reappearing here for `--db`.
    let other = stores.path().join("other.redb");
    env.fl(ra.path())
        .args(["--db", other.to_str().unwrap(), "gate", "show", &iri])
        .assert()
        .code(2)
        .stderr(contains("no store holds"))
        .stderr(contains(other.to_str().unwrap()))
        .stderr(contains(b.to_str().unwrap()).not());
    assert!(
        !other.exists(),
        "looking up an IRI created the confined --db store"
    );
}

// Fix round 2, item 3: the $FL_DB confinement tier had no test of its own —
// flipping `db_path`'s `$FL_DB` branch from `confined = true` to `false`
// failed nothing. Mirrors the `--db` test above through `$FL_DB` instead,
// including item 2's never-creates-the-store assertion.
#[test]
fn fl_db_confines_the_search_and_never_names_another_configured_store() {
    let env = Env::new();
    let (ra, rb, stores, _a, b) = two_bound_stores(&env);
    let iri = a_gate_iri_in(&env, rb.path());

    let other = stores.path().join("other-via-fl-db.redb");
    env.fl(ra.path())
        .env("FL_DB", &other)
        .args(["gate", "show", &iri])
        .assert()
        .code(2)
        .stderr(contains("no store holds"))
        .stderr(contains(other.to_str().unwrap()))
        .stderr(contains(b.to_str().unwrap()).not());
    assert!(
        !other.exists(),
        "looking up an IRI created the confined $FL_DB store"
    );
}

// Fix round 1 — Important 1: a handle resolves only in the store it was
// read from. Mixing one into a command whose IRI sends the search to a
// DIFFERENT store than the bound one must refuse rather than silently
// resolve the handle against that other store's numbering.
#[test]
fn a_handle_mixed_with_an_iri_held_by_a_different_store_is_refused() {
    let env = Env::new();
    let (ra, rb, _stores, _a, _b) = two_bound_stores(&env);
    let gate_iri = a_gate_iri_in(&env, rb.path());

    // From A (bound to a.redb): project "1" BY HANDLE — A's own project —
    // alongside a gate named BY IRI that only b.redb holds. The IRI search
    // sends this command to b.redb, which differs from A's bound store; the
    // handle "1" must not be silently resolved against b.redb's numbering.
    env.fl(ra.path())
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
            &gate_iri,
        ])
        .assert()
        .code(2)
        .stderr(contains("handle"));

    // No transition exists in either store — asserted on the refusal TEXT
    // `transition show` prints for a missing transition, not just exit 2:
    // every refusal exits 2, so that alone would not distinguish "the
    // transition really isn't there" from some unrelated failure.
    env.fl(ra.path())
        .args(["transition", "show", "--project", "1", "--name", "launch"])
        .assert()
        .code(2)
        .stderr(contains("declares no transition named `launch`"));
    env.fl(rb.path())
        .args(["transition", "show", "--project", "1", "--name", "launch"])
        .assert()
        .code(2)
        .stderr(contains("declares no transition named `launch`"));
}

// Fix round 1 — Important 3: `choose_store` must never create a candidate
// store merely by checking whether it owns an id.
#[test]
fn searching_for_an_iri_never_creates_a_candidate_store_that_does_not_exist() {
    let env = Env::new();
    let ra = git_repo();
    let rb = git_repo();
    let stores = tempfile::tempdir().unwrap();
    let a = stores.path().join("a.redb");
    let b = stores.path().join("b.redb"); // never created
    env.write_config(&format!("{}{}", bind(ra.path(), &a), bind(rb.path(), &b)));
    env.fl(ra.path())
        .args(["project", "add", ra.path().to_str().unwrap()])
        .assert()
        .success();
    assert!(a.exists());
    assert!(!b.exists(), "fixture must start absent");

    env.fl(ra.path())
        .args(["gate", "show", STRANGER])
        .assert()
        .code(2);

    assert!(
        !b.exists(),
        "searching created a store that was never opened before"
    );
}

// Fix round 1 — Important 4a: IRIs in one command held by different stores.
// `choose_store` treats every `Iri` `Cmd::iris()` collects the same way
// regardless of which flag it came from — there is no `finding show`/`get`
// in this CLI that could print a raw finding IRI (`finding list` always
// prefers the RESOLVING store's own handle for it, unlike `gate show`'s
// JSON dump, which always prints the literal id), so two gates — one from
// each store, fed to `transition add`'s repeatable `--gate` — exercise
// exactly the same refusal path a `finding reproduce` with a finding IRI
// and a gate IRI would.
#[test]
fn iris_in_one_command_held_by_different_stores_is_refused_naming_both() {
    let env = Env::new();
    let (ra, rb, _stores, a, b) = two_bound_stores(&env);

    env.fl(ra.path())
        .args([
            "gate",
            "add",
            "--project",
            "1",
            "--name",
            "in-a",
            "--glob",
            "*.rs",
            "--program",
            "true",
        ])
        .assert()
        .success();
    let shown = env
        .fl(ra.path())
        .args(["gate", "show", "1"])
        .output()
        .unwrap();
    let gate_a: serde_json::Value = serde_json::from_slice(&shown.stdout).unwrap();
    let gate_a_iri = gate_a["id"].as_str().unwrap().to_string();

    env.fl(rb.path())
        .args([
            "gate",
            "add",
            "--project",
            "1",
            "--name",
            "in-b",
            "--glob",
            "*.rs",
            "--program",
            "true",
        ])
        .assert()
        .success();
    let shown = env
        .fl(rb.path())
        .args(["gate", "show", "1"])
        .output()
        .unwrap();
    let gate_b: serde_json::Value = serde_json::from_slice(&shown.stdout).unwrap();
    let gate_b_iri = gate_b["id"].as_str().unwrap().to_string();

    // `--project` takes `gate_a_iri` here, not project A's own id or a
    // handle: `choose_store` only ever sees the flat `Vec<Iri>`
    // `Cmd::iris()` collects, so this is enough to drive its "two different
    // stores" refusal without `--project` needing to be semantically a
    // project. A HANDLE here instead would ALSO trip the item-1 "handle
    // held by a different store" refusal once the IRI search moved the
    // command to store B — a different refusal, for a different reason,
    // that would make this test pass even if the one it means to pin broke.
    // The exact-phrase assertion below exists for the same reason from the
    // other direction: `NotOwned`'s message also names a store (or two),
    // so asserting only that `stderr` contains both paths would stay green
    // even if this bail were replaced by a `NotOwned` return — each store
    // really does hold its own id, so that would be a wrong answer that
    // reads as a pass.
    env.fl(ra.path())
        .args([
            "transition",
            "add",
            "--project",
            &gate_a_iri,
            "--name",
            "launch",
            "--from",
            "review",
            "--to",
            "done",
            "--regret",
            "low",
            "--gate",
            &gate_b_iri,
        ])
        .assert()
        .code(2)
        .stderr(contains("two different stores"))
        .stderr(contains("no store holds").not())
        .stderr(contains(a.to_str().unwrap()))
        .stderr(contains(b.to_str().unwrap()));
}

// Fix round 1 — Important 4b: one id owned by two stores at once.
#[test]
fn an_id_owned_by_two_stores_at_once_is_refused_naming_both() {
    let env = Env::new();
    let ra = git_repo();
    let rc = git_repo();
    let stores = tempfile::tempdir().unwrap();
    let a = stores.path().join("a.redb");
    let c = stores.path().join("c.redb");
    env.write_config(&format!("{}{}", bind(ra.path(), &a), bind(rc.path(), &c)));
    env.fl(ra.path())
        .args(["project", "add", ra.path().to_str().unwrap()])
        .assert()
        .success();
    env.fl(ra.path())
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
        .fl(ra.path())
        .args(["gate", "show", "1"])
        .output()
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&shown.stdout).unwrap();
    let iri = json["id"].as_str().unwrap().to_string();

    // Duplicate the store wholesale: c.redb now holds every id a.redb does,
    // including this gate's.
    std::fs::copy(&a, &c).unwrap();

    env.fl(ra.path())
        .args(["gate", "show", &iri])
        .assert()
        .code(2)
        .stderr(contains("held by more than one store"))
        .stderr(contains("no store holds").not())
        .stderr(contains(a.to_str().unwrap()))
        .stderr(contains(c.to_str().unwrap()));
}

/// One repo registered in the default store of `env`, run from the repo.
fn registered(env: &Env) -> tempfile::TempDir {
    let repo = git_repo();
    env.fl(repo.path())
        .args(["project", "add", repo.path().to_str().unwrap()])
        .assert()
        .success();
    repo
}

// Final review, item 1: an IRI the store holds as a GATE, passed as
// `--project`, must be refused (exit 2) — never listed as an empty project,
// and never reported as `attempts: 0`.
#[test]
fn a_gate_iri_passed_as_a_project_is_refused_not_listed_as_empty() {
    let env = Env::new();
    let repo = registered(&env);
    let gate = a_gate_iri_in(&env, repo.path());
    for cmd in [
        &["record", "list"][..],
        &["finding", "list"][..],
        &["gate", "list"][..],
        &["stats"][..],
    ] {
        env.fl(repo.path())
            .args(cmd)
            .args(["--project", &gate])
            .assert()
            .code(2)
            .stdout("")
            .stderr(contains(gate.as_str()).and(contains("not a project")));
    }
}

// Final review, item 2: `project add <path>` registers `<path>` in the
// store the config binds `<path>` to — not the store bound to the directory
// the command happens to run in.
#[test]
fn project_add_uses_the_store_bound_to_the_path_not_the_current_directory() {
    let env = Env::new();
    let (ra, rb) = (git_repo(), git_repo());
    let stores = tempfile::tempdir().unwrap();
    let (a, b) = (stores.path().join("a.redb"), stores.path().join("b.redb"));
    env.write_config(&format!("{}{}", bind(ra.path(), &a), bind(rb.path(), &b)));
    // `a` exists (and holds nothing), so "not written there" is a check on a
    // store that was really there to be written.
    env.fl(ra.path())
        .args(["project", "list"])
        .assert()
        .success()
        .stdout("");
    assert!(a.exists());

    env.fl(ra.path())
        .args(["project", "add", rb.path().to_str().unwrap()])
        .assert()
        .success();

    let root = rb.path().canonicalize().unwrap();
    let root = root.to_str().unwrap();
    env.fl(rb.path())
        .args(["project", "list"])
        .assert()
        .success()
        .stdout(contains(root));
    env.fl(ra.path())
        .args(["project", "list"])
        .assert()
        .success()
        .stdout("");
    assert!(!env.default_store().exists(), "the default store was used");
}

// Final review, item 5: an empty or relative `$XDG_DATA_HOME` is ignored,
// as `$XDG_CONFIG_HOME`'s is — used as-is it would create the store under
// whatever directory the command ran in.
#[test]
fn an_empty_or_relative_xdg_data_home_falls_back_to_home_not_the_cwd() {
    for value in ["", "relative/data"] {
        let env = Env::new();
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        env.fl(cwd.path())
            .env("XDG_DATA_HOME", value)
            .env("HOME", home.path())
            .args(["project", "list"])
            .assert()
            .success();
        assert!(
            home.path().join(".local/share/fl/fl.redb").exists(),
            "XDG_DATA_HOME={value:?}: the store is not under $HOME/.local/share"
        );
        assert_eq!(
            std::fs::read_dir(cwd.path()).unwrap().count(),
            0,
            "XDG_DATA_HOME={value:?}: something was created in the current directory"
        );
    }
}

// Final review, item 6: with no store anywhere, `NotOwned` must say that no
// store exists yet — not print `(searched: )`, an empty list that reads
// like a search that ran.
#[test]
fn an_iri_on_a_fresh_install_says_no_store_exists_yet() {
    let env = Env::new();
    let cwd = tempfile::tempdir().unwrap();
    env.fl(cwd.path())
        .args(["gate", "show", STRANGER])
        .assert()
        .code(2)
        .stderr(
            contains(STRANGER)
                .and(contains("no store exists yet"))
                .and(contains("searched: )").not()),
        );
}

// Final review, item 9: success output names an item by its handle when it
// has one, even when the person typed its full IRI.
#[test]
fn gate_affirm_by_iri_prints_the_handle_not_the_iri() {
    let env = Env::new();
    let repo = registered(&env);
    let gate = a_gate_iri_in(&env, repo.path());
    assert!(gate.starts_with("urn:uuid:"), "fixture: {gate}");
    env.fl(repo.path())
        .args(["gate", "affirm", &gate, "--by", "tester"])
        .assert()
        .success()
        .stdout(predicates::str::starts_with("1\t").and(contains("urn:uuid:").not()));
}

// Final review, item 8: an attempt names its record by the record's PRIMARY
// id, even when the person typed an alias.
#[test]
fn an_attempt_through_a_record_alias_stores_the_primary() {
    use fl_core::store::{Catalog, Ledger, Tracker};
    let env = Env::new();
    let repo = registered(&env);
    env.fl(repo.path())
        .args(["record", "add", "--project", "1", "--title", "t"])
        .assert()
        .success();
    let alias = "https://github.com/o/r/issues/41";
    let (project, record) = {
        let s = fl_store::RedbStore::open(&env.default_store()).unwrap();
        let project = s.list_projects().unwrap().remove(0).id;
        let record = s.list_records(&project).unwrap().remove(0).id;
        s.add_alias(record.iri(), fl_core::Iri::parse(alias).unwrap())
            .unwrap();
        (project, record)
    };
    // A zero budget is refused before anything is spawned, and still
    // recorded.
    env.fl(repo.path())
        .args(["attempt", alias, "--budget-usd-micros", "0"])
        .assert()
        .code(1);
    let s = fl_store::RedbStore::open(&env.default_store()).unwrap();
    let attempts = s.attempts(&project).unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].record, record, "the attempt stored the alias");
}
