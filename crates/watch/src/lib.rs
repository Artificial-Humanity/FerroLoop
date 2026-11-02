//! FerroWatch (`fl-watch`): observability, telemetry rollups, liveness and alerting over a
//! FerroLoop store.
//!
//! The crate is usable on its own: it depends only on `fl-core` and `fl-store`. It reads the
//! store, and never writes it.
//!
//! - [`rollups`]: attempt and gate-run telemetry ([`WatchTelemetryReport`]): totals summed from
//!   the ledger rows, breakdowns by project, adapter (vendor) and agent, failure streaks,
//!   and the Decision 21 transition-regret audit.
//! - [`liveness`]: agent health ([`WatchHealthReport`]) from attempt heartbeats and hosted-session
//!   records, with the clock passed in.
//! - [`alerts`]: thresholds ([`WatchThresholds`]) and the pure alert evaluation
//!   ([`evaluate_watch_alerts`]).
//! - [`runtime`]: the workspace runtime entries the other modules read, decoded in one pass.
//!
//! [`build_summary`] runs all of them over one store: [`read_store_snapshot`] reads it once and
//! [`summarize`] evaluates health and alerts from that read, so a caller can cache the read.

pub mod alerts;
pub mod liveness;
pub mod rollups;
pub mod runtime;

use fl_core::ids::ProjectId;
use fl_core::store::{Catalog, StoreError};
use fl_store::AnyStore;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub use alerts::QUOTA_EXHAUSTED;
pub use alerts::{
    QuotaWindowKind, Severity, WatchAlert, WatchAlertRule, WatchEntity, WatchEntityKind,
    WatchQuotaWindow, WatchThresholds, evaluate_watch_alerts,
};
pub use liveness::{
    LivenessOptions, WatchAgentHealth, WatchGroupHealth, WatchHealthReport, WatchLiveness,
    WatchLivenessCounts, build_health, classify_liveness, evaluate_health,
};
pub use rollups::{
    WatchAttemptCounts, WatchBreakdown, WatchFailureStreak, WatchGateTotals, WatchProjectRollup,
    WatchRegretChallenge, WatchRegretSummary, WatchTelemetryReport, build_telemetry,
    build_telemetry_with,
};
pub use runtime::{RuntimeSnapshot, WatchHostedSession, read_runtime};

/// Why a watch report could not be built.
#[derive(Debug, thiserror::Error)]
pub enum WatchError {
    #[error(transparent)]
    Store(#[from] StoreError),
    /// A threshold is out of range: the message names the field.
    #[error("invalid threshold: {0}")]
    InvalidThreshold(String),
    /// A `--project` / `?project=` value that names no project, or more than one.
    #[error("{0}")]
    UnknownProject(String),
}

/// Everything FerroWatch reports for one store at one instant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WatchSummary {
    pub telemetry: WatchTelemetryReport,
    pub health: WatchHealthReport,
    pub quotas: Vec<WatchQuotaWindow>,
    pub alerts: Vec<WatchAlert>,
    pub thresholds: WatchThresholds,
}

/// What FerroWatch reads from a store for a summary, in one pass: `list_state` once,
/// `list_projects` once, then per project its attempts, gates, each gate's runs and its
/// transitions (`3P + G + 2` reads). It depends on neither the thresholds nor the clock, so a
/// caller may keep it a while and [`summarize`] it again and again (the hub does, for 2 s).
#[derive(Debug, Clone, PartialEq)]
pub struct WatchStoreSnapshot {
    pub telemetry: WatchTelemetryReport,
    pub runtime: RuntimeSnapshot,
    /// Project IRI → slug, for every project the catalog lists.
    pub project_labels: BTreeMap<String, String>,
}

/// Read a [`WatchStoreSnapshot`] of `store`, scoped to `project` when given.
pub fn read_store_snapshot(
    store: &AnyStore,
    project: Option<&ProjectId>,
) -> Result<WatchStoreSnapshot, WatchError> {
    let runtime = read_runtime(store)?;
    let projects = store.list_projects()?;
    let telemetry = rollups::telemetry_over(store, &projects, project, &runtime)?;
    let project_labels = projects
        .iter()
        .map(|p| (p.id.as_str().to_string(), project_slug(&p.root)))
        .collect();
    Ok(WatchStoreSnapshot {
        telemetry,
        runtime,
        project_labels,
    })
}

/// The health and alerts of `snapshot` against `thresholds` at `now_ms`. Reads no store.
///
/// `quotas` are vendor quota windows the caller knows of (the hub passes its discovered
/// sessions' quota); quota windows stored as `watch-quota-*` runtime entries are added to them.
/// `thresholds` are validated first. Health uses `thresholds.stale_after_ms`.
pub fn summarize(
    snapshot: &WatchStoreSnapshot,
    thresholds: &WatchThresholds,
    quotas: Vec<WatchQuotaWindow>,
    now_ms: u64,
    local_host: &str,
    pid_is_alive: &dyn Fn(u32) -> bool,
) -> Result<WatchSummary, WatchError> {
    thresholds.validate()?;
    let health = evaluate_health(
        &snapshot.runtime,
        &snapshot.project_labels,
        now_ms,
        &LivenessOptions {
            stale_after_ms: thresholds.stale_after_ms,
            local_host,
            pid_is_alive,
        },
    );
    let mut all_quotas = quotas;
    all_quotas.extend(snapshot.runtime.quotas.iter().cloned());
    let alerts = evaluate_watch_alerts(
        &snapshot.telemetry,
        &health,
        &all_quotas,
        thresholds,
        now_ms,
    );
    Ok(WatchSummary {
        telemetry: snapshot.telemetry.clone(),
        health,
        quotas: all_quotas,
        alerts,
        thresholds: thresholds.clone(),
    })
}

/// Build the telemetry, health and alerts for `store` at `now_ms`: [`read_store_snapshot`]
/// then [`summarize`]. Bad `thresholds` are refused before anything is read.
pub fn build_summary(
    store: &AnyStore,
    project: Option<&ProjectId>,
    thresholds: &WatchThresholds,
    quotas: Vec<WatchQuotaWindow>,
    now_ms: u64,
    local_host: &str,
    pid_is_alive: &dyn Fn(u32) -> bool,
) -> Result<WatchSummary, WatchError> {
    thresholds.validate()?;
    let snapshot = read_store_snapshot(store, project)?;
    summarize(
        &snapshot,
        thresholds,
        quotas,
        now_ms,
        local_host,
        pid_is_alive,
    )
}

/// The short name of a project: the last component of its root, lowercased (`/src/FerroLoop/`
/// is `ferroloop`). The same rule the CLI config uses to match a project by name.
pub fn project_slug(root: &str) -> String {
    let trimmed = root.trim().trim_end_matches(['/', '\\']);
    let base = trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed);
    if base.is_empty() {
        trimmed.to_ascii_lowercase()
    } else {
        base.to_ascii_lowercase()
    }
}

/// The project `raw` names: its id (IRI), or its slug ([`project_slug`], case-insensitive).
/// A slug two projects share is refused rather than guessed.
pub fn resolve_project<S: Catalog + ?Sized>(store: &S, raw: &str) -> Result<ProjectId, WatchError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(WatchError::UnknownProject(
            "the project name is empty".into(),
        ));
    }
    let projects = store.list_projects()?;
    if let Some(p) = projects.iter().find(|p| p.id.as_str() == raw) {
        return Ok(p.id.clone());
    }
    let want = raw.to_ascii_lowercase();
    let matches: Vec<_> = projects
        .iter()
        .filter(|p| project_slug(&p.root) == want)
        .collect();
    match matches.as_slice() {
        [one] => Ok(one.id.clone()),
        [] => Err(WatchError::UnknownProject(format!(
            "no project is named `{raw}`: give its id or the last component of its root"
        ))),
        many => Err(WatchError::UnknownProject(format!(
            "`{raw}` names {} projects; give the project id instead",
            many.len()
        ))),
    }
}

#[cfg(test)]
pub(crate) mod testkit;

#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::MemStore;

    #[test]
    fn project_slug_is_the_lowercased_last_component() {
        assert_eq!(project_slug("/src/FerroLoop"), "ferroloop");
        assert_eq!(project_slug("/src/FerroLoop/"), "ferroloop");
        assert_eq!(project_slug("relative"), "relative");
        assert_eq!(project_slug("C:\\work\\Repo"), "repo");
    }

    #[test]
    fn resolve_project_by_id_slug_and_refuses_unknown_or_ambiguous() {
        let store = MemStore::default();
        let a = store.add_project("/w/alpha").unwrap();
        let _b1 = store.add_project("/x/dup").unwrap();
        let _b2 = store.add_project("/y/dup").unwrap();
        assert_eq!(resolve_project(&store, a.as_str()).unwrap(), a);
        assert_eq!(resolve_project(&store, "ALPHA").unwrap(), a);
        assert!(matches!(
            resolve_project(&store, "nope"),
            Err(WatchError::UnknownProject(_))
        ));
        let err = resolve_project(&store, "dup").unwrap_err().to_string();
        assert!(err.contains("2 projects"), "{err}");
        assert!(resolve_project(&store, "  ").is_err());
    }

    /// The owner rule: fl-watch stands alone on fl-core, fl-store and three serde/error crates.
    #[test]
    fn the_crate_depends_only_on_core_store_and_serde() {
        let manifest = include_str!("../Cargo.toml");
        let mut in_deps = false;
        let mut deps = Vec::new();
        for line in manifest.lines() {
            let line = line.trim();
            if line.starts_with('[') {
                in_deps = line == "[dependencies]";
                continue;
            }
            if in_deps
                && !line.is_empty()
                && !line.starts_with('#')
                && let Some((name, _)) = line.split_once(['=', '.'])
            {
                deps.push(name.trim().to_string());
            }
        }
        deps.sort();
        assert_eq!(
            deps,
            ["fl-core", "fl-store", "serde", "serde_json", "thiserror"]
        );
    }

    #[test]
    fn build_summary_joins_telemetry_health_quotas_and_alerts() {
        use crate::testkit::{heartbeat, put_runtime};
        use fl_core::ids::{RecordId, seq_iri};
        use fl_core::log::{Attempt, AttemptStatus, PathsTouched};
        use fl_core::store::Ledger;
        use fl_core::telemetry::LivenessStatus;

        let dir = tempfile::tempdir().unwrap();
        let store = fl_store::open_store_with(
            &dir.path().join("s"),
            &fl_store::StoreOptions {
                backend: Some(fl_store::BackendKind::Ferro),
                ..fl_store::StoreOptions::default()
            },
        )
        .unwrap();
        let p = store.add_project("/w/pricey").unwrap();
        store
            .append_attempt(Attempt {
                id: None,
                at: None,
                project: p.clone(),
                record: RecordId(seq_iri(5)),
                adapter: "claude".into(),
                status: AttemptStatus::Completed,
                duration_ms: 1,
                tokens_in: 1,
                tokens_out: 1,
                cost_usd_micros: 600_000_000,
                paths_touched: PathsTouched::Counted(0),
                output_excerpt: None,
            })
            .unwrap();
        let now = 1_800_000_000_000;
        let hb = heartbeat("x", "ada", "far", now - 60_000, LivenessStatus::Live);
        put_runtime(
            &store,
            "liveness-agent-ada",
            serde_json::to_value(&hb).unwrap(),
        );
        put_runtime(
            &store,
            "watch-quota-claude",
            serde_json::json!({"vendor": "claude", "window": "secondary", "used_percent": 82.0}),
        );
        let given = vec![WatchQuotaWindow {
            vendor: "codex".into(),
            window: QuotaWindowKind::Primary,
            used_percent: 99.0,
            resets_at_ms: Some(now + 1),
            session: None,
            window_minutes: Some(300),
            exhausted_reason: None,
        }];
        let alive = |_: u32| true;
        let s = build_summary(
            &store,
            None,
            &WatchThresholds::default(),
            given.clone(),
            now,
            "here",
            &alive,
        )
        .unwrap();
        // The same summary from one store read evaluated apart, as the hub caches it.
        let snap = read_store_snapshot(&store, None).unwrap();
        let again = summarize(
            &snap,
            &WatchThresholds::default(),
            given,
            now,
            "here",
            &alive,
        );
        assert_eq!(again.unwrap(), s);
        assert_eq!(
            snap.project_labels.get(p.as_str()).map(String::as_str),
            Some("pricey")
        );
        let ids: Vec<&str> = s.alerts.iter().map(|a| a.id.as_str()).collect();
        let spend = format!("spend:project:{}", p.as_str());
        assert_eq!(
            ids,
            vec![
                "quota:codex:primary",
                spend.as_str(),
                "spend:workspace",
                "heartbeat_stale:agent:ada",
                "quota:claude:secondary",
            ]
        );
        assert_eq!(s.quotas.len(), 2);
        assert_eq!(s.health.agents.len(), 1);
        assert_eq!(s.telemetry.totals.attempts, 1);
        // Bad thresholds are refused before anything is read.
        let bad = WatchThresholds {
            warn_quota_pct: f64::NAN,
            ..WatchThresholds::default()
        };
        assert!(matches!(
            build_summary(&store, None, &bad, vec![], now, "here", &alive),
            Err(WatchError::InvalidThreshold(_))
        ));
    }
}
