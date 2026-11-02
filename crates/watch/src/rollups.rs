//! Attempt and gate-run telemetry.
//!
//! Where each number comes from:
//!
//! - **Totals and per-project sums** (attempts, tokens, cost, wall clock): fl-store's
//!   [`attempt_totals`], which on the ferro engine answers from the columnar rollup
//!   (`FerroStore::attempt_rollup`) without handing rows to the caller, and on a remote ferro
//!   store is one RPC each. `source` says `columnar`, or `rows` when any of them fell back.
//! - **Workspace gate-run totals**: [`gate_run_totals`] (columnar the same way). Scoped to one
//!   project they are summed from that project's gate rows, which are read anyway.
//! - **Outcome counts, adapter and agent breakdowns, failure streaks**: the columnar rollup
//!   carries no status, adapter or record column, so these read each project's attempt rows
//!   (one `attempts(project)` call per project) and the gate rows (one `gate_runs(gate)` per
//!   gate).
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
//! ⚠ Like the columnar rollup, this reads only the store's own ledger tables, never a GitHub
//! ledger.

use fl_core::ids::ProjectId;
use fl_core::log::{Attempt, AttemptStatus, GateRun};
use fl_core::store::{Catalog, Ledger};
use fl_core::telemetry::{RegretChallenge, audit_transition_regret};
use fl_core::verdict::Verdict;
use fl_store::{AnyStore, TotalsSource, attempt_totals, gate_run_totals};
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

    fn set_sums(&mut self, t: &fl_store::AttemptTotals) {
        self.attempts = t.count;
        self.tokens_in = t.tokens_in;
        self.tokens_out = t.tokens_out;
        self.cost_usd_micros = t.cost_usd_micros;
        self.duration_ms = t.duration_ms;
    }

    fn add_outcomes(&mut self, other: &WatchAttemptCounts) {
        self.succeeded = self.succeeded.saturating_add(other.succeeded);
        self.failed = self.failed.saturating_add(other.failed);
        self.crashed = self.crashed.saturating_add(other.crashed);
        self.timed_out = self.timed_out.saturating_add(other.timed_out);
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
    /// Projects with at least two gated transitions, the least the audit compares.
    pub projects_audited: u64,
    pub challenges: Vec<WatchRegretChallenge>,
}

/// A record's trailing run of failures: attempts that did not complete, and gate runs that did
/// not pass, counted back from the newest until the first success.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchFailureStreak {
    pub record: String,
    pub project: String,
    pub slug: String,
    pub attempt_streak: u64,
    pub gate_streak: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_attempt_status: Option<AttemptStatus>,
    /// `pass`, `fail` or `error`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_gate_verdict: Option<String>,
}

/// What a store's ledger says about attempts and gate runs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchTelemetryReport {
    /// The project the report is scoped to (IRI); `None` is the whole store.
    #[serde(default)]
    pub project: Option<String>,
    /// `columnar` when every attempt sum came from the columnar rollup, else `rows`.
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

/// Telemetry for `project`, or the whole store when `None`. `runtime` supplies the heartbeats
/// that name each attempt's agent.
pub fn build_telemetry_with(
    store: &AnyStore,
    project: Option<&ProjectId>,
    runtime: &RuntimeSnapshot,
) -> Result<WatchTelemetryReport, WatchError> {
    let mut projects = store.list_projects()?;
    if let Some(p) = project {
        projects.retain(|x| &x.id == p);
        if projects.is_empty() {
            return Err(WatchError::UnknownProject(format!(
                "no project `{}` in this store",
                p.as_str()
            )));
        }
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
        ..WatchTelemetryReport::default()
    };
    let (totals, mut source) = attempt_totals(store, project)?;
    report.totals.set_sums(&totals);

    let mut by_adapter: BTreeMap<String, WatchAttemptCounts> = BTreeMap::new();
    let mut by_agent: BTreeMap<String, WatchAttemptCounts> = BTreeMap::new();
    let mut scoped_gates = WatchGateTotals::default();

    for p in &projects {
        let slug = project_slug(&p.root);
        let (sums, s) = attempt_totals(store, Some(&p.id))?;
        if s == TotalsSource::Rows {
            source = TotalsSource::Rows;
        }
        let mut row = WatchProjectRollup {
            project: p.id.as_str().to_string(),
            slug: slug.clone(),
            root: p.root.clone(),
            counts: WatchAttemptCounts::default(),
        };
        row.counts.set_sums(&sums);

        // Outcomes and breakdowns: the rows.
        let attempts = store.attempts(&p.id)?;
        let mut outcomes = WatchAttemptCounts::default();
        let mut per_record: BTreeMap<String, Vec<&Attempt>> = BTreeMap::new();
        for a in &attempts {
            outcomes.add_status(a.status);
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
        row.counts.add_outcomes(&outcomes);
        report.totals.add_outcomes(&outcomes);

        // Gate runs: every gate of the project, for streaks, regret and scoped totals.
        let mut runs: Vec<GateRun> = Vec::new();
        for g in store.list_gates(&p.id)? {
            runs.extend(store.gate_runs(&g.id)?);
        }
        for r in &runs {
            let slot = match r.verdict {
                Verdict::Pass { .. } => &mut scoped_gates.pass,
                Verdict::Fail { .. } => &mut scoped_gates.fail,
                Verdict::Error { .. } => &mut scoped_gates.error,
            };
            *slot = slot.saturating_add(1);
            scoped_gates.duration_ms = scoped_gates.duration_ms.saturating_add(r.duration_ms);
            scoped_gates.cost_usd_micros = scoped_gates
                .cost_usd_micros
                .saturating_add(r.cost_usd_micros);
        }
        let mut gate_per_record: BTreeMap<String, Vec<(u64, usize, &GateRun)>> = BTreeMap::new();
        for (i, r) in runs.iter().enumerate() {
            if let Some(rec) = &r.record {
                let at = r.at.as_ref().map(|a| a.unix_millis()).unwrap_or(0);
                gate_per_record
                    .entry(rec.as_str().to_string())
                    .or_default()
                    .push((at, i, r));
            }
        }

        let mut records: Vec<String> = per_record.keys().cloned().collect();
        records.extend(gate_per_record.keys().cloned());
        records.sort();
        records.dedup();
        for rec in records {
            let atts = per_record.get(&rec).map(Vec::as_slice).unwrap_or(&[]);
            let attempt_streak = trailing(atts, |a| a.status != AttemptStatus::Completed);
            let mut gates = gate_per_record.remove(&rec).unwrap_or_default();
            gates.sort_by_key(|(at, i, _)| (*at, *i));
            let gate_streak = trailing(&gates, |(_, _, r)| {
                !matches!(r.verdict, Verdict::Pass { .. })
            });
            if attempt_streak == 0 && gate_streak == 0 {
                continue;
            }
            report.failure_streaks.push(WatchFailureStreak {
                record: rec,
                project: row.project.clone(),
                slug: slug.clone(),
                attempt_streak,
                gate_streak,
                last_attempt_status: atts.last().map(|a| a.status),
                last_gate_verdict: gates
                    .last()
                    .map(|(_, _, r)| verdict_name(&r.verdict).to_string()),
            });
        }

        let transitions = store.list_transitions(&p.id)?;
        if transitions.iter().filter(|t| !t.gates.is_empty()).count() >= 2 {
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

    report.gate_runs = if project.is_none() {
        let (g, s) = gate_run_totals(store, None)?;
        if s == TotalsSource::Rows {
            source = TotalsSource::Rows;
        }
        WatchGateTotals {
            pass: g.pass,
            fail: g.fail,
            error: g.error,
            duration_ms: g.duration_ms,
            cost_usd_micros: g.cost_usd_micros,
        }
    } else {
        scoped_gates
    };
    report.source = source.as_str().to_string();
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
        // Record 100: a gate failure streak of 2 after a pass, ordered by time across gates.
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
        let mut want = [(rec(100), 0, 2), (rec(101), 3, 0), (rec(200), 1, 0)];
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
        // The ferro engine answered from its columnar rollup; redb from the rows.
        assert_eq!(rf.source, "columnar");
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
        assert_eq!(r.source, "columnar");
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
        assert_eq!(r.source, "columnar");
    }
}
