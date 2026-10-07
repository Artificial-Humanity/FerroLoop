//! Escalation (routing spec §3): `fl record escalate` and `fl finding
//! escalate` move a local item to the fake GitHub's `acme/widgets`, after
//! every check, and a rerun finishes what a stop left.

use assert_cmd::Command;
use fl_github::fake::FakeGithub;
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

/// The disclosure warning's own words (routing spec §3.2).
const WARNED: &str = "the escalation publishes";

/// The context a failure after the mark carries (routing spec §3.3).
const STOPPED: &str = "stopped after its mark was written";

/// The note a rerun given another who or why prints.
const KEPT: &str = "the mark's who and why are kept";

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
    let cfg = format!(
        "[[project]]\nroot = \"{}\"\nstore = \"{}\"\n{tracker}",
        r.repo.path().canonicalize().unwrap().display(),
        r.home.path().join("fl.redb").display()
    );
    fs::create_dir_all(r.home.path().join("config/fl")).unwrap();
    fs::write(r.home.path().join("config/fl/config.toml"), cfg).unwrap();
    r
}

impl R {
    fn fl(&self) -> Command {
        let home = self.home.path();
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

    /// `fl args`, which must succeed; its stdout and stderr.
    fn ok_said(&self, args: &[&str]) -> (String, String) {
        let out = self.fl().args(args).output().unwrap();
        let err = String::from_utf8(out.stderr).unwrap();
        assert!(out.status.success(), "fl {args:?}: {err}");
        (String::from_utf8(out.stdout).unwrap(), err)
    }

    /// `fl args`, which must fail with exit 2; its stderr.
    fn refused(&self, args: &[&str]) -> String {
        let out = self.fl().args(args).output().unwrap();
        let err = String::from_utf8(out.stderr).unwrap();
        assert_eq!(out.status.code(), Some(2), "fl {args:?}: {err}");
        err
    }

    /// The project, routed with the starting set (`code` and `tests` local).
    fn routed(&self) {
        self.ok(&["project", "add", "."]);
        self.ok(&["routing", "set", "--project", "1", "code", "local"]);
    }

    /// A local record in `area`, with `--tier local`.
    fn local_record(&self, title: &str, area: &str) {
        self.ok(&[
            "record",
            "add",
            "--project",
            "1",
            "--title",
            title,
            "--area",
            area,
            "--tier",
            "local",
        ]);
    }

    /// Issue `n`'s block.
    fn block(&self, n: u64) -> fl_github::meta::Meta {
        fl_github::meta::parse_body(&self.fake.issue(n).body)
            .unwrap()
            .1
    }
}

// Routing spec §3.3: a local record becomes an issue with its own state,
// area and open findings, its old IRI as the create key and an alias, and
// a line naming who escalated it and why; the old handle then names the
// issue (§2.3).
#[test]
fn a_record_escalates_to_an_issue_with_its_state_area_findings_and_provenance() {
    let g = world(BOUND);
    g.routed();
    g.local_record("fix the parser", "code");
    g.ok(&["record", "move", "1", "--to", "doing"]);
    g.ok(&[
        "finding",
        "raise",
        "--record",
        "1",
        "--claim",
        "it drops a token",
        "--by",
        "rev",
    ]);
    let (out, err) = g.ok_said(&[
        "record",
        "escalate",
        "1",
        "--by",
        "alice",
        "--reason",
        "needs a design call",
    ]);
    assert_eq!(out, "1\tescalated\t#1\n");
    assert!(!err.contains(WARNED), "a private repository: {err}");
    assert_eq!(g.fake.issue_count(), 1);
    let issue = g.fake.issue(1);
    assert_eq!(issue.title, "fix the parser");
    assert_eq!(
        issue.labels,
        vec!["fl:record", "fl:record/doing", "fl:area/code"]
    );
    let block = g.block(1);
    assert_eq!(block.fl_format, 3);
    let from = block.escalated.as_ref().unwrap();
    assert_eq!(
        (from.by.as_str(), from.reason.as_str()),
        ("alice", "needs a design call")
    );
    assert_eq!(block.create_key, from.from.as_str());
    assert_eq!(block.also_known_as, vec![from.from.clone()]);
    assert!(
        issue.body.contains(&format!(
            "Escalated from the local tier by alice: needs a design call. Its local IRI was {}.",
            from.from
        )),
        "{}",
        issue.body
    );
    assert!(
        issue
            .body
            .contains("Open findings when this record was escalated:")
            && issue.body.contains("- raised: it drops a token — "),
        "{}",
        issue.body
    );
    // The tombstoned row is left out; the issue is listed in its place.
    assert_eq!(
        g.ok(&["record", "list", "--project", "1"]),
        "#1\tgithub\tdoing\tfix the parser\n"
    );
    // The old handle follows the tombstone to the issue.
    let moved = g.ok(&["record", "move", "1", "--to", "review"]);
    assert!(moved.starts_with("#1\treview\t"), "{moved}");
    assert!(
        g.fake
            .issue(1)
            .labels
            .contains(&"fl:record/review".to_string())
    );
}

// Routing spec §2.5, §3.3: a finding about a local record escalates, and
// its issue names the record, which stays local.
#[test]
fn a_finding_about_a_local_record_escalates_and_its_issue_names_the_record() {
    let g = world(BOUND);
    g.routed();
    g.local_record("fix the parser", "code");
    g.ok(&[
        "finding",
        "raise",
        "--record",
        "1",
        "--claim",
        "it drops a token",
        "--by",
        "rev",
    ]);
    assert_eq!(
        g.ok(&[
            "finding",
            "escalate",
            "1",
            "--by",
            "alice",
            "--reason",
            "a person decides",
        ]),
        "1\tescalated\t#1\n"
    );
    let issue = g.fake.issue(1);
    assert_eq!(
        issue.labels,
        vec!["fl:finding", "fl:finding/raised", "fl:area/code"]
    );
    assert!(
        issue.body.contains("Record: fix the parser — urn:uuid:")
            && issue
                .body
                .contains("held in the local tier, not on GitHub."),
        "{}",
        issue.body
    );
    assert!(
        issue
            .body
            .contains("Escalated from the local tier by alice: a person decides."),
        "{}",
        issue.body
    );
    assert_eq!(
        g.ok(&["record", "list", "--project", "1"]),
        "1\tlocal\ttodo\tfix the parser\n"
    );
}

// Routing spec §3.2, decisions 18 and 21: to a public repository a
// sensitive item is refused before the mark, and anything else is published
// after a warning naming what goes out.
#[test]
fn an_escalation_to_a_public_repository_warns_what_it_publishes() {
    let g = world(BOUND);
    g.routed();
    g.fake.state().repos[0].visibility = "public".into();
    g.local_record("rotate the keys", "security");
    let err = g.refused(&["record", "escalate", "1", "--by", "alice", "--reason", "r"]);
    assert!(
        err.contains("this record is security-sensitive")
            && err.contains("Use the local tier for it")
            && !err.contains(WARNED)
            && !err.contains(STOPPED),
        "{err}"
    );
    assert_eq!(g.fake.issue_count(), 0);
    // Not marked: the record still moves.
    g.ok(&["record", "move", "1", "--to", "doing"]);

    g.local_record("fix the parser", "code");
    g.ok(&[
        "finding",
        "raise",
        "--record",
        "2",
        "--claim",
        "it drops a token",
        "--by",
        "rev",
    ]);
    let (out, err) = g.ok_said(&[
        "record",
        "escalate",
        "2",
        "--by",
        "alice",
        "--reason",
        "needs a design call",
    ]);
    assert_eq!(out, "2\tescalated\t#1\n");
    let iri = g.block(1).escalated.unwrap().from;
    assert!(
        err.contains("warning: acme/widgets is public")
            && err.contains(WARNED)
            && err.contains("\"fix the parser\"")
            && err.contains(&format!("its local IRI, {iri},"))
            && err.contains(
                "who escalated it, \"alice\", and its 1 open finding, with its claim, state and IRI"
            ),
        "{err}"
    );
    assert_eq!(g.fake.issue_count(), 1);

    // A finding about a record on GitHub publishes its claim; one about a
    // local record publishes that record's title and IRI too.
    let (out, err) = g.ok_said(&["finding", "escalate", "1", "--by", "bob", "--reason", "why"]);
    assert_eq!(out, "1\tescalated\t#2\n");
    let iri = g.block(2).escalated.unwrap().from;
    assert!(
        err.contains(WARNED)
            && err.contains("this finding's claim, \"it drops a token\"")
            && err.contains(&format!("its local IRI, {iri},"))
            && err.contains("who escalated it, \"bob\", raised by \"rev\"")
            && !err.contains("assigned to")
            && !err.contains("its local record's title"),
        "{err}"
    );
    g.local_record("tidy the lexer", "code");
    g.ok(&[
        "finding",
        "raise",
        "--record",
        "3",
        "--claim",
        "it is slow",
        "--by",
        "rev",
    ]);
    let (_, err) = g.ok_said(&["finding", "escalate", "2", "--by", "bob", "--reason", "why"]);
    assert!(
        err.contains(WARNED) && err.contains("and its local record's title, \"tidy the lexer\""),
        "{err}"
    );
    assert_eq!(g.fake.issue_count(), 3);

    // A finding that is assigned publishes who it is assigned to.
    g.local_record("hone the lexer", "code");
    g.ok(&[
        "finding",
        "raise",
        "--record",
        "4",
        "--claim",
        "it is slow too",
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
    g.ok(&["finding", "reproduce", "3", "--gate", "1"]);
    g.ok(&["finding", "assign", "3", "--to", "carol"]);
    g.ok(&["manifest", "export", "--project", "1"]);
    git(g.repo.path(), &["add", "-A"]);
    git(g.repo.path(), &["commit", "-qm", "manifest"]);
    let (_, err) = g.ok_said(&["finding", "escalate", "3", "--by", "bob", "--reason", "why"]);
    assert!(
        err.contains(
            "who escalated it, \"bob\", raised by \"rev\", assigned to \"carol\", and its local"
        ),
        "{err}"
    );

    // A record with several open findings says they go out with their
    // claims, states and IRIs.
    g.local_record("sand the lexer", "code");
    for claim in ["one", "two"] {
        g.ok(&[
            "finding", "raise", "--record", "5", "--claim", claim, "--by", "rev",
        ]);
    }
    let (_, err) = g.ok_said(&["record", "escalate", "5", "--by", "bob", "--reason", "why"]);
    assert!(
        err.contains("and its 2 open findings, with their claims, states and IRIs"),
        "{err}"
    );
    assert_eq!(g.fake.issue_count(), 5);
}

// Routing spec §3.2, decisions 18, 21 and 22: a record's issue lists its
// open findings, but never one in an area that is sensitive now — here
// marked so after the finding was raised, so it carries no security flag.
#[test]
fn a_records_issue_never_lists_a_finding_in_a_sensitive_area() {
    let g = world(BOUND);
    g.routed();
    g.fake.state().repos[0].visibility = "public".into();
    g.local_record("fix the parser", "code");
    g.ok(&[
        "finding",
        "raise",
        "--record",
        "1",
        "--claim",
        "the key is in the log",
        "--area",
        "tests",
        "--by",
        "rev",
    ]);
    g.ok(&[
        "routing",
        "set",
        "--project",
        "1",
        "tests",
        "local",
        "--sensitive",
    ]);
    let (out, err) = g.ok_said(&["record", "escalate", "1", "--by", "alice", "--reason", "r"]);
    assert_eq!(out, "1\tescalated\t#1\n");
    let body = g.fake.issue(1).body;
    assert!(!body.contains("the key is in the log"), "{body}");
    assert!(
        err.contains(WARNED) && err.contains("and its 0 open findings"),
        "{err}"
    );
}

// Routing spec §3.2: a closed item is refused before the mark — the GitHub
// tracker creates open issues only.
#[test]
fn a_done_record_is_refused_before_the_mark() {
    let g = world(BOUND);
    g.routed();
    g.local_record("fix the parser", "code");
    g.ok(&["record", "move", "1", "--to", "done"]);
    let err = g.refused(&["record", "escalate", "1", "--by", "alice", "--reason", "r"]);
    assert!(
        err.contains("is in a closed state") && !err.contains(STOPPED),
        "{err}"
    );
    assert_eq!(g.fake.issue_count(), 0);
    // Not marked: the record still moves.
    g.ok(&["record", "move", "1", "--to", "doing"]);
}

// Routing spec §3.2: a visibility that cannot be read is not private, so
// the escalation is refused before the mark.
#[test]
fn an_unreadable_visibility_refuses_before_the_mark() {
    let g = world(BOUND);
    g.routed();
    g.local_record("fix the parser", "code");
    g.fake.state().fail_repo_read_after = Some(1);
    let err = g.refused(&["record", "escalate", "1", "--by", "alice", "--reason", "r"]);
    assert!(
        err.contains("could not read the visibility of acme/widgets")
            && !err.contains(STOPPED)
            && !err.contains(WARNED),
        "{err}"
    );
    assert_eq!(g.fake.issue_count(), 0);
    g.ok(&["record", "move", "1", "--to", "doing"]);
}

// Routing spec §3.3: a stop between the create and its labels leaves the
// item marked and the issue unlabelled. The command says how to finish;
// `--abandon` is refused, naming the issue; a rerun finds the issue —
// skipping the checks the issue already passed — and labels it.
#[test]
fn a_stop_between_the_create_and_its_labels_is_finished_by_a_rerun_with_one_issue() {
    let g = world(BOUND);
    g.routed();
    g.ok(&["manifest", "export", "--project", "1"]);
    g.local_record("fix the parser", "code");
    g.fake.state().fail_label_add_next = true;
    let args = ["record", "escalate", "1", "--by", "alice", "--reason", "r"];
    let err = g.refused(&args);
    assert!(
        err.contains(STOPPED)
            && err.contains("`fl record escalate 1 --by <who> --reason <why>`")
            && err.contains("`fl record escalate 1 --abandon`"),
        "{err}"
    );
    assert_eq!(g.fake.issue_count(), 1);
    let err = g.refused(&["record", "escalate", "1", "--abandon"]);
    assert!(
        err.contains("its issue exists")
            && err.contains("https://github.com/acme/widgets/issues/1"),
        "{err}"
    );
    // An issue fl cannot read — it has no labels yet — is not the record:
    // the local copy answers, refusing a move and allowing a finding about
    // it (routing spec §2.2, §3.3 step 1). So it does when the search fails,
    // and offline.
    let err = g.refused(&["record", "move", "1", "--to", "doing"]);
    assert!(
        err.contains("so this store refuses to change it") && err.contains("fl record escalate"),
        "{err}"
    );
    let raise = |claim: &'static str| {
        [
            "finding", "raise", "--record", "1", "--claim", claim, "--by", "rev",
        ]
    };
    g.ok(&raise("one"));
    g.fake.state().fail_issues_query_after = Some(0);
    g.ok(&raise("two"));
    let offline = g
        .fl()
        .env("FL_GITHUB_API_URL", "http://127.0.0.1:9")
        .args(raise("three"))
        .output()
        .unwrap();
    assert!(
        offline.status.success(),
        "{}",
        String::from_utf8_lossy(&offline.stderr)
    );
    // The routing map changes after the export: a first run would now be
    // refused, and the rerun is not.
    g.ok(&["routing", "set", "--project", "1", "product", "local"]);
    let (out, err) = g.ok_said(&args);
    assert_eq!(out, "1\tescalated\t#1\n");
    assert!(!err.contains(KEPT), "the same who and why: {err}");
    assert_eq!(g.fake.issue_count(), 1);
    assert_eq!(
        g.fake.issue(1).labels,
        vec!["fl:record", "fl:record/todo", "fl:area/code"]
    );
}

// Routing spec §3.3: a stop before the create leaves a mark and no issue;
// `--abandon` removes the mark, and the record is writable again.
#[test]
fn an_abandon_before_any_issue_clears_the_mark() {
    let g = world(BOUND);
    g.routed();
    g.local_record("fix the parser", "code");
    g.fake.state().rate_limited_next_create = true;
    let err = g.refused(&["record", "escalate", "1", "--by", "alice", "--reason", "r"]);
    assert!(err.contains(STOPPED), "{err}");
    let err = g.refused(&["record", "move", "1", "--to", "doing"]);
    assert!(err.contains("fl record escalate"), "{err}");
    assert_eq!(
        g.ok(&["record", "escalate", "1", "--abandon"]),
        "1\tabandoned\n"
    );
    g.ok(&["record", "move", "1", "--to", "doing"]);
    assert_eq!(g.fake.issue_count(), 0);
    let err = g.refused(&["record", "escalate", "1", "--abandon"]);
    assert!(err.contains("is not marked escalating"), "{err}");
}

// Routing spec §3.3: a rerun resumes with the mark's who and why, and says
// so when it was given others.
#[test]
fn a_rerun_with_another_reason_keeps_the_marks_and_says_so() {
    let g = world(BOUND);
    g.routed();
    // Public, so the warning says what goes out: the mark's reason.
    g.fake.state().repos[0].visibility = "public".into();
    g.local_record("fix the parser", "code");
    g.fake.state().rate_limited_next_create = true;
    let first = g.refused(&[
        "record", "escalate", "1", "--by", "alice", "--reason", "first",
    ]);
    assert!(!first.contains(KEPT), "{first}");
    let (out, err) = g.ok_said(&[
        "record", "escalate", "1", "--by", "alice", "--reason", "second",
    ]);
    assert_eq!(out, "1\tescalated\t#1\n");
    assert!(
        err.contains(KEPT)
            && err.contains("by alice for \"first\"")
            && err.contains("the reason, \"first\"")
            && !err.contains("the reason, \"second\""),
        "{err}"
    );
    assert!(
        g.fake
            .issue(1)
            .body
            .contains("Escalated from the local tier by alice: first."),
        "{}",
        g.fake.issue(1).body
    );
}

// GitHub tracker spec §4.3: an escalated finding names its reproduction
// gate where another machine reads it, so the gate must be in the committed
// manifest — checked before the mark.
#[test]
fn a_finding_whose_gate_is_not_in_the_committed_manifest_is_refused_before_the_mark() {
    let g = world(BOUND);
    g.routed();
    g.local_record("fix the parser", "code");
    g.ok(&[
        "finding",
        "raise",
        "--record",
        "1",
        "--claim",
        "it drops a token",
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
    g.ok(&["finding", "reproduce", "1", "--gate", "1"]);
    g.ok(&["manifest", "export", "--project", "1"]);
    let err = g.refused(&["finding", "escalate", "1", "--by", "alice", "--reason", "r"]);
    assert!(
        err.contains("is not committed") && err.contains("committed manifest: commit it"),
        "{err}"
    );
    assert_eq!(g.fake.issue_count(), 0);
    // Not marked: the finding is still writable.
    g.ok(&["finding", "assign", "1", "--to", "bob"]);
}

// Routing spec §1.2: a routing-map change the manifest does not carry
// refuses the escalation before the mark, as it refuses a new item.
#[test]
fn a_routing_map_the_manifest_does_not_carry_is_refused_before_the_mark() {
    let g = world(BOUND);
    g.routed();
    g.ok(&["manifest", "export", "--project", "1"]);
    g.local_record("fix the parser", "code");
    g.ok(&["routing", "set", "--project", "1", "product", "local"]);
    let err = g.refused(&["record", "escalate", "1", "--by", "alice", "--reason", "r"]);
    assert!(
        err.contains("changed since the manifest at") && !err.contains(STOPPED),
        "{err}"
    );
    assert_eq!(g.fake.issue_count(), 0);
    g.ok(&["record", "move", "1", "--to", "doing"]);
}

// Routing spec §3.1: only a routed store has a local tier to escalate from.
#[test]
fn an_unrouted_store_is_refused_naming_fl_routing_set() {
    let g = world("");
    g.ok(&["project", "add", "."]);
    g.ok(&["record", "add", "--project", "1", "--title", "t"]);
    let err = g.refused(&["record", "escalate", "1", "--by", "alice", "--reason", "r"]);
    assert!(
        err.contains("has no local tier to escalate from") && err.contains("fl routing set"),
        "{err}"
    );
    let err = g.refused(&["finding", "escalate", "1", "--abandon"]);
    assert!(err.contains("has no local tier to escalate from"), "{err}");
}

// Routing spec §3.1: a GitHub issue is not a local item.
#[test]
fn a_github_issue_is_refused_as_not_local() {
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
    let err = g.refused(&["record", "escalate", "#1", "--by", "alice", "--reason", "r"]);
    assert!(err.contains("is not a local item"), "{err}");
}

// Routing spec §3.3 step 1: while a finding is marked, a write to it is
// refused naming the command that finishes the escalation.
#[test]
fn a_write_to_a_marked_finding_names_fl_finding_escalate() {
    let g = world(BOUND);
    g.routed();
    g.local_record("fix the parser", "code");
    g.ok(&[
        "finding",
        "raise",
        "--record",
        "1",
        "--claim",
        "it drops a token",
        "--by",
        "rev",
    ]);
    g.fake.state().rate_limited_next_create = true;
    let err = g.refused(&["finding", "escalate", "1", "--by", "alice", "--reason", "r"]);
    assert!(
        err.contains(STOPPED) && err.contains("`fl finding escalate 1 --abandon`"),
        "{err}"
    );
    let err = g.refused(&["finding", "withdraw", "1", "--reason", "wrong"]);
    assert!(
        err.contains("so this store refuses to change it") && err.contains("fl finding escalate"),
        "{err}"
    );
}

// `--by` and `--reason` go together, and `--abandon` takes neither.
#[test]
fn an_escalation_takes_by_and_reason_or_abandon() {
    let g = world(BOUND);
    g.routed();
    g.local_record("fix the parser", "code");
    for (args, said) in [
        (vec!["record", "escalate", "1"], "--by <BY>"),
        (
            vec!["record", "escalate", "1", "--by", "alice"],
            "--reason <REASON>",
        ),
        (
            vec!["record", "escalate", "1", "--abandon", "--by", "alice"],
            "cannot be used with",
        ),
        (
            vec!["finding", "escalate", "1", "--abandon", "--reason", "r"],
            "cannot be used with",
        ),
    ] {
        g.fl().args(&args).assert().code(2).stderr(contains(said));
    }
    assert_eq!(g.fake.issue_count(), 0);
}
