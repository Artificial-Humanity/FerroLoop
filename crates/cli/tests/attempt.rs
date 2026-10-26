use assert_cmd::Command;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;
use std::path::Path;
use std::process::Command as Sys;

fn cli(db: &str) -> Command {
    let mut c = Command::cargo_bin("fl").unwrap();
    // Isolate from the developer's own
    // ~/.config/fl/config.toml, which `fl` reads unconditionally even when
    // --db confines which store it uses. `db`'s own parent directory is a
    // scratch TempDir the caller already holds, so it doubles as an empty
    // config home with no `fl/config.toml` inside it.
    let home = Path::new(db).parent().expect("db has a parent directory");
    c.env("XDG_CONFIG_HOME", home).arg("--db").arg(db);
    c
}

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

/// A registered project, in a real git working tree.
///
/// ⚠ These tests used to register the bare string `/tmp/x`, which is not a
/// git tree and on most machines does not exist at all. That worked only
/// because `project add` stored whatever it was handed. Keep the repo alive
/// for the test's duration — dropping the TempDir deletes it.
fn project(db: &str) -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q"]);
    git(repo.path(), &["config", "user.email", "t@example.com"]);
    git(repo.path(), &["config", "user.name", "t"]);
    std::fs::write(repo.path().join("a.rs"), "fn a() {}").unwrap();
    git(repo.path(), &["add", "-A"]);
    git(repo.path(), &["commit", "-qm", "first"]);
    cli(db)
        .args(["project", "add", &repo.path().display().to_string()])
        .assert()
        .success();
    repo
}

#[test]
fn an_unknown_adapter_is_refused_and_names_the_ones_that_exist() {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("t.redb").display().to_string();
    let _repo = project(&db);
    cli(&db)
        .args(["record", "add", "--project", "1", "--title", "t"])
        .assert()
        .success();
    cli(&db)
        .args(["attempt", "1", "--adapter", "telepathy"])
        .assert()
        .failure()
        .stderr(contains("telepathy").and(contains("claude")));
}

#[test]
fn a_refused_attempt_is_still_recorded_and_shows_up_in_stats() {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("t.redb").display().to_string();
    let _repo = project(&db);
    cli(&db)
        .args(["record", "add", "--project", "1", "--title", "t"])
        .assert()
        .success();

    // A zero budget is refused by the adapter before anything is spawned.
    cli(&db)
        .args([
            "attempt",
            "1",
            "--adapter",
            "claude",
            "--budget-usd-micros",
            "0",
        ])
        .assert()
        .code(1);

    cli(&db)
        .args(["stats", "--project", "1"])
        .assert()
        .success()
        .stdout(contains("refused").and(contains("attempts: 1")));
}

#[test]
fn stats_over_a_project_with_no_attempts_says_so_rather_than_printing_nothing() {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("t.redb").display().to_string();
    let _repo = project(&db);
    cli(&db)
        .args(["stats", "--project", "1"])
        .assert()
        .success()
        .stdout(contains("attempts: 0"));
}

#[cfg(unix)]
fn write_executable(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, body).unwrap();
    let mut perms = std::fs::metadata(&tmp).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&tmp, perms).unwrap();
    std::fs::rename(&tmp, path).unwrap();
}

#[cfg(unix)]
#[test]
fn all_five_adapters_run_record_telemetry_paths_touched_and_liveness() {
    use fl_core::ids::ProjectId;
    use fl_core::log::PathsTouched;
    use fl_core::store::{Catalog, Ledger};

    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("t.redb").display().to_string();
    let repo = project(&db);
    cli(&db)
        .args([
            "record",
            "add",
            "--project",
            "1",
            "--title",
            "implement telemetry",
        ])
        .assert()
        .success();

    let stub = d.path().join("vendor-stub.sh");
    write_executable(
        &stub,
        "#!/bin/sh\n\
         echo touched >> touched.txt\n\
         printf '%s\\n' '{\"type\":\"result\",\"result\":\"ok\",\"total_cost_usd\":0.005,\"usage\":{\"input_tokens\":100,\"output_tokens\":25}}'\n",
    );
    let stub_str = stub.display().to_string();

    for vendor in ["claude", "codex", "antigravity", "muse", "grok"] {
        cli(&db)
            .env("AGENT_PERSONA", "ferris")
            .args(["attempt", "1", "--adapter", vendor, "--binary", &stub_str])
            .assert()
            .success()
            .stdout(contains("completed").and(contains("\t| ok")));
    }

    cli(&db)
        .args(["stats", "--project", "1"])
        .assert()
        .success()
        .stdout(
            contains("attempts: 5")
                .and(contains("  completed: 5"))
                .and(contains("tokens in: 500"))
                .and(contains("tokens out: 125"))
                .and(contains("cost: 25000 micro-USD"))
                .and(contains("by adapter:"))
                .and(contains("  antigravity: 1 attempts (1 completed), "))
                .and(contains("  claude: 1 attempts (1 completed), "))
                .and(contains("  codex: 1 attempts (1 completed), "))
                .and(contains("  grok: 1 attempts (1 completed), "))
                .and(contains("  muse: 1 attempts (1 completed), ")),
        );

    cli(&db)
        .args(["agent", "liveness", "1", "--json"])
        .assert()
        .success()
        .stdout(
            contains("\"status\": \"exited\"")
                .and(contains("\"outcome\": \"completed\""))
                .and(contains("\"tokens_in\": 100"))
                .and(contains("\"tokens_out\": 25"))
                .and(contains("\"cost_usd_micros\": 5000")),
        );

    cli(&db)
        .args(["agent", "liveness", "ferris"])
        .assert()
        .success()
        .stdout(
            contains("ferris\texited (completed)\tadapter=grok")
                .and(contains("tokens=100/25\tcost=5000 micro-USD")),
        );

    let store = fl_store::RedbStore::open(Path::new(&db)).unwrap();
    let projects = store.list_projects().unwrap();
    let attempts = store
        .attempts(&ProjectId(projects[0].id.iri().clone()))
        .unwrap();
    assert_eq!(attempts.len(), 5);
    for a in &attempts {
        assert_eq!(a.tokens_in, 100);
        assert_eq!(a.tokens_out, 25);
        assert_eq!(a.cost_usd_micros, 5000);
        assert_eq!(
            a.paths_touched,
            PathsTouched::Listed(vec!["touched.txt".to_string()])
        );
    }
    drop(repo);
}

#[cfg(unix)]
#[test]
fn stats_reports_partial_cost_floor_and_regret_audit_challenge() {
    use fl_core::at::At;
    use fl_core::log::GateRun;
    use fl_core::model::{
        CommandSpec, GateKind, PopulationDelivery, Regret, Selector, State, Transition,
    };
    use fl_core::store::{Catalog, Ledger};
    use fl_core::verdict::Verdict;

    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("t.redb").display().to_string();
    let _repo = project(&db);
    cli(&db)
        .args(["record", "add", "--project", "1", "--title", "partial cost"])
        .assert()
        .success();

    let paid_stub = d.path().join("paid.sh");
    write_executable(
        &paid_stub,
        "#!/bin/sh\nprintf '%s\\n' '{\"type\":\"result\",\"result\":\"paid\",\"total_cost_usd\":0.01,\"usage\":{\"input_tokens\":200,\"output_tokens\":50}}'\n",
    );
    let free_stub = d.path().join("free.sh");
    write_executable(
        &free_stub,
        "#!/bin/sh\nprintf '%s\\n' '{\"type\":\"result\",\"result\":\"free\",\"usage\":{\"input_tokens\":100,\"output_tokens\":20}}'\n",
    );

    cli(&db)
        .args([
            "attempt",
            "1",
            "--adapter",
            "claude",
            "--binary",
            &paid_stub.display().to_string(),
        ])
        .assert()
        .success();
    cli(&db)
        .args([
            "attempt",
            "1",
            "--adapter",
            "muse",
            "--binary",
            &free_stub.display().to_string(),
        ])
        .assert()
        .success();

    {
        let store = fl_store::RedbStore::open(Path::new(&db)).unwrap();
        let proj = store.list_projects().unwrap()[0].id.clone();
        let g_fast = store
            .add_gate(
                &proj,
                "fast-lint",
                GateKind::Command(CommandSpec {
                    program: "true".into(),
                    args: vec![],
                    delivery: PopulationDelivery::Args,
                    timeout_secs: 10,
                    pass_codes: vec![0],
                }),
                Selector::Glob {
                    pattern: "*.rs".into(),
                },
                1,
                "deadbeef",
                "ferris",
            )
            .unwrap();
        let g_med = store
            .add_gate(
                &proj,
                "med-test",
                GateKind::Command(CommandSpec {
                    program: "true".into(),
                    args: vec![],
                    delivery: PopulationDelivery::Args,
                    timeout_secs: 10,
                    pass_codes: vec![0],
                }),
                Selector::Glob {
                    pattern: "*.rs".into(),
                },
                1,
                "deadbeef",
                "ferris",
            )
            .unwrap();
        let g_heavy = store
            .add_gate(
                &proj,
                "heavy-e2e",
                GateKind::Command(CommandSpec {
                    program: "true".into(),
                    args: vec![],
                    delivery: PopulationDelivery::Args,
                    timeout_secs: 10,
                    pass_codes: vec![0],
                }),
                Selector::Glob {
                    pattern: "*.rs".into(),
                },
                1,
                "deadbeef",
                "ferris",
            )
            .unwrap();
        store
            .add_transition(Transition {
                project: proj.clone(),
                name: "start".into(),
                from: State::Todo,
                to: State::Doing,
                regret: Regret::Low,
                gates: vec![g_fast.clone()],
            })
            .unwrap();
        store
            .add_transition(Transition {
                project: proj.clone(),
                name: "review".into(),
                from: State::Doing,
                to: State::Review,
                regret: Regret::Low,
                gates: vec![g_med.clone()],
            })
            .unwrap();
        store
            .add_transition(Transition {
                project: proj,
                name: "ship".into(),
                from: State::Review,
                to: State::Done,
                regret: Regret::Low,
                gates: vec![g_heavy.clone()],
            })
            .unwrap();
        store
            .append_gate_run(GateRun {
                id: None,
                at: Some(At::from_unix_millis(1_000)),
                gate: g_fast,
                record: None,
                commit: "deadbeef".into(),
                verdict: Verdict::from_predicate(true, 1),
                population: 1,
                output_excerpt: None,
                duration_ms: 100,
                cost_usd_micros: 100,
            })
            .unwrap();
        store
            .append_gate_run(GateRun {
                id: None,
                at: Some(At::from_unix_millis(1_500)),
                gate: g_med,
                record: None,
                commit: "deadbeef".into(),
                verdict: Verdict::from_predicate(true, 1),
                population: 1,
                output_excerpt: None,
                duration_ms: 100,
                cost_usd_micros: 300,
            })
            .unwrap();
        store
            .append_gate_run(GateRun {
                id: None,
                at: Some(At::from_unix_millis(2_000)),
                gate: g_heavy,
                record: None,
                commit: "deadbeef".into(),
                verdict: Verdict::from_predicate(true, 1),
                population: 1,
                output_excerpt: None,
                duration_ms: 5_000,
                cost_usd_micros: 6_000,
            })
            .unwrap();
    }

    cli(&db)
        .args(["stats", "--project", "1"])
        .assert()
        .success()
        .stdout(
            contains(
                "note: 1 of 2 attempts reported no cost, so the figure above is a floor and not a total",
            )
            .and(contains(
                "regret audit: transition review -> done (ship) is declared low regret, but its measured mean (6000 micro-USD) is 20x the project median (300 micro-USD)",
            )),
        );

    cli(&db)
        .args(["stats", "--project", "1", "--json"])
        .assert()
        .success()
        .stdout(
            contains("\"zero_cost_attempts\": 1")
                .and(contains("\"tokens_in\": 300"))
                .and(contains("\"tokens_out\": 70"))
                .and(contains("\"cost_usd_micros\": 10000"))
                .and(contains("\"regret_audit\"")),
        );
}
