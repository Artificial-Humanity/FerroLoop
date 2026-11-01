//! `fl stats --summary`, black box, on both engines.
//!
//! The same ledger goes into a redb store and a ferro store; the totals must be equal, the
//! source must name the path that produced them, and the plain `fl stats` output must not move.

use assert_cmd::Command;
use fl_core::ids::{GateId, ProjectId, RecordId, seq_iri};
use fl_core::log::{Attempt, AttemptStatus, GateRun, PathsTouched};
use fl_core::model::{CommandSpec, GateKind, PopulationDelivery, Selector};
use fl_core::store::{Catalog, Ledger};
use fl_core::verdict::Verdict;
use fl_store::{AnyStore, BackendKind, StoreOptions};
use serde_json::{Value, json};
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

    /// The logical store path; the ferro engine keeps `fl.fls` beside it.
    fn logical(&self) -> PathBuf {
        self.stores.path().join("fl.redb")
    }

    fn fl(&self, kind: BackendKind) -> Command {
        let mut c = Command::cargo_bin("fl").unwrap();
        c.env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.home.path().join("config"))
            .env("XDG_DATA_HOME", self.home.path().join("data"))
            .env_remove("FL_DB")
            .env_remove("FL_STORE_BACKEND")
            .current_dir(self.home.path());
        // Name the engine: neither leg depends on the default.
        c.env(
            "FL_STORE_BACKEND",
            match kind {
                BackendKind::Ferro => "ferro",
                BackendKind::Redb => "redb",
            },
        );
        c.arg("--db").arg(self.logical());
        c
    }
}

fn open(path: &Path, kind: BackendKind) -> AnyStore {
    fl_store::open_store_with(
        path,
        &StoreOptions {
            backend: Some(kind),
            ..StoreOptions::default()
        },
    )
    .unwrap()
}

fn attempt(project: &ProjectId, status: AttemptStatus, cost: u64, ms: u64) -> Attempt {
    Attempt {
        id: None,
        at: None,
        project: project.clone(),
        record: RecordId(seq_iri(9)),
        adapter: "claude".into(),
        status,
        duration_ms: ms,
        tokens_in: 10,
        tokens_out: 4,
        cost_usd_micros: cost,
        paths_touched: PathsTouched::Counted(0),
        output_excerpt: None,
    }
}

fn run(gate: &GateId, verdict: Verdict) -> GateRun {
    GateRun {
        id: None,
        at: None,
        gate: gate.clone(),
        record: None,
        commit: "c1".into(),
        verdict,
        population: 1,
        output_excerpt: None,
        duration_ms: 3,
        cost_usd_micros: 1,
    }
}

/// Seed one store with a fixed ledger and return the project's IRI.
fn seed(path: &Path, kind: BackendKind, with_gates: bool) -> String {
    let store = open(path, kind);
    let p = store.add_project("root").unwrap();
    let other = store.add_project("other").unwrap();
    for (status, cost, ms) in [
        (AttemptStatus::Completed, 1_000, 100),
        (AttemptStatus::Refused, 0, 0),
        (AttemptStatus::Timeout, 50, 900),
    ] {
        store.append_attempt(attempt(&p, status, cost, ms)).unwrap();
    }
    // Another project's attempt must not leak into the scoped totals.
    store
        .append_attempt(attempt(&other, AttemptStatus::Completed, 99, 99))
        .unwrap();
    if with_gates {
        let g = store
            .add_gate(
                &p,
                "fmt",
                GateKind::Command(CommandSpec {
                    program: "true".into(),
                    args: vec![],
                    delivery: PopulationDelivery::Args,
                    timeout_secs: 5,
                    pass_codes: vec![0],
                }),
                Selector::Glob {
                    pattern: "**/*.rs".into(),
                },
                1,
                "c1",
                "o",
            )
            .unwrap();
        store
            .append_gate_run(run(&g, Verdict::from_predicate(true, 1)))
            .unwrap();
        store
            .append_gate_run(run(&g, Verdict::from_predicate(true, 2)))
            .unwrap();
        store
            .append_gate_run(run(&g, Verdict::from_predicate(false, 1)))
            .unwrap();
        store
            .append_gate_run(run(&g, Verdict::error("boom")))
            .unwrap();
    }
    p.as_str().to_string()
}

fn json_of(cmd: &mut Command, args: &[&str]) -> Value {
    let out = cmd.args(args).output().unwrap();
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn summary_json_is_equal_on_both_engines_and_names_its_source() {
    let mut seen = Vec::new();
    for (kind, source) in [
        (BackendKind::Redb, "rows"),
        (BackendKind::Ferro, "columnar"),
    ] {
        let env = Env::new();
        let iri = seed(&env.logical(), kind, true);
        let v = json_of(
            &mut env.fl(kind),
            &["stats", "--project", &iri, "--summary", "--json"],
        );
        assert_eq!(v["source"], source, "{kind:?}");
        seen.push(v);
    }
    let want = json!({
        "attempts": 3,
        "coverage": "complete",
        "wall_clock_ms": 1000,
        "tokens_in": 30,
        "tokens_out": 12,
        "cost_usd_micros": 1050,
        "gate_runs": {"pass": 2, "fail": 1, "error": 1},
    });
    for v in &mut seen {
        v.as_object_mut().unwrap().remove("source");
    }
    assert_eq!(seen[0], want);
    assert_eq!(seen[1], want);
}

#[test]
fn a_project_without_gates_reports_no_gate_runs() {
    for kind in [BackendKind::Redb, BackendKind::Ferro] {
        let env = Env::new();
        let iri = seed(&env.logical(), kind, false);
        let v = json_of(
            &mut env.fl(kind),
            &["stats", "--project", &iri, "--summary", "--json"],
        );
        assert!(v.get("gate_runs").is_none(), "{v}");
        assert_eq!(v["attempts"], 3);
    }
}

#[test]
fn summary_text_prints_the_totals_and_the_source() {
    for (kind, source) in [
        (BackendKind::Redb, "rows"),
        (BackendKind::Ferro, "columnar"),
    ] {
        let env = Env::new();
        let iri = seed(&env.logical(), kind, true);
        let out = env
            .fl(kind)
            .args(["stats", "--project", &iri, "--summary"])
            .output()
            .unwrap();
        assert!(out.status.success());
        assert_eq!(
            String::from_utf8(out.stdout).unwrap(),
            format!(
                "attempts: 3\nwall clock: 1000ms\ntokens in: 30\ntokens out: 12\n\
                 cost: 1050 micro-USD\ngate runs: 2 pass, 1 fail, 1 error\nsource: {source}\n"
            )
        );
    }
}

#[test]
fn plain_stats_is_the_same_on_both_engines_and_has_no_summary_fields() {
    let mut seen = Vec::new();
    for kind in [BackendKind::Redb, BackendKind::Ferro] {
        let env = Env::new();
        let iri = seed(&env.logical(), kind, true);
        let v = json_of(&mut env.fl(kind), &["stats", "--project", &iri, "--json"]);
        for key in ["source", "gate_runs"] {
            assert!(v.get(key).is_none(), "{key} leaked into plain stats: {v}");
        }
        assert_eq!(v["attempts"], 3);
        assert_eq!(
            v["by_status"],
            json!({"completed": 1, "refused": 1, "timeout": 1})
        );
        seen.push(v);
    }
    assert_eq!(seen[0], seen[1]);
}
