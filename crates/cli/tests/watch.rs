//! `fl watch`, black box: human and `--json` output, and the `--fail-on-critical` exit status.
//! Hermetic: a temporary home and store, no hub.

use assert_cmd::Command;
use fl_core::at::At;
use fl_core::ids::{ProjectId, RecordId, seq_iri};
use fl_core::iri::Iri;
use fl_core::log::{Attempt, AttemptStatus, PathsTouched};
use fl_core::state::{
    MergeMode, RuntimeEntry, SensitivityPatch, StatePayload, StateScope, UpsertStateArgs,
};
use fl_core::store::{Catalog, Ledger, StateStore};
use fl_core::telemetry::{AttemptHeartbeat, LivenessStatus};
use fl_store::{AnyStore, BackendKind, StoreOptions};
use serde_json::{Value, json};
use std::path::PathBuf;

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

    fn logical(&self) -> PathBuf {
        self.stores.path().join("fl.redb")
    }

    fn fl(&self) -> Command {
        let mut c = Command::cargo_bin("fl").unwrap();
        c.env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.home.path().join("config"))
            .env("XDG_DATA_HOME", self.home.path().join("data"))
            .env_remove("FL_DB")
            .env("FL_STORE_BACKEND", "ferro")
            .env("HOSTNAME", "watch-test-host")
            .current_dir(self.home.path());
        c.arg("--db").arg(self.logical());
        c
    }

    fn open(&self) -> AnyStore {
        fl_store::open_store_with(
            &self.logical(),
            &StoreOptions {
                backend: Some(BackendKind::Ferro),
                ..StoreOptions::default()
            },
        )
        .unwrap()
    }
}

fn attempt(project: &ProjectId, record: u64, status: AttemptStatus, cost: u64) -> Attempt {
    Attempt {
        id: None,
        at: None,
        project: project.clone(),
        record: RecordId(seq_iri(record)),
        adapter: "claude".into(),
        status,
        duration_ms: 10,
        tokens_in: 3,
        tokens_out: 4,
        cost_usd_micros: cost,
        paths_touched: PathsTouched::Counted(0),
        output_excerpt: None,
    }
}

fn put_runtime(store: &AnyStore, slug: &str, value: Value) {
    store
        .upsert_state(UpsertStateArgs {
            scope: StateScope::Workspace,
            slug: slug.into(),
            sensitivity: SensitivityPatch::Inherit,
            payload: StatePayload::Runtime(RuntimeEntry {
                value,
                merge_mode: MergeMode::LastWriter,
            }),
            at: At::from_unix_millis(1),
            by: "test".into(),
        })
        .unwrap();
}

/// Project `pricey` spends $600 (critical); record 9 crashed three times in a row (warning);
/// agent `ada` last beat in 2020 on another host (stale, warning); a hosted session for `bo`.
fn seed(env: &Env) -> ProjectId {
    let store = env.open();
    let p = store.add_project("/w/pricey").unwrap();
    let cheap = store.add_project("/w/cheap").unwrap();
    store
        .append_attempt(attempt(&p, 1, AttemptStatus::Completed, 600_000_000))
        .unwrap();
    for _ in 0..3 {
        store
            .append_attempt(attempt(&cheap, 9, AttemptStatus::Crashed, 1))
            .unwrap();
    }
    let beat = At::from_unix_millis(1_600_000_000_000);
    let hb = AttemptHeartbeat {
        attempt_id: Iri::parse("urn:fl:attempt:ada-1").unwrap(),
        project: p.clone(),
        record: RecordId(seq_iri(1)),
        adapter: "claude".into(),
        agent: Some("ada".into()),
        host: "elsewhere".into(),
        pid: 1,
        started_at: beat.clone(),
        heartbeat_at: beat,
        status: LivenessStatus::Live,
        outcome: None,
        exit_at: None,
        duration_ms: 1,
        tokens_in: 1,
        tokens_out: 1,
        cost_usd_micros: 1,
    };
    put_runtime(
        &store,
        "liveness-agent-ada",
        serde_json::to_value(&hb).unwrap(),
    );
    put_runtime(
        &store,
        "hosted-session-claude-bo",
        json!({"session_id": "claude:bo", "persona": "bo", "project": "cheap",
               "vendor": "claude", "node_id": "ai-lab-0", "status": "running"}),
    );
    p
}

fn json_of(out: &[u8]) -> Value {
    serde_json::from_slice(out).unwrap_or_else(|e| {
        panic!("not JSON ({e}): {}", String::from_utf8_lossy(out));
    })
}

#[test]
fn status_human_and_json() {
    let env = Env::new();
    seed(&env);
    let out = env.fl().args(["watch", "status"]).output().unwrap();
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("telemetry (workspace): 4 attempts"), "{text}");
    assert!(text.contains("health: 2 agents"), "{text}");
    assert!(
        text.contains("alerts: 2 critical, 2 warning, 0 info"),
        "{text}"
    );
    assert!(text.contains("no live vendor quota source"), "{text}");

    let out = env
        .fl()
        .args(["watch", "status", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let v = json_of(&out.stdout);
    for key in ["telemetry", "health", "quotas", "alerts", "thresholds"] {
        assert!(v.get(key).is_some(), "missing {key}: {v}");
    }
    assert_eq!(v["telemetry"]["totals"]["attempts"], 4);
    assert_eq!(v["telemetry"]["source"], "rows");
    assert_eq!(v["thresholds"]["warn_quota_pct"], 80.0);
    let ids: Vec<&str> = v["alerts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids[1], "spend:workspace");
    assert!(ids.contains(&"heartbeat_stale:agent:ada"), "{ids:?}");
    assert!(
        ids.iter().any(|i| i.starts_with("crash_loop:record:")),
        "{ids:?}"
    );
}

#[test]
fn rollups_human_json_and_project_scope() {
    let env = Env::new();
    let p = seed(&env);
    let out = env
        .fl()
        .args(["watch", "rollups", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let v = json_of(&out.stdout);
    let t = &v["telemetry"];
    assert_eq!(t["totals"]["attempts"], 4);
    assert_eq!(t["totals"]["crashed"], 3);
    assert_eq!(t["totals"]["succeeded"], 1);
    assert_eq!(t["by_project"].as_array().unwrap().len(), 2);
    assert_eq!(t["by_adapter"][0]["key"], "claude");

    let out = env
        .fl()
        .args(["watch", "rollups", "--project", "PRICEY"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.starts_with(&format!("scope: {} (source: rows)", p.as_str())),
        "{text}"
    );
    assert!(text.contains("totals: 1 attempts"), "{text}");
    assert!(text.contains("cost $600.00"), "{text}");

    let out = env
        .fl()
        .args(["watch", "rollups", "--project", "nope"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("no project is named `nope`"));
}

#[test]
fn liveness_human_and_json() {
    let env = Env::new();
    seed(&env);
    let out = env
        .fl()
        .args(["watch", "liveness", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let v = json_of(&out.stdout);
    let agents = v["health"]["agents"].as_array().unwrap();
    assert_eq!(agents.len(), 2);
    assert_eq!(agents[0]["agent"], "ada");
    assert_eq!(agents[0]["liveness"], "stale");
    assert_eq!(agents[0]["verdict"]["reason"], "host_unavailable");
    assert_eq!(agents[1]["agent"], "bo");
    assert_eq!(agents[1]["liveness"], "missing");
    assert_eq!(v["health"]["local_host"], "watch-test-host");

    let out = env.fl().args(["watch", "liveness"]).output().unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.lines()
            .any(|l| l.starts_with("ada\tstale\tactive\tnode=elsewhere")),
        "{text}"
    );
    assert!(
        text.contains("agents: 2 (2 active): 0 live, 1 stale"),
        "{text}"
    );
}

#[test]
fn alerts_fail_on_critical_exit_status_and_overrides() {
    let env = Env::new();
    seed(&env);
    // Without the flag a critical alert does not change the status.
    let out = env.fl().args(["watch", "alerts"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.starts_with("CRITICAL\t"), "{text}");
    assert!(text.contains("\t-> "), "{text}");

    let out = env
        .fl()
        .args(["watch", "alerts", "--fail-on-critical"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{out:?}");

    // Raised cost lines leave only warnings: the flag passes.
    let out = env
        .fl()
        .args([
            "watch",
            "alerts",
            "--warn-cost-usd",
            "1000",
            "--critical-cost-usd",
            "2000",
            "--fail-on-critical",
            "--json",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let v = json_of(&out.stdout);
    assert_eq!(v["thresholds"]["warn_cost_usd_micros"], 1_000_000_000u64);
    let severities: Vec<&str> = v["alerts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["severity"].as_str().unwrap())
        .collect();
    assert!(!severities.contains(&"critical"), "{severities:?}");
    assert!(severities.contains(&"warning"), "{severities:?}");

    // Bad thresholds are refused.
    for args in [
        vec!["--warn-quota-pct", "120"],
        vec!["--warn-quota-pct", "NaN"],
        vec!["--warn-quota-pct", "96", "--critical-quota-pct", "90"],
        vec!["--warn-cost-usd=-1"],
    ] {
        let out = env
            .fl()
            .args(["watch", "alerts"])
            .args(&args)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1), "{args:?}: {out:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("error:"),
            "{args:?}"
        );
    }
}

#[test]
fn a_missing_store_is_refused_not_created() {
    let env = Env::new();
    let out = env.fl().args(["watch", "status"]).output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("nothing to watch"));
    assert!(!fl_store::store_exists(&env.logical()));
}

/// `fl watch` opens the store for a scan: a lone `.redb` under the ferro engine is read with the
/// redb engine and is never migrated, chmodded or written.
#[test]
fn a_lone_redb_under_the_ferro_engine_is_not_migrated() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new();
    let redb = env.logical();
    {
        let store = fl_store::open_store_with(
            &redb,
            &StoreOptions {
                backend: Some(BackendKind::Redb),
                ..StoreOptions::default()
            },
        )
        .unwrap();
        let p = store.add_project("/w/lone").unwrap();
        store
            .append_attempt(attempt(&p, 1, AttemptStatus::Crashed, 5))
            .unwrap();
    }
    std::fs::set_permissions(&redb, std::fs::Permissions::from_mode(0o644)).unwrap();
    let fls = fl_store::physical_path(&redb, BackendKind::Ferro);
    assert_ne!(fls, redb);
    assert!(!fls.exists());
    let before = std::fs::read(&redb).unwrap();
    let meta = std::fs::metadata(&redb).unwrap();
    let listing = |dir: &std::path::Path| {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    };
    let files_before = listing(env.stores.path());

    for args in [
        vec!["watch", "status", "--json"],
        vec!["watch", "rollups"],
        vec!["watch", "alerts"],
    ] {
        let out = env.fl().args(&args).output().unwrap();
        assert!(out.status.success(), "{args:?}: {out:?}");
        if args.contains(&"--json") {
            let v = json_of(&out.stdout);
            assert_eq!(v["telemetry"]["totals"]["attempts"], 1, "{v}");
        }
    }

    assert!(
        !fls.exists(),
        "no `.fls` was migrated beside the lone `.redb`"
    );
    assert_eq!(listing(env.stores.path()), files_before, "no file created");
    assert_eq!(
        std::fs::read(&redb).unwrap(),
        before,
        "the `.redb` bytes are unchanged"
    );
    let after = std::fs::metadata(&redb).unwrap();
    assert_eq!(after.permissions().mode() & 0o777, 0o644, "not chmodded");
    assert_eq!(after.modified().unwrap(), meta.modified().unwrap());
}

#[test]
fn usage_reports_offline_windows_and_rejects_bad_arguments() {
    let env = Env::new();
    let store = env.open();
    let ferro = store.as_ferro().unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let bucket = fl_watch::usage::UsageBucket {
        model: Some("fixture-model".into()),
        hour_ms: Some(now / 3_600_000 * 3_600_000),
        counts: fl_watch::usage::TokenCounts {
            input: Some(42),
            output: Some(0),
            cache_read: None,
            cache_write: None,
        },
        turns: 0,
        turns_unavailable: true,
        unavailable: fl_watch::usage::CategoryUnavailable {
            input: false,
            output: false,
            cache_read: true,
            cache_write: true,
        },
        missing: Default::default(),
        observations: 1,
    };
    let source = fl_watch::usage::SourceUsageDocument {
        source_id: "safe".into(),
        session_id: "s".into(),
        vendor: fl_watch::usage::UsageVendor::Codex,
        offset: 0,
        generation: 0,
        file_identity: None,
        anchor: String::new(),
        discarding_line: false,
        parser: Default::default(),
        buckets: vec![
            bucket.clone(),
            fl_watch::usage::UsageBucket {
                hour_ms: None,
                counts: fl_watch::usage::TokenCounts {
                    input: Some(17),
                    output: None,
                    ..Default::default()
                },
                unavailable: fl_watch::usage::CategoryUnavailable {
                    input: false,
                    output: true,
                    cache_read: true,
                    cache_write: true,
                },
                ..bucket
            },
        ],
        coverage: Default::default(),
    };
    ferro
        .put_doc(
            "watch_usage_v1",
            "source-1",
            &json!({"kind":"source","document":source}),
        )
        .unwrap();
    ferro
        .put_doc(
            "watch_usage_v1",
            "collector-status",
            &json!({"kind":"status","last_success_ms":now,"pending":false,"incomplete":false}),
        )
        .unwrap();
    drop(store);
    let out = env
        .fl()
        .args([
            "watch", "usage", "--since", "48h", "--by", "vendor", "--json",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report = json_of(&out.stdout);
    assert_eq!(report["rows"][0]["key"], "codex");
    assert_eq!(report["rows"][0]["input"], 42);
    assert!(report["rows"][0]["cache_read"].is_null());
    assert!(
        report["effective_start_ms"].as_u64().unwrap()
            <= report["requested_start_ms"].as_u64().unwrap()
    );
    let out = env
        .fl()
        .args(["watch", "usage", "--since", "0h"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let out = env
        .fl()
        .args(["watch", "usage", "--by", "bogus"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let out = env.fl().args(["watch", "usage"]).output().unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("Effective"));
    assert!(text.contains("undated\t17\tNo data"), "{text}");
}
