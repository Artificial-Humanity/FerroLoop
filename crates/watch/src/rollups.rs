//! Attempt and gate-run telemetry.
//!
//! Every number comes from the rows, read once per report:
//!
//! - `list_projects` once, then per project one `attempts(project)`, one `list_gates(project)`,
//!   one `gate_runs(gate)` per gate and one `list_transitions(project)`: `3P + G + 1` store
//!   reads, plus the one `list_state` of [`read_runtime`]. The store has no read for every
//!   attempt or gate run at once, so these stay per project and per gate.
//! - **Totals and per-project sums** (attempts, outcomes, tokens, cost, wall clock) are summed
//!   from those attempt rows, so `succeeded + failed + crashed + timed_out == attempts` holds in
//!   every count. The columnar rollup (`fl_store::attempt_totals`) is not read: it is a
//!   separate read, and on a live store it can disagree with the rows. `source` is `rows`.
//! - **Gate-run totals** are summed from the same gate rows.
//! - **Failure streaks**: per record, the trailing run of attempts that did not complete, in
//!   store append order; and per record and gate, the trailing run of that gate's runs that did
//!   not pass, in store append order (never by `at`, which a run may lack). A record reports its
//!   longest gate streak and the gate it is on.
//! - **Transition regret**: core's [`audit_transition_regret`] over each project's transitions
//!   and gate runs, as `fl stats` runs it.
//!
//! Outcome names: `succeeded` is `completed`, `failed` is `refused`, `crashed`, `timed_out` is
//! `timeout`.
//!
//! The agent of an attempt is the `agent` of the heartbeat with the same attempt id. Heartbeat
//! entries keep only the newest attempt per record and per agent, so older attempts count under
//! `unattributed`.
//!
//! ⚠ This reads only the store's own ledger tables, never a GitHub ledger, and only the
//! attempts of the projects the catalog lists.

use fl_core::ids::ProjectId;
use fl_core::log::{Attempt, AttemptStatus, GateRun};
use fl_core::model::Project;
use fl_core::store::{Catalog, Ledger};
use fl_core::telemetry::{RegretChallenge, audit_transition_regret};
use fl_core::verdict::Verdict;
use fl_store::AnyStore;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::runtime::{RuntimeSnapshot, read_runtime};
use crate::{WatchError, project_slug};

/// The agent key for attempts no heartbeat names.
pub const UNATTRIBUTED: &str = "unattributed";

/// Counts and sums over a set of attempts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchAttemptCounts {
    pub attempts: u64,
    pub succeeded: u64,
    pub failed: u64,
    pub crashed: u64,
    pub timed_out: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cost_usd_micros: u64,
    pub duration_ms: u64,
}

impl WatchAttemptCounts {
    fn add_status(&mut self, status: AttemptStatus) {
        let slot = match status {
            AttemptStatus::Completed => &mut self.succeeded,
            AttemptStatus::Refused => &mut self.failed,
            AttemptStatus::Crashed => &mut self.crashed,
            AttemptStatus::Timeout => &mut self.timed_out,
        };
        *slot = slot.saturating_add(1);
    }

    fn add_attempt(&mut self, a: &Attempt) {
        self.attempts = self.attempts.saturating_add(1);
        self.add_status(a.status);
        self.tokens_in = self.tokens_in.saturating_add(a.tokens_in);
        self.tokens_out = self.tokens_out.saturating_add(a.tokens_out);
        self.cost_usd_micros = self.cost_usd_micros.saturating_add(a.cost_usd_micros);
        self.duration_ms = self.duration_ms.saturating_add(a.duration_ms);
    }

    fn add_counts(&mut self, other: &WatchAttemptCounts) {
        self.attempts = self.attempts.saturating_add(other.attempts);
        self.succeeded = self.succeeded.saturating_add(other.succeeded);
        self.failed = self.failed.saturating_add(other.failed);
        self.crashed = self.crashed.saturating_add(other.crashed);
        self.timed_out = self.timed_out.saturating_add(other.timed_out);
        self.tokens_in = self.tokens_in.saturating_add(other.tokens_in);
        self.tokens_out = self.tokens_out.saturating_add(other.tokens_out);
        self.cost_usd_micros = self.cost_usd_micros.saturating_add(other.cost_usd_micros);
        self.duration_ms = self.duration_ms.saturating_add(other.duration_ms);
    }
}

/// One project's attempts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchProjectRollup {
    /// The project id (IRI).
    pub project: String,
    pub slug: String,
    pub root: String,
    #[serde(flatten)]
    pub counts: WatchAttemptCounts,
}

/// One adapter's or agent's attempts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchBreakdown {
    pub key: String,
    #[serde(flatten)]
    pub counts: WatchAttemptCounts,
}

/// Gate-run counts by verdict, and their sums.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchGateTotals {
    pub pass: u64,
    pub fail: u64,
    pub error: u64,
    pub duration_ms: u64,
    pub cost_usd_micros: u64,
}

/// One regret challenge, with the project it is in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchRegretChallenge {
    pub project: String,
    pub slug: String,
    #[serde(flatten)]
    pub challenge: RegretChallenge,
}

/// The Decision 21 audit over every project in scope.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchRegretSummary {
    /// Projects with at least two gated transitions that have gate runs, the least the audit
    /// compares (core `audit_transition_regret` skips a transition no run measures).
    pub projects_audited: u64,
    pub challenges: Vec<WatchRegretChallenge>,
}

/// A record's trailing run of failures, counted back from the newest in store append order
/// until the first success: attempts that did not complete, and, per gate, that gate's runs
/// that did not pass. `gate_streak` is the longest of the per-gate streaks, on `gate`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchFailureStreak {
    pub record: String,
    pub project: String,
    pub slug: String,
    pub attempt_streak: u64,
    pub gate_streak: u64,
    /// The gate (IRI) `gate_streak` is on; `None` when no gate's newest run on the record
    /// failed. Of gates with equal streaks, the lowest id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate: Option<String>,
    /// That gate's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_attempt_status: Option<AttemptStatus>,
    /// The newest verdict of `gate` (`fail` or `error`); `pass` when the record has gate runs
    /// and every gate's newest run on it passed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_gate_verdict: Option<String>,
}

/// What a store's ledger says about attempts and gate runs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchTelemetryReport {
    /// The project the report is scoped to (IRI); `None` is the whole store.
    #[serde(default)]
    pub project: Option<String>,
    /// Where the numbers came from: always `rows` (see the module docs).
    pub source: String,
    pub totals: WatchAttemptCounts,
    pub gate_runs: WatchGateTotals,
    /// Sorted by project id.
    pub by_project: Vec<WatchProjectRollup>,
    /// Sorted by adapter.
    pub by_adapter: Vec<WatchBreakdown>,
    /// Sorted by agent key; [`UNATTRIBUTED`] for attempts no heartbeat names.
    pub by_agent: Vec<WatchBreakdown>,
    pub regret: WatchRegretSummary,
    /// Records whose newest attempt or gate run failed, sorted by record id.
    pub failure_streaks: Vec<WatchFailureStreak>,
}

fn verdict_name(v: &Verdict) -> &'static str {
    match v {
        Verdict::Pass { .. } => "pass",
        Verdict::Fail { .. } => "fail",
        Verdict::Error { .. } => "error",
    }
}

fn trailing<T>(items: &[T], failed: impl Fn(&T) -> bool) -> u64 {
    items.iter().rev().take_while(|x| failed(x)).count() as u64
}

/// [`build_telemetry_with`], reading the heartbeats it attributes agents by from `store`.
pub fn build_telemetry(
    store: &AnyStore,
    project: Option<&ProjectId>,
) -> Result<WatchTelemetryReport, WatchError> {
    let runtime = read_runtime(store)?;
    build_telemetry_with(store, project, &runtime)
}

/// The source every report names: the rows (see the module docs).
pub const SOURCE_ROWS: &str = "rows";

/// Telemetry for `project`, or the whole store when `None`. `runtime` supplies the heartbeats
/// that name each attempt's agent.
pub fn build_telemetry_with(
    store: &AnyStore,
    project: Option<&ProjectId>,
    runtime: &RuntimeSnapshot,
) -> Result<WatchTelemetryReport, WatchError> {
    let projects = store.list_projects()?;
    telemetry_over(store, &projects, project, runtime)
}

/// The trailing streak of failed runs of one gate on each record, the gate's runs in store
/// append order. Records with no run of the gate are absent.
fn gate_streaks_by_record(runs: &[GateRun]) -> BTreeMap<String, (u64, &'static str)> {
    let mut per_record: BTreeMap<String, Vec<&GateRun>> = BTreeMap::new();
    for r in runs {
        if let Some(rec) = &r.record {
            per_record
                .entry(rec.as_str().to_string())
                .or_default()
                .push(r);
        }
    }
    per_record
        .into_iter()
        .filter_map(|(rec, runs)| {
            let last = verdict_name(&runs.last()?.verdict);
            let streak = trailing(&runs, |r| !matches!(r.verdict, Verdict::Pass { .. }));
            Some((rec, (streak, last)))
        })
        .collect()
}

/// The longest per-gate failure streak on one record, so far.
#[derive(Default)]
struct WorstGate {
    streak: u64,
    gate: Option<(String, String)>,
    last_verdict: Option<&'static str>,
}

/// [`build_telemetry_with`] over a project list already read (`all_projects`, the whole
/// catalog).
pub(crate) fn telemetry_over(
    store: &AnyStore,
    all_projects: &[Project],
    project: Option<&ProjectId>,
    runtime: &RuntimeSnapshot,
) -> Result<WatchTelemetryReport, WatchError> {
    let mut projects: Vec<&Project> = all_projects
        .iter()
        .filter(|x| project.is_none_or(|p| &x.id == p))
        .collect();
    if let Some(p) = project
        && projects.is_empty()
    {
        return Err(WatchError::UnknownProject(format!(
            "no project `{}` in this store",
            p.as_str()
        )));
    }
    projects.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));

    let agent_of: BTreeMap<String, String> = runtime
        .latest_per_attempt()
        .into_iter()
        .filter_map(|(id, hb)| {
            hb.agent
                .as_deref()
                .map(str::trim)
                .filter(|a| !a.is_empty())
                .map(|a| (id, a.to_ascii_lowercase()))
        })
        .collect();

    let mut report = WatchTelemetryReport {
        project: project.map(|p| p.as_str().to_string()),
        source: SOURCE_ROWS.to_string(),
        ..WatchTelemetryReport::default()
    };
    let mut by_adapter: BTreeMap<String, WatchAttemptCounts> = BTreeMap::new();
    let mut by_agent: BTreeMap<String, WatchAttemptCounts> = BTreeMap::new();

    for p in projects {
        let slug = project_slug(&p.root);
        let mut row = WatchProjectRollup {
            project: p.id.as_str().to_string(),
            slug: slug.clone(),
            root: p.root.clone(),
            counts: WatchAttemptCounts::default(),
        };

        // Attempts: every count and sum, the breakdowns and the attempt streaks.
        let attempts = store.attempts(&p.id)?;
        let mut per_record: BTreeMap<String, Vec<&Attempt>> = BTreeMap::new();
        for a in &attempts {
            row.counts.add_attempt(a);
            by_adapter
                .entry(a.adapter.clone())
                .or_default()
                .add_attempt(a);
            let agent =
                a.id.as_ref()
                    .and_then(|id| agent_of.get(id.as_str()))
                    .map(String::as_str)
                    .unwrap_or(UNATTRIBUTED);
            by_agent
                .entry(agent.to_string())
                .or_default()
                .add_attempt(a);
            per_record
                .entry(a.record.as_str().to_string())
                .or_default()
                .push(a);
        }
        report.totals.add_counts(&row.counts);

        // Gate runs: one read per gate, in id order so a tie between gates is deterministic.
        let mut gates = store.list_gates(&p.id)?;
        gates.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
        let mut runs: Vec<GateRun> = Vec::new();
        let mut worst: BTreeMap<String, WorstGate> = BTreeMap::new();
        for g in &gates {
            let gate_runs = store.gate_runs(&g.id)?;
            for r in &gate_runs {
                let t = &mut report.gate_runs;
                let slot = match r.verdict {
                    Verdict::Pass { .. } => &mut t.pass,
                    Verdict::Fail { .. } => &mut t.fail,
                    Verdict::Error { .. } => &mut t.error,
                };
                *slot = slot.saturating_add(1);
                t.duration_ms = t.duration_ms.saturating_add(r.duration_ms);
                t.cost_usd_micros = t.cost_usd_micros.saturating_add(r.cost_usd_micros);
            }
            for (rec, (streak, last)) in gate_streaks_by_record(&gate_runs) {
                let w = worst.entry(rec).or_default();
                if streak > w.streak {
                    *w = WorstGate {
                        streak,
                        gate: Some((g.id.as_str().to_string(), g.name.clone())),
                        last_verdict: Some(last),
                    };
                } else if w.last_verdict.is_none() {
                    w.last_verdict = Some(last);
                }
            }
            runs.extend(gate_runs);
        }

        let mut records: Vec<String> = per_record.keys().cloned().collect();
        records.extend(worst.keys().cloned());
        records.sort();
        records.dedup();
        for rec in records {
            let atts = per_record.get(&rec).map(Vec::as_slice).unwrap_or(&[]);
            let attempt_streak = trailing(atts, |a| a.status != AttemptStatus::Completed);
            let w = worst.remove(&rec).unwrap_or_default();
            if attempt_streak == 0 && w.streak == 0 {
                continue;
            }
            let (gate, gate_name) = w.gate.unzip();
            report.failure_streaks.push(WatchFailureStreak {
                record: rec,
                project: row.project.clone(),
                slug: slug.clone(),
                attempt_streak,
                gate_streak: w.streak,
                gate,
                gate_name,
                last_attempt_status: atts.last().map(|a| a.status),
                last_gate_verdict: w.last_verdict.map(str::to_string),
            });
        }

        let transitions = store.list_transitions(&p.id)?;
        let measured = transitions
            .iter()
            .filter(|t| !t.gates.is_empty() && runs.iter().any(|r| t.gates.contains(&r.gate)))
            .count();
        if measured >= 2 {
            report.regret.projects_audited += 1;
            for challenge in audit_transition_regret(&transitions, &runs) {
                report.regret.challenges.push(WatchRegretChallenge {
                    project: row.project.clone(),
                    slug: slug.clone(),
                    challenge,
                });
            }
        }

        report.by_project.push(row);
    }

    report.by_adapter = by_adapter
        .into_iter()
        .map(|(key, counts)| WatchBreakdown { key, counts })
        .collect();
    report.by_agent = by_agent
        .into_iter()
        .map(|(key, counts)| WatchBreakdown { key, counts })
        .collect();
    report
        .failure_streaks
        .sort_by(|a, b| a.record.cmp(&b.record));
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{heartbeat, put_runtime};
    use fl_core::at::At;
    use fl_core::ids::{GateId, RecordId, seq_iri};
    use fl_core::iri::Iri;
    use fl_core::log::PathsTouched;
    use fl_core::model::{
        CommandSpec, GateKind, PopulationDelivery, Regret, Selector, State, Transition,
    };
    use fl_core::telemetry::LivenessStatus;
    use fl_store::{BackendKind, StoreOptions, open_store_with};

    fn open(dir: &tempfile::TempDir, name: &str, kind: BackendKind) -> AnyStore {
        open_store_with(
            &dir.path().join(name),
            &StoreOptions {
                backend: Some(kind),
                ..StoreOptions::default()
            },
        )
        .unwrap()
    }

    fn gate_kind() -> GateKind {
        GateKind::Command(CommandSpec {
            program: "true".into(),
            args: vec![],
            delivery: PopulationDelivery::Args,
            timeout_secs: 5,
            pass_codes: vec![0],
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn attempt(
        id: Option<&str>,
        project: &ProjectId,
        record: u64,
        adapter: &str,
        status: AttemptStatus,
        cost: u64,
        tin: u64,
        ms: u64,
    ) -> Attempt {
        Attempt {
            id: id.map(|s| Iri::parse(&format!("urn:fl:attempt:{s}")).unwrap()),
            at: None,
            project: project.clone(),
            record: RecordId(seq_iri(record)),
            adapter: adapter.into(),
            status,
            duration_ms: ms,
            tokens_in: tin,
            tokens_out: tin * 2,
            cost_usd_micros: cost,
            paths_touched: PathsTouched::Counted(0),
            output_excerpt: None,
        }
    }

    fn run(gate: &GateId, record: Option<u64>, pass: bool, at_ms: u64, cost: u64) -> GateRun {
        GateRun {
            id: None,
            at: Some(At::from_unix_millis(at_ms)),
            gate: gate.clone(),
            record: record.map(|r| RecordId(seq_iri(r))),
            commit: "c1".into(),
            verdict: Verdict::from_predicate(pass, 1),
            population: 1,
            output_excerpt: None,
            duration_ms: 10,
            cost_usd_micros: cost,
        }
    }

    struct Seeded {
        alpha: ProjectId,
        beta: ProjectId,
    }

    fn seed(store: &AnyStore) -> Seeded {
        let alpha = store.add_project("/w/Alpha").unwrap();
        let beta = store.add_project("/w/beta").unwrap();
        for a in [
            attempt(
                Some("a1"),
                &alpha,
                100,
                "claude",
                AttemptStatus::Completed,
                1_000,
                10,
                100,
            ),
            attempt(
                Some("a2"),
                &alpha,
                101,
                "codex",
                AttemptStatus::Crashed,
                2_000,
                20,
                200,
            ),
            attempt(
                Some("a3"),
                &alpha,
                101,
                "codex",
                AttemptStatus::Timeout,
                3_000,
                30,
                300,
            ),
            attempt(
                Some("a4"),
                &alpha,
                101,
                "codex",
                AttemptStatus::Refused,
                0,
                0,
                0,
            ),
            attempt(
                None,
                &beta,
                200,
                "claude",
                AttemptStatus::Completed,
                500,
                5,
                50,
            ),
            attempt(
                Some("b2"),
                &beta,
                200,
                "muse",
                AttemptStatus::Crashed,
                70,
                7,
                70,
            ),
        ] {
            store.append_attempt(a).unwrap();
        }
        let sel = || Selector::Glob {
            pattern: "**/*.rs".into(),
        };
        let fmt = store
            .add_gate(&alpha, "fmt", gate_kind(), sel(), 1, "c1", "o")
            .unwrap();
        let lint = store
            .add_gate(&alpha, "lint", gate_kind(), sel(), 1, "c1", "o")
            .unwrap();
        // Record 100: per gate in append order, `fmt` passed then failed (streak 1) and `lint`
        // failed once (streak 1); the `at` stamps are out of order and do not count.
        for r in [
            run(&fmt, Some(100), true, 1_000, 1),
            run(&lint, Some(100), false, 3_000, 1),
            run(&fmt, Some(100), false, 2_000, 1),
            run(&lint, None, true, 4_000, 1),
        ] {
            store.append_gate_run(r).unwrap();
        }
        // Regret: `ship` is low regret and costs far more than the median of the gated set.
        let cheap = store
            .add_gate(&beta, "cheap", gate_kind(), sel(), 1, "c1", "o")
            .unwrap();
        let cheap2 = store
            .add_gate(&beta, "cheap2", gate_kind(), sel(), 1, "c1", "o")
            .unwrap();
        let pricey = store
            .add_gate(&beta, "pricey", gate_kind(), sel(), 1, "c1", "o")
            .unwrap();
        for (gate, cost) in [(&cheap, 1_000), (&cheap2, 1_000), (&pricey, 1_000_000)] {
            store
                .append_gate_run(run(gate, None, true, 1, cost))
                .unwrap();
        }
        for (name, from, to, gates, regret) in [
            (
                "start",
                State::Todo,
                State::Doing,
                vec![cheap.clone()],
                Regret::Low,
            ),
            (
                "review",
                State::Doing,
                State::Review,
                vec![cheap2.clone()],
                Regret::Low,
            ),
            (
                "ship",
                State::Review,
                State::Done,
                vec![pricey.clone()],
                Regret::Low,
            ),
        ] {
            store
                .add_transition(Transition {
                    project: beta.clone(),
                    name: name.into(),
                    from,
                    to,
                    regret,
                    gates,
                })
                .unwrap();
        }
        // Attempts a1 (alpha) and b2 (beta) are attributed through their heartbeats.
        let mut hb = heartbeat("a1", "Ada", "h", 1, LivenessStatus::Exited);
        hb.record = RecordId(seq_iri(100));
        put_runtime(
            store,
            "liveness-agent-ada",
            serde_json::to_value(&hb).unwrap(),
        );
        let hb = heartbeat("b2", "bo", "h", 1, LivenessStatus::Exited);
        put_runtime(
            store,
            "liveness-agent-bo",
            serde_json::to_value(&hb).unwrap(),
        );
        let anon = heartbeat("a2", "", "h", 1, LivenessStatus::Exited);
        put_runtime(
            store,
            "liveness-record-x",
            serde_json::to_value(&anon).unwrap(),
        );
        Seeded { alpha, beta }
    }

    fn check_workspace(r: &WatchTelemetryReport, s: &Seeded) {
        assert_eq!(r.project, None);
        let t = r.totals;
        assert_eq!(t.attempts, 6);
        assert_eq!(
            (t.succeeded, t.failed, t.crashed, t.timed_out),
            (2, 1, 2, 1)
        );
        assert_eq!(t.cost_usd_micros, 6_570);
        assert_eq!(t.tokens_in, 72);
        assert_eq!(t.tokens_out, 144);
        assert_eq!(t.duration_ms, 720);

        assert_eq!(r.by_project.len(), 2);
        let alpha = r
            .by_project
            .iter()
            .find(|p| p.project == s.alpha.as_str())
            .unwrap();
        assert_eq!(alpha.slug, "alpha");
        assert_eq!(alpha.counts.attempts, 4);
        assert_eq!(alpha.counts.cost_usd_micros, 6_000);
        assert_eq!(
            (
                alpha.counts.succeeded,
                alpha.counts.failed,
                alpha.counts.crashed,
                alpha.counts.timed_out
            ),
            (1, 1, 1, 1)
        );
        let beta = r
            .by_project
            .iter()
            .find(|p| p.project == s.beta.as_str())
            .unwrap();
        assert_eq!(beta.counts.attempts, 2);
        assert_eq!(beta.counts.cost_usd_micros, 570);

        let adapters: Vec<(&str, u64, u64)> = r
            .by_adapter
            .iter()
            .map(|b| (b.key.as_str(), b.counts.attempts, b.counts.cost_usd_micros))
            .collect();
        assert_eq!(
            adapters,
            vec![("claude", 2, 1_500), ("codex", 3, 5_000), ("muse", 1, 70)]
        );
        let agents: Vec<(&str, u64)> = r
            .by_agent
            .iter()
            .map(|b| (b.key.as_str(), b.counts.attempts))
            .collect();
        assert_eq!(agents, vec![("ada", 1), ("bo", 1), (UNATTRIBUTED, 4)]);

        // Gate runs: 3 pass + 2 fail on alpha, 3 pass on beta.
        assert_eq!(
            (r.gate_runs.pass, r.gate_runs.fail, r.gate_runs.error),
            (5, 2, 0)
        );
        assert_eq!(r.gate_runs.cost_usd_micros, 4 + 1_002_000);

        let streaks: Vec<(&str, u64, u64)> = r
            .failure_streaks
            .iter()
            .map(|f| (f.record.as_str(), f.attempt_streak, f.gate_streak))
            .collect();
        let rec = |n: u64| seq_iri(n).as_str().to_string();
        let mut want = [(rec(100), 0, 1), (rec(101), 3, 0), (rec(200), 1, 0)];
        want.sort();
        let want: Vec<(&str, u64, u64)> =
            want.iter().map(|(r, a, g)| (r.as_str(), *a, *g)).collect();
        assert_eq!(streaks, want);
        let r101 = r
            .failure_streaks
            .iter()
            .find(|f| f.record == rec(101))
            .unwrap();
        assert_eq!(r101.last_attempt_status, Some(AttemptStatus::Refused));
        let r100 = r
            .failure_streaks
            .iter()
            .find(|f| f.record == rec(100))
            .unwrap();
        assert_eq!(r100.last_gate_verdict.as_deref(), Some("fail"));
        assert!(r100.gate.is_some(), "the streak names its gate");
        assert_eq!(t.succeeded + t.failed + t.crashed + t.timed_out, t.attempts);

        assert_eq!(r.regret.projects_audited, 1);
        assert_eq!(r.regret.challenges.len(), 1);
        assert_eq!(r.regret.challenges[0].challenge.name, "ship");
        assert_eq!(r.regret.challenges[0].slug, "beta");
    }

    #[test]
    fn mixed_outcomes_adapters_and_projects_roll_up_on_both_engines() {
        let dir = tempfile::tempdir().unwrap();
        let ferro = open(&dir, "f", BackendKind::Ferro);
        let redb = open(&dir, "r", BackendKind::Redb);
        let sf = seed(&ferro);
        let sr = seed(&redb);
        let rf = build_telemetry(&ferro, None).unwrap();
        let rr = build_telemetry(&redb, None).unwrap();
        check_workspace(&rf, &sf);
        check_workspace(&rr, &sr);
        // Both engines answer from the rows, so both say the same.
        assert_eq!(rf.source, "rows");
        assert_eq!(rr.source, "rows");
    }

    #[test]
    fn a_project_scope_reports_only_that_project() {
        let dir = tempfile::tempdir().unwrap();
        let store = open(&dir, "f", BackendKind::Ferro);
        let s = seed(&store);
        let r = build_telemetry(&store, Some(&s.beta)).unwrap();
        assert_eq!(r.project.as_deref(), Some(s.beta.as_str()));
        assert_eq!(r.totals.attempts, 2);
        assert_eq!(r.totals.cost_usd_micros, 570);
        assert_eq!((r.totals.succeeded, r.totals.crashed), (1, 1));
        assert_eq!(r.by_project.len(), 1);
        assert_eq!((r.gate_runs.pass, r.gate_runs.fail), (3, 0));
        assert_eq!(r.gate_runs.cost_usd_micros, 1_002_000);
        assert_eq!(r.source, "rows");
        let unknown = ProjectId(seq_iri(9_999));
        assert!(build_telemetry(&store, Some(&unknown)).is_err());
    }

    #[test]
    fn an_empty_store_reports_zeros() {
        let dir = tempfile::tempdir().unwrap();
        let store = open(&dir, "f", BackendKind::Ferro);
        let r = build_telemetry(&store, None).unwrap();
        assert_eq!(r.totals, WatchAttemptCounts::default());
        assert!(r.by_project.is_empty() && r.by_adapter.is_empty() && r.failure_streaks.is_empty());
        assert_eq!(r.source, "rows");
    }

    fn undated(mut r: GateRun) -> GateRun {
        r.at = None;
        r
    }

    /// Finding: the gate streak is per record and gate. A passing lint gate interleaved with a
    /// failing test gate used to keep the record's streak at 1 or less.
    #[test]
    fn a_failing_gate_beside_a_passing_one_is_a_crash_loop() {
        let dir = tempfile::tempdir().unwrap();
        for kind in [BackendKind::Ferro, BackendKind::Redb] {
            let store = open(&dir, &format!("{kind:?}"), kind);
            let p = store.add_project("/w/p").unwrap();
            let sel = || Selector::Glob {
                pattern: "**".into(),
            };
            let lint = store
                .add_gate(&p, "lint", gate_kind(), sel(), 1, "c1", "o")
                .unwrap();
            let test = store
                .add_gate(&p, "test", gate_kind(), sel(), 1, "c1", "o")
                .unwrap();
            for i in 0..4u64 {
                store
                    .append_gate_run(run(&lint, Some(7), true, 10 * i + 1, 0))
                    .unwrap();
                store
                    .append_gate_run(run(&test, Some(7), false, 10 * i + 2, 0))
                    .unwrap();
            }
            let r = build_telemetry(&store, None).unwrap();
            assert_eq!(r.failure_streaks.len(), 1, "{kind:?}");
            let s = &r.failure_streaks[0];
            assert_eq!(s.gate_streak, 4, "{kind:?}");
            assert_eq!(s.gate.as_deref(), Some(test.as_str()));
            assert_eq!(s.gate_name.as_deref(), Some("test"));
            assert_eq!(s.last_gate_verdict.as_deref(), Some("fail"));
            let alerts = crate::evaluate_watch_alerts(
                &r,
                &crate::WatchHealthReport::default(),
                &[],
                &crate::WatchThresholds::default(),
                0,
            );
            let want = format!(
                "crash_loop:record:{}:gate:{}",
                seq_iri(7).as_str(),
                test.as_str()
            );
            assert_eq!(alerts.len(), 1, "{alerts:?}");
            assert_eq!(alerts[0].id, want);
            assert_eq!(alerts[0].severity, crate::Severity::Warning);
            assert!(
                alerts[0].message.contains("`test`"),
                "{}",
                alerts[0].message
            );
        }
    }

    /// Finding: attempts and gate runs both follow store append order. An undated run appended
    /// last is the newest; a run stamped earlier but appended later is newer.
    #[test]
    fn streaks_follow_append_order_not_at() {
        let dir = tempfile::tempdir().unwrap();
        let store = open(&dir, "f", BackendKind::Ferro);
        let p = store.add_project("/w/p").unwrap();
        let sel = || Selector::Glob {
            pattern: "**".into(),
        };
        let g = store
            .add_gate(&p, "g", gate_kind(), sel(), 1, "c1", "o")
            .unwrap();
        // Record 1: fail at 9000, then an undated pass appended last: streak 0.
        // Record 2: pass at 9000, then fails stamped 1 and undated: streak 2.
        for r in [
            run(&g, Some(1), false, 9_000, 0),
            undated(run(&g, Some(1), true, 0, 0)),
            run(&g, Some(2), true, 9_000, 0),
            run(&g, Some(2), false, 1, 0),
            undated(run(&g, Some(2), false, 0, 0)),
        ] {
            store.append_gate_run(r).unwrap();
        }
        let r = build_telemetry(&store, None).unwrap();
        let streaks: Vec<(String, u64)> = r
            .failure_streaks
            .iter()
            .map(|f| (f.record.clone(), f.gate_streak))
            .collect();
        assert_eq!(streaks, vec![(seq_iri(2).as_str().to_string(), 2)]);
    }

    /// Finding: the totals and the outcome counts come from the same rows, so they add up.
    #[test]
    fn totals_and_outcomes_come_from_the_same_rows() {
        let dir = tempfile::tempdir().unwrap();
        for kind in [BackendKind::Ferro, BackendKind::Redb] {
            let store = open(&dir, &format!("t{kind:?}"), kind);
            let s = seed(&store);
            for scope in [None, Some(&s.alpha), Some(&s.beta)] {
                let r = build_telemetry(&store, scope).unwrap();
                let t = r.totals;
                assert_eq!(t.succeeded + t.failed + t.crashed + t.timed_out, t.attempts);
                let summed = r
                    .by_project
                    .iter()
                    .fold(WatchAttemptCounts::default(), |mut a, p| {
                        a.add_counts(&p.counts);
                        a
                    });
                assert_eq!(summed, t, "{kind:?} {scope:?}");
            }
        }
    }

    /// Finding: a project counts as audited only with two transitions whose gates have runs.
    #[test]
    fn a_project_with_unmeasured_transitions_is_not_audited() {
        let dir = tempfile::tempdir().unwrap();
        let store = open(&dir, "f", BackendKind::Ferro);
        let p = store.add_project("/w/p").unwrap();
        let sel = || Selector::Glob {
            pattern: "**".into(),
        };
        let ran = store
            .add_gate(&p, "ran", gate_kind(), sel(), 1, "c1", "o")
            .unwrap();
        let idle = store
            .add_gate(&p, "idle", gate_kind(), sel(), 1, "c1", "o")
            .unwrap();
        store.append_gate_run(run(&ran, None, true, 1, 5)).unwrap();
        for (name, from, to, gate) in [
            ("start", State::Todo, State::Doing, ran.clone()),
            ("ship", State::Doing, State::Done, idle.clone()),
        ] {
            store
                .add_transition(Transition {
                    project: p.clone(),
                    name: name.into(),
                    from,
                    to,
                    regret: Regret::Low,
                    gates: vec![gate],
                })
                .unwrap();
        }
        let r = build_telemetry(&store, None).unwrap();
        assert_eq!(
            r.regret.projects_audited, 0,
            "only one transition is measured"
        );
        store.append_gate_run(run(&idle, None, true, 2, 5)).unwrap();
        let r = build_telemetry(&store, None).unwrap();
        assert_eq!(r.regret.projects_audited, 1);
    }
}
