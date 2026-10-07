//! A routed project (routing spec): two tiers — the local store and the
//! fake GitHub's `acme/widgets` — and the rule that routes each new item.

use assert_cmd::Command;
use fl_github::fake::FakeGithub;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;
use std::fs;
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

/// The config line binding the project to the fake's repository.
const BOUND: &str = "tracker = { github = \"acme/widgets\", credential = \"env\" }\n";

struct R {
    home: tempfile::TempDir,
    repo: tempfile::TempDir,
    fake: FakeGithub,
}

/// A git working tree with `src/a.rs`, whose project's config entry
/// carries `tracker` — [`BOUND`], or "" for none.
fn world(tracker: &str) -> R {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q"]);
    git(repo.path(), &["config", "user.email", "t@example.com"]);
    git(repo.path(), &["config", "user.name", "t"]);
    fs::create_dir_all(repo.path().join("src")).unwrap();
    fs::write(repo.path().join("src/a.rs"), "fn a() {}").unwrap();
    git(repo.path(), &["add", "-A"]);
    git(repo.path(), &["commit", "-qm", "first"]);
    let r = R {
        home,
        repo,
        fake: FakeGithub::start("acme/widgets"),
    };
    r.configure(r.home.path(), tracker);
    r
}

impl R {
    /// `home`'s config: this repository's project, its store under `home`,
    /// and `tracker`.
    fn configure(&self, home: &Path, tracker: &str) {
        let cfg = format!(
            "[[project]]\nroot = \"{}\"\nstore = \"{}\"\n{tracker}",
            self.repo.path().canonicalize().unwrap().display(),
            home.join("fl.redb").display()
        );
        fs::create_dir_all(home.join("config/fl")).unwrap();
        fs::write(home.join("config/fl/config.toml"), cfg).unwrap();
    }

    /// `fl` on the machine whose home is `home`.
    fn fl_at(&self, home: &Path) -> Command {
        let mut c = Command::cargo_bin("fl").unwrap();
        c.env("XDG_CONFIG_HOME", home.join("config"))
            .env("XDG_DATA_HOME", home.join("data"))
            .env("FL_GITHUB_TOKEN", "t")
            .env("FL_GITHUB_API_URL", self.fake.url())
            .env_remove("GITHUB_TOKEN")
            .env_remove("FL_DB")
            .current_dir(self.repo.path());
        c
    }

    fn fl(&self) -> Command {
        self.fl_at(self.home.path())
    }

    /// `fl args`, which must succeed; its stdout.
    fn ok(&self, args: &[&str]) -> String {
        let out = self.fl().args(args).output().unwrap();
        assert!(
            out.status.success(),
            "fl {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    /// The project, routed with the starting set.
    fn routed(&self) {
        self.ok(&["project", "add", "."]);
        self.ok(&["routing", "set", "--project", "1", "code", "local"]);
    }
}

// Routing spec §1.2, §2.3: the first set writes the starting set, and says
// what changes for a project that was local-only.
#[test]
fn the_first_set_writes_the_starting_set_and_says_how_handles_change() {
    let g = world("");
    g.ok(&["project", "add", "."]);
    g.fl()
        .args(["routing", "set", "--project", "1", "ops", "github"])
        .assert()
        .success()
        .stdout("ops\tgithub\t-\n")
        .stderr(
            contains("wrote the starting set first")
                .and(contains("security to github, sensitive"))
                .and(contains("handles change in this project"))
                .and(contains("`#3` named local item 3")),
        );
    let shown = g.ok(&["routing", "show", "--project", "1"]);
    assert_eq!(
        shown,
        "code\tlocal\t-\ndesign\tgithub\t-\nops\tgithub\t-\nproduct\tgithub\t-\n\
         security\tgithub\tsensitive\ntests\tlocal\t-\n"
    );
}

#[test]
fn a_github_bound_project_hears_that_a_bare_number_now_names_a_local_item() {
    let g = world(BOUND);
    g.ok(&["project", "add", "."]);
    g.fl()
        .args(["routing", "set", "--project", "1", "code", "local"])
        .assert()
        .success()
        .stderr(contains("a bare number such as `41` named GitHub issue 41"));
    assert!(
        g.fake.state().requests.is_empty(),
        "authoring the map needs no GitHub"
    );
}

#[test]
fn a_later_set_changes_one_area_and_says_nothing_of_handles() {
    let g = world("");
    g.routed();
    g.fl()
        .args([
            "routing",
            "set",
            "--project",
            "1",
            "code",
            "github",
            "--sensitive",
        ])
        .assert()
        .success()
        .stdout("code\tgithub\tsensitive\n")
        .stderr(contains("handles change in this project").not());
    assert!(
        g.ok(&["routing", "show", "--project", "1"])
            .starts_with("code\tgithub\tsensitive\n")
    );
}

#[test]
fn an_area_or_a_tier_that_is_not_one_is_refused_and_nothing_is_written() {
    let g = world("");
    g.ok(&["project", "add", "."]);
    g.fl()
        .args(["routing", "set", "--project", "1", "Code", "local"])
        .assert()
        .failure()
        .stderr(contains("is not an area name"));
    g.fl()
        .args(["routing", "set", "--project", "1", "code", "cloud"])
        .assert()
        .failure()
        .stderr(contains("is not a tier"));
    g.fl()
        .args(["routing", "show", "--project", "1"])
        .assert()
        .success()
        .stdout("")
        .stderr(contains("has no routing map"));
}

// Routing spec decision 22: a set that only changes a tier keeps the area's
// sensitivity.
#[test]
fn a_tier_change_keeps_the_areas_sensitivity() {
    let g = world("");
    g.routed();
    assert_eq!(
        g.ok(&["routing", "set", "--project", "1", "security", "local"]),
        "security\tlocal\tsensitive\n"
    );
}

// Routing spec decision 20: handles are numbered per store, so a routed
// store holds one project; the refusal names the remedy and writes nothing.
#[test]
fn routing_a_project_that_shares_its_store_is_refused_naming_its_own_store() {
    let g = world("");
    g.ok(&["project", "add", "."]);
    let other = tempfile::tempdir().unwrap();
    git(other.path(), &["init", "-q"]);
    git(
        other.path(),
        &[
            "-c",
            "user.email=t@example.com",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "first",
        ],
    );
    let db = g.home.path().join("fl.redb");
    g.fl()
        .args(["--db", db.to_str().unwrap(), "project", "add"])
        .arg(other.path())
        .assert()
        .success();
    g.fl()
        .args(["routing", "set", "--project", "1", "code", "local"])
        .assert()
        .failure()
        .stderr(contains("needs a store of its own").and(contains("its own `store`")));
    g.fl()
        .args(["routing", "show", "--project", "1"])
        .assert()
        .success()
        .stderr(contains("has no routing map"));
}

// Routing spec §2.3: the import that first routes a store says how handles
// change there, as the first `set` does where the project is authored.
#[test]
fn an_import_that_first_routes_a_store_says_how_handles_change() {
    let g = world("");
    g.routed();
    g.ok(&["manifest", "export", "--project", "1"]);
    let other = tempfile::tempdir().unwrap();
    g.configure(other.path(), BOUND);
    g.fl_at(other.path())
        .args(["manifest", "import"])
        .assert()
        .success()
        .stderr(contains("a bare number such as `41` named GitHub issue 41"));
    g.fl_at(other.path())
        .args(["manifest", "import"])
        .assert()
        .success()
        .stderr(contains("handles change in this project").not());
}

// Routing spec §2.6: work on local items needs no network and no credential.
#[test]
fn a_local_item_is_made_and_moved_without_a_request_to_github() {
    let g = world(BOUND);
    g.routed();
    g.fl()
        .args([
            "record",
            "add",
            "--project",
            "1",
            "--title",
            "fix it",
            "--area",
            "code",
        ])
        .assert()
        .success()
        .stdout("1\tfix it\n");
    g.fl()
        .args(["record", "move", "1", "--to", "doing"])
        .assert()
        .success();
    assert!(
        g.fake.state().requests.is_empty(),
        "{:?}",
        g.fake.state().requests
    );
}

// Routing spec §1.1, §2.3: a GitHub-tier record is an issue carrying its
// area's label, printed as `#1`.
#[test]
fn a_github_tier_record_is_an_issue_with_its_area_label_printed_as_hash() {
    let g = world(BOUND);
    g.routed();
    g.fl()
        .args([
            "record",
            "add",
            "--project",
            "1",
            "--title",
            "look",
            "--area",
            "design",
        ])
        .assert()
        .success()
        .stdout("#1\tlook\n");
    assert_eq!(
        g.fake.issue(1).labels,
        vec!["fl:record", "fl:record/todo", "fl:area/design"]
    );
}

#[test]
fn a_routed_create_with_no_area_or_an_undeclared_one_is_refused_naming_the_areas() {
    let g = world("");
    g.routed();
    g.fl()
        .args(["record", "add", "--project", "1", "--title", "t"])
        .assert()
        .failure()
        .stderr(contains("needs an area for every new item").and(contains("code, design")));
    g.fl()
        .args([
            "record",
            "add",
            "--project",
            "1",
            "--title",
            "t",
            "--area",
            "ops",
        ])
        .assert()
        .failure()
        .stderr(contains("`ops` is not an area this project declares"));
}

#[test]
fn area_in_a_project_with_no_routing_map_is_refused_naming_routing_set() {
    let g = world("");
    g.ok(&["project", "add", "."]);
    g.fl()
        .args([
            "record",
            "add",
            "--project",
            "1",
            "--title",
            "t",
            "--area",
            "code",
        ])
        .assert()
        .failure()
        .stderr(contains(
            "needs a routing map, and this project declares none",
        ));
}

// Routing spec §1.3: "routing never changes tier silently".
#[test]
fn a_github_tier_create_on_an_unbound_machine_names_the_config_entry() {
    let g = world("");
    g.routed();
    g.fl()
        .args([
            "record",
            "add",
            "--project",
            "1",
            "--title",
            "t",
            "--area",
            "design",
        ])
        .assert()
        .failure()
        .stderr(
            contains("binds no GitHub repository for the project")
                .and(contains("never puts an item in the other tier")),
        );
    // Nothing landed locally: the next local record is the first.
    g.fl()
        .args([
            "record",
            "add",
            "--project",
            "1",
            "--title",
            "u",
            "--area",
            "code",
        ])
        .assert()
        .success()
        .stdout("1\tu\n");
}

// Routing spec decision 12.
#[test]
fn a_routed_binding_with_the_github_ledger_is_refused_when_the_command_starts() {
    let with_ledger =
        "tracker = { github = \"acme/widgets\", credential = \"env\", ledger = \"github\" }\n";
    let g = world(with_ledger);
    g.ok(&["project", "add", "."]);
    g.fl()
        .args(["routing", "set", "--project", "1", "code", "local"])
        .assert()
        .failure()
        .stderr(contains(
            "a routed project keeps its runs and decisions in the local ledger",
        ));
    g.fl()
        .args(["routing", "show", "--project", "1"])
        .assert()
        .success()
        .stderr(contains("has no routing map"));
    g.configure(g.home.path(), BOUND);
    g.ok(&["routing", "set", "--project", "1", "code", "local"]);
    g.configure(g.home.path(), with_ledger);
    g.fl()
        .args([
            "record",
            "add",
            "--project",
            "1",
            "--title",
            "t",
            "--area",
            "code",
        ])
        .assert()
        .failure()
        .stderr(contains(
            "a routed project keeps its runs and decisions in the local ledger",
        ));
}

// Routing spec §1.2: one routing rule per project, not per machine — on
// the authoring machine, once a manifest exists, it must carry the map.
#[test]
fn a_routed_create_on_the_authoring_machine_needs_the_map_exported() {
    let g = world("");
    g.routed();
    g.ok(&[
        "record",
        "add",
        "--project",
        "1",
        "--title",
        "a",
        "--area",
        "code",
    ]);
    g.ok(&["manifest", "export", "--project", "1"]);
    g.ok(&[
        "record",
        "add",
        "--project",
        "1",
        "--title",
        "b",
        "--area",
        "code",
    ]);
    g.ok(&["routing", "set", "--project", "1", "ops", "local"]);
    g.fl()
        .args([
            "record",
            "add",
            "--project",
            "1",
            "--title",
            "c",
            "--area",
            "code",
        ])
        .assert()
        .failure()
        .stderr(contains("changed since the manifest at"));
    g.ok(&["manifest", "export", "--project", "1"]);
    g.ok(&[
        "record",
        "add",
        "--project",
        "1",
        "--title",
        "c",
        "--area",
        "code",
    ]);
}

// …and on an importing machine, the import must be current.
#[test]
fn a_routed_create_on_an_importing_machine_needs_the_import_current() {
    let g = world("");
    g.routed();
    g.ok(&["manifest", "export", "--project", "1"]);
    git(g.repo.path(), &["add", "-A"]);
    git(g.repo.path(), &["commit", "-qm", "manifest"]);
    let other = tempfile::tempdir().unwrap();
    g.configure(other.path(), "");
    let on_other = |args: &[&str]| g.fl_at(other.path()).args(args).assert();
    on_other(&["manifest", "import"])
        .success()
        .stdout(contains("routing\t5 areas"));
    on_other(&[
        "record",
        "add",
        "--project",
        "1",
        "--title",
        "t",
        "--area",
        "code",
    ])
    .success();
    g.ok(&["routing", "set", "--project", "1", "ops", "local"]);
    g.ok(&["manifest", "export", "--project", "1"]);
    on_other(&[
        "record",
        "add",
        "--project",
        "1",
        "--title",
        "u",
        "--area",
        "code",
    ])
    .failure()
    .stderr(contains("may route items differently"));
    on_other(&["manifest", "import"]).success();
    on_other(&[
        "record",
        "add",
        "--project",
        "1",
        "--title",
        "u",
        "--area",
        "code",
    ])
    .success();
}

// GitHub tracker spec §4.3: a gate a GitHub item names must be in the
// committed manifest — in a routed store, for a GitHub-tier finding.
#[test]
fn reproducing_a_github_tier_finding_checks_the_committed_manifest() {
    let g = world(BOUND);
    g.routed();
    g.ok(&[
        "record",
        "add",
        "--project",
        "1",
        "--title",
        "look",
        "--area",
        "design",
    ]);
    g.ok(&[
        "finding",
        "raise",
        "--record",
        "https://github.com/acme/widgets/issues/1",
        "--claim",
        "it is off",
        "--by",
        "rev",
    ]);
    g.ok(&[
        "gate",
        "add",
        "--project",
        "1",
        "--name",
        "g",
        "--glob",
        "src/**/*.rs",
        "--program",
        "false",
    ]);
    g.fl()
        .args([
            "finding",
            "reproduce",
            "https://github.com/acme/widgets/issues/2",
            "--gate",
            "1",
        ])
        .assert()
        .failure()
        .stderr(contains("there is no manifest at"));
}

// Routing spec decision 12: a routed manifest is not imported where the
// binding names the GitHub ledger, and nothing is imported.
#[test]
fn a_routed_manifest_is_not_imported_where_the_binding_names_the_github_ledger() {
    let g = world("");
    g.routed();
    g.ok(&["manifest", "export", "--project", "1"]);
    let other = tempfile::tempdir().unwrap();
    g.configure(
        other.path(),
        "tracker = { github = \"acme/widgets\", credential = \"env\", ledger = \"github\" }\n",
    );
    g.fl_at(other.path())
        .args(["manifest", "import"])
        .assert()
        .failure()
        .stderr(contains(
            "a routed project keeps its runs and decisions in the local ledger",
        ));
    g.fl_at(other.path())
        .args(["project", "list"])
        .assert()
        .success()
        .stdout("");
}

// Routing spec §2.3, §2.2: after a repository rename a GitHub item still
// prints as `#n` — its URL names the repository's name now.
#[test]
fn after_a_rename_a_github_item_still_prints_as_hash() {
    let g = world(BOUND);
    g.routed();
    g.fake.rename("acme/gadgets");
    assert_eq!(
        g.ok(&[
            "record",
            "add",
            "--project",
            "1",
            "--title",
            "look",
            "--area",
            "design"
        ]),
        "#1\tlook\n"
    );
    let moved = g.ok(&["record", "move", "#1", "--to", "doing"]);
    assert!(moved.starts_with("#1\tdoing\t"), "{moved}");
}

// Routing spec §2.3: in a routed project the two spellings name two items,
// and each prints back as it can be typed.
#[test]
fn a_bare_number_and_a_hash_number_name_different_items_and_print_back_as_typed() {
    let g = world(BOUND);
    g.routed();
    assert_eq!(
        g.ok(&[
            "record",
            "add",
            "--project",
            "1",
            "--title",
            "local one",
            "--area",
            "code"
        ]),
        "1\tlocal one\n"
    );
    assert_eq!(
        g.ok(&[
            "record",
            "add",
            "--project",
            "1",
            "--title",
            "issue one",
            "--area",
            "design"
        ]),
        "#1\tissue one\n"
    );
    // An ungated move prints `<handle>\t<state>\tungated: …`.
    let moved = g.ok(&["record", "move", "1", "--to", "doing"]);
    assert!(moved.starts_with("1\tdoing\t"), "{moved}");
    let moved = g.ok(&["record", "move", "#1", "--to", "review"]);
    assert!(moved.starts_with("#1\treview\t"), "{moved}");
    let labels = g.fake.issue(1).labels;
    assert!(
        labels.contains(&"fl:record/review".to_string())
            && !labels.contains(&"fl:record/doing".to_string()),
        "{labels:?}"
    );
}

#[test]
fn a_bare_number_no_local_item_holds_asks_did_you_mean_the_issue() {
    let g = world(BOUND);
    g.routed();
    g.fl()
        .args(["record", "move", "5", "--to", "doing"])
        .assert()
        .failure()
        .stderr(contains("Did you mean `#5`?"));
    assert!(
        g.fake.state().requests.is_empty(),
        "a bare number never reaches GitHub"
    );
}

#[test]
fn on_an_unbound_machine_a_bare_number_gets_no_hint() {
    let g = world("");
    g.routed();
    g.fl()
        .args(["record", "move", "5", "--to", "doing"])
        .assert()
        .failure()
        .stderr(contains("in this machine's local tier").and(contains("Did you mean").not()));
}

#[test]
fn a_hash_number_that_is_no_record_is_refused_naming_the_repository() {
    let g = world(BOUND);
    g.routed();
    g.fl()
        .args(["record", "move", "#9", "--to", "doing"])
        .assert()
        .failure()
        .stderr(contains("there is no record #9 in acme/widgets"));
}

// Routing spec §2.3: `fl github …` names GitHub items only, so a bare
// number there is an issue.
#[test]
fn a_bare_number_given_to_fl_github_is_an_issue() {
    let g = world(BOUND);
    g.routed();
    g.ok(&[
        "record",
        "add",
        "--project",
        "1",
        "--title",
        "t",
        "--area",
        "design",
    ]);
    g.fl()
        .args(["github", "repair", "1", "--by", "owner"])
        .assert()
        .success()
        .stdout("consistent\t1\ttodo\n");
}

// Routing spec §2.3: `owner/repo#41` names the issue in a routed store too.
#[test]
fn owner_repo_hash_names_the_issue_in_a_routed_store() {
    let g = world(BOUND);
    g.routed();
    g.ok(&[
        "record",
        "add",
        "--project",
        "1",
        "--title",
        "l",
        "--area",
        "code",
    ]);
    g.ok(&[
        "record",
        "add",
        "--project",
        "1",
        "--title",
        "g",
        "--area",
        "design",
    ]);
    let moved = g.ok(&["record", "move", "acme/widgets#1", "--to", "doing"]);
    assert!(moved.starts_with("#1\tdoing\t"), "{moved}");
    assert!(
        g.fake
            .issue(1)
            .labels
            .contains(&"fl:record/doing".to_string())
    );
}

// Routing spec §2.2: an id no local store holds reaches the router —
// GitHub's alias scan finds an item another machine moved there; otherwise
// it is held on another machine's local tier.
#[test]
fn an_id_no_local_store_holds_is_looked_for_on_github_then_said_to_be_elsewhere() {
    let g = world(BOUND);
    g.routed();
    g.ok(&[
        "record",
        "add",
        "--project",
        "1",
        "--title",
        "look",
        "--area",
        "design",
    ]);
    let moved_here = "urn:uuid:00000000-0000-7000-8000-0000000000aa";
    g.fake.web_edit(1, |i| {
        let (prose, mut m) = fl_github::meta::parse_body(&i.body).unwrap();
        m.also_known_as
            .push(fl_core::Iri::parse(moved_here).unwrap());
        i.body = fl_github::meta::render_body(&prose, &m);
    });
    let moved = g.ok(&["record", "move", moved_here, "--to", "doing"]);
    assert!(moved.starts_with("#1\tdoing\t"), "{moved}");
    g.fl()
        .args([
            "record",
            "move",
            "urn:uuid:00000000-0000-7000-8000-0000000000bb",
            "--to",
            "doing",
        ])
        .assert()
        .failure()
        .stderr(contains("another machine's local tier"));
}

// Routing spec §1.3: on an unbound machine an issue URL is refused as the
// missing config entry, never searched for as a local id.
#[test]
fn an_issue_url_on_an_unbound_machine_names_the_missing_config_entry() {
    let g = world("");
    g.routed();
    g.fl()
        .args([
            "record",
            "move",
            "https://github.com/acme/widgets/issues/1",
            "--to",
            "doing",
        ])
        .assert()
        .failure()
        .stderr(contains("binds no GitHub repository for the project"));
}

// Routing spec §2.1: `--tier` overrides the map; the area is recorded
// either way.
#[test]
fn a_tier_given_puts_a_record_there_with_its_area() {
    let g = world(BOUND);
    g.routed();
    assert_eq!(
        g.ok(&[
            "record",
            "add",
            "--project",
            "1",
            "--title",
            "t",
            "--area",
            "code",
            "--tier",
            "github",
        ]),
        "#1\tt\n"
    );
    assert!(g.fake.issue(1).labels.contains(&"fl:area/code".to_string()));
}

#[test]
fn tier_or_area_in_a_project_with_no_routing_map_is_refused() {
    let g = world("");
    g.ok(&["project", "add", "."]);
    g.fl()
        .args([
            "record",
            "add",
            "--project",
            "1",
            "--title",
            "t",
            "--tier",
            "local",
        ])
        .assert()
        .failure()
        .stderr(contains(
            "`--tier` needs a routing map, and this project declares none",
        ));
    g.ok(&["record", "add", "--project", "1", "--title", "t"]);
    for extra in [["--area", "code"], ["--tier", "local"]] {
        let mut args = vec![
            "finding", "raise", "--record", "1", "--claim", "c", "--by", "r",
        ];
        args.extend(extra);
        g.fl().args(&args).assert().failure().stderr(contains(
            "needs a routing map, and this project declares none",
        ));
    }
}

// Routing spec §1.1: a finding takes its record's area, and says so.
#[test]
fn a_finding_takes_its_records_area_and_says_so() {
    let g = world(BOUND);
    g.routed();
    g.ok(&[
        "record",
        "add",
        "--project",
        "1",
        "--title",
        "look",
        "--area",
        "design",
    ]);
    g.fl()
        .args([
            "finding", "raise", "--record", "#1", "--claim", "off", "--by", "rev",
        ])
        .assert()
        .success()
        .stdout("#2\traised\toff\n")
        .stderr(contains("note: area: design, from its record"));
    g.fl()
        .args([
            "finding", "raise", "--record", "#1", "--claim", "here", "--by", "rev", "--area",
            "code",
        ])
        .assert()
        .success()
        .stdout("1\traised\there\n")
        .stderr(contains("from its record").not());
}

#[test]
fn a_finding_whose_record_has_no_area_needs_one() {
    let g = world("");
    g.ok(&["project", "add", "."]);
    g.ok(&[
        "record",
        "add",
        "--project",
        "1",
        "--title",
        "before routing",
    ]);
    g.ok(&["routing", "set", "--project", "1", "code", "local"]);
    g.fl()
        .args([
            "finding", "raise", "--record", "1", "--claim", "c", "--by", "r",
        ])
        .assert()
        .failure()
        .stderr(contains("has no area to inherit"));
    g.fl()
        .args([
            "finding", "raise", "--record", "1", "--claim", "c", "--by", "r", "--area", "code",
        ])
        .assert()
        .success();
}

// Routing spec §2.1, decision 13: never to a public repository by the map;
// nothing is created, and the refusal names `--tier local`.
#[test]
fn a_sensitive_area_routed_to_a_public_repository_creates_nothing_and_names_tier_local() {
    let g = world(BOUND);
    g.routed();
    g.fake.state().repos[0].visibility = "public".into();
    g.ok(&[
        "record",
        "add",
        "--project",
        "1",
        "--title",
        "t",
        "--area",
        "code",
    ]);
    for extra in [
        vec!["--area", "security"],
        vec!["--area", "design", "--security"],
    ] {
        let mut args = vec![
            "finding", "raise", "--record", "1", "--claim", "c", "--by", "r",
        ];
        args.extend(extra);
        g.fl()
            .args(&args)
            .assert()
            .failure()
            .stderr(contains("security-sensitive").and(contains("--tier local")));
    }
    g.fl()
        .args([
            "record",
            "add",
            "--project",
            "1",
            "--title",
            "s",
            "--area",
            "security",
        ])
        .assert()
        .failure()
        .stderr(contains("security-sensitive"));
    // Named by the person, the tier is theirs, and the refusal still speaks of
    // the record rather than of a security finding.
    g.fl()
        .args([
            "record",
            "add",
            "--project",
            "1",
            "--title",
            "s",
            "--area",
            "security",
            "--tier",
            "github",
        ])
        .assert()
        .failure()
        .stderr(
            contains("this record is security-sensitive").and(contains("a security finding").not()),
        );
    assert_eq!(g.fake.issue_count(), 0, "nothing created on GitHub");
    g.fl()
        .args([
            "finding", "raise", "--record", "1", "--claim", "c", "--by", "r", "--area", "security",
            "--tier", "local",
        ])
        .assert()
        .success()
        .stdout("1\traised\tc\n");
}

#[test]
fn a_finding_in_a_sensitive_area_on_a_private_repository_is_a_security_finding() {
    let g = world(BOUND);
    g.routed();
    g.ok(&[
        "record",
        "add",
        "--project",
        "1",
        "--title",
        "t",
        "--area",
        "code",
    ]);
    g.ok(&[
        "finding", "raise", "--record", "1", "--claim", "c", "--by", "r", "--area", "security",
    ]);
    assert!(
        g.fake.issue(1).body.contains("\"security\":true"),
        "{}",
        g.fake.issue(1).body
    );
}

// Routing spec §2.5: a GitHub finding about a local record publishes the
// record's title; on a repository that is not private, fl says so first.
#[test]
fn a_github_finding_about_a_local_record_warns_on_a_public_repository() {
    let g = world(BOUND);
    g.routed();
    g.ok(&[
        "record",
        "add",
        "--project",
        "1",
        "--title",
        "fix the parser",
        "--area",
        "code",
    ]);
    g.fl()
        .args([
            "finding", "raise", "--record", "1", "--claim", "c", "--by", "r", "--area", "design",
        ])
        .assert()
        .success()
        .stderr(contains("names its local record").not());
    g.fake.state().repos[0].visibility = "public".into();
    g.fl()
        .args([
            "finding", "raise", "--record", "1", "--claim", "d", "--by", "r", "--area", "design",
        ])
        .assert()
        .success()
        .stderr(
            contains("warning: acme/widgets is public").and(contains("names its local record")),
        );
    assert!(
        g.fake.issue(2).body.contains("Record: fix the parser"),
        "{}",
        g.fake.issue(2).body
    );
    // Only a GitHub finding about a LOCAL record publishes a local title.
    g.ok(&[
        "record",
        "add",
        "--project",
        "1",
        "--title",
        "look",
        "--area",
        "design",
    ]);
    for args in [
        [
            "finding", "raise", "--record", "#3", "--claim", "e", "--by", "r",
        ],
        [
            "finding", "raise", "--record", "1", "--claim", "f", "--by", "r",
        ],
    ] {
        g.fl()
            .args(args)
            .assert()
            .success()
            .stderr(contains("names its local record").not());
    }
}

// Routing spec decision 21: a finding about a local record in a sensitive
// area never reaches a public repository, whatever its own area.
#[test]
fn a_finding_about_a_sensitive_local_record_is_refused_on_a_public_repository() {
    let g = world(BOUND);
    g.routed();
    g.fake.state().repos[0].visibility = "public".into();
    g.ok(&[
        "record",
        "add",
        "--project",
        "1",
        "--title",
        "the key leaks",
        "--area",
        "security",
        "--tier",
        "local",
    ]);
    g.fl()
        .args([
            "finding", "raise", "--record", "1", "--claim", "c", "--by", "r", "--area", "design",
        ])
        .assert()
        .failure()
        .stderr(
            contains("about a record in a sensitive area")
                .and(contains("--tier local"))
                .and(contains("names its local record").not()),
        );
    assert_eq!(g.fake.issue_count(), 0, "nothing created on GitHub");
}

// Routing spec §1.2: a finding is a routed create too.
#[test]
fn a_finding_on_the_authoring_machine_needs_the_map_exported() {
    let g = world("");
    g.routed();
    g.ok(&[
        "record",
        "add",
        "--project",
        "1",
        "--title",
        "t",
        "--area",
        "code",
    ]);
    g.ok(&["manifest", "export", "--project", "1"]);
    g.ok(&["routing", "set", "--project", "1", "ops", "local"]);
    g.fl()
        .args([
            "finding", "raise", "--record", "1", "--claim", "c", "--by", "r",
        ])
        .assert()
        .failure()
        .stderr(contains("changed since the manifest at"));
}

// Routing spec §2.5: the warning comes before the write, and a visibility
// that cannot be read refuses the create — an unknown visibility is not
// private. Nothing is written.
#[test]
fn a_visibility_that_cannot_be_read_refuses_a_finding_about_a_local_record_unwritten() {
    let g = world(BOUND);
    g.routed();
    g.ok(&[
        "record",
        "add",
        "--project",
        "1",
        "--title",
        "fix the parser",
        "--area",
        "code",
    ]);
    // Opening GitHub reads the repository once; the warning's read is next.
    g.fake.state().fail_repo_read_after = Some(1);
    g.fl()
        .args([
            "finding", "raise", "--record", "1", "--claim", "c", "--by", "r", "--area", "design",
        ])
        .assert()
        .failure()
        .stderr(contains("could not read the visibility of acme/widgets"));
    assert_eq!(g.fake.issue_count(), 0, "nothing created on GitHub");
}
