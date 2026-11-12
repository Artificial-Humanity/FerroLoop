//! `fl watch`: FerroWatch telemetry, liveness and alerts over the workspace store.
//!
//! Every report comes from `fl-watch`; this module only resolves arguments, supplies the clock,
//! this host's name and the pid probe, and renders the result.

use clap::Subcommand;
use fl_core::ids::ProjectId;
use fl_core::telemetry::DEFAULT_STALE_AFTER_MS;
use fl_store::AnyStore;
use fl_watch::alerts::usd_to_micros;
use fl_watch::{
    LivenessOptions, Severity, WatchAlert, WatchAttemptCounts, WatchHealthReport, WatchSummary,
    WatchTelemetryReport, WatchThresholds, build_health, build_summary, build_telemetry,
    resolve_project,
};

/// Exit status of `fl watch alerts --fail-on-critical` when a critical alert is active.
pub const EXIT_CRITICAL: i32 = 2;

const QUOTA_NOTE: &str = "The CLI has no live vendor quota source: its quota alerts come only \
from `watch-quota-*` workspace runtime entries in the store (no fl component writes them today). \
The hub's /api/v1/watch/summary and /api/v1/watch/alerts add the quota of its discovered sessions.";

#[derive(Subcommand)]
pub enum WatchCmd {
    /// Token usage collected by the hub, grouped by persona, vendor, or model.
    Usage {
        /// Relative duration (e.g. 48h) or RFC3339 UTC timestamp.
        #[arg(long, default_value = "24h")]
        since: String,
        /// Group by persona, vendor, or model.
        #[arg(long, default_value = "persona")]
        by: String,
        #[arg(long)]
        json: bool,
    },
    /// Telemetry totals, agent health and active alerts, in one view.
    ///
    /// The CLI has no live vendor quota source: quota alerts come only from `watch-quota-*`
    /// workspace runtime entries in the store (no fl component writes them today). The hub's
    /// /api/v1/watch/summary adds the quota of its discovered sessions.
    Status {
        /// Scope the telemetry to one project: its id, or the last component of its root.
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Attempt and gate-run rollups: totals and outcomes summed from the ledger rows, and
    /// breakdowns by project, adapter and agent, failure streaks and transition regret.
    Rollups {
        /// One project: its id, or the last component of its root.
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Agent liveness from attempt heartbeats and hosted-session records, per agent, node and
    /// project.
    Liveness {
        /// Heartbeat age in milliseconds past which a `live` heartbeat is stale.
        #[arg(long, default_value_t = DEFAULT_STALE_AFTER_MS)]
        stale_after_ms: u64,
        #[arg(long)]
        json: bool,
    },
    /// Active alerts: spend, vendor quota windows, stale or dead heartbeats, crash loops and
    /// transition regret, most severe first.
    ///
    /// The CLI has no live vendor quota source: quota alerts come only from `watch-quota-*`
    /// workspace runtime entries in the store (no fl component writes them today). The hub's
    /// /api/v1/watch/alerts adds the quota of its discovered sessions.
    Alerts {
        /// Quota percentage for a warning (default 80).
        #[arg(long)]
        warn_quota_pct: Option<f64>,
        /// Quota percentage for a critical alert (default 95).
        #[arg(long)]
        critical_quota_pct: Option<f64>,
        /// Recorded attempt spend in USD for a warning, per project and for the workspace
        /// (default 100).
        #[arg(long)]
        warn_cost_usd: Option<f64>,
        /// Recorded attempt spend in USD for a critical alert (default 500).
        #[arg(long)]
        critical_cost_usd: Option<f64>,
        /// Heartbeat age in milliseconds past which a `live` heartbeat is stale.
        #[arg(long)]
        stale_after_ms: Option<u64>,
        /// Exit 2 when any critical alert is active.
        #[arg(long)]
        fail_on_critical: bool,
        #[arg(long)]
        json: bool,
    },
    /// Subscription usage balancing: plan, apply, and the policy. A client of the hub, which
    /// holds the live quota and runs the remodels.
    Balance(crate::cmd::watch_balance::BalanceArgs),
}

/// The thresholds `fl watch alerts` runs with: the defaults, with each flag given replacing one.
pub fn thresholds_from_flags(
    warn_quota_pct: Option<f64>,
    critical_quota_pct: Option<f64>,
    warn_cost_usd: Option<f64>,
    critical_cost_usd: Option<f64>,
    stale_after_ms: Option<u64>,
) -> Result<WatchThresholds, String> {
    let mut t = WatchThresholds::default();
    if let Some(v) = warn_quota_pct {
        t.warn_quota_pct = v;
    }
    if let Some(v) = critical_quota_pct {
        t.critical_quota_pct = v;
    }
    let micros = |flag: &str, usd: f64| {
        usd_to_micros(usd)
            .ok_or_else(|| format!("`--{flag}` must be a non-negative number of USD, not {usd}"))
    };
    if let Some(v) = warn_cost_usd {
        t.warn_cost_usd_micros = micros("warn-cost-usd", v)?;
    }
    if let Some(v) = critical_cost_usd {
        t.critical_cost_usd_micros = micros("critical-cost-usd", v)?;
    }
    if let Some(v) = stale_after_ms {
        t.stale_after_ms = v;
    }
    t.validate().map_err(|e| e.to_string())?;
    Ok(t)
}

fn usd(micros: u64) -> String {
    format!(
        "${}.{:02}",
        micros / 1_000_000,
        (micros % 1_000_000) / 10_000
    )
}

fn counts_line(c: &WatchAttemptCounts) -> String {
    format!(
        "{} attempts ({} succeeded, {} failed, {} crashed, {} timed out), cost {}, tokens {}/{}, \
         wall clock {} ms",
        c.attempts,
        c.succeeded,
        c.failed,
        c.crashed,
        c.timed_out,
        usd(c.cost_usd_micros),
        c.tokens_in,
        c.tokens_out,
        c.duration_ms
    )
}

/// The lines `fl watch rollups` prints.
pub fn render_telemetry(t: &WatchTelemetryReport) -> Vec<String> {
    let mut out = vec![
        format!(
            "scope: {} (source: {})",
            t.project.as_deref().unwrap_or("workspace"),
            t.source
        ),
        format!("totals: {}", counts_line(&t.totals)),
        format!(
            "gate runs: {} pass, {} fail, {} error, cost {}",
            t.gate_runs.pass,
            t.gate_runs.fail,
            t.gate_runs.error,
            usd(t.gate_runs.cost_usd_micros)
        ),
    ];
    for p in &t.by_project {
        out.push(format!(
            "project {} ({}): {}",
            p.slug,
            p.project,
            counts_line(&p.counts)
        ));
    }
    for b in &t.by_adapter {
        out.push(format!("adapter {}: {}", b.key, counts_line(&b.counts)));
    }
    for b in &t.by_agent {
        out.push(format!("agent {}: {}", b.key, counts_line(&b.counts)));
    }
    for s in &t.failure_streaks {
        let on_gate = match (&s.gate_name, &s.gate) {
            (Some(name), _) => format!(" of gate `{name}`"),
            (None, Some(id)) => format!(" of gate {id}"),
            (None, None) => String::new(),
        };
        out.push(format!(
            "failure streak {} ({}): {} attempts, {} gate runs{on_gate}",
            s.record, s.slug, s.attempt_streak, s.gate_streak
        ));
    }
    out.push(format!(
        "regret: {} challenge(s) over {} audited project(s)",
        t.regret.challenges.len(),
        t.regret.projects_audited
    ));
    for c in &t.regret.challenges {
        out.push(format!(
            "regret {} `{}` ({} -> {}): {}x the project median",
            c.slug,
            c.challenge.name,
            c.challenge.from.as_wire(),
            c.challenge.to.as_wire(),
            c.challenge.ratio
        ));
    }
    out
}

/// The lines `fl watch liveness` prints.
pub fn render_health(h: &WatchHealthReport) -> Vec<String> {
    let mut out = Vec::new();
    for a in &h.agents {
        let mut line = format!(
            "{}\t{}\t{}",
            a.agent,
            a.liveness.as_str(),
            if a.active { "active" } else { "inactive" }
        );
        if let Some(n) = &a.node {
            line.push_str(&format!("\tnode={n}"));
        }
        if let Some(p) = &a.project {
            line.push_str(&format!("\tproject={p}"));
        }
        if let Some(age) = a.heartbeat_age_ms {
            line.push_str(&format!("\tage={age}ms"));
        }
        if let Some(s) = &a.hosted_session {
            line.push_str(&format!("\tsession={} ({})", s.session_id, s.status));
        }
        out.push(line);
    }
    let c = &h.totals;
    out.push(format!(
        "agents: {} ({} active): {} live, {} stale, {} dead, {} exited, {} missing",
        c.agents, c.active, c.live, c.stale, c.dead, c.exited, c.missing
    ));
    for g in &h.by_node {
        out.push(format!(
            "node {}: {} live, {} stale, {} dead of {}",
            g.key, g.counts.live, g.counts.stale, g.counts.dead, g.counts.agents
        ));
    }
    for g in &h.by_project {
        out.push(format!(
            "project {}: {} live, {} stale, {} dead of {}",
            g.key, g.counts.live, g.counts.stale, g.counts.dead, g.counts.agents
        ));
    }
    if h.hosted_sessions_undecodable > 0 {
        out.push(format!(
            "{} hosted-session record(s) could not be read",
            h.hosted_sessions_undecodable
        ));
    }
    out
}

/// The lines `fl watch alerts` prints.
pub fn render_alerts(alerts: &[WatchAlert]) -> Vec<String> {
    if alerts.is_empty() {
        return vec!["no active alerts".to_string()];
    }
    let mut out = Vec::new();
    for a in alerts {
        out.push(format!(
            "{}\t{}\t{}",
            a.severity.as_str().to_ascii_uppercase(),
            a.id,
            a.message
        ));
        out.push(format!("\t-> {}", a.remediation));
    }
    out
}

fn alert_counts(alerts: &[WatchAlert]) -> String {
    let n = |s: Severity| alerts.iter().filter(|a| a.severity == s).count();
    format!(
        "alerts: {} critical, {} warning, {} info",
        n(Severity::Critical),
        n(Severity::Warning),
        n(Severity::Info)
    )
}

/// The lines `fl watch status` prints.
pub fn render_status(s: &WatchSummary) -> Vec<String> {
    let t = &s.telemetry;
    let c = &s.health.totals;
    let mut out = vec![
        format!(
            "telemetry ({}): {}",
            t.project.as_deref().unwrap_or("workspace"),
            counts_line(&t.totals)
        ),
        format!(
            "health: {} agents ({} active): {} live, {} stale, {} dead, {} exited, {} missing",
            c.agents, c.active, c.live, c.stale, c.dead, c.exited, c.missing
        ),
        alert_counts(&s.alerts),
    ];
    if !s.alerts.is_empty() {
        out.extend(render_alerts(&s.alerts));
    }
    out.push(format!("note: {QUOTA_NOTE}"));
    out
}

fn project_arg(store: &AnyStore, raw: Option<&str>) -> Result<Option<ProjectId>, (i32, String)> {
    raw.map(|r| resolve_project(store, r).map_err(|e| (1, e.to_string())))
        .transpose()
}

fn print_json<T: serde::Serialize>(value: &T) -> Result<(), (i32, String)> {
    let text = serde_json::to_string_pretty(value).map_err(|e| (1, e.to_string()))?;
    println!("{text}");
    Ok(())
}

fn print_lines(lines: Vec<String>) {
    for line in lines {
        println!("{line}");
    }
}

/// Run one `fl watch` command at `now_ms` on `local_host`. Returns the exit status.
pub fn run_at(
    store: &AnyStore,
    cmd: WatchCmd,
    now_ms: u64,
    local_host: &str,
    pid_is_alive: &dyn Fn(u32) -> bool,
) -> Result<i32, (i32, String)> {
    let fail = |e: fl_watch::WatchError| (1, e.to_string());
    match cmd {
        WatchCmd::Usage { since, by, json } => {
            use fl_watch::usage::{UsageReportQuery, aggregate_usage, format_time, parse_since};
            let since_ms = parse_since(&since, now_ms).map_err(|e| (1, e.to_string()))?;
            let by = by
                .parse()
                .map_err(|e: fl_watch::usage::UsageError| (1, e.to_string()))?;
            let report = aggregate_usage(store, &UsageReportQuery { since_ms, by }, now_ms)
                .map_err(|e| (1, e.to_string()))?;
            if json {
                print_json(&report)?;
            } else {
                println!(
                    "Requested: {} .. {}",
                    format_time(report.requested_start_ms),
                    format_time(report.requested_end_ms)
                );
                println!(
                    "Effective: {} .. {} (whole UTC hours)",
                    format_time(report.effective_start_ms),
                    format_time(report.effective_end_ms)
                );
                println!(
                    "Coverage: {}{}; last successful scan: {}",
                    if report.coverage.incomplete {
                        "incomplete"
                    } else {
                        "complete"
                    },
                    if report.coverage.pending {
                        ", pending"
                    } else {
                        ""
                    },
                    report
                        .coverage
                        .last_success_ms
                        .map_or_else(|| "unknown".into(), format_time)
                );
                println!("GROUP\tINPUT\tOUTPUT\tCACHE READ\tCACHE WRITE\tTURNS\tSUBAGENTS");
                let number = |v: Option<u64>, unavailable: bool| {
                    v.map_or_else(
                        || "No data".into(),
                        |n| {
                            if unavailable {
                                format!("{n} (partial)")
                            } else {
                                n.to_string()
                            }
                        },
                    )
                };
                for row in &report.rows {
                    println!(
                        "{}\t{}\t{}\t{}\t{}\t{}\t{}",
                        row.key,
                        number(row.input, row.unavailable.input),
                        number(row.output, row.unavailable.output),
                        number(row.cache_read, row.unavailable.cache_read),
                        number(row.cache_write, row.unavailable.cache_write),
                        number(row.turns, row.turns_unavailable),
                        row.subagents
                    );
                }
                if report.coverage.undated_observations > 0 {
                    println!(
                        "Undated observations: {} (outside the window)",
                        report.coverage.undated_observations
                    );
                    let row = &report.undated;
                    println!(
                        "{}\t{}\t{}\t{}\t{}\t{}\t{}",
                        row.key,
                        number(row.input, row.unavailable.input),
                        number(row.output, row.unavailable.output),
                        number(row.cache_read, row.unavailable.cache_read),
                        number(row.cache_write, row.unavailable.cache_write),
                        number(row.turns, row.turns_unavailable),
                        row.subagents
                    );
                }
                if report.coverage.unknown_launch_times > 0 {
                    println!(
                        "Subagents with unknown launch time: {}",
                        report.coverage.unknown_launch_times
                    );
                }
            }
            Ok(0)
        }
        WatchCmd::Status { project, json } => {
            let project = project_arg(store, project.as_deref())?;
            let s = build_summary(
                store,
                project.as_ref(),
                &WatchThresholds::default(),
                Vec::new(),
                now_ms,
                local_host,
                pid_is_alive,
            )
            .map_err(fail)?;
            if json {
                print_json(&s)?;
            } else {
                print_lines(render_status(&s));
            }
            Ok(0)
        }
        WatchCmd::Rollups { project, json } => {
            let project = project_arg(store, project.as_deref())?;
            let t = build_telemetry(store, project.as_ref()).map_err(fail)?;
            if json {
                print_json(&serde_json::json!({ "telemetry": t }))?;
            } else {
                print_lines(render_telemetry(&t));
            }
            Ok(0)
        }
        WatchCmd::Liveness {
            stale_after_ms,
            json,
        } => {
            if stale_after_ms == 0 {
                return Err((1, "`--stale-after-ms` must be at least 1".into()));
            }
            let h = build_health(
                store,
                now_ms,
                &LivenessOptions {
                    stale_after_ms,
                    local_host,
                    pid_is_alive,
                },
            )
            .map_err(fail)?;
            if json {
                print_json(&serde_json::json!({ "health": h }))?;
            } else {
                print_lines(render_health(&h));
            }
            Ok(0)
        }
        WatchCmd::Alerts {
            warn_quota_pct,
            critical_quota_pct,
            warn_cost_usd,
            critical_cost_usd,
            stale_after_ms,
            fail_on_critical,
            json,
        } => {
            let thresholds = thresholds_from_flags(
                warn_quota_pct,
                critical_quota_pct,
                warn_cost_usd,
                critical_cost_usd,
                stale_after_ms,
            )
            .map_err(|e| (1, e))?;
            let s = build_summary(
                store,
                None,
                &thresholds,
                Vec::new(),
                now_ms,
                local_host,
                pid_is_alive,
            )
            .map_err(fail)?;
            if json {
                print_json(&serde_json::json!({
                    "alerts": s.alerts,
                    "thresholds": s.thresholds,
                }))?;
            } else {
                print_lines(render_alerts(&s.alerts));
            }
            let critical = s.alerts.iter().any(|a| a.severity == Severity::Critical);
            Ok(if fail_on_critical && critical {
                EXIT_CRITICAL
            } else {
                0
            })
        }
        // A hub client that needs no store: `main` runs `fl watch balance` (with the configured
        // `[hub].listen`) before it opens one, so it never reaches here.
        WatchCmd::Balance(_) => {
            unreachable!("`fl watch balance` is dispatched by main before a store is opened")
        }
    }
}

/// Run one `fl watch` command now, on this host.
pub fn run(store: &AnyStore, cmd: WatchCmd) -> Result<i32, (i32, String)> {
    let now_ms = fl_exec::stamp::now().unix_millis();
    let host = fl_exec::adapters::harness::detect_host();
    run_at(store, cmd, now_ms, &host, &crate::cmd::agent::pid_is_alive)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_watch::{WatchAlertRule, WatchEntity, WatchEntityKind};

    fn alert(id: &str, severity: Severity) -> WatchAlert {
        WatchAlert {
            id: id.into(),
            rule: WatchAlertRule::Spend,
            severity,
            entity: WatchEntity {
                kind: WatchEntityKind::Workspace,
                id: "workspace".into(),
                label: None,
            },
            message: "spend is high".into(),
            observed: 1.0,
            threshold: 1.0,
            at_ms: 1,
            resets_at_ms: None,
            remediation: "spend less".into(),
        }
    }

    #[test]
    fn flags_replace_defaults_and_bad_values_are_refused() {
        let t = thresholds_from_flags(Some(50.0), None, Some(1.5), Some(2.0), Some(10)).unwrap();
        assert_eq!(t.warn_quota_pct, 50.0);
        assert_eq!(t.critical_quota_pct, 95.0);
        assert_eq!(t.warn_cost_usd_micros, 1_500_000);
        assert_eq!(t.critical_cost_usd_micros, 2_000_000);
        assert_eq!(t.stale_after_ms, 10);
        assert_eq!(
            thresholds_from_flags(None, None, None, None, None).unwrap(),
            WatchThresholds::default()
        );
        for (w, c, wc, cc) in [
            (Some(f64::NAN), None, None, None),
            (Some(-1.0), None, None, None),
            (None, Some(101.0), None, None),
            (Some(96.0), None, None, None),
            (None, None, Some(-0.5), None),
            (None, None, Some(600.0), None),
            (None, None, None, Some(f64::INFINITY)),
        ] {
            assert!(
                thresholds_from_flags(w, c, wc, cc, None).is_err(),
                "{w:?} {c:?} {wc:?} {cc:?}"
            );
        }
    }

    #[test]
    fn alerts_render_one_line_and_a_hint_each() {
        assert_eq!(render_alerts(&[]), vec!["no active alerts"]);
        let lines = render_alerts(&[alert("spend:workspace", Severity::Critical)]);
        assert_eq!(
            lines,
            vec![
                "CRITICAL\tspend:workspace\tspend is high".to_string(),
                "\t-> spend less".to_string()
            ]
        );
        assert_eq!(
            alert_counts(&[
                alert("a", Severity::Critical),
                alert("b", Severity::Info),
                alert("c", Severity::Info)
            ]),
            "alerts: 1 critical, 0 warning, 2 info"
        );
    }

    #[test]
    fn usd_renders_cents() {
        assert_eq!(usd(0), "$0.00");
        assert_eq!(usd(1_234_567), "$1.23");
        assert_eq!(usd(600_000_000), "$600.00");
    }

    /// `fl watch balance` has one path, main's (which passes `[hub].listen`): the store path
    /// refuses it loudly instead of running it without the configured listen address.
    #[test]
    #[should_panic(expected = "dispatched by main")]
    fn balance_never_runs_on_the_store_path() {
        let args = crate::cmd::watch_balance::BalanceArgs {
            cmd: None,
            dry_run: true,
            apply: false,
            warrant: None,
            hub: Some("127.0.0.1:1".into()),
            token_file: None,
            agent: None,
            json: false,
        };
        let _ = run_at(
            &AnyStore::in_memory(),
            WatchCmd::Balance(args),
            0,
            "host",
            &|_| true,
        );
    }
}
