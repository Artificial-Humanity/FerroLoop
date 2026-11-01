use crate::refs::{self, Ref};
use anyhow::Result;
use clap::Args;
use fl_core::ids::{GateId, ProjectId};
use fl_core::log::{Attempt, AttemptStatus, GateRun};
use fl_core::model::Transition;
use fl_core::split::{Coverage, SplitLedger};
use fl_core::store::{Catalog, Ledger};
use fl_core::telemetry::{RegretChallenge, RegretMetric, audit_transition_regret};
use fl_core::{Iri, Kind};
use fl_store::{AnyStore, AttemptTotals, GateRunTotals, TotalsSource};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

/// Where `fl stats` reads attempts from (GitHub ledger spec §2.5, §2.6).
pub enum Source<'a> {
    /// No GitHub ledger is involved: the local store is the whole ledger.
    Local,
    /// The store records a GitHub ledger this command does not read, and
    /// why: the count covers the local store only, and says so.
    LocalOnly(String),
    /// The local store and the project's GitHub ledger, merged.
    Split(&'a SplitLedger<'a>),
}

#[derive(Args)]
pub struct Cmd {
    #[arg(long)]
    pub project: Ref,
    #[arg(long)]
    pub json: bool,
    /// Totals only: attempts, wall clock, tokens, cost and gate-run counts. On a FerroStore
    /// store with no GitHub ledger involved this never loads the attempt rows.
    #[arg(long)]
    pub summary: bool,
}

impl Cmd {
    /// The one item this command names, by `Ref` — the single source
    /// `iris()` and `has_handle()` both derive from.
    fn refs(&self) -> Vec<&Ref> {
        vec![&self.project]
    }

    pub fn iris(&self) -> Vec<Iri> {
        refs::iris(&self.refs())
    }

    /// Whether this command names its item by handle rather than IRI.
    pub fn has_handle(&self) -> bool {
        refs::has_handle(&self.refs())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
struct AdapterStats {
    attempts: u64,
    completed: u64,
    duration_ms: u64,
    tokens_in: u64,
    tokens_out: u64,
    cost_usd_micros: u64,
}

#[derive(Debug, Clone, Serialize)]
struct StatsJson {
    attempts: usize,
    coverage: String,
    by_status: BTreeMap<String, u64>,
    wall_clock_ms: u64,
    tokens_in: u64,
    tokens_out: u64,
    cost_usd_micros: u64,
    zero_cost_attempts: u64,
    by_adapter: BTreeMap<String, AdapterStats>,
    regret_audit: Vec<RegretChallenge>,
}

/// One gate's runs, read as rows: from the merged ledger when there is one, else the local store.
fn gate_run_rows(
    store: &AnyStore,
    source: &Source<'_>,
    coverage: &Coverage,
    gate_id: &GateId,
) -> Result<Vec<GateRun>> {
    Ok(match (source, coverage) {
        (Source::Split(split), Coverage::Complete) => match split.gate_runs(gate_id) {
            Ok(r) => r,
            Err(e) if e.is_transient() => Ledger::gate_runs(store, gate_id)?,
            Err(e) => return Err(e.into()),
        },
        _ => Ledger::gate_runs(store, gate_id)?,
    })
}

pub fn run(store: &AnyStore, cmd: Cmd, source: Source<'_>) -> Result<i32> {
    let project = ProjectId(refs::resolve(
        store,
        store.label(),
        Kind::Project,
        &cmd.project,
    )?);
    if cmd.summary {
        return run_summary(store, &project, &cmd, &source);
    }
    let (attempts, coverage) = match &source {
        Source::Local => (store.attempts(&project)?, Coverage::Complete),
        Source::LocalOnly(reason) => (
            store.attempts(&project)?,
            Coverage::LocalOnly {
                reason: reason.clone(),
            },
        ),
        // ⚠ Falls back to the local store only when GitHub could not be
        // read; a damaged or missing ledger is an error.
        Source::Split(split) => split.attempts_for_stats(&project)?,
    };
    let transitions = Catalog::list_transitions(store, &project)?;
    let should_audit = transitions.iter().filter(|t| !t.gates.is_empty()).count() >= 2
        && transitions
            .iter()
            .any(|t| !t.gates.is_empty() && t.regret == fl_core::model::Regret::Low);
    let mut gate_runs = Vec::new();
    if should_audit {
        let mut gate_ids: BTreeSet<GateId> = BTreeSet::new();
        for t in &transitions {
            for g in &t.gates {
                gate_ids.insert(g.clone());
            }
        }
        for gate_id in gate_ids {
            gate_runs.extend(gate_run_rows(store, &source, &coverage, &gate_id)?);
        }
    }

    if cmd.json {
        let json_out = build_json(&attempts, &coverage, &transitions, &gate_runs);
        println!("{}", serde_json::to_string_pretty(&json_out)?);
    } else {
        for line in report_with_audit(&attempts, &coverage, &transitions, &gate_runs) {
            println!("{line}");
        }
    }
    Ok(0)
}

#[derive(Debug, Clone, Serialize)]
struct SummaryGateRuns {
    pass: u64,
    fail: u64,
    error: u64,
}

#[derive(Debug, Clone, Serialize)]
struct SummaryJson {
    attempts: u64,
    coverage: String,
    wall_clock_ms: u64,
    tokens_in: u64,
    tokens_out: u64,
    cost_usd_micros: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    gate_runs: Option<SummaryGateRuns>,
    source: &'static str,
}

/// Totals over attempts already in memory (a GitHub ledger is involved).
fn totals_of_rows(attempts: &[Attempt]) -> AttemptTotals {
    let mut t = AttemptTotals::default();
    for a in attempts {
        t.count = t.count.saturating_add(1);
        t.cost_usd_micros = t.cost_usd_micros.saturating_add(a.cost_usd_micros);
        t.tokens_in = t.tokens_in.saturating_add(a.tokens_in);
        t.tokens_out = t.tokens_out.saturating_add(a.tokens_out);
        t.duration_ms = t.duration_ms.saturating_add(a.duration_ms);
    }
    t
}

/// `fl stats --summary`: totals only.
///
/// ⚠ The columnar rollup reads the local store's own tables and cannot see GitHub entries, so
/// it answers only for [`Source::Local`]. With a GitHub ledger involved the totals come from
/// the same rows `fl stats` reads, and `source` says `rows`.
fn run_summary(
    store: &AnyStore,
    project: &ProjectId,
    cmd: &Cmd,
    source: &Source<'_>,
) -> Result<i32> {
    let (attempts, coverage, mut origin) = match source {
        Source::Local => {
            let (t, o) = fl_store::attempt_totals(store, Some(project))?;
            (t, Coverage::Complete, o)
        }
        Source::LocalOnly(reason) => (
            totals_of_rows(&store.attempts(project)?),
            Coverage::LocalOnly {
                reason: reason.clone(),
            },
            TotalsSource::Rows,
        ),
        // ⚠ Same fallback as the full report: GitHub unreadable means local only.
        Source::Split(split) => {
            let (rows, coverage) = split.attempts_for_stats(project)?;
            (totals_of_rows(&rows), coverage, TotalsSource::Rows)
        }
    };
    let gates = Catalog::list_gates(store, project)?;
    let mut runs = GateRunTotals::default();
    for gate in &gates {
        let (t, o) = match source {
            Source::Local => fl_store::gate_run_totals(store, Some(&gate.id))?,
            _ => {
                let mut t = GateRunTotals::default();
                for run in gate_run_rows(store, source, &coverage, &gate.id)? {
                    match run.verdict {
                        fl_core::verdict::Verdict::Pass { .. } => t.pass = t.pass.saturating_add(1),
                        fl_core::verdict::Verdict::Fail { .. } => t.fail = t.fail.saturating_add(1),
                        fl_core::verdict::Verdict::Error { .. } => {
                            t.error = t.error.saturating_add(1)
                        }
                    }
                }
                (t, TotalsSource::Rows)
            }
        };
        runs.pass = runs.pass.saturating_add(t.pass);
        runs.fail = runs.fail.saturating_add(t.fail);
        runs.error = runs.error.saturating_add(t.error);
        if o == TotalsSource::Rows {
            origin = TotalsSource::Rows;
        }
    }
    let gate_runs = (!gates.is_empty()).then_some(SummaryGateRuns {
        pass: runs.pass,
        fail: runs.fail,
        error: runs.error,
    });
    if cmd.json {
        let coverage_str = match &coverage {
            Coverage::Complete => "complete".to_string(),
            Coverage::LocalOnly { reason } => format!("local_only ({reason})"),
        };
        let out = SummaryJson {
            attempts: attempts.count,
            coverage: coverage_str,
            wall_clock_ms: attempts.duration_ms,
            tokens_in: attempts.tokens_in,
            tokens_out: attempts.tokens_out,
            cost_usd_micros: attempts.cost_usd_micros,
            gate_runs,
            source: origin.as_str(),
        };
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        println!("attempts: {}", attempts.count);
        if let Coverage::LocalOnly { reason } = &coverage {
            println!("note: this covers the local store only, because {reason}");
        }
        println!("wall clock: {}ms", attempts.duration_ms);
        println!("tokens in: {}", attempts.tokens_in);
        println!("tokens out: {}", attempts.tokens_out);
        println!("cost: {} micro-USD", attempts.cost_usd_micros);
        if let Some(g) = &gate_runs {
            println!(
                "gate runs: {} pass, {} fail, {} error",
                g.pass, g.fail, g.error
            );
        }
        println!("source: {}", origin.as_str());
    }
    Ok(0)
}

fn build_json(
    attempts: &[Attempt],
    coverage: &Coverage,
    transitions: &[Transition],
    gate_runs: &[GateRun],
) -> StatsJson {
    let mut by_status: BTreeMap<String, u64> = BTreeMap::new();
    let mut by_adapter: BTreeMap<String, AdapterStats> = BTreeMap::new();
    let mut total_ms = 0u64;
    let mut total_tokens_in = 0u64;
    let mut total_tokens_out = 0u64;
    let mut total_cost = 0u64;
    let mut zero_cost_attempts = 0u64;

    for a in attempts {
        *by_status.entry(a.status.as_wire().to_string()).or_default() += 1;
        total_ms = total_ms.saturating_add(a.duration_ms);
        total_tokens_in = total_tokens_in.saturating_add(a.tokens_in);
        total_tokens_out = total_tokens_out.saturating_add(a.tokens_out);
        total_cost = total_cost.saturating_add(a.cost_usd_micros);
        if a.status != AttemptStatus::Refused && a.cost_usd_micros == 0 {
            zero_cost_attempts += 1;
        }
        let entry = by_adapter.entry(a.adapter.clone()).or_default();
        entry.attempts += 1;
        if a.status == AttemptStatus::Completed {
            entry.completed += 1;
        }
        entry.duration_ms = entry.duration_ms.saturating_add(a.duration_ms);
        entry.tokens_in = entry.tokens_in.saturating_add(a.tokens_in);
        entry.tokens_out = entry.tokens_out.saturating_add(a.tokens_out);
        entry.cost_usd_micros = entry.cost_usd_micros.saturating_add(a.cost_usd_micros);
    }

    let coverage_str = match coverage {
        Coverage::Complete => "complete".to_string(),
        Coverage::LocalOnly { reason } => format!("local_only ({reason})"),
    };
    let regret_audit = audit_transition_regret(transitions, gate_runs);

    StatsJson {
        attempts: attempts.len(),
        coverage: coverage_str,
        by_status,
        wall_clock_ms: total_ms,
        tokens_in: total_tokens_in,
        tokens_out: total_tokens_out,
        cost_usd_micros: total_cost,
        zero_cost_attempts,
        by_adapter,
        regret_audit,
    }
}

/// The report's lines.
///
/// ⚠ The count is printed even when it is zero: a report that prints
/// nothing when it found nothing is indistinguishable from a report that
/// did not run. And a report over the local store alone says so (GitHub
/// ledger spec §2.5): a count that silently omitted GitHub would read as the
/// total.
#[allow(dead_code)]
fn report(attempts: &[Attempt], coverage: &Coverage) -> Vec<String> {
    report_with_audit(attempts, coverage, &[], &[])
}

fn report_with_audit(
    attempts: &[Attempt],
    coverage: &Coverage,
    transitions: &[Transition],
    gate_runs: &[GateRun],
) -> Vec<String> {
    let mut out = vec![format!("attempts: {}", attempts.len())];
    if let Coverage::LocalOnly { reason } = coverage {
        out.push(format!(
            "note: this covers the local store only, because {reason}"
        ));
    }
    if !attempts.is_empty() {
        let mut by_status: BTreeMap<String, u64> = BTreeMap::new();
        let mut by_adapter: BTreeMap<String, AdapterStats> = BTreeMap::new();
        let mut total_ms = 0u64;
        let mut total_tokens_in = 0u64;
        let mut total_tokens_out = 0u64;
        let mut total_cost = 0u64;
        let mut non_refused_count = 0u64;
        let mut zero_cost_attempts = 0u64;

        for a in attempts {
            *by_status.entry(a.status.as_wire().to_string()).or_default() += 1;
            total_ms = total_ms.saturating_add(a.duration_ms);
            total_tokens_in = total_tokens_in.saturating_add(a.tokens_in);
            total_tokens_out = total_tokens_out.saturating_add(a.tokens_out);
            total_cost = total_cost.saturating_add(a.cost_usd_micros);
            if a.status != AttemptStatus::Refused {
                non_refused_count += 1;
                if a.cost_usd_micros == 0 {
                    zero_cost_attempts += 1;
                }
            }
            let entry = by_adapter.entry(a.adapter.clone()).or_default();
            entry.attempts += 1;
            if a.status == AttemptStatus::Completed {
                entry.completed += 1;
            }
            entry.duration_ms = entry.duration_ms.saturating_add(a.duration_ms);
            entry.tokens_in = entry.tokens_in.saturating_add(a.tokens_in);
            entry.tokens_out = entry.tokens_out.saturating_add(a.tokens_out);
            entry.cost_usd_micros = entry.cost_usd_micros.saturating_add(a.cost_usd_micros);
        }

        for (status, n) in by_status {
            out.push(format!("  {status}: {n}"));
        }
        out.push(format!("wall clock: {total_ms}ms"));
        out.push(format!("tokens in: {total_tokens_in}"));
        out.push(format!("tokens out: {total_tokens_out}"));
        out.push(format!("cost: {total_cost} micro-USD"));
        if total_cost == 0 {
            out.push(
                "note: no adapter reported a cost, so the figure above is a floor and not a total"
                    .to_string(),
            );
        } else if zero_cost_attempts > 0 {
            out.push(format!(
                "note: {zero_cost_attempts} of {non_refused_count} attempts reported no cost, so the figure above is a floor and not a total"
            ));
        }
        out.push("by adapter:".to_string());
        for (adapter, st) in by_adapter {
            out.push(format!(
                "  {adapter}: {} attempts ({} completed), {}ms, {} in / {} out, {} micro-USD",
                st.attempts,
                st.completed,
                st.duration_ms,
                st.tokens_in,
                st.tokens_out,
                st.cost_usd_micros
            ));
        }
    }

    let challenges = audit_transition_regret(transitions, gate_runs);
    for c in challenges {
        let unit = match c.metric {
            RegretMetric::CostUsdMicros => " micro-USD",
            RegretMetric::DurationMs => "ms",
        };
        out.push(format!(
            "regret audit: transition {} -> {} ({}) is declared low regret, but its measured mean ({}{unit}) is {}x the project median ({}{unit})",
            c.from.as_wire(),
            c.to.as_wire(),
            c.name,
            c.measured_mean,
            c.ratio,
            c.project_median
        ));
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Spec §2.5: a report over the local store alone says so — a count that
    // silently omitted GitHub would read as the total.
    #[test]
    fn a_local_only_report_says_so_even_when_it_found_nothing() {
        let lines = report(
            &[],
            &Coverage::LocalOnly {
                reason: "GitHub could not be read: connection refused".into(),
            },
        );
        assert_eq!(lines[0], "attempts: 0");
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("note: this covers the local store only")
                    && l.contains("connection refused")),
            "{lines:?}"
        );
    }

    #[test]
    fn a_complete_report_adds_no_note() {
        let lines = report(&[], &Coverage::Complete);
        assert_eq!(lines, vec!["attempts: 0".to_string()]);
    }
}
